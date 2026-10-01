//! The collection loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::accounting::{exit_event, span_of, vanished_event, Known};
use crate::cgroup::{unit_of, Containers};
use crate::collect::process::{ExitedCpu, ProcessCollector, Tracked, Units};
use crate::collect::system::SystemCollector;
use crate::collect::units::UnitCollector;
use crate::collect::users::Users;
use crate::lineage::{Lineage, Seen, START_TOLERANCE};
use crate::model::{no_labels, Event, MetricBatch, Span};
use crate::procevents::{Exec, ExecListener};
use crate::procfs::process::{group_name, is_launcher, parse_pid_stat};
use crate::procfs::ProcRoot;
use crate::queue::settle;
use crate::sink::{Sink, Tick};
use crate::taskstats::{epoch_now, Aggregator, Listener, ProcessExit};

/// How long the threads of a process are remembered after the process is
/// gone without its last record.
const PARTIAL_IDLE: f64 = 120.0;
/// How often a failure that keeps happening is mentioned again.
const REPEAT_EVERY: u64 = 30;

pub struct Schedule {
    pub system: Option<Duration>,
    pub processes: Option<Duration>,
    pub flush: Duration,
    pub maintain: Option<Duration>,
}

/// What an engine is made of. Each collector and listener is optional:
/// turned off, unavailable on this host, or refused to this user.
pub struct Parts {
    pub host: String,
    pub root: ProcRoot,
    pub system: Option<SystemCollector>,
    pub processes: Option<ProcessCollector>,
    pub units: Option<UnitCollector>,
    pub exits: Option<Listener>,
    pub execs: Option<ExecListener>,
    /// Absent if spans are not kept.
    pub lineage: Option<Lineage>,
    pub clock: Clock,
    pub sink: Box<dyn Sink>,
    pub schedule: Schedule,
}

/// What turns a process's start, in ticks since boot, into a time.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub units: Units,
    /// Epoch seconds.
    pub boot_epoch: f64,
}

/// Everywhere a process can be looked for, to give another its place in a
/// trace: its parent, or its group's leader.
struct Sources<'a> {
    processes: Option<&'a ProcessCollector>,
    described: &'a HashMap<u32, Exec>,
    unexplained: &'a [Tracked],
    /// Processes whose exit is being accounted in this same tick.
    ending: &'a HashMap<u32, Seen>,
    root: &'a ProcRoot,
    clock: Clock,
}

fn seen_tracked(tracked: &Tracked) -> Seen {
    Seen {
        pid: tracked.pid,
        start_epoch: tracked.start_epoch,
        pgid: Some(tracked.pgid),
        parent: Some(tracked.parent),
    }
}

fn seen_exec(exec: &Exec) -> Seen {
    Seen {
        pid: exec.pid,
        start_epoch: exec.start_epoch,
        pgid: Some(exec.pgid),
        parent: Some(exec.ppid),
    }
}

/// The kernel's record of an exit does not say which group the process was
/// in, and names the parent it had at the end.
fn seen_exit(exit: &ProcessExit) -> Seen {
    Seen {
        pid: exit.pid,
        start_epoch: exit.start_epoch,
        pgid: None,
        parent: Some(exit.ppid),
    }
}

impl Sources<'_> {
    /// From what knew the process longest to what knows it least. The
    /// last is the process itself: alive, and not yet seen by a sweep or
    /// heard of by an exec, as a subshell is that is still running.
    fn find(&self, pid: u32) -> Option<Seen> {
        if let Some(tracked) = self.processes.and_then(|c| c.get(pid)) {
            return Some(seen_tracked(tracked));
        }
        if let Some(tracked) = self.unexplained.iter().find(|t| t.pid == pid) {
            return Some(seen_tracked(tracked));
        }
        if let Some(exec) = self.described.get(&pid) {
            return Some(seen_exec(exec));
        }
        if let Some(seen) = self.ending.get(&pid) {
            return Some(*seen);
        }
        let mut buf = String::new();
        self.root.read_pid(pid, "stat", &mut buf).ok()?;
        let stat = parse_pid_stat(&buf)?;
        Some(Seen {
            pid,
            start_epoch: self.clock.boot_epoch
                + stat.start_ticks as f64 / self.clock.units.ticks_per_second,
            pgid: Some(stat.pgrp),
            parent: Some(stat.ppid),
        })
    }
}

