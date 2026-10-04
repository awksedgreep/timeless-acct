//! Per-process sampling, and the totals by command and by user.
//!
//! Three tiers come out of one sweep of `/proc`:
//!
//! - `proc_*`: one set of series per process, for processes that have lived
//!   long enough to be worth a set of series.
//! - `procgroup_*`: totals by command name, over every process.
//! - `procuser_*`: totals by user, over every process.
//!
//! The totals are what keep the accounting complete. A build that runs ten
//! thousand compilers for two seconds each would leave a hundred thousand
//! dead series behind if each got its own; under its command name it is one
//! line that shows exactly how much CPU compiling took.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Instant;

use crate::cgroup::Containers;
use crate::collect::{percent, rate};
use crate::model::{labels, Labels, MetricBatch};
use crate::procfs::process::{
    group_name, is_launcher, parse_pid_io, parse_pid_schedstat, parse_pid_stat, parse_pid_status,
    PidIo, PidStat,
};
use crate::procfs::{Described, ProcRoot};

use super::users::Users;

#[derive(Debug, Clone)]
pub struct ProcessOptions {
    /// Seconds a process must have lived before it gets its own series.
    pub min_age: f64,
    /// Give kernel threads their own series. They are always in the totals.
    pub kernel_threads: bool,
    /// Longest command line kept for an accounting record.
    pub cmdline_max: usize,
    /// Whether the kernel is recording block I/O delay.
    pub delay_accounting: bool,
}

impl Default for ProcessOptions {
    fn default() -> Self {
        Self {
            min_age: 30.0,
            kernel_threads: false,
            cmdline_max: 1024,
            delay_accounting: false,
        }
    }
}

/// The kernel's unit conversions, read once.
#[derive(Debug, Clone, Copy)]
pub struct Units {
    pub ticks_per_second: f64,
    pub page_bytes: u64,
}

impl Units {
    pub fn from_system() -> Self {
        // SAFETY: sysconf reads a constant and has no preconditions.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Self {
            ticks_per_second: if ticks > 0 { ticks as f64 } else { 100.0 },
            page_bytes: if page > 0 { page as u64 } else { 4096 },
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Counters {
    utime: u64,
    stime: u64,
    minflt: u64,
    majflt: u64,
    switches: u64,
    wait_ns: Option<u64>,
    blkio_ticks: u64,
    io: Option<PidIo>,
}

/// What is known about a live process, kept between sweeps.
#[derive(Debug, Clone)]
pub struct Tracked {
    pub pid: u32,
    /// Its parent now: whoever it was handed to, if its own has ended.
    pub ppid: u32,
    /// Its parent when it was first seen.
    pub parent: u32,
    /// Its process group when it was first seen.
    pub pgid: u32,
    pub start_ticks: u64,
    /// Epoch seconds.
    pub start_epoch: f64,
    pub comm: String,
    pub group: String,
    pub uid: u32,
    pub user: String,
    pub kernel_thread: bool,
    pub cmdline: String,
    /// What it was running when first seen, if it has called exec since.
    pub started_as: String,
    pub exe: String,
    /// Its control group's path.
    pub cgroup: String,
    /// The unit that control group belongs to; empty if none.
    pub unit: String,
    pub threads: u64,
    pub rss_bytes: u64,
    /// The largest resident size any sweep saw.
    pub peak_rss_bytes: u64,
    /// Epoch seconds of the last sweep that saw it.
    pub last_seen_epoch: f64,
    labels: Labels,
    last: Counters,
    last_at: Instant,
    generation: u64,
    units: Units,
}

impl Tracked {
    pub fn cpu_user_seconds(&self) -> f64 {
        self.last.utime as f64 / self.units.ticks_per_second
    }

    pub fn cpu_system_seconds(&self) -> f64 {
        self.last.stime as f64 / self.units.ticks_per_second
    }

    pub fn cpu_seconds(&self) -> f64 {
        self.cpu_user_seconds() + self.cpu_system_seconds()
    }

    pub fn minor_faults(&self) -> u64 {
        self.last.minflt
    }

    pub fn major_faults(&self) -> u64 {
        self.last.majflt
    }

    pub fn io(&self) -> Option<PidIo> {
        self.last.io
    }
}

/// CPU that a process used after it was last sampled and before it exited.
/// Without it, the last interval of every process would go unaccounted.
#[derive(Debug, Clone)]
pub struct ExitedCpu {
    pub group: String,
    pub user: String,
    pub cpu_seconds: f64,
}

#[derive(Debug, Default)]
pub struct Sweep {
    /// Processes that were tracked and are gone.
    pub vanished: Vec<Tracked>,
    pub processes: usize,
    pub reported: usize,
    /// Processes seen for the first time.
    pub admitted: Vec<u32>,
    /// What the processes directly in each control group add up to.
    pub cgroups: HashMap<String, GroupUse>,
}

/// The processes of one control group, as the sweep found them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GroupUse {
    pub processes: u64,
    /// Bytes read and written since the last sweep, by the processes whose
    /// I/O could be read; `None` if that was none of them.
    pub io: Option<(u64, u64)>,
}

#[derive(Default)]
struct Total {
    processes: u64,
    threads: u64,
    rss_bytes: u64,
    cpu_seconds: f64,
    read_bytes: f64,
    write_bytes: f64,
    io_seen: bool,
}

pub struct ProcessCollector {
    root: ProcRoot,
    options: ProcessOptions,
    units: Units,
    boot_epoch: f64,
    users: Users,
    containers: Arc<Containers>,
    tracked: HashMap<u32, Tracked>,
    generation: u64,
    previous_sweep: Option<(Instant, f64)>,
    group_labels: HashMap<String, Labels>,
    user_labels: HashMap<String, Labels>,
    buf: String,
}

impl ProcessCollector {
    /// `boot_epoch` is the kernel's boot time in epoch seconds; process
    /// start times are counted from it.
    pub fn new(root: ProcRoot, options: ProcessOptions, units: Units, boot_epoch: f64) -> Self {
        Self {
            root,
            options,
            units,
            boot_epoch,
            users: Users::default(),
            containers: Arc::default(),
            tracked: HashMap::new(),
            generation: 0,
            previous_sweep: None,
            group_labels: HashMap::new(),
            user_labels: HashMap::new(),
            buf: String::with_capacity(4096),
        }
    }

