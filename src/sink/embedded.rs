//! Own three SQLite databases through the in-process engine.
//!
//! The layout is the one the Timeless signal servers use: one database per
//! signal, the tables named `metric_samples`, `logs`, and `traces`, each
//! guarded by the same owner lease. A directory written here can be handed
//! to the three signal servers as it stands. The lease is what stops a
//! server and a collector from owning it at once.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::encode::{event_metadata, prometheus_text, resource, scope};

use super::{Footprint, Sink, Tick};
use crate::accounting::{human_bytes, human_duration};
use crate::clock;

pub const METRICS_DB: &str = "metrics.db";
pub const LOGS_DB: &str = "logs.db";
pub const TRACES_DB: &str = "traces.db";
pub const METRICS_TABLE: &str = "metric_samples";
pub const LOGS_TABLE: &str = "logs";
pub const TRACES_TABLE: &str = "traces";

/// The keys the logs plane indexes by default, in its order.
const LOG_INDEX_KEYS: &str = "service,path,status,host";
/// Source entries one optimize pass may rewrite.
const OPTIMIZE_BUDGET: u32 = 65_536;
/// Free pages one maintenance pass may return: a bound on the time it holds
/// the database, at 4 KiB a page.
const VACUUM_PAGES: u32 = 16_384;
/// What a write-ahead log may keep of the size it once grew to. A
/// maintenance pass writes tens of megabytes; a tick writes a few pages.
const WAL_KEPT: u32 = 8 << 20;
/// The owner lease the signal servers take, relative to the database.
const LEASE_SUFFIX: &str = ".timeless-api.lock";

#[derive(Debug, Clone)]
pub struct EmbeddedOptions {
    pub dir: PathBuf,
    /// How long raw samples are kept, as the engine spells it: `7d`.
    pub retention: String,
    /// The rollup ladder: `5m@30d,1h@180d`. Empty for none.
    pub rollups: String,
    /// How long accounting records are kept.
    pub log_retention: String,
    /// How long spans are kept.
    pub trace_retention: String,
    /// What the store may take on disk, in bytes. Over it, the oldest of
    /// the least valuable kind is pruned until it fits.
    pub limit: Option<u64>,
}

impl Default for EmbeddedOptions {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("timeless-acct-data"),
            retention: "7d".into(),
            rollups: "5m@30d,1h@180d".into(),
            log_retention: "30d".into(),
            trace_retention: "30d".into(),
            limit: Some(2 << 30),
        }
    }
}

/// Table arguments go into SQL text, where a quote would end the literal.
fn table_argument(name: &str, value: &str) -> Result<String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | ',' | '_'))
    {
        bail!("{name} {value:?} is not a duration or a rollup ladder");
    }
    Ok(format!("{name}='{value}'"))
}

/// An exclusive advisory lock, held for as long as the file is open.
fn lease(database: &Path) -> Result<File> {
    let mut name = database.as_os_str().to_owned();
    name.push(LEASE_SUFFIX);
    let path = PathBuf::from(name);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open owner lease {}", path.display()))?;
    // SAFETY: flock takes a descriptor this function owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!(
            "{} is owned by another process (a signal server, or another collector)",
            database.display()
        );
    }
    Ok(file)
}

/// Open one database the way the signal servers do, and make the engine's
/// tables available on the connection.
pub fn open(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    // auto_vacuum comes first: it can only be chosen while the database is
    // empty, and switching to WAL writes the header that ends that. The
    // page size is the traces server's, for the same reason and at the
    // same moment; a database that has pages already keeps its own.
    if path.file_name().is_some_and(|name| name == TRACES_DB) {
        connection.execute_batch("PRAGMA page_size = 16384;")?;
    }
    connection.execute_batch(&format!(
        "PRAGMA auto_vacuum = INCREMENTAL;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA journal_size_limit = {WAL_KEPT};
         PRAGMA temp_store = MEMORY;
         PRAGMA busy_timeout = 5000;"
    ))?;
    timeless_ext::register_telemetry(&connection)
        .map_err(|error| anyhow!("register the timeless engine: {error}"))?;

    let capabilities: String =
        connection.query_row("SELECT timeless_capabilities()", [], |row| row.get(0))?;
    let capabilities: serde_json::Value = serde_json::from_str(&capabilities)?;
    if capabilities["data_abi"] != 1 {
        bail!(
            "the timeless engine speaks data ABI {}, and this collector speaks 1",
            capabilities["data_abi"]
        );
    }
    Ok(connection)
}