/// What a process was running, and in which unit. Each part is empty if
/// it was never learned.
#[derive(Default)]
struct Described {
    cmdline: String,
    started_as: String,
    exe: String,
    unit: String,
}

pub struct Engine {
    parts: Parts,
    users: Users,
    containers: Arc<Containers>,
    aggregator: Aggregator,
    /// Processes a sweep found gone, held for one sweep in case the
    /// kernel's record of their exit is still on its way.
    unexplained: Vec<Tracked>,
    /// Processes described when they called exec and not seen by a sweep
    /// since, waiting for the record of their exit.
    described: HashMap<u32, Exec>,
    exits_seen: u64,
    write_failures: u64,
}

/// The next multiple of `interval` strictly after `after`, in epoch
/// seconds. Readings land on round times, so two hosts' lines are sampled
/// at the same moments and a bucket boundary never splits an interval.
fn next_tick(after: f64, interval: Duration) -> f64 {
    let step = interval.as_secs_f64().max(1.0);
    ((after / step).floor() + 1.0) * step
}

impl Engine {
    pub fn new(parts: Parts) -> Self {
        Self {
            parts,
            users: Users::default(),
            containers: Arc::default(),
            aggregator: Aggregator::default(),
            unexplained: Vec::new(),
            described: HashMap::new(),
            exits_seen: 0,
            write_failures: 0,
        }
    }

    /// Collect until `stop` is set, then flush and close the sink.
    pub fn run(&mut self, stop: &Arc<AtomicBool>) -> Result<()> {
        let schedule = &self.parts.schedule;
        let (every_system, every_sweep) = (schedule.system, schedule.processes);
        let (every_flush, every_maintain) = (schedule.flush, schedule.maintain);

        let started = epoch_now();
        let mut system_due = every_system.map(|i| next_tick(started, i));
        let mut processes_due = every_sweep.map(|i| next_tick(started, i));
        let mut flush_due = Instant::now() + every_flush;
        let mut maintain_due = every_maintain.map(|i| Instant::now() + i);

        // What the last run flushed and did not live to compact is
        // compacted now. Compaction is due an hour from the start, and a
        // collector that is restarted more often than that would
        // otherwise never come to it.
        if every_maintain.is_some() {
            self.report("maintenance", |sink| sink.maintain());
        }

        // A first reading now, so that the first tick has something to take
        // a difference against and reports rates.
        self.tick(started.floor() as i64, true, true, true);

        while !stop.load(Ordering::Relaxed) {
            let due = [system_due, processes_due]
                .into_iter()
                .flatten()
                .fold(f64::INFINITY, f64::min);
            if !due.is_finite() {
                break;
            }
            if !sleep_until(due, stop) {
                break;
            }

            let wall = epoch_now();
            let system = system_due.is_some_and(|at| at <= wall);
            let processes = processes_due.is_some_and(|at| at <= wall);
            self.tick(due.round() as i64, system, processes, false);

            // From the time it is now, not the time it was due: a sweep that
            // overran, or a host that slept, skips the ticks it missed.
            let after = epoch_now();
            if system {
                system_due = every_system.map(|i| next_tick(after, i));
            }
            if processes {
                processes_due = every_sweep.map(|i| next_tick(after, i));
            }

            let now = Instant::now();
            if now >= flush_due {
                self.report("flush", |sink| sink.flush());
                flush_due = now + every_flush;
            }
            if maintain_due.is_some_and(|at| now >= at) {
                self.report("maintenance", |sink| sink.maintain());
                maintain_due = every_maintain.map(|i| now + i);
            }
        }

        // What ended since the last sweep is accounted before closing.
        self.tick(epoch_now().round() as i64, false, true, true);
        let unexplained = std::mem::take(&mut self.unexplained);
        let identities: Vec<_> = unexplained
            .iter()
            .map(|tracked| self.identity_of(tracked.pid, tracked.start_epoch))
            .collect();
        let leftover: Vec<Event> = unexplained
            .iter()
            .map(|tracked| vanished_event(tracked, epoch_now()))
            .collect();
        if !leftover.is_empty() {
            let batch = MetricBatch::new(epoch_now() as i64);
            let spans: Vec<Span> = leftover
                .iter()
                .zip(&identities)
                .filter_map(|(record, identity)| Some(span_of(record, identity.as_ref()?)))
                .collect();
            let tick = Tick {
                metrics: &batch,
                events: &leftover,
                spans: &spans,
            };
            if let Err(error) = self.parts.sink.write(&self.parts.host, &tick) {
                eprintln!("timeless-acct: final write failed: {error:#}");
            }
        }
        self.parts.sink.close()
    }

