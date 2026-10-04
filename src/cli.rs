//! The command line.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "timeless-acct",
    version,
    about = "Linux system and process accounting history in timeless-libsql",
    long_about = "Records what sar records, a set of series for every long-lived process, \
                  and an accounting record for every process that ends, into timeless-libsql: \
                  either a local store, or the Timeless planes that the canvas reads."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Collect until stopped
    Run(Box<RunArgs>),
    /// Take two readings and print what would be stored
    Once(OnceArgs),
    /// Report what this host, and this user, let the collector see
    Check(CheckArgs),
    /// The processes of a past moment, from a local store
    #[cfg(feature = "embedded")]
    Top(TopArgs),
    /// Accounting records of processes that ended, from a local store
    #[cfg(feature = "embedded")]
    Exits(ExitsArgs),
    /// Jobs, each as the tree of processes it ran, from a local store
    #[cfg(feature = "embedded")]
    Trees(TreesArgs),
    /// Watch the host in a terminal: now, and at any moment a local store
    /// holds
    #[cfg(feature = "watch")]
    Watch(WatchArgs),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SinkKind {
    /// A local store, owned by this process
    #[cfg(feature = "embedded")]
    Embedded,
    /// The Timeless planes, which the canvas reads
    #[cfg(feature = "http")]
    Http,
    /// Print instead of storing
    Stdout,
}

#[derive(Args, Clone)]
pub struct CollectArgs {
    /// The name this host is recorded under [default: its hostname]
    #[arg(long, env = "TIMELESS_ACCT_HOST")]
    pub host: Option<String>,

    /// Report every CPU, not only their total
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, value_name = "BOOL")]
    pub per_cpu: bool,

    /// Do not report interfaces whose name starts with this; repeatable
    #[arg(long, value_name = "PREFIX", default_values_t = ["veth".to_string()])]
    pub net_exclude: Vec<String>,

    /// Seconds a process must have lived before it gets series of its own.
    /// Younger ones are counted under their command name and their user
    #[arg(long, default_value_t = 30.0, value_name = "SECONDS")]
    pub min_age: f64,

    /// Give kernel threads series of their own
    #[arg(long)]
    pub kernel_threads: bool,

    /// Where the kernel's process information is mounted
    #[arg(long, default_value = "/proc", hide = true)]
    pub proc_root: PathBuf,

    /// Where the kernel's device information is mounted
    #[arg(long, default_value = "/sys", hide = true)]
    pub sys_root: PathBuf,
}

