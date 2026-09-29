//! One accounting record per process that ended.
//!
//! Records are log entries. Their metadata uses the keys the logs plane
//! indexes — `service`, `host`, `path`, `status` — so "every exit of
//! postgres", "everything that died of SIGSEGV", and "everything run from
//! /usr/bin/rsync" are index lookups, not scans.

use serde_json::{Map, Value};

use crate::collect::process::Tracked;
use crate::lineage::Identity;
use crate::model::{Event, Level, Span};
use crate::taskstats::record::ACORE;
use crate::taskstats::ProcessExit;

/// How a process ended, decoded from its wait status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Exited(u8),
    Signaled { signal: u8, core: bool },
}

impl Ending {
    /// The status decides, and the record's flags do not. When a process
    /// exits, the kernel ends its other threads with a signal of its own,
    /// so the last thread of a process that exited normally is flagged as
    /// signaled while carrying the process's ordinary exit status.
    pub fn from_status(status: u32, flag: u8) -> Self {
        let signal = (status & 0x7f) as u8;
        if signal != 0 {
            Ending::Signaled {
                signal,
                core: status & 0x80 != 0 || flag & ACORE != 0,
            }
        } else {
            Ending::Exited(((status >> 8) & 0xff) as u8)
        }
    }

    /// The value of the indexed `status` key: an exit code or a signal
    /// name.
    pub fn status(&self) -> String {
        match self {
            Ending::Exited(code) => code.to_string(),
            Ending::Signaled { signal, .. } => signal_name(*signal),
        }
    }

    /// A host's canvas element turns red on an error-level entry and amber
    /// on a warning, so the level is a judgement about the host, not the
    /// process. A program that faulted is a defect on the host. A kill may
    /// be the out-of-memory killer. A non-zero exit is how grep says "no
    /// match" and is nothing to colour a host for.
    pub fn level(&self) -> Level {
        match self {
            Ending::Exited(0) => Level::Info,
            Ending::Exited(_) => Level::Notice,
            Ending::Signaled { signal, .. } => match i32::from(*signal) {
                libc::SIGSEGV
                | libc::SIGBUS
                | libc::SIGILL
                | libc::SIGFPE
                | libc::SIGABRT
                | libc::SIGSYS => Level::Error,
                libc::SIGKILL => Level::Warning,
                _ => Level::Notice,
            },
        }
    }

    fn describe(&self) -> String {
        match self {
            Ending::Exited(code) => format!("exited {code}"),
            Ending::Signaled { signal, core: true } => {
                format!("killed by {} (core dumped)", signal_name(*signal))
            }
            Ending::Signaled {
                signal,
                core: false,
            } => {
                format!("killed by {}", signal_name(*signal))
            }
        }
    }
}

/// Signal names as numbered on x86-64 and aarch64.
pub fn signal_name(signal: u8) -> String {
    const NAMES: [&str; 31] = [
        "SIGHUP",
        "SIGINT",
        "SIGQUIT",
        "SIGILL",
        "SIGTRAP",
        "SIGABRT",
        "SIGBUS",
        "SIGFPE",
        "SIGKILL",
        "SIGUSR1",
        "SIGSEGV",
        "SIGUSR2",
        "SIGPIPE",
        "SIGALRM",
        "SIGTERM",
        "SIGSTKFLT",
        "SIGCHLD",
        "SIGCONT",
        "SIGSTOP",
        "SIGTSTP",
        "SIGTTIN",
        "SIGTTOU",
        "SIGURG",
        "SIGXCPU",
        "SIGXFSZ",
        "SIGVTALRM",
        "SIGPROF",
        "SIGWINCH",
        "SIGIO",
        "SIGPWR",
        "SIGSYS",
    ];
    match signal {
        1..=31 => NAMES[usize::from(signal) - 1].to_string(),
        other => format!("SIG{other}"),
    }
}

