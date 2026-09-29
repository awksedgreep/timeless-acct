//! Parsers for the per-process files.

/// Set in the `flags` field of `/proc/<pid>/stat` for a kernel thread.
const PF_KTHREAD: u64 = 0x0020_0000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PidStat {
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    /// Its process group: the job it is part of.
    pub pgrp: u32,
    pub flags: u64,
    pub minflt: u64,
    pub majflt: u64,
    /// Ticks.
    pub utime: u64,
    /// Ticks.
    pub stime: u64,
    pub nice: i64,
    pub threads: u64,
    /// Ticks since boot. With the pid, this identifies a process: a reused
    /// pid has a different start time.
    pub start_ticks: u64,
    pub vsize_bytes: u64,
    pub rss_pages: u64,
    /// Ticks spent waiting on block I/O. Zero unless delay accounting is on.
    pub blkio_ticks: u64,
}

impl PidStat {
    pub fn is_kernel_thread(&self) -> bool {
        self.flags & PF_KTHREAD != 0
    }
}

/// Parse `/proc/<pid>/stat`.
///
/// The command name sits in parentheses and may itself contain spaces and
/// parentheses, so the numeric fields are located from the LAST closing
/// parenthesis rather than by splitting the line.
pub fn parse_pid_stat(text: &str) -> Option<PidStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = text[open + 1..close].to_string();
    let fields: Vec<&str> = text[close + 1..].split_ascii_whitespace().collect();
    // Field N of proc(5), counted from 1, is index N - 3 here: the pid and
    // the command name come before, and state (field 3) is index 0.
    let number = |field: usize| -> Option<u64> { fields.get(field - 3)?.parse().ok() };

    Some(PidStat {
        comm,
        state: fields.first()?.chars().next()?,
        ppid: number(4)? as u32,
        pgrp: number(5)? as u32,
        flags: number(9)?,
        minflt: number(10)?,
        majflt: number(12)?,
        utime: number(14)?,
        stime: number(15)?,
        nice: fields.get(19 - 3)?.parse().ok()?,
        threads: number(20)?,
        start_ticks: number(22)?,
        vsize_bytes: number(23)?,
        rss_pages: number(24)?,
        blkio_ticks: number(42).unwrap_or(0),
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidStatus {
    /// Effective uid: whose authority the process acts with.
    pub uid: u32,
    /// Absent for kernel threads, which have no address space.
    pub rss_kb: Option<u64>,
    pub swap_kb: Option<u64>,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
}

pub fn parse_pid_status(text: &str) -> PidStatus {
    let mut status = PidStatus::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let mut fields = rest.split_ascii_whitespace();
        match key {
            // Uid: real effective saved filesystem
            "Uid" => {
                if let Some(uid) = fields.nth(1).and_then(|f| f.parse().ok()) {
                    status.uid = uid;
                }
            }
            "VmRSS" => status.rss_kb = fields.next().and_then(|f| f.parse().ok()),
            "VmSwap" => status.swap_kb = fields.next().and_then(|f| f.parse().ok()),
            "voluntary_ctxt_switches" => {
                status.voluntary_switches = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0)
            }
            "nonvoluntary_ctxt_switches" => {
                status.involuntary_switches =
                    fields.next().and_then(|f| f.parse().ok()).unwrap_or(0)
            }
            _ => {}
        }
    }
    status
}

/// Bytes that reached, or were read from, the storage layer. `rchar` and
/// `wchar` count every read and write call, page cache and pipes included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidIo {
    pub rchar: u64,
    pub wchar: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
}

pub fn parse_pid_io(text: &str) -> PidIo {
    let mut io = PidIo::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().parse().unwrap_or(0);
        match key {
            "rchar" => io.rchar = value,
            "wchar" => io.wchar = value,
            "read_bytes" => io.read_bytes = value,
            "write_bytes" => io.write_bytes = value,
            _ => {}
        }
    }
    io
}

/// `/proc/<pid>/schedstat`: nanoseconds on a CPU, nanoseconds runnable but
/// waiting for one, and timeslices run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidSchedstat {
    pub run_ns: u64,
    pub wait_ns: u64,
}

pub fn parse_pid_schedstat(text: &str) -> Option<PidSchedstat> {
    let mut fields = text.split_ascii_whitespace();
    Some(PidSchedstat {
        run_ns: fields.next()?.parse().ok()?,
        wait_ns: fields.next()?.parse().ok()?,
    })
}

/// What stands in a command line for what was taken out of it.
pub const WITHHELD: &str = "***";

/// Words that, in the name of an argument, say its value is a secret.
const SECRET: [&str; 10] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "cookie",
    "credential",
    "apikey",
    "api-key",
    "api_key",
];

fn names_a_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET.iter().any(|word| name.contains(word))
}