#[derive(Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub collect: CollectArgs,

    /// Where to store
    #[cfg_attr(
        feature = "embedded",
        arg(long, value_enum, default_value = "embedded")
    )]
    #[cfg_attr(
        not(feature = "embedded"),
        arg(long, value_enum, default_value = "stdout")
    )]
    pub sink: SinkKind,

    /// Seconds between readings of the system
    #[arg(long, default_value_t = 10, value_name = "SECONDS")]
    pub interval: u64,

    /// Seconds between sweeps of the processes
    #[arg(long, default_value_t = 10, value_name = "SECONDS")]
    pub process_interval: u64,

    /// Do not read system-wide statistics
    #[arg(long)]
    pub no_system: bool,

    /// Do not sweep processes
    #[arg(long)]
    pub no_processes: bool,

    /// Do not ask the kernel for exit records. Processes that end are then
    /// noticed by their absence, and those that start and end between two
    /// sweeps are not seen at all
    #[arg(long)]
    pub no_exit_accounting: bool,

    /// Do not ask the kernel for word of each exec. The record of a process
    /// that ends before a sweep has seen it then has its command's name,
    /// and not its arguments, its executable, or its unit
    #[arg(long)]
    pub no_exec_events: bool,

    /// Do not report units: services, scopes, and slices
    #[arg(long)]
    pub no_units: bool,

    /// Do not keep a span for each process that ends
    #[arg(long)]
    pub no_traces: bool,

    /// How long after a job starts a process may start and still be part
    /// of its trace. What a process group older than this starts, as a
    /// daemon's is, is a trace of its own
    #[arg(long, default_value = "1h", value_name = "SPAN")]
    pub trace_max_age: String,

    /// Local store: its directory. Unless told, the user's own
    /// (~/.local/share/timeless-acct), or for root /var/lib/timeless-acct
    #[cfg(feature = "embedded")]
    #[arg(long, env = "TIMELESS_ACCT_DATA", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Local store: how long samples are kept. Applies when the store is created
    #[cfg(feature = "embedded")]
    #[arg(long, default_value = "7d", value_name = "SPAN")]
    pub retention: String,

    /// Local store: coarser copies kept after samples age out, as
    /// RESOLUTION@RETENTION. Applies when the store is created
    #[cfg(feature = "embedded")]
    #[arg(long, default_value = "5m@30d,1h@180d", value_name = "LADDER")]
    pub rollups: String,

    /// Local store: how long accounting records are kept. Applies when the
    /// store is created
    #[cfg(feature = "embedded")]
    #[arg(long, default_value = "30d", value_name = "SPAN")]
    pub log_retention: String,

    /// Local store: how long spans are kept. Applies when the store is
    /// created
    #[cfg(feature = "embedded")]
    #[arg(long, default_value = "30d", value_name = "SPAN")]
    pub trace_retention: String,

    /// Local store: what it may hold on disk, as 2G or 500M. Over it, the
    /// oldest samples go first, then spans, then records; 0 for no limit
    #[cfg(feature = "embedded")]
    #[arg(long, default_value = "2G", value_name = "SIZE")]
    pub store_limit: String,

    /// Seconds between flushes. What is written since the last one is lost
    /// if the collector is killed
    #[arg(long, default_value_t = 60, value_name = "SECONDS")]
    pub flush_interval: u64,

    /// Local store: seconds between compactions. Each one turns what a
    /// series has gathered since the last into one chunk, and a chunk costs
    /// about 155 bytes however few samples it holds, so compacting more
    /// often than this stores the same samples in more bytes
    #[cfg(feature = "embedded")]
    #[arg(long, default_value_t = 3600, value_name = "SECONDS")]
    pub maintain_interval: u64,

    /// Planes: the metrics plane
    #[cfg(feature = "http")]
    #[arg(
        long,
        env = "TIMELESS_ACCT_METRICS_URL",
        default_value = "http://127.0.0.1:8428",
        value_name = "URL"
    )]
    pub metrics_url: String,

    /// Planes: the logs plane
    #[cfg(feature = "http")]
    #[arg(
        long,
        env = "TIMELESS_ACCT_LOGS_URL",
        default_value = "http://127.0.0.1:9428",
        value_name = "URL"
    )]
    pub logs_url: String,

    /// Planes: the traces plane
    #[cfg(feature = "http")]
    #[arg(
        long,
        env = "TIMELESS_ACCT_TRACES_URL",
        default_value = "http://127.0.0.1:10428",
        value_name = "URL"
    )]
    pub traces_url: String,

    /// Planes: a bearer token, if they require one
    #[cfg(feature = "http")]
    #[arg(
        long,
        env = "TIMELESS_ACCT_TOKEN",
        hide_env_values = true,
        value_name = "TOKEN"
    )]
    pub token: Option<String>,
}

#[derive(Args)]
pub struct OnceArgs {
    #[command(flatten)]
    pub collect: CollectArgs,

    /// Seconds between the two readings
    #[arg(long, default_value_t = 1.0, value_name = "SECONDS")]
    pub wait: f64,
}

#[derive(Args)]
pub struct CheckArgs {
    /// The metrics plane to look for
    #[cfg(feature = "http")]
    #[arg(
        long,
        env = "TIMELESS_ACCT_METRICS_URL",
        default_value = "http://127.0.0.1:8428",
        value_name = "URL"
    )]
    pub metrics_url: String,

    /// The logs plane to look for
    #[cfg(feature = "http")]
    #[arg(
        long,
        env = "TIMELESS_ACCT_LOGS_URL",
        default_value = "http://127.0.0.1:9428",
        value_name = "URL"
    )]
    pub logs_url: String,

    /// The traces plane to look for
    #[cfg(feature = "http")]
    #[arg(
        long,
        env = "TIMELESS_ACCT_TRACES_URL",
        default_value = "http://127.0.0.1:10428",
        value_name = "URL"
    )]
    pub traces_url: String,
}