    /// One reading. `quiet` readings are for the differences they make
    /// possible (or for the exits they account): their samples are not
    /// stored, because they do not fall on a round time.
    pub fn tick(&mut self, ts: i64, system: bool, processes: bool, quiet: bool) {
        let now = Instant::now();
        let wall = epoch_now();
        let mut batch = MetricBatch::new(ts);
        let mut events = Vec::new();
        let mut spans = Vec::new();

        if system {
            if let Some(collector) = &mut self.parts.system {
                collector.collect(now, &mut batch);
            }
        }
        if processes {
            // In this order: an exec is heard of before the exit it
            // describes is accounted, and an exit before the sweep that
            // would find the process gone.
            // Containers come and go; what is named for one is named for
            // what runs it as of now.
            if let Some(units) = &mut self.parts.units {
                self.containers = units.containers();
                if let Some(processes) = &mut self.parts.processes {
                    processes.set_containers(Arc::clone(&self.containers));
                }
            }
            let execs = self.take_execs();
            let exits = self.take_exits();
            // Everything heard of since the last tick is laid out before
            // any of it is given its place. A child is heard of before its
            // parent as often as after: a subshell calls exec after what
            // it ran did, and ends after it too.
            let ending: HashMap<u32, Seen> = exits
                .iter()
                .map(|exit| (exit.pid, seen_exit(exit)))
                .collect();
            for seen in execs {
                self.place(seen, wall, &ending);
            }
            let exited = self.account_exits(exits, &ending, wall, &mut events, &mut spans);

            let mut swept = None;
            let mut cgroups = HashMap::new();
            if let Some(collector) = &mut self.parts.processes {
                let sweep = collector.sweep(now, wall, &exited, &mut batch);
                swept = Some((sweep.processes, sweep.reported));
                cgroups = sweep.cgroups;
                // Those new to the sweep are given their place while their
                // parents are there to be found.
                let admitted: Vec<Seen> = sweep
                    .admitted
                    .iter()
                    .filter_map(|pid| collector.get(*pid).map(seen_tracked))
                    .collect();
                for seen in admitted {
                    self.place(seen, wall, &HashMap::new());
                }
                self.account_vanished(sweep.vanished, wall, &mut events, &mut spans);
            }
            if let Some(collector) = &mut self.parts.units {
                collector.collect(now, &cgroups, &mut batch);
            }
            self.forget(wall);

            let none = no_labels();
            if let Some((seen, reported)) = swept {
                batch.push("acct_processes", &none, seen as f64);
                batch.push("acct_processes_reported", &none, reported as f64);
                batch.push("acct_sweep_seconds", &none, now.elapsed().as_secs_f64());
            }
            if let Some(listener) = &self.parts.exits {
                batch.push("acct_exits", &none, self.exits_seen as f64);
                batch.push("acct_exits_lost", &none, listener.lost() as f64);
            }
            if let Some(listener) = &self.parts.execs {
                batch.push("acct_execs", &none, listener.seen() as f64);
                batch.push("acct_execs_missed", &none, listener.missed() as f64);
                batch.push("acct_execs_lost", &none, listener.lost() as f64);
            }
            // How often this collector was told to look, so that a reader
            // elsewhere can choose its lookback from the store.
            if let Some(every) = self.parts.schedule.system {
                batch.push("acct_interval_seconds", &none, every.as_secs_f64());
            }
            if let Some(every) = self.parts.schedule.processes {
                batch.push("acct_process_interval_seconds", &none, every.as_secs_f64());
            }
            if let Some(footprint) = self.parts.sink.footprint() {
                batch.push("acct_store_bytes", &none, footprint.bytes as f64);
                if let Some(limit) = footprint.limit {
                    batch.push("acct_store_limit_bytes", &none, limit as f64);
                }
            }
        }

        if quiet {
            batch.samples.clear();
        }
        let tick = Tick {
            metrics: &batch,
            events: &events,
            spans: &spans,
        };
        if tick.is_empty() {
            return;
        }
        match self.parts.sink.write(&self.parts.host, &tick) {
            Ok(()) => {
                if self.write_failures > 0 {
                    eprintln!(
                        "timeless-acct: storing again after {} failed writes",
                        self.write_failures
                    );
                }
                self.write_failures = 0;
            }
            Err(error) => {
                if self.write_failures.is_multiple_of(REPEAT_EVERY) {
                    eprintln!("timeless-acct: write failed: {error:#}");
                }
                self.write_failures += 1;
            }
        }
    }

