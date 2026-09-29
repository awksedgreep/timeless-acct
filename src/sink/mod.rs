//! Where samples and accounting records go.

#[cfg(feature = "embedded")]
pub mod embedded;
#[cfg(feature = "http")]
pub mod http;
pub mod stdout;

use anyhow::Result;

use crate::model::{Event, MetricBatch, Span};

/// What one tick produced.
pub struct Tick<'a> {
    pub metrics: &'a MetricBatch,
    /// Accounting records.
    pub events: &'a [Event],
    pub spans: &'a [Span],
}

impl Tick<'_> {
    pub fn is_empty(&self) -> bool {
        self.metrics.samples.is_empty() && self.events.is_empty() && self.spans.is_empty()
    }
}

pub trait Sink {
    /// Store what one tick produced.
    fn write(&mut self, host: &str, tick: &Tick) -> Result<()>;

    /// Make everything written so far survive a crash.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// Compaction, rollups, retention: the work a store needs done now and
    /// then, and that nothing else will do for an embedded one.
    fn maintain(&mut self) -> Result<()> {
        Ok(())
    }

    /// The last flush, and whatever a clean shutdown owes the store.
    fn close(&mut self) -> Result<()> {
        self.flush()
    }

    /// A line for the startup banner.
    fn describe(&self) -> String;
}
