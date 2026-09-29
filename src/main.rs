//! timeless-acct: system and process accounting history in timeless-libsql.

mod accounting;
mod cgroup;
mod check;
mod cli;
// Its times of day are for reading a local store; its lengths of time are
// for everyone.
#[cfg_attr(not(feature = "embedded"), allow(dead_code))]
mod clock;
mod collect;
mod encode;
mod engine;
mod lineage;
mod model;
mod netlink;
mod procevents;
mod procfs;
#[cfg(feature = "embedded")]
mod query;
mod sink;
mod taskstats;
#[cfg(test)]
mod testutil;
#[cfg(feature = "watch")]
mod watch;

use std::io::ErrorKind;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;

use cli::{Cli, CollectArgs, Command, OnceArgs, RunArgs, SinkKind};
use collect::process::{ProcessCollector, ProcessOptions, Units};
use collect::system::{SystemCollector, SystemOptions};
use collect::units::UnitCollector;
use engine::{Clock, Engine, Parts, Schedule};
use lineage::Lineage;
use model::MetricBatch;
use procevents::ExecListener;
use procfs::system::parse_stat;
use procfs::ProcRoot;
use sink::stdout::StdoutSink;
use sink::Sink;
use taskstats::{epoch_now, Listener};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(args) => run(args),
        Command::Once(args) => once(args),
        Command::Check(args) => {
            check::run(&args);
            Ok(())
        }
        #[cfg(feature = "embedded")]
        Command::Top(args) => query::top(&args),
        #[cfg(feature = "embedded")]
        Command::Exits(args) => query::exits(&args),
        #[cfg(feature = "embedded")]
        Command::Trees(args) => query::trees(&args),
        #[cfg(feature = "watch")]
        Command::Watch(args) => watch::watch(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("timeless-acct: {error:#}");
            ExitCode::FAILURE
        }
    }
}

struct Collectors {
    host: String,
    root: ProcRoot,
    system: SystemCollector,
    processes: ProcessCollector,
    /// Absent on a host without the unified control group hierarchy.
    units: Option<UnitCollector>,
    /// What the exec listener needs to describe a process as a sweep would.
    describe: (Units, f64, usize),
}

fn collectors(args: &CollectArgs) -> Result<Collectors> {
    let root = ProcRoot::new(&args.proc_root, &args.sys_root);
    let stat = root
        .read("stat")
        .with_context(|| format!("read {}", root.proc_path("stat").display()))?;
    let boot_time = parse_stat(&stat).boot_time;
    if boot_time == 0 {
        bail!(
            "{} has no boot time: is it a proc filesystem?",
            root.proc_path("stat").display()
        );
    }
    let host = match &args.host {
        Some(host) if !host.is_empty() => host.clone(),
        _ => check::hostname(&root),
    };
    if args.min_age.is_nan() || args.min_age < 0.0 {
        bail!("--min-age must not be negative");
    }

    let system = SystemCollector::new(
        root.clone(),
        SystemOptions {
            per_cpu: args.per_cpu,
            net_exclude: args.net_exclude.clone(),
        },
    );
    let options = ProcessOptions {
        min_age: args.min_age,
        kernel_threads: args.kernel_threads,
        delay_accounting: check::delay_accounting(&root),
        ..ProcessOptions::default()
    };
    let units = Units::from_system();
    let describe = (units, boot_time as f64, options.cmdline_max);
    let processes = ProcessCollector::new(root.clone(), options, units, boot_time as f64);
    Ok(Collectors {
        host,
        units: UnitCollector::new(root.clone()),
        root,
        system,
        processes,
        describe,
    })
}

/// Start a listener that needs `CAP_NET_ADMIN`, or say why there is none.
fn listen<T>(what: &str, without: &str, started: std::io::Result<T>) -> Option<T> {
    match started {
        Ok(listener) => Some(listener),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "timeless-acct: the kernel refused {what} (it needs CAP_NET_ADMIN).                  {without}; `timeless-acct check` says what to do about it."
            );
            None
        }
        Err(error) => {
            eprintln!("timeless-acct: {what} is unavailable: {error}");
            None
        }
    }
}

fn open_sink(args: &RunArgs) -> Result<Box<dyn Sink>> {
    Ok(match args.sink {
        #[cfg(feature = "embedded")]
        SinkKind::Embedded => Box::new(sink::embedded::EmbeddedSink::open(
            &sink::embedded::EmbeddedOptions {
                dir: args.data_dir.clone(),
                retention: args.retention.clone(),
                rollups: args.rollups.clone(),
                log_retention: args.log_retention.clone(),
                trace_retention: args.trace_retention.clone(),
            },
        )?),
        #[cfg(feature = "http")]
        SinkKind::Http => Box::new(sink::http::HttpSink::new(sink::http::HttpOptions {
            metrics_url: args.metrics_url.clone(),
            logs_url: args.logs_url.clone(),
            traces_url: args.traces_url.clone(),
            token: args.token.clone().filter(|token| !token.is_empty()),
            ..sink::http::HttpOptions::default()
        })),
        SinkKind::Stdout => Box::new(StdoutSink),
    })
}

