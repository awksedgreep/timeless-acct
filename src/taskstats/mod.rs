//! Exit accounting from the kernel.
//!
//! Sampling `/proc` cannot see a process that starts and ends between two
//! sweeps, and on a busy host those are most processes. The taskstats
//! interface reports every task as it exits, with its full accounting: the
//! successor to BSD process accounting, without the accounting file.
//!
//! The kernel reports per task, which is per thread. This module adds the
//! threads of a process together and yields one record when the last of
//! them is gone.

pub mod netlink;
pub mod record;

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::netlink::Received;
use crate::queue::{queue, settle, Receiver};
use netlink::{exit_records, Socket};
use record::{TaskExit, AFORK};

/// How long a read waits before the thread checks whether to stop.
const POLL: Duration = Duration::from_millis(250);
/// Exit records waiting for the collector, at the most. What arrives at
/// a full queue is dropped and counted, rather than the queue growing
/// without bound during a fork storm.
///
/// A record is about 300 bytes, so a full queue is 80 MiB, and an empty
/// one is nothing. At the default sweep of ten seconds it absorbs 26,000
/// tasks ending a second. A queue of 16,384 lost 7,850 records in two
/// minutes of a browser being compiled on 22 CPUs.
const QUEUE: usize = 262_144;

pub fn epoch_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// A task's exit and when it was received, in epoch seconds.
pub struct Received1 {
    pub at: f64,
    pub exit: TaskExit,
}