/// Give the kernel back what was used and freed.
///
/// Compaction decodes and rewrites much of a store at once, and a burst
/// of processes ending is held until it is flushed: either is a heap
/// several times the size the collector otherwise runs in. glibc keeps
/// what is freed for the next allocation of that size, which may be an
/// hour away, or never come.
pub(crate) fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim takes no pointer and leaves every allocation
    // as it was.
    unsafe {
        libc::malloc_trim(0);
    }
}

pub struct EmbeddedSink {
    metrics: Connection,
    logs: Connection,
    traces: Connection,
    dir: PathBuf,
    limit: Option<u64>,
    _leases: [File; 3],
}

/// What is in the store, in the order it is given up when the store is
/// over its limit: the least valuable first. Samples are most of the
/// bytes and nobody looks at a process's series from three weeks ago;
/// spans are interesting for as long as their exits are; rollups go
/// finest first, since the coarsest is the long view; records are the
/// audit trail, at forty bytes each, and go last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Samples,
    Spans,
    /// Samples rolled up to this many seconds.
    Rollups(i64),
    Records,
}

impl Kind {
    /// The three the store always has, one to a file.
    const FILES: [Kind; 3] = [Kind::Samples, Kind::Spans, Kind::Records];

    fn name(self) -> String {
        match self {
            Kind::Samples => "samples".into(),
            Kind::Spans => "spans".into(),
            Kind::Rollups(resolution) => {
                format!("{} rollups", human_duration(resolution as f64))
            }
            Kind::Records => "records".into(),
        }
    }

    /// Timestamps as the store keeps them: seconds, nanoseconds, and
    /// microseconds to the second.
    fn per_second(self) -> i64 {
        match self {
            Kind::Samples | Kind::Rollups(_) => 1,
            Kind::Spans => 1_000_000_000,
            Kind::Records => 1_000_000,
        }
    }
}

/// What the store gave up to fit its limit: a kind, and the moment
/// before which it is gone, in epoch seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pruned {
    pub kind: Kind,
    pub before: f64,
}

/// The last hour of anything is never pruned: a limit under an hour of
/// data is a misconfiguration, not a policy.
const KEPT_WHATEVER: i64 = 3600;

