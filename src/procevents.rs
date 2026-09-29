//! Word of each exec, from the kernel's process connector.
//!
//! The kernel's record of an exit has the command's name and its numbers,
//! and not its arguments: `cc1plus`, and not what was being compiled. The
//! arguments can only be read from `/proc` while the process is alive, and
//! most processes are gone before a sweep comes round.
//!
//! The connector says when a process calls exec. This listener reads what
//! the process is running at that moment, and the description waits for
//! the exit record it belongs to.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::collect::process::Units;
use crate::netlink::{message, messages, Netlink, Received, NLMSG_DONE};
use crate::procfs::process::parse_pid_stat;
use crate::procfs::{Described, ProcRoot};
use crate::queue::{queue, Receiver};
use crate::taskstats::epoch_now;

const NETLINK_CONNECTOR: libc::c_int = 11;
/// The connector's multicast group, and message id, for process events.
const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const PROC_CN_MCAST_IGNORE: u32 = 2;

/// `struct cn_msg`: id (index, value), sequence, ack, length, flags.
const CN_HEADER: usize = 20;
/// Offsets into `struct proc_event`.
const EVENT_WHAT: usize = 0;
const EVENT_DATA: usize = 16;
/// Within the event's data, for an exec: the thread, then its process.
const EXEC_TGID: usize = EVENT_DATA + 4;
/// Within the event's data, for the kernel's answer to a request.
const ACK_ERROR: usize = EVENT_DATA;

const PROC_EVENT_NONE: u32 = 0;
const PROC_EVENT_EXEC: u32 = 2;

const POLL: Duration = Duration::from_millis(250);
/// Descriptions waiting for the collector, at the most. One is larger
/// than an exit record, by a command line.
const QUEUE: usize = 131_072;

/// What the connector said, as far as this listener cares.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// The kernel's answer to a request: zero, or an errno.
    Ack(u32),
    /// This process called exec.
    Exec(u32),
    Other,
}

/// The events in one datagram.
pub fn events(datagram: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    for message in messages(datagram) {
        let body = message.body;
        if body.len() < CN_HEADER + EVENT_DATA + 8 {
            continue;
        }
        let word = |at: usize| u32::from_ne_bytes(body[at..at + 4].try_into().expect("4 bytes"));
        if word(0) != CN_IDX_PROC || word(4) != CN_VAL_PROC {
            continue;
        }
        let event = CN_HEADER;
        out.push(match word(event + EVENT_WHAT) {
            PROC_EVENT_NONE => Event::Ack(word(event + ACK_ERROR)),
            PROC_EVENT_EXEC => Event::Exec(word(event + EXEC_TGID)),
            _ => Event::Other,
        });
    }
    out
}

/// Ask the connector to start or stop sending.
///
/// The request carries the operation alone, which every kernel accepts and
/// answers with every kind of event. Newer kernels can be asked for exec
/// events only, in a longer request that older ones discard without a
/// word; sorting the events here costs less than telling the two apart.
fn request(operation: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(CN_HEADER + 4);
    body.extend_from_slice(&CN_IDX_PROC.to_ne_bytes());
    body.extend_from_slice(&CN_VAL_PROC.to_ne_bytes());
    body.extend_from_slice(&0_u32.to_ne_bytes()); // sequence
    body.extend_from_slice(&0_u32.to_ne_bytes()); // ack
    body.extend_from_slice(&4_u16.to_ne_bytes()); // length of what follows
    body.extend_from_slice(&0_u16.to_ne_bytes()); // flags
    body.extend_from_slice(&operation.to_ne_bytes());
    message(NLMSG_DONE, 0, &body)
}

/// A process, described at the moment it called exec.
#[derive(Debug, Clone)]
pub struct Exec {
    pub pid: u32,
    /// Epoch seconds.
    pub at: f64,
    pub start_ticks: u64,
    /// Epoch seconds.
    pub start_epoch: f64,
    pub ppid: u32,
    /// Its process group. A shell puts a command in its group before the
    /// command calls exec, so this is the job's.
    pub pgid: u32,
    /// Its name after the exec: what a child of it that never calls exec
    /// will be called too.
    pub comm: String,
    pub described: Described,
    /// What it was running before, if this was not its first exec: a
    /// shell runs the last command it is given in its own place.
    pub started_as: Option<String>,
}