/// An argument with what is secret in it withheld, and whether it says
/// that the next argument is the secret.
///
/// A command line is there for anyone on the host to read while the
/// process runs. Kept in a store, it is there for anyone who can read the
/// store, for as long as the store is kept, and wherever the store is
/// sent. So what is plainly a secret is not kept:
///
/// - the value of `--password=…`, `TOKEN=…`, and their like;
/// - the argument after `--password`, `-setcookie`, and their like;
/// - the password in `scheme://user:password@host`.
///
/// A secret that does not say it is one is kept, as it is shown by `ps`.
fn withhold(arg: &str) -> (String, bool) {
    if let Some((name, value)) = arg.split_once('=') {
        if names_a_secret(name) && !value.is_empty() {
            return (format!("{name}={WITHHELD}"), false);
        }
    } else if arg.starts_with('-') && names_a_secret(arg) {
        return (arg.to_string(), true);
    }
    // user:password@ in a URL, in an argument or in the value of one.
    if let Some(scheme) = arg.find("://") {
        let rest = &arg[scheme + 3..];
        let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
        if let Some((user, _)) = authority
            .rsplit_once('@')
            .and_then(|(userinfo, _)| userinfo.split_once(':'))
        {
            let host = &rest[authority.rfind('@').unwrap_or(0)..];
            return (
                format!("{}{user}:{WITHHELD}{host}", &arg[..scheme + 3]),
                false,
            );
        }
    }
    (arg.to_string(), false)
}

/// `/proc/<pid>/cmdline` separates arguments with NUL bytes. Joined with
/// spaces and bounded, because one process can carry megabytes of
/// arguments; and with what is plainly a secret withheld.
pub fn parse_cmdline(text: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut secret = false;
    for arg in text.split('\0').filter(|arg| !arg.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        if secret {
            out.push_str(WITHHELD);
            secret = false;
        } else {
            let (arg, next) = withhold(arg);
            out.push_str(&arg);
            secret = next;
        }
        if out.len() > max_len {
            break;
        }
    }
    if out.len() > max_len {
        let mut cut = max_len;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push('…');
    }
    out
}

/// Whether a command line is that of a launcher: a program whose whole
/// purpose is to become another, and which says nothing of what was
/// started. systemd starts every process of every unit through one.
pub fn is_launcher(cmdline: &str) -> bool {
    let program = cmdline.split(' ').next().unwrap_or("");
    program.rsplit('/').next() == Some("systemd-executor")
}

