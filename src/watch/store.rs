//! Reading moments out of a local store.
//!
//! The store is read and never written. The collector that fills it goes
//! on filling it, and what it has flushed since the last look is there at
//! the next one.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::types::Value as Sql;
use rusqlite::{params, Connection};
use serde_json::Value;

use crate::accounting::human_duration;
use crate::query::{nodes, open, tree};
use crate::sink::embedded::{
    LOGS_DB, LOGS_TABLE, METRICS_DB, METRICS_TABLE, TRACES_DB, TRACES_TABLE,
};

use super::data::{Series, Source};

/// A process that ended.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Exit {
    /// Epoch seconds.
    pub at: f64,
    /// The command's name.
    pub name: String,
    pub pid: u64,
    pub user: String,
    pub status: String,
    pub level: String,
    pub elapsed: f64,
    pub cpu: f64,
    pub peak_rss: u64,
    pub unit: String,
    pub command: String,
}

/// A job: the processes of one trace.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Job {
    /// Epoch seconds.
    pub started: f64,
    pub duration: f64,
    pub cpu: f64,
    pub processes: usize,
    pub failed: usize,
    pub unit: String,
    /// What its first process ran.
    pub command: String,
    /// Its processes, drawn as the tree they were.
    pub tree: Vec<String>,
    /// It has not ended: its figures are so far.
    pub running: bool,
}

/// A process that ended badly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Incident {
    /// Epoch seconds.
    pub at: f64,
    /// By a fault, and not by being killed.
    pub error: bool,
}

/// How far to look, and for how many.
#[derive(Debug, Clone, Copy)]
pub struct Reach {
    /// Up to this moment, in epoch seconds.
    pub until: f64,
    /// From this many seconds before it.
    pub span: f64,
    /// For at most this many.
    pub limit: usize,
}

pub struct Store {
    metrics: Connection,
    logs: Connection,
    traces: Option<Connection>,
    /// As the store's filters spell it.
    host: Option<String>,
}

impl Store {
    pub fn open(dir: &Path, host: Option<&str>) -> Result<Self> {
        Ok(Self {
            metrics: open(dir, METRICS_DB)?,
            logs: open(dir, LOGS_DB)?,
            // A store written before spans were kept has none.
            traces: open(dir, TRACES_DB).ok(),
            host: host.map(|host| serde_json::json!({ "host": host }).to_string()),
        })
    }

    /// The first and last moments the store holds samples of.
    pub fn range(&self) -> Option<(f64, f64)> {
        let bound = |key: &str| -> Option<f64> {
            let value: Sql = self
                .metrics
                .query_row(
                    "SELECT value FROM timeless_stats(?1) WHERE key = ?2",
                    params![METRICS_TABLE, key],
                    |row| row.get(0),
                )
                .ok()?;
            match value {
                Sql::Integer(n) => Some(n as f64),
                Sql::Real(n) => Some(n),
                Sql::Text(text) => text.parse().ok(),
                _ => None,
            }
        };
        Some((bound("ts_min")?, bound("ts_max")?)).filter(|(first, last)| last >= first)
    }

