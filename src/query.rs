//! Reading a local store back: the processes of a past moment, and the
//! accounting records of those that ended.
//!
//! These read what has been flushed. A collector that is running holds up
//! to a flush interval of samples in memory that are not visible here yet.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::types::Value as Sql;
use rusqlite::{params_from_iter, Connection, OpenFlags};
use serde_json::Value;

use crate::accounting::{human_bytes, human_duration};
use crate::cli::{ExitsArgs, SummaryBy, TopArgs, TopSort, TreesArgs};
use crate::clock;
use crate::lineage::hex;
use crate::sink::embedded::{
    LOGS_DB, LOGS_TABLE, METRICS_DB, METRICS_TABLE, TRACES_DB, TRACES_TABLE,
};
use crate::taskstats::epoch_now;

pub(crate) fn open(dir: &Path, name: &str) -> Result<Connection> {
    let path = dir.join(name);
    if !path.exists() {
        bail!(
            "{} does not exist: is {} a store's directory?",
            path.display(),
            dir.display()
        );
    }
    // Without the owner lease: a reader takes nothing from the collector.
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", path.display()))?;
    connection.execute_batch("PRAGMA busy_timeout = 5000;")?;
    timeless_ext::register_telemetry(&connection)
        .map_err(|error| anyhow::anyhow!("register the timeless engine: {error}"))?;
    Ok(connection)
}

/// The latest value of every series of `metric` within the window, keyed
/// by the value of `key` among its labels.
fn latest(
    connection: &Connection,
    metric: &str,
    key: &str,
    host: Option<&str>,
    start: i64,
    stop: i64,
) -> Result<HashMap<String, (Value, f64)>> {
    let filter = host.map(|host| serde_json::json!({ "host": host }).to_string());
    let mut statement = connection
        .prepare_cached("SELECT labels, value FROM timeless_latest(?1, ?2, ?3, ?4, ?5)")?;
    let rows = statement.query_map(
        rusqlite::params![METRICS_TABLE, metric, filter, start, stop],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
    )?;
    let mut out = HashMap::new();
    for row in rows {
        let (labels, value) = row?;
        let labels: Value = serde_json::from_str(&labels)?;
        let id = format!(
            "{}\u{0}{}",
            labels["host"].as_str().unwrap_or(""),
            labels[key].as_str().unwrap_or("")
        );
        out.insert(id, (labels, value));
    }
    Ok(out)
}

fn single(
    connection: &Connection,
    metric: &str,
    host: Option<&str>,
    start: i64,
    stop: i64,
) -> Option<f64> {
    let values = latest(connection, metric, "cpu", host, start, stop).ok()?;
    // Series without the label, or the total among those with it.
    values
        .values()
        .find(|(labels, _)| {
            labels
                .get("cpu")
                .is_none_or(|cpu| cpu.as_str() == Some("all"))
        })
        .map(|(_, value)| *value)
}

