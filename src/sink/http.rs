//! Push to the Timeless metrics, logs, and traces planes.
//!
//! This is the sink the canvas reads from: the planes own their databases,
//! and everything else reaches them over HTTP.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::encode::{ndjson, otlp_json, prometheus_text};

use super::{Sink, Tick};

#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub metrics_url: String,
    pub logs_url: String,
    pub traces_url: String,
    /// Bearer token, for planes started with authentication required.
    pub token: Option<String>,
    pub timeout: Duration,
    /// Ticks kept while a plane is unreachable. At a ten-second interval
    /// the default holds an hour.
    pub backlog: usize,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            metrics_url: "http://127.0.0.1:8428".into(),
            logs_url: "http://127.0.0.1:9428".into(),
            traces_url: "http://127.0.0.1:10428".into(),
            token: None,
            timeout: Duration::from_secs(5),
            backlog: 360,
        }
    }
}

enum Plane {
    Metrics,
    Logs,
    Traces,
}

struct Pending {
    plane: Plane,
    body: String,
}

pub struct HttpSink {
    agent: ureq::Agent,
    metrics_endpoint: String,
    logs_endpoint: String,
    traces_endpoint: String,
    token: Option<String>,
    backlog: VecDeque<Pending>,
    capacity: usize,
    dropped: u64,
}

impl HttpSink {
    pub fn new(options: HttpOptions) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(options.timeout).build();
        Self {
            agent,
            metrics_endpoint: format!(
                "{}/api/v1/import/prometheus",
                options.metrics_url.trim_end_matches('/')
            ),
            logs_endpoint: format!("{}/insert/jsonline", options.logs_url.trim_end_matches('/')),
            traces_endpoint: format!(
                "{}/insert/opentelemetry/v1/traces",
                options.traces_url.trim_end_matches('/')
            ),
            token: options.token,
            backlog: VecDeque::new(),
            // A tick is up to three bodies, one per plane.
            capacity: options.backlog.max(1) * 3,
            dropped: 0,
        }
    }

    fn post(&self, pending: &Pending) -> Result<()> {
        let (url, content_type) = match pending.plane {
            Plane::Metrics => (&self.metrics_endpoint, "text/plain"),
            Plane::Logs => (&self.logs_endpoint, "application/x-ndjson"),
            Plane::Traces => (&self.traces_endpoint, "application/json"),
        };
        let mut request = self.agent.post(url).set("Content-Type", content_type);
        if let Some(token) = &self.token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        match request.send_string(&pending.body) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, response)) => {
                let detail = response.into_string().unwrap_or_default();
                Err(anyhow!("{url} answered {code}: {}", detail.trim()))
            }
            Err(error) => Err(anyhow!("{url}: {error}")),
        }
    }

    /// Send what is waiting, oldest first, and stop at the first failure:
    /// if a plane is down, asking it again for every waiting body would
    /// cost a timeout each.
    fn drain(&mut self) -> Result<()> {
        while let Some(pending) = self.backlog.front() {
            self.post(pending)?;
            self.backlog.pop_front();
        }
        Ok(())
    }
}

impl Sink for HttpSink {
    fn write(&mut self, host: &str, tick: &Tick) -> Result<()> {
        // Every sample, record, and span carries its own time, so one that
        // waits in the backlog is stored at the time it was taken.
        if !tick.metrics.samples.is_empty() {
            self.backlog.push_back(Pending {
                plane: Plane::Metrics,
                body: prometheus_text(host, tick.metrics),
            });
        }
        if !tick.events.is_empty() {
            self.backlog.push_back(Pending {
                plane: Plane::Logs,
                body: ndjson(host, tick.events),
            });
        }
        if !tick.spans.is_empty() {
            self.backlog.push_back(Pending {
                plane: Plane::Traces,
                body: otlp_json(host, tick.spans),
            });
        }
        while self.backlog.len() > self.capacity {
            self.backlog.pop_front();
            self.dropped += 1;
        }
        self.drain().map_err(|error| {
            anyhow!(
                "{error} ({} waiting, {} dropped so far)",
                self.backlog.len(),
                self.dropped
            )
        })
    }

    fn flush(&mut self) -> Result<()> {
        self.drain()
    }

    fn describe(&self) -> String {
        format!(
            "http: {}, {}, and {}",
            self.metrics_endpoint, self.logs_endpoint, self.traces_endpoint
        )
    }
}
