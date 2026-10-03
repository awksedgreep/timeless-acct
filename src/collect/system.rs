//! System-wide statistics: what sar reports, as gauges.
//!
//! Each metric differs from its siblings by at most one label, because a
//! canvas element selects a series by host, metric, and one label. That is
//! why CPU modes are separate metrics rather than a `mode` label.

use std::collections::HashMap;
use std::ffi::CString;
use std::time::Instant;

use crate::collect::{percent, rate};
use crate::model::{labels, no_labels, Labels, MetricBatch};
use crate::procfs::system::{
    parse_diskstats, parse_keyed, parse_loadavg, parse_mounts, parse_net_dev, parse_numbers,
    parse_pairs, parse_pressure, parse_snmp, parse_sockstat, parse_stat, CpuTimes, Disk, NetDev,
    Stat,
};
use crate::procfs::ProcRoot;

const SECTOR_BYTES: f64 = 512.0;
const KIB: f64 = 1024.0;

#[derive(Debug, Clone)]
pub struct SystemOptions {
    /// Report every CPU as well as the total.
    pub per_cpu: bool,
    /// Interfaces whose name starts with one of these are not reported.
    /// Container veth pairs come and go with every container, and each one
    /// would leave a set of series behind.
    pub net_exclude: Vec<String>,
}

impl Default for SystemOptions {
    fn default() -> Self {
        Self {
            per_cpu: true,
            net_exclude: vec!["veth".into()],
        }
    }
}

/// The counters a rate needs a previous reading of.
struct Reading {
    at: Instant,
    stat: Stat,
    vmstat: HashMap<String, u64>,
    disks: HashMap<String, Disk>,
    nets: HashMap<String, NetDev>,
    snmp: HashMap<String, i64>,
    pressure_us: HashMap<&'static str, u64>,
    /// Microjoules used since boot, by power domain.
    energy_uj: HashMap<String, u64>,
}

pub struct SystemCollector {
    root: ProcRoot,
    options: SystemOptions,
    previous: Option<Reading>,
    cpu_labels: HashMap<Option<u32>, Labels>,
    device_labels: HashMap<String, Labels>,
    interface_labels: HashMap<String, Labels>,
    mount_labels: HashMap<String, Labels>,
    sensor_labels: HashMap<String, Labels>,
    none: Labels,
}

impl SystemCollector {
    pub fn new(root: ProcRoot, options: SystemOptions) -> Self {
        Self {
            root,
            options,
            previous: None,
            cpu_labels: HashMap::new(),
            device_labels: HashMap::new(),
            interface_labels: HashMap::new(),
            mount_labels: HashMap::new(),
            sensor_labels: HashMap::new(),
            none: no_labels(),
        }
    }

    #[cfg(test)]
    /// Boot time in epoch seconds, from the last reading.
    pub fn boot_time(&self) -> Option<u64> {
        self.previous.as_ref().map(|r| r.stat.boot_time)
    }

    /// Append this instant's samples. The first call has nothing to take a
    /// difference against, so it reports levels and no rates.
    pub fn collect(&mut self, now: Instant, batch: &mut MetricBatch) {
        let stat = parse_stat(&self.root.read("stat").unwrap_or_default());
        let vmstat_text = self.root.read("vmstat").unwrap_or_default();
        let vmstat: HashMap<String, u64> = parse_pairs(&vmstat_text)
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let disks: HashMap<String, Disk> =
            parse_diskstats(&self.root.read("diskstats").unwrap_or_default())
                .into_iter()
                .filter(|disk| self.is_whole_disk(&disk.name))
                .map(|disk| (disk.name.clone(), disk))
                .collect();
        let nets: HashMap<String, NetDev> =
            parse_net_dev(&self.root.read("net/dev").unwrap_or_default())
                .into_iter()
                .filter(|dev| !self.is_excluded_interface(&dev.name))
                .map(|dev| (dev.name.clone(), dev))
                .collect();
        let snmp = parse_snmp(&self.root.read("net/snmp").unwrap_or_default());

        let mut reading = Reading {
            at: now,
            stat,
            vmstat,
            disks,
            nets,
            snmp,
            pressure_us: HashMap::new(),
            energy_uj: HashMap::new(),
        };

        self.levels(batch, &reading);
        self.pressure(batch, &mut reading);
        self.filesystems(batch);
        self.sensors(batch);
        self.energy(&mut reading);

        if let Some(previous) = self.previous.take() {
            let seconds = now
                .checked_duration_since(previous.at)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if seconds > 0.0 {
                self.cpu(batch, &previous, &reading);
                self.activity(batch, &previous, &reading, seconds);
                self.paging(batch, &previous, &reading, seconds);
                self.disks(batch, &previous, &reading, seconds);
                self.network(batch, &previous, &reading, seconds);
                self.pressure_rates(batch, &previous, &reading, seconds);
                self.power(batch, &previous, &reading, seconds);
            }
        }
        self.previous = Some(reading);
    }

    /// Whole devices appear in `/sys/block`; partitions do not. Loop and RAM
    /// devices are images and scratch space rather than storage.
    fn is_whole_disk(&self, name: &str) -> bool {
        if name.starts_with("loop") || name.starts_with("ram") {
            return false;
        }
        // Device names with a slash (cciss/c0d0) use `!` in sysfs.
        let sys_name = name.replace('/', "!");
        self.root.sys_path("block").join(sys_name).exists()
    }

    fn is_excluded_interface(&self, name: &str) -> bool {
        self.options
            .net_exclude
            .iter()
            .any(|prefix| !prefix.is_empty() && name.starts_with(prefix.as_str()))
    }

