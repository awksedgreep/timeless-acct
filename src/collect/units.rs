//! Units: what systemd runs, as the kernel accounts for it.
//!
//! These figures are not added up from processes. The kernel keeps them
//! for each control group, and they hold everything that ever ran in it,
//! including what started and ended between two readings. Nothing has to
//! be alive at the moment of a sweep to be counted.
//!
//! Every service, scope, and slice that has tasks in it is reported, under
//! its unit's name. Instances of the same application are added together.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::cgroup::{kind, parse_io_stat, unit_name, Containers};
use crate::collect::percent;
use crate::collect::process::GroupUse;
use crate::model::{labels, Labels, MetricBatch};
use crate::procfs::system::{parse_pairs, parse_pressure};
use crate::procfs::ProcRoot;

use super::users::Users;

const USEC: f64 = 1e6;

/// One control group's counters, as they stood at a reading.
#[derive(Debug, Clone, Copy, Default)]
struct Counters {
    cpu_usec: u64,
    cpu_user_usec: u64,
    cpu_system_usec: u64,
    /// Only a group with a CPU limit is ever throttled.
    throttled_usec: Option<u64>,
    io: Option<(u64, u64)>,
    oom_kills: Option<u64>,
    /// Microseconds some task was stalled: cpu, memory, io.
    stalled_usec: [Option<u64>; 3],
}

/// What a unit used over an interval, and holds now.
#[derive(Debug, Default)]
struct Total {
    tasks: u64,
    processes: u64,
    memory: Option<u64>,
    anon: Option<u64>,
    file: Option<u64>,
    swap: Option<u64>,
    /// Whether any of its groups was read before, or is new since then.
    measured: bool,
    cpu_usec: u64,
    cpu_user_usec: u64,
    cpu_system_usec: u64,
    throttled_usec: Option<u64>,
    io: Option<(u64, u64)>,
    oom_kills: Option<u64>,
    stalled_usec: [Option<u64>; 3],
}

fn add(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0) + value);
    }
}

pub struct UnitCollector {
    root: ProcRoot,
    base: PathBuf,
    users: Users,
    containers: Arc<Containers>,
    previous: HashMap<String, Counters>,
    previous_at: Option<Instant>,
    labels: HashMap<String, Labels>,
    /// Whether a device, by `major:minor`, is built on other devices.
    stacked: HashMap<String, bool>,
}

impl UnitCollector {
    /// `None` if this host has no unified control group hierarchy.
    pub fn new(root: ProcRoot) -> Option<Self> {
        let base = root.sys_path("fs/cgroup");
        base.join("cgroup.controllers").exists().then(|| Self {
            root,
            base,
            users: Users::default(),
            containers: Arc::default(),
            previous: HashMap::new(),
            previous_at: None,
            labels: HashMap::new(),
            stacked: HashMap::new(),
        })
    }

    /// Look for containers, so that what is named for one can be named
    /// for what runs it. The same map is for whoever else names units.
    pub fn containers(&mut self) -> Arc<Containers> {
        self.containers = Arc::new(Containers::scan(&self.base));
        Arc::clone(&self.containers)
    }

