//! What is on the screen: the host at one moment.
//!
//! A moment is read from one of two places. Now is read from the kernel,
//! by the same collectors the store is filled by. Any other moment is read
//! from the store. Both are turned into the same snapshot by the same
//! code, so what is watched live and what is rewound to are the same
//! figures under the same names.

use std::collections::{BTreeMap, HashMap};

use crate::model::MetricBatch;

/// The series of one metric at a moment: each one's labels, and its value.
pub type Series = Vec<(BTreeMap<String, String>, f64)>;

/// Where a moment is read from.
pub trait Source {
    fn series(&mut self, metric: &str) -> Series;
}

/// A reading just taken from the kernel.
pub struct Sampled(HashMap<&'static str, Series>);

impl Sampled {
    pub fn new(batch: &MetricBatch) -> Self {
        let mut by_name: HashMap<&'static str, Series> = HashMap::new();
        for sample in &batch.samples {
            let labels = sample
                .labels
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            by_name
                .entry(sample.name)
                .or_default()
                .push((labels, sample.value));
        }
        Self(by_name)
    }
}

impl Source for Sampled {
    fn series(&mut self, metric: &str) -> Series {
        self.0.get(metric).cloned().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct System {
    pub load: Option<[f64; 3]>,
    pub cpu_busy: Option<f64>,
    pub cpu_user: Option<f64>,
    pub cpu_system: Option<f64>,
    pub cpu_iowait: Option<f64>,
    pub mem_used: Option<f64>,
    pub mem_total: Option<f64>,
    pub swap_used: Option<f64>,
    pub swap_total: Option<f64>,
    pub io_read: Option<f64>,
    pub io_write: Option<f64>,
    pub net_rx: Option<f64>,
    pub net_tx: Option<f64>,
    pub tasks: Option<f64>,
    /// The hottest of the sensors, in degrees Celsius.
    pub temp: Option<f64>,
    /// What the CPU packages draw, in watts.
    pub power: Option<f64>,
    /// Share of time something was stalled for want of: CPU, memory, I/O.
    pub pressure: [Option<f64>; 3],
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Unit {
    pub name: String,
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub processes: Option<f64>,
    pub tasks: Option<f64>,
    pub read: Option<f64>,
    pub write: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Process {
    /// `postgres[1234]`: what names its series.
    pub name: String,
    pub pid: String,
    pub user: String,
    pub comm: String,
    /// Empty if it is in none.
    pub unit: String,
    pub cpu: Option<f64>,
    pub rss: Option<f64>,
    pub threads: Option<f64>,
    pub read: Option<f64>,
    pub write: Option<f64>,
    pub cpu_seconds: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Epoch seconds.
    pub at: f64,
    pub system: System,
    pub units: Vec<Unit>,
    pub processes: Vec<Process>,
}

/// The one series of a metric that has no label but the host's.
fn only(source: &mut dyn Source, metric: &str) -> Option<f64> {
    source
        .series(metric)
        .into_iter()
        .find(|(labels, _)| labels.keys().all(|key| key == "host"))
        .map(|(_, value)| value)
}

fn labelled(source: &mut dyn Source, metric: &str, key: &str, want: &str) -> Option<f64> {
    source
        .series(metric)
        .into_iter()
        .find(|(labels, _)| labels.get(key).is_some_and(|value| value == want))
        .map(|(_, value)| value)
}

/// Each series of a metric, by the value of one of its labels.
fn by(source: &mut dyn Source, metric: &str, key: &str) -> HashMap<String, f64> {
    source
        .series(metric)
        .into_iter()
        .filter_map(|(mut labels, value)| Some((labels.remove(key)?, value)))
        .collect()
}

impl Snapshot {
    pub fn read(at: f64, source: &mut dyn Source) -> Self {
        let load = (
            only(source, "sys_load1"),
            only(source, "sys_load5"),
            only(source, "sys_load15"),
        );
        let cpu = |source: &mut dyn Source, metric| labelled(source, metric, "cpu", "all");
        // Every interface but the one a host talks to itself on.
        let network = |source: &mut dyn Source, metric| {
            let interfaces = by(source, metric, "iface");
            (!interfaces.is_empty()).then(|| {
                interfaces
                    .iter()
                    .filter(|(name, _)| name.as_str() != "lo")
                    .map(|(_, value)| value)
                    .sum()
            })
        };
        let system = System {
            load: match load {
                (Some(a), Some(b), Some(c)) => Some([a, b, c]),
                _ => None,
            },
            cpu_busy: cpu(source, "sys_cpu_busy_pct"),
            cpu_user: cpu(source, "sys_cpu_user_pct"),
            cpu_system: cpu(source, "sys_cpu_system_pct"),
            cpu_iowait: cpu(source, "sys_cpu_iowait_pct"),
            mem_used: only(source, "sys_mem_used_bytes"),
            mem_total: only(source, "sys_mem_total_bytes"),
            swap_used: only(source, "sys_swap_used_bytes"),
            swap_total: only(source, "sys_swap_total_bytes"),
            io_read: only(source, "sys_io_read_bytes_per_sec"),
            io_write: only(source, "sys_io_write_bytes_per_sec"),
            net_rx: network(source, "sys_net_rx_bytes_per_sec"),
            net_tx: network(source, "sys_net_tx_bytes_per_sec"),
            tasks: only(source, "sys_tasks"),
            temp: by(source, "sys_temp_celsius", "sensor")
                .into_values()
                .reduce(f64::max),
            // A domain inside another is in its figure already.
            power: {
                let domains = by(source, "sys_power_watts", "domain");
                let packages: Vec<f64> = domains
                    .iter()
                    .filter(|(name, _)| !name.contains('/') && name.starts_with("package"))
                    .map(|(_, watts)| *watts)
                    .collect();
                (!packages.is_empty()).then(|| packages.iter().sum())
            },
            pressure: [
                only(source, "sys_pressure_cpu_some_pct"),
                only(source, "sys_pressure_memory_some_pct"),
                only(source, "sys_pressure_io_some_pct"),
            ],
        };

        let cpu = by(source, "unit_cpu_pct", "unit");
        let memory = by(source, "unit_memory_bytes", "unit");
        let processes = by(source, "unit_processes", "unit");
        let tasks = by(source, "unit_tasks", "unit");
        let read = by(source, "unit_io_read_bytes_per_sec", "unit");
        let write = by(source, "unit_io_write_bytes_per_sec", "unit");
        // A unit is there if it has anything in it, which is what its
        // count of tasks says; its rates come a reading later.
        let mut units: Vec<Unit> = tasks
            .keys()
            .chain(cpu.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|name| Unit {
                name: name.clone(),
                cpu: cpu.get(name).copied(),
                memory: memory.get(name).copied(),
                processes: processes.get(name).copied(),
                tasks: tasks.get(name).copied(),
                read: read.get(name).copied(),
                write: write.get(name).copied(),
            })
            .collect();
        units.sort_by(|a, b| a.name.cmp(&b.name));

        let rss = by(source, "proc_rss_bytes", "proc");
        let threads = by(source, "proc_threads", "proc");
        let read = by(source, "proc_io_read_bytes_per_sec", "proc");
        let write = by(source, "proc_io_write_bytes_per_sec", "proc");
        let seconds = by(source, "proc_cpu_seconds", "proc");
        let mut processes: Vec<Process> = source
            .series("proc_cpu_pct")
            .into_iter()
            .filter_map(|(labels, cpu)| {
                let name = labels.get("proc")?.clone();
                let label = |key: &str| labels.get(key).cloned().unwrap_or_default();
                Some(Process {
                    pid: label("pid"),
                    user: label("user"),
                    comm: label("comm"),
                    unit: label("unit"),
                    cpu: Some(cpu),
                    rss: rss.get(&name).copied(),
                    threads: threads.get(&name).copied(),
                    read: read.get(&name).copied(),
                    write: write.get(&name).copied(),
                    cpu_seconds: seconds.get(&name).copied(),
                    name,
                })
            })
            .collect();
        processes.sort_by(|a, b| a.name.cmp(&b.name));

        Self {
            at,
            system,
            units,
            processes,
        }
    }

    /// Whether the moment has anything in it. A moment before the store
    /// began, or in a gap while the collector was stopped, has not.
    pub fn is_empty(&self) -> bool {
        self.system == System::default() && self.units.is_empty() && self.processes.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{labels, no_labels};

    /// A reading of a host with two units and two processes.
    pub(crate) fn batch() -> MetricBatch {
        let mut batch = MetricBatch::new(1000);
        let none = no_labels();
        for (name, value) in [
            ("sys_load1", 2.5),
            ("sys_load5", 2.0),
            ("sys_load15", 1.5),
            ("sys_mem_used_bytes", 4.0e9),
            ("sys_mem_total_bytes", 16.0e9),
            ("sys_io_write_bytes_per_sec", 1.0e6),
            ("sys_tasks", 900.0),
            ("sys_pressure_io_some_pct", 3.0),
        ] {
            batch.push(name, &none, value);
        }
        for (sensor, degrees) in [("coretemp/Package id 0", 56.0), ("nvme/Composite", 33.0)] {
            let label = labels(vec![("sensor", sensor.into())]);
            batch.push("sys_temp_celsius", &label, degrees);
        }
        for (domain, watts) in [
            ("package-0", 14.0),
            ("package-0/core", 7.0),
            ("package-1", 6.0),
        ] {
            let label = labels(vec![("domain", domain.into())]);
            batch.push("sys_power_watts", &label, watts);
        }
        for (cpu, busy) in [("all", 12.5), ("0", 50.0), ("1", 0.0)] {
            batch.push("sys_cpu_busy_pct", &labels(vec![("cpu", cpu.into())]), busy);
        }
        for (iface, rx) in [("lo", 9.0e9), ("eth0", 1000.0), ("wlan0", 500.0)] {
            let label = labels(vec![("iface", iface.into())]);
            batch.push("sys_net_rx_bytes_per_sec", &label, rx);
        }
        for (unit, cpu, memory, tasks) in [
            ("postgresql.service", 40.0, 2.0e9, 12.0),
            ("mark/caddy.service", 1.5, 9.0e7, 18.0),
        ] {
            let label = labels(vec![("unit", unit.into())]);
            batch.push("unit_cpu_pct", &label, cpu);
            batch.push("unit_memory_bytes", &label, memory);
            batch.push("unit_tasks", &label, tasks);
        }
        // A unit seen for the first time: it has levels, and no rates yet.
        batch.push(
            "unit_tasks",
            &labels(vec![("unit", "new.service".into())]),
            3.0,
        );
        for (pid, comm, user, unit, cpu, rss) in [
            (
                "100",
                "postgres",
                "postgres",
                "postgresql.service",
                38.0,
                1.5e9,
            ),
            ("200", "caddy", "mark", "mark/caddy.service", 1.5, 8.0e7),
        ] {
            let label = labels(vec![
                ("pid", pid.into()),
                ("comm", comm.into()),
                ("user", user.into()),
                ("proc", format!("{comm}[{pid}]")),
                ("unit", unit.into()),
            ]);
            batch.push("proc_cpu_pct", &label, cpu);
            batch.push("proc_rss_bytes", &label, rss);
            batch.push("proc_threads", &label, 4.0);
        }
        batch
    }

    #[test]
    fn a_reading_becomes_the_host_its_units_and_its_processes() {
        let snapshot = Snapshot::read(1000.0, &mut Sampled::new(&batch()));

        let system = &snapshot.system;
        assert_eq!(system.load, Some([2.5, 2.0, 1.5]));
        // The total, and not one CPU's.
        assert_eq!(system.cpu_busy, Some(12.5));
        assert_eq!(system.mem_used, Some(4.0e9));
        assert_eq!(system.io_write, Some(1.0e6));
        assert_eq!(system.io_read, None);
        // Every interface but the loopback.
        assert_eq!(system.net_rx, Some(1500.0));
        assert_eq!(system.net_tx, None);
        assert_eq!(system.pressure, [None, None, Some(3.0)]);
        assert_eq!(system.temp, Some(56.0));
        // The packages, and not the cores inside them again.
        assert_eq!(system.power, Some(20.0));

        let names: Vec<&str> = snapshot.units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            ["mark/caddy.service", "new.service", "postgresql.service"]
        );
        let postgres = &snapshot.units[2];
        assert_eq!(postgres.cpu, Some(40.0));
        assert_eq!(postgres.memory, Some(2.0e9));
        assert_eq!(postgres.tasks, Some(12.0));
        assert_eq!(postgres.read, None);
        assert_eq!(snapshot.units[1].cpu, None);
        assert_eq!(snapshot.units[1].tasks, Some(3.0));

        assert_eq!(snapshot.processes.len(), 2);
        let caddy = &snapshot.processes[0];
        assert_eq!(caddy.name, "caddy[200]");
        assert_eq!((caddy.pid.as_str(), caddy.user.as_str()), ("200", "mark"));
        assert_eq!(caddy.unit, "mark/caddy.service");
        assert_eq!(caddy.rss, Some(8.0e7));
        assert_eq!(caddy.threads, Some(4.0));
        assert!(!snapshot.is_empty());
    }

    #[test]
    fn a_moment_with_nothing_in_it_is_empty() {
        let snapshot = Snapshot::read(5.0, &mut Sampled::new(&MetricBatch::new(5)));
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.at, 5.0);
    }
}