    /// Take in what the exec listener described. A process the sweep
    /// already tracks is brought up to date; any other is remembered until
    /// its exit is accounted or a sweep finds it.
    fn take_execs(&mut self) -> Vec<Seen> {
        let Some(listener) = &self.parts.execs else {
            return Vec::new();
        };
        let mut seen = Vec::new();
        for mut exec in listener.drain() {
            seen.push(seen_exec(&exec));
            let tracked = self
                .parts
                .processes
                .as_mut()
                .is_some_and(|c| c.note_exec(exec.pid, exec.start_ticks, &exec.described));
            if tracked {
                continue;
            }
            // A process may exec more than once. The last is what it was
            // running when it ended, and the first is what it was started
            // as.
            if let Some(before) = self
                .described
                .get(&exec.pid)
                .filter(|before| before.start_ticks == exec.start_ticks)
            {
                let first = before
                    .started_as
                    .clone()
                    .unwrap_or_else(|| before.described.cmdline.clone());
                exec.started_as = Some(first)
                    .filter(|first| *first != exec.described.cmdline && !is_launcher(first));
            }
            self.described.insert(exec.pid, exec);
        }
        seen
    }

    /// The processes that have ended since the last tick, each with its
    /// threads added together.
    fn take_exits(&mut self) -> Vec<ProcessExit> {
        let Some(listener) = &self.parts.exits else {
            return Vec::new();
        };
        let mut exits = Vec::new();
        for received in listener.drain() {
            if let Some(exit) = self.aggregator.push(&received.exit, received.at) {
                exits.push(exit);
            }
        }
        exits
    }

    /// Give a process its place in a trace, if traces are kept.
    fn place(&mut self, seen: Seen, wall: f64, ending: &HashMap<u32, Seen>) {
        let Some(lineage) = &mut self.parts.lineage else {
            return;
        };
        let sources = Sources {
            processes: self.parts.processes.as_ref(),
            described: &self.described,
            unexplained: &self.unexplained,
            ending,
            root: &self.parts.root,
            clock: self.parts.clock,
        };
        lineage.register(seen, wall, &mut |pid| sources.find(pid));
    }

    fn identity_of(&self, pid: u32, start_epoch: f64) -> Option<crate::lineage::Identity> {
        self.parts
            .lineage
            .as_ref()
            .and_then(|lineage| lineage.get(pid, start_epoch))
            .copied()
    }