/// Read what a process is running. `None` if it is already gone.
///
/// As little is read as will do: each read is time in which a process that
/// lives for a millisecond can end.
pub fn describe(
    root: &ProcRoot,
    pid: u32,
    units: Units,
    boot_epoch: f64,
    cmdline_max: usize,
    buf: &mut String,
) -> Option<Exec> {
    root.read_pid(pid, "stat", buf).ok()?;
    let stat = parse_pid_stat(buf)?;
    let described = root.describe(pid, cmdline_max, buf);
    // A process that exits while it is being read leaves some of its files
    // readable and empty. Its name alone is what the exit record has
    // already.
    if described.cmdline.is_empty() && described.exe.is_empty() {
        return None;
    }
    Some(Exec {
        pid,
        at: epoch_now(),
        start_ticks: stat.start_ticks,
        start_epoch: boot_epoch + stat.start_ticks as f64 / units.ticks_per_second,
        ppid: stat.ppid,
        pgid: stat.pgrp,
        comm: stat.comm,
        described,
        started_as: None,
    })
}

#[derive(Default)]
struct Counters {
    seen: AtomicU64,
    missed: AtomicU64,
    lost: AtomicU64,
}

pub struct ExecListener {
    receiver: Receiver<Exec>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ExecListener {
    /// Subscribe and start reading. Fails with `PermissionDenied` without
    /// `CAP_NET_ADMIN`.
    pub fn start(
        root: ProcRoot,
        units: Units,
        boot_epoch: f64,
        cmdline_max: usize,
    ) -> io::Result<Self> {
        let netlink = Netlink::open(NETLINK_CONNECTOR, CN_IDX_PROC)?;
        netlink.set_timeout(Duration::from_secs(2))?;
        netlink.grow_receive_buffer()?;
        netlink.send(&request(PROC_CN_MCAST_LISTEN))?;
        acknowledged(&netlink)?;
        netlink.set_timeout(POLL)?;

        let (sender, receiver) = queue(QUEUE);
        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let counters = Arc::clone(&counters);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("procevents".into())
                .spawn(move || {
                    let mut datagram = vec![0_u8; 65_536];
                    let mut buf = String::with_capacity(4096);
                    while !stop.load(Ordering::Relaxed) {
                        match netlink.receive(&mut datagram) {
                            Ok(Received::Data(len)) => {
                                for event in events(&datagram[..len]) {
                                    let Event::Exec(pid) = event else {
                                        continue;
                                    };
                                    counters.seen.fetch_add(1, Ordering::Relaxed);
                                    let exec = describe(
                                        &root,
                                        pid,
                                        units,
                                        boot_epoch,
                                        cmdline_max,
                                        &mut buf,
                                    );
                                    match exec {
                                        None => {
                                            counters.missed.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Some(exec) => {
                                            if !sender.offer(exec) {
                                                counters.lost.fetch_add(1, Ordering::Relaxed);
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(Received::Idle) => {}
                            Ok(Received::Overrun) => {
                                counters.lost.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(error) => {
                                eprintln!("timeless-acct: process events read failed: {error}");
                                thread::sleep(POLL);
                            }
                        }
                    }
                    // Closing the socket ends the subscription as well.
                    let _ = netlink.send(&request(PROC_CN_MCAST_IGNORE));
                })?
        };

        Ok(Self {
            receiver,
            counters,
            stop,
            thread: Some(thread),
        })
    }

    /// Everything described since the last call.
    pub fn drain(&self) -> Vec<Exec> {
        self.receiver.drain()
    }

    /// Execs the kernel reported.
    pub fn seen(&self) -> u64 {
        self.counters.seen.load(Ordering::Relaxed)
    }

    /// Execs of processes that were gone before they could be read.
    pub fn missed(&self) -> u64 {
        self.counters.missed.load(Ordering::Relaxed)
    }

    /// Times events were lost: a full kernel buffer, or a full queue.
    pub fn lost(&self) -> u64 {
        self.counters.lost.load(Ordering::Relaxed)
    }
}

impl Drop for ExecListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Wait for the kernel's answer to the subscription. Events of other
/// listeners' making may arrive first.
fn acknowledged(netlink: &Netlink) -> io::Result<()> {
    let mut datagram = vec![0_u8; 65_536];
    for _ in 0..256 {
        let Received::Data(len) = netlink.receive(&mut datagram)? else {
            break;
        };
        for event in events(&datagram[..len]) {
            match event {
                Event::Ack(0) => return Ok(()),
                Event::Ack(errno) => return Err(io::Error::from_raw_os_error(errno as i32)),
                _ => {}
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "no answer from the process connector (CONFIG_PROC_EVENTS)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;

    /// An event the way the kernel lays one out.
    fn event(what: u32, first: u32, second: u32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&CN_IDX_PROC.to_ne_bytes());
        body.extend_from_slice(&CN_VAL_PROC.to_ne_bytes());
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&40_u16.to_ne_bytes());
        body.extend_from_slice(&0_u16.to_ne_bytes());
        let mut event = vec![0_u8; 40];
        event[EVENT_WHAT..EVENT_WHAT + 4].copy_from_slice(&what.to_ne_bytes());
        event[EVENT_DATA..EVENT_DATA + 4].copy_from_slice(&first.to_ne_bytes());
        event[EVENT_DATA + 4..EVENT_DATA + 8].copy_from_slice(&second.to_ne_bytes());
        body.extend(event);
        message(NLMSG_DONE, 0, &body)
    }

    #[test]
    fn an_exec_is_reported_for_the_process_not_the_thread() {
        // Thread 4243 of process 4242 called exec.
        let datagram = event(PROC_EVENT_EXEC, 4243, 4242);
        assert_eq!(events(&datagram), [Event::Exec(4242)]);
    }

    #[test]
    fn forks_and_exits_are_not_execs() {
        let mut datagram = event(0x1, 100, 100);
        datagram.extend(event(PROC_EVENT_EXEC, 7, 7));
        datagram.extend(event(0x8000_0000, 100, 100));
        assert_eq!(
            events(&datagram),
            [Event::Other, Event::Exec(7), Event::Other]
        );
    }

    #[test]
    fn the_kernels_answer_carries_its_verdict() {
        assert_eq!(events(&event(PROC_EVENT_NONE, 0, 0)), [Event::Ack(0)]);
        assert_eq!(
            events(&event(PROC_EVENT_NONE, libc::EPERM as u32, 0)),
            [Event::Ack(libc::EPERM as u32)]
        );
    }

    #[test]
    fn what_is_not_a_process_event_is_ignored() {
        let mut other = event(PROC_EVENT_EXEC, 7, 7);
        // Another connector user's message id.
        other[16..20].copy_from_slice(&9_u32.to_ne_bytes());
        assert!(events(&other).is_empty());
        let mut short = event(PROC_EVENT_EXEC, 7, 7);
        short.truncate(30);
        assert!(events(&short).is_empty());
    }

    #[test]
    fn a_request_is_a_connector_message_with_the_operation_in_it() {
        let bytes = request(PROC_CN_MCAST_LISTEN);
        assert_eq!(bytes.len(), 16 + CN_HEADER + 4);
        assert_eq!(u16::from_ne_bytes([bytes[32], bytes[33]]), 4);
        assert_eq!(
            u32::from_ne_bytes(bytes[36..40].try_into().unwrap()),
            PROC_CN_MCAST_LISTEN
        );
    }

    const UNITS: Units = Units {
        ticks_per_second: 100.0,
        page_bytes: 4096,
    };

    #[test]
    fn a_process_is_described_as_it_is_at_the_moment() {
        let fixture = Fixture::new("procevents_describe");
        fixture.proc_file(
            "77/stat",
            "77 (cc1plus) R 70 70 70 0 -1 0 0 0 0 0 5 1 0 0 20 0 1 0 250000 1000 10 0\n",
        );
        fixture.proc_file("77/cmdline", "/usr/lib/gcc/cc1plus\0-quiet\0main.cpp\0");
        fixture.proc_file(
            "77/cgroup",
            "0::/user.slice/user-1000.slice/session-2.scope\n",
        );
        let mut buf = String::new();

        let exec = describe(&fixture.root(), 77, UNITS, 1_000_000.0, 1024, &mut buf).unwrap();
        assert_eq!(exec.pid, 77);
        assert_eq!(exec.comm, "cc1plus");
        assert_eq!((exec.ppid, exec.pgid), (70, 70));
        assert_eq!(exec.start_ticks, 250_000);
        assert_eq!(exec.start_epoch, 1_002_500.0);
        assert_eq!(
            exec.described.cmdline,
            "/usr/lib/gcc/cc1plus -quiet main.cpp"
        );
        assert_eq!(
            exec.described.cgroup,
            "/user.slice/user-1000.slice/session-2.scope"
        );
    }

    #[test]
    fn a_process_that_is_already_gone_is_not_described() {
        let fixture = Fixture::new("procevents_gone");
        let mut buf = String::new();
        assert!(describe(&fixture.root(), 78, UNITS, 0.0, 1024, &mut buf).is_none());

        // Exiting while it is read: its files are there and empty.
        fixture.proc_file(
            "79/stat",
            "79 (true) Z 70 70 70 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 250000 0 0 0\n",
        );
        fixture.proc_file("79/cmdline", "");
        assert!(describe(&fixture.root(), 79, UNITS, 0.0, 1024, &mut buf).is_none());
    }
}