    /// Append this instant's samples. `processes` is what the sweep found
    /// directly in each control group.
    pub fn collect(
        &mut self,
        now: Instant,
        processes: &HashMap<String, GroupUse>,
        batch: &mut MetricBatch,
    ) {
        let interval = self
            .previous_at
            .map(|at| now.duration_since(at).as_secs_f64())
            .filter(|seconds| *seconds > 0.0);

        let mut groups = Vec::new();
        self.walk(&self.base.clone(), "", &mut groups);

        let mut readings = HashMap::with_capacity(groups.len());
        let mut totals: HashMap<String, Total> = HashMap::new();
        for (path, name, tasks) in groups {
            let dir = self.base.join(path.trim_start_matches('/'));
            let counters = self.read_counters(&dir);
            let total = totals.entry(name).or_default();

            total.tasks += tasks;
            let below = format!("{path}/");
            let mut sampled_io = None;
            for (_, used) in processes
                .iter()
                .filter(|(group, _)| **group == path || group.starts_with(&below))
            {
                total.processes += used.processes;
                if let Some((read, write)) = used.io {
                    let (read_so_far, write_so_far) = sampled_io.unwrap_or_default();
                    sampled_io = Some((read_so_far + read, write_so_far + write));
                }
            }
            add(&mut total.memory, number(&dir, "memory.current"));
            add(&mut total.swap, number(&dir, "memory.swap.current"));
            if let Ok(text) = fs::read_to_string(dir.join("memory.stat")) {
                let stat = parse_pairs(&text);
                add(&mut total.anon, stat.get("anon").copied());
                add(&mut total.file, stat.get("file").copied());
            }

            if interval.is_some() {
                // A group that was not there at the last reading was made
                // since, and all it has used, it used in this interval.
                let before = self.previous.get(&path).copied().unwrap_or_default();
                total.measured = true;
                total.cpu_usec += counters.cpu_usec.saturating_sub(before.cpu_usec);
                total.cpu_user_usec += counters.cpu_user_usec.saturating_sub(before.cpu_user_usec);
                total.cpu_system_usec += counters
                    .cpu_system_usec
                    .saturating_sub(before.cpu_system_usec);
                let since = |now: Option<u64>, before: Option<u64>| {
                    now.map(|now| now.saturating_sub(before.unwrap_or(0)))
                };
                add(
                    &mut total.throttled_usec,
                    since(counters.throttled_usec, before.throttled_usec),
                );
                add(
                    &mut total.oom_kills,
                    since(counters.oom_kills, before.oom_kills),
                );
                // The kernel keeps a group's I/O only where the I/O
                // controller is on, and for a user's units it is not. There,
                // what its processes did between two sweeps is what there
                // is: all of it, but for processes that did not live to one.
                let io = match counters.io {
                    Some((read, write)) => {
                        let (read_before, write_before) = before.io.unwrap_or_default();
                        Some((
                            read.saturating_sub(read_before),
                            write.saturating_sub(write_before),
                        ))
                    }
                    None => sampled_io,
                };
                if let Some((read, write)) = io {
                    let (read_total, write_total) = total.io.unwrap_or_default();
                    total.io = Some((read_total + read, write_total + write));
                }
                for (index, stalled) in counters.stalled_usec.iter().enumerate() {
                    add(
                        &mut total.stalled_usec[index],
                        since(*stalled, before.stalled_usec[index]),
                    );
                }
            }
            readings.insert(path, counters);
        }

        let mut names: Vec<&String> = totals.keys().collect();
        names.sort();
        for name in names {
            let total = &totals[name];
            let label = self
                .labels
                .entry(name.clone())
                .or_insert_with(|| labels(vec![("unit", name.clone())]))
                .clone();
            let l = &label;

            batch.push("unit_tasks", l, total.tasks as f64);
            batch.push("unit_processes", l, total.processes as f64);
            for (metric, value) in [
                ("unit_memory_bytes", total.memory),
                ("unit_memory_anon_bytes", total.anon),
                ("unit_memory_file_bytes", total.file),
                ("unit_swap_bytes", total.swap),
            ] {
                if let Some(value) = value {
                    batch.push(metric, l, value as f64);
                }
            }

            let Some(seconds) = interval.filter(|_| total.measured) else {
                continue;
            };
            let share = |usec: u64| percent(usec as f64 / USEC, seconds);
            batch.push("unit_cpu_pct", l, share(total.cpu_usec));
            batch.push("unit_cpu_user_pct", l, share(total.cpu_user_usec));
            batch.push("unit_cpu_system_pct", l, share(total.cpu_system_usec));
            if let Some(throttled) = total.throttled_usec {
                batch.push("unit_cpu_throttled_pct", l, share(throttled));
            }
            if let Some((read, write)) = total.io {
                batch.push("unit_io_read_bytes_per_sec", l, read as f64 / seconds);
                batch.push("unit_io_write_bytes_per_sec", l, write as f64 / seconds);
            }
            if let Some(kills) = total.oom_kills {
                batch.push("unit_oom_kills_per_sec", l, kills as f64 / seconds);
            }
            for (metric, stalled) in [
                "unit_pressure_cpu_pct",
                "unit_pressure_memory_pct",
                "unit_pressure_io_pct",
            ]
            .into_iter()
            .zip(total.stalled_usec)
            {
                if let Some(stalled) = stalled {
                    // Instances stall side by side; together they cannot
                    // have been stalled for more than all of the time.
                    batch.push(metric, l, share(stalled).min(100.0));
                }
            }
        }

        self.previous = readings;
        self.previous_at = Some(now);
    }