    /// Turn the kernel's exit records into accounting records and spans,
    /// and return the CPU those processes used since they were last
    /// sampled.
    fn account_exits(
        &mut self,
        exits: Vec<ProcessExit>,
        ending: &HashMap<u32, Seen>,
        wall: f64,
        events: &mut Vec<Event>,
        spans: &mut Vec<Span>,
    ) -> Vec<ExitedCpu> {
        // Every one of them is given its place before any is accounted. A
        // child's record comes before its parent's, and the parent may be
        // known from nothing but its own record, further down the batch.
        if self.parts.lineage.is_some() {
            for exit in &exits {
                let sources = Sources {
                    processes: self.parts.processes.as_ref(),
                    described: &self.described,
                    unexplained: &self.unexplained,
                    ending,
                    root: &self.parts.root,
                    clock: self.parts.clock,
                };
                // As it was seen alive, if it was: that has its group.
                let seen = sources
                    .find(exit.pid)
                    .filter(|seen| (seen.start_epoch - exit.start_epoch).abs() <= START_TOLERANCE)
                    .unwrap_or_else(|| seen_exit(exit));
                self.place(seen, wall, ending);
            }
        }

        let mut exited = Vec::new();
        for exit in exits {
            self.exits_seen += 1;
            let same = |start: f64| (start - exit.start_epoch).abs() <= START_TOLERANCE;

            // Known from sampling: either still tracked, or found gone by
            // the last sweep and waiting here for this record.
            let waiting = self
                .unexplained
                .iter()
                .position(|t| t.pid == exit.pid && same(t.start_epoch));
            let tracked = match waiting {
                Some(index) => Some(self.unexplained.swap_remove(index)),
                None => self.parts.processes.as_mut().and_then(|collector| {
                    collector
                        .find(exit.pid, exit.start_epoch, START_TOLERANCE)
                        .is_some()
                        .then(|| collector.forget(exit.pid))
                        .flatten()
                }),
            };
            // Known from its exec. The pid alone is not enough: it may have
            // belonged to an earlier process whose exit record was lost.
            let described = self
                .described
                .remove(&exit.pid)
                .filter(|exec| tracked.is_none() && same(exec.start_epoch));

            let user = self.users.name(exit.uid).to_string();
            let was = self.what_it_was(&exit, tracked.as_ref(), described.as_ref());
            let known = Known {
                cmdline: &was.cmdline,
                started_as: &was.started_as,
                exe: &was.exe,
                unit: &was.unit,
            };

            // The kernel's own threads are the children of pid 2, and have
            // no executable.
            let kernel_thread = match &tracked {
                Some(tracked) => tracked.kernel_thread,
                None => exit.ppid == 2 || exit.pid == 2,
            };
            let group = match &tracked {
                Some(tracked) => tracked.group.clone(),
                None => group_name(&exit.comm, kernel_thread).to_string(),
            };
            let sampled = tracked.as_ref().map_or(0.0, Tracked::cpu_seconds);
            exited.push(ExitedCpu {
                group,
                user: user.clone(),
                cpu_seconds: (exit.cpu_seconds() - sampled).max(0.0),
            });
            let record = exit_event(&exit, tracked.as_ref(), known, &user);
            if let Some(lineage) = &mut self.parts.lineage {
                // A kernel thread is not part of anyone's job.
                if let Some(identity) = lineage
                    .get(exit.pid, exit.start_epoch)
                    .filter(|_| !kernel_thread)
                {
                    spans.push(span_of(&record, identity));
                }
                lineage.ended(exit.pid, exit.start_epoch, exit.end_epoch);
            }
            events.push(record);
        }
        exited
    }

    /// What a process that has ended was running, and in which unit, from
    /// the best source there is:
    ///
    /// 1. the sweep, if it lived to one;
    /// 2. the moment it called exec;
    /// 3. its parent, if it never called exec: it ran what its parent runs.
    ///
    /// For the third, the names are compared, because the parent on record
    /// at a process's exit is whoever it was last handed to, if its own had
    /// gone before it.
    fn what_it_was(
        &mut self,
        exit: &ProcessExit,
        tracked: Option<&Tracked>,
        described: Option<&Exec>,
    ) -> Described {
        let users = &mut self.users;
        let containers = &self.containers;
        let mut unit_of = |cgroup: &str| {
            unit_of(cgroup, &mut |uid| users.name(uid).to_string(), containers).unwrap_or_default()
        };
        let from = |tracked: &Tracked| Described {
            cmdline: tracked.cmdline.clone(),
            started_as: tracked.started_as.clone(),
            exe: tracked.exe.clone(),
            unit: tracked.unit.clone(),
        };
        let mut from_exec = |exec: &Exec| Described {
            cmdline: exec.described.cmdline.clone(),
            started_as: exec.started_as.clone().unwrap_or_default(),
            exe: exec.described.exe.clone(),
            unit: unit_of(&exec.described.cgroup),
        };

        if let Some(tracked) = tracked {
            return from(tracked);
        }
        if let Some(exec) = described {
            return from_exec(exec);
        }
        let parent = self.parts.processes.as_ref().and_then(|c| c.get(exit.ppid));
        if exit.forked {
            // What its parent runs now; what its parent ran before that
            // is the parent's history, and not the child's.
            if let Some(parent) = parent.filter(|p| p.comm == exit.comm) {
                return Described {
                    started_as: String::new(),
                    ..from(parent)
                };
            }
            // A parent that has not lived to a sweep itself.
            if let Some(parent) = self
                .described
                .get(&exit.ppid)
                .filter(|p| p.comm == exit.comm && p.start_epoch <= exit.start_epoch + 1.0)
            {
                return Described {
                    started_as: String::new(),
                    ..from_exec(parent)
                };
            }
        }
        // Nothing was learned of it but where it probably ran: a child
        // starts in its parent's control group, and few are moved out.
        Described {
            unit: parent.map(|p| p.unit.clone()).unwrap_or_default(),
            ..Described::default()
        }
    }