#[cfg(feature = "embedded")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TopSort {
    Cpu,
    Rss,
    Io,
}

#[cfg(feature = "embedded")]
#[derive(Args)]
pub struct TopArgs {
    /// The store's directory. Unless told, the user's own if there is
    /// one, or the host's (/var/lib/timeless-acct)
    #[arg(long, env = "TIMELESS_ACCT_DATA", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// The moment to look at: now, -15m, 14:30, "2026-09-29 14:30"
    #[arg(
        long,
        default_value = "now",
        allow_hyphen_values = true,
        value_name = "TIME"
    )]
    pub at: String,

    /// How far before that moment a process's last sample may be
    #[arg(long, default_value = "60s", value_name = "SPAN")]
    pub within: String,

    /// What to order by
    #[arg(long, value_enum, default_value = "cpu")]
    pub sort: TopSort,

    /// How many processes to show
    #[arg(short = 'n', long, default_value_t = 20)]
    pub count: usize,

    /// Only this host
    #[arg(long)]
    pub host: Option<String>,
}

#[cfg(feature = "embedded")]
#[derive(Args)]
pub struct ExitsArgs {
    /// The store's directory. Unless told, the user's own if there is
    /// one, or the host's (/var/lib/timeless-acct)
    #[arg(long, env = "TIMELESS_ACCT_DATA", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// The earliest moment
    #[arg(
        long,
        default_value = "-1h",
        allow_hyphen_values = true,
        value_name = "TIME"
    )]
    pub since: String,

    /// The latest moment
    #[arg(
        long,
        default_value = "now",
        allow_hyphen_values = true,
        value_name = "TIME"
    )]
    pub until: String,

    /// Only this command name
    #[arg(long, value_name = "NAME")]
    pub comm: Option<String>,

    /// Only this ending: an exit code, or a signal name such as SIGSEGV
    #[arg(long)]
    pub status: Option<String>,

    /// Only this host
    #[arg(long)]
    pub host: Option<String>,

    /// Only this user
    #[arg(long)]
    pub user: Option<String>,

    /// Only this unit
    #[arg(long)]
    pub unit: Option<String>,

    /// Total instead of listing, as sa(8) does
    #[arg(long)]
    pub summary: bool,

    /// What to total by
    #[arg(long, value_enum, default_value = "comm", requires = "summary")]
    pub by: SummaryBy,

    /// How many to show, most recent first; or how many totals
    #[arg(short = 'n', long, default_value_t = 50)]
    pub count: usize,
}

#[cfg(feature = "embedded")]
#[derive(Args)]
pub struct TreesArgs {
    /// The store's directory. Unless told, the user's own if there is
    /// one, or the host's (/var/lib/timeless-acct)
    #[arg(long, env = "TIMELESS_ACCT_DATA", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// The earliest moment a process of the job started
    #[arg(
        long,
        default_value = "-1h",
        allow_hyphen_values = true,
        value_name = "TIME"
    )]
    pub since: String,

    /// The latest
    #[arg(
        long,
        default_value = "now",
        allow_hyphen_values = true,
        value_name = "TIME"
    )]
    pub until: String,

    /// Only jobs that ran this command, anywhere in them
    #[arg(long, value_name = "NAME")]
    pub comm: Option<String>,

    /// Only jobs with a process in this unit
    #[arg(long)]
    pub unit: Option<String>,

    /// The host the unit is on [default: this one]
    #[arg(long, requires = "unit")]
    pub host: Option<String>,

    /// Only jobs in which something failed
    #[arg(long)]
    pub failed: bool,

    /// Only jobs of at least this many processes
    #[arg(long, default_value_t = 2, value_name = "COUNT")]
    pub min_processes: usize,

    /// How many jobs to show, most recent first
    #[arg(short = 'n', long, default_value_t = 5)]
    pub count: usize,

    /// How many processes of a job to show
    #[arg(long, default_value_t = 60, value_name = "COUNT")]
    pub max_processes: usize,

    /// How much of a command line to show; 0 for all of it
    #[arg(long, default_value_t = 72, value_name = "CHARACTERS")]
    pub width: usize,
}