pub fn top(args: &TopArgs) -> Result<()> {
    let now = epoch_now();
    let at = clock::parse(&args.at, now)?;
    let within = clock::parse_span(&args.within)?;
    let (start, stop) = ((at - within).floor() as i64, at.ceil() as i64);
    let host = args.host.as_deref();
    let connection = open(&args.data_dir, METRICS_DB)?;

    let cpu = latest(&connection, "proc_cpu_pct", "proc", host, start, stop)?;
    if cpu.is_empty() {
        bail!(
            "no process samples between {} and {}.\n\
             Samples become visible when the collector flushes, once a minute by default.",
            clock::format(start as f64),
            clock::format(stop as f64)
        );
    }
    let of = |metric: &str| latest(&connection, metric, "proc", host, start, stop);
    let rss = of("proc_rss_bytes")?;
    let threads = of("proc_threads")?;
    let read = of("proc_io_read_bytes_per_sec")?;
    let write = of("proc_io_write_bytes_per_sec")?;
    let seconds = of("proc_cpu_seconds")?;

    struct Row {
        labels: Value,
        cpu: f64,
        rss: f64,
        io: f64,
        id: String,
    }
    let value = |map: &HashMap<String, (Value, f64)>, id: &str| map.get(id).map(|(_, v)| *v);
    let mut rows: Vec<Row> = cpu
        .into_iter()
        .map(|(id, (labels, cpu))| Row {
            cpu,
            rss: value(&rss, &id).unwrap_or(0.0),
            io: value(&read, &id).unwrap_or(0.0) + value(&write, &id).unwrap_or(0.0),
            labels,
            id,
        })
        .collect();
    let by = |row: &Row| match args.sort {
        TopSort::Cpu => row.cpu,
        TopSort::Rss => row.rss,
        TopSort::Io => row.io,
    };
    rows.sort_by(|a, b| by(b).total_cmp(&by(a)).then_with(|| a.id.cmp(&b.id)));

    let one = |metric: &str| single(&connection, metric, host, start, stop);
    println!("{}  ({} processes)", clock::format(at), rows.len());
    let mut summary = Vec::new();
    if let (Some(a), Some(b), Some(c)) = (one("sys_load1"), one("sys_load5"), one("sys_load15")) {
        summary.push(format!("load {a:.2} {b:.2} {c:.2}"));
    }
    if let Some(busy) = one("sys_cpu_busy_pct") {
        summary.push(format!("cpu {busy:.1}%"));
    }
    if let (Some(used), Some(total)) = (one("sys_mem_used_bytes"), one("sys_mem_total_bytes")) {
        summary.push(format!(
            "mem {} of {}",
            human_bytes(used as u64),
            human_bytes(total as u64)
        ));
    }
    if !summary.is_empty() {
        println!("{}", summary.join("   "));
    }
    println!();
    println!(
        "{:>8} {:<10} {:>6} {:>9} {:>4} {:>10} {:>10} {:>9}  COMMAND",
        "PID", "USER", "CPU%", "RSS", "THR", "READ/s", "WRITE/s", "TIME"
    );
    for row in rows.iter().take(args.count) {
        let label = |key: &str| row.labels[key].as_str().unwrap_or("-");
        let rate = |map: &HashMap<String, (Value, f64)>| match value(map, &row.id) {
            Some(bytes) => human_bytes(bytes as u64),
            None => "-".into(),
        };
        println!(
            "{:>8} {:<10.10} {:>6.1} {:>9} {:>4} {:>10} {:>10} {:>9}  {}",
            label("pid"),
            label("user"),
            row.cpu,
            human_bytes(row.rss as u64),
            value(&threads, &row.id).map_or("-".into(), |n| format!("{n:.0}")),
            rate(&read),
            rate(&write),
            value(&seconds, &row.id).map_or("-".into(), human_duration),
            label("comm"),
        );
    }
    Ok(())
}