    /// The store at one moment: for each series, its last sample in the
    /// `within` seconds up to `at`. A process that had ended by then has
    /// none, and is not there.
    pub fn at(&self, at: f64, within: f64) -> At<'_> {
        At {
            store: self,
            start: (at - within).floor() as i64,
            stop: at.floor() as i64,
        }
    }

    /// One series over a stretch of time: `(epoch seconds, value)`.
    pub fn history(
        &self,
        metric: &str,
        key: &str,
        want: &str,
        from: f64,
        to: f64,
    ) -> Vec<(f64, f64)> {
        let mut filter = serde_json::Map::new();
        filter.insert(key.into(), want.into());
        let read = || -> Result<Vec<(f64, f64)>> {
            let mut statement = self.metrics.prepare_cached(
                "SELECT ts, value FROM timeless_raw(?1, ?2, ?3, ?4, ?5) ORDER BY ts",
            )?;
            let rows = statement.query_map(
                params![
                    METRICS_TABLE,
                    metric,
                    Value::Object(filter).to_string(),
                    from.floor() as i64,
                    to.ceil() as i64
                ],
                |row| Ok((row.get::<_, i64>(0)? as f64, row.get::<_, f64>(1)?)),
            )?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        read().unwrap_or_default()
    }

    /// How busy the host was over a stretch of time, and how far apart
    /// the figures are: `(epoch seconds, percent)`.
    ///
    /// A stretch of hours is read from the samples. A longer one is read
    /// from what the store keeps of them after they are gone, at five
    /// minutes and at an hour, where it keeps the highest of each.
    pub fn timeline(&self, from: f64, to: f64) -> (Vec<(f64, f64)>, f64) {
        let span = to - from;
        let resolution: i64 = if span <= 2.0 * 3600.0 {
            0
        } else if span <= 48.0 * 3600.0 {
            300
        } else {
            3600
        };
        if resolution > 0 {
            let read = || -> Result<Vec<(f64, f64)>> {
                let mut statement = self.metrics.prepare_cached(
                    "SELECT ts, value FROM timeless_rollup(?1, ?2, ?3, ?4, ?5, ?6, 'max')
                      ORDER BY ts",
                )?;
                let rows = statement.query_map(
                    params![
                        METRICS_TABLE,
                        "sys_cpu_busy_pct",
                        r#"{"cpu":"all"}"#,
                        resolution,
                        from.floor() as i64,
                        to.ceil() as i64
                    ],
                    |row| Ok((row.get::<_, i64>(0)? as f64, row.get::<_, f64>(1)?)),
                )?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            };
            match read() {
                Ok(mut points) if !points.is_empty() => {
                    // What is kept of the samples is made when they are
                    // compacted, which is once an hour: the last hour is
                    // in the samples and not in what is kept of them.
                    let step = resolution as f64;
                    let covered = points.last().map_or(from, |(at, _)| at + step);
                    let mut highest: Option<(f64, f64)> = None;
                    for (at, busy) in self.history("sys_cpu_busy_pct", "cpu", "all", covered, to) {
                        let bucket = (at / step).floor() * step;
                        match &mut highest {
                            Some((of, value)) if *of == bucket => *value = value.max(busy),
                            _ => {
                                points.extend(highest.take());
                                highest = Some((bucket, busy));
                            }
                        }
                    }
                    points.extend(highest);
                    return (points, step);
                }
                // A store too young to have them yet.
                _ => {}
            }
        }
        (
            self.history("sys_cpu_busy_pct", "cpu", "all", from, to),
            10.0,
        )
    }

    /// The processes that ended badly over a stretch of time: by a fault,
    /// or by being killed.
    pub fn incidents(&self, from: f64, to: f64) -> Vec<Incident> {
        let mut incidents = Vec::new();
        for (level, error) in [("error", true), ("warning", false)] {
            let read = || -> Result<Vec<f64>> {
                let mut statement = self.logs.prepare_cached(&format!(
                    "SELECT ts FROM {LOGS_TABLE} WHERE ts BETWEEN ?1 AND ?2 AND level = ?3"
                ))?;
                let rows = statement.query_map(
                    params![(from * 1e6) as i64, (to * 1e6) as i64, level],
                    |row| Ok(row.get::<_, i64>(0)? as f64 / 1e6),
                )?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            };
            incidents.extend(
                read()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|at| Incident { at, error }),
            );
        }
        incidents.sort_by(|a, b| a.at.total_cmp(&b.at));
        incidents
    }

    /// The processes that ended within reach and are wanted, the last to
    /// end first.
    ///
    /// What is wanted is decided as the store is read, and not after: a
    /// busy host ends hundreds of processes a minute, and the one that is
    /// looked for is seldom among the last few.
    pub fn exits(&self, reach: Reach, wanted: &dyn Fn(&Exit) -> bool) -> Result<Vec<Exit>> {
        let Reach { until, span, limit } = reach;
        let mut statement = self.logs.prepare_cached(&format!(
            "SELECT ts, level, metadata FROM {LOGS_TABLE}
              WHERE ts BETWEEN ?1 AND ?2 ORDER BY ts DESC"
        ))?;
        let mut rows =
            statement.query(params![((until - span) * 1e6) as i64, (until * 1e6) as i64])?;
        let mut exits = Vec::new();
        while let Some(row) = rows.next()? {
            if exits.len() == limit {
                break;
            }
            let record: Value = serde_json::from_str(&row.get::<_, String>(2)?).unwrap_or_default();
            if record["kind"] != "exit" {
                continue;
            }
            let text = |key: &str| record[key].as_str().unwrap_or("").to_string();
            let exit = Exit {
                at: row.get::<_, i64>(0)? as f64 / 1e6,
                name: text("service"),
                pid: record["pid"].as_u64().unwrap_or(0),
                user: text("user"),
                status: text("status"),
                level: row.get(1)?,
                elapsed: record["elapsed_seconds"].as_f64().unwrap_or(0.0),
                cpu: record["cpu_seconds"].as_f64().unwrap_or(0.0),
                peak_rss: record["peak_rss_bytes"].as_u64().unwrap_or(0),
                unit: text("unit"),
                command: match text("cmdline") {
                    cmdline if cmdline.is_empty() => text("service"),
                    cmdline => cmdline,
                },
            };
            if wanted(&exit) {
                exits.push(exit);
            }
        }
        Ok(exits)
    }

    /// The record of how a process ended, if it has: the first of that
    /// name and pid to end after `from`. A pid is given out again in time,
    /// so the first is the one that was running then.
    pub fn record(&self, name: &str, pid: u64, from: f64) -> Option<(f64, Value)> {
        // By the command's name, which the store keeps an index of.
        let mut statement = self
            .logs
            .prepare_cached(&format!(
                "SELECT ts, metadata FROM {LOGS_TABLE}
                  WHERE service = ?1 AND ts >= ?2 ORDER BY ts"
            ))
            .ok()?;
        let mut rows = statement.query(params![name, (from * 1e6) as i64]).ok()?;
        while let Ok(Some(row)) = rows.next() {
            let at: i64 = row.get(0).ok()?;
            let record: Value = serde_json::from_str(&row.get::<_, String>(1).ok()?).ok()?;
            if record["pid"].as_u64() == Some(pid) {
                return Some((at as f64 / 1e6, record));
            }
        }
        None
    }

    /// The jobs with a process that started within reach, and that are
    /// wanted, the last to start first. A job is more than one process.
    pub fn jobs(
        &self,
        reach: Reach,
        width: usize,
        wanted: &dyn Fn(&Job) -> bool,
    ) -> Result<Vec<Job>> {
        let Reach { until, span, limit } = reach;
        let Some(traces) = &self.traces else {
            return Ok(Vec::new());
        };
        // The latest to start are the ones wanted, and the store gives
        // spans in no order that says so: the ids of all of them are read,
        // and the jobs of the latest are read in full.
        let mut latest: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
        {
            let mut statement = traces.prepare_cached(&format!(
                "SELECT trace_id, start_ts FROM {TRACES_TABLE} WHERE start_ts BETWEEN ?1 AND ?2"
            ))?;
            let mut rows =
                statement.query(params![((until - span) * 1e9) as i64, (until * 1e9) as i64])?;
            let mut seen: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
            while let Some(row) = rows.next()? {
                let trace: Vec<u8> = row.get(0)?;
                let start: i64 = row.get(1)?;
                *seen.entry(trace.clone()).or_default() += 1;
                let first = latest.entry(trace).or_insert(start);
                *first = (*first).min(start);
            }
            latest.retain(|trace, _| seen[trace] > 1);
        }
        let mut order: Vec<(i64, Vec<u8>)> = latest
            .into_iter()
            .map(|(trace, start)| (start, trace))
            .collect();
        order.sort_unstable_by(|a, b| b.cmp(a));

        let mut jobs = Vec::new();
        for (_, trace) in order {
            if jobs.len() == limit {
                break;
            }
            let all = nodes(traces, &trace)?;
            let Some(first) = all.first() else {
                continue;
            };
            let start = all.iter().map(|n| n.start_ns).min().unwrap_or(0);
            let end = all
                .iter()
                .map(|n| n.start_ns + n.duration_ns)
                .max()
                .unwrap_or(start);
            let job = Job {
                started: start as f64 / 1e9,
                duration: (end - start) as f64 / 1e9,
                cpu: all
                    .iter()
                    .filter_map(|n| n.attributes["process.cpu_seconds"].as_f64())
                    .sum(),
                processes: all.len(),
                failed: all.iter().filter(|n| n.failed).count(),
                unit: first.unit.clone(),
                command: first.command(width),
                tree: tree(&all, 200, width),
                running: false,
            };
            if wanted(&job) {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }
}

/// A store, at a moment.
pub struct At<'a> {
    store: &'a Store,
    start: i64,
    stop: i64,
}

