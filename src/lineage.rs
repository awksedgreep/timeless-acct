//! Who started whom, and which job each process is part of.
//!
//! A process has a start, a duration, and a parent, which is what a span
//! is. What it does not have is a trace: the tree of processes has one
//! root, pid 1, and a trace of everything since boot says nothing.
//!
//! A trace here is a **job**: a process group. A shell makes one for each
//! command it is given, a pipeline is one, and systemd makes one for each
//! run of a service. `cargo build` and every compiler it starts are one
//! trace, and the shell it was typed into is not in it.
//!
//! A job ends. A daemon's process group does not, and its workers would be
//! one trace for as long as it runs. So a process joins its group's trace
//! only if it started within `max_age` of the group; past that, each
//! process it starts is the root of a trace of its own.
//!
//! The trace has to be decided when a process is first seen, not when it
//! ends: a child ends before its parent, and must already carry the id its
//! parent will carry. So every id is a function of what is known at the
//! start.

use std::collections::HashMap;

use crate::queue::settle;

/// How far apart two readings of a process's start may be for them to be
/// the same process. One is counted from boot in ticks; the other is the
/// kernel's, in whole seconds, from a different clock.
pub const START_TOLERANCE: f64 = 5.0;
/// How far up the tree a parent is looked for before giving up.
const DEPTH: u8 = 32;

pub type SpanId = [u8; 8];
pub type TraceId = [u8; 16];

/// What is known of a process when it is first heard of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seen {
    pub pid: u32,
    /// Epoch seconds.
    pub start_epoch: f64,
    /// Its process group. Not known for a process heard of only by its
    /// exit; it is then taken to be its parent's, as it is at a fork.
    pub pgid: Option<u32>,
    pub parent: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Identity {
    pub pid: u32,
    pub start_epoch: f64,
    pub pgid: Option<u32>,
    pub span_id: SpanId,
    pub trace_id: TraceId,
    /// Its parent's span, if its parent is part of the same trace.
    pub parent_span_id: Option<SpanId>,
    ended: Option<f64>,
    registered: f64,
}

/// A process group, as the root of a trace.
#[derive(Debug, Clone, Copy)]
struct Origin {
    start_epoch: f64,
    trace_id: TraceId,
    used: f64,
}

pub struct Lineage {
    seed: u64,
    max_age: f64,
    known: HashMap<u32, Identity>,
    groups: HashMap<u32, Origin>,
}

