//! The two things a collector produces: gauge samples and accounting events.
//!
//! Every metric is a gauge. The canvas reads the last value in a bucket and
//! draws it, so a raw kernel counter would plot as an ever-rising line and
//! rewind to a meaningless number. Rates are computed here, once, the way
//! sar records them.

use std::sync::Arc;

use serde_json::{Map, Value};

/// A label set shared by every sample of one subject (a process, a disk).
/// `host` is not in here; the sink adds it to everything.
pub type Labels = Arc<Vec<(&'static str, String)>>;

pub fn labels(pairs: Vec<(&'static str, String)>) -> Labels {
    Arc::new(pairs)
}

pub fn no_labels() -> Labels {
    Arc::new(Vec::new())
}

#[derive(Debug, Clone)]
pub struct Sample {
    pub name: &'static str,
    pub labels: Labels,
    pub value: f64,
}

/// Samples taken at one instant. `ts` is epoch seconds, the metrics unit.
#[derive(Debug, Default)]
pub struct MetricBatch {
    pub ts: i64,
    pub samples: Vec<Sample>,
}

impl MetricBatch {
    pub fn new(ts: i64) -> Self {
        Self {
            ts,
            samples: Vec::new(),
        }
    }

    /// Non-finite values are dropped rather than stored: they come from a
    /// zero-length interval or a counter that went backwards, and a gap is
    /// more honest than a NaN on a graph.
    pub fn push(&mut self, name: &'static str, labels: &Labels, value: f64) {
        if value.is_finite() {
            self.samples.push(Sample {
                name,
                labels: Arc::clone(labels),
                value: measured(value),
            });
        }
    }
}

/// A value at the precision it was measured to: thousandths, or whole
/// units from a thousand up.
///
/// A rate is a difference of integer counters over a measured interval,
/// and the digits past the first few are the interval's jitter, not the
/// subject's behaviour. Storing them costs bytes on the wire and defeats
/// the store's compression, which does best on short decimals.
pub fn measured(value: f64) -> f64 {
    let rounded = if value.abs() >= 1000.0 {
        value.round()
    } else {
        (value * 1000.0).round() / 1000.0
    };
    // Rounding can produce negative zero, which renders as "-0".
    if rounded == 0.0 { 0.0 } else { rounded }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Notice,
    Warning,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Notice => "notice",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

/// One accounting record, stored as a log entry. `ts_us` is epoch
/// microseconds, the unit the logs plane creates its table with.
#[derive(Debug, Clone)]
pub struct Event {
    pub ts_us: i64,
    pub level: Level,
    pub message: String,
    pub fields: Map<String, Value>,
}

/// A process, from its start to its end, as a span of a trace.
#[derive(Debug, Clone)]
pub struct Span {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: Option<[u8; 8]>,
    /// The command's name.
    pub name: String,
    /// The unit it ran in; `-` if it ran in none.
    pub unit: String,
    /// `None` if how it ended is not known.
    pub ok: Option<bool>,
    /// How it ended, in words.
    pub ending: String,
    /// Epoch nanoseconds.
    pub start_ns: i64,
    pub duration_ns: i64,
    pub attributes: Map<String, Value>,
}

impl Span {
    /// What a span's service is: the unit, on the host.
    ///
    /// A unit is what runs on a host as a service does in a system. The
    /// host is part of the name because the planes hold many hosts' spans
    /// together, and can tell spans apart by their service and their name
    /// and by nothing else: `timescaledb.service` on two hosts would be
    /// one service, with no way to ask for either's.
    pub fn service(&self, host: &str) -> String {
        format!("{host}/{}", self.unit)
    }

    /// As the store spells it.
    #[cfg(any(feature = "embedded", test))]
    pub fn status(&self) -> &'static str {
        match self.ok {
            Some(true) => "ok",
            Some(false) => "error",
            None => "unset",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_kept_at_the_precision_they_were_measured_to() {
        assert_eq!(measured(13.997_094_598_813_23), 13.997);
        assert_eq!(measured(0.000_4), 0.0);
        assert_eq!(measured(0.016_7), 0.017);
        assert_eq!(measured(-2.345_6), -2.346);
        assert_eq!(measured(999.999_9), 1000.0);
        assert_eq!(measured(7_180_264.329), 7_180_264.0);
        assert_eq!(measured(40.0), 40.0);
    }
}