/// `2h03m`, `4m07s`, `12.5s`, `340ms`.
pub fn human_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    if seconds < 1.0 {
        format!("{:.0}ms", seconds * 1000.0)
    } else if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else if seconds < 3600.0 {
        format!(
            "{}m{:02}s",
            (seconds / 60.0) as u64,
            (seconds % 60.0) as u64
        )
    } else if seconds < 86_400.0 {
        format!(
            "{}h{:02}m",
            (seconds / 3600.0) as u64,
            ((seconds % 3600.0) / 60.0) as u64
        )
    } else {
        format!(
            "{}d{:02}h",
            (seconds / 86_400.0) as u64,
            ((seconds % 86_400.0) / 3600.0) as u64
        )
    }
}

/// `1.5 GiB`, `340 MiB`, `12 KiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Seconds, to the microsecond the kernel counts in. Stored figures should
/// not carry the noise of a binary fraction.
fn seconds(value: f64) -> Value {
    Value::from((value * 1e6).round() / 1e6)
}

fn percent_of(part: f64, whole: f64) -> Value {
    if whole > 0.0 {
        Value::from((1000.0 * part / whole).round() / 10.0)
    } else {
        Value::Null
    }
}

/// What a process was, which the kernel's record of its exit does not say:
/// the record has the command's name and the numbers. Each part is empty
/// if it was never learned.
#[derive(Debug, Clone, Copy, Default)]
pub struct Known<'a> {
    pub cmdline: &'a str,
    /// What it ran first, if that was something else.
    pub started_as: &'a str,
    pub exe: &'a str,
    pub unit: &'a str,
}

impl<'a> Known<'a> {
    pub fn of(tracked: &'a Tracked) -> Self {
        Self {
            cmdline: &tracked.cmdline,
            started_as: &tracked.started_as,
            exe: &tracked.exe,
            unit: &tracked.unit,
        }
    }

    fn put(&self, fields: &mut Map<String, Value>) {
        for (key, value) in [
            ("path", self.exe),
            ("cmdline", self.cmdline),
            ("started_as", self.started_as),
            ("unit", self.unit),
        ] {
            if !value.is_empty() {
                fields.insert(key.into(), value.into());
            }
        }
    }
}