    /// Gauges that need no previous reading: load, memory, swap, tables.
    fn levels(&mut self, batch: &mut MetricBatch, reading: &Reading) {
        let none = &self.none;

        if let Some(load) = parse_loadavg(&self.root.read("loadavg").unwrap_or_default()) {
            batch.push("sys_load1", none, load.load1);
            batch.push("sys_load5", none, load.load5);
            batch.push("sys_load15", none, load.load15);
            batch.push("sys_tasks", none, load.tasks as f64);
        }
        batch.push("sys_procs_running", none, reading.stat.procs_running as f64);
        batch.push("sys_procs_blocked", none, reading.stat.procs_blocked as f64);
        if let Some(uptime) = self
            .root
            .read("uptime")
            .ok()
            .and_then(|t| t.split_ascii_whitespace().next()?.parse::<f64>().ok())
        {
            batch.push("sys_uptime_seconds", none, uptime);
        }

        let meminfo_text = self.root.read("meminfo").unwrap_or_default();
        let mem = parse_keyed(&meminfo_text);
        let kb = |key: &str| mem.get(key).map(|v| *v as f64 * KIB);
        if let (Some(total), Some(available)) = (kb("MemTotal"), kb("MemAvailable")) {
            let used = total - available;
            batch.push("sys_mem_total_bytes", none, total);
            batch.push("sys_mem_available_bytes", none, available);
            batch.push("sys_mem_used_bytes", none, used);
            batch.push("sys_mem_used_pct", none, percent(used, total));
        }
        for (key, name) in [
            ("MemFree", "sys_mem_free_bytes"),
            ("Buffers", "sys_mem_buffers_bytes"),
            ("Cached", "sys_mem_cached_bytes"),
            ("Active", "sys_mem_active_bytes"),
            ("Inactive", "sys_mem_inactive_bytes"),
            ("AnonPages", "sys_mem_anon_bytes"),
            ("Shmem", "sys_mem_shmem_bytes"),
            ("Slab", "sys_mem_slab_bytes"),
            ("KernelStack", "sys_mem_kernel_stack_bytes"),
            ("PageTables", "sys_mem_page_tables_bytes"),
            ("Dirty", "sys_mem_dirty_bytes"),
            ("Writeback", "sys_mem_writeback_bytes"),
            ("Committed_AS", "sys_mem_committed_bytes"),
        ] {
            if let Some(value) = kb(key) {
                batch.push(name, none, value);
            }
        }
        if let (Some(committed), Some(total), Some(swap)) =
            (kb("Committed_AS"), kb("MemTotal"), kb("SwapTotal"))
        {
            batch.push(
                "sys_mem_committed_pct",
                none,
                percent(committed, total + swap),
            );
        }
        if let (Some(total), Some(free)) = (kb("SwapTotal"), kb("SwapFree")) {
            let used = total - free;
            batch.push("sys_swap_total_bytes", none, total);
            batch.push("sys_swap_used_bytes", none, used);
            // A host without swap uses none of it, which is a fact worth
            // drawing, rather than a division by zero worth a gap.
            let pct = if total > 0.0 {
                percent(used, total)
            } else {
                0.0
            };
            batch.push("sys_swap_used_pct", none, pct);
        }
        if let (Some(total), Some(free)) = (mem.get("HugePages_Total"), mem.get("HugePages_Free")) {
            if *total > 0 {
                batch.push("sys_hugepages_total", none, *total as f64);
                batch.push("sys_hugepages_free", none, *free as f64);
            }
        }

        let files = parse_numbers(&self.root.read("sys/fs/file-nr").unwrap_or_default());
        if files.len() >= 3 {
            batch.push("sys_file_handles", none, files[0] as f64);
            batch.push(
                "sys_file_handles_pct",
                none,
                percent(files[0] as f64, files[2] as f64),
            );
        }
        let inodes = parse_numbers(&self.root.read("sys/fs/inode-nr").unwrap_or_default());
        if inodes.len() >= 2 {
            batch.push(
                "sys_inodes",
                none,
                inodes[0].saturating_sub(inodes[1]) as f64,
            );
        }
        let dentries = parse_numbers(&self.root.read("sys/fs/dentry-state").unwrap_or_default());
        if dentries.len() >= 2 {
            batch.push("sys_dentries_unused", none, dentries[1] as f64);
        }

        let sockets = parse_sockstat(&self.root.read("net/sockstat").unwrap_or_default());
        for (key, name) in [
            ("sockets.used", "sys_sockets"),
            ("TCP.inuse", "sys_tcp_sockets"),
            ("TCP.orphan", "sys_tcp_sockets_orphan"),
            ("TCP.tw", "sys_tcp_sockets_time_wait"),
            ("UDP.inuse", "sys_udp_sockets"),
        ] {
            if let Some(value) = sockets.get(key) {
                batch.push(name, none, *value as f64);
            }
        }
        if let Some(value) = reading.snmp.get("Tcp.CurrEstab") {
            batch.push("sys_tcp_connections", none, *value as f64);
        }
    }

    fn cpu(&mut self, batch: &mut MetricBatch, previous: &Reading, reading: &Reading) {
        let all = self.cpu_label(None);
        push_cpu(batch, &all, &previous.stat.all, &reading.stat.all);
        if !self.options.per_cpu {
            return;
        }
        let before: HashMap<u32, CpuTimes> = previous.stat.cpus.iter().copied().collect();
        for (number, times) in &reading.stat.cpus {
            // A CPU brought online since the last reading has no interval.
            if let Some(earlier) = before.get(number) {
                let label = self.cpu_label(Some(*number));
                push_cpu(batch, &label, earlier, times);
            }
        }
    }

    fn cpu_label(&mut self, cpu: Option<u32>) -> Labels {
        self.cpu_labels
            .entry(cpu)
            .or_insert_with(|| {
                let value = cpu.map_or_else(|| "all".to_string(), |n| n.to_string());
                labels(vec![("cpu", value)])
            })
            .clone()
    }