pub fn exits(args: &ExitsArgs) -> Result<()> {
    let now = epoch_now();
    let since = clock::parse(&args.since, now)?;
    let until = clock::parse(&args.until, now)?;
    if until < since {
        bail!("--until is before --since");
    }
    let connection = open(&args.data_dir, LOGS_DB)?;

    // The indexed keys are hidden columns of the table; each of these is an
    // index lookup.
    let mut sql =
        format!("SELECT ts, message, metadata FROM {LOGS_TABLE} WHERE ts BETWEEN ?1 AND ?2");
    let mut values: Vec<Sql> = vec![
        Sql::Integer((since * 1e6) as i64),
        Sql::Integer((until * 1e6) as i64),
    ];
    for (column, wanted) in [
        ("service", &args.comm),
        ("status", &args.status),
        ("host", &args.host),
    ] {
        if let Some(wanted) = wanted {
            values.push(Sql::Text(wanted.clone()));
            sql.push_str(&format!(" AND {column} = ?{}", values.len()));
        }
    }
    sql.push_str(" ORDER BY ts DESC");

    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values))?;

    #[derive(Default)]
    struct Total {
        count: u64,
        elapsed: f64,
        cpu: f64,
        peak_rss: u64,
        read: u64,
        write: u64,
        failed: u64,
    }
    let mut totals: HashMap<String, Total> = HashMap::new();
    let mut shown = 0;

    if !args.summary {
        println!(
            "{:<19} {:>8} {:<10} {:<9} {:>9} {:>9} {:>9}  COMMAND",
            "ENDED", "PID", "USER", "STATUS", "ELAPSED", "CPU", "PEAK RSS"
        );
    }
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(0)?;
        let metadata: String = row.get(2)?;
        let record: Value = serde_json::from_str(&metadata)?;
        if record["kind"] != "exit" {
            continue;
        }
        // Not among the indexed keys, so these are read from each record.
        let wanted = |key: &str, value: &Option<String>| {
            value
                .as_deref()
                .is_none_or(|value| record[key].as_str() == Some(value))
        };
        if !wanted("user", &args.user) || !wanted("unit", &args.unit) {
            continue;
        }
        let text = |key: &str| record[key].as_str().unwrap_or("-");
        let number = |key: &str| record[key].as_f64().unwrap_or(0.0);

        if args.summary {
            let key = match args.by {
                SummaryBy::Comm => "service",
                SummaryBy::Unit => "unit",
                SummaryBy::User => "user",
            };
            let total = totals.entry(text(key).to_string()).or_default();
            total.count += 1;
            total.elapsed += number("elapsed_seconds");
            total.cpu += number("cpu_seconds");
            total.peak_rss = total.peak_rss.max(number("peak_rss_bytes") as u64);
            total.read += number("io_read_bytes") as u64;
            total.write += number("io_write_bytes") as u64;
            if text("status") != "0" && text("status") != "unknown" {
                total.failed += 1;
            }
            continue;
        }
        if shown == args.count {
            break;
        }
        shown += 1;
        let command = match record["cmdline"].as_str() {
            Some(cmdline) if !cmdline.is_empty() => cmdline,
            _ => text("service"),
        };
        println!(
            "{:<19} {:>8} {:<10.10} {:<9} {:>9} {:>9} {:>9}  {}",
            clock::format(ts as f64 / 1e6),
            record["pid"].as_u64().unwrap_or(0),
            text("user"),
            text("status"),
            human_duration(number("elapsed_seconds")),
            human_duration(number("cpu_seconds")),
            human_bytes(number("peak_rss_bytes") as u64),
            command,
        );
    }

    if args.summary {
        let mut totals: Vec<(String, Total)> = totals.into_iter().collect();
        totals.sort_by(|a, b| b.1.cpu.total_cmp(&a.1.cpu).then_with(|| a.0.cmp(&b.0)));
        println!("{} to {}", clock::format(since), clock::format(until));
        println!();
        println!(
            "{:>8} {:>7} {:>10} {:>10} {:>9} {:>10} {:>10}  {}",
            "COUNT",
            "FAILED",
            "CPU",
            "ELAPSED",
            "PEAK RSS",
            "READ",
            "WRITTEN",
            match args.by {
                SummaryBy::Comm => "COMMAND",
                SummaryBy::Unit => "UNIT",
                SummaryBy::User => "USER",
            }
        );
        let all = totals.iter().fold(Total::default(), |mut all, (_, t)| {
            all.count += t.count;
            all.failed += t.failed;
            all.cpu += t.cpu;
            all.elapsed += t.elapsed;
            all.peak_rss = all.peak_rss.max(t.peak_rss);
            all.read += t.read;
            all.write += t.write;
            all
        });
        let print = |name: &str, t: &Total| {
            println!(
                "{:>8} {:>7} {:>10} {:>10} {:>9} {:>10} {:>10}  {}",
                t.count,
                t.failed,
                human_duration(t.cpu),
                human_duration(t.elapsed),
                human_bytes(t.peak_rss),
                human_bytes(t.read),
                human_bytes(t.write),
                name
            );
        };
        for (name, total) in totals.iter().take(args.count) {
            print(name, total);
        }
        if totals.len() > 1 {
            print(&format!("(all {})", totals.len()), &all);
        }
    } else if shown == 0 {
        println!("(none)");
    }
    Ok(())
}

/// One process of a job.
pub(crate) struct Node {
    pub id: Vec<u8>,
    pub parent: Option<Vec<u8>>,
    pub name: String,
    pub unit: String,
    pub failed: bool,
    pub ending: String,
    pub start_ns: i64,
    pub duration_ns: i64,
    pub attributes: Value,
}

impl Node {
    /// What it ran, in at most `width` characters; and what it ran before
    /// that, if it was started as something else.
    pub(crate) fn command(&self, width: usize) -> String {
        let text = |key: &str| {
            self.attributes[key]
                .as_str()
                .filter(|text| !text.is_empty())
        };
        let now = shorten(text("process.command_line").unwrap_or(&self.name), width);
        match text("process.started_as") {
            Some(first) => format!("{} → {now}", shorten(first, width)),
            None => now,
        }
    }
}