impl Source for At<'_> {
    fn series(&mut self, metric: &str) -> Series {
        let read = || -> Result<Series> {
            let mut statement = self
                .store
                .metrics
                .prepare_cached("SELECT labels, value FROM timeless_latest(?1, ?2, ?3, ?4, ?5)")?;
            let rows = statement.query_map(
                params![
                    METRICS_TABLE,
                    metric,
                    self.store.host,
                    self.start,
                    self.stop
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
            )?;
            let mut series = Series::new();
            for row in rows {
                let (labels, value) = row?;
                let labels: BTreeMap<String, String> =
                    serde_json::from_str(&labels).unwrap_or_default();
                series.push((labels, value));
            }
            Ok(series)
        };
        // A metric the store has never held is a metric with no series.
        read().unwrap_or_default()
    }
}

/// `3m ago`, `2h05m ago`: how far back a moment is.
pub fn ago(now: f64, at: f64) -> String {
    format!("{} ago", human_duration((now - at).max(0.0)))
}

#[cfg(test)]
mod tests {
    use super::super::data::tests::batch;
    use super::super::data::Snapshot;
    use super::*;
    use crate::model::{Event, Level, MetricBatch, Span};
    use crate::sink::embedded::{EmbeddedOptions, EmbeddedSink};
    use crate::sink::{Sink, Tick};
    use crate::testutil::Fixture;
    use serde_json::Map;