    fn activity(&self, batch: &mut MetricBatch, previous: &Reading, reading: &Reading, s: f64) {
        let none = &self.none;
        let (a, b) = (&previous.stat, &reading.stat);
        batch.push("sys_forks_per_sec", none, rate(b.forks, a.forks, s));
        batch.push(
            "sys_context_switches_per_sec",
            none,
            rate(b.context_switches, a.context_switches, s),
        );
        batch.push(
            "sys_interrupts_per_sec",
            none,
            rate(b.interrupts, a.interrupts, s),
        );
        batch.push(
            "sys_softirqs_per_sec",
            none,
            rate(b.softirqs, a.softirqs, s),
        );

        for (key, name) in [
            ("Tcp.ActiveOpens", "sys_tcp_active_opens_per_sec"),
            ("Tcp.PassiveOpens", "sys_tcp_passive_opens_per_sec"),
            ("Tcp.InSegs", "sys_tcp_segments_in_per_sec"),
            ("Tcp.OutSegs", "sys_tcp_segments_out_per_sec"),
            ("Tcp.RetransSegs", "sys_tcp_retransmits_per_sec"),
            ("Tcp.AttemptFails", "sys_tcp_attempt_fails_per_sec"),
            ("Tcp.EstabResets", "sys_tcp_resets_per_sec"),
            ("Udp.InDatagrams", "sys_udp_datagrams_in_per_sec"),
            ("Udp.OutDatagrams", "sys_udp_datagrams_out_per_sec"),
            ("Udp.InErrors", "sys_udp_errors_in_per_sec"),
        ] {
            if let (Some(now), Some(before)) = (reading.snmp.get(key), previous.snmp.get(key)) {
                if *now >= 0 && *before >= 0 {
                    batch.push(name, none, rate(*now as u64, *before as u64, s));
                }
            }
        }
    }

    fn paging(&self, batch: &mut MetricBatch, previous: &Reading, reading: &Reading, s: f64) {
        let none = &self.none;
        let delta = |key: &str| -> f64 {
            match (reading.vmstat.get(key), previous.vmstat.get(key)) {
                (Some(now), Some(before)) => rate(*now, *before, s),
                _ => f64::NAN,
            }
        };
        // pgpgin and pgpgout count kibibytes despite their names.
        batch.push("sys_page_in_bytes_per_sec", none, delta("pgpgin") * KIB);
        batch.push("sys_page_out_bytes_per_sec", none, delta("pgpgout") * KIB);
        batch.push("sys_page_faults_per_sec", none, delta("pgfault"));
        batch.push("sys_major_faults_per_sec", none, delta("pgmajfault"));
        batch.push("sys_pages_freed_per_sec", none, delta("pgfree"));
        batch.push("sys_swap_in_pages_per_sec", none, delta("pswpin"));
        batch.push("sys_swap_out_pages_per_sec", none, delta("pswpout"));
        batch.push("sys_oom_kills_per_sec", none, delta("oom_kill"));

        // Reclaim counters are split by who did the scanning, and the split
        // has changed across kernel versions; sum whatever this one has.
        let sum_prefix = |prefix: &str| -> f64 {
            let mut total = 0.0;
            let mut any = false;
            for (key, now) in &reading.vmstat {
                if !key.starts_with(prefix) {
                    continue;
                }
                if let Some(before) = previous.vmstat.get(key) {
                    let value = rate(*now, *before, s);
                    if value.is_finite() {
                        total += value;
                        any = true;
                    }
                }
            }
            if any {
                total
            } else {
                f64::NAN
            }
        };
        batch.push("sys_pages_scanned_per_sec", none, sum_prefix("pgscan_"));
        batch.push("sys_pages_reclaimed_per_sec", none, sum_prefix("pgsteal_"));
    }

    fn disks(&mut self, batch: &mut MetricBatch, previous: &Reading, reading: &Reading, s: f64) {
        let mut total_reads = 0.0;
        let mut total_writes = 0.0;
        let mut total_read_bytes = 0.0;
        let mut total_write_bytes = 0.0;
        let mut any = false;

        let mut names: Vec<&String> = reading.disks.keys().collect();
        names.sort();
        for name in names {
            let now = &reading.disks[name];
            let Some(before) = previous.disks.get(name) else {
                continue;
            };
            let label = self
                .device_labels
                .entry(name.clone())
                .or_insert_with(|| labels(vec![("dev", name.clone())]))
                .clone();

            let reads = rate(now.reads, before.reads, s);
            let writes = rate(now.writes, before.writes, s);
            let read_bytes = rate(now.sectors_read, before.sectors_read, s) * SECTOR_BYTES;
            let write_bytes = rate(now.sectors_written, before.sectors_written, s) * SECTOR_BYTES;
            batch.push("sys_disk_reads_per_sec", &label, reads);
            batch.push("sys_disk_writes_per_sec", &label, writes);
            batch.push("sys_disk_read_bytes_per_sec", &label, read_bytes);
            batch.push("sys_disk_write_bytes_per_sec", &label, write_bytes);
            // Milliseconds the device had work in flight, per millisecond
            // elapsed. Parallel devices can be busy all the time and still
            // have headroom; this is sar's %util, with the same caveat.
            batch.push(
                "sys_disk_util_pct",
                &label,
                (rate(now.busy_ms, before.busy_ms, s) / 10.0).min(100.0),
            );
            batch.push(
                "sys_disk_queue_depth",
                &label,
                rate(now.weighted_ms, before.weighted_ms, s) / 1000.0,
            );
            let completed = (now.reads + now.writes).saturating_sub(before.reads + before.writes);
            let waited =
                (now.read_ms + now.write_ms).saturating_sub(before.read_ms + before.write_ms);
            // No request completed means no wait time was observed, which
            // is zero latency to draw rather than a hole in the line.
            let await_ms = if completed > 0 {
                waited as f64 / completed as f64
            } else {
                0.0
            };
            batch.push("sys_disk_await_ms", &label, await_ms);

            // Device-mapper and md devices re-count the I/O of the devices
            // beneath them, so the host total is the physical devices only.
            if !(name.starts_with("dm-") || name.starts_with("md")) {
                for (total, value) in [
                    (&mut total_reads, reads),
                    (&mut total_writes, writes),
                    (&mut total_read_bytes, read_bytes),
                    (&mut total_write_bytes, write_bytes),
                ] {
                    if value.is_finite() {
                        *total += value;
                        any = true;
                    }
                }
            }
        }

        if any {
            let none = &self.none;
            batch.push("sys_io_reads_per_sec", none, total_reads);
            batch.push("sys_io_writes_per_sec", none, total_writes);
            batch.push("sys_io_read_bytes_per_sec", none, total_read_bytes);
            batch.push("sys_io_write_bytes_per_sec", none, total_write_bytes);
        }
    }