/// The record of a process the kernel reported the exit of.
///
/// `tracked` is what sampling last saw of the process, if it lived long
/// enough to be sampled. `known` is what it was, from sampling or from the
/// moment it called exec.
pub fn exit_event(
    exit: &ProcessExit,
    tracked: Option<&Tracked>,
    known: Known,
    user: &str,
) -> Event {
    let ending = Ending::from_status(exit.exit_status, exit.flag);
    // A process's own figures include those of its threads that ended
    // before this collector was listening; the kernel's per-thread records
    // since then cannot. Whichever saw more is nearer the truth.
    let sampled_user = tracked.map_or(0.0, Tracked::cpu_user_seconds);
    let sampled_system = tracked.map_or(0.0, Tracked::cpu_system_seconds);
    let user_seconds = exit.user_seconds.max(sampled_user);
    let system_seconds = exit.system_seconds.max(sampled_system);
    let cpu = user_seconds + system_seconds;
    let peak_rss = exit
        .peak_rss_bytes
        .max(tracked.map_or(0, |t| t.peak_rss_bytes));
    let sampled_io = tracked.and_then(Tracked::io);

    let mut fields = Map::new();
    let mut put = |key: &str, value: Value| {
        fields.insert(key.to_string(), value);
    };
    put("kind", "exit".into());
    put("source", "taskstats".into());
    put("service", exit.comm.clone().into());
    put("status", ending.status().into());
    put("pid", exit.pid.into());
    put("ppid", exit.ppid.into());
    put("uid", exit.uid.into());
    put("gid", exit.gid.into());
    put("user", user.into());
    put("nice", exit.nice.into());
    match ending {
        Ending::Exited(code) => put("exit_code", code.into()),
        Ending::Signaled { signal, core } => {
            put("signal", signal_name(signal).into());
            put("core_dumped", core.into());
        }
    }
    put("started", seconds(exit.start_epoch));
    put("elapsed_seconds", seconds(exit.elapsed_seconds));
    put("cpu_seconds", seconds(cpu));
    put("cpu_user_seconds", seconds(user_seconds));
    put("cpu_system_seconds", seconds(system_seconds));
    put("cpu_pct", percent_of(cpu, exit.elapsed_seconds));
    put("threads", exit.threads.into());
    put("peak_rss_bytes", peak_rss.into());
    put("peak_vm_bytes", exit.peak_vm_bytes.into());
    put(
        "minor_faults",
        exit.minor_faults
            .max(tracked.map_or(0, Tracked::minor_faults))
            .into(),
    );
    put(
        "major_faults",
        exit.major_faults
            .max(tracked.map_or(0, Tracked::major_faults))
            .into(),
    );
    put(
        "io_read_bytes",
        exit.read_bytes
            .max(sampled_io.map_or(0, |io| io.read_bytes))
            .into(),
    );
    put(
        "io_write_bytes",
        exit.write_bytes
            .max(sampled_io.map_or(0, |io| io.write_bytes))
            .into(),
    );
    put(
        "io_read_chars",
        exit.read_char
            .max(sampled_io.map_or(0, |io| io.rchar))
            .into(),
    );
    put(
        "io_write_chars",
        exit.write_char
            .max(sampled_io.map_or(0, |io| io.wchar))
            .into(),
    );
    put("context_switches_voluntary", exit.voluntary_switches.into());
    put(
        "context_switches_involuntary",
        exit.involuntary_switches.into(),
    );
    put("delay_cpu_seconds", seconds(exit.cpu_delay_seconds));
    put("delay_blkio_seconds", seconds(exit.blkio_delay_seconds));
    put("delay_swapin_seconds", seconds(exit.swapin_delay_seconds));
    put("delay_reclaim_seconds", seconds(exit.reclaim_delay_seconds));
    put(
        "delay_thrashing_seconds",
        seconds(exit.thrashing_delay_seconds),
    );
    if exit.forked {
        put("forked", true.into());
    }
    known.put(&mut fields);

    Event {
        ts_us: (exit.end_epoch * 1e6) as i64,
        level: ending.level(),
        message: format!(
            "{}[{}] {} after {}, cpu {}, peak rss {}",
            exit.comm,
            exit.pid,
            ending.describe(),
            human_duration(exit.elapsed_seconds),
            human_duration(cpu),
            human_bytes(peak_rss),
        ),
        fields,
    }
}

/// The record of a process that was there at one sweep and gone at the
/// next, with no word from the kernel about how it ended.
///
/// Its figures are those of the last sweep that saw it: a floor, since the
/// process went on using CPU until some moment before `noticed`.
pub fn vanished_event(tracked: &Tracked, noticed: f64) -> Event {
    let cpu = tracked.cpu_seconds();
    let elapsed = (tracked.last_seen_epoch - tracked.start_epoch).max(0.0);

    let mut fields = Map::new();
    let mut put = |key: &str, value: Value| {
        fields.insert(key.to_string(), value);
    };
    put("kind", "exit".into());
    put("source", "sampled".into());
    put("service", tracked.comm.clone().into());
    put("status", "unknown".into());
    put("pid", tracked.pid.into());
    put("ppid", tracked.ppid.into());
    put("uid", tracked.uid.into());
    put("user", tracked.user.clone().into());
    put("started", seconds(tracked.start_epoch));
    put("last_seen", seconds(tracked.last_seen_epoch));
    put("elapsed_seconds", seconds(elapsed));
    put("cpu_seconds", seconds(cpu));
    put("cpu_user_seconds", seconds(tracked.cpu_user_seconds()));
    put("cpu_system_seconds", seconds(tracked.cpu_system_seconds()));
    put("cpu_pct", percent_of(cpu, elapsed));
    put("threads", tracked.threads.into());
    put("peak_rss_bytes", tracked.peak_rss_bytes.into());
    put("minor_faults", tracked.minor_faults().into());
    put("major_faults", tracked.major_faults().into());
    if let Some(io) = tracked.io() {
        put("io_read_bytes", io.read_bytes.into());
        put("io_write_bytes", io.write_bytes.into());
        put("io_read_chars", io.rchar.into());
        put("io_write_chars", io.wchar.into());
    }
    Known::of(tracked).put(&mut fields);

    Event {
        ts_us: (noticed * 1e6) as i64,
        level: Level::Info,
        message: format!(
            "{}[{}] gone after at least {}, cpu at least {}, peak rss {}",
            tracked.comm,
            tracked.pid,
            human_duration(elapsed),
            human_duration(cpu),
            human_bytes(tracked.peak_rss_bytes),
        ),
        fields,
    }
}