    fn exit(at: f64, pid: u64, status: &str, cmdline: &str) -> Event {
        let mut fields = Map::new();
        for (key, value) in [
            ("kind", Value::from("exit")),
            ("service", Value::from("cc")),
            ("status", Value::from(status)),
            ("pid", Value::from(pid)),
            ("user", Value::from("mark")),
            ("unit", Value::from("build.service")),
            ("elapsed_seconds", Value::from(2.5)),
            ("cpu_seconds", Value::from(1.25)),
            ("peak_rss_bytes", Value::from(1_048_576)),
        ] {
            fields.insert(key.into(), value);
        }
        if !cmdline.is_empty() {
            fields.insert("cmdline".into(), cmdline.into());
        }
        Event {
            ts_us: (at * 1e6) as i64,
            level: match status {
                "0" => Level::Info,
                "SIGSEGV" => Level::Error,
                "SIGKILL" => Level::Warning,
                _ => Level::Notice,
            },
            message: format!("cc[{pid}] exited {status}"),
            fields,
        }
    }

    fn span(trace: u8, id: u8, parent: Option<u8>, name: &str, start: f64, ok: bool) -> Span {
        let mut attributes = Map::new();
        attributes.insert("process.command_line".into(), format!("{name} -x").into());
        attributes.insert("process.cpu_seconds".into(), 0.5.into());
        attributes.insert("process.unit".into(), "build.service".into());
        Span {
            trace_id: [trace; 16],
            span_id: [id; 8],
            parent_span_id: parent.map(|id| [id; 8]),
            name: name.into(),
            unit: "build.service".into(),
            ok: Some(ok),
            ending: if ok { String::new() } else { "exited 1".into() },
            start_ns: (start * 1e9) as i64,
            duration_ns: 2_000_000_000,
            attributes,
        }
    }