    #[cfg(test)]
    pub fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    /// The tracked process with this pid, if it started within
    /// `tolerance` seconds of `start_epoch`. The start time is what tells a
    /// process from a later one that was given the same pid.
    pub fn find(&self, pid: u32, start_epoch: f64, tolerance: f64) -> Option<&Tracked> {
        self.tracked
            .get(&pid)
            .filter(|t| (t.start_epoch - start_epoch).abs() <= tolerance)
    }

    /// Stop tracking a process whose exit has been accounted for, so the
    /// next sweep does not report it as vanished.
    pub fn forget(&mut self, pid: u32) -> Option<Tracked> {
        self.tracked.remove(&pid)
    }

    /// The containers there are now, for naming what is named for one.
    pub fn set_containers(&mut self, containers: Arc<Containers>) {
        self.containers = containers;
    }

    /// Every process that was there at the last sweep.
    #[cfg(feature = "watch")]
    pub fn all(&self) -> impl Iterator<Item = &Tracked> {
        self.tracked.values()
    }

    pub fn get(&self, pid: u32) -> Option<&Tracked> {
        self.tracked.get(&pid)
    }

    /// The unit of a live process.
    #[cfg(test)]
    pub fn unit_of(&self, pid: u32) -> Option<&str> {
        self.tracked
            .get(&pid)
            .map(|t| t.unit.as_str())
            .filter(|unit| !unit.is_empty())
    }

    /// A tracked process called exec: it is the same process, and what it
    /// is running has changed. Returns false if the process is not
    /// tracked, which leaves the caller to remember the exec itself.
    pub fn note_exec(&mut self, pid: u32, start_ticks: u64, described: &Described) -> bool {
        let Some(tracked) = self
            .tracked
            .get_mut(&pid)
            .filter(|t| t.start_ticks == start_ticks)
        else {
            return false;
        };
        if tracked.started_as.is_empty()
            && tracked.cmdline != described.cmdline
            && !is_launcher(&tracked.cmdline)
        {
            tracked.started_as = std::mem::take(&mut tracked.cmdline);
        }
        tracked.cmdline.clone_from(&described.cmdline);
        tracked.exe.clone_from(&described.exe);
        if tracked.cgroup != described.cgroup {
            tracked.cgroup.clone_from(&described.cgroup);
            tracked.unit = unit_of(&described.cgroup, &mut self.users, &self.containers);
        }
        true
    }

