//! Parsers for the system-wide files sar reads.

use std::collections::HashMap;

/// Ticks of CPU time by mode, as `/proc/stat` reports them, with guest time
/// moved out of `user` and `nice` (the kernel counts it in both places and
/// sar reports it once).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub guest: u64,
}

impl CpuTimes {
    pub fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
            + self.guest
    }
}

#[derive(Debug, Clone, Default)]
pub struct Stat {
    pub all: CpuTimes,
    /// `(cpu number, times)` for every online CPU.
    pub cpus: Vec<(u32, CpuTimes)>,
    pub interrupts: u64,
    pub softirqs: u64,
    pub context_switches: u64,
    pub forks: u64,
    pub boot_time: u64,
    pub procs_running: u64,
    pub procs_blocked: u64,
}

pub fn parse_stat(text: &str) -> Stat {
    let mut stat = Stat::default();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(key) = fields.next() else { continue };
        if let Some(cpu) = key.strip_prefix("cpu") {
            let times = parse_cpu_times(fields);
            if cpu.is_empty() {
                stat.all = times;
            } else if let Ok(number) = cpu.parse() {
                stat.cpus.push((number, times));
            }
            continue;
        }
        let first = fields.next().and_then(|f| f.parse::<u64>().ok());
        match (key, first) {
            ("intr", Some(v)) => stat.interrupts = v,
            ("softirq", Some(v)) => stat.softirqs = v,
            ("ctxt", Some(v)) => stat.context_switches = v,
            ("processes", Some(v)) => stat.forks = v,
            ("btime", Some(v)) => stat.boot_time = v,
            ("procs_running", Some(v)) => stat.procs_running = v,
            ("procs_blocked", Some(v)) => stat.procs_blocked = v,
            _ => {}
        }
    }
    stat
}

fn parse_cpu_times<'a>(fields: impl Iterator<Item = &'a str>) -> CpuTimes {
    let v: Vec<u64> = fields.map(|f| f.parse().unwrap_or(0)).collect();
    let at = |index: usize| v.get(index).copied().unwrap_or(0);
    let guest = at(8);
    let guest_nice = at(9);
    CpuTimes {
        user: at(0).saturating_sub(guest),
        nice: at(1).saturating_sub(guest_nice),
        system: at(2),
        idle: at(3),
        iowait: at(4),
        irq: at(5),
        softirq: at(6),
        steal: at(7),
        guest: guest + guest_nice,
    }
}

/// `Key: value [kB]` files: meminfo. Values keep the file's own unit.
pub fn parse_keyed(text: &str) -> HashMap<&str, u64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(value) = rest
            .split_ascii_whitespace()
            .next()
            .and_then(|f| f.parse().ok())
        {
            map.insert(key.trim(), value);
        }
    }
    map
}

/// `key value` files: vmstat.
pub fn parse_pairs(text: &str) -> HashMap<&str, u64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        if let (Some(key), Some(value)) = (fields.next(), fields.next()) {
            if let Ok(value) = value.parse() {
                map.insert(key, value);
            }
        }
    }
    map
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadAvg {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    /// Scheduling entities that exist: processes and threads together.
    pub tasks: u64,
}

