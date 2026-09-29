//! Print what would be stored. For looking at what the collector sees.

use std::io::{self, Write};

use anyhow::Result;

use crate::encode::{ndjson, otlp_json, prometheus_text};

use super::{Sink, Tick};

#[derive(Default)]
pub struct StdoutSink;

impl Sink for StdoutSink {
    fn write(&mut self, host: &str, tick: &Tick) -> Result<()> {
        let mut out = io::stdout().lock();
        out.write_all(prometheus_text(host, tick.metrics).as_bytes())?;
        out.write_all(ndjson(host, tick.events).as_bytes())?;
        if !tick.spans.is_empty() {
            writeln!(out, "{}", otlp_json(host, tick.spans))?;
        }
        out.flush()?;
        Ok(())
    }

    fn describe(&self) -> String {
        "stdout".into()
    }
}