    /// Read every process and append this instant's samples.
    ///
    /// `wall` is the sweep's time in epoch seconds. `exited` carries the CPU
    /// of processes that ended since the last sweep, for the totals.
    pub fn sweep(
        &mut self,
        now: Instant,
        wall: f64,
        exited: &[ExitedCpu],
        batch: &mut MetricBatch,
    ) -> Sweep {
        self.generation += 1;
        let interval = self
            .previous_sweep
            .and_then(|(at, _)| now.checked_duration_since(at))
            .map(|d| d.as_secs_f64())
            .filter(|seconds| *seconds > 0.0);
        let previous_wall = self.previous_sweep.map(|(_, wall)| wall);

        let mut sweep = Sweep::default();
        let mut groups: HashMap<String, Total> = HashMap::new();
        let mut users: HashMap<String, Total> = HashMap::new();

        let mut pids = self.root.pids().unwrap_or_default();
        pids.sort_unstable();
        for pid in pids {
            // A process can exit between the directory listing and the
            // read; that is an ordinary race, not an error.
            if self.root.read_pid(pid, "stat", &mut self.buf).is_err() {
                continue;
            }
            let Some(stat) = parse_pid_stat(&self.buf) else {
                continue;
            };
            sweep.processes += 1;

            let known = self
                .tracked
                .get(&pid)
                .is_some_and(|t| t.start_ticks == stat.start_ticks);
            if !known {
                if let Some(replaced) = self.tracked.remove(&pid) {
                    sweep.vanished.push(replaced);
                }
                let tracked = self.admit(pid, &stat, now, wall);
                // Everything a process has used since it started belongs to
                // this interval if it started within it.
                let born_this_interval =
                    previous_wall.is_some_and(|previous| tracked.start_epoch >= previous);
                let cpu = if born_this_interval {
                    tracked.cpu_seconds()
                } else {
                    0.0
                };
                add_to_totals(&mut groups, &mut users, &tracked, cpu, None);
                sweep
                    .cgroups
                    .entry(tracked.cgroup.clone())
                    .or_default()
                    .processes += 1;
                sweep.admitted.push(pid);
                self.tracked.insert(pid, tracked);
                continue;
            }

            let current = self.read_counters(pid, &stat);
            // read_pid clears the buffer first, so a failed read leaves it
            // empty: only parse on success, otherwise keep what tracking
            // already knows instead of attributing defaults (uid 0) to it.
            let status = self
                .root
                .read_pid(pid, "status", &mut self.buf)
                .ok()
                .map(|()| parse_pid_status(&self.buf));
            let Some(previous) = self.tracked.get(&pid) else {
                continue;
            };
            // Counting file descriptors walks every fd of every process:
            // only for the few old enough to be reported.
            let old_enough = wall - previous.start_epoch >= self.options.min_age
                && !(previous.kernel_thread && !self.options.kernel_threads);
            let fds = old_enough
                .then(|| {
                    fs::read_dir(self.root.pid_path(pid, "fd"))
                        .ok()
                        .map(|entries| entries.count())
                })
                .flatten();
            let user = match &status {
                None => None,
                Some(status) if previous.uid == status.uid => None,
                Some(status) => Some(self.users.name(status.uid).to_string()),
            };
            // A process is moved between control groups rarely, and by
            // someone else: a login is moved into its session's scope.
            let cgroup = self.root.cgroup_of(pid, &mut self.buf);
            let moved = (!cgroup.is_empty() && previous.cgroup != cgroup)
                .then(|| (unit_of(&cgroup, &mut self.users, &self.containers), cgroup));

            let tracked = self.tracked.get_mut(&pid).expect("checked above");
            let switches = match status.as_ref() {
                Some(status) => status.voluntary_switches + status.involuntary_switches,
                // Unreadable: carry the last count forward so the rate is
                // zero rather than a spike or a reset.
                None => tracked.last.switches,
            };
            let counters = Counters {
                switches,
                ..current
            };
            let renamed = tracked.comm != stat.comm;
            if renamed {
                // The same process under a new name: it called exec, or
                // renamed itself. Its series continue under the new name.
                tracked.comm = stat.comm.clone();
                tracked.group = group_name(&stat.comm, tracked.kernel_thread).to_string();
            }
            if let Some(user) = user {
                if let Some(status) = status.as_ref() {
                    tracked.uid = status.uid;
                }
                tracked.user = user;
            }
            if let Some((unit, cgroup)) = moved {
                tracked.unit = unit;
                tracked.cgroup = cgroup;
            }
            sweep
                .cgroups
                .entry(tracked.cgroup.clone())
                .or_default()
                .processes += 1;
            // Its series are named for what it is now: renamed, handed to
            // another user, or moved to another unit, by a sweep or by an
            // exec heard of since.
            if renamed
                || label(&tracked.labels, "user") != tracked.user
                || label(&tracked.labels, "unit") != tracked.unit
            {
                tracked.labels = process_labels(pid, &tracked.comm, &tracked.user, &tracked.unit);
            }

            let rss_bytes = match status.as_ref().and_then(|s| s.rss_kb) {
                Some(kb) => kb * 1024,
                None => stat.rss_pages * self.units.page_bytes,
            };
            tracked.ppid = stat.ppid;
            tracked.threads = stat.threads;
            tracked.rss_bytes = rss_bytes;
            tracked.peak_rss_bytes = tracked.peak_rss_bytes.max(rss_bytes);
            tracked.last_seen_epoch = wall;
            tracked.generation = self.generation;

            let seconds = now
                .checked_duration_since(tracked.last_at)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let before = tracked.last;
            tracked.last = counters;
            tracked.last_at = now;
            if seconds <= 0.0 {
                continue;
            }

            let ticks = self.units.ticks_per_second;
            let user_seconds = counters.utime.saturating_sub(before.utime) as f64 / ticks;
            let system_seconds = counters.stime.saturating_sub(before.stime) as f64 / ticks;
            let io_delta = match (counters.io, before.io) {
                (Some(now), Some(before)) => Some((
                    now.read_bytes.saturating_sub(before.read_bytes) as f64,
                    now.write_bytes.saturating_sub(before.write_bytes) as f64,
                )),
                _ => None,
            };
            add_to_totals(
                &mut groups,
                &mut users,
                tracked,
                user_seconds + system_seconds,
                io_delta,
            );
            if let Some((read, write)) = io_delta {
                let group = sweep.cgroups.entry(tracked.cgroup.clone()).or_default();
                let (read_so_far, write_so_far) = group.io.unwrap_or_default();
                group.io = Some((read_so_far + read as u64, write_so_far + write as u64));
            }

            let old_enough = wall - tracked.start_epoch >= self.options.min_age;
            if !old_enough || (tracked.kernel_thread && !self.options.kernel_threads) {
                continue;
            }
            sweep.reported += 1;
            let l = &tracked.labels;
            batch.push(
                "proc_cpu_pct",
                l,
                percent(user_seconds + system_seconds, seconds),
            );
            batch.push("proc_cpu_user_pct", l, percent(user_seconds, seconds));
            batch.push("proc_cpu_system_pct", l, percent(system_seconds, seconds));
            batch.push(
                "proc_cpu_seconds",
                l,
                (counters.utime + counters.stime) as f64 / ticks,
            );
            batch.push("proc_threads", l, stat.threads as f64);
            batch.push(
                "proc_minor_faults_per_sec",
                l,
                rate(counters.minflt, before.minflt, seconds),
            );
            batch.push(
                "proc_major_faults_per_sec",
                l,
                rate(counters.majflt, before.majflt, seconds),
            );
            batch.push(
                "proc_context_switches_per_sec",
                l,
                rate(counters.switches, before.switches, seconds),
            );
            if !tracked.kernel_thread {
                batch.push("proc_rss_bytes", l, rss_bytes as f64);
                batch.push("proc_vsize_bytes", l, stat.vsize_bytes as f64);
                if let Some(kb) = status.as_ref().and_then(|s| s.swap_kb) {
                    batch.push("proc_swap_bytes", l, kb as f64 * 1024.0);
                }
            }
            if let Some(fds) = fds {
                batch.push("proc_fds", l, fds as f64);
            }
            if let Some((read, write)) = io_delta {
                batch.push("proc_io_read_bytes_per_sec", l, read / seconds);
                batch.push("proc_io_write_bytes_per_sec", l, write / seconds);
            }
            if let (Some(now), Some(before)) = (counters.wait_ns, before.wait_ns) {
                // Nanoseconds spent runnable without a CPU, per second.
                batch.push(
                    "proc_cpu_wait_pct",
                    l,
                    rate(now, before, seconds) / 10_000_000.0,
                );
            }
            if self.options.delay_accounting {
                batch.push(
                    "proc_io_wait_pct",
                    l,
                    percent(
                        counters.blkio_ticks.saturating_sub(before.blkio_ticks) as f64 / ticks,
                        seconds,
                    ),
                );
            }
        }

        let gone: Vec<u32> = self
            .tracked
            .iter()
            .filter(|(_, t)| t.generation != self.generation)
            .map(|(pid, _)| *pid)
            .collect();
        for pid in gone {
            if let Some(tracked) = self.tracked.remove(&pid) {
                sweep.vanished.push(tracked);
            }
        }

        for exit in exited {
            groups.entry(exit.group.clone()).or_default().cpu_seconds += exit.cpu_seconds;
            users.entry(exit.user.clone()).or_default().cpu_seconds += exit.cpu_seconds;
        }
        self.push_totals(batch, groups, users, interval);

        self.previous_sweep = Some((now, wall));
        sweep
    }