/// The name a process is grouped under.
///
/// Kernel threads are named per CPU and per work item
/// (`kworker/3:1-events`, `ksoftirqd/7`); grouping by the full name would
/// make one group per thread. The part before the first `/` or `:` is the
/// kind of thread it is.
pub fn group_name(comm: &str, kernel_thread: bool) -> &str {
    if !kernel_thread {
        return comm;
    }
    let end = comm.find(['/', ':']).unwrap_or(comm.len());
    if end == 0 {
        comm
    } else {
        &comm[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat_line(comm: &str, flags: u64) -> String {
        format!(
            "4242 ({comm}) S 1 4242 4242 0 -1 {flags} 1500 0 12 0 250 80 0 0 20 0 7 0 \
             987654 104857600 5000 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 \
             33 0 0 0 0 0 0 0 0 0 0\n"
        )
    }

    #[test]
    fn stat_reads_fields_by_their_documented_position() {
        let stat = parse_pid_stat(&stat_line("firefox", 4_194_304)).unwrap();
        assert_eq!(stat.comm, "firefox");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.pgrp, 4242);
        assert_eq!(stat.minflt, 1500);
        assert_eq!(stat.majflt, 12);
        assert_eq!(stat.utime, 250);
        assert_eq!(stat.stime, 80);
        assert_eq!(stat.nice, 0);
        assert_eq!(stat.threads, 7);
        assert_eq!(stat.start_ticks, 987_654);
        assert_eq!(stat.vsize_bytes, 104_857_600);
        assert_eq!(stat.rss_pages, 5000);
        assert_eq!(stat.blkio_ticks, 33);
        assert!(!stat.is_kernel_thread());
    }

    #[test]
    fn stat_survives_a_hostile_command_name() {
        let stat = parse_pid_stat(&stat_line("evil) R 0 (name", 0)).unwrap();
        assert_eq!(stat.comm, "evil) R 0 (name");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.utime, 250);
    }

    #[test]
    fn stat_recognizes_kernel_threads() {
        let stat = parse_pid_stat(&stat_line("kworker/3:1-events", 0x0020_0000 | 0x40)).unwrap();
        assert!(stat.is_kernel_thread());
    }

    #[test]
    fn stat_rejects_truncated_input() {
        assert!(parse_pid_stat("4242 (cut) S 1 2").is_none());
        assert!(parse_pid_stat("").is_none());
    }

    #[test]
    fn status_reads_the_effective_uid_and_memory() {
        let status = parse_pid_status(
            "Name:\tbash\nUid:\t1000\t0\t1000\t1000\nVmRSS:\t    5120 kB\nVmSwap:\t     64 kB\n\
             voluntary_ctxt_switches:\t150\nnonvoluntary_ctxt_switches:\t9\n",
        );
        assert_eq!(status.uid, 0);
        assert_eq!(status.rss_kb, Some(5120));
        assert_eq!(status.swap_kb, Some(64));
        assert_eq!(status.voluntary_switches, 150);
        assert_eq!(status.involuntary_switches, 9);
    }

    #[test]
    fn status_of_a_kernel_thread_has_no_memory() {
        let status = parse_pid_status("Name:\tkthreadd\nUid:\t0\t0\t0\t0\n");
        assert_eq!(status.rss_kb, None);
        assert_eq!(status.swap_kb, None);
    }

    #[test]
    fn io_reads_storage_and_call_level_bytes() {
        let io = parse_pid_io(
            "rchar: 1000\nwchar: 2000\nsyscr: 5\nsyscw: 6\nread_bytes: 4096\n\
             write_bytes: 8192\ncancelled_write_bytes: 0\n",
        );
        assert_eq!(
            io,
            PidIo {
                rchar: 1000,
                wchar: 2000,
                read_bytes: 4096,
                write_bytes: 8192
            }
        );
    }

    #[test]
    fn schedstat_reads_run_and_wait() {
        let sched = parse_pid_schedstat("123456789 4567 890\n").unwrap();
        assert_eq!(sched.run_ns, 123_456_789);
        assert_eq!(sched.wait_ns, 4567);
        assert!(parse_pid_schedstat("").is_none());
    }

    #[test]
    fn cmdline_joins_arguments_and_bounds_length() {
        assert_eq!(parse_cmdline("sleep\x0010\0", 100), "sleep 10");
        assert_eq!(parse_cmdline("", 100), "");
        let long = parse_cmdline(&format!("cmd\0{}\0", "x".repeat(500)), 20);
        assert_eq!(long.chars().count(), 21);
        assert!(long.ends_with('…'));
    }

    fn cmdline(args: &[&str]) -> String {
        parse_cmdline(&args.join("\0"), 4096)
    }

    #[test]
    fn what_is_plainly_a_secret_is_not_kept() {
        for (args, kept) in [
            (
                vec!["mysql", "--user=app", "--password=hunter2", "db"],
                "mysql --user=app --password=*** db",
            ),
            (
                vec!["beam.smp", "-setcookie", "K7Q2M9X4TZ", "-noshell"],
                "beam.smp -setcookie *** -noshell",
            ),
            (
                vec!["curl", "--oauth2-bearer-token", "abc123", "https://x"],
                "curl --oauth2-bearer-token *** https://x",
            ),
            (
                vec!["env", "API_KEY=abc", "AWS_SECRET_ACCESS_KEY=xyz", "run"],
                "env API_KEY=*** AWS_SECRET_ACCESS_KEY=*** run",
            ),
            (
                vec!["psql", "postgres://app:hunter2@db.example:5432/main?ssl=1"],
                "psql postgres://app:***@db.example:5432/main?ssl=1",
            ),
            (
                vec!["app", "--db=mysql://root:pw@localhost/x"],
                "app --db=mysql://root:***@localhost/x",
            ),
            // The last argument says the next is a secret, and there is
            // no next.
            (vec!["vault", "login", "--token"], "vault login --token"),
        ] {
            assert_eq!(cmdline(&args), kept);
        }
    }

    #[test]
    fn what_is_not_a_secret_is_kept_as_it_is() {
        for args in [
            vec!["cc", "-c", "a.c", "-o", "a.o"],
            vec![
                "git",
                "clone",
                "https://github.com/awksedgreep/timeless-libsql",
            ],
            vec!["ssh", "mark@host"],
            vec!["rsync", "-a", "user@host:/srv/", "/backup/"],
            // No value to withhold.
            vec!["app", "--password="],
            // An argument that is about secrets, and is not one.
            vec!["grep", "password", "/etc/app.conf"],
            vec!["curl", "http://host/path?x=1@2:3"],
        ] {
            assert_eq!(cmdline(&args), args.join(" "));
        }
    }

    #[test]
    fn systemds_launcher_is_not_what_a_process_was_started_as() {
        assert!(is_launcher(
            "/usr/lib/systemd/systemd-executor --deserialize 100 --log-level notice"
        ));
        assert!(is_launcher("systemd-executor"));
        assert!(!is_launcher("sh -c /usr/lib/systemd/systemd-executor"));
        assert!(!is_launcher("setsid ./build.sh"));
        assert!(!is_launcher(""));
    }

    #[test]
    fn kernel_threads_group_by_kind() {
        assert_eq!(group_name("kworker/3:1-events", true), "kworker");
        assert_eq!(group_name("ksoftirqd/7", true), "ksoftirqd");
        assert_eq!(group_name("kthreadd", true), "kthreadd");
        assert_eq!(group_name("irq/42-nvme", true), "irq");
        // A user process named with a slash is its own group.
        assert_eq!(group_name("a/b", false), "a/b");
    }
}