pub fn parse_loadavg(text: &str) -> Option<LoadAvg> {
    let mut fields = text.split_ascii_whitespace();
    let load1 = fields.next()?.parse().ok()?;
    let load5 = fields.next()?.parse().ok()?;
    let load15 = fields.next()?.parse().ok()?;
    let tasks = fields.next()?.split_once('/')?.1.parse().ok()?;
    Some(LoadAvg {
        load1,
        load5,
        load15,
        tasks,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Disk {
    pub name: String,
    pub reads: u64,
    pub sectors_read: u64,
    pub read_ms: u64,
    pub writes: u64,
    pub sectors_written: u64,
    pub write_ms: u64,
    pub in_flight: u64,
    pub busy_ms: u64,
    pub weighted_ms: u64,
}

/// Every line of diskstats, partitions included; the caller decides which
/// names are whole devices.
pub fn parse_diskstats(text: &str) -> Vec<Disk> {
    let mut disks = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        if fields.len() < 14 {
            continue;
        }
        let at = |index: usize| fields[index].parse::<u64>().unwrap_or(0);
        disks.push(Disk {
            name: fields[2].to_string(),
            reads: at(3),
            sectors_read: at(5),
            read_ms: at(6),
            writes: at(7),
            sectors_written: at(9),
            write_ms: at(10),
            in_flight: at(11),
            busy_ms: at(12),
            weighted_ms: at(13),
        });
    }
    disks
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetDev {
    pub name: String,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}

pub fn parse_net_dev(text: &str) -> Vec<NetDev> {
    let mut devices = Vec::new();
    for line in text.lines() {
        // The two header lines have no "name:" prefix followed by numbers.
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
        if fields.len() < 16 || fields[0].parse::<u64>().is_err() {
            continue;
        }
        let at = |index: usize| fields[index].parse::<u64>().unwrap_or(0);
        devices.push(NetDev {
            name: name.trim().to_string(),
            rx_bytes: at(0),
            rx_packets: at(1),
            rx_errors: at(2),
            rx_dropped: at(3),
            tx_bytes: at(8),
            tx_packets: at(9),
            tx_errors: at(10),
            tx_dropped: at(11),
        });
    }
    devices
}

/// One line of a pressure file: the share of time stalled and the running
/// total of stalled microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PressureLine {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_us: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pressure {
    pub some: Option<PressureLine>,
    pub full: Option<PressureLine>,
}

pub fn parse_pressure(text: &str) -> Pressure {
    let mut pressure = Pressure::default();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let kind = fields.next();
        let mut parsed = PressureLine::default();
        for field in fields {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "avg10" => parsed.avg10 = value.parse().unwrap_or(0.0),
                "avg60" => parsed.avg60 = value.parse().unwrap_or(0.0),
                "avg300" => parsed.avg300 = value.parse().unwrap_or(0.0),
                "total" => parsed.total_us = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        match kind {
            Some("some") => pressure.some = Some(parsed),
            Some("full") => pressure.full = Some(parsed),
            _ => {}
        }
    }
    pressure
}

/// `Proto: key value key value` lines: sockstat. Keys come back as
/// `"TCP.inuse"`.
pub fn parse_sockstat(text: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((proto, rest)) = line.split_once(':') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
        for pair in fields.as_chunks::<2>().0 {
            if let Ok(value) = pair[1].parse() {
                map.insert(format!("{}.{}", proto.trim(), pair[0]), value);
            }
        }
    }
    map
}

/// Header-line-then-value-line files: `/proc/net/snmp`. Keys come back as
/// `"Tcp.RetransSegs"`. Values are signed because a few are (`MaxConn -1`).
pub fn parse_snmp(text: &str) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    let mut lines = text.lines();
    while let (Some(header), Some(values)) = (lines.next(), lines.next()) {
        let (Some((proto, names)), Some((_, values))) =
            (header.split_once(':'), values.split_once(':'))
        else {
            continue;
        };
        for (name, value) in names
            .split_ascii_whitespace()
            .zip(values.split_ascii_whitespace())
        {
            if let Ok(value) = value.parse() {
                map.insert(format!("{proto}.{name}"), value);
            }
        }
    }
    map
}

/// The first N whitespace-separated integers of a one-line file
/// (`file-nr`, `inode-nr`, `dentry-state`).
pub fn parse_numbers(text: &str) -> Vec<u64> {
    text.split_ascii_whitespace()
        .map_while(|f| f.parse().ok())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub fstype: String,
}

/// Filesystems that hold data on a device or a server, as opposed to the
/// kernel's own views (proc, sysfs, cgroup) and memory (tmpfs).
const REAL_FILESYSTEMS: &[&str] = &[
    "bcachefs", "btrfs", "exfat", "ext2", "ext3", "ext4", "f2fs", "jfs", "nfs", "nfs4", "ntfs",
    "ntfs3", "reiserfs", "vfat", "xfs", "zfs",
];