    fn network(&mut self, batch: &mut MetricBatch, previous: &Reading, reading: &Reading, s: f64) {
        let mut names: Vec<&String> = reading.nets.keys().collect();
        names.sort();
        for name in names {
            let now = &reading.nets[name];
            let Some(before) = previous.nets.get(name) else {
                continue;
            };
            let label = self
                .interface_labels
                .entry(name.clone())
                .or_insert_with(|| labels(vec![("iface", name.clone())]))
                .clone();
            for (metric, now, before) in [
                ("sys_net_rx_bytes_per_sec", now.rx_bytes, before.rx_bytes),
                ("sys_net_tx_bytes_per_sec", now.tx_bytes, before.tx_bytes),
                (
                    "sys_net_rx_packets_per_sec",
                    now.rx_packets,
                    before.rx_packets,
                ),
                (
                    "sys_net_tx_packets_per_sec",
                    now.tx_packets,
                    before.tx_packets,
                ),
                ("sys_net_rx_errors_per_sec", now.rx_errors, before.rx_errors),
                ("sys_net_tx_errors_per_sec", now.tx_errors, before.tx_errors),
                (
                    "sys_net_rx_dropped_per_sec",
                    now.rx_dropped,
                    before.rx_dropped,
                ),
                (
                    "sys_net_tx_dropped_per_sec",
                    now.tx_dropped,
                    before.tx_dropped,
                ),
            ] {
                batch.push(metric, &label, rate(now, before, s));
            }
        }
    }

    /// Pressure stall information. The kernel's own 10-second average is
    /// reported as it stands, and the stall total is kept for an exact
    /// share over this collector's interval.
    fn pressure(&self, batch: &mut MetricBatch, reading: &mut Reading) {
        let none = &self.none;
        for (file, some_avg, full_avg, some_key, full_key) in [
            (
                "pressure/cpu",
                "sys_pressure_cpu_some_avg10",
                "sys_pressure_cpu_full_avg10",
                "cpu_some",
                "cpu_full",
            ),
            (
                "pressure/memory",
                "sys_pressure_memory_some_avg10",
                "sys_pressure_memory_full_avg10",
                "memory_some",
                "memory_full",
            ),
            (
                "pressure/io",
                "sys_pressure_io_some_avg10",
                "sys_pressure_io_full_avg10",
                "io_some",
                "io_full",
            ),
        ] {
            let Ok(text) = self.root.read(file) else {
                continue;
            };
            let pressure = parse_pressure(&text);
            if let Some(line) = pressure.some {
                batch.push(some_avg, none, line.avg10);
                reading.pressure_us.insert(some_key, line.total_us);
            }
            if let Some(line) = pressure.full {
                batch.push(full_avg, none, line.avg10);
                reading.pressure_us.insert(full_key, line.total_us);
            }
        }
    }

    fn pressure_rates(
        &self,
        batch: &mut MetricBatch,
        previous: &Reading,
        reading: &Reading,
        s: f64,
    ) {
        for (key, name) in [
            ("cpu_some", "sys_pressure_cpu_some_pct"),
            ("cpu_full", "sys_pressure_cpu_full_pct"),
            ("memory_some", "sys_pressure_memory_some_pct"),
            ("memory_full", "sys_pressure_memory_full_pct"),
            ("io_some", "sys_pressure_io_some_pct"),
            ("io_full", "sys_pressure_io_full_pct"),
        ] {
            if let (Some(now), Some(before)) =
                (reading.pressure_us.get(key), previous.pressure_us.get(key))
            {
                // Stalled microseconds per second elapsed, as a percentage.
                batch.push(name, &self.none, rate(*now, *before, s) / 10_000.0);
            }
        }
    }

    fn sensor_label(&mut self, key: &'static str, name: String) -> Labels {
        self.sensor_labels
            .entry(format!("{key}={name}"))
            .or_insert_with(|| labels(vec![(key, name)]))
            .clone()
    }

