//! OpenTelemetry span export over OTLP/HTTP with JSON encoding.
//!
//! Spans are handed to a bounded queue (never blocking a request; when full
//! they are dropped and counted) and a background task batches them to
//! `{otlpEndpoint}/v1/traces`. The JSON follows the OTLP spec's JSON
//! encoding: lowerCamelCase field names, hex trace/span ids, enum values as
//! integers, and 64-bit integers as decimal strings.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::config::TracingConfig;

/// Spans waiting to be exported; beyond this, new spans are dropped.
const QUEUE_CAPACITY: usize = 4096;

/// Spans sent per export request.
const MAX_BATCH: usize = 512;

/// Longest a span waits in a partial batch.
const EXPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Time allowed for one export request.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// OTLP `SpanKind` values used here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Client = 3,
}

/// A span attribute value.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(String),
    Int(i64),
}

impl From<&str> for AttrValue {
    fn from(s: &str) -> Self {
        AttrValue::Str(s.to_string())
    }
}

impl From<String> for AttrValue {
    fn from(s: String) -> Self {
        AttrValue::Str(s)
    }
}

impl From<i64> for AttrValue {
    fn from(n: i64) -> Self {
        AttrValue::Int(n)
    }
}

pub type Attributes = Vec<(&'static str, AttrValue)>;

/// A timestamped annotation on a span (e.g. a retry).
#[derive(Debug, Clone)]
pub struct SpanEvent {
    pub time: SystemTime,
    pub name: &'static str,
    pub attributes: Attributes,
}

/// One finished span.
#[derive(Debug, Clone)]
pub struct SpanRecord {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub kind: SpanKind,
    pub start: SystemTime,
    pub end: SystemTime,
    pub attributes: Attributes,
    pub events: Vec<SpanEvent>,
    /// `Some(message)` marks the span as an error.
    pub error: Option<String>,
}

enum Command {
    Span(Box<SpanRecord>),
    Flush(oneshot::Sender<()>),
}

/// Handle for recording spans; cheap to share.
pub struct Tracer {
    tx: mpsc::Sender<Command>,
    sample_ratio: f64,
}

impl Tracer {
    /// Start the exporter task for `cfg` (assumed validated).
    pub fn start(cfg: &TracingConfig, node_id: &str) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let endpoint = format!("{}/v1/traces", cfg.otlp_endpoint.trim_end_matches('/'));
        let resource = resource_attributes(&cfg.service_name, node_id);
        info!("exporting spans to {endpoint} (sample ratio {})", cfg.sample_ratio);
        tokio::spawn(run_exporter(rx, endpoint, resource));
        Arc::new(Self { tx, sample_ratio: cfg.sample_ratio })
    }

    /// Decide whether a new trace (no incoming `traceparent`) is sampled.
    pub fn sample_new_trace(&self) -> bool {
        self.sample_ratio >= 1.0 || (self.sample_ratio > 0.0 && rand::random::<f64>() < self.sample_ratio)
    }

    /// Queue a span for export without blocking; drops it if the queue is full.
    pub fn record(&self, span: SpanRecord) {
        if self.tx.try_send(Command::Span(Box::new(span))).is_err() {
            metrics::counter!("stouter_tracing_spans_dropped_total", "reason" => "queue_full").increment(1);
        }
    }

    /// Export everything queued so far, waiting at most `timeout`.
    pub async fn flush(&self, timeout: Duration) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(Command::Flush(ack_tx)).await.is_ok() {
            let _ = tokio::time::timeout(timeout, ack_rx).await;
        }
    }
}

fn resource_attributes(service_name: &str, node_id: &str) -> Attributes {
    let mut attrs: Attributes = vec![
        ("service.name", service_name.into()),
        ("telemetry.sdk.name", "stouter".into()),
        ("telemetry.sdk.language", "rust".into()),
        ("telemetry.sdk.version", env!("CARGO_PKG_VERSION").into()),
    ];
    if !node_id.is_empty() {
        attrs.push(("service.instance.id", node_id.into()));
    }
    attrs
}

async fn run_exporter(mut rx: mpsc::Receiver<Command>, endpoint: String, resource: Attributes) {
    let client: Client<HttpConnector, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let mut batch: Vec<SpanRecord> = Vec::with_capacity(MAX_BATCH);
    let mut interval = tokio::time::interval(EXPORT_INTERVAL);
    let mut failing = false;

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Command::Span(span)) => {
                    batch.push(*span);
                    if batch.len() >= MAX_BATCH {
                        export(&client, &endpoint, &resource, &mut batch, &mut failing).await;
                    }
                }
                Some(Command::Flush(ack)) => {
                    export(&client, &endpoint, &resource, &mut batch, &mut failing).await;
                    let _ = ack.send(());
                }
                None => {
                    export(&client, &endpoint, &resource, &mut batch, &mut failing).await;
                    break;
                }
            },
            _ = interval.tick() => {
                export(&client, &endpoint, &resource, &mut batch, &mut failing).await;
            }
        }
    }
}