/// Real filesystems, one entry per backing source. A btrfs volume mounted
/// at six subvolume paths is one filesystem with one fill level; it is
/// reported once, at its shortest mount point.
pub fn parse_mounts(text: &str) -> Vec<Mount> {
    let mut by_source: HashMap<String, Mount> = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(source), Some(target), Some(fstype)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !REAL_FILESYSTEMS.contains(&fstype) {
            continue;
        }
        let mount = Mount {
            source: source.to_string(),
            target: unescape_mount(target),
            fstype: fstype.to_string(),
        };
        match by_source.get(source) {
            Some(existing) if existing.target.len() <= mount.target.len() => {}
            _ => {
                by_source.insert(source.to_string(), mount);
            }
        }
    }
    let mut mounts: Vec<Mount> = by_source.into_values().collect();
    mounts.sort_by(|a, b| a.target.cmp(&b.target));
    mounts
}

/// Mount paths escape space, tab, newline, and backslash as octal.
fn unescape_mount(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let octal = &path[index + 1..index + 4];
            if let Ok(value) = u8::from_str_radix(octal, 8) {
                out.push(value);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A kernel CPU list (`0-21`, `0-3,8-11`) as the number of CPUs it names.
pub fn cpu_list_len(text: &str) -> usize {
    text.trim()
        .split(',')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('-') {
            Some((low, high)) => {
                let low: usize = low.parse().unwrap_or(0);
                let high: usize = high.parse().unwrap_or(low);
                high.saturating_sub(low) + 1
            }
            None => 1,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "\
cpu  1000 50 300 8000 120 10 20 5 100 10
cpu0 600 30 200 4000 60 5 10 3 60 5
cpu1 400 20 100 4000 60 5 10 2 40 5
intr 123456 1 2 3
ctxt 987654
btime 1753000000
processes 4321
procs_running 3
procs_blocked 1
softirq 55555 1 2 3
";

    #[test]
    fn stat_separates_guest_time_from_user_and_nice() {
        let stat = parse_stat(STAT);
        assert_eq!(stat.all.user, 900);
        assert_eq!(stat.all.nice, 40);
        assert_eq!(stat.all.guest, 110);
        assert_eq!(
            stat.all.total(),
            900 + 40 + 300 + 8000 + 120 + 10 + 20 + 5 + 110
        );
        assert_eq!(stat.cpus.len(), 2);
        assert_eq!(stat.cpus[1].0, 1);
        assert_eq!(stat.cpus[1].1.system, 100);
        assert_eq!(stat.interrupts, 123_456);
        assert_eq!(stat.softirqs, 55_555);
        assert_eq!(stat.context_switches, 987_654);
        assert_eq!(stat.forks, 4321);
        assert_eq!(stat.boot_time, 1_753_000_000);
        assert_eq!(stat.procs_running, 3);
        assert_eq!(stat.procs_blocked, 1);
    }

    #[test]
    fn keyed_files_keep_their_own_units() {
        let map = parse_keyed("MemTotal:       32000000 kB\nHugePages_Total:       4\n");
        assert_eq!(map["MemTotal"], 32_000_000);
        assert_eq!(map["HugePages_Total"], 4);
    }

    #[test]
    fn pair_files_parse() {
        let map = parse_pairs("pgfault 100\npgmajfault 3\nnot_a_number x\n");
        assert_eq!(map["pgfault"], 100);
        assert_eq!(map["pgmajfault"], 3);
        assert!(!map.contains_key("not_a_number"));
    }

    #[test]
    fn loadavg_reads_the_task_count_after_the_slash() {
        let load = parse_loadavg("0.52 0.41 0.38 2/1873 99123\n").unwrap();
        assert_eq!(load.load1, 0.52);
        assert_eq!(load.load15, 0.38);
        assert_eq!(load.tasks, 1873);
        assert!(parse_loadavg("garbage").is_none());
    }

    #[test]
    fn diskstats_reads_the_classic_fields() {
        let disks = parse_diskstats(
            " 259       0 nvme0n1 100 5 8000 250 200 10 16000 900 2 700 1150 0 0 0 0 0 0\n\
             \x20259       1 nvme0n1p1 10 0 800 25 0 0 0 0 0 20 25\n",
        );
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].name, "nvme0n1");
        assert_eq!(disks[0].reads, 100);
        assert_eq!(disks[0].sectors_read, 8000);
        assert_eq!(disks[0].writes, 200);
        assert_eq!(disks[0].sectors_written, 16_000);
        assert_eq!(disks[0].in_flight, 2);
        assert_eq!(disks[0].busy_ms, 700);
        assert_eq!(disks[0].weighted_ms, 1150);
    }

    #[test]
    fn net_dev_skips_headers_and_reads_both_directions() {
        let devices = parse_net_dev(
            "Inter-|   Receive                                                |  Transmit\n\
             \x20face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
             \x20   lo: 1000 10 0 0 0 0 0 0 1000 10 0 0 0 0 0 0\n\
             \x20 eth0: 5000 50 1 2 0 0 0 0 7000 70 3 4 0 0 0 0\n",
        );
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[1].name, "eth0");
        assert_eq!(devices[1].rx_bytes, 5000);
        assert_eq!(devices[1].rx_errors, 1);
        assert_eq!(devices[1].rx_dropped, 2);
        assert_eq!(devices[1].tx_bytes, 7000);
        assert_eq!(devices[1].tx_errors, 3);
        assert_eq!(devices[1].tx_dropped, 4);
    }

    #[test]
    fn pressure_reads_both_lines() {
        let pressure = parse_pressure(
            "some avg10=1.50 avg60=0.75 avg300=0.10 total=123456\n\
             full avg10=0.50 avg60=0.25 avg300=0.05 total=6543\n",
        );
        assert_eq!(pressure.some.unwrap().avg10, 1.5);
        assert_eq!(pressure.some.unwrap().total_us, 123_456);
        assert_eq!(pressure.full.unwrap().avg300, 0.05);
        // The cpu file has carried only a "some" line on older kernels.
        assert!(
            parse_pressure("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n")
                .full
                .is_none()
        );
    }

    #[test]
    fn sockstat_keys_are_protocol_qualified() {
        let map = parse_sockstat(
            "sockets: used 300\nTCP: inuse 5 orphan 0 tw 1 alloc 8 mem 2\nUDP: inuse 3 mem 1\n",
        );
        assert_eq!(map["sockets.used"], 300);
        assert_eq!(map["TCP.inuse"], 5);
        assert_eq!(map["TCP.tw"], 1);
        assert_eq!(map["UDP.inuse"], 3);
    }

    #[test]
    fn snmp_pairs_header_lines_with_value_lines() {
        let map = parse_snmp(
            "Tcp: RtoAlgorithm MaxConn ActiveOpens RetransSegs\n\
             Tcp: 1 -1 40 7\n\
             Udp: InDatagrams NoPorts\n\
             Udp: 900 2\n",
        );
        assert_eq!(map["Tcp.MaxConn"], -1);
        assert_eq!(map["Tcp.ActiveOpens"], 40);
        assert_eq!(map["Tcp.RetransSegs"], 7);
        assert_eq!(map["Udp.InDatagrams"], 900);
    }

    #[test]
    fn mounts_report_each_real_filesystem_once() {
        let mounts = parse_mounts(
            "proc /proc proc rw 0 0\n\
             /dev/nvme0n1p2 /home btrfs rw,subvol=/@home 0 0\n\
             /dev/nvme0n1p2 / btrfs rw,subvol=/@ 0 0\n\
             /dev/nvme0n1p2 /var/log btrfs rw,subvol=/@log 0 0\n\
             tmpfs /tmp tmpfs rw 0 0\n\
             /dev/nvme0n1p1 /boot vfat rw 0 0\n\
             /dev/sdb1 /mnt/my\\040disk ext4 rw 0 0\n",
        );
        let targets: Vec<&str> = mounts.iter().map(|m| m.target.as_str()).collect();
        assert_eq!(targets, ["/", "/boot", "/mnt/my disk"]);
    }

    #[test]
    fn cpu_lists_count_ranges_and_singles() {
        assert_eq!(cpu_list_len("0-21\n"), 22);
        assert_eq!(cpu_list_len("0-3,8-11"), 8);
        assert_eq!(cpu_list_len("0,2,4"), 3);
        assert_eq!(cpu_list_len("0"), 1);
    }

    #[test]
    fn numbers_stop_at_the_first_non_number() {
        assert_eq!(
            parse_numbers("4512\t0\t9223372036854775807\n"),
            [4512, 0, 9_223_372_036_854_775_807]
        );
    }
}