    /// Temperatures and fans, from the kernel's hardware monitoring, and
    /// the speed the CPUs are running at: what sar reports with `-m`.
    ///
    /// A sensor is named for its chip and for what the chip calls it:
    /// `coretemp/Package id 0`. Where a host has two of a chip, as it has
    /// of a drive, each is named for the device it is on as well:
    /// `nvme:nvme1/Composite`.
    fn sensors(&mut self, batch: &mut MetricBatch) {
        let mut chips: Vec<(String, String, std::path::PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.root.sys_path("class/hwmon")) {
            for entry in entries.flatten() {
                let dir = entry.path();
                let Ok(name) = std::fs::read_to_string(dir.join("name")) else {
                    continue;
                };
                let device = std::fs::canonicalize(dir.join("device"))
                    .ok()
                    .and_then(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                chips.push((name.trim().to_string(), device, dir));
            }
        }
        // In an order that is the same after a restart, which the order
        // the kernel numbers them in is not.
        chips.sort();
        for (name, device, dir) in &chips {
            let alone = chips.iter().filter(|(other, _, _)| other == name).count() == 1;
            let chip = if alone || device.is_empty() {
                name.clone()
            } else {
                format!("{name}:{device}")
            };
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut inputs: Vec<String> = entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                .filter(|file| file.ends_with("_input"))
                .collect();
            inputs.sort();
            for file in inputs {
                let channel = file.trim_end_matches("_input");
                let (metric, scale) = if channel.starts_with("temp") {
                    // Thousandths of a degree.
                    ("sys_temp_celsius", 0.001)
                } else if channel.starts_with("fan") {
                    ("sys_fan_rpm", 1.0)
                } else {
                    continue;
                };
                let Some(value) = std::fs::read_to_string(dir.join(&file))
                    .ok()
                    .and_then(|text| text.trim().parse::<f64>().ok())
                else {
                    // A sensor that is there and cannot be read: the
                    // device is asleep, or the channel is not wired.
                    continue;
                };
                let what = std::fs::read_to_string(dir.join(format!("{channel}_label")))
                    .map(|text| text.trim().to_string())
                    .ok()
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| channel.to_string());
                let label = self.sensor_label("sensor", format!("{chip}/{what}"));
                batch.push(metric, &label, value * scale);
            }
        }

        let mut speeds = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.root.sys_path("devices/system/cpu")) {
            for entry in entries.flatten() {
                let speed = std::fs::read_to_string(entry.path().join("cpufreq/scaling_cur_freq"))
                    .ok()
                    .and_then(|text| text.trim().parse::<f64>().ok());
                speeds.extend(speed);
            }
        }
        if !speeds.is_empty() {
            // Kilohertz. The mean is how hard the host is being driven;
            // the highest is how hard its busiest CPU is.
            let mean = speeds.iter().sum::<f64>() / speeds.len() as f64;
            let highest = speeds.iter().copied().fold(0.0, f64::max);
            batch.push("sys_cpu_mhz", &self.none, mean / 1000.0);
            batch.push("sys_cpu_mhz_max", &self.none, highest / 1000.0);
        }
    }

    /// The energy each power domain has used, from the running average
    /// power limit interface. Only root may read it, since what a CPU
    /// draws says something of what it is computing.
    fn energy(&self, reading: &mut Reading) {
        let zones = self.root.sys_path("class/powercap");
        let Ok(entries) = std::fs::read_dir(&zones) else {
            return;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let zone = entry.file_name().to_string_lossy().into_owned();
            let read = |file: &str| std::fs::read_to_string(dir.join(file)).ok();
            let (Some(name), Some(energy)) = (
                read("name"),
                read("energy_uj").and_then(|text| text.trim().parse::<u64>().ok()),
            ) else {
                continue;
            };
            // A domain inside another is named within it: the cores of
            // package 0 are `package-0/core`.
            let parent = zone
                .rsplit_once(':')
                .filter(|(parent, _)| parent.contains(':'))
                .and_then(|(parent, _)| {
                    std::fs::read_to_string(zones.join(parent).join("name")).ok()
                });
            let domain = match parent {
                Some(parent) => format!("{}/{}", parent.trim(), name.trim()),
                None => name.trim().to_string(),
            };
            reading.energy_uj.insert(domain, energy);
        }
    }

    fn power(&mut self, batch: &mut MetricBatch, previous: &Reading, reading: &Reading, s: f64) {
        let mut domains: Vec<&String> = reading.energy_uj.keys().collect();
        domains.sort();
        for domain in domains {
            let Some(before) = previous.energy_uj.get(domain) else {
                continue;
            };
            let label = self.sensor_label("domain", domain.clone());
            // The counter wraps, and a wrapped interval is left out.
            batch.push(
                "sys_power_watts",
                &label,
                rate(reading.energy_uj[domain], *before, s) / 1e6,
            );
        }
    }

    fn filesystems(&mut self, batch: &mut MetricBatch) {
        let Ok(text) = self.root.read("self/mounts") else {
            return;
        };
        for mount in parse_mounts(&text) {
            let Some(usage) = statvfs(&mount.target) else {
                continue;
            };
            let label = self
                .mount_labels
                .entry(mount.target.clone())
                .or_insert_with(|| labels(vec![("mount", mount.target.clone())]))
                .clone();
            batch.push("sys_fs_size_bytes", &label, usage.size);
            batch.push("sys_fs_used_bytes", &label, usage.used);
            batch.push("sys_fs_available_bytes", &label, usage.available);
            // df's figure: what is used, out of what an unprivileged writer
            // could ever use. Blocks reserved for root are in neither.
            batch.push(
                "sys_fs_used_pct",
                &label,
                percent(usage.used, usage.used + usage.available),
            );
            if usage.inodes > 0.0 {
                batch.push(
                    "sys_fs_inodes_used_pct",
                    &label,
                    percent(usage.inodes - usage.inodes_free, usage.inodes),
                );
            }
        }
    }
}

fn push_cpu(batch: &mut MetricBatch, label: &Labels, before: &CpuTimes, now: &CpuTimes) {
    let total = now.total().saturating_sub(before.total()) as f64;
    if total <= 0.0 {
        return;
    }
    let share = |now: u64, before: u64| percent(now.saturating_sub(before) as f64, total);
    let idle = share(now.idle, before.idle);
    let iowait = share(now.iowait, before.iowait);
    batch.push("sys_cpu_user_pct", label, share(now.user, before.user));
    batch.push("sys_cpu_nice_pct", label, share(now.nice, before.nice));
    batch.push(
        "sys_cpu_system_pct",
        label,
        share(now.system, before.system),
    );
    batch.push("sys_cpu_iowait_pct", label, iowait);
    batch.push("sys_cpu_irq_pct", label, share(now.irq, before.irq));
    batch.push(
        "sys_cpu_softirq_pct",
        label,
        share(now.softirq, before.softirq),
    );
    batch.push("sys_cpu_steal_pct", label, share(now.steal, before.steal));
    batch.push("sys_cpu_guest_pct", label, share(now.guest, before.guest));
    batch.push("sys_cpu_idle_pct", label, idle);
    // Waiting on I/O is idle time with a reason attached; a CPU doing it is
    // free to run something else.
    batch.push("sys_cpu_busy_pct", label, (100.0 - idle - iowait).max(0.0));
}