    fn admit(&mut self, pid: u32, stat: &PidStat, now: Instant, wall: f64) -> Tracked {
        // On failure leave the status empty rather than parsing the cleared
        // buffer into uid-0 defaults.
        let status = self
            .root
            .read_pid(pid, "status", &mut self.buf)
            .ok()
            .map(|()| parse_pid_status(&self.buf))
            .unwrap_or_default();
        let Described {
            cmdline,
            exe,
            cgroup,
        } = self
            .root
            .describe(pid, self.options.cmdline_max, &mut self.buf);
        let unit = unit_of(&cgroup, &mut self.users, &self.containers);
        let kernel_thread = stat.is_kernel_thread();
        let user = self.users.name(status.uid).to_string();
        let counters = Counters {
            switches: status.voluntary_switches + status.involuntary_switches,
            ..self.read_counters(pid, stat)
        };
        let rss_bytes = match status.rss_kb {
            Some(kb) => kb * 1024,
            None => stat.rss_pages * self.units.page_bytes,
        };

        Tracked {
            pid,
            ppid: stat.ppid,
            parent: stat.ppid,
            pgid: stat.pgrp,
            start_ticks: stat.start_ticks,
            start_epoch: self.boot_epoch + stat.start_ticks as f64 / self.units.ticks_per_second,
            comm: stat.comm.clone(),
            group: group_name(&stat.comm, kernel_thread).to_string(),
            uid: status.uid,
            labels: process_labels(pid, &stat.comm, &user, &unit),
            user,
            kernel_thread,
            cmdline,
            started_as: String::new(),
            exe,
            cgroup,
            unit,
            threads: stat.threads,
            rss_bytes,
            peak_rss_bytes: rss_bytes,
            last_seen_epoch: wall,
            last: counters,
            last_at: now,
            generation: self.generation,
            units: self.units,
        }
    }

    /// The counters that live outside `status`. I/O and scheduler figures
    /// are absent, rather than zero, when the kernel will not show them to
    /// this user.
    fn read_counters(&mut self, pid: u32, stat: &PidStat) -> Counters {
        let io = self
            .root
            .read_pid(pid, "io", &mut self.buf)
            .ok()
            .map(|()| parse_pid_io(&self.buf));
        let wait_ns = self
            .root
            .read_pid(pid, "schedstat", &mut self.buf)
            .ok()
            .and_then(|()| parse_pid_schedstat(&self.buf))
            .map(|sched| sched.wait_ns);
        Counters {
            utime: stat.utime,
            stime: stat.stime,
            minflt: stat.minflt,
            majflt: stat.majflt,
            switches: 0,
            wait_ns,
            blkio_ticks: stat.blkio_ticks,
            io,
        }
    }

