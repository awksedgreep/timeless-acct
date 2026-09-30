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
    release_freed_memory, LOGS_DB, LOGS_TABLE, METRICS_DB, METRICS_TABLE, TRACES_DB, TRACES_TABLE,
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
    /// Everything in it that can be looked for: what each of its
    /// processes ran, as whom, and where.
    pub said: String,
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

/// How far apart samples are taken to be, of a store that has too few to
/// say: the collector's own default.
pub const SPACING: f64 = 10.0;
const SPACING_NEAR: f64 = 300.0;
const SPACING_FAR: f64 = 4.0 * 3600.0;

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

    /// Give back what a reading took. SQLite keeps the pages it read, and
    /// the allocator what was freed, until they are asked.
    pub fn release(&self) {
        for connection in [Some(&self.metrics), Some(&self.logs), self.traces.as_ref()]
            .into_iter()
            .flatten()
        {
            let _ = connection.execute_batch("PRAGMA shrink_memory;");
        }
        release_freed_memory();
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
        self.samples(metric, &Value::Object(filter).to_string(), from, to)
    }

    /// How far apart the store's samples are around a moment, in seconds:
    /// of the system, and of processes and units. The collector is told
    /// both when it is started, and a store does not say what it was told.
    ///
    /// The gap that half of the samples before the moment are no further
    /// apart than: a tick that was missed, or a collector that was stopped
    /// and started, is a gap and not the spacing.
    pub fn spacing(&self, until: f64) -> (Option<f64>, Option<f64>) {
        let of = |metric: &str, label: Option<(&str, &str)>| -> Option<f64> {
            let mut filter = match &self.host {
                Some(host) => serde_json::from_str(host).unwrap_or_default(),
                None => serde_json::Map::new(),
            };
            if let Some((key, want)) = label {
                filter.insert(key.into(), want.into());
            }
            let filter = Value::Object(filter).to_string();
            // Near the moment; and further back, for a store sampled by
            // the minute.
            [SPACING_NEAR, SPACING_FAR].into_iter().find_map(|back| {
                let mut gaps: Vec<f64> = self
                    .samples(metric, &filter, until - back, until)
                    .windows(2)
                    .map(|pair| pair[1].0 - pair[0].0)
                    .filter(|gap| *gap > 0.0)
                    .collect();
                gaps.sort_by(f64::total_cmp);
                (gaps.len() >= 2).then(|| gaps[gaps.len() / 2])
            })
        };
        (
            of("sys_cpu_busy_pct", Some(("cpu", "all"))),
            of("acct_processes", None),
        )
    }

    fn samples(&self, metric: &str, filter: &str, from: f64, to: f64) -> Vec<(f64, f64)> {
        let read = || -> Result<Vec<(f64, f64)>> {
            let mut statement = self.metrics.prepare_cached(
                "SELECT ts, value FROM timeless_raw(?1, ?2, ?3, ?4, ?5) ORDER BY ts",
            )?;
            let rows = statement.query_map(
                params![
                    METRICS_TABLE,
                    metric,
                    filter,
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
            self.spacing(to).0.unwrap_or(SPACING),
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
    /// end first: all of the reach, read a page at a time.
    #[cfg(test)]
    pub fn exits(&self, reach: Reach, wanted: &dyn Fn(&Exit) -> bool) -> Result<Vec<Exit>> {
        let from = ((reach.until - reach.span) * 1e6) as i64;
        let mut upto = (reach.until * 1e6).floor() as i64;
        let mut seen = std::collections::HashSet::new();
        let mut exits = Vec::new();
        loop {
            let page = self.exits_page(from, upto, reach.limit - exits.len(), "", wanted)?;
            exits.extend(
                page.found
                    .into_iter()
                    .filter(|exit| seen.insert(((exit.at * 1e6).round() as i64, exit.pid))),
            );
            match page.next {
                Some(next) if exits.len() < reach.limit => upto = next,
                _ => break,
            }
        }
        Ok(exits)
    }

    /// The last processes to end from `from` to `upto`, in microseconds
    /// and with both in it: the last first, and no more than `limit`. The
    /// store stops at that many itself, and an hour is no more work than
    /// a minute.
    pub fn exits_latest(&self, from: i64, upto: i64, limit: usize) -> Result<Vec<Exit>> {
        Ok(self.exits_page(from, upto, limit, "", &|_| true)?.found)
    }

    /// A page of the records from `from` to `upto`, in microseconds and
    /// with both in it: the last `PAGE` of them, and of those the ones
    /// that are wanted, the last to end first and no more than `limit`.
    ///
    /// The store hands over every record it is asked for before the first
    /// can be looked at, at some ten kilobytes each: an hour in which a
    /// quarter of a million processes ended took four seconds and 2.8 GiB
    /// to count. It does stop at a number it is given, if the stretch is
    /// given with both its ends in it. So a stretch is read in pages, and
    /// the next page is up to where this one ended.
    ///
    /// `looked_for` is what `wanted` is after, if it is a text: a record
    /// that does not have it anywhere is not taken apart to be asked.
    pub fn exits_page(
        &self,
        from: i64,
        upto: i64,
        limit: usize,
        looked_for: &str,
        wanted: &dyn Fn(&Exit) -> bool,
    ) -> Result<Page> {
        let needle = Needle::new(looked_for);
        // A page of what is looked for is a page of records read; a page
        // of all there is, is what was asked for and no more.
        let page = if needle.is_empty() {
            limit.min(PAGE)
        } else {
            PAGE
        };
        let mut statement = self.logs.prepare_cached(&format!(
            "SELECT ts, level, metadata FROM {LOGS_TABLE}
              WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts DESC LIMIT ?3"
        ))?;
        let mut rows = statement.query(params![from, upto, page as i64])?;
        let (mut found, mut read, mut oldest) = (Vec::new(), 0, upto);
        while let Some(row) = rows.next()? {
            read += 1;
            oldest = row.get(0)?;
            if found.len() >= limit {
                continue;
            }
            let record = row.get_ref(2)?.as_str()?;
            if !needle.may_be_in(record) {
                continue;
            }
            let Some(exit) = exit_of(oldest, row.get(1)?, record) else {
                continue;
            };
            if wanted(&exit) {
                found.push(exit);
            }
        }
        // A full page may have stopped among records of one instant: the
        // next begins at that instant again, and what it finds twice is
        // for whoever keeps them to know. Unless all of the page was of
        // one instant, and then it begins before it.
        let next =
            (read == page && page > 0).then(|| if oldest < upto { oldest } else { oldest - 1 });
        Ok(Page {
            found,
            read,
            reached: oldest,
            next: next.filter(|next| *next >= from),
        })
    }

    /// The processes that ended from `from` to `upto` and whose record
    /// says `text`: the command's name is in what a record says, and how
    /// it ended. The store looks for this itself, and an hour is no more
    /// work than a minute.
    pub fn exits_saying(&self, text: &str, from: i64, upto: i64, limit: usize) -> Vec<Exit> {
        let read = || -> Result<Vec<Exit>> {
            let mut statement = self.logs.prepare_cached(&format!(
                "SELECT ts, level, metadata FROM {LOGS_TABLE}
                  WHERE ts >= ?1 AND ts <= ?2 AND message_contains = ?3
                  ORDER BY ts DESC LIMIT ?4"
            ))?;
            let mut rows = statement.query(params![from, upto, text, limit as i64])?;
            let mut exits = Vec::new();
            while let Some(row) = rows.next()? {
                let record = row.get_ref(2)?.as_str()?;
                exits.extend(exit_of(row.get(0)?, row.get(1)?, record));
            }
            Ok(exits)
        };
        // A store whose engine cannot look is one that is read through.
        read().unwrap_or_default()
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

    /// The jobs with a process that started within reach, the last to
    /// start first. A job is more than one process.
    ///
    /// With something `looked_for`, the jobs that have it in them: in what
    /// any of their processes ran, or as whom, or where. A build is found
    /// by its compiler, and not only by what it was started with.
    pub fn jobs(&self, reach: Reach, width: usize, looked_for: &str) -> Result<Vec<Job>> {
        let Reach { until, span, limit } = reach;
        let Some(traces) = &self.traces else {
            return Ok(Vec::new());
        };
        let needle = Needle::new(looked_for);
        // The store gives spans in no order that says which are the
        // latest: all of the reach is read once, for which jobs there are,
        // when each began, and whether it is wanted. Spans are handed over
        // one at a time, and an hour of them is a third of a second.
        struct Seen {
            first: i64,
            spans: usize,
            wanted: bool,
        }
        let mut seen: BTreeMap<Vec<u8>, Seen> = BTreeMap::new();
        {
            let columns = if needle.is_empty() {
                "trace_id, start_ts"
            } else {
                "trace_id, start_ts, name, attributes"
            };
            let mut statement = traces.prepare_cached(&format!(
                "SELECT {columns} FROM {TRACES_TABLE} WHERE start_ts BETWEEN ?1 AND ?2"
            ))?;
            let mut rows =
                statement.query(params![((until - span) * 1e9) as i64, (until * 1e9) as i64])?;
            while let Some(row) = rows.next()? {
                let start: i64 = row.get(1)?;
                let wanted = needle.is_empty() || {
                    let name = row.get_ref(2)?.as_str()?;
                    let attributes = row.get_ref(3)?.as_str()?;
                    needle.is_in(name)
                        || (needle.may_be_in(attributes)
                            && serde_json::from_str::<Value>(attributes)
                                .is_ok_and(|attributes| needle.is_in(&said(name, &attributes))))
                };
                let job = seen.entry(row.get(0)?).or_insert(Seen {
                    first: start,
                    spans: 0,
                    wanted: false,
                });
                job.first = job.first.min(start);
                job.spans += 1;
                job.wanted |= wanted;
            }
        }
        // One process seen is not yet not a job: the rest of it may have
        // started before the reach. What is looked for is rare enough to
        // read and see; what is not, is left for a reach that has it.
        let mut order: Vec<(i64, Vec<u8>)> = seen
            .into_iter()
            .filter(|(_, job)| job.wanted && (job.spans > 1 || !needle.is_empty()))
            .map(|(trace, job)| (job.first, trace))
            .collect();
        order.sort_unstable_by(|a, b| b.cmp(a));

        let mut jobs = Vec::new();
        for (_, trace) in order {
            if jobs.len() == limit {
                break;
            }
            let all = nodes(traces, &trace)?;
            let Some(first) = all.first().filter(|_| all.len() > 1) else {
                continue;
            };
            let start = all.iter().map(|n| n.start_ns).min().unwrap_or(0);
            let end = all
                .iter()
                .map(|n| n.start_ns + n.duration_ns)
                .max()
                .unwrap_or(start);
            jobs.push(Job {
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
                said: all
                    .iter()
                    .map(|n| said(&n.name, &n.attributes))
                    .collect::<Vec<_>>()
                    .join("\n"),
                running: false,
            });
        }
        Ok(jobs)
    }
}

/// What can be looked for in a process of a job: what it ran, what it was
/// started as, as whom, and where.
pub fn said(name: &str, attributes: &Value) -> String {
    let mut said = name.to_string();
    for key in [
        "process.command_line",
        "process.started_as",
        "process.owner",
        "process.unit",
    ] {
        if let Some(text) = attributes[key].as_str().filter(|text| !text.is_empty()) {
            said.push(' ');
            said.push_str(text);
        }
    }
    said
}

/// A process that ended, from its record.
fn exit_of(ts: i64, level: String, record: &str) -> Option<Exit> {
    let record: Value = serde_json::from_str(record).ok()?;
    if record["kind"] != "exit" {
        return None;
    }
    let text = |key: &str| record[key].as_str().unwrap_or("").to_string();
    Some(Exit {
        at: ts as f64 / 1e6,
        name: text("service"),
        pid: record["pid"].as_u64().unwrap_or(0),
        user: text("user"),
        status: text("status"),
        level,
        elapsed: record["elapsed_seconds"].as_f64().unwrap_or(0.0),
        cpu: record["cpu_seconds"].as_f64().unwrap_or(0.0),
        peak_rss: record["peak_rss_bytes"].as_u64().unwrap_or(0),
        unit: text("unit"),
        command: match text("cmdline") {
            cmdline if cmdline.is_empty() => text("service"),
            cmdline => cmdline,
        },
    })
}

/// Records the store is asked for at once. At the ten kilobytes it takes
/// to hand one over, a fifth of a gigabyte while a page is read.
const PAGE: usize = if cfg!(test) { 2_000 } else { 20_000 };

/// A page of records: what it had of what was wanted, and where it ended.
#[derive(Debug)]
pub struct Page {
    pub found: Vec<Exit>,
    /// How many records it held.
    #[cfg_attr(not(test), allow(dead_code))]
    pub read: usize,
    /// The time of the earliest of them, in microseconds.
    pub reached: i64,
    /// Up to when the next page is, if there is one.
    pub next: Option<i64>,
}

/// A text that is looked for, in whatever case it is written.
pub struct Needle {
    text: String,
    /// It can be looked for in a record as the store keeps it, before the
    /// record is taken apart: it has nothing in it that is written
    /// otherwise there.
    plain: bool,
}

impl Needle {
    pub fn new(text: &str) -> Self {
        Self {
            plain: text.is_ascii()
                && !text.contains(['"', '\\'])
                && !text.chars().any(char::is_control),
            text: text.to_lowercase(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Whether it is in `text`.
    pub fn is_in(&self, text: &str) -> bool {
        self.text.is_empty() || text.to_lowercase().contains(&self.text)
    }

    /// False only if it cannot be in any field of `record`, which is JSON.
    pub fn may_be_in(&self, record: &str) -> bool {
        if self.text.is_empty() || !self.plain {
            return true;
        }
        let (needle, hay) = (self.text.as_bytes(), record.as_bytes());
        hay.len() >= needle.len()
            && hay
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle))
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
    use crate::model::{labels, no_labels, Event, Level, MetricBatch, Span};
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

    /// A store of the system sampled every `system` seconds and the
    /// processes every `processes`, for ten minutes.
    fn paced(fixture: &Fixture, system: i64, processes: i64) -> Store {
        let options = EmbeddedOptions {
            dir: fixture.path("data"),
            ..EmbeddedOptions::default()
        };
        let mut sink = EmbeddedSink::open(&options).unwrap();
        for second in 0..600 {
            let mut metrics = MetricBatch::new(1_753_000_000 + second);
            // A collector that was stopped for a while, and a tick it
            // missed: gaps, and not how far apart its samples are.
            if (200..290).contains(&second) || second == 300 {
                continue;
            }
            if second % system == 0 {
                metrics.push(
                    "sys_cpu_busy_pct",
                    &labels(vec![("cpu", "all".into())]),
                    5.0,
                );
                metrics.push("sys_cpu_busy_pct", &labels(vec![("cpu", "0".into())]), 5.0);
            }
            if second % processes == 0 {
                metrics.push("acct_processes", &no_labels(), 200.0);
                metrics.push(
                    "unit_cpu_pct",
                    &labels(vec![("unit", "db.service".into())]),
                    second as f64,
                );
            }
            if metrics.samples.is_empty() {
                continue;
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
        sink.close().unwrap();
        drop(sink);
        Store::open(&fixture.path("data"), None).unwrap()
    }

    #[test]
    fn a_store_says_how_far_apart_its_samples_are() {
        let last = 1_753_000_599.0;
        for (name, system, processes) in [
            ("watch_store_pace_1", 1, 1),
            ("watch_store_pace_10", 10, 10),
            ("watch_store_pace_mixed", 5, 60),
        ] {
            let fixture = Fixture::new(name);
            let store = paced(&fixture, system, processes);
            assert_eq!(
                store.spacing(last),
                (Some(system as f64), Some(processes as f64)),
                "{name}"
            );
            // Of one host, in a store that may hold another's.
            let of_host = Store::open(&fixture.path("data"), Some("host-a")).unwrap();
            assert_eq!(of_host.spacing(last).0, Some(system as f64), "{name}");
            let of_another = Store::open(&fixture.path("data"), Some("host-b")).unwrap();
            assert_eq!(of_another.spacing(last), (None, None), "{name}");
        }
    }

    #[test]
    fn a_store_with_too_few_samples_does_not_say() {
        let fixture = Fixture::new("watch_store_pace_few");
        let store = store(&fixture);
        // Three samples of the system, ten seconds apart, and none of
        // `acct_processes`.
        assert_eq!(store.spacing(1_753_000_020.0), (Some(10.0), None));
        assert_eq!(store.spacing(1_752_000_000.0), (None, None));
    }

    #[test]
    fn a_series_sampled_by_the_minute_is_there_between_its_samples() {
        let fixture = Fixture::new("watch_store_pace_minute");
        let store = paced(&fixture, 60, 60);
        let at = 1_753_000_530.0;
        let unit = |within: f64| {
            Snapshot::read(at, &mut store.at(at, within))
                .units
                .first()
                .and_then(|unit| unit.cpu)
        };
        // The last sample was fifty seconds before.
        assert_eq!(unit(30.0), None);
        let (_, processes) = store.spacing(at);
        assert_eq!(unit(3.0 * processes.unwrap()), Some(480.0));
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
        let jobs = store.jobs(reach(1_753_000_020.0, 10), 72, "").unwrap();
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

        let earlier = store.jobs(reach(1_753_000_010.0, 10), 72, "").unwrap();
        assert_eq!(earlier.len(), 1);
        assert_eq!(earlier[0].command, "make -x");

        let latest = store.jobs(reach(1_753_000_020.0, 1), 72, "").unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].command, "sh -x");
    }

    #[test]
    fn a_job_is_found_by_anything_that_ran_in_it() {
        let fixture = Fixture::new("watch_store_jobs_looked_for");
        let store = store(&fixture);
        let found = |text: &str| -> Vec<String> {
            store
                .jobs(reach(1_753_000_020.0, 10), 72, text)
                .unwrap()
                .into_iter()
                .map(|job| job.command)
                .collect()
        };
        // By what it was started with, and by what it went on to run.
        assert_eq!(found("make"), ["make -x"]);
        assert_eq!(found("cc -x"), ["make -x"]);
        assert_eq!(found("ls"), ["sh -x"]);
        // By where, in whatever case it is written.
        assert_eq!(found("BUILD.service"), ["sh -x", "make -x"]);
        // Not by the name of a field every span has.
        assert!(found("command_line").is_empty());
        // A process by itself is found, and is not a job.
        assert!(found("postgres").is_empty());
        assert!(found("no such thing").is_empty());

        let make = &store.jobs(reach(1_753_000_020.0, 10), 72, "cc").unwrap()[0];
        assert!(
            make.said.contains("make -x") && make.said.contains("cc -x"),
            "{}",
            make.said
        );
    }

    #[test]
    fn records_are_read_a_page_at_a_time() {
        let fixture = Fixture::new("watch_store_pages");
        let store = store(&fixture);
        // Four exits, at 5, 15, 400, and 500 seconds.
        let all = store.exits(reach(1_753_000_600.0, 10), &|_| true).unwrap();
        assert_eq!(
            all.iter().map(|exit| exit.pid).collect::<Vec<_>>(),
            [73, 72, 71, 70]
        );

        let us = |seconds: f64| ((1_753_000_000.0 + seconds) * 1e6) as i64;
        // Both ends of a stretch are in it.
        let page = store
            .exits_page(us(15.0), us(400.0), 10, "", &|_| true)
            .unwrap();
        assert_eq!(
            page.found.iter().map(|e| e.pid).collect::<Vec<_>>(),
            [72, 71]
        );
        assert_eq!((page.read, page.reached, page.next), (2, us(15.0), None));

        // What is looked for narrows what is taken apart, and not what is
        // counted as read.
        let page = store
            .exits_page(us(0.0), us(600.0), 10, "SLEEP", &|exit| {
                exit.command.contains("sleep")
            })
            .unwrap();
        assert_eq!(page.found.iter().map(|e| e.pid).collect::<Vec<_>>(), [72]);
        assert_eq!(page.read, 4);

        // Of all there is, no more is read than was asked for; and there
        // is a next page, up to where this one ended.
        let page = store
            .exits_page(us(0.0), us(600.0), 2, "", &|_| true)
            .unwrap();
        assert_eq!(
            page.found.iter().map(|e| e.pid).collect::<Vec<_>>(),
            [73, 72]
        );
        assert_eq!((page.read, page.next), (2, Some(us(400.0))));
        let latest = store.exits_latest(us(0.0), us(400.0), 3).unwrap();
        assert_eq!(
            latest.iter().map(|e| e.pid).collect::<Vec<_>>(),
            [72, 71, 70]
        );
    }

    #[test]
    fn the_store_looks_for_what_a_record_says() {
        let fixture = Fixture::new("watch_store_saying");
        let store = store(&fixture);
        let us = |seconds: f64| ((1_753_000_000.0 + seconds) * 1e6) as i64;
        let said = |text: &str| -> Vec<u64> {
            store
                .exits_saying(text, us(0.0), us(600.0), 10)
                .iter()
                .map(|exit| exit.pid)
                .collect()
        };
        // How it ended is in what the record says.
        assert_eq!(said("SIGKILL"), [72]);
        assert_eq!(said("sigsegv"), [73]);
        assert!(said("no such thing").is_empty());
    }

    #[test]
    fn a_stretch_that_holds_more_than_a_page_is_read_in_pages_and_each_record_once() {
        let fixture = Fixture::new("watch_store_many");
        let options = EmbeddedOptions {
            dir: fixture.path("data"),
            ..EmbeddedOptions::default()
        };
        let mut sink = EmbeddedSink::open(&options).unwrap();
        // A fork storm: more than two pages of processes ending in a
        // minute, a hundred of them at each instant.
        let count = 2 * PAGE as u64 + 5_000;
        let events: Vec<Event> = (0..count)
            .map(|n| exit(1_753_000_000.0 + (n / 100) as f64 * 0.1, n, "0", "true"))
            .collect();
        sink.write(
            "host-a",
            &Tick {
                metrics: &MetricBatch::new(0),
                events: &events,
                spans: &[],
            },
        )
        .unwrap();
        sink.close().unwrap();
        drop(sink);
        let store = Store::open(&fixture.path("data"), None).unwrap();

        let us = |seconds: f64| ((1_753_000_000.0 + seconds) * 1e6) as i64;
        // A page is what is read at once, and says where the next is.
        let first = store
            .exits_page(us(0.0), us(60.0), 10, "true", &|_| true)
            .unwrap();
        assert_eq!((first.found.len(), first.read), (10, PAGE));
        assert!(first
            .next
            .is_some_and(|next| next < us(60.0) && next >= first.reached));

        // Read through, every one is found once, the last first.
        let all = store
            .exits(
                Reach {
                    until: 1_753_000_060.0,
                    span: 3600.0,
                    limit: count as usize + 1,
                },
                &|_| true,
            )
            .unwrap();
        assert_eq!(all.len(), count as usize);
        assert!(all.windows(2).all(|pair| pair[0].at >= pair[1].at));
        let mut pids: Vec<u64> = all.iter().map(|exit| exit.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), count as usize);
    }

    #[test]
    fn what_is_looked_for_is_found_in_any_case_and_not_in_what_cannot_have_it() {
        let needle = Needle::new("RustC");
        assert!(needle.is_in("/usr/bin/rustc --edition"));
        assert!(needle.may_be_in(r#"{"cmdline":"RUSTC -O"}"#));
        assert!(!needle.may_be_in(r#"{"cmdline":"cc -O"}"#));
        assert!(!needle.may_be_in("ru"));
        // What a record writes otherwise is not judged before it is read.
        assert!(Needle::new(r#"say "hi""#).may_be_in(r#"{"cmdline":"cc"}"#));
        assert!(Needle::new("naïve").may_be_in(r#"{"cmdline":"cc"}"#));
        assert!(!Needle::new("naïve").is_in("cc"));
        assert!(Needle::new("").is_in("anything") && Needle::new("").may_be_in("{}"));
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