pub struct Listener {
    receiver: Receiver<Received1>,
    lost: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// Register with the kernel and start reading. `cpus` is the kernel's
    /// list of possible CPUs.
    ///
    /// Registration is done here, on the caller's thread, so that a refusal
    /// (no `CAP_NET_ADMIN`) is an error the caller can act on.
    pub fn start(cpus: &str) -> io::Result<Self> {
        let socket = Socket::listen(cpus, POLL)?;
        let (sender, receiver) = queue(QUEUE);
        let lost = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let lost = Arc::clone(&lost);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("taskstats".into())
                .spawn(move || {
                    let mut buf = vec![0_u8; 65_536];
                    while !stop.load(Ordering::Relaxed) {
                        match socket.receive(&mut buf) {
                            Ok(Received::Data(len)) => {
                                let at = epoch_now();
                                for bytes in exit_records(&buf[..len], socket.family()) {
                                    let Some(exit) = record::parse(bytes) else {
                                        continue;
                                    };
                                    if !sender.offer(Received1 { at, exit }) {
                                        lost.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            Ok(Received::Idle) => {}
                            Ok(Received::Overrun) => {
                                // The kernel does not say how many.
                                lost.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(error) => {
                                eprintln!("timeless-acct: taskstats read failed: {error}");
                                thread::sleep(POLL);
                            }
                        }
                    }
                })?
        };

        Ok(Self {
            receiver,
            lost,
            stop,
            thread: Some(thread),
        })
    }

    /// Everything received since the last call.
    pub fn drain(&self) -> Vec<Received1> {
        self.receiver.drain()
    }

    /// Times records were lost: a full kernel buffer or a full queue. Each
    /// kernel overrun counts once however many records it cost.
    pub fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The accounting of one whole process, all of its threads together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessExit {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub gid: u32,
    pub comm: String,
    pub nice: i8,
    /// A wait status.
    pub exit_status: u32,
    pub flag: u8,
    /// Epoch seconds.
    pub start_epoch: f64,
    pub end_epoch: f64,
    pub elapsed_seconds: f64,
    pub user_seconds: f64,
    pub system_seconds: f64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub peak_rss_bytes: u64,
    pub peak_vm_bytes: u64,
    pub read_char: u64,
    pub write_char: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
    pub cpu_delay_seconds: f64,
    pub blkio_delay_seconds: f64,
    pub swapin_delay_seconds: f64,
    pub reclaim_delay_seconds: f64,
    pub thrashing_delay_seconds: f64,
    /// Threads whose exit was seen, the last one included.
    pub threads: u32,
    /// Zero for a kernel thread.
    pub exe_inode: u64,
    /// It was made by fork and never called exec: it ran what its parent
    /// was running.
    pub forked: bool,
}

impl ProcessExit {
    pub fn cpu_seconds(&self) -> f64 {
        self.user_seconds + self.system_seconds
    }
}

/// The threads of a process that have exited while it lives on.
#[derive(Default)]
struct Partial {
    threads: u32,
    user_us: u64,
    system_us: u64,
    minor_faults: u64,
    major_faults: u64,
    peak_rss_kb: u64,
    peak_vm_kb: u64,
    read_char: u64,
    write_char: u64,
    read_bytes: u64,
    write_bytes: u64,
    voluntary_switches: u64,
    involuntary_switches: u64,
    cpu_delay_ns: u64,
    blkio_delay_ns: u64,
    swapin_delay_ns: u64,
    reclaim_delay_ns: u64,
    thrashing_delay_ns: u64,
    /// The name of the thread group's leader, which is the process's name.
    /// Other threads may carry names of their own.
    leader_comm: Option<String>,
    leader_forked: bool,
    updated: f64,
}

impl Partial {
    fn add(&mut self, exit: &TaskExit, at: f64) {
        self.threads += 1;
        self.user_us += exit.user_us;
        self.system_us += exit.system_us;
        self.minor_faults += exit.minor_faults;
        self.major_faults += exit.major_faults;
        // Threads share one address space; its high-water mark is a
        // property of the process, reported by each of them.
        self.peak_rss_kb = self.peak_rss_kb.max(exit.peak_rss_kb);
        self.peak_vm_kb = self.peak_vm_kb.max(exit.peak_vm_kb);
        self.read_char += exit.read_char;
        self.write_char += exit.write_char;
        self.read_bytes += exit.read_bytes;
        self.write_bytes += exit.write_bytes;
        self.voluntary_switches += exit.voluntary_switches;
        self.involuntary_switches += exit.involuntary_switches;
        self.cpu_delay_ns += exit.cpu_delay_ns;
        self.blkio_delay_ns += exit.blkio_delay_ns;
        self.swapin_delay_ns += exit.swapin_delay_ns;
        self.reclaim_delay_ns += exit.reclaim_delay_ns;
        self.thrashing_delay_ns += exit.thrashing_delay_ns;
        if exit.pid == exit.tgid {
            self.leader_comm = Some(exit.comm.clone());
            self.leader_forked = exit.flag & AFORK != 0;
        }
        self.updated = at;
    }
}

/// Adds the threads of each process together.
#[derive(Default)]
pub struct Aggregator {
    partial: HashMap<u32, Partial>,
}

impl Aggregator {
    /// Take one task's exit. Returns the process's record if this was the
    /// last of its threads.
    pub fn push(&mut self, exit: &TaskExit, at: f64) -> Option<ProcessExit> {
        // Kernels before version 12 do not say which thread group a task
        // belonged to, so each task is accounted as a process of its own.
        let last = !exit.has_thread_group() || exit.is_last_in_group();
        if !last {
            self.partial.entry(exit.tgid).or_default().add(exit, at);
            return None;
        }
        let mut total = self.partial.remove(&exit.tgid).unwrap_or_default();
        total.add(exit, at);

        let elapsed_us = if exit.has_thread_group() {
            exit.group_elapsed_us
        } else {
            exit.elapsed_us
        };
        let elapsed_seconds = elapsed_us as f64 / 1e6;
        const NS: f64 = 1e9;
        Some(ProcessExit {
            pid: exit.tgid,
            ppid: exit.ppid,
            uid: exit.uid,
            gid: exit.gid,
            comm: total.leader_comm.unwrap_or_else(|| exit.comm.clone()),
            nice: exit.nice,
            exit_status: exit.exit_status,
            flag: exit.flag,
            // Finer than the record's own start time, which is in seconds.
            start_epoch: at - elapsed_seconds,
            end_epoch: at,
            elapsed_seconds,
            user_seconds: total.user_us as f64 / 1e6,
            system_seconds: total.system_us as f64 / 1e6,
            minor_faults: total.minor_faults,
            major_faults: total.major_faults,
            peak_rss_bytes: total.peak_rss_kb * 1024,
            peak_vm_bytes: total.peak_vm_kb * 1024,
            read_char: total.read_char,
            write_char: total.write_char,
            read_bytes: total.read_bytes,
            write_bytes: total.write_bytes,
            voluntary_switches: total.voluntary_switches,
            involuntary_switches: total.involuntary_switches,
            cpu_delay_seconds: total.cpu_delay_ns as f64 / NS,
            blkio_delay_seconds: total.blkio_delay_ns as f64 / NS,
            swapin_delay_seconds: total.swapin_delay_ns as f64 / NS,
            reclaim_delay_seconds: total.reclaim_delay_ns as f64 / NS,
            thrashing_delay_seconds: total.thrashing_delay_ns as f64 / NS,
            threads: total.threads,
            exe_inode: exit.exe_inode,
            forked: total.leader_forked,
        })
    }

    /// Forget processes that are gone without their last thread having been
    /// seen: its record was lost. `alive` says whether a pid still exists.
    pub fn prune(&mut self, now: f64, max_idle: f64, alive: impl Fn(u32) -> bool) {
        self.partial
            .retain(|pid, partial| now - partial.updated < max_idle || alive(*pid));
        settle(&mut self.partial);
    }

    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.partial.len()
    }
}

#[cfg(test)]
mod tests {
    use super::record::tests::sample;
    use super::record::AGROUP;
    use super::*;

    #[test]
    fn a_single_threaded_process_is_one_record() {
        let mut aggregator = Aggregator::default();
        let exit = aggregator.push(&sample(), 1000.0).unwrap();
        assert_eq!(exit.pid, 4242);
        assert_eq!(exit.comm, "cc1plus");
        assert_eq!(exit.threads, 1);
        assert_eq!(exit.elapsed_seconds, 2.5);
        assert_eq!(exit.start_epoch, 997.5);
        assert_eq!(exit.end_epoch, 1000.0);
        assert_eq!(exit.user_seconds, 1.9);
        assert_eq!(exit.system_seconds, 0.3);
        assert_eq!(exit.peak_rss_bytes, 512_000 * 1024);
        assert_eq!(exit.cpu_delay_seconds, 0.15);
        assert_eq!(aggregator.pending(), 0);
    }

    #[test]
    fn the_threads_of_a_process_are_added_together() {
        let mut aggregator = Aggregator::default();
        let worker = |pid: u32, comm: &str| TaskExit {
            pid,
            tgid: 500,
            comm: comm.into(),
            flag: 0,
            exit_status: 0,
            user_us: 1_000_000,
            system_us: 250_000,
            read_bytes: 100,
            peak_rss_kb: 2000,
            group_elapsed_us: 1_000_000,
            ..sample()
        };
        assert!(aggregator
            .push(&worker(501, "tokio-worker"), 10.0)
            .is_none());
        assert!(aggregator
            .push(&worker(502, "tokio-worker"), 11.0)
            .is_none());
        // The leader exits, and another thread outlives it.
        assert!(aggregator.push(&worker(500, "server"), 12.0).is_none());
        assert_eq!(aggregator.pending(), 1);

        let last = TaskExit {
            flag: AGROUP,
            exit_status: 0x0200,
            peak_rss_kb: 3000,
            group_elapsed_us: 60_000_000,
            ..worker(503, "tokio-worker")
        };
        let exit = aggregator.push(&last, 70.0).unwrap();
        assert_eq!(exit.pid, 500);
        // Named for its leader, not for the thread that happened to be last.
        assert_eq!(exit.comm, "server");
        assert_eq!(exit.threads, 4);
        assert_eq!(exit.user_seconds, 4.0);
        assert_eq!(exit.system_seconds, 1.0);
        assert_eq!(exit.read_bytes, 400);
        assert_eq!(exit.peak_rss_bytes, 3000 * 1024);
        assert_eq!(exit.exit_status, 0x0200);
        assert_eq!(exit.elapsed_seconds, 60.0);
        assert_eq!(exit.start_epoch, 10.0);
        assert_eq!(aggregator.pending(), 0);
    }

    #[test]
    fn a_process_that_never_called_exec_is_marked_as_forked() {
        let mut aggregator = Aggregator::default();
        let child = TaskExit {
            flag: AGROUP | AFORK,
            ..sample()
        };
        assert!(aggregator.push(&child, 10.0).unwrap().forked);
        assert!(!aggregator.push(&sample(), 10.0).unwrap().forked);

        // The flag is the leader's to carry, whichever thread is last.
        let leader = TaskExit {
            pid: 500,
            tgid: 500,
            flag: AFORK,
            ..sample()
        };
        let last = TaskExit {
            pid: 501,
            tgid: 500,
            flag: AGROUP,
            ..sample()
        };
        assert!(aggregator.push(&leader, 10.0).is_none());
        assert!(aggregator.push(&last, 11.0).unwrap().forked);
    }

    #[test]
    fn an_older_kernel_accounts_each_task_on_its_own() {
        let mut aggregator = Aggregator::default();
        let task = TaskExit {
            version: 9,
            flag: 0,
            tgid: 4242,
            elapsed_us: 3_000_000,
            group_elapsed_us: 0,
            ..sample()
        };
        let exit = aggregator.push(&task, 100.0).unwrap();
        assert_eq!(exit.elapsed_seconds, 3.0);
        assert_eq!(exit.threads, 1);
    }

    #[test]
    fn a_process_whose_last_record_was_lost_is_forgotten_once_it_is_gone() {
        let mut aggregator = Aggregator::default();
        let thread = TaskExit {
            pid: 601,
            tgid: 600,
            flag: 0,
            ..sample()
        };
        aggregator.push(&thread, 10.0);
        aggregator.prune(20.0, 60.0, |_| false);
        assert_eq!(
            aggregator.pending(),
            1,
            "recent: its last record may still come"
        );
        aggregator.prune(100.0, 60.0, |_| true);
        assert_eq!(aggregator.pending(), 1, "old, but the process is alive");
        aggregator.prune(100.0, 60.0, |_| false);
        assert_eq!(aggregator.pending(), 0);
    }
}