    fn push_totals(
        &mut self,
        batch: &mut MetricBatch,
        groups: HashMap<String, Total>,
        users: HashMap<String, Total>,
        interval: Option<f64>,
    ) {
        use crate::queue::settle;
        // Bound the label caches: churn in command names would otherwise
        // keep an entry forever. Prune to the present when large.
        if self.group_labels.len() > 4096 {
            let present: std::collections::HashSet<&str> =
                groups.keys().map(String::as_str).collect();
            self.group_labels
                .retain(|name, _| present.contains(name.as_str()));
            settle(&mut self.group_labels);
        }
        if self.user_labels.len() > 1024 {
            let present: std::collections::HashSet<&str> =
                users.keys().map(String::as_str).collect();
            self.user_labels
                .retain(|name, _| present.contains(name.as_str()));
            settle(&mut self.user_labels);
        }
        let mut groups: Vec<(String, Total)> = groups.into_iter().collect();
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, total) in groups {
            let label = self
                .group_labels
                .entry(name.clone())
                .or_insert_with(|| labels(vec![("comm", name)]))
                .clone();
            batch.push("procgroup_processes", &label, total.processes as f64);
            batch.push("procgroup_threads", &label, total.threads as f64);
            batch.push("procgroup_rss_bytes", &label, total.rss_bytes as f64);
            if let Some(seconds) = interval {
                batch.push(
                    "procgroup_cpu_pct",
                    &label,
                    percent(total.cpu_seconds, seconds),
                );
                if total.io_seen {
                    batch.push(
                        "procgroup_io_read_bytes_per_sec",
                        &label,
                        total.read_bytes / seconds,
                    );
                    batch.push(
                        "procgroup_io_write_bytes_per_sec",
                        &label,
                        total.write_bytes / seconds,
                    );
                }
            }
        }

        let mut users: Vec<(String, Total)> = users.into_iter().collect();
        users.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, total) in users {
            let label = self
                .user_labels
                .entry(name.clone())
                .or_insert_with(|| labels(vec![("user", name)]))
                .clone();
            batch.push("procuser_processes", &label, total.processes as f64);
            batch.push("procuser_rss_bytes", &label, total.rss_bytes as f64);
            if let Some(seconds) = interval {
                batch.push(
                    "procuser_cpu_pct",
                    &label,
                    percent(total.cpu_seconds, seconds),
                );
            }
        }
    }
}

/// The unit a control group belongs to, named with `users`; empty if none.
fn unit_of(cgroup: &str, users: &mut Users, containers: &Containers) -> String {
    crate::cgroup::unit_of(cgroup, &mut |uid| users.name(uid).to_string(), containers)
        .unwrap_or_default()
}

fn add_to_totals(
    groups: &mut HashMap<String, Total>,
    users: &mut HashMap<String, Total>,
    tracked: &Tracked,
    cpu_seconds: f64,
    io: Option<(f64, f64)>,
) {
    for total in [
        groups.entry(tracked.group.clone()).or_default(),
        users.entry(tracked.user.clone()).or_default(),
    ] {
        total.processes += 1;
        total.threads += tracked.threads;
        total.rss_bytes += tracked.rss_bytes;
        total.cpu_seconds += cpu_seconds;
        if let Some((read, write)) = io {
            total.read_bytes += read;
            total.write_bytes += write;
            total.io_seen = true;
        }
    }
}

/// `proc` names one process in one label, which is what a canvas element
/// selects by. The others are there to select many at once: every
/// process of a command, of a user, or of a unit.
fn process_labels(pid: u32, comm: &str, user: &str, unit: &str) -> Labels {
    let mut all = vec![
        ("pid", pid.to_string()),
        ("comm", comm.to_string()),
        ("user", user.to_string()),
        ("proc", format!("{comm}[{pid}]")),
    ];
    // A process in no unit has no unit to be selected by, which is not
    // the same as being in a unit with no name.
    if !unit.is_empty() {
        all.push(("unit", unit.to_string()));
    }
    labels(all)
}