struct Usage {
    size: f64,
    used: f64,
    available: f64,
    inodes: f64,
    inodes_free: f64,
}

fn statvfs(path: &str) -> Option<Usage> {
    let path = CString::new(path).ok()?;
    // SAFETY: statvfs is plain data; the call fills it or fails.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if rc != 0 || stat.f_blocks == 0 {
        return None;
    }
    let block = stat.f_frsize as f64;
    Some(Usage {
        size: stat.f_blocks as f64 * block,
        used: stat.f_blocks.saturating_sub(stat.f_bfree) as f64 * block,
        available: stat.f_bavail as f64 * block,
        inodes: stat.f_files as f64,
        inodes_free: stat.f_ffree as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::time::Duration;

    fn value(batch: &MetricBatch, name: &str, label: Option<(&str, &str)>) -> Option<f64> {
        batch
            .samples
            .iter()
            .find(|sample| {
                sample.name == name
                    && match label {
                        None => sample.labels.is_empty(),
                        Some((key, want)) => {
                            sample.labels.iter().any(|(k, v)| *k == key && v == want)
                        }
                    }
            })
            .map(|sample| sample.value)
    }

    fn write_reading(fixture: &Fixture, scale: u64) {
        // scale 0 is the first reading; each step adds ten seconds of work.
        let t = scale;
        fixture.proc_file(
            "stat",
            &format!(
                "cpu  {} 0 {} {} {} 0 0 0 0 0\n\
                 cpu0 {} 0 {} {} {} 0 0 0 0 0\n\
                 intr {} 0\nctxt {}\nbtime 1753000000\nprocesses {}\n\
                 procs_running 2\nprocs_blocked 0\nsoftirq {} 0\n",
                1000 + 300 * t,
                500 + 100 * t,
                8000 + 500 * t,
                100 + 100 * t,
                1000 + 300 * t,
                500 + 100 * t,
                8000 + 500 * t,
                100 + 100 * t,
                5000 + 2000 * t,
                9000 + 30_000 * t,
                400 + 50 * t,
                700 + 1000 * t,
            ),
        );
        fixture.proc_file("loadavg", "1.50 1.25 1.00 3/900 4242\n");
        fixture.proc_file("uptime", "86400.50 170000.00\n");
        fixture.proc_file(
            "meminfo",
            "MemTotal: 16000000 kB\nMemFree: 2000000 kB\nMemAvailable: 12000000 kB\n\
             Buffers: 100000 kB\nCached: 5000000 kB\nSwapTotal: 8000000 kB\n\
             SwapFree: 6000000 kB\nCommitted_AS: 12000000 kB\n",
        );
        fixture.proc_file(
            "vmstat",
            &format!(
                "pgpgin {}\npgpgout {}\npgfault {}\npgmajfault {}\npswpin 0\npswpout 0\n\
                 pgscan_kswapd {}\npgscan_direct {}\npgsteal_kswapd {}\n",
                1000 + 100 * t,
                2000 + 400 * t,
                50_000 + 10_000 * t,
                10 + 20 * t,
                100 + 30 * t,
                10 + 20 * t,
                90 + 40 * t,
            ),
        );
        fixture.proc_file(
            "diskstats",
            &format!(
                " 259 0 nvme0n1 {} 0 {} {} {} 0 {} {} 0 {} {} 0 0 0 0\n\
                 \x20259 1 nvme0n1p1 9 0 9 9 9 0 9 9 0 9 9\n\
                 \x20  7 0 loop0 5 0 5 5 5 0 5 5 0 5 5\n",
                100 + 50 * t,
                8000 + 4000 * t,
                200 + 100 * t,
                300 + 150 * t,
                16_000 + 2000 * t,
                900 + 300 * t,
                700 + 2500 * t,
                1100 + 5000 * t,
            ),
        );
        fixture.sys_dir("block/nvme0n1");
        fixture.sys_dir("block/loop0");
        fixture.proc_file(
            "net/dev",
            &format!(
                "Inter-| Receive | Transmit\n face |bytes packets|bytes packets\n\
                 \x20eth0: {} {} 0 0 0 0 0 0 {} {} 0 0 0 0 0 0\n\
                 veth12ab: 1 1 0 0 0 0 0 0 1 1 0 0 0 0 0 0\n",
                10_000 + 50_000 * t,
                100 + 400 * t,
                20_000 + 10_000 * t,
                200 + 100 * t,
            ),
        );
        fixture.proc_file(
            "net/snmp",
            &format!(
                "Tcp: MaxConn ActiveOpens RetransSegs CurrEstab\nTcp: -1 {} {} 12\n",
                40 + 20 * t,
                7 + 5 * t
            ),
        );
        fixture.proc_file(
            "net/sockstat",
            "sockets: used 300\nTCP: inuse 5 orphan 0 tw 1\n",
        );
        fixture.proc_file("sys/fs/file-nr", "5000\t0\t100000\n");
        fixture.proc_file(
            "pressure/io",
            &format!(
                "some avg10=2.50 avg60=1.00 avg300=0.50 total={}\n\
                 full avg10=1.25 avg60=0.50 avg300=0.25 total={}\n",
                1_000_000 + 500_000 * t,
                400_000 + 100_000 * t,
            ),
        );
    }

    #[test]
    fn the_first_reading_reports_levels_and_the_second_adds_rates() {
        let fixture = Fixture::new("system_rates");
        let mut collector = SystemCollector::new(fixture.root(), SystemOptions::default());
        let start = Instant::now();

        write_reading(&fixture, 0);
        let mut first = MetricBatch::new(100);
        collector.collect(start, &mut first);
        assert_eq!(value(&first, "sys_load1", None), Some(1.5));
        assert_eq!(value(&first, "sys_tasks", None), Some(900.0));
        assert_eq!(value(&first, "sys_mem_used_pct", None), Some(25.0));
        assert_eq!(value(&first, "sys_swap_used_pct", None), Some(25.0));
        assert_eq!(value(&first, "sys_mem_committed_pct", None), Some(50.0));
        assert_eq!(value(&first, "sys_file_handles_pct", None), Some(5.0));
        assert_eq!(value(&first, "sys_tcp_connections", None), Some(12.0));
        assert_eq!(value(&first, "sys_pressure_io_some_avg10", None), Some(2.5));
        assert_eq!(
            value(&first, "sys_cpu_busy_pct", Some(("cpu", "all"))),
            None
        );
        assert_eq!(value(&first, "sys_forks_per_sec", None), None);
        assert_eq!(collector.boot_time(), Some(1_753_000_000));

        write_reading(&fixture, 1);
        let mut second = MetricBatch::new(110);
        collector.collect(start + Duration::from_secs(10), &mut second);

        // 300 user + 100 system + 500 idle + 100 iowait ticks in the interval.
        let all = Some(("cpu", "all"));
        assert_eq!(value(&second, "sys_cpu_user_pct", all), Some(30.0));
        assert_eq!(value(&second, "sys_cpu_system_pct", all), Some(10.0));
        assert_eq!(value(&second, "sys_cpu_iowait_pct", all), Some(10.0));
        assert_eq!(value(&second, "sys_cpu_idle_pct", all), Some(50.0));
        assert_eq!(value(&second, "sys_cpu_busy_pct", all), Some(40.0));
        assert_eq!(
            value(&second, "sys_cpu_user_pct", Some(("cpu", "0"))),
            Some(30.0)
        );

        assert_eq!(value(&second, "sys_forks_per_sec", None), Some(5.0));
        assert_eq!(
            value(&second, "sys_context_switches_per_sec", None),
            Some(3000.0)
        );
        assert_eq!(value(&second, "sys_interrupts_per_sec", None), Some(200.0));
        assert_eq!(value(&second, "sys_softirqs_per_sec", None), Some(100.0));
        assert_eq!(
            value(&second, "sys_tcp_active_opens_per_sec", None),
            Some(2.0)
        );
        assert_eq!(
            value(&second, "sys_tcp_retransmits_per_sec", None),
            Some(0.5)
        );

        assert_eq!(
            value(&second, "sys_page_in_bytes_per_sec", None),
            Some(10_240.0)
        );
        assert_eq!(
            value(&second, "sys_page_out_bytes_per_sec", None),
            Some(40_960.0)
        );
        assert_eq!(
            value(&second, "sys_page_faults_per_sec", None),
            Some(1000.0)
        );
        assert_eq!(value(&second, "sys_major_faults_per_sec", None), Some(2.0));
        assert_eq!(value(&second, "sys_pages_scanned_per_sec", None), Some(5.0));
        assert_eq!(
            value(&second, "sys_pages_reclaimed_per_sec", None),
            Some(4.0)
        );

        let dev = Some(("dev", "nvme0n1"));
        assert_eq!(value(&second, "sys_disk_reads_per_sec", dev), Some(5.0));
        assert_eq!(value(&second, "sys_disk_writes_per_sec", dev), Some(15.0));
        assert_eq!(
            value(&second, "sys_disk_read_bytes_per_sec", dev),
            Some(204_800.0)
        );
        assert_eq!(
            value(&second, "sys_disk_write_bytes_per_sec", dev),
            Some(102_400.0)
        );
        assert_eq!(value(&second, "sys_disk_util_pct", dev), Some(25.0));
        assert_eq!(value(&second, "sys_disk_queue_depth", dev), Some(0.5));
        // 100 + 300 ms waited over 50 + 150 requests.
        assert_eq!(value(&second, "sys_disk_await_ms", dev), Some(2.0));
        assert_eq!(value(&second, "sys_io_reads_per_sec", None), Some(5.0));
        // Partitions and loop devices are not devices to report.
        assert_eq!(
            value(
                &second,
                "sys_disk_reads_per_sec",
                Some(("dev", "nvme0n1p1"))
            ),
            None
        );
        assert_eq!(
            value(&second, "sys_disk_reads_per_sec", Some(("dev", "loop0"))),
            None
        );

        let eth = Some(("iface", "eth0"));
        assert_eq!(
            value(&second, "sys_net_rx_bytes_per_sec", eth),
            Some(5000.0)
        );
        assert_eq!(
            value(&second, "sys_net_tx_bytes_per_sec", eth),
            Some(1000.0)
        );
        assert_eq!(
            value(&second, "sys_net_rx_packets_per_sec", eth),
            Some(40.0)
        );
        assert_eq!(
            value(
                &second,
                "sys_net_rx_bytes_per_sec",
                Some(("iface", "veth12ab"))
            ),
            None
        );

        // Half a second stalled in ten is five percent.
        assert_eq!(value(&second, "sys_pressure_io_some_pct", None), Some(5.0));
        assert_eq!(value(&second, "sys_pressure_io_full_pct", None), Some(1.0));
    }

    #[test]
    fn a_counter_reset_leaves_a_gap_instead_of_a_spike() {
        let fixture = Fixture::new("system_reset");
        let mut collector = SystemCollector::new(fixture.root(), SystemOptions::default());
        let start = Instant::now();

        write_reading(&fixture, 1);
        collector.collect(start, &mut MetricBatch::new(100));
        // The counters are now lower than they were: a reboot's worth.
        write_reading(&fixture, 0);
        let mut batch = MetricBatch::new(110);
        collector.collect(start + Duration::from_secs(10), &mut batch);

        assert_eq!(value(&batch, "sys_forks_per_sec", None), None);
        assert_eq!(
            value(&batch, "sys_net_rx_bytes_per_sec", Some(("iface", "eth0"))),
            None
        );
        assert_eq!(
            value(&batch, "sys_disk_reads_per_sec", Some(("dev", "nvme0n1"))),
            None
        );
        // Levels do not depend on the previous reading.
        assert_eq!(value(&batch, "sys_load1", None), Some(1.5));
    }

    #[test]
    fn sensors_are_named_for_their_chip_and_what_it_calls_them() {
        let fixture = Fixture::new("system_sensors");
        // Numbered by the kernel in the order they were found, which says
        // nothing of which is which.
        for (hwmon, name, device) in [
            ("hwmon3", "nvme", "pci/nvme1"),
            ("hwmon0", "coretemp", "platform/coretemp.0"),
            ("hwmon1", "nvme", "pci/nvme0"),
        ] {
            fixture.sys_file(&format!("devices/{device}/present"), "");
            fixture.sys_file(&format!("class/hwmon/{hwmon}/name"), &format!("{name}\n"));
            fixture.sys_link(
                &format!("class/hwmon/{hwmon}/device"),
                &format!("../../../devices/{device}"),
            );
        }
        let chip = |hwmon: &str, file: &str, content: &str| {
            fixture.sys_file(&format!("class/hwmon/{hwmon}/{file}"), content);
        };
        chip("hwmon0", "temp1_input", "56000\n");
        chip("hwmon0", "temp1_label", "Package id 0\n");
        chip("hwmon0", "temp2_input", "54500\n");
        chip("hwmon0", "temp2_label", "Core 0\n");
        chip("hwmon0", "temp1_max", "100000\n");
        chip("hwmon0", "fan1_input", "1200\n");
        chip("hwmon1", "temp1_input", "32850\n");
        chip("hwmon1", "temp1_label", "Composite\n");
        // No label: the channel is what it is called.
        chip("hwmon3", "temp1_input", "30850\n");
        for (cpu, speed) in [("cpu0", "4400000"), ("cpu1", "800000")] {
            fixture.sys_file(
                &format!("devices/system/cpu/{cpu}/cpufreq/scaling_cur_freq"),
                speed,
            );
        }
        fixture.sys_dir("devices/system/cpu/cpufreq");

        let mut collector = SystemCollector::new(fixture.root(), SystemOptions::default());
        let mut batch = MetricBatch::new(1);
        collector.collect(Instant::now(), &mut batch);

        let sensor = |name, sensor| value(&batch, name, Some(("sensor", sensor)));
        assert_eq!(
            sensor("sys_temp_celsius", "coretemp/Package id 0"),
            Some(56.0)
        );
        assert_eq!(sensor("sys_temp_celsius", "coretemp/Core 0"), Some(54.5));
        assert_eq!(sensor("sys_fan_rpm", "coretemp/fan1"), Some(1200.0));
        // Two of a chip: each is named for the device it is on.
        assert_eq!(
            sensor("sys_temp_celsius", "nvme:nvme0/Composite"),
            Some(32.85)
        );
        assert_eq!(sensor("sys_temp_celsius", "nvme:nvme1/temp1"), Some(30.85));
        // A limit is not a reading.
        assert_eq!(
            batch
                .samples
                .iter()
                .filter(|s| s.name == "sys_temp_celsius")
                .count(),
            4
        );
        assert_eq!(value(&batch, "sys_cpu_mhz", None), Some(2600.0));
        assert_eq!(value(&batch, "sys_cpu_mhz_max", None), Some(4400.0));
    }

    #[test]
    fn power_is_the_energy_used_over_the_interval() {
        let fixture = Fixture::new("system_power");
        let zone = |zone: &str, name: &str, energy: u64| {
            fixture.sys_file(&format!("class/powercap/{zone}/name"), &format!("{name}\n"));
            fixture.sys_file(
                &format!("class/powercap/{zone}/energy_uj"),
                &format!("{energy}\n"),
            );
        };
        // The control itself has a name and no energy.
        fixture.sys_file("class/powercap/intel-rapl/enabled", "1\n");
        let mut collector = SystemCollector::new(fixture.root(), SystemOptions::default());
        let start = Instant::now();

        zone("intel-rapl:0", "package-0", 1_000_000_000);
        zone("intel-rapl:0:0", "core", 400_000_000);
        let mut first = MetricBatch::new(1);
        collector.collect(start, &mut first);
        assert!(first.samples.iter().all(|s| s.name != "sys_power_watts"));

        zone("intel-rapl:0", "package-0", 1_250_000_000);
        zone("intel-rapl:0:0", "core", 500_000_000);
        let mut second = MetricBatch::new(2);
        collector.collect(start + Duration::from_secs(10), &mut second);
        let watts = |domain| value(&second, "sys_power_watts", Some(("domain", domain)));
        // 250 joules in ten seconds.
        assert_eq!(watts("package-0"), Some(25.0));
        assert_eq!(watts("package-0/core"), Some(10.0));

        // The counter wraps: the interval is left out, and not a spike.
        zone("intel-rapl:0", "package-0", 5_000);
        let mut third = MetricBatch::new(3);
        collector.collect(start + Duration::from_secs(20), &mut third);
        assert_eq!(
            value(&third, "sys_power_watts", Some(("domain", "package-0"))),
            None
        );
    }

    #[test]
    fn a_host_without_swap_uses_zero_percent_of_it() {
        let fixture = Fixture::new("system_noswap");
        fixture.proc_file(
            "meminfo",
            "MemTotal: 1000 kB\nMemAvailable: 500 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        );
        let mut collector = SystemCollector::new(fixture.root(), SystemOptions::default());
        let mut batch = MetricBatch::new(1);
        collector.collect(Instant::now(), &mut batch);
        assert_eq!(value(&batch, "sys_swap_used_pct", None), Some(0.0));
    }
}