fn run(args: RunArgs) -> Result<()> {
    if args.interval == 0 || args.process_interval == 0 || args.flush_interval == 0 {
        bail!("intervals are whole seconds, and at least one");
    }
    if args.no_system && args.no_processes && args.no_exit_accounting && args.no_units {
        bail!("nothing to collect: everything is turned off");
    }
    let Collectors {
        host,
        root,
        system,
        processes,
        units,
        describe: (ticks, boot_epoch, cmdline_max),
    } = collectors(&args.collect)?;
    let sink = open_sink(&args)?;

    let listener = (!args.no_exit_accounting)
        .then(|| {
            listen(
                "exit accounting",
                "Processes that end will be noticed by their absence",
                Listener::start(&check::possible_cpus(&root)),
            )
        })
        .flatten();
    // An exec is described for the sake of an exit record; without those
    // there is nothing to put the description in.
    let execs = (listener.is_some() && !args.no_exec_events)
        .then(|| {
            listen(
                "exec events",
                "Short-lived processes will be recorded by name alone",
                ExecListener::start(root.clone(), ticks, boot_epoch, cmdline_max),
            )
        })
        .flatten();
    let units = units.filter(|_| !args.no_units);

    let max_age = clock::parse_span(&args.trace_max_age).context("--trace-max-age")?;
    // A span is made of an exit record, so without those there are none.
    let lineage = (listener.is_some() && !args.no_traces).then(|| {
        // The kernel's name for this boot: a pid means a different process
        // after every one. And the host's, since the planes hold many
        // hosts' spans together, and two hosts made from one image may
        // tell the same story of their boot.
        let boot_id = root.read("sys/kernel/random/boot_id").unwrap_or_default();
        Lineage::new(&format!("{host} {}", boot_id.trim()), max_age)
    });

    #[cfg(feature = "embedded")]
    let maintain = (args.sink == SinkKind::Embedded)
        .then(|| Duration::from_secs(args.maintain_interval.max(60)));
    #[cfg(not(feature = "embedded"))]
    let maintain = None;

    // Exit records are accounted at process sweeps; with sweeps turned off,
    // they still need a tick to be drained on.
    let process_tick = (!args.no_processes || listener.is_some() || units.is_some())
        .then(|| Duration::from_secs(args.process_interval));
    let schedule = Schedule {
        system: (!args.no_system).then(|| Duration::from_secs(args.interval)),
        processes: process_tick,
        flush: Duration::from_secs(args.flush_interval),
        maintain,
    };

    eprintln!(
        "timeless-acct {}: host {host}, {}, system every {}, processes every {}, units {}, \
         exit accounting {}, command lines {}, traces {}",
        env!("CARGO_PKG_VERSION"),
        sink.describe(),
        schedule
            .system
            .map_or("never".into(), |i| format!("{}s", i.as_secs())),
        if args.no_processes {
            "never".into()
        } else {
            format!("{}s", args.process_interval)
        },
        if units.is_some() { "on" } else { "off" },
        if listener.is_some() {
            "from the kernel"
        } else {
            "by absence"
        },
        if execs.is_some() {
            "at exec"
        } else {
            "at the sweep"
        },
        if lineage.is_some() { "on" } else { "off" },
    );

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop)).context("install signal handler")?;
    }

    let mut engine = Engine::new(Parts {
        host,
        root,
        system: (!args.no_system).then_some(system),
        processes: (!args.no_processes).then_some(processes),
        units,
        exits: listener,
        execs,
        lineage,
        clock: Clock {
            units: ticks,
            boot_epoch,
        },
        sink,
        schedule,
    });
    engine.run(&stop)?;
    eprintln!("timeless-acct: stopped");
    Ok(())
}

fn once(args: OnceArgs) -> Result<()> {
    if args.wait.is_nan() || args.wait <= 0.0 || args.wait > 3600.0 {
        bail!("--wait is a number of seconds, more than none and at most an hour");
    }
    let Collectors {
        host,
        mut system,
        mut processes,
        mut units,
        ..
    } = collectors(&args.collect)?;

    let mut discard = MetricBatch::new(0);
    let first = Instant::now();
    system.collect(first, &mut discard);
    let sweep = processes.sweep(first, epoch_now(), &[], &mut discard);
    if let Some(units) = &mut units {
        processes.set_containers(units.containers());
        units.collect(first, &sweep.cgroups, &mut discard);
    }

    thread::sleep(Duration::from_secs_f64(args.wait));

    let wall = epoch_now();
    let mut batch = MetricBatch::new(wall as i64);
    let second = Instant::now();
    system.collect(second, &mut batch);
    let sweep = processes.sweep(second, wall, &[], &mut batch);
    if let Some(units) = &mut units {
        units.collect(second, &sweep.cgroups, &mut batch);
    }
    StdoutSink.write(
        &host,
        &sink::Tick {
            metrics: &batch,
            events: &[],
            spans: &[],
        },
    )
}
