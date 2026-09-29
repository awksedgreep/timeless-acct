//! Wire encodings shared by every sink.
//!
//! Metrics travel as Prometheus exposition text and events as NDJSON. Both
//! the embedded engine and the HTTP planes accept exactly these, so there is
//! one encoder and the two sinks cannot drift apart.

use std::fmt::Write;

use serde_json::{Map, Value};

use crate::lineage::hex;
use crate::model::{Event, MetricBatch, Span};

/// `name{host="h",k="v"} value timestamp_ms`, one line per sample.
///
/// Exposition timestamps are milliseconds by specification; the engine
/// normalizes them back to the seconds it stores.
pub fn prometheus_text(host: &str, batch: &MetricBatch) -> String {
    let mut out = String::with_capacity(batch.samples.len() * 96);
    let ts_ms = batch.ts * 1000;
    let mut host_label = String::new();
    push_escaped(&mut host_label, host);

    for sample in &batch.samples {
        out.push_str(sample.name);
        out.push_str("{host=\"");
        out.push_str(&host_label);
        out.push('"');
        for (key, value) in sample.labels.iter() {
            out.push(',');
            out.push_str(key);
            out.push_str("=\"");
            push_escaped(&mut out, value);
            out.push('"');
        }
        // Display for f64 is the shortest string that parses back to the
        // same bits, so the value survives the text hop exactly.
        let _ = writeln!(out, "}} {} {}", sample.value, ts_ms);
    }
    out
}

fn push_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
}

/// The metadata object stored with an event: its fields plus `host`.
pub fn event_metadata(host: &str, event: &Event) -> Map<String, Value> {
    let mut object = event.fields.clone();
    object.insert("host".into(), Value::String(host.into()));
    object
}

/// One JSON object per line, in the shape `/insert/jsonline` reads.
pub fn ndjson(host: &str, events: &[Event]) -> String {
    let mut out = String::new();
    for event in events {
        let mut object = event_metadata(host, event);
        object.insert("_msg".into(), Value::String(event.message.clone()));
        object.insert("_time".into(), Value::from(event.ts_us));
        object.insert("level".into(), Value::String(event.level.as_str().into()));
        out.push_str(&Value::Object(object).to_string());
        out.push('\n');
    }
    out
}

/// What every span of this collector's is said to come from.
pub fn scope() -> Value {
    serde_json::json!({"name": "timeless-acct", "version": env!("CARGO_PKG_VERSION")})
}

/// What a span's resource is: the unit, on the host.
pub fn resource(host: &str, span: &Span) -> Map<String, Value> {
    let mut resource = Map::new();
    resource.insert("service.name".into(), span.service.clone().into());
    resource.insert("host.name".into(), host.into());
    resource
}

/// A value as OTLP writes one: tagged with its type.
///
/// OTLP writes a 64-bit integer as a string, since a JSON number cannot be
/// trusted to hold one, and readers are to take either. The traces plane
/// keeps what it is given as it is given, and an integer sent as a string
/// is stored as a string. Nothing counted here comes near the 53 bits a
/// JSON number holds exactly, so integers are sent as numbers, and are the
/// same in the planes as in a local store.
fn any_value(value: &Value) -> Value {
    match value {
        Value::Bool(flag) => serde_json::json!({ "boolValue": flag }),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            serde_json::json!({ "intValue": number })
        }
        Value::Number(number) => serde_json::json!({ "doubleValue": number }),
        Value::String(text) => serde_json::json!({ "stringValue": text }),
        other => serde_json::json!({ "stringValue": other.to_string() }),
    }
}

fn key_values(object: &Map<String, Value>) -> Value {
    object
        .iter()
        .map(|(key, value)| serde_json::json!({"key": key, "value": any_value(value)}))
        .collect()
}