fn label<'a>(labels: &'a Labels, wanted: &str) -> &'a str {
    labels
        .iter()
        .find(|(key, _)| *key == wanted)
        .map_or("", |(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::time::Duration;

    const UNITS: Units = Units {
        ticks_per_second: 100.0,
        page_bytes: 4096,
    };
    const BOOT: f64 = 1_000_000.0;

    struct Proc<'a> {
        pid: u32,
        comm: &'a str,
        start_ticks: u64,
        utime: u64,
        stime: u64,
        rss_kb: u64,
        read_bytes: u64,
        wait_ns: u64,
        flags: u64,
        cgroup: &'a str,
    }

    impl Default for Proc<'_> {
        fn default() -> Self {
            Self {
                pid: 100,
                comm: "worker",
                start_ticks: 0,
                utime: 0,
                stime: 0,
                rss_kb: 1024,
                read_bytes: 0,
                wait_ns: 0,
                flags: 0,
                cgroup: "/system.slice/worker.service",
            }
        }
    }

    fn write(fixture: &Fixture, p: &Proc) {
        let dir = p.pid.to_string();
        fixture.proc_file(
            &format!("{dir}/stat"),
            &format!(
                "{} ({}) S 1 1 1 0 -1 {} 10 0 2 0 {} {} 0 0 20 0 3 0 {} 8192000 256 \
                 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0\n",
                p.pid, p.comm, p.flags, p.utime, p.stime, p.start_ticks
            ),
        );
        fixture.proc_file(
            &format!("{dir}/status"),
            &format!(
                "Uid:\t0\t0\t0\t0\nVmRSS:\t{} kB\nVmSwap:\t0 kB\n\
                 voluntary_ctxt_switches:\t{}\nnonvoluntary_ctxt_switches:\t0\n",
                p.rss_kb,
                p.utime * 2
            ),
        );
        fixture.proc_file(
            &format!("{dir}/io"),
            &format!(
                "rchar: 0\nwchar: 0\nread_bytes: {}\nwrite_bytes: 0\n",
                p.read_bytes
            ),
        );
        fixture.proc_file(&format!("{dir}/schedstat"), &format!("0 {} 0\n", p.wait_ns));
        fixture.proc_file(
            &format!("{dir}/cmdline"),
            &format!("/usr/bin/{}\0--flag\0", p.comm),
        );
        fixture.proc_file(&format!("{dir}/cgroup"), &format!("0::{}\n", p.cgroup));
    }

    fn value(batch: &MetricBatch, name: &str, key: &str, want: &str) -> Option<f64> {
        batch
            .samples
            .iter()
            .find(|s| s.name == name && s.labels.iter().any(|(k, v)| *k == key && v == want))
            .map(|s| s.value)
    }

    fn collector(fixture: &Fixture, options: ProcessOptions) -> ProcessCollector {
        ProcessCollector::new(fixture.root(), options, UNITS, BOOT)
    }

    #[test]
    fn a_long_lived_process_gets_rates_from_its_second_sweep() {
        let fixture = Fixture::new("process_rates");
        let mut collector = collector(&fixture, ProcessOptions::default());
        let start = Instant::now();
        // Started at boot; the sweeps happen 1000 seconds later.
        let wall = BOOT + 1000.0;

        write(
            &fixture,
            &Proc {
                utime: 100,
                stime: 50,
                ..Proc::default()
            },
        );
        let mut first = MetricBatch::new(1);
        let sweep = collector.sweep(start, wall, &[], &mut first);
        assert_eq!(sweep.processes, 1);
        assert_eq!(sweep.reported, 0);
        assert_eq!(value(&first, "proc_cpu_pct", "pid", "100"), None);
        assert_eq!(
            value(&first, "procgroup_processes", "comm", "worker"),
            Some(1.0)
        );
        assert_eq!(value(&first, "procgroup_cpu_pct", "comm", "worker"), None);

        write(
            &fixture,
            &Proc {
                utime: 400,
                stime: 150,
                rss_kb: 2048,
                read_bytes: 1_000_000,
                wait_ns: 500_000_000,
                ..Proc::default()
            },
        );
        let mut second = MetricBatch::new(2);
        let sweep = collector.sweep(
            start + Duration::from_secs(10),
            wall + 10.0,
            &[],
            &mut second,
        );
        assert_eq!(sweep.reported, 1);

        // 3 s user and 1 s system over 10 s.
        assert_eq!(
            value(&second, "proc_cpu_pct", "proc", "worker[100]"),
            Some(40.0)
        );
        assert_eq!(
            value(&second, "proc_cpu_user_pct", "pid", "100"),
            Some(30.0)
        );
        assert_eq!(
            value(&second, "proc_cpu_system_pct", "pid", "100"),
            Some(10.0)
        );
        assert_eq!(value(&second, "proc_cpu_seconds", "pid", "100"), Some(5.5));
        assert_eq!(
            value(&second, "proc_rss_bytes", "pid", "100"),
            Some(2_097_152.0)
        );
        assert_eq!(value(&second, "proc_threads", "pid", "100"), Some(3.0));
        assert_eq!(
            value(&second, "proc_io_read_bytes_per_sec", "pid", "100"),
            Some(100_000.0)
        );
        assert_eq!(value(&second, "proc_cpu_wait_pct", "pid", "100"), Some(5.0));
        assert_eq!(
            value(&second, "proc_context_switches_per_sec", "pid", "100"),
            Some(60.0)
        );
        assert_eq!(value(&second, "proc_cpu_pct", "user", "root"), Some(40.0));
        // Every process of a unit is selected by the unit.
        assert_eq!(
            value(&second, "proc_cpu_pct", "unit", "worker.service"),
            Some(40.0)
        );

        assert_eq!(
            value(&second, "procgroup_cpu_pct", "comm", "worker"),
            Some(40.0)
        );
        assert_eq!(
            value(&second, "procgroup_rss_bytes", "comm", "worker"),
            Some(2_097_152.0)
        );
        assert_eq!(
            value(&second, "procuser_cpu_pct", "user", "root"),
            Some(40.0)
        );
        assert_eq!(
            value(&second, "procuser_processes", "user", "root"),
            Some(1.0)
        );
    }

    #[test]
    fn a_young_process_is_in_the_totals_but_has_no_series_of_its_own() {
        let fixture = Fixture::new("process_young");
        let mut collector = collector(&fixture, ProcessOptions::default());
        let start = Instant::now();
        let wall = BOOT + 1000.0;

        write(&fixture, &Proc::default());
        collector.sweep(start, wall, &[], &mut MetricBatch::new(1));

        // Starts 4 seconds into the interval, and by the next sweep has
        // used 2 seconds of CPU.
        write(
            &fixture,
            &Proc {
                pid: 200,
                comm: "rustc",
                start_ticks: 100_400,
                utime: 200,
                ..Proc::default()
            },
        );
        let mut second = MetricBatch::new(2);
        let sweep = collector.sweep(
            start + Duration::from_secs(10),
            wall + 10.0,
            &[],
            &mut second,
        );
        assert_eq!(sweep.processes, 2);
        assert_eq!(value(&second, "proc_cpu_pct", "pid", "200"), None);
        assert_eq!(
            value(&second, "procgroup_processes", "comm", "rustc"),
            Some(1.0)
        );
        assert_eq!(
            value(&second, "procgroup_cpu_pct", "comm", "rustc"),
            Some(20.0)
        );

        // Ten seconds later it is still too young for its own series.
        write(
            &fixture,
            &Proc {
                pid: 200,
                comm: "rustc",
                start_ticks: 100_400,
                utime: 700,
                ..Proc::default()
            },
        );
        let mut third = MetricBatch::new(3);
        collector.sweep(
            start + Duration::from_secs(20),
            wall + 20.0,
            &[],
            &mut third,
        );
        assert_eq!(value(&third, "proc_cpu_pct", "pid", "200"), None);
        assert_eq!(
            value(&third, "procgroup_cpu_pct", "comm", "rustc"),
            Some(50.0)
        );

        // Past thirty seconds of age, it is reported.
        write(
            &fixture,
            &Proc {
                pid: 200,
                comm: "rustc",
                start_ticks: 100_400,
                utime: 800,
                ..Proc::default()
            },
        );
        let mut later = MetricBatch::new(4);
        collector.sweep(
            start + Duration::from_secs(40),
            wall + 40.0,
            &[],
            &mut later,
        );
        assert_eq!(value(&later, "proc_cpu_pct", "pid", "200"), Some(5.0));
    }

    #[test]
    fn cpu_of_exited_processes_is_counted_in_the_totals() {
        let fixture = Fixture::new("process_exited");
        let mut collector = collector(&fixture, ProcessOptions::default());
        let start = Instant::now();
        let wall = BOOT + 1000.0;

        write(&fixture, &Proc::default());
        collector.sweep(start, wall, &[], &mut MetricBatch::new(1));
        let exited = [
            ExitedCpu {
                group: "cc1".into(),
                user: "root".into(),
                cpu_seconds: 3.0,
            },
            ExitedCpu {
                group: "cc1".into(),
                user: "root".into(),
                cpu_seconds: 2.0,
            },
        ];
        let mut batch = MetricBatch::new(2);
        collector.sweep(
            start + Duration::from_secs(10),
            wall + 10.0,
            &exited,
            &mut batch,
        );

        // No cc1 process was ever seen alive, and its CPU is all there.
        assert_eq!(
            value(&batch, "procgroup_cpu_pct", "comm", "cc1"),
            Some(50.0)
        );
        assert_eq!(
            value(&batch, "procgroup_processes", "comm", "cc1"),
            Some(0.0)
        );
        assert_eq!(
            value(&batch, "procuser_cpu_pct", "user", "root"),
            Some(50.0)
        );
    }

    #[test]
    fn a_process_that_is_gone_is_reported_as_vanished_with_its_last_figures() {
        let fixture = Fixture::new("process_vanished");
        let mut collector = collector(&fixture, ProcessOptions::default());
        let start = Instant::now();
        let wall = BOOT + 1000.0;

        write(
            &fixture,
            &Proc {
                utime: 250,
                stime: 50,
                rss_kb: 4096,
                ..Proc::default()
            },
        );
        collector.sweep(start, wall, &[], &mut MetricBatch::new(1));
        assert_eq!(collector.tracked_count(), 1);
        assert!(collector.find(100, BOOT, 5.0).is_some());
        assert!(collector.find(100, BOOT + 60.0, 5.0).is_none());

        fixture.remove_proc("100");
        let sweep = collector.sweep(
            start + Duration::from_secs(10),
            wall + 10.0,
            &[],
            &mut MetricBatch::new(2),
        );
        assert_eq!(collector.tracked_count(), 0);
        assert_eq!(sweep.vanished.len(), 1);
        let gone = &sweep.vanished[0];
        assert_eq!(gone.comm, "worker");
        assert_eq!(gone.cpu_seconds(), 3.0);
        assert_eq!(gone.peak_rss_bytes, 4096 * 1024);
        assert_eq!(gone.cmdline, "/usr/bin/worker --flag");
        assert_eq!(gone.last_seen_epoch, wall);
    }

    #[test]
    fn a_reused_pid_is_a_different_process() {
        let fixture = Fixture::new("process_reuse");
        let mut collector = collector(&fixture, ProcessOptions::default());
        let start = Instant::now();
        let wall = BOOT + 1000.0;

        write(
            &fixture,
            &Proc {
                utime: 900,
                ..Proc::default()
            },
        );
        collector.sweep(start, wall, &[], &mut MetricBatch::new(1));

        // Same pid, later start, smaller counters.
        write(
            &fixture,
            &Proc {
                comm: "other",
                start_ticks: 100_500,
                utime: 10,
                ..Proc::default()
            },
        );
        let mut batch = MetricBatch::new(2);
        let sweep = collector.sweep(
            start + Duration::from_secs(10),
            wall + 10.0,
            &[],
            &mut batch,
        );
        assert_eq!(sweep.vanished.len(), 1);
        assert_eq!(sweep.vanished[0].comm, "worker");
        // The new process's counters are not differenced against the old.
        assert_eq!(value(&batch, "proc_cpu_pct", "pid", "100"), None);
        assert_eq!(
            value(&batch, "procgroup_processes", "comm", "other"),
            Some(1.0)
        );
        assert_eq!(value(&batch, "procgroup_processes", "comm", "worker"), None);
    }

    #[test]
    fn kernel_threads_are_grouped_by_kind_and_kept_out_of_the_process_tier() {
        let fixture = Fixture::new("process_kthread");
        let mut collector = collector(&fixture, ProcessOptions::default());
        let start = Instant::now();
        let wall = BOOT + 1000.0;

        for (pid, comm) in [(30, "kworker/0:1-events"), (31, "kworker/1:0")] {
            write(
                &fixture,
                &Proc {
                    pid,
                    comm,
                    flags: 0x0020_0000,
                    ..Proc::default()
                },
            );
        }
        collector.sweep(start, wall, &[], &mut MetricBatch::new(1));
        for (pid, comm) in [(30, "kworker/0:1-events"), (31, "kworker/1:0")] {
            write(
                &fixture,
                &Proc {
                    pid,
                    comm,
                    flags: 0x0020_0000,
                    stime: 50,
                    ..Proc::default()
                },
            );
        }
        let mut batch = MetricBatch::new(2);
        let sweep = collector.sweep(
            start + Duration::from_secs(10),
            wall + 10.0,
            &[],
            &mut batch,
        );
        assert_eq!(sweep.reported, 0);
        assert_eq!(value(&batch, "proc_cpu_pct", "pid", "30"), None);
        assert_eq!(
            value(&batch, "procgroup_processes", "comm", "kworker"),
            Some(2.0)
        );
        assert_eq!(
            value(&batch, "procgroup_cpu_pct", "comm", "kworker"),
            Some(10.0)
        );
    }

    #[test]
    fn processes_are_counted_by_control_group_and_know_their_unit() {
        let fixture = Fixture::new("process_cgroup");
        let mut collector = collector(&fixture, ProcessOptions::default());
        let start = Instant::now();
        let wall = BOOT + 1000.0;

        write(&fixture, &Proc::default());
        write(
            &fixture,
            &Proc {
                pid: 101,
                ..Proc::default()
            },
        );
        write(
            &fixture,
            &Proc {
                pid: 102,
                comm: "login",
                cgroup: "/system.slice/sshd.service",
                ..Proc::default()
            },
        );
        let sweep = collector.sweep(start, wall, &[], &mut MetricBatch::new(1));
        assert_eq!(sweep.cgroups["/system.slice/worker.service"].processes, 2);
        assert_eq!(sweep.cgroups["/system.slice/sshd.service"].processes, 1);
        assert_eq!(sweep.cgroups["/system.slice/worker.service"].io, None);
        assert_eq!(collector.unit_of(102), Some("sshd.service"));

        // The login is moved into its session.
        write(
            &fixture,
            &Proc {
                pid: 102,
                comm: "login",
                cgroup: "/user.slice/user-0.slice/session-7.scope",
                ..Proc::default()
            },
        );
        let sweep = collector.sweep(
            start + Duration::from_secs(10),
            wall + 10.0,
            &[],
            &mut MetricBatch::new(2),
        );
        assert_eq!(sweep.cgroups.get("/system.slice/sshd.service"), None);
        assert_eq!(
            sweep.cgroups["/user.slice/user-0.slice/session-7.scope"].processes,
            1
        );
        // Two processes, each of which read nothing: known, and zero.
        assert_eq!(
            sweep.cgroups["/system.slice/worker.service"].io,
            Some((0, 0))
        );
        assert_eq!(collector.unit_of(102), Some("session.scope"));
        assert_eq!(collector.unit_of(9999), None);
    }

    #[test]
    fn an_exec_changes_what_a_tracked_process_is_running() {
        let fixture = Fixture::new("process_exec");
        let mut collector = collector(&fixture, ProcessOptions::default());
        write(&fixture, &Proc::default());
        collector.sweep(Instant::now(), BOOT + 1000.0, &[], &mut MetricBatch::new(1));

        let described = Described {
            cmdline: "postgres -D /var/lib/postgres".into(),
            exe: "/usr/bin/postgres".into(),
            cgroup: "/system.slice/postgresql.service".into(),
        };
        // A different start time is a different process with the same pid.
        assert!(!collector.note_exec(100, 77, &described));
        assert!(!collector.note_exec(555, 0, &described));
        assert!(collector.note_exec(100, 0, &described));

        // A second exec does not change what it started as.
        let again = Described {
            cmdline: "postgres: checkpointer".into(),
            ..described.clone()
        };
        assert!(collector.note_exec(100, 0, &again));

        let tracked = collector.forget(100).unwrap();
        assert_eq!(tracked.started_as, "/usr/bin/worker --flag");
        assert_eq!(tracked.cmdline, "postgres: checkpointer");
        assert_eq!(tracked.exe, "/usr/bin/postgres");
        assert_eq!(tracked.unit, "postgresql.service");
    }

    #[test]
    fn a_renamed_process_continues_under_its_new_name() {
        let fixture = Fixture::new("process_rename");
        let mut collector = collector(&fixture, ProcessOptions::default());
        let start = Instant::now();
        let wall = BOOT + 1000.0;

        write(
            &fixture,
            &Proc {
                comm: "sh",
                ..Proc::default()
            },
        );
        collector.sweep(start, wall, &[], &mut MetricBatch::new(1));
        write(
            &fixture,
            &Proc {
                comm: "postgres",
                utime: 100,
                ..Proc::default()
            },
        );
        let mut batch = MetricBatch::new(2);
        let sweep = collector.sweep(
            start + Duration::from_secs(10),
            wall + 10.0,
            &[],
            &mut batch,
        );
        assert!(sweep.vanished.is_empty());
        assert_eq!(
            value(&batch, "proc_cpu_pct", "proc", "postgres[100]"),
            Some(10.0)
        );
        assert_eq!(value(&batch, "proc_cpu_pct", "proc", "sh[100]"), None);
        assert_eq!(
            value(&batch, "procgroup_cpu_pct", "comm", "postgres"),
            Some(10.0)
        );
    }
}