impl EmbeddedSink {
    pub fn open(options: &EmbeddedOptions) -> Result<Self> {
        // A store holds what every process on the host was run with. It
        // is its owner's to read, and no one else's.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&options.dir)
            .with_context(|| format!("create {}", options.dir.display()))?;
        let metrics_path = options.dir.join(METRICS_DB);
        let logs_path = options.dir.join(LOGS_DB);
        let traces_path = options.dir.join(TRACES_DB);
        let leases = [
            lease(&metrics_path)?,
            lease(&logs_path)?,
            lease(&traces_path)?,
        ];

        let metrics = open(&metrics_path)?;
        let mut arguments = vec![table_argument("retention", &options.retention)?];
        if !options.rollups.is_empty() {
            arguments.push(table_argument("rollups", &options.rollups)?);
        }
        // The arguments of an existing table are the ones it was created
        // with; these apply to a new one.
        metrics.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {METRICS_TABLE}
             USING timeless_metrics({});",
            arguments.join(", ")
        ))?;

        let logs = open(&logs_path)?;
        logs.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {LOGS_TABLE}
             USING timeless_logs(index_keys='{LOG_INDEX_KEYS}', timestamp_unit='us', {});",
            table_argument("retention", &options.log_retention)?
        ))?;

        let traces = open(&traces_path)?;
        traces.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {TRACES_TABLE}
             USING timeless_traces({});",
            table_argument("retention", &options.trace_retention)?
        ))?;

        Ok(Self {
            metrics,
            logs,
            traces,
            dir: options.dir.clone(),
            limit: options.limit,
            _leases: leases,
        })
    }

    fn connection(&self, kind: Kind) -> (&Connection, &str, &str) {
        match kind {
            Kind::Samples | Kind::Rollups(_) => (&self.metrics, METRICS_TABLE, METRICS_DB),
            Kind::Spans => (&self.traces, TRACES_TABLE, TRACES_DB),
            Kind::Records => (&self.logs, LOGS_TABLE, LOGS_DB),
        }
    }

    /// The rollup ladder the store keeps, as the engine persists it:
    /// `(resolution, retention)` in seconds, finest first; a retention of
    /// zero is forever.
    fn ladder(&self) -> Result<Vec<(i64, i64)>> {
        let spec: Option<String> = self
            .metrics
            .query_row(
                &format!("SELECT CAST(v AS TEXT) FROM {METRICS_TABLE}_meta WHERE k = 'rollups'"),
                [],
                |row| row.get(0),
            )
            .optional()?;
        let mut tiers = Vec::new();
        for part in spec.unwrap_or_default().split(',') {
            if let Some((resolution, retention)) = part.trim().split_once(':') {
                if let (Ok(resolution), Ok(retention)) =
                    (resolution.parse::<i64>(), retention.parse::<i64>())
                {
                    tiers.push((resolution, retention));
                }
            }
        }
        tiers.sort_unstable();
        Ok(tiers)
    }

    /// What the store holds, in the order it is given up.
    fn by_value(&self) -> Result<Vec<Kind>> {
        let mut kinds = vec![Kind::Samples, Kind::Spans];
        kinds.extend(self.ladder()?.into_iter().map(|(r, _)| Kind::Rollups(r)));
        kinds.push(Kind::Records);
        Ok(kinds)
    }

    /// Give up what is older than `cutoff`, in epoch seconds, of a kind.
    /// Samples, spans, and records are pruned outright. A rollup tier is
    /// given a shorter window, which the engine applies at the compaction
    /// that follows; the store's ladder stays shortened.
    fn prune(&self, kind: Kind, cutoff: i64) -> Result<()> {
        let (connection, table, _) = self.connection(kind);
        match kind {
            Kind::Rollups(resolution) => {
                let Some((_, newest)) = self.range(Kind::Samples)? else {
                    return Ok(());
                };
                let ladder = self
                    .ladder()?
                    .into_iter()
                    .map(|(r, kept)| {
                        let kept = if r == resolution {
                            (newest - cutoff).max(KEPT_WHATEVER)
                        } else {
                            kept
                        };
                        if kept == 0 {
                            format!("{r}s@forever")
                        } else {
                            format!("{r}s@{kept}s")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                Self::command(connection, table, &format!("rollups:{ladder}"))?;
                Self::command(connection, table, "compact")?;
            }
            _ => Self::command(
                connection,
                table,
                &format!("prune:{}", cutoff * kind.per_second()),
            )?,
        }
        self.vacuum(connection)?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// What the store takes on disk: the pages in use of each database,
    /// and its write-ahead log. Free pages are not counted; they are
    /// given back at the next vacuum, and a store that has just pruned is
    /// as small as it is about to be.
    pub fn bytes(&self) -> Result<u64> {
        let mut total = 0;
        for kind in Kind::FILES {
            let (connection, _, file) = self.connection(kind);
            let page = |what: &str| -> Result<i64> {
                Ok(connection.query_row(&format!("PRAGMA {what}"), [], |row| row.get(0))?)
            };
            let used = page("page_count")? - page("freelist_count")?;
            total += (used.max(0) * page("page_size")?) as u64;
            let mut log = self.dir.join(file).into_os_string();
            log.push("-wal");
            total += fs::metadata(log).map(|m| m.len()).unwrap_or(0);
        }
        Ok(total)
    }

    /// The first and last moments the store holds of a kind, in epoch
    /// seconds, from what it keeps of each chunk: no payload is read.
    fn range(&self, kind: Kind) -> Result<Option<(i64, i64)>> {
        let (connection, table, _) = self.connection(kind);
        let sql = match kind {
            Kind::Samples => {
                format!("SELECT min(ts_min), max(ts_max) FROM {table}_chunks WHERE resolution = 0")
            }
            Kind::Rollups(resolution) => format!(
                "SELECT min(ts_min), max(ts_max) FROM {table}_chunks WHERE resolution = {resolution}"
            ),
            Kind::Spans | Kind::Records => {
                format!("SELECT min(ts_min), max(ts_max) FROM {table}_blocks")
            }
        };
        let bounds: (Option<i64>, Option<i64>) =
            connection.query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let per = kind.per_second();
        Ok(match bounds {
            (Some(first), Some(last)) => Some((first / per, last / per)),
            _ => None,
        })
    }

    /// Give back every page the store has freed.
    fn vacuum(&self, connection: &Connection) -> Result<()> {
        loop {
            // The pragma answers with a row for each page it frees, and
            // frees a page for each row that is asked for. Run as a
            // statement that answers nothing, it frees one.
            let mut vacuum =
                connection.prepare(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES})"))?;
            let mut freed = vacuum.query([])?;
            let mut rows = 0;
            while freed.next()?.is_some() {
                rows += 1;
            }
            if rows < VACUUM_PAGES {
                return Ok(());
            }
        }
    }

    /// Prune the oldest of the least valuable kind until the store is
    /// under its limit, or until every kind is down to its last hour.
    /// Returns what was pruned, in order, and whether the limit was met.
    pub fn fit(&mut self) -> Result<(Vec<Pruned>, bool)> {
        let Some(limit) = self.limit else {
            return Ok((Vec::new(), true));
        };
        let mut pruned = Vec::new();
        for kind in self.by_value()? {
            // A tenth of what is held at a time, so that it fits in a few
            // rounds and lands near the limit. Pruning is chunk-granular:
            // a chunk that reaches past the cutoff stays whole, so a round
            // that frees nothing doubles the step rather than giving up.
            let mut step: Option<i64> = None;
            loop {
                if self.bytes()? <= limit {
                    return Ok((pruned, true));
                }
                let Some((first, last)) = self.range(kind)? else {
                    break;
                };
                let held = last - first;
                let this = step.unwrap_or((held / 10).max(60));
                if held <= KEPT_WHATEVER || this >= held {
                    break;
                }
                // Never into the last hour.
                let cutoff = (first + this).min(last - KEPT_WHATEVER);
                let before = self.bytes()?;
                self.prune(kind, cutoff)?;
                // A rollup tier is judged by its reach alone: the pass that
                // applies its window merges chunks too, and that frees
                // bytes without giving anything up.
                let moved = self.range(kind)? != Some((first, last));
                let freed = !matches!(kind, Kind::Rollups(_)) && self.bytes()? < before;
                if !moved && !freed {
                    if cutoff >= last - KEPT_WHATEVER {
                        break;
                    }
                    step = Some(this * 2);
                    continue;
                }
                step = None;
                match pruned.last_mut() {
                    // One line per kind, saying how far back it reached.
                    Some(Pruned { kind: same, before }) if *same == kind => {
                        *before = cutoff as f64;
                    }
                    _ => pruned.push(Pruned {
                        kind,
                        before: cutoff as f64,
                    }),
                }
            }
        }
        Ok((pruned, self.bytes()? <= limit))
    }

    fn command(connection: &Connection, table: &str, command: &str) -> Result<()> {
        connection
            .execute(
                &format!("INSERT INTO {table}({table}) VALUES (?1)"),
                params![command],
            )
            .with_context(|| format!("{table}: {command}"))?;
        Ok(())
    }
}

impl Sink for EmbeddedSink {
    fn write(&mut self, host: &str, tick: &Tick) -> Result<()> {
        let Tick {
            metrics,
            events,
            spans,
        } = *tick;
        if !metrics.samples.is_empty() {
            // Bound as a BLOB: the engine reads TEXT in this column as a
            // command and a BLOB as a body to ingest.
            let body = prometheus_text(host, metrics);
            self.metrics
                .execute(
                    &format!("INSERT INTO {METRICS_TABLE}({METRICS_TABLE}) VALUES (?1)"),
                    params![body.as_bytes()],
                )
                .context("store samples")?;
        }
        if !events.is_empty() {
            let transaction = self.logs.transaction()?;
            {
                let mut insert = transaction.prepare_cached(&format!(
                    "INSERT INTO {LOGS_TABLE}(ts, level, message, metadata)
                     VALUES (?1, ?2, ?3, ?4)"
                ))?;
                for event in events {
                    let metadata = serde_json::Value::Object(event_metadata(host, event));
                    insert.execute(params![
                        event.ts_us,
                        event.level.as_str(),
                        event.message,
                        metadata.to_string()
                    ])?;
                }
            }
            transaction.commit().context("store accounting records")?;
        }
        if !spans.is_empty() {
            let scope = scope().to_string();
            let transaction = self.traces.transaction()?;
            {
                let mut insert = transaction.prepare_cached(&format!(
                    "INSERT INTO {TRACES_TABLE}(
                       trace_id, span_id, parent_span_id, name, service, kind, status,
                       start_ts, duration_ns, attributes, status_description, resource,
                       instrumentation_scope
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'internal', ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
                ))?;
                for span in spans {
                    insert.execute(params![
                        span.trace_id.as_slice(),
                        span.span_id.as_slice(),
                        span.parent_span_id.as_ref().map(|id| id.as_slice()),
                        span.name,
                        span.service(host),
                        span.status(),
                        span.start_ns,
                        span.duration_ns,
                        serde_json::Value::Object(span.attributes.clone()).to_string(),
                        span.ending,
                        serde_json::Value::Object(resource(host, span)).to_string(),
                        scope,
                    ])?;
                }
            }
            transaction.commit().context("store spans")?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Self::command(&self.metrics, METRICS_TABLE, "flush")?;
        Self::command(&self.logs, LOGS_TABLE, "flush")?;
        Self::command(&self.traces, TRACES_TABLE, "flush")?;
        // What was waiting to be written is written. A build that ends
        // five thousand processes in a minute leaves that much behind:
        // in the pages SQLite keeps of what it wrote, which it holds on
        // to until it is asked, and in the allocator.
        for connection in [&self.metrics, &self.logs, &self.traces] {
            connection.execute_batch("PRAGMA shrink_memory;")?;
        }
        release_freed_memory();
        Ok(())
    }

    fn maintain(&mut self) -> Result<()> {
        Self::command(&self.metrics, METRICS_TABLE, "compact")?;
        Self::command(
            &self.logs,
            LOGS_TABLE,
            &format!("optimize:{OPTIMIZE_BUDGET}"),
        )?;
        Self::command(
            &self.traces,
            TRACES_TABLE,
            &format!("optimize:{OPTIMIZE_BUDGET}"),
        )?;
        for connection in [&self.metrics, &self.logs, &self.traces] {
            // Compaction replaces many small chunks with few large ones;
            // give the pages that freed back to the filesystem.
            //
            // The pragma answers with a row for each page it frees, and
            // frees a page for each row that is asked for. Run as a
            // statement that answers nothing, it frees one.
            let mut vacuum =
                connection.prepare(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES})"))?;
            let mut freed = vacuum.query([])?;
            while freed.next()?.is_some() {}
            drop(freed);
            drop(vacuum);
            // All of that went through the write-ahead log, which keeps
            // the size it grew to until it is cut back. A reader in the
            // way leaves it for the next pass.
            connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        let (pruned, fits) = self.fit()?;
        if let Some(limit) = self.limit {
            for Pruned { kind, before } in &pruned {
                eprintln!(
                    "timeless-acct: the store is over {}: {} before {} are gone",
                    human_bytes(limit),
                    kind.name(),
                    clock::format(*before)
                );
            }
            if !fits {
                eprintln!(
                    "timeless-acct: the store is {} with the last hour of everything, over the limit of {}",
                    human_bytes(self.bytes()?),
                    human_bytes(limit)
                );
            }
        }
        release_freed_memory();
        Ok(())
    }

    fn footprint(&self) -> Option<Footprint> {
        Some(Footprint {
            bytes: self.bytes().ok()?,
            limit: self.limit,
        })
    }

    fn close(&mut self) -> Result<()> {
        self.flush()?;
        for connection in [&self.metrics, &self.logs, &self.traces] {
            // Leave one file per database behind, with nothing in a WAL
            // that a copy of the directory could miss.
            connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("embedded: {}", self.dir.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{labels, no_labels, Event, Level, MetricBatch, Span};
    use crate::testutil::Fixture;
    use serde_json::{Map, Value};

    fn span(id: u8, parent: Option<u8>, name: &str, ok: bool) -> Span {
        let mut attributes = Map::new();
        attributes.insert("process.pid".into(), Value::from(4000 + i64::from(id)));
        attributes.insert(
            "process.command_line".into(),
            Value::from(format!("{name} -x")),
        );
        Span {
            trace_id: [0xab; 16],
            span_id: [id; 8],
            parent_span_id: parent.map(|id| [id; 8]),
            name: name.into(),
            unit: "build.service".into(),
            ok: Some(ok),
            ending: if ok { String::new() } else { "exited 1".into() },
            start_ns: 1_753_000_000_000_000_000 + i64::from(id),
            duration_ns: 2_500_000_000,
            attributes,
        }
    }

    fn options(fixture: &Fixture) -> EmbeddedOptions {
        EmbeddedOptions {
            dir: fixture.path("data"),
            ..EmbeddedOptions::default()
        }
    }

    fn batch(ts: i64, cpu: f64) -> MetricBatch {
        let mut batch = MetricBatch::new(ts);
        batch.push("sys_load1", &no_labels(), 0.25);
        batch.push(
            "proc_cpu_pct",
            &labels(vec![
                ("pid", "42".into()),
                ("comm", "postgres".into()),
                ("proc", "postgres[42]".into()),
            ]),
            cpu,
        );
        batch
    }

    fn event(ts_us: i64, status: &str, level: Level) -> Event {
        let mut fields = Map::new();
        fields.insert("service".into(), Value::from("postgres"));
        fields.insert("status".into(), Value::from(status));
        fields.insert("cpu_seconds".into(), Value::from(1.5));
        Event {
            ts_us,
            level,
            message: format!("postgres[42] ended with {status}"),
            fields,
        }
    }

    #[test]
    fn samples_and_records_survive_a_reopen() {
        let fixture = Fixture::new("embedded_reopen");
        let options = options(&fixture);
        {
            let mut sink = EmbeddedSink::open(&options).unwrap();
            sink.write(
                "host-a",
                &Tick {
                    metrics: &batch(1_753_000_000, 12.5),
                    events: &[],
                    spans: &[],
                },
            )
            .unwrap();
            sink.write(
                "host-a",
                &Tick {
                    metrics: &batch(1_753_000_010, 50.0),
                    events: &[
                        event(1_753_000_010_000_000, "0", Level::Info),
                        event(1_753_000_011_000_000, "SIGSEGV", Level::Error),
                    ],
                    spans: &[
                        span(2, Some(1), "rustc", false),
                        span(1, None, "cargo", true),
                    ],
                },
            )
            .unwrap();
            sink.maintain().unwrap();
            sink.close().unwrap();
        }

        let sink = EmbeddedSink::open(&options).unwrap();
        for connection in [&sink.metrics, &sink.logs, &sink.traces] {
            let mode: i64 = connection
                .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, 2, "incremental");
        }
        let points: Vec<(i64, f64)> = sink
            .metrics
            .prepare(
                "SELECT ts, value FROM metric_samples
                  WHERE name = 'proc_cpu_pct' ORDER BY ts",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        // Exposition timestamps are milliseconds; the store's are seconds.
        assert_eq!(points, [(1_753_000_000, 12.5), (1_753_000_010, 50.0)]);

        let stored: String = sink
            .metrics
            .query_row(
                "SELECT labels FROM metric_samples WHERE name = 'proc_cpu_pct' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let stored: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(stored["host"], "host-a");
        assert_eq!(stored["proc"], "postgres[42]");

        // The indexed keys select without a scan of the metadata.
        let (level, message, metadata): (String, String, String) = sink
            .logs
            .query_row(
                "SELECT level, message, metadata FROM logs
                  WHERE service = 'postgres' AND status = 'SIGSEGV' AND host = 'host-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(level, "error");
        assert_eq!(message, "postgres[42] ended with SIGSEGV");
        let metadata: Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(metadata["cpu_seconds"], 1.5);
        let records: i64 = sink
            .logs
            .query_row("SELECT count(*) FROM logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(records, 2);

        // A trace is found by its id, and comes back as a tree.
        type Row = (
            String,
            Vec<u8>,
            Option<Vec<u8>>,
            String,
            String,
            String,
            String,
        );
        let spans: Vec<Row> = sink
            .traces
            .prepare(
                "SELECT name, span_id, parent_span_id, status, status_description,
                        service, attributes
                   FROM traces WHERE trace_id = ?1 ORDER BY start_ts",
            )
            .unwrap()
            .query_map(params![[0xab_u8; 16].as_slice()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(spans.len(), 2);
        let (cargo, rustc) = (&spans[0], &spans[1]);
        assert_eq!((cargo.0.as_str(), cargo.3.as_str()), ("cargo", "ok"));
        assert_eq!((rustc.0.as_str(), rustc.3.as_str()), ("rustc", "error"));
        assert_eq!(rustc.4, "exited 1");
        assert_eq!(rustc.5, "host-a/build.service");
        assert_eq!(rustc.2.as_deref(), Some(cargo.1.as_slice()));
        // A root has no parent, however the store writes that.
        assert!(cargo.2.as_ref().is_none_or(|id| id.iter().all(|b| *b == 0)));
        let attributes: Value = serde_json::from_str(&rustc.6).unwrap();
        assert_eq!(attributes["process.pid"], 4002);
        assert_eq!(attributes["process.command_line"], "rustc -x");

        // And is found by what ran and how it ended, without its id.
        let failed: i64 = sink
            .traces
            .query_row(
                "SELECT count(*) FROM traces
                  WHERE service = 'host-a/build.service' AND name = 'rustc' AND status = 'error'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(failed, 1);
    }

    /// Ten hours of everything, written an hour at a time.
    fn store_of_ten_hours(fixture: &Fixture, limit: Option<u64>) -> EmbeddedSink {
        let options = EmbeddedOptions {
            dir: fixture.path("data"),
            limit,
            ..EmbeddedOptions::default()
        };
        let mut sink = EmbeddedSink::open(&options).unwrap();
        for hour in 0..10_i64 {
            let begin = 1_753_000_000 + hour * 3600;
            for minute in 0..60 {
                let at = begin + minute * 60;
                let mut metrics = MetricBatch::new(at);
                for series in 0..400 {
                    metrics.push(
                        "proc_cpu_pct",
                        &labels(vec![("proc", format!("worker[{series}]"))]),
                        f64::from(series) + (minute % 7) as f64,
                    );
                }
                let events: Vec<Event> = (0..20)
                    .map(|n| event((at * 1_000_000) + n, "0", Level::Info))
                    .collect();
                let spans: Vec<Span> = (0..20)
                    .map(|n| {
                        let mut span = span(n as u8, None, "sh", true);
                        span.trace_id = [
                            (hour as u8),
                            (minute as u8),
                            n as u8,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            1,
                        ];
                        span.start_ns = at * 1_000_000_000 + i64::from(n);
                        span
                    })
                    .collect();
                sink.write(
                    "host-a",
                    &Tick {
                        metrics: &metrics,
                        events: &events,
                        spans: &spans,
                    },
                )
                .unwrap();
            }
            // As the collector does: flushed each minute, compacted each
            // hour, so a series has a chunk an hour.
            sink.flush().unwrap();
            sink.maintain().unwrap();
        }
        sink
    }

    #[test]
    fn over_its_limit_the_store_gives_up_the_oldest_samples_first_and_records_last() {
        let fixture = Fixture::new("embedded_limit");
        let mut sink = store_of_ten_hours(&fixture, None);
        let full = sink.bytes().unwrap();
        let hour = |sink: &EmbeddedSink, kind: Kind| -> (i64, i64) {
            let (first, last) = sink.range(kind).unwrap().unwrap();
            (
                (first - 1_753_000_000) / 3600,
                (last - 1_753_000_000) / 3600,
            )
        };
        assert_eq!(hour(&sink, Kind::Samples), (0, 9));
        assert_eq!(hour(&sink, Kind::Records), (0, 9));
        assert_eq!(hour(&sink, Kind::Spans), (0, 9));

        // Under the limit, nothing goes.
        sink.limit = Some(full * 2);
        let (pruned, fits) = sink.fit().unwrap();
        assert!(pruned.is_empty() && fits);

        // A little over: the oldest samples go, and nothing else.
        sink.limit = Some(full * 9 / 10);
        let (pruned, fits) = sink.fit().unwrap();
        assert!(fits);
        assert!(!pruned.is_empty());
        assert!(pruned.iter().all(|p| p.kind == Kind::Samples), "{pruned:?}");
        assert!(hour(&sink, Kind::Samples).0 > 0);
        assert_eq!(hour(&sink, Kind::Records), (0, 9));
        assert_eq!(hour(&sink, Kind::Spans), (0, 9));
        assert!(sink.bytes().unwrap() <= full * 9 / 10);

        // Far over: samples down to their last hour, then spans, then
        // records, and the last hour of each is kept whatever the limit.
        sink.limit = Some(1);
        let (pruned, fits) = sink.fit().unwrap();
        assert!(!fits);
        // Samples, spans, the five-minute rollups, the hourly, records.
        let order: Vec<Kind> = pruned.iter().map(|p| p.kind).collect();
        let mut seen = Vec::new();
        for kind in &order {
            if seen.last() != Some(kind) {
                seen.push(*kind);
            }
        }
        // A tier whose merged chunks all reach into the last hours has
        // nothing to give, and is passed over.
        let by_value = [
            Kind::Samples,
            Kind::Spans,
            Kind::Rollups(300),
            Kind::Rollups(3600),
            Kind::Records,
        ];
        let mut expected = by_value.iter();
        for kind in &seen {
            assert!(expected.any(|k| k == kind), "{order:?}");
        }
        assert!(
            seen.len() >= 3
                && seen.first() == Some(&Kind::Samples)
                && seen.last() == Some(&Kind::Records),
            "{order:?}"
        );
        // The last hour, and the chunk that reaches it: pruning is
        // chunk-granular, and a chunk here is an hour.
        for kind in Kind::FILES {
            let (first, last) = sink.range(kind).unwrap().unwrap();
            assert!(last - first <= 2 * 3600, "{kind:?} holds {}s", last - first);
            assert_eq!((last - 1_753_000_000) / 3600, 9, "{kind:?} lost its newest");
        }
        // Two hours of ten of samples, spans, and records; the rollups,
        // whose merged chunks reach into the last hours and stay whole;
        // and the series, which are kept until the engine lets them go.
        let left = sink.bytes().unwrap();
        assert!(left < full / 2, "{left} of {full}");
        let (first, last) = sink.range(Kind::Rollups(300)).unwrap().unwrap();
        assert!(
            last - first < 9 * 3600,
            "the five-minute tier gave up nothing"
        );

        // What a reader finds is what is left.
        let points: i64 = sink
            .metrics
            .query_row("SELECT count(*) FROM metric_samples", [], |row| row.get(0))
            .unwrap();
        assert!(points > 0 && points <= 2 * 60 * 400, "{points}");
    }

    #[test]
    fn maintenance_fits_the_store_to_its_limit() {
        let fixture = Fixture::new("embedded_limit_maintain");
        let sink = store_of_ten_hours(&fixture, Some(1));
        // The store was made over the limit and then maintained once.
        for kind in Kind::FILES {
            let (first, last) = sink.range(kind).unwrap().unwrap();
            assert!(last - first <= 2 * 3600, "{kind:?}");
        }
        assert!(sink.footprint().unwrap().limit == Some(1));
    }

    #[test]
    fn what_compaction_frees_is_given_back() {
        let fixture = Fixture::new("embedded_vacuum");
        let mut sink = EmbeddedSink::open(&options(&fixture)).unwrap();
        // Many flushes of a few samples each: many small chunks.
        for tick in 0..120 {
            let mut metrics = MetricBatch::new(1_753_000_000 + 10 * tick);
            for series in 0..40 {
                metrics.push(
                    "proc_cpu_pct",
                    &labels(vec![("proc", format!("worker[{series}]"))]),
                    f64::from(series) + 0.5,
                );
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
            sink.flush().unwrap();
        }
        let pages = |sink: &EmbeddedSink, what: &str| -> i64 {
            sink.metrics
                .query_row(&format!("PRAGMA {what}"), [], |row| row.get(0))
                .unwrap()
        };
        let before = pages(&sink, "page_count");

        sink.maintain().unwrap();
        assert_eq!(pages(&sink, "freelist_count"), 0);
        let after = pages(&sink, "page_count");
        assert!(after * 2 < before, "{after} pages, of {before} before");

        // And the log it was all written through holds none of it.
        let log = fixture.path("data").join("metrics.db-wal");
        assert_eq!(fs::metadata(&log).unwrap().len(), 0);

        let points: i64 = sink
            .metrics
            .query_row("SELECT count(*) FROM metric_samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(points, 120 * 40);
    }

    #[test]
    fn a_store_is_its_owners_to_read() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new("embedded_private");
        let options = options(&fixture);
        let _sink = EmbeddedSink::open(&options).unwrap();
        let mode = fs::metadata(&options.dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_directory_has_one_owner() {
        let fixture = Fixture::new("embedded_owner");
        let options = options(&fixture);
        let first = EmbeddedSink::open(&options).unwrap();
        let error = EmbeddedSink::open(&options).err().unwrap().to_string();
        assert!(error.contains("owned by another process"), "{error}");
        drop(first);
        EmbeddedSink::open(&options).unwrap();
    }

    #[test]
    fn table_arguments_cannot_carry_sql() {
        assert_eq!(
            table_argument("retention", "30d").unwrap(),
            "retention='30d'"
        );
        assert_eq!(
            table_argument("rollups", "5m@30d,1h@180d").unwrap(),
            "rollups='5m@30d,1h@180d'"
        );
        assert!(table_argument("retention", "30d'); DROP TABLE logs; --").is_err());
        assert!(table_argument("retention", "").is_err());
    }
}