/// An OTLP export request, as JSON: the spans, under the resource each
/// belongs to.
pub fn otlp_json(host: &str, spans: &[Span]) -> String {
    let mut services: Vec<&str> = spans.iter().map(|span| span.service.as_str()).collect();
    services.sort_unstable();
    services.dedup();

    let resource_spans: Vec<Value> = services
        .into_iter()
        .map(|service| {
            let of_service: Vec<&Span> = spans
                .iter()
                .filter(|span| span.service == service)
                .collect();
            let encoded: Vec<Value> = of_service
                .iter()
                .map(|span| {
                    let mut out = serde_json::json!({
                        "traceId": hex(&span.trace_id),
                        "spanId": hex(&span.span_id),
                        "name": span.name,
                        "kind": 1, // internal
                        "startTimeUnixNano": span.start_ns.to_string(),
                        "endTimeUnixNano": (span.start_ns + span.duration_ns).to_string(),
                        "attributes": key_values(&span.attributes),
                        "status": {
                            "code": match span.ok {
                                None => 0,
                                Some(true) => 1,
                                Some(false) => 2,
                            },
                            "message": span.ending,
                        },
                    });
                    if let Some(parent) = span.parent_span_id {
                        out["parentSpanId"] = hex(&parent).into();
                    }
                    out
                })
                .collect();
            serde_json::json!({
                "resource": {"attributes": key_values(&resource(host, of_service[0]))},
                "scopeSpans": [{"scope": scope(), "spans": encoded}],
            })
        })
        .collect();
    serde_json::json!({ "resourceSpans": resource_spans }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{labels, no_labels, Level, Sample};

    #[test]
    fn text_carries_host_labels_and_millisecond_timestamp() {
        let mut batch = MetricBatch::new(1_753_000_000);
        batch.push("sys_load1", &no_labels(), 0.5);
        batch.push(
            "proc_cpu_pct",
            &labels(vec![("pid", "42".into()), ("comm", "a\"b\\c".into())]),
            12.0,
        );
        let text = prometheus_text("ohm", &batch);
        assert_eq!(
            text,
            "sys_load1{host=\"ohm\"} 0.5 1753000000000\n\
             proc_cpu_pct{host=\"ohm\",pid=\"42\",comm=\"a\\\"b\\\\c\"} 12 1753000000000\n"
        );
    }

    #[test]
    fn values_round_trip_through_text() {
        for value in [0.1 + 0.2, 1.0e-9, 123_456_789.123_456_79, 1.0e21] {
            let mut batch = MetricBatch::new(1);
            batch.samples.push(Sample {
                name: "m",
                labels: no_labels(),
                value,
            });
            let text = prometheus_text("h", &batch);
            let field = text.split(' ').nth(1).unwrap();
            assert_eq!(field.parse::<f64>().unwrap().to_bits(), value.to_bits());
        }
    }

    #[test]
    fn non_finite_samples_are_not_stored() {
        let mut batch = MetricBatch::new(1);
        batch.push("m", &no_labels(), f64::NAN);
        batch.push("m", &no_labels(), f64::INFINITY);
        assert!(batch.samples.is_empty());
    }

    fn span(service: &str, parent: Option<[u8; 8]>, ok: Option<bool>) -> Span {
        let mut attributes = Map::new();
        attributes.insert("process.pid".into(), Value::from(4242));
        attributes.insert("process.cpu_seconds".into(), Value::from(2.5));
        attributes.insert("process.forked".into(), Value::from(true));
        attributes.insert("process.owner".into(), Value::from("mark"));
        Span {
            trace_id: [0xab; 16],
            span_id: [0x01; 8],
            parent_span_id: parent,
            name: "rustc".into(),
            service: service.into(),
            ok,
            ending: "exited 1".into(),
            start_ns: 1_753_000_000_000_000_000,
            duration_ns: 2_500_000_000,
            attributes,
        }
    }

    #[test]
    fn spans_are_exported_under_the_unit_they_ran_in() {
        let spans = [
            span("build.service", None, Some(true)),
            span("mark/app-term.scope", Some([0x02; 8]), Some(false)),
            span("build.service", None, None),
        ];
        let body: Value = serde_json::from_str(&otlp_json("ohm", &spans)).unwrap();
        let resources = body["resourceSpans"].as_array().unwrap();
        assert_eq!(resources.len(), 2);

        let build = &resources[0];
        assert_eq!(
            build["resource"]["attributes"][0],
            serde_json::json!({"key": "host.name", "value": {"stringValue": "ohm"}})
        );
        assert_eq!(
            build["resource"]["attributes"][1]["value"]["stringValue"],
            "build.service"
        );
        let of_build = build["scopeSpans"][0]["spans"].as_array().unwrap();
        assert_eq!(of_build.len(), 2);
        assert_eq!(of_build[0]["status"]["code"], 1);
        assert_eq!(of_build[1]["status"]["code"], 0);
        assert!(of_build[0].get("parentSpanId").is_none());

        let one = &resources[1]["scopeSpans"][0]["spans"][0];
        assert_eq!(one["traceId"], "ab".repeat(16));
        assert_eq!(one["spanId"], "01".repeat(8));
        assert_eq!(one["parentSpanId"], "02".repeat(8));
        assert_eq!(one["name"], "rustc");
        assert_eq!(one["startTimeUnixNano"], "1753000000000000000");
        assert_eq!(one["endTimeUnixNano"], "1753000002500000000");
        assert_eq!(
            one["status"],
            serde_json::json!({"code": 2, "message": "exited 1"})
        );
        let attribute = |key: &str| {
            one["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == key)
                .map(|a| a["value"].clone())
                .unwrap()
        };
        assert_eq!(
            attribute("process.pid"),
            serde_json::json!({"intValue": 4242})
        );
        assert_eq!(
            attribute("process.cpu_seconds"),
            serde_json::json!({"doubleValue": 2.5})
        );
        assert_eq!(
            attribute("process.forked"),
            serde_json::json!({"boolValue": true})
        );
        assert_eq!(
            attribute("process.owner"),
            serde_json::json!({"stringValue": "mark"})
        );
    }

    #[test]
    fn ndjson_lines_carry_message_time_level_and_host() {
        let mut fields = Map::new();
        fields.insert("service".into(), Value::String("bash".into()));
        fields.insert("pid".into(), Value::from(7));
        let event = Event {
            ts_us: 1_753_000_000_000_001,
            level: Level::Warning,
            message: "bash[7] killed".into(),
            fields,
        };
        let text = ndjson("ohm", &[event]);
        let line: Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(line["_msg"], "bash[7] killed");
        assert_eq!(line["_time"], 1_753_000_000_000_001_i64);
        assert_eq!(line["level"], "warning");
        assert_eq!(line["host"], "ohm");
        assert_eq!(line["service"], "bash");
        assert_eq!(line["pid"], 7);
    }
}