    fn reach(until: f64, limit: usize) -> Reach {
        Reach {
            until,
            span: 3600.0,
            limit,
        }
    }

    /// A store of three readings ten seconds apart, two exits, and jobs.
    fn store(fixture: &Fixture) -> Store {
        let options = EmbeddedOptions {
            dir: fixture.path("data"),
            ..EmbeddedOptions::default()
        };
        let mut sink = EmbeddedSink::open(&options).unwrap();
        for (tick, cpu) in [(0, 10.0), (1, 40.0), (2, 90.0)] {
            let mut metrics = MetricBatch::new(1_753_000_000 + 10 * tick);
            metrics.samples = batch().samples;
            for sample in &mut metrics.samples {
                if sample.name == "unit_cpu_pct"
                    && sample.labels.iter().any(|(_, v)| v == "postgresql.service")
                {
                    sample.value = cpu;
                }
            }
            sink.write(
                "host-a",
                &Tick {
                    metrics: &metrics,
                    events: &[],
                    spans: &[],
                },
            )
            .unwrap();
        }
        sink.write(
            "host-a",
            &Tick {
                metrics: &MetricBatch::new(0),
                events: &[
                    exit(1_753_000_005.0, 70, "0", "cc -c a.c"),
                    exit(1_753_000_015.0, 71, "1", ""),
                    exit(1_753_000_400.0, 72, "SIGKILL", "sleep 9"),
                    exit(1_753_000_500.0, 73, "SIGSEGV", "crash"),
                ],
                spans: &[
                    span(1, 1, None, "make", 1_753_000_001.0, true),
                    span(1, 2, Some(1), "cc", 1_753_000_002.0, false),
                    // A worker of a daemon: a trace of one, and not a job.
                    span(2, 3, None, "postgres", 1_753_000_003.0, true),
                    span(3, 4, None, "sh", 1_753_000_012.0, true),
                    span(3, 5, Some(4), "ls", 1_753_000_013.0, true),
                ],
            },
        )
        .unwrap();
        sink.close().unwrap();
        drop(sink);
        Store::open(&fixture.path("data"), None).unwrap()
    }

    #[test]
    fn a_moment_is_read_back_as_it_was_written() {
        let fixture = Fixture::new("watch_store_moment");
        let store = store(&fixture);
        assert_eq!(store.range(), Some((1_753_000_000.0, 1_753_000_020.0)));

        let live = Snapshot::read(0.0, &mut super::super::data::Sampled::new(&batch()));
        let past = Snapshot::read(0.0, &mut store.at(1_753_000_000.0, 30.0));
        // The same figures, whichever they are read from.
        let mut expected = live;
        expected.units[2].cpu = Some(10.0);
        assert_eq!(past, expected);

        let cpu = |at: f64| Snapshot::read(at, &mut store.at(at, 30.0)).units[2].cpu;
        assert_eq!(cpu(1_753_000_012.0), Some(40.0));
        assert_eq!(cpu(1_753_000_020.0), Some(90.0));
        // Long after the last sample, there is nothing to show.
        assert!(Snapshot::read(0.0, &mut store.at(1_753_000_900.0, 30.0)).is_empty());
        assert!(Snapshot::read(0.0, &mut store.at(1_752_000_000.0, 30.0)).is_empty());
    }