    /// Find every unit that has tasks: `(path, reported name, tasks)`.
    fn walk(&mut self, dir: &Path, path: &str, found: &mut Vec<(String, String, u64)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(component) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let child = format!("{path}/{component}");
            let dir = entry.path();
            if kind(&component).is_some() {
                match number(&dir, "pids.current") {
                    // A count of everything at and below the group: where
                    // it is zero, there is nothing below to look for.
                    Some(0) => continue,
                    Some(tasks) => {
                        let users = &mut self.users;
                        let name = unit_name(
                            &child,
                            &mut |uid| users.name(uid).to_string(),
                            &self.containers,
                        );
                        if let Some(name) = name {
                            found.push((child.clone(), name, tasks));
                        }
                    }
                    // Task accounting is off for this group, which says
                    // nothing about the ones inside it.
                    None => {}
                }
            }
            // A container's own groups, a delegated subtree: not units,
            // and there may be units beneath them.
            self.walk(&dir, &child, found);
        }
    }

    fn read_counters(&mut self, dir: &Path) -> Counters {
        let mut counters = Counters::default();
        if let Ok(text) = fs::read_to_string(dir.join("cpu.stat")) {
            let stat = parse_pairs(&text);
            let get = |key: &str| stat.get(key).copied();
            counters.cpu_usec = get("usage_usec").unwrap_or(0);
            counters.cpu_user_usec = get("user_usec").unwrap_or(0);
            counters.cpu_system_usec = get("system_usec").unwrap_or(0);
            counters.throttled_usec =
                get("throttled_usec").filter(|_| get("nr_periods").is_some_and(|n| n > 0));
        }
        if let Ok(text) = fs::read_to_string(dir.join("memory.events")) {
            counters.oom_kills = parse_pairs(&text).get("oom_kill").copied();
        }
        if let Ok(text) = fs::read_to_string(dir.join("io.stat")) {
            counters.io = Some(self.storage_io(&text));
        }
        for (index, file) in ["cpu.pressure", "memory.pressure", "io.pressure"]
            .into_iter()
            .enumerate()
        {
            if let Ok(text) = fs::read_to_string(dir.join(file)) {
                counters.stalled_usec[index] = parse_pressure(&text).some.map(|s| s.total_us);
            }
        }
        counters
    }

    /// Bytes read and written, over the devices that are storage.
    ///
    /// A filesystem on an encrypted or a logical volume is on a device
    /// built on another, and its I/O is charged to both. The devices at
    /// the bottom are counted; only if a group touched none of them are
    /// the ones above.
    fn storage_io(&mut self, text: &str) -> (u64, u64) {
        let devices = parse_io_stat(text);
        let mut physical = (0, 0, false);
        let mut stacked = (0, 0);
        for io in devices {
            if self.is_stacked(&io.device) {
                stacked = (stacked.0 + io.read_bytes, stacked.1 + io.write_bytes);
            } else {
                physical = (
                    physical.0 + io.read_bytes,
                    physical.1 + io.write_bytes,
                    true,
                );
            }
        }
        if physical.2 {
            (physical.0, physical.1)
        } else {
            stacked
        }
    }

    fn is_stacked(&mut self, device: &str) -> bool {
        if let Some(known) = self.stacked.get(device) {
            return *known;
        }
        // /sys/dev/block/253:0 -> ../../devices/virtual/block/dm-0
        let name = fs::read_link(self.root.sys_path("dev/block").join(device))
            .ok()
            .and_then(|target| {
                target
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let stacked = name.starts_with("dm-") || name.starts_with("md");
        self.stacked.insert(device.to_string(), stacked);
        stacked
    }
}

fn number(dir: &Path, file: &str) -> Option<u64> {
    fs::read_to_string(dir.join(file)).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::time::Duration;

    const USER: &str = "user.slice/user-0.slice/user@0.service";

    struct Group<'a> {
        path: &'a str,
        tasks: u64,
        cpu_usec: u64,
        memory: u64,
        read_bytes: u64,
        stalled_usec: u64,
        oom_kills: u64,
    }

    impl Default for Group<'_> {
        fn default() -> Self {
            Self {
                path: "system.slice/db.service",
                tasks: 4,
                cpu_usec: 0,
                memory: 1 << 20,
                read_bytes: 0,
                stalled_usec: 0,
                oom_kills: 0,
            }
        }
    }

    fn write(fixture: &Fixture, g: &Group) {
        let file = |name: &str, content: String| {
            fixture.sys_file(&format!("fs/cgroup/{}/{name}", g.path), &content);
        };
        file("pids.current", format!("{}\n", g.tasks));
        file(
            "cpu.stat",
            format!(
                "usage_usec {}\nuser_usec {}\nsystem_usec {}\nnr_periods 0\nthrottled_usec 0\n",
                g.cpu_usec,
                g.cpu_usec * 3 / 4,
                g.cpu_usec / 4
            ),
        );
        file("memory.current", format!("{}\n", g.memory));
        file("memory.swap.current", "0\n".into());
        file(
            "memory.stat",
            format!("anon {}\nfile {}\n", g.memory / 2, g.memory / 4),
        );
        file(
            "memory.events",
            format!("oom 0\noom_kill {}\n", g.oom_kills),
        );
        file(
            "io.stat",
            format!(
                "259:0 rbytes={0} wbytes=0 rios=1 wios=0\n253:0 rbytes={0} wbytes=0 rios=1 wios=0\n",
                g.read_bytes
            ),
        );
        file(
            "io.pressure",
            format!(
                "some avg10=0.00 avg60=0.00 avg300=0.00 total={}\n",
                g.stalled_usec
            ),
        );
    }

    fn fixture(name: &str) -> Fixture {
        let fixture = Fixture::new(name);
        fixture.sys_file("fs/cgroup/cgroup.controllers", "cpu io memory pids\n");
        fixture.sys_link("dev/block/259:0", "../../devices/pci/nvme0n1");
        fixture.sys_link("dev/block/253:0", "../../devices/virtual/block/dm-0");
        fixture
    }

    fn value(batch: &MetricBatch, name: &str, unit: &str) -> Option<f64> {
        batch
            .samples
            .iter()
            .find(|s| s.name == name && s.labels.iter().any(|(k, v)| *k == "unit" && v == unit))
            .map(|s| s.value)
    }

    #[test]
    fn a_host_without_the_unified_hierarchy_has_no_units() {
        let fixture = Fixture::new("units_absent");
        assert!(UnitCollector::new(fixture.root()).is_none());
    }

    #[test]
    fn a_unit_is_reported_from_the_kernels_own_accounts() {
        let fixture = fixture("units_rates");
        let mut collector = UnitCollector::new(fixture.root()).unwrap();
        let start = Instant::now();
        let of = |processes: u64| GroupUse {
            processes,
            // Not used where the kernel keeps the account itself.
            io: Some((777, 777)),
        };
        let processes = HashMap::from([
            ("/system.slice/db.service".to_string(), of(1)),
            ("/system.slice/db.service/workers".to_string(), of(2)),
            ("/system.slice/dbx.service".to_string(), of(9)),
        ]);

        write(
            &fixture,
            &Group {
                cpu_usec: 1_000_000,
                ..Group::default()
            },
        );
        let mut first = MetricBatch::new(1);
        collector.collect(start, &processes, &mut first);
        assert_eq!(value(&first, "unit_tasks", "db.service"), Some(4.0));
        // Its own processes and those of the groups inside it, and not
        // those of a unit whose name merely starts the same.
        assert_eq!(value(&first, "unit_processes", "db.service"), Some(3.0));
        assert_eq!(
            value(&first, "unit_memory_bytes", "db.service"),
            Some(1_048_576.0)
        );
        assert_eq!(
            value(&first, "unit_memory_anon_bytes", "db.service"),
            Some(524_288.0)
        );
        assert_eq!(value(&first, "unit_cpu_pct", "db.service"), None);

        write(
            &fixture,
            &Group {
                cpu_usec: 5_000_000,
                read_bytes: 1_000_000,
                stalled_usec: 500_000,
                oom_kills: 1,
                ..Group::default()
            },
        );
        let mut second = MetricBatch::new(2);
        collector.collect(start + Duration::from_secs(10), &processes, &mut second);
        assert_eq!(value(&second, "unit_cpu_pct", "db.service"), Some(40.0));
        assert_eq!(
            value(&second, "unit_cpu_user_pct", "db.service"),
            Some(30.0)
        );
        assert_eq!(
            value(&second, "unit_cpu_system_pct", "db.service"),
            Some(10.0)
        );
        // Charged to the disk and to the volume on it; counted once.
        assert_eq!(
            value(&second, "unit_io_read_bytes_per_sec", "db.service"),
            Some(100_000.0)
        );
        assert_eq!(
            value(&second, "unit_pressure_io_pct", "db.service"),
            Some(5.0)
        );
        assert_eq!(
            value(&second, "unit_oom_kills_per_sec", "db.service"),
            Some(0.1)
        );
        // No limit was set, so there is no throttling to report.
        assert_eq!(value(&second, "unit_cpu_throttled_pct", "db.service"), None);
    }

    #[test]
    fn instances_of_an_application_are_one_unit() {
        let fixture = fixture("units_instances");
        let mut collector = UnitCollector::new(fixture.root()).unwrap();
        let start = Instant::now();
        let first_path = format!("{USER}/app.slice/app-term-0a1b2c3d.scope");
        let second_path = format!("{USER}/app.slice/app-term-4e5f6a7b.scope");
        let none = HashMap::new();

        write(
            &fixture,
            &Group {
                path: &first_path,
                tasks: 2,
                cpu_usec: 1_000_000,
                ..Group::default()
            },
        );
        collector.collect(start, &none, &mut MetricBatch::new(1));

        // The first goes on; a second is opened, and uses a second of CPU
        // before the next reading.
        write(
            &fixture,
            &Group {
                path: &first_path,
                tasks: 2,
                cpu_usec: 2_000_000,
                ..Group::default()
            },
        );
        write(
            &fixture,
            &Group {
                path: &second_path,
                tasks: 3,
                cpu_usec: 1_000_000,
                ..Group::default()
            },
        );
        let mut batch = MetricBatch::new(2);
        collector.collect(start + Duration::from_secs(10), &none, &mut batch);

        let unit = "root/app-term.scope";
        assert_eq!(value(&batch, "unit_tasks", unit), Some(5.0));
        assert_eq!(value(&batch, "unit_memory_bytes", unit), Some(2_097_152.0));
        assert_eq!(value(&batch, "unit_cpu_pct", unit), Some(20.0));

        // The first is closed. What is left is the second alone, and the
        // counters of the one that went do not come off the total.
        fixture.remove_sys(&format!("fs/cgroup/{first_path}"));
        write(
            &fixture,
            &Group {
                path: &second_path,
                tasks: 3,
                cpu_usec: 1_500_000,
                ..Group::default()
            },
        );
        let mut batch = MetricBatch::new(3);
        collector.collect(start + Duration::from_secs(20), &none, &mut batch);
        assert_eq!(value(&batch, "unit_tasks", unit), Some(3.0));
        assert_eq!(value(&batch, "unit_cpu_pct", unit), Some(5.0));
    }

    #[test]
    fn where_the_kernel_keeps_no_account_of_io_the_sweeps_is_used() {
        let fixture = fixture("units_sampled_io");
        let mut collector = UnitCollector::new(fixture.root()).unwrap();
        let start = Instant::now();
        let path = format!("{USER}/app.slice/stack.service");
        let io_stat = fixture.path(&format!("sys/fs/cgroup/{path}/io.stat"));
        let sampled = |read: u64| {
            HashMap::from([
                (
                    format!("/{path}/payload"),
                    GroupUse {
                        processes: 3,
                        io: Some((read, 50_000)),
                    },
                ),
                (
                    format!("/{path}/runtime"),
                    GroupUse {
                        processes: 1,
                        io: None,
                    },
                ),
            ])
        };

        write(
            &fixture,
            &Group {
                path: &path,
                ..Group::default()
            },
        );
        std::fs::remove_file(&io_stat).unwrap();
        collector.collect(start, &sampled(0), &mut MetricBatch::new(1));

        write(
            &fixture,
            &Group {
                path: &path,
                ..Group::default()
            },
        );
        std::fs::remove_file(&io_stat).unwrap();
        let mut batch = MetricBatch::new(2);
        collector.collect(
            start + Duration::from_secs(10),
            &sampled(200_000),
            &mut batch,
        );

        let unit = "root/stack.service";
        assert_eq!(value(&batch, "unit_processes", unit), Some(4.0));
        assert_eq!(
            value(&batch, "unit_io_read_bytes_per_sec", unit),
            Some(20_000.0)
        );
        assert_eq!(
            value(&batch, "unit_io_write_bytes_per_sec", unit),
            Some(5000.0)
        );
    }

    #[test]
    fn a_containers_health_check_is_part_of_what_the_container_costs() {
        let fixture = fixture("units_containers");
        let mut collector = UnitCollector::new(fixture.root()).unwrap();
        let start = Instant::now();
        let id = "3f9a1c07e5b24d68a0c1f2e3d4b5a69788776655443322110ffeeddccbbaa991";
        let stack = format!("{USER}/app.slice/stack.service");
        let check = format!("{USER}/app.slice/{id}-1f2e3d4c5b6a7980.service");
        fixture.sys_dir(&format!("fs/cgroup/{stack}/libpod-payload-{id}"));

        write(
            &fixture,
            &Group {
                path: &stack,
                tasks: 20,
                ..Group::default()
            },
        );
        collector.containers();
        collector.collect(start, &HashMap::new(), &mut MetricBatch::new(1));

        // The check runs, in a unit of its own, and uses a second of CPU.
        write(
            &fixture,
            &Group {
                path: &stack,
                tasks: 20,
                cpu_usec: 2_000_000,
                ..Group::default()
            },
        );
        write(
            &fixture,
            &Group {
                path: &check,
                tasks: 2,
                cpu_usec: 1_000_000,
                ..Group::default()
            },
        );
        collector.containers();
        let mut batch = MetricBatch::new(2);
        collector.collect(start + Duration::from_secs(10), &HashMap::new(), &mut batch);

        let unit = "root/stack.service";
        assert_eq!(value(&batch, "unit_tasks", unit), Some(22.0));
        assert_eq!(value(&batch, "unit_cpu_pct", unit), Some(30.0));
        assert_eq!(
            value(&batch, "unit_cpu_pct", "root/transient.service"),
            None
        );
    }

    #[test]
    fn a_slice_is_reported_beside_the_units_in_it() {
        let fixture = fixture("units_slices");
        let mut collector = UnitCollector::new(fixture.root()).unwrap();
        let start = Instant::now();
        for (cpu, at) in [(0, 0), (4_000_000, 10)] {
            write(
                &fixture,
                &Group {
                    path: "system.slice",
                    tasks: 9,
                    cpu_usec: cpu * 2,
                    ..Group::default()
                },
            );
            write(
                &fixture,
                &Group {
                    cpu_usec: cpu,
                    ..Group::default()
                },
            );
            let mut batch = MetricBatch::new(at);
            collector.collect(
                start + Duration::from_secs(at as u64),
                &HashMap::new(),
                &mut batch,
            );
            if at > 0 {
                // The kernel counts a group's descendants in the group, so
                // the slice is read, and nothing is added up twice.
                assert_eq!(value(&batch, "unit_cpu_pct", "system.slice"), Some(80.0));
                assert_eq!(value(&batch, "unit_tasks", "system.slice"), Some(9.0));
                assert_eq!(value(&batch, "unit_cpu_pct", "db.service"), Some(40.0));
            }
        }
    }

    #[test]
    fn a_unit_with_nothing_in_it_is_not_reported() {
        let fixture = fixture("units_empty");
        let mut collector = UnitCollector::new(fixture.root()).unwrap();
        write(
            &fixture,
            &Group {
                tasks: 0,
                ..Group::default()
            },
        );
        write(
            &fixture,
            &Group {
                path: "system.slice/web.service/payload",
                ..Group::default()
            },
        );
        let mut batch = MetricBatch::new(1);
        collector.collect(Instant::now(), &HashMap::new(), &mut batch);
        assert_eq!(value(&batch, "unit_tasks", "db.service"), None);
        // A group inside a unit is not a unit, whatever is in it.
        assert!(batch
            .samples
            .iter()
            .all(|s| s.labels.iter().all(|(_, v)| !v.contains("payload"))));
    }
}