    /// Processes the sweep found gone. With the kernel reporting exits,
    /// each waits one sweep for its record; if none comes (it was lost, or
    /// accounting is off), what sampling knew is the record.
    fn account_vanished(
        &mut self,
        vanished: Vec<Tracked>,
        wall: f64,
        events: &mut Vec<Event>,
        spans: &mut Vec<Span>,
    ) {
        let overdue = std::mem::take(&mut self.unexplained);
        let unrecorded = if self.parts.exits.is_some() {
            self.unexplained = vanished;
            overdue
        } else {
            vanished
        };
        for tracked in unrecorded {
            let record = vanished_event(&tracked, wall);
            if let Some(lineage) = &mut self.parts.lineage {
                if let Some(identity) = lineage
                    .get(tracked.pid, tracked.start_epoch)
                    .filter(|_| !tracked.kernel_thread)
                {
                    spans.push(span_of(&record, identity));
                }
                lineage.ended(tracked.pid, tracked.start_epoch, wall);
            }
            events.push(record);
        }
    }

    /// Let go of what will not be asked for again.
    fn forget(&mut self, wall: f64) {
        let root = &self.parts.root;
        self.aggregator.prune(wall, PARTIAL_IDLE, |pid| {
            root.proc_path(&pid.to_string()).exists()
        });

        // A described process that has lived to a sweep is tracked now,
        // and one that has not will have its exit accounted at the next.
        let sweep = self
            .parts
            .schedule
            .processes
            .map_or(0.0, |i| i.as_secs_f64());
        let keep = 2.0 * sweep + START_TOLERANCE;
        let processes = &self.parts.processes;
        self.described.retain(|pid, exec| {
            wall - exec.at <= keep
                && !processes
                    .as_ref()
                    .is_some_and(|c| c.find(*pid, exec.start_epoch, START_TOLERANCE).is_some())
        });
        settle(&mut self.described);

        // The ended are kept for the children that end after them, which
        // is as long as a process can go unseen: until the next sweep.
        if let Some(lineage) = &mut self.parts.lineage {
            lineage.prune(wall, keep, |pid| {
                processes.as_ref().is_some_and(|c| c.get(pid).is_some())
                    || root.proc_path(&pid.to_string()).exists()
            });
        }
    }

    fn report(&mut self, what: &str, action: impl FnOnce(&mut dyn Sink) -> Result<()>) {
        if let Err(error) = action(self.parts.sink.as_mut()) {
            eprintln!("timeless-acct: {what} failed: {error:#}");
        }
    }
}