/// Send `batch` (emptying it) and account for the outcome.
async fn export(
    client: &Client<HttpConnector, Full<Bytes>>,
    endpoint: &str,
    resource: &Attributes,
    batch: &mut Vec<SpanRecord>,
    failing: &mut bool,
) {
    if batch.is_empty() {
        return;
    }
    let spans = std::mem::take(batch);
    let count = spans.len() as u64;
    let body = encode_request(resource, &spans).to_string();

    let result = async {
        let uri: Uri = endpoint.parse().map_err(|e| format!("bad endpoint: {e}"))?;
        let req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| e.to_string())?;
        let resp = tokio::time::timeout(EXPORT_TIMEOUT, client.request(req))
            .await
            .map_err(|_| "timed out".to_string())?
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
        if !status.is_success() {
            return Err(format!("collector answered {status}: {}", String::from_utf8_lossy(&body)));
        }
        Ok(rejected_spans(&body))
    }
    .await;

    match result {
        Ok(rejected) => {
            if *failing {
                info!("span export to {endpoint} recovered");
                *failing = false;
            }
            let accepted = count.saturating_sub(rejected);
            metrics::counter!("stouter_tracing_spans_exported_total").increment(accepted);
            if rejected > 0 {
                metrics::counter!("stouter_tracing_spans_dropped_total", "reason" => "rejected").increment(rejected);
                warn!("collector rejected {rejected} of {count} spans");
            }
        }
        Err(e) => {
            metrics::counter!("stouter_tracing_spans_dropped_total", "reason" => "export_failed").increment(count);
            // Warn once per outage rather than every interval.
            if *failing {
                debug!("span export to {endpoint} failed: {e}");
            } else {
                warn!("span export to {endpoint} failed, dropping spans until it recovers: {e}");
                *failing = true;
            }
        }
    }
}