/// FNV-1a. The ids have to be the same after a restart of the collector,
/// and the standard library's hasher does not promise to be.
fn hash(seed: u64, kind: u8, number: u32, start_epoch: f64) -> u64 {
    let mut state = 0xcbf2_9ce4_8422_2325_u64 ^ seed;
    let micros = (start_epoch * 1e6).round() as i64;
    for byte in [kind]
        .into_iter()
        .chain(number.to_le_bytes())
        .chain(micros.to_le_bytes())
    {
        state ^= u64::from(byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // FNV mixes its last bytes least; this spreads them.
    state ^= state >> 32;
    state = state.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^ (state >> 29)
}

fn trace_id(seed: u64, kind: u8, number: u32, start_epoch: f64) -> TraceId {
    let mut id = [0_u8; 16];
    id[..8].copy_from_slice(&hash(seed, kind, number, start_epoch).to_be_bytes());
    id[8..].copy_from_slice(&hash(!seed, kind, number, start_epoch).to_be_bytes());
    // All zeroes means "no trace"; an id that hashed to it is nudged off.
    if id == [0; 16] {
        id[15] = 1;
    }
    id
}

impl Lineage {
    /// `boot_id` tells this boot's pids from the last boot's, and this
    /// host's from another's: it is whatever names the two.
    pub fn new(boot_id: &str, max_age: f64) -> Self {
        let seed = boot_id
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |state, byte| {
                (state ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        Self {
            seed,
            max_age,
            known: HashMap::new(),
            groups: HashMap::new(),
        }
    }

    fn same(a: f64, b: f64) -> bool {
        (a - b).abs() <= START_TOLERANCE
    }

    /// The identity of a process that has been registered.
    pub fn get(&self, pid: u32, start_epoch: f64) -> Option<&Identity> {
        self.known
            .get(&pid)
            .filter(|known| Self::same(known.start_epoch, start_epoch))
    }

    /// Give a process its identity, or return the one it has.
    ///
    /// `find` looks a process up by pid, among whatever the caller knows
    /// of: it is asked for a parent, or a group's leader, that has not
    /// been registered itself.
    pub fn register(
        &mut self,
        seen: Seen,
        now: f64,
        find: &mut dyn FnMut(u32) -> Option<Seen>,
    ) -> Identity {
        self.register_within(seen, now, find, DEPTH)
    }

    fn register_within(
        &mut self,
        seen: Seen,
        now: f64,
        find: &mut dyn FnMut(u32) -> Option<Seen>,
        depth: u8,
    ) -> Identity {
        if let Some(known) = self.get(seen.pid, seen.start_epoch) {
            return *known;
        }
        let parent = seen
            .parent
            .filter(|parent| *parent != 0 && *parent != seen.pid)
            .and_then(|parent| self.resolve(parent, seen.start_epoch, now, find, depth));
        let pgid = seen.pgid.or(parent.and_then(|parent| parent.pgid));
        let trace_id = self.trace_of(&seen, pgid, parent.as_ref(), now, find);

        let mut span_id = hash(self.seed, b's', seen.pid, seen.start_epoch).to_be_bytes();
        if span_id == [0; 8] {
            span_id[7] = 1;
        }
        let identity = Identity {
            pid: seen.pid,
            start_epoch: seen.start_epoch,
            pgid,
            span_id,
            trace_id,
            parent_span_id: parent
                .filter(|parent| parent.trace_id == trace_id)
                .map(|parent| parent.span_id),
            ended: None,
            registered: now,
        };
        self.known.insert(seen.pid, identity);
        identity
    }

    /// The process with this pid that was there when another started at
    /// `before`: its parent, or its group's leader.
    fn resolve(
        &mut self,
        pid: u32,
        before: f64,
        now: f64,
        find: &mut dyn FnMut(u32) -> Option<Seen>,
        depth: u8,
    ) -> Option<Identity> {
        let earlier = |start: f64| start <= before + START_TOLERANCE;
        if let Some(known) = self.known.get(&pid).filter(|k| earlier(k.start_epoch)) {
            return Some(*known);
        }
        if depth == 0 {
            return None;
        }
        // A pid is given out again: a process that started after this one
        // has the pid of its parent, and is not its parent.
        let seen = find(pid).filter(|seen| seen.pid == pid && earlier(seen.start_epoch))?;
        Some(self.register_within(seen, now, find, depth - 1))
    }

    fn trace_of(
        &mut self,
        seen: &Seen,
        pgid: Option<u32>,
        parent: Option<&Identity>,
        now: f64,
        find: &mut dyn FnMut(u32) -> Option<Seen>,
    ) -> TraceId {
        if let Some(group) = pgid {
            if !self.groups.contains_key(&group) {
                // A group is as old as its leader, if its leader can be
                // found; and as the first of it to be seen, if not.
                let leader = if group == seen.pid {
                    None
                } else {
                    self.known
                        .get(&group)
                        .map(|known| known.start_epoch)
                        .or_else(|| find(group).map(|leader| leader.start_epoch))
                        .filter(|start| *start <= seen.start_epoch + START_TOLERANCE)
                };
                let start_epoch = leader.unwrap_or(seen.start_epoch);
                let origin = Origin {
                    start_epoch,
                    trace_id: trace_id(self.seed, b'g', group, start_epoch),
                    used: now,
                };
                self.groups.insert(group, origin);
            }
            let origin = self.groups.get_mut(&group).expect("just ensured");
            origin.used = now;
            if seen.start_epoch - origin.start_epoch <= self.max_age {
                return origin.trace_id;
            }
        }
        // Its group is older than a job gets: a daemon's. What a daemon
        // starts is the root of its own trace, and what that starts in
        // turn is part of it.
        match parent {
            Some(parent)
                if parent.pgid == pgid && seen.start_epoch - parent.start_epoch <= self.max_age =>
            {
                parent.trace_id
            }
            _ => trace_id(self.seed, b'p', seen.pid, seen.start_epoch),
        }
    }

    /// The process has ended. Its identity is kept a while longer, for the
    /// children that end after it.
    pub fn ended(&mut self, pid: u32, start_epoch: f64, at: f64) {
        if let Some(known) = self
            .known
            .get_mut(&pid)
            .filter(|known| Self::same(known.start_epoch, start_epoch))
        {
            known.ended = Some(at);
        }
    }

    /// Let go of processes that ended more than `grace` seconds ago, of
    /// those that are gone without their end having been heard of, and of
    /// groups that nothing has joined for longer than a job lasts.
    pub fn prune(&mut self, now: f64, grace: f64, alive: impl Fn(u32) -> bool) {
        self.known.retain(|pid, known| match known.ended {
            Some(at) => now - at <= grace,
            None => now - known.registered <= grace || alive(*pid),
        });
        let max_age = self.max_age;
        self.groups
            .retain(|_, origin| now - origin.used <= max_age + grace);
        settle(&mut self.known);
        settle(&mut self.groups);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.known.len()
    }
}

/// An id as hexadecimal, the way trace tools write one.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: f64 = 3600.0;

    fn lineage() -> Lineage {
        Lineage::new("9d8f7e6a-boot", HOUR)
    }

    fn seen(pid: u32, start_epoch: f64, pgid: u32, parent: u32) -> Seen {
        Seen {
            pid,
            start_epoch,
            pgid: Some(pgid),
            parent: Some(parent),
        }
    }

    /// Register each of `all` in turn, with the rest there to be found.
    fn register(lineage: &mut Lineage, all: &[Seen]) -> Vec<Identity> {
        all.iter()
            .map(|one| {
                lineage.register(*one, 0.0, &mut |pid| {
                    all.iter().copied().find(|other| other.pid == pid)
                })
            })
            .collect()
    }

    #[test]
    fn a_build_is_one_trace_and_the_shell_is_not_in_it() {
        let mut lineage = lineage();
        let ids = register(
            &mut lineage,
            &[
                seen(100, 1000.0, 100, 1),   // the shell
                seen(200, 5000.0, 200, 100), // cargo, a job of the shell's
                seen(201, 5001.0, 200, 200), // rustc
                seen(202, 5002.0, 200, 201), // the linker rustc runs
            ],
        );
        let (shell, cargo, rustc, linker) = (ids[0], ids[1], ids[2], ids[3]);

        assert_ne!(cargo.trace_id, shell.trace_id);
        assert_eq!(rustc.trace_id, cargo.trace_id);
        assert_eq!(linker.trace_id, cargo.trace_id);
        // The job's root has no parent in the trace.
        assert_eq!(cargo.parent_span_id, None);
        assert_eq!(rustc.parent_span_id, Some(cargo.span_id));
        assert_eq!(linker.parent_span_id, Some(rustc.span_id));
    }

    #[test]
    fn a_child_that_ends_first_already_carries_its_parents_ids() {
        // The order spans are made in: the child's, then its parent's.
        let all = [seen(201, 5001.0, 200, 200), seen(200, 5000.0, 200, 100)];
        let mut lineage = lineage();
        let ids = register(&mut lineage, &all);
        assert_eq!(ids[0].trace_id, ids[1].trace_id);
        assert_eq!(ids[0].parent_span_id, Some(ids[1].span_id));
    }

    #[test]
    fn a_pipeline_is_one_trace_with_a_root_for_each_command() {
        let mut lineage = lineage();
        let ids = register(
            &mut lineage,
            &[
                seen(100, 1000.0, 100, 1),
                seen(300, 5000.0, 300, 100), // cat, the group's leader
                seen(301, 5000.1, 300, 100), // grep
                seen(302, 5000.2, 300, 100), // wc
            ],
        );
        assert_eq!(ids[1].trace_id, ids[2].trace_id);
        assert_eq!(ids[1].trace_id, ids[3].trace_id);
        // Each is the shell's child, and the shell is another trace's.
        assert!(ids[1..].iter().all(|id| id.parent_span_id.is_none()));
    }

    #[test]
    fn what_a_daemon_starts_is_a_trace_of_its_own() {
        let mut lineage = lineage();
        let day = 24.0 * HOUR;
        let ids = register(
            &mut lineage,
            &[
                seen(50, 1000.0, 50, 1),        // the daemon
                seen(60, 1000.0 + day, 50, 50), // a worker, a day later
                seen(61, 1001.0 + day, 50, 60), // what the worker runs
                seen(70, 2000.0 + day, 50, 50), // another worker
            ],
        );
        let (daemon, worker, child, other) = (ids[0], ids[1], ids[2], ids[3]);
        assert_ne!(worker.trace_id, daemon.trace_id);
        assert_eq!(worker.parent_span_id, None);
        assert_eq!(child.trace_id, worker.trace_id);
        assert_eq!(child.parent_span_id, Some(worker.span_id));
        assert_ne!(other.trace_id, worker.trace_id);
    }

    #[test]
    fn a_daemons_first_hour_is_its_own_trace() {
        let mut lineage = lineage();
        let ids = register(
            &mut lineage,
            &[
                seen(50, 1000.0, 50, 1),
                seen(51, 1000.0 + 0.5 * HOUR, 50, 50),
            ],
        );
        assert_eq!(ids[1].trace_id, ids[0].trace_id);
        assert_eq!(ids[1].parent_span_id, Some(ids[0].span_id));
    }

    #[test]
    fn a_process_known_only_by_its_exit_is_in_its_parents_group() {
        let mut lineage = lineage();
        let cargo = lineage.register(seen(200, 5000.0, 200, 100), 0.0, &mut |_| None);
        let child = lineage.register(
            Seen {
                pid: 250,
                start_epoch: 5003.0,
                pgid: None,
                parent: Some(200),
            },
            0.0,
            &mut |_| None,
        );
        assert_eq!(child.pgid, Some(200));
        assert_eq!(child.trace_id, cargo.trace_id);
        assert_eq!(child.parent_span_id, Some(cargo.span_id));
    }

    #[test]
    fn a_process_with_no_one_known_above_it_is_a_trace_of_one() {
        let mut lineage = lineage();
        let orphan = lineage.register(
            Seen {
                pid: 900,
                start_epoch: 5000.0,
                pgid: None,
                parent: Some(899),
            },
            0.0,
            &mut |_| None,
        );
        assert_eq!(orphan.parent_span_id, None);
        assert_ne!(orphan.trace_id, [0; 16]);
    }

    #[test]
    fn a_later_process_with_the_parents_pid_is_not_the_parent() {
        let mut lineage = lineage();
        // Pid 200 has been given out again, after the child started.
        let imposter = seen(200, 9000.0, 200, 1);
        let child = lineage.register(seen(201, 5001.0, 201, 200), 0.0, &mut |pid| {
            (pid == 200).then_some(imposter)
        });
        assert_eq!(child.parent_span_id, None);
        assert_eq!(lineage.len(), 1);

        // And one registered under that pid is replaced by the process
        // that has it now.
        let first = lineage.register(seen(300, 100.0, 300, 1), 0.0, &mut |_| None);
        let second = lineage.register(seen(300, 8000.0, 300, 1), 0.0, &mut |_| None);
        assert_ne!(first.span_id, second.span_id);
        assert!(lineage.get(300, 100.0).is_none());
        assert!(lineage.get(300, 8001.0).is_some());
    }

    #[test]
    fn a_process_is_registered_once() {
        let mut lineage = lineage();
        let first = lineage.register(seen(200, 5000.0, 200, 100), 0.0, &mut |_| None);
        // Seen again, from another source, a second off, in another group.
        let again = lineage.register(seen(200, 5001.0, 777, 1), 0.0, &mut |_| None);
        assert_eq!(first, again);
    }

    #[test]
    fn ids_are_the_same_after_a_restart_and_differ_between_boots() {
        let one = Lineage::new("boot-a", HOUR).register(seen(7, 50.0, 7, 1), 0.0, &mut |_| None);
        let same = Lineage::new("boot-a", HOUR).register(seen(7, 50.0, 7, 1), 9.0, &mut |_| None);
        let other = Lineage::new("boot-b", HOUR).register(seen(7, 50.0, 7, 1), 0.0, &mut |_| None);
        // Two hosts, booted from one image.
        let twin =
            Lineage::new("edge-7 boot-a", HOUR).register(seen(7, 50.0, 7, 1), 0.0, &mut |_| None);
        assert_ne!(one.trace_id, twin.trace_id);
        assert_ne!(one.span_id, twin.span_id);
        assert_eq!(one.span_id, same.span_id);
        assert_eq!(one.trace_id, same.trace_id);
        assert_ne!(one.span_id, other.span_id);
        assert_ne!(one.trace_id, other.trace_id);
    }

    #[test]
    fn ids_of_neighbouring_processes_are_not_neighbours() {
        let mut lineage = lineage();
        let ids: Vec<Identity> = (0..2000)
            .map(|n| lineage.register(seen(1000 + n, 5000.0, 1000 + n, 1), 0.0, &mut |_| None))
            .collect();
        let mut spans: Vec<SpanId> = ids.iter().map(|id| id.span_id).collect();
        let mut traces: Vec<TraceId> = ids.iter().map(|id| id.trace_id).collect();
        spans.sort_unstable();
        spans.dedup();
        traces.sort_unstable();
        traces.dedup();
        assert_eq!(spans.len(), 2000);
        assert_eq!(traces.len(), 2000);
        // Every byte of an id varies.
        for byte in 0..8 {
            let mut values: Vec<u8> = spans.iter().map(|id| id[byte]).collect();
            values.sort_unstable();
            values.dedup();
            assert!(
                values.len() > 200,
                "byte {byte} takes {} values",
                values.len()
            );
        }
    }

    #[test]
    fn the_ended_are_kept_for_their_children_and_then_let_go() {
        let mut lineage = lineage();
        lineage.register(seen(200, 5000.0, 200, 100), 10.0, &mut |_| None);
        lineage.register(seen(201, 5001.0, 200, 200), 10.0, &mut |_| None);
        lineage.ended(200, 5000.0, 20.0);

        lineage.prune(40.0, 30.0, |_| true);
        assert_eq!(lineage.len(), 2);
        // A child that ends after its parent still finds it.
        let late = lineage.register(seen(202, 5002.0, 200, 200), 40.0, &mut |_| None);
        assert!(late.parent_span_id.is_some());

        lineage.prune(60.0, 30.0, |_| true);
        assert!(lineage.get(200, 5000.0).is_none());
        assert!(lineage.get(201, 5001.0).is_some());

        // Gone, and its end was never heard of.
        lineage.prune(100.0, 30.0, |pid| pid != 201);
        assert!(lineage.get(201, 5001.0).is_none());
        assert!(lineage.get(202, 5002.0).is_some());
    }
}