    #[test]
    fn a_series_is_read_over_a_stretch_of_time() {
        let fixture = Fixture::new("watch_store_history");
        let store = store(&fixture);
        let history = store.history(
            "unit_cpu_pct",
            "unit",
            "postgresql.service",
            1_753_000_000.0,
            1_753_000_020.0,
        );
        assert_eq!(
            history,
            [
                (1_753_000_000.0, 10.0),
                (1_753_000_010.0, 40.0),
                (1_753_000_020.0, 90.0)
            ]
        );
        let part = store.history(
            "unit_cpu_pct",
            "unit",
            "postgresql.service",
            1_753_000_005.0,
            1_753_000_015.0,
        );
        assert_eq!(part, [(1_753_000_010.0, 40.0)]);
        assert!(store
            .history("unit_cpu_pct", "unit", "none.service", 0.0, 2e9)
            .is_empty());
        assert!(store
            .history("no_such_metric", "unit", "x", 0.0, 2e9)
            .is_empty());
    }

    #[test]
    fn exits_are_read_up_to_a_moment_the_last_first() {
        let fixture = Fixture::new("watch_store_exits");
        let store = store(&fixture);
        let exits = store.exits(reach(1_753_000_020.0, 10), &|_| true).unwrap();
        assert_eq!(exits.len(), 2);
        assert_eq!(exits[0].pid, 71);
        assert_eq!(exits[0].status, "1");
        assert_eq!(exits[0].level, "notice");
        // With no command line, its name is what there is.
        assert_eq!(exits[0].command, "cc");
        assert_eq!(exits[1].command, "cc -c a.c");
        assert_eq!(exits[1].unit, "build.service");
        assert_eq!(exits[1].peak_rss, 1_048_576);

        // Rewound to before the second one ended.
        let earlier = store.exits(reach(1_753_000_010.0, 10), &|_| true).unwrap();
        assert_eq!(earlier.len(), 1);
        assert_eq!(earlier[0].pid, 70);
        let one = store.exits(reach(1_753_000_020.0, 1), &|_| true).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].pid, 71);
        // The one that is wanted, and not the last one: what is wanted
        // is decided before what is enough.
        let first = store
            .exits(reach(1_753_000_020.0, 1), &|exit| exit.status == "0")
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].pid, 70);
    }

    #[test]
    fn how_a_process_ended_is_found_by_its_name_and_pid() {
        let fixture = Fixture::new("watch_store_record");
        let store = store(&fixture);
        assert_eq!(
            store.exits(reach(1_753_000_020.0, 10), &|_| true).unwrap()[0].name,
            "cc"
        );

        let (at, record) = store.record("cc", 70, 1_753_000_000.0).unwrap();
        assert_eq!(at, 1_753_000_005.0);
        assert_eq!(record["cmdline"], "cc -c a.c");
        assert_eq!(record["status"], "0");
        // Looked for from a moment after it ended, it is not the one.
        assert!(store.record("cc", 70, 1_753_000_006.0).is_none());
        assert!(store.record("cc", 999, 0.0).is_none());
        assert!(store.record("ld", 70, 0.0).is_none());
    }

    #[test]
    fn jobs_are_read_up_to_a_moment_the_last_first() {
        let fixture = Fixture::new("watch_store_jobs");
        let store = store(&fixture);
        let jobs = store
            .jobs(reach(1_753_000_020.0, 10), 72, &|_| true)
            .unwrap();
        // The trace of one process is not a job.
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].command, "sh -x");
        assert_eq!(jobs[0].processes, 2);
        assert_eq!(jobs[0].failed, 0);
        let make = &jobs[1];
        assert_eq!(make.command, "make -x");
        assert_eq!(make.started, 1_753_000_001.0);
        // From the start of the first to the end of the last.
        assert_eq!(make.duration, 3.0);
        assert_eq!(make.cpu, 1.0);
        assert_eq!(make.failed, 1);
        assert_eq!(make.unit, "build.service");
        assert_eq!(
            make.tree,
            [
                "make -x  2.0s, cpu 500ms",
                "└─ cc -x  2.0s, cpu 500ms  [exited 1]"
            ]
        );

        let earlier = store
            .jobs(reach(1_753_000_010.0, 10), 72, &|_| true)
            .unwrap();
        assert_eq!(earlier.len(), 1);
        assert_eq!(earlier[0].command, "make -x");

        let failed = store
            .jobs(reach(1_753_000_020.0, 1), 72, &|job| job.failed > 0)
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].command, "make -x");
    }

    #[test]
    fn how_busy_the_host_was_and_what_went_wrong_are_read_over_a_stretch() {
        let fixture = Fixture::new("watch_store_timeline");
        let store = store(&fixture);
        let (busy, step) = store.timeline(1_753_000_000.0, 1_753_000_020.0);
        assert_eq!(step, 10.0);
        assert_eq!(
            busy,
            [
                (1_753_000_000.0, 12.5),
                (1_753_000_010.0, 12.5),
                (1_753_000_020.0, 12.5)
            ]
        );
        // A stretch of days, of a store too young for what is kept of
        // days: what there is, is read.
        let (busy, _) = store.timeline(1_752_900_000.0, 1_753_100_000.0);
        assert_eq!(busy.len(), 3);

        let incidents = store.incidents(1_753_000_000.0, 1_753_001_000.0);
        assert_eq!(
            incidents,
            [
                Incident {
                    at: 1_753_000_400.0,
                    error: false
                },
                Incident {
                    at: 1_753_000_500.0,
                    error: true
                },
            ]
        );
        assert!(store.incidents(1_753_000_000.0, 1_753_000_100.0).is_empty());
    }

    #[test]
    fn a_long_stretch_has_the_last_hour_in_it() {
        let fixture = Fixture::new("watch_store_tail");
        let options = EmbeddedOptions {
            dir: fixture.path("data"),
            ..EmbeddedOptions::default()
        };
        let mut sink = EmbeddedSink::open(&options).unwrap();
        let start = 1_753_000_200;
        let write = |sink: &mut EmbeddedSink, at: i64, busy: f64| {
            let mut metrics = MetricBatch::new(at);
            metrics.push(
                "sys_cpu_busy_pct",
                &crate::model::labels(vec![("cpu", "all".into())]),
                busy,
            );
            sink.write(
                "host-a",
                &Tick {
                    metrics: &metrics,
                    events: &[],
                    spans: &[],
                },
            )
            .unwrap();
        };
        // Twenty minutes, busiest in the second five; compacted.
        for tick in 0..120 {
            let busy = if (30..60).contains(&tick) { 80.0 } else { 5.0 };
            write(&mut sink, start + 10 * tick, busy);
        }
        sink.flush().unwrap();
        sink.maintain().unwrap();
        // And ten minutes since, busy for one reading of them.
        for tick in 120..180 {
            let busy = if tick == 150 { 60.0 } else { 7.0 };
            write(&mut sink, start + 10 * tick, busy);
        }
        sink.close().unwrap();
        drop(sink);

        let store = Store::open(&fixture.path("data"), None).unwrap();
        let (from, to) = (start as f64 - 3.0 * 3600.0, start as f64 + 1800.0);
        let (busy, step) = store.timeline(from, to);
        assert_eq!(step, 300.0);
        let at = |minutes: i64| (start + 60 * minutes) as f64;
        assert_eq!(
            busy,
            [
                (at(0), 5.0),
                (at(5), 80.0),
                (at(10), 5.0),
                (at(15), 5.0),
                // Since the compaction: from the samples themselves.
                (at(20), 7.0),
                (at(25), 60.0),
            ]
        );
    }

    #[test]
    fn how_far_back_a_moment_is() {
        assert_eq!(ago(1000.0, 820.0), "3m00s ago");
        assert_eq!(ago(1000.0, 1000.5), "0ms ago");
    }
}