/// `partialSuccess.rejectedSpans` from an OTLP export response, if any.
fn rejected_spans(body: &[u8]) -> u64 {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return 0 };
    match &v["partialSuccess"]["rejectedSpans"] {
        Value::String(s) => s.parse().unwrap_or(0),
        Value::Number(n) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// OTLP JSON encoding
// ---------------------------------------------------------------------------

/// Build an `ExportTraceServiceRequest` in OTLP JSON.
pub fn encode_request(resource: &Attributes, spans: &[SpanRecord]) -> Value {
    json!({
        "resourceSpans": [{
            "resource": { "attributes": encode_attributes(resource) },
            "scopeSpans": [{
                "scope": { "name": "stouter", "version": env!("CARGO_PKG_VERSION") },
                "spans": spans.iter().map(encode_span).collect::<Vec<_>>(),
            }],
        }],
    })
}

fn encode_span(span: &SpanRecord) -> Value {
    let mut v = json!({
        "traceId": span.trace_id,
        "spanId": span.span_id,
        "name": span.name,
        "kind": span.kind as i32,
        "startTimeUnixNano": unix_nanos(span.start),
        "endTimeUnixNano": unix_nanos(span.end),
        "attributes": encode_attributes(&span.attributes),
        "status": match &span.error {
            // STATUS_CODE_ERROR
            Some(message) => json!({ "code": 2, "message": message }),
            // STATUS_CODE_UNSET
            None => json!({}),
        },
    });
    if let Some(parent) = &span.parent_span_id {
        v["parentSpanId"] = json!(parent);
    }
    if !span.events.is_empty() {
        v["events"] = span
            .events
            .iter()
            .map(|e| {
                json!({
                    "timeUnixNano": unix_nanos(e.time),
                    "name": e.name,
                    "attributes": encode_attributes(&e.attributes),
                })
            })
            .collect();
    }
    v
}

fn encode_attributes(attrs: &Attributes) -> Value {
    attrs
        .iter()
        .map(|(key, value)| {
            let value = match value {
                AttrValue::Str(s) => json!({ "stringValue": s }),
                // int64 is a decimal string in proto3 JSON.
                AttrValue::Int(n) => json!({ "intValue": n.to_string() }),
            };
            json!({ "key": key, "value": value })
        })
        .collect()
}

/// fixed64 nanoseconds since the epoch, as a decimal string.
fn unix_nanos(t: SystemTime) -> String {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn span(parent: Option<&str>) -> SpanRecord {
        let start = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_789);
        SpanRecord {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            span_id: "00f067aa0ba902b7".into(),
            parent_span_id: parent.map(str::to_string),
            name: "GET api".into(),
            kind: SpanKind::Client,
            start,
            end: start + Duration::from_millis(5),
            attributes: vec![("http.request.method", "GET".into()), ("http.response.status_code", 200.into())],
            events: vec![SpanEvent {
                time: start + Duration::from_millis(1),
                name: "retry",
                attributes: vec![("stouter.node", "n2".into())],
            }],
            error: None,
        }
    }

    #[test]
    fn encodes_otlp_json() {
        let resource = resource_attributes("stouter", "sub-1");
        let v = encode_request(&resource, &[span(Some("aaaaaaaaaaaaaaaa"))]);
        let rs = &v["resourceSpans"][0];
        assert_eq!(rs["resource"]["attributes"][0], json!({"key": "service.name", "value": {"stringValue": "stouter"}}));
        assert!(rs["resource"]["attributes"]
            .as_array()
            .unwrap()
            .contains(&json!({"key": "service.instance.id", "value": {"stringValue": "sub-1"}})));
        assert_eq!(rs["scopeSpans"][0]["scope"]["name"], "stouter");
        let s = &rs["scopeSpans"][0]["spans"][0];
        assert_eq!(s["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(s["spanId"], "00f067aa0ba902b7");
        assert_eq!(s["parentSpanId"], "aaaaaaaaaaaaaaaa");
        assert_eq!(s["kind"], 3);
        assert_eq!(s["startTimeUnixNano"], "1700000000123456789");
        assert_eq!(s["endTimeUnixNano"], "1700000000128456789");
        assert_eq!(s["attributes"][1], json!({"key": "http.response.status_code", "value": {"intValue": "200"}}));
        assert_eq!(s["status"], json!({}));
        assert_eq!(s["events"][0]["name"], "retry");
        assert_eq!(s["events"][0]["timeUnixNano"], "1700000000124456789");
    }

    #[test]
    fn root_spans_omit_parent_and_errors_set_status() {
        let mut s = span(None);
        s.events.clear();
        s.error = Some("503".into());
        let v = encode_span(&s);
        assert!(v.get("parentSpanId").is_none());
        assert!(v.get("events").is_none());
        assert_eq!(v["status"], json!({"code": 2, "message": "503"}));
    }

    #[test]
    fn parses_partial_success() {
        assert_eq!(rejected_spans(br#"{"partialSuccess":{"rejectedSpans":"3"}}"#), 3);
        assert_eq!(rejected_spans(br#"{"partialSuccess":{"rejectedSpans":2}}"#), 2);
        assert_eq!(rejected_spans(br#"{}"#), 0);
        assert_eq!(rejected_spans(b""), 0);
    }

    #[test]
    fn sampling_ratio() {
        let make = |ratio| Tracer { tx: mpsc::channel(1).0, sample_ratio: ratio };
        assert!((0..100).all(|_| make(1.0).sample_new_trace()));
        assert!((0..100).all(|_| !make(0.0).sample_new_trace()));
        let t = make(0.25);
        let n = (0..10_000).filter(|_| t.sample_new_trace()).count();
        assert!((2_000..3_000).contains(&n), "{n}");
    }

    /// Accept one HTTP request, reply 200, and return the request body.
    async fn fake_collector() -> (String, tokio::task::JoinHandle<(String, String)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, body) = loop {
                let n = s.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    if body.len() >= len {
                        break (head.to_string(), body.to_string());
                    }
                }
            };
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}")
                .await
                .unwrap();
            (head, body)
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn flush_posts_queued_spans_to_collector() {
        let (endpoint, collector) = fake_collector().await;
        let cfg = TracingConfig { otlp_endpoint: endpoint, sample_ratio: 1.0, service_name: "mesh".into() };
        let tracer = Tracer::start(&cfg, "n1");
        tracer.record(span(None));
        tracer.record(span(None));
        tracer.flush(Duration::from_secs(5)).await;

        let (head, body) = collector.await.unwrap();
        assert!(head.starts_with("POST /v1/traces HTTP/1.1"), "{head}");
        assert!(head.to_ascii_lowercase().contains("content-type: application/json"));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap().len(), 2);
        assert_eq!(v["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"], "mesh");
    }
}
