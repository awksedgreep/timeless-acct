//! Jobs that have not ended.
//!
//! A span is written when its process ends, so the store knows a job when
//! it is over. What is running is known to the kernel, and is read from
//! the processes that are there: the same jobs, by the same rule, as the
//! spans will say they were.

use std::collections::HashMap;

use serde_json::json;

use crate::collect::process::Tracked;
use crate::query::{tree, Node};

use super::store::Job;

/// The jobs among the processes that are running.
///
/// A job is a process group of more than one process, that began within
/// `max_age` seconds: a group older than that is a daemon's, and what it
/// starts are jobs of their own, as they are in the store.
pub fn running<'a>(
    processes: impl Iterator<Item = &'a Tracked>,
    now: f64,
    max_age: f64,
    width: usize,
) -> Vec<Job> {
    let mut groups: HashMap<u32, Vec<&Tracked>> = HashMap::new();
    for process in processes.filter(|process| !process.kernel_thread) {
        groups.entry(process.pgid).or_default().push(process);
    }

    let mut jobs = Vec::new();
    for (group, mut members) in groups {
        // A group began when its leader did, if its leader is still
        // there; and when the first of what is left of it did, if not.
        let began = members
            .iter()
            .find(|process| process.pid == group)
            .map_or_else(
                || {
                    members
                        .iter()
                        .map(|process| process.start_epoch)
                        .fold(f64::INFINITY, f64::min)
                },
                |leader| leader.start_epoch,
            );
        if now - began > max_age || members.len() < 2 {
            continue;
        }
        members.sort_by(|a, b| {
            a.start_epoch
                .total_cmp(&b.start_epoch)
                .then(a.pid.cmp(&b.pid))
        });

        let nodes: Vec<Node> = members
            .iter()
            .map(|process| Node {
                id: process.pid.to_be_bytes().to_vec(),
                parent: Some(process.parent.to_be_bytes().to_vec()),
                name: process.comm.clone(),
                unit: process.unit.clone(),
                failed: false,
                ending: String::new(),
                start_ns: (process.start_epoch * 1e9) as i64,
                duration_ns: ((now - process.start_epoch).max(0.0) * 1e9) as i64,
                attributes: json!({
                    "process.command_line": process.cmdline,
                    "process.started_as": process.started_as,
                    "process.cpu_seconds": process.cpu_seconds(),
                    "process.peak_rss_bytes": process.peak_rss_bytes,
                }),
            })
            .collect();
        jobs.push(Job {
            started: began,
            duration: (now - began).max(0.0),
            cpu: members.iter().map(|process| process.cpu_seconds()).sum(),
            processes: members.len(),
            failed: 0,
            unit: members[0].unit.clone(),
            command: nodes[0].command(width),
            tree: tree(&nodes, 200, width),
            running: true,
        });
    }
    jobs.sort_by(|a, b| b.started.total_cmp(&a.started));
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::process::{ProcessCollector, ProcessOptions, Units};
    use crate::model::MetricBatch;
    use crate::testutil::Fixture;
    use std::time::Instant;

    const BOOT: f64 = 1_000_000.0;

    /// A process that started `age` seconds before the sweep.
    fn process(fixture: &Fixture, pid: u32, ppid: u32, pgid: u32, comm: &str, age: u64) {
        let start = (1000 - age) * 100;
        fixture.proc_file(
            &format!("{pid}/stat"),
            &format!(
                "{pid} ({comm}) S {ppid} {pgid} 1 0 -1 0 0 0 0 0 150 50 0 0 20 0 1 0 {start} 1000 10 0\n"
            ),
        );
        fixture.proc_file(
            &format!("{pid}/status"),
            "Uid:\t0\t0\t0\t0\nVmRSS:\t2048 kB\n",
        );
        fixture.proc_file(&format!("{pid}/cmdline"), &format!("{comm}\0--flag\0"));
        fixture.proc_file(&format!("{pid}/cgroup"), "0::/system.slice/build.service\n");
    }

    fn jobs(fixture: &Fixture) -> Vec<Job> {
        let units = Units {
            ticks_per_second: 100.0,
            page_bytes: 4096,
        };
        let mut collector =
            ProcessCollector::new(fixture.root(), ProcessOptions::default(), units, BOOT);
        let now = BOOT + 1000.0;
        collector.sweep(Instant::now(), now, &[], &mut MetricBatch::new(0));
        running(collector.all(), now, 3600.0, 72)
    }

    #[test]
    fn a_build_that_is_running_is_a_job_already() {
        let fixture = Fixture::new("running_build");
        process(&fixture, 10, 1, 10, "bash", 900);
        process(&fixture, 20, 10, 20, "make", 60);
        process(&fixture, 21, 20, 20, "cc", 5);
        process(&fixture, 22, 21, 20, "as", 1);
        let jobs = jobs(&fixture);

        // The shell it was typed into is a group of one, and not a job.
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert!(job.running);
        assert_eq!(job.command, "make --flag");
        assert_eq!(job.processes, 3);
        assert_eq!(job.started, BOOT + 940.0);
        assert_eq!(job.duration, 60.0);
        // Two seconds each, so far.
        assert_eq!(job.cpu, 6.0);
        assert_eq!(job.unit, "build.service");
        assert_eq!(
            job.tree,
            [
                "make --flag  1m00s, cpu 2.0s, 2.0 MiB",
                "└─ cc --flag  5.0s, cpu 2.0s, 2.0 MiB",
                "   └─ as --flag  1.0s, cpu 2.0s, 2.0 MiB",
            ]
        );
    }

    #[test]
    fn what_a_daemon_is_running_is_not_one_job() {
        let fixture = Fixture::new("running_daemon");
        // A day old, with workers it started just now.
        process(&fixture, 50, 1, 50, "postgres", 1000);
        process(&fixture, 51, 50, 50, "postgres", 3);
        process(&fixture, 52, 50, 50, "postgres", 2);
        let units = Units {
            ticks_per_second: 100.0,
            page_bytes: 4096,
        };
        let mut collector =
            ProcessCollector::new(fixture.root(), ProcessOptions::default(), units, BOOT);
        let now = BOOT + 1000.0;
        collector.sweep(Instant::now(), now, &[], &mut MetricBatch::new(0));
        assert_eq!(running(collector.all(), now, 3600.0, 72).len(), 1);
        // With a job being at most ten minutes, the group is a daemon's.
        assert!(running(collector.all(), now, 600.0, 72).is_empty());
    }

    #[test]
    fn a_pipeline_whose_first_command_has_ended_is_still_a_job() {
        let fixture = Fixture::new("running_pipeline");
        // The group is named for a process that is gone.
        process(&fixture, 31, 10, 30, "grep", 20);
        process(&fixture, 32, 10, 30, "sort", 19);
        let jobs = jobs(&fixture);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].started, BOOT + 980.0);
        // Neither is the child of the other: each is a root.
        assert_eq!(jobs[0].tree.len(), 2);
        assert!(jobs[0].tree.iter().all(|line| !line.contains('─')));
    }
}