/// Sleep until the wall clock reads `due`, waking to look at `stop`.
/// Returns false if stopped.
fn sleep_until(due: f64, stop: &Arc<AtomicBool>) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let remaining = due - epoch_now();
        if remaining <= 0.0 {
            return true;
        }
        thread::sleep(Duration::from_secs_f64(remaining.min(0.2)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::process::{ProcessOptions, Units};
    use crate::sink::stdout::StdoutSink;
    use crate::testutil::Fixture;

    const BOOT: f64 = 1_000_000.0;

    fn engine(fixture: &Fixture, sweep: bool) -> Engine {
        let units = Units {
            ticks_per_second: 100.0,
            page_bytes: 4096,
        };
        let mut processes =
            ProcessCollector::new(fixture.root(), ProcessOptions::default(), units, BOOT);
        if sweep {
            processes.sweep(Instant::now(), BOOT + 50.0, &[], &mut MetricBatch::new(0));
        }
        Engine::new(Parts {
            host: "test".into(),
            root: fixture.root(),
            system: None,
            processes: Some(processes),
            units: None,
            exits: None,
            execs: None,
            lineage: Some(Lineage::new("boot", 3600.0)),
            clock: Clock {
                units,
                boot_epoch: BOOT,
            },
            sink: Box::new(StdoutSink),
            schedule: Schedule {
                system: None,
                processes: Some(Duration::from_secs(10)),
                flush: Duration::from_secs(60),
                maintain: None,
            },
        })
    }

    /// A live process, as the sweep will find it.
    fn alive(fixture: &Fixture, pid: u32, comm: &str, cgroup: &str) {
        fixture.proc_file(
            &format!("{pid}/stat"),
            &format!("{pid} ({comm}) S 1 1 1 0 -1 0 0 0 0 0 5 1 0 0 20 0 1 0 100 1000 10 0\n"),
        );
        fixture.proc_file(&format!("{pid}/status"), "Uid:\t0\t0\t0\t0\n");
        fixture.proc_file(
            &format!("{pid}/cmdline"),
            &format!("/usr/bin/{comm}\0-D\0/data\0"),
        );
        fixture.proc_file(&format!("{pid}/cgroup"), &format!("0::{cgroup}\n"));
    }

    fn exec(pid: u32, comm: &str, start_epoch: f64) -> Exec {
        Exec {
            pid,
            at: start_epoch,
            start_ticks: 0,
            start_epoch,
            ppid: 1,
            pgid: pid,
            comm: comm.into(),
            described: crate::procfs::Described {
                cmdline: format!("{comm} --from-exec"),
                exe: format!("/opt/{comm}"),
                cgroup: "/system.slice/build.service".into(),
            },
            started_as: Some("sh -c build".into()),
        }
    }

    fn exit(comm: &str, ppid: u32, forked: bool) -> ProcessExit {
        ProcessExit {
            pid: 900,
            ppid,
            comm: comm.into(),
            start_epoch: BOOT + 60.0,
            forked,
            ..ProcessExit::default()
        }
    }

    #[test]
    fn a_process_is_described_by_its_own_exec() {
        let fixture = Fixture::new("engine_own_exec");
        alive(&fixture, 50, "make", "/system.slice/other.service");
        let mut engine = engine(&fixture, true);
        let own = exec(900, "cc1plus", BOOT + 60.0);

        let was = engine.what_it_was(&exit("cc1plus", 50, false), None, Some(&own));
        assert_eq!(was.cmdline, "cc1plus --from-exec");
        assert_eq!(was.started_as, "sh -c build");
        assert_eq!(was.exe, "/opt/cc1plus");
        // Where it was when it called exec, not where its parent is.
        assert_eq!(was.unit, "build.service");
    }

    #[test]
    fn a_child_that_never_called_exec_is_described_as_its_parent() {
        let fixture = Fixture::new("engine_forked");
        alive(&fixture, 50, "postgres", "/system.slice/postgresql.service");
        let mut engine = engine(&fixture, true);

        let was = engine.what_it_was(&exit("postgres", 50, true), None, None);
        assert_eq!(was.cmdline, "/usr/bin/postgres -D /data");
        assert_eq!(was.unit, "postgresql.service");
    }

    #[test]
    fn a_parent_too_young_for_a_sweep_is_known_from_its_exec() {
        let fixture = Fixture::new("engine_young_parent");
        let mut engine = engine(&fixture, true);
        engine
            .described
            .insert(70, exec(70, "build.sh", BOOT + 59.0));

        let was = engine.what_it_was(&exit("build.sh", 70, true), None, None);
        assert_eq!(was.cmdline, "build.sh --from-exec");
        assert_eq!(was.unit, "build.service");
        // What its parent ran before is not what the child started as.
        assert_eq!(was.started_as, "");

        // A process with that pid that started after the child is not its
        // parent, whatever it is called.
        engine
            .described
            .insert(70, exec(70, "build.sh", BOOT + 90.0));
        let was = engine.what_it_was(&exit("build.sh", 70, true), None, None);
        assert_eq!(was.cmdline, "");
    }

    #[test]
    fn a_child_handed_to_another_parent_is_not_described_as_it() {
        let fixture = Fixture::new("engine_reparented");
        alive(&fixture, 1, "systemd", "/init.scope");
        let mut engine = engine(&fixture, true);

        // Its own parent ended first, and it was handed to pid 1.
        let was = engine.what_it_was(&exit("worker", 1, true), None, None);
        assert_eq!(was.cmdline, "");
        assert_eq!(was.exe, "");
        // Where it ran is a guess, and is given as one: its last parent's.
        assert_eq!(was.unit, "init.scope");
    }

    #[test]
    fn a_process_that_called_exec_unheard_is_not_described_as_its_parent() {
        let fixture = Fixture::new("engine_unheard");
        alive(&fixture, 50, "bash", "/system.slice/cron.service");
        let mut engine = engine(&fixture, true);

        // Same name as its parent, but the kernel says it called exec:
        // what it ran is not known, and is not made up.
        let was = engine.what_it_was(&exit("bash", 50, false), None, None);
        assert_eq!(was.cmdline, "");
        assert_eq!(was.unit, "cron.service");
    }

    #[test]
    fn a_parent_nothing_has_seen_yet_is_found_where_it_runs() {
        let fixture = Fixture::new("engine_unseen_parent");
        alive(&fixture, 40, "bash", "/user.slice/session-2.scope");
        let mut engine = engine(&fixture, true);
        // A subshell, started since the sweep, that has not called exec.
        fixture.proc_file(
            "70/stat",
            "70 (bash) S 40 70 40 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 6000 1000 10 0\n",
        );
        // What it ran, heard of by its exec.
        let child = Seen {
            pid: 71,
            start_epoch: BOOT + 61.0,
            pgid: Some(70),
            parent: Some(70),
        };
        engine.place(child, BOOT + 62.0, &HashMap::new());

        let child = engine.identity_of(71, BOOT + 61.0).unwrap();
        let subshell = engine.identity_of(70, BOOT + 60.0).unwrap();
        assert_eq!(child.parent_span_id, Some(subshell.span_id));
        assert_eq!(child.trace_id, subshell.trace_id);
        // The shell it was typed into is found too, and is another job.
        let shell = engine.identity_of(40, BOOT + 1.0).unwrap();
        assert_ne!(shell.trace_id, subshell.trace_id);
        assert_eq!(subshell.parent_span_id, None);
    }

    #[test]
    fn a_parent_known_only_by_its_exit_is_found_in_the_same_batch() {
        let fixture = Fixture::new("engine_batch_parent");
        let mut engine = engine(&fixture, true);
        let parent = exit("make", 1, false);
        let child = ProcessExit {
            pid: 901,
            ppid: 900,
            start_epoch: BOOT + 60.5,
            ..exit("cc", 900, false)
        };
        let ending: HashMap<u32, Seen> = [&child, &parent]
            .into_iter()
            .map(|exit| (exit.pid, seen_exit(exit)))
            .collect();
        // In the order their records come: the child's first.
        engine.place(seen_exit(&child), BOOT + 70.0, &ending);
        engine.place(seen_exit(&parent), BOOT + 70.0, &ending);

        let child = engine.identity_of(901, BOOT + 60.5).unwrap();
        let parent = engine.identity_of(900, BOOT + 60.0).unwrap();
        assert_eq!(child.parent_span_id, Some(parent.span_id));
        assert_eq!(child.trace_id, parent.trace_id);
    }

    #[test]
    fn ticks_land_on_round_times() {
        let ten = Duration::from_secs(10);
        assert_eq!(next_tick(1_753_000_003.2, ten), 1_753_000_010.0);
        assert_eq!(next_tick(1_753_000_009.999, ten), 1_753_000_010.0);
        // Exactly on a tick, the next one is a whole interval away.
        assert_eq!(next_tick(1_753_000_010.0, ten), 1_753_000_020.0);
        assert_eq!(next_tick(100.5, Duration::from_secs(60)), 120.0);
    }
}
