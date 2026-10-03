//! What this host, and this user, let the collector see.
//!
//! Everything here degrades quietly when it is unavailable: a collector
//! without privileges still collects. This report is where the difference
//! is said out loud.

use std::fs;

use crate::cli::CheckArgs;
use crate::collect::process::Units;
use crate::collect::units::UnitCollector;
use crate::procevents::ExecListener;
use crate::procfs::system::{cpu_list_len, parse_stat};
use crate::procfs::ProcRoot;
use crate::taskstats::Listener;

pub fn possible_cpus(root: &ProcRoot) -> String {
    root.read_sys("devices/system/cpu/possible")
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "0".into())
}

pub fn delay_accounting(root: &ProcRoot) -> bool {
    root.read("sys/kernel/task_delayacct")
        .is_ok_and(|text| text.trim() == "1")
}

pub fn hostname(root: &ProcRoot) -> String {
    root.read("sys/kernel/hostname")
        .map(|text| text.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

fn line(what: &str, state: &str) {
    println!("{what:<28} {state}");
}

pub fn run(args: &CheckArgs) {
    let root = ProcRoot::default();
    let units = Units::from_system();
    let cpus = possible_cpus(&root);

    line("host", &hostname(&root));
    line(
        "kernel",
        root.read("sys/kernel/osrelease").unwrap_or_default().trim(),
    );
    line("cpus", &format!("{} ({cpus})", cpu_list_len(&cpus)));
    line("clock ticks", &format!("{}/s", units.ticks_per_second));
    println!();

    match Listener::start(&cpus) {
        Ok(_) => line("exit accounting", "available"),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            line("exit accounting", "refused: needs CAP_NET_ADMIN");
            println!(
                "{:<28} processes that end will be noticed by their absence, and those",
                ""
            );
            println!(
                "{:<28} shorter than a sweep not at all. Run as root, or grant it:",
                ""
            );
            println!(
                "{:<28}   sudo setcap cap_net_admin,cap_sys_ptrace,cap_dac_read_search+ep {}",
                "",
                std::env::current_exe()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| "timeless-acct".into())
            );
        }
        Err(error) => line("exit accounting", &format!("unavailable: {error}")),
    }

    let boot = parse_stat(&root.read("stat").unwrap_or_default()).boot_time as f64;
    match ExecListener::start(root.clone(), units, boot, 1024) {
        Ok(_) => line("exec events", "available"),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            line("exec events", "refused: needs CAP_NET_ADMIN");
            println!(
                "{:<28} the record of a process that ends before a sweep has seen it",
                ""
            );
            println!(
                "{:<28} will have its command's name, and not its arguments.",
                ""
            );
        }
        Err(error) => line("exec events", &format!("unavailable: {error}")),
    }

    line(
        "units",
        if UnitCollector::new(root.clone()).is_some() {
            "available"
        } else {
            "unavailable: no unified control group hierarchy at /sys/fs/cgroup"
        },
    );

    if delay_accounting(&root) {
        line("delay accounting", "on");
    } else {
        line("delay accounting", "off");
        println!(
            "{:<28} proc_io_wait_pct is not reported and the delay_* fields of",
            ""
        );
        println!("{:<28} accounting records are zero. To turn it on:", "");
        println!("{:<28}   sudo sysctl kernel.task_delayacct=1", "");
    }

    line(
        "pressure stall information",
        if root.proc_path("pressure/cpu").exists() {
            "available"
        } else {
            "unavailable (CONFIG_PSI, or psi=1 on the kernel command line)"
        },
    );

    let pids = root.pids().unwrap_or_default();
    let readable = |file: &str| {
        pids.iter()
            .filter(|pid| fs::read(root.pid_path(**pid, file)).is_ok())
            .count()
    };
    let io = readable("io");
    line(
        "process I/O",
        &format!("visible for {io} of {} processes", pids.len()),
    );
    if io < pids.len() {
        println!(
            "{:<28} the rest belong to other users: their I/O and open files are",
            ""
        );
        println!(
            "{:<28} not reported. Run as root, or grant both CAP_SYS_PTRACE and",
            ""
        );
        println!(
            "{:<28} CAP_DAC_READ_SEARCH: the files are private to their owner, and",
            ""
        );
        println!(
            "{:<28} reading them is then checked as tracing the process would be.",
            ""
        );
    }
    line(
        "scheduler wait",
        &format!(
            "visible for {} of {} processes",
            readable("schedstat"),
            pids.len()
        ),
    );

    #[cfg(feature = "http")]
    {
        println!();
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(2))
            .build();
        plane(&agent, "metrics plane", &args.metrics_url);
        plane(&agent, "logs plane", &args.logs_url);
        plane(&agent, "traces plane", &args.traces_url);
    }
    #[cfg(not(feature = "http"))]
    let _ = args;
}

#[cfg(feature = "http")]
fn plane(agent: &ureq::Agent, what: &str, url: &str) {
    let base = url.trim_end_matches('/');
    let state = match agent.get(&format!("{base}/live")).call() {
        Ok(_) => {
            let version = agent
                .get(&format!("{base}/metrics"))
                .call()
                .ok()
                .and_then(|response| response.into_string().ok())
                .and_then(|text| build_version(&text));
            match version {
                Some(version) => format!("{base}  answering, version {version}"),
                None => format!("{base}  answering"),
            }
        }
        Err(error) => format!("{base}  not answering: {error}"),
    };
    line(what, &state);
}

/// The version out of a plane's `timeless_build_info` line.
#[cfg(feature = "http")]
fn build_version(metrics: &str) -> Option<String> {
    let line = metrics
        .lines()
        .find(|line| line.starts_with("timeless_build_info{"))?;
    let rest = &line[line.find("version=\"")? + "version=\"".len()..];
    Some(rest[..rest.find('"')?].to_string())
}

#[cfg(all(test, feature = "http"))]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_read_from_the_build_info_line() {
        let metrics = "# HELP timeless_build_info Build identity\n\
                       timeless_build_info{name=\"timeless-metrics-api\",version=\"0.8.5\",commit=\"e8a\"} 1\n";
        assert_eq!(build_version(metrics).as_deref(), Some("0.8.5"));
        assert_eq!(build_version("up 1\n"), None);
    }
}