fn shorten(text: &str, width: usize) -> String {
    if width == 0 || text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

pub(crate) fn nodes(connection: &Connection, trace: &[u8]) -> Result<Vec<Node>> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT span_id, parent_span_id, name, service, status, status_description,
                start_ts, duration_ns, attributes
           FROM {TRACES_TABLE} WHERE trace_id = ?1 ORDER BY start_ts, span_id"
    ))?;
    let rows = statement.query_map([trace], |row| {
        Ok(Node {
            id: row.get(0)?,
            // The store writes "no parent" as all zeroes.
            parent: row
                .get::<_, Option<Vec<u8>>>(1)?
                .filter(|id| id.iter().any(|byte| *byte != 0)),
            name: row.get(2)?,
            unit: row.get(3)?,
            failed: row.get::<_, String>(4)? == "error",
            ending: row.get(5)?,
            start_ns: row.get(6)?,
            duration_ns: row.get(7)?,
            attributes: serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Draw the processes under `parent`, each on a line, with the lines of
/// the tree to its left.
fn draw(
    all: &[Node],
    children: &HashMap<Option<&[u8]>, Vec<usize>>,
    parent: Option<&[u8]>,
    prefix: &str,
    (left, width): (&mut usize, usize),
    out: &mut Vec<String>,
) {
    let Some(below) = children.get(&parent) else {
        return;
    };
    for (position, index) in below.iter().enumerate() {
        if *left == 0 {
            return;
        }
        *left -= 1;
        let node = &all[*index];
        let last = position + 1 == below.len();
        let (branch, extend) = match (parent.is_none(), last) {
            (true, _) => ("", ""),
            (false, false) => ("├─ ", "│  "),
            (false, true) => ("└─ ", "   "),
        };
        let mut line = format!(
            "{prefix}{branch}{}  {}",
            node.command(width),
            human_duration(node.duration_ns as f64 / 1e9)
        );
        if let Some(cpu) = node.attributes["process.cpu_seconds"].as_f64() {
            line.push_str(&format!(", cpu {}", human_duration(cpu)));
        }
        if let Some(rss) = node.attributes["process.peak_rss_bytes"].as_u64() {
            line.push_str(&format!(", {}", human_bytes(rss)));
        }
        if node.failed {
            line.push_str(&format!("  [{}]", node.ending));
        }
        out.push(line);
        draw(
            all,
            children,
            Some(&node.id),
            &format!("{prefix}{extend}"),
            (left, width),
            out,
        );
    }
}

/// The lines of one job's tree.
pub(crate) fn tree(all: &[Node], max: usize, width: usize) -> Vec<String> {
    let known: std::collections::HashSet<&[u8]> = all.iter().map(|n| n.id.as_slice()).collect();
    let mut children: HashMap<Option<&[u8]>, Vec<usize>> = HashMap::new();
    for (index, node) in all.iter().enumerate() {
        // A process whose parent is not among these is a root of the
        // job: its parent is another job's, or its span was not kept.
        let parent = node
            .parent
            .as_deref()
            .filter(|parent| known.contains(parent));
        children.entry(parent).or_default().push(index);
    }
    let mut out = Vec::new();
    let mut left = max;
    draw(all, &children, None, "", (&mut left, width), &mut out);
    if all.len() > max {
        out.push(format!("… and {} more", all.len() - max));
    }
    out
}

pub fn trees(args: &TreesArgs) -> Result<()> {
    let now = epoch_now();
    let since = clock::parse(&args.since, now)?;
    let until = clock::parse(&args.until, now)?;
    if until < since {
        bail!("--until is before --since");
    }
    let connection = open(&args.data_dir, TRACES_DB)?;

    // The jobs with a process that started in the window and is what was
    // asked for. Each of these is pushed down into the store.
    let mut sql =
        format!("SELECT DISTINCT trace_id FROM {TRACES_TABLE} WHERE start_ts BETWEEN ?1 AND ?2");
    let mut values: Vec<Sql> = vec![
        Sql::Integer((since * 1e9) as i64),
        Sql::Integer((until * 1e9) as i64),
    ];
    for (column, wanted) in [("name", &args.comm), ("service", &args.unit)] {
        if let Some(wanted) = wanted {
            values.push(Sql::Text(wanted.clone()));
            sql.push_str(&format!(" AND {column} = ?{}", values.len()));
        }
    }
    if args.failed {
        sql.push_str(" AND status = 'error'");
    }
    let traces: Vec<Vec<u8>> = connection
        .prepare(&sql)?
        .query_map(params_from_iter(values), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut jobs: Vec<(Vec<u8>, Vec<Node>)> = Vec::new();
    for trace in traces {
        let all = nodes(&connection, &trace)?;
        if all.len() >= args.min_processes {
            jobs.push((trace, all));
        }
    }
    jobs.sort_by_key(|(_, all)| std::cmp::Reverse(all.iter().map(|n| n.start_ns).min()));

    if jobs.is_empty() {
        println!("(none)");
        println!("Spans become visible when the collector flushes, once a minute by default.");
        return Ok(());
    }
    for (trace, all) in jobs.iter().take(args.count) {
        let start = all.iter().map(|n| n.start_ns).min().unwrap_or(0);
        let end = all
            .iter()
            .map(|n| n.start_ns + n.duration_ns)
            .max()
            .unwrap_or(start);
        let cpu: f64 = all
            .iter()
            .filter_map(|n| n.attributes["process.cpu_seconds"].as_f64())
            .sum();
        let failed = all.iter().filter(|n| n.failed).count();
        let mut units: Vec<&str> = all.iter().map(|n| n.unit.as_str()).collect();
        units.sort_unstable();
        units.dedup();
        println!(
            "{}  {} processes over {}, cpu {}{}  in {}  (trace {})",
            clock::format(start as f64 / 1e9),
            all.len(),
            human_duration((end - start) as f64 / 1e9),
            human_duration(cpu),
            if failed > 0 {
                format!(", {failed} failed")
            } else {
                String::new()
            },
            units.join(", "),
            hex(trace),
        );
        for line in tree(all, args.max_processes, args.width) {
            println!("  {line}");
        }
        println!();
    }
    if jobs.len() > args.count {
        println!("({} more jobs)", jobs.len() - args.count);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u8, parent: Option<u8>, command: &str, failed: bool) -> Node {
        Node {
            id: vec![id; 8],
            parent: parent.map(|id| vec![id; 8]),
            name: command.split(' ').next().unwrap().into(),
            unit: "build.service".into(),
            failed,
            ending: "exited 1".into(),
            start_ns: i64::from(id),
            duration_ns: 1_500_000_000,
            attributes: serde_json::json!({
                "process.command_line": command,
                "process.cpu_seconds": 1.25,
                "process.peak_rss_bytes": 10_485_760,
            }),
        }
    }

    #[test]
    fn a_job_is_drawn_as_the_tree_it_was() {
        let all = [
            node(1, None, "make all", false),
            node(2, Some(1), "cc -c a.c", false),
            node(3, Some(2), "as a.s", false),
            node(4, Some(1), "cc -c b.c", true),
            node(5, Some(1), "ld a.o b.o", false),
        ];
        let figures = "1.5s, cpu 1.2s, 10.0 MiB";
        assert_eq!(
            tree(&all, 60, 72),
            [
                format!("make all  {figures}"),
                format!("├─ cc -c a.c  {figures}"),
                format!("│  └─ as a.s  {figures}"),
                format!("├─ cc -c b.c  {figures}  [exited 1]"),
                format!("└─ ld a.o b.o  {figures}"),
            ]
        );
    }

    #[test]
    fn a_pipeline_has_a_root_for_each_command() {
        // Each is the child of a shell that is not part of the job.
        let all = [
            node(1, Some(99), "cat log", false),
            node(2, Some(99), "grep error", false),
        ];
        let lines = tree(&all, 60, 72);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("cat log  "));
        assert!(lines[1].starts_with("grep error  "));
    }

    #[test]
    fn a_long_job_is_cut_short_and_says_so() {
        let mut all = vec![node(1, None, "make", false)];
        all.extend((2..=100).map(|id| node(id, Some(1), "cc", false)));
        let lines = tree(&all, 10, 72);
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[10], "… and 90 more");
    }

    #[test]
    fn a_command_is_shown_with_what_it_was_started_as_and_no_wider_than_asked() {
        let mut shell = node(1, None, "/bin/false", true);
        shell.attributes["process.started_as"] = "sh -c sleep 11; /bin/false".into();
        assert_eq!(shell.command(72), "sh -c sleep 11; /bin/false → /bin/false");
        assert_eq!(shell.command(10), "sh -c sle… → /bin/false");

        let linker = node(2, None, &format!("ld {}", "-L/usr/lib ".repeat(40)), false);
        assert_eq!(linker.command(20).chars().count(), 20);
        assert!(linker.command(20).ends_with('…'));
        assert_eq!(linker.command(0).chars().count(), 3 + 11 * 40);

        // With no command line, its name is what there is.
        let mut bare = node(3, None, "kworker", false);
        bare.attributes = serde_json::json!({});
        assert_eq!(bare.command(72), "kworker");
    }
}