/// What a span says of a process, and the field of its accounting record
/// that each part is taken from. Where OpenTelemetry has a name for
/// something, that is the name.
const SPAN_ATTRIBUTES: [(&str, &str); 18] = [
    ("process.pid", "pid"),
    ("process.parent_pid", "ppid"),
    ("process.owner", "user"),
    ("process.command_line", "cmdline"),
    ("process.started_as", "started_as"),
    ("process.executable.path", "path"),
    ("process.exit.code", "exit_code"),
    ("process.signal", "signal"),
    ("process.core_dumped", "core_dumped"),
    ("process.unit", "unit"),
    ("process.forked", "forked"),
    ("process.threads", "threads"),
    ("process.cpu_seconds", "cpu_seconds"),
    ("process.cpu_pct", "cpu_pct"),
    ("process.peak_rss_bytes", "peak_rss_bytes"),
    ("process.io_read_bytes", "io_read_bytes"),
    ("process.io_write_bytes", "io_write_bytes"),
    ("process.accounting", "source"),
];

/// The span of a process, from its accounting record.
///
/// The record and the span say the same things of the same process, to
/// two readers: the record is found by what happened (everything that died
/// of SIGSEGV), and the span by where it happened (everything this build
/// ran, and in what order).
pub fn span_of(record: &Event, identity: &Identity) -> Span {
    let field = |key: &str| record.fields.get(key);
    let text = |key: &str| field(key).and_then(Value::as_str).unwrap_or("");
    let number = |key: &str| field(key).and_then(Value::as_f64).unwrap_or(0.0);

    let (ok, ending) = match text("status") {
        "unknown" | "" => (None, String::new()),
        "0" => (Some(true), String::new()),
        code if code.bytes().all(|b| b.is_ascii_digit()) => (Some(false), format!("exited {code}")),
        signal => (Some(false), format!("killed by {signal}")),
    };
    let mut attributes = Map::new();
    for (attribute, key) in SPAN_ATTRIBUTES {
        if let Some(value) = field(key).filter(|value| !value.is_null()) {
            attributes.insert(attribute.into(), value.clone());
        }
    }

    Span {
        trace_id: identity.trace_id,
        span_id: identity.span_id,
        parent_span_id: identity.parent_span_id,
        name: text("service").to_string(),
        service: match text("unit") {
            "" => "-".to_string(),
            unit => unit.to_string(),
        },
        ok,
        ending,
        start_ns: (number("started") * 1e9) as i64,
        duration_ns: (number("elapsed_seconds") * 1e9) as i64,
        attributes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::{Lineage, Seen};
    use crate::taskstats::record::AXSIG;

    fn identity() -> Identity {
        let mut lineage = Lineage::new("boot", 3600.0);
        lineage.register(
            Seen {
                pid: 4000,
                start_epoch: 990.0,
                pgid: Some(4000),
                parent: None,
            },
            0.0,
            &mut |_| None,
        );
        lineage.register(
            Seen {
                pid: 4242,
                start_epoch: 997.5,
                pgid: Some(4000),
                parent: Some(4000),
            },
            0.0,
            &mut |_| None,
        )
    }

    #[test]
    fn a_span_says_what_its_record_says() {
        let known = Known {
            cmdline: "cc1plus -quiet main.cpp",
            started_as: "",
            exe: "/usr/lib/gcc/cc1plus",
            unit: "mark/app-term.scope",
        };
        let record = exit_event(&exit(0x0100, 0), None, known, "mark");
        let identity = identity();
        let span = span_of(&record, &identity);

        assert_eq!(span.trace_id, identity.trace_id);
        assert_eq!(span.span_id, identity.span_id);
        assert!(span.parent_span_id.is_some());
        assert_eq!(span.name, "cc1plus");
        assert_eq!(span.service, "mark/app-term.scope");
        assert_eq!(span.status(), "error");
        assert_eq!(span.ending, "exited 1");
        assert_eq!(span.start_ns, 997_500_000_000);
        assert_eq!(span.duration_ns, 2_500_000_000);
        let a = &span.attributes;
        assert_eq!(a["process.pid"], 4242);
        assert_eq!(a["process.command_line"], "cc1plus -quiet main.cpp");
        assert_eq!(a["process.exit.code"], 1);
        assert_eq!(a["process.cpu_seconds"], 2.2);
        assert_eq!(a["process.accounting"], "taskstats");
        // What the record does not have, the span does not make up.
        assert!(!a.contains_key("process.signal"));
        assert!(!a.contains_key("process.forked"));
    }

    #[test]
    fn a_span_ends_as_its_process_did() {
        let of = |status, flag| {
            let record = exit_event(&exit(status, flag), None, Known::default(), "mark");
            let span = span_of(&record, &identity());
            (span.status(), span.ending, span.service)
        };
        assert_eq!(of(0, 0), ("ok", String::new(), "-".to_string()));
        assert_eq!(
            of(0x80 | 11, AXSIG | ACORE),
            ("error", "killed by SIGSEGV".to_string(), "-".to_string())
        );
    }

    fn exit(status: u32, flag: u8) -> ProcessExit {
        ProcessExit {
            pid: 4242,
            ppid: 4000,
            uid: 1000,
            gid: 1000,
            comm: "cc1plus".into(),
            exit_status: status,
            flag,
            start_epoch: 997.5,
            end_epoch: 1000.0,
            elapsed_seconds: 2.5,
            user_seconds: 1.9,
            system_seconds: 0.3,
            peak_rss_bytes: 500 * 1024 * 1024,
            read_bytes: 4096,
            threads: 1,
            ..ProcessExit::default()
        }
    }

    #[test]
    fn wait_statuses_decode_to_exit_codes_and_signals() {
        assert_eq!(Ending::from_status(0, 0), Ending::Exited(0));
        assert_eq!(Ending::from_status(0x0100, 0), Ending::Exited(1));
        assert_eq!(Ending::from_status(0xff00, 0), Ending::Exited(255));
        assert_eq!(
            Ending::from_status(9, AXSIG),
            Ending::Signaled {
                signal: 9,
                core: false
            }
        );
        assert_eq!(
            Ending::from_status(0x80 | 11, AXSIG | ACORE),
            Ending::Signaled {
                signal: 11,
                core: true
            }
        );
        // The last thread of a process that called exit(0): ended by the
        // kernel, on behalf of an exit that was not a signal's doing.
        assert_eq!(Ending::from_status(0, AXSIG), Ending::Exited(0));
        assert_eq!(Ending::from_status(0x0300, AXSIG), Ending::Exited(3));
    }

    #[test]
    fn the_level_judges_the_host_not_the_process() {
        let level = |status, flag| Ending::from_status(status, flag).level();
        assert_eq!(level(0, 0), Level::Info);
        assert_eq!(level(0x0100, 0), Level::Notice);
        assert_eq!(level(15, AXSIG), Level::Notice);
        assert_eq!(level(9, AXSIG), Level::Warning);
        assert_eq!(level(11, AXSIG), Level::Error);
        assert_eq!(level(6, AXSIG), Level::Error);
    }

    #[test]
    fn statuses_are_exit_codes_or_signal_names() {
        assert_eq!(Ending::from_status(0x0200, 0).status(), "2");
        assert_eq!(Ending::from_status(11, AXSIG).status(), "SIGSEGV");
        assert_eq!(signal_name(15), "SIGTERM");
        assert_eq!(signal_name(31), "SIGSYS");
        assert_eq!(signal_name(40), "SIG40");
    }

    #[test]
    fn durations_and_sizes_read_naturally() {
        assert_eq!(human_duration(0.34), "340ms");
        assert_eq!(human_duration(12.54), "12.5s");
        assert_eq!(human_duration(247.0), "4m07s");
        assert_eq!(human_duration(7380.0), "2h03m");
        assert_eq!(human_duration(90_000.0), "1d01h");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(12 * 1024), "12.0 KiB");
        assert_eq!(human_bytes(340 * 1024 * 1024), "340 MiB");
        assert_eq!(human_bytes(1536 * 1024 * 1024), "1.5 GiB");
    }

    #[test]
    fn an_exit_record_carries_the_indexed_keys_and_the_figures() {
        let event = exit_event(&exit(0x0100, 0), None, Known::default(), "mark");
        assert_eq!(event.level, Level::Notice);
        assert_eq!(event.ts_us, 1_000_000_000);
        assert_eq!(
            event.message,
            "cc1plus[4242] exited 1 after 2.5s, cpu 2.2s, peak rss 500 MiB"
        );
        let f = &event.fields;
        assert_eq!(f["service"], "cc1plus");
        assert_eq!(f["status"], "1");
        assert_eq!(f["exit_code"], 1);
        assert_eq!(f["user"], "mark");
        assert_eq!(f["source"], "taskstats");
        assert_eq!(f["elapsed_seconds"], 2.5);
        assert_eq!(f["cpu_seconds"], 2.2);
        assert_eq!(f["cpu_pct"], 88.0);
        assert_eq!(f["io_read_bytes"], 4096);
        assert!(!f.contains_key("signal"));
        // Nothing was learned of it, so there is nothing to report.
        assert!(!f.contains_key("cmdline"));
        assert!(!f.contains_key("path"));
        assert!(!f.contains_key("unit"));
    }

    #[test]
    fn what_is_known_of_a_process_goes_into_its_record() {
        let known = Known {
            cmdline: "cc1plus -quiet main.cpp",
            started_as: "",
            exe: "/usr/lib/gcc/cc1plus",
            unit: "mark/app-term.scope",
        };
        let f = exit_event(&exit(0, 0), None, known, "mark").fields;
        assert_eq!(f["cmdline"], "cc1plus -quiet main.cpp");
        assert_eq!(f["path"], "/usr/lib/gcc/cc1plus");
        assert_eq!(f["unit"], "mark/app-term.scope");
        assert!(!f.contains_key("forked"));

        let mut child = exit(0, 0);
        child.forked = true;
        assert_eq!(
            exit_event(&child, None, known, "mark").fields["forked"],
            true
        );
    }

    #[test]
    fn a_fault_is_an_error_with_the_signal_named() {
        let event = exit_event(
            &exit(0x80 | 11, AXSIG | ACORE),
            None,
            Known::default(),
            "mark",
        );
        assert_eq!(event.level, Level::Error);
        assert_eq!(event.fields["status"], "SIGSEGV");
        assert_eq!(event.fields["signal"], "SIGSEGV");
        assert_eq!(event.fields["core_dumped"], true);
        assert!(!event.fields.contains_key("exit_code"));
        assert!(event.message.contains("killed by SIGSEGV (core dumped)"));
    }

    #[test]
    fn a_process_that_used_no_time_has_no_cpu_percentage() {
        let mut instant = exit(0, 0);
        instant.elapsed_seconds = 0.0;
        assert_eq!(
            exit_event(&instant, None, Known::default(), "mark").fields["cpu_pct"],
            Value::Null
        );
    }
}