#[cfg(feature = "watch")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum WatchView {
    Units,
    Processes,
    Jobs,
    Exits,
}

#[cfg(feature = "watch")]
#[derive(Args)]
pub struct WatchArgs {
    /// The store's directory. Unless told, the user's own if there is
    /// one, or the host's (/var/lib/timeless-acct)
    #[arg(long, env = "TIMELESS_ACCT_DATA", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// The moment to start at: now, -15m, 14:30, "2026-09-29 14:30"
    #[arg(
        long,
        default_value = "now",
        allow_hyphen_values = true,
        value_name = "TIME"
    )]
    pub at: String,

    /// The view to start in
    #[arg(long, value_enum, default_value = "units")]
    pub view: WatchView,

    /// Seconds between readings of now
    #[arg(long, default_value_t = 2, value_name = "SECONDS")]
    pub refresh: u64,

    /// Only this host, of a store that holds more than one
    #[arg(long)]
    pub host: Option<String>,

    /// Draw the screen once, as text of this size, and leave
    #[arg(long, value_name = "WIDTHxHEIGHT", num_args = 0..=1, default_missing_value = "120x40")]
    pub print: Option<String>,
}

#[cfg(feature = "embedded")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SummaryBy {
    /// The command's name
    Comm,
    /// The unit it ran in
    Unit,
    User,
}

/// A size on disk, as it is written on the command line: `2G`, `500M`,
/// `1.5G`, in binary units; `0`, `none`, or `off` for none at all.
pub fn parse_size(text: &str) -> anyhow::Result<Option<u64>> {
    let text = text.trim();
    if matches!(text, "0" | "none" | "off") {
        return Ok(None);
    }
    let (number, unit) = match text.char_indices().last() {
        Some((index, unit)) if unit.is_ascii_alphabetic() => (&text[..index], unit),
        _ => (text, 'B'),
    };
    let scale: f64 = match unit.to_ascii_uppercase() {
        'B' => 1.0,
        'K' => 1024.0,
        'M' => 1024.0 * 1024.0,
        'G' => 1024.0 * 1024.0 * 1024.0,
        'T' => 1024.0f64.powi(4),
        _ => anyhow::bail!("{text:?} is not a size: expected 2G, 500M, or 0 for no limit"),
    };
    let number: f64 = number.trim().parse().map_err(|_| {
        anyhow::anyhow!("{text:?} is not a size: expected 2G, 500M, or 0 for no limit")
    })?;
    if number.is_nan() || number <= 0.0 || !number.is_finite() {
        anyhow::bail!("{text:?} is not a size: expected 2G, 500M, or 0 for no limit");
    }
    let bytes = number * scale;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        anyhow::bail!("{text:?} is not a size: out of range");
    }
    Ok(Some(bytes as u64))
}

#[cfg(test)]
mod size_tests {
    use super::parse_size;

    #[test]
    fn a_size_is_read_in_binary_units_or_is_no_limit() {
        assert_eq!(parse_size("2G").unwrap(), Some(2 << 30));
        assert_eq!(parse_size("500m").unwrap(), Some(500 << 20));
        assert_eq!(parse_size("1.5G").unwrap(), Some(3 << 29));
        assert_eq!(parse_size("4096").unwrap(), Some(4096));
        assert_eq!(parse_size("0").unwrap(), None);
        assert_eq!(parse_size("none").unwrap(), None);
        for bad in ["", "G", "-1G", "2X", "two"] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }
}
