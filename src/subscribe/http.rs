//! HTTP/1.1-aware proxying for services with `protocol: http`.
//!
//! Each request is balanced, timed and logged on its own, and travels over a
//! pooled keep-alive connection to the chosen node. Pooled connections run
//! over ordinary tunnels, so hosting nodes need no HTTP awareness. See
//! `docs/http-proxy.md` for the design and the semantics below.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;
use tracing::{debug, info};

use super::balancer::{Balancer, Pick};
use super::proxy::{
    establish_tunnel, note_tunnel_failure, prefer_healthy, service_backends, Tunnel, MAX_TUNNEL_ATTEMPTS,
};
use crate::config::{labels_match, Route};
use crate::state::SharedState;

type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// How long an idle pooled upstream connection is kept.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Response header naming why the proxy itself answered.
const ERROR_HEADER: &str = "x-stouter-error";

/// Per-service HTTP proxy state, shared by all of the service's connections.
pub(super) struct HttpProxy {
    service_name: String,
    state: Arc<SharedState>,
    balancer: Arc<Balancer>,
    client: Client<TunnelConnector, ProxyBody>,
    retry_budget: RetryBudget,
}

impl HttpProxy {
    pub fn new(service_name: String, state: Arc<SharedState>, balancer: Arc<Balancer>) -> Arc<Self> {
        let connector = TunnelConnector {
            service_name: service_name.clone(),
            state: state.clone(),
        };
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            // The URI authority is a synthetic per-node pool key; the
            // client's own Host header is forwarded instead.
            .set_host(false)
            .build(connector);
        Arc::new(Self { service_name, state, balancer, client, retry_budget: RetryBudget::new() })
    }

    /// Serve HTTP/1.1 on one accepted client connection.
    pub async fn serve_connection(self: Arc<Self>, client: TcpStream) {
        let service = service_fn(move |req| {
            let proxy = self.clone();
            async move { Ok::<_, std::convert::Infallible>(proxy.handle(req).await) }
        });
        if let Err(e) = http1::Builder::new()
            .serve_connection(TokioIo::new(client), service)
            .with_upgrades()
            .await
        {
            debug!("HTTP connection error: {e}");
        }
    }

    async fn handle(&self, mut req: Request<Incoming>) -> Response<ProxyBody> {
        let start = Instant::now();
        let method = req.method().clone();
        let path = req.uri().path().to_string();

        let trace = TraceContext::from_headers(req.headers()).unwrap_or_else(TraceContext::new_root);
        let upstream_trace = trace.child();

        let upgrade = is_upgrade(req.headers());
        let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));

        let (mut parts, body) = req.into_parts();
        strip_hop_by_hop(&mut parts.headers, upgrade);
        if !parts.headers.contains_key(header::HOST) {
            if let Ok(host) = HeaderValue::from_str(&self.service_name) {
                parts.headers.insert(header::HOST, host);
            }
        }
        parts.headers.insert(
            HeaderName::from_static("traceparent"),
            HeaderValue::from_str(&upstream_trace.to_header()).expect("traceparent is ASCII"),
        );
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());

        let routes = self.state.routes.load();
        let routed = select_route(
            routes.get(&self.service_name).map(Vec::as_slice).unwrap_or_default(),
            &method,
            &path,
            &parts.headers,
        );
        let route_label = routed.as_ref().map_or_else(|| "default".to_string(), |r| r.label.clone());

        self.retry_budget.deposit();
        let mut tried: Vec<String> = Vec::new();
        let mut last_err: Option<String> = None;

        let outcome = match RequestBody::prepare(body).await {
            Err(e) => {
                last_err = Some(e.to_string());
                Outcome::Error(StatusCode::BAD_REQUEST, "bad_request_body")
            }
            Ok(body) => {
                let subset = routed.as_ref().map(|r| &r.labels);
                self.attempt(&parts, &path_and_query, body, subset, &mut tried, &mut last_err)
                    .await
            }
        };

        let (response, node) = match outcome {
            Outcome::Response { mut resp, slot, node_id } => {
                if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
                    if let Some(client_upgrade) = client_upgrade {
                        let backend_upgrade = hyper::upgrade::on(&mut resp);
                        tokio::spawn(async move {
                            let _slot = slot;
                            match tokio::try_join!(client_upgrade, backend_upgrade) {
                                Ok((client_io, backend_io)) => {
                                    let _ = crate::io::proxy_bidirectional(
                                        TokioIo::new(client_io),
                                        TokioIo::new(backend_io),
                                    )
                                    .await;
                                }
                                Err(e) => debug!("upgrade failed: {e}"),
                            }
                        });
                    }
                    (resp.map(|b| b.boxed()), Some(node_id))
                } else {
                    strip_hop_by_hop(resp.headers_mut(), false);
                    // Hold the connection slot until the body is fully sent.
                    let resp = resp.map(move |b| {
                        b.map_frame(move |frame| {
                            let _ = &slot;
                            frame
                        })
                        .boxed()
                    });
                    (resp, Some(node_id))
                }
            }
            Outcome::Error(status, reason) => {
                if let Some(e) = &last_err {
                    debug!("{} {} for {}: {reason}: {e}", status.as_u16(), path, self.service_name);
                }
                (error_response(status, reason), tried.last().cloned())
            }
        };

        let status = response.status().as_u16();
        let elapsed = start.elapsed();
        metrics::counter!(
            "stouter_http_requests_total",
            "service" => self.service_name.clone(),
            "method" => method_label(&method),
            "code" => format!("{}xx", status / 100),
            "route" => route_label.clone()
        )
        .increment(1);
        metrics::histogram!("stouter_http_request_duration_seconds", "service" => self.service_name.clone())
            .record(elapsed.as_secs_f64());
        info!(
            target: "stouter::access",
            service = %self.service_name,
            method = %method,
            path = %path,
            route = %route_label,
            status,
            node = node.as_deref().unwrap_or("-"),
            attempts = tried.len(),
            duration_ms = elapsed.as_millis() as u64,
            trace_id = %trace.trace_id,
            "request"
        );

        response
    }
}

impl HttpProxy {
    /// Send the request to up to [`MAX_TUNNEL_ATTEMPTS`] distinct nodes.
    ///
    /// Moves to another node when a connection can't be established (the
    /// request was never sent), or, for idempotent methods and within the
    /// retry budget, when a node answers 502/503/504. Only replayable bodies
    /// can be re-sent.
    async fn attempt(
        &self,
        parts: &http::request::Parts,
        path_and_query: &str,
        mut body: RequestBody,
        subset: Option<&BTreeMap<String, String>>,
        tried: &mut Vec<String>,
        last_err: &mut Option<String>,
    ) -> Outcome {
        let all = service_backends(&self.service_name, &self.state);
        self.balancer.retain_nodes(&all);
        // A matched route narrows the candidates to its label subset; health
        // filtering then fails open within that subset, not across it.
        let candidates: Vec<_> = match subset {
            Some(labels) => all.into_iter().filter(|b| labels_match(&b.labels, labels)).collect(),
            None => all,
        };
        if candidates.is_empty() && subset.is_some() {
            return Outcome::Error(StatusCode::SERVICE_UNAVAILABLE, "no_route_backend");
        }
        let backends = prefer_healthy(candidates, &self.service_name, &self.state);
        let max_attempts = backends.len().min(MAX_TUNNEL_ATTEMPTS);
        // A retryable status from an earlier node, returned if no later
        // attempt does better.
        let mut fallback: Option<Outcome> = None;

        let outcome = loop {
            if tried.len() >= max_attempts {
                break Outcome::Error(StatusCode::BAD_GATEWAY, "no_backend_reachable");
            }
            let (idx, slot) = match self.balancer.pick(&backends, tried, Instant::now()) {
                Pick::Chosen { index, slot } => (index, slot),
                Pick::AllTried => break Outcome::Error(StatusCode::BAD_GATEWAY, "no_backend_reachable"),
                Pick::AtCapacity if tried.is_empty() => {
                    break Outcome::Error(StatusCode::SERVICE_UNAVAILABLE, "circuit_open")
                }
                Pick::AtCapacity => break Outcome::Error(StatusCode::BAD_GATEWAY, "no_backend_reachable"),
            };
            let backend = &backends[idx];
            let node_id = backend.node.id.clone();
            tried.push(node_id.clone());

            let Some(uri) = node_uri(&node_id, path_and_query) else {
                break Outcome::Error(StatusCode::BAD_GATEWAY, "no_backend_reachable");
            };
            let Some(req_body) = body.for_attempt() else {
                break Outcome::Error(StatusCode::BAD_GATEWAY, "no_backend_reachable");
            };
            let mut upstream = Request::new(req_body);
            *upstream.method_mut() = parts.method.clone();
            *upstream.uri_mut() = uri;
            *upstream.version_mut() = Version::HTTP_11;
            *upstream.headers_mut() = parts.headers.clone();

            let sent = self.client.request(upstream);
            let result = match backend.request_timeout {
                Some(limit) => match tokio::time::timeout(limit, sent).await {
                    Ok(r) => r,
                    Err(_) => break Outcome::Error(StatusCode::GATEWAY_TIMEOUT, "timeout"),
                },
                None => sent.await,
            };

            match result {
                Ok(resp) => {
                    self.balancer.record_success(&node_id);
                    let status = resp.status();
                    let outcome = Outcome::Response { resp, slot, node_id };
                    if !is_retryable_status(status)
                        || !is_idempotent(&parts.method)
                        || !body.is_replayable()
                        || tried.len() >= max_attempts
                    {
                        break outcome;
                    }
                    if !self.retry_budget.try_withdraw() {
                        metrics::counter!("stouter_http_retry_budget_exhausted_total", "service" => self.service_name.clone()).increment(1);
                        break outcome;
                    }
                    metrics::counter!("stouter_http_retries_total", "service" => self.service_name.clone(), "reason" => "status").increment(1);
                    debug!("{} from node {} for {}; retrying on another node", status.as_u16(), tried.last().unwrap(), self.service_name);
                    fallback = Some(outcome);
                }
                Err(e) if e.is_connect() => {
                    debug!("connect to node {node_id} for {} failed: {e:?}", self.service_name);
                    note_tunnel_failure(&self.balancer, &self.service_name, &node_id, &e);
                    *last_err = Some(e.to_string());
                    if !body.is_replayable() {
                        break Outcome::Error(StatusCode::BAD_GATEWAY, "no_backend_reachable");
                    }
                    // The request never left, so this retry doesn't spend budget.
                    metrics::counter!("stouter_http_retries_total", "service" => self.service_name.clone(), "reason" => "connect").increment(1);
                }
                Err(e) => {
                    // The request may have reached the backend: don't retry.
                    debug!("request to node {node_id} for {} failed: {e:?}", self.service_name);
                    *last_err = Some(e.to_string());
                    break Outcome::Error(StatusCode::BAD_GATEWAY, "upstream_error");
                }
            }
        };

        // A real backend answer beats a proxy-generated error.
        match (outcome, fallback) {
            (Outcome::Error(..), Some(fallback)) => fallback,
            (outcome, _) => outcome,
        }
    }
}

/// The outcome of route selection for one request.
#[derive(Debug, PartialEq)]
struct RoutedTo {
    /// Route name (or position) for logs and metrics.
    label: String,
    /// Labels an instance must carry to serve the request.
    labels: BTreeMap<String, String>,
}

/// Find the first route matching the request and pick one of its backend
/// subsets by weight. `None` means no route matched: use every instance.
fn select_route(routes: &[Route], method: &Method, path: &str, headers: &HeaderMap) -> Option<RoutedTo> {
    let (index, route) = routes.iter().enumerate().find(|(_, r)| {
        r.matches
            .matches(method.as_str(), path, |name| headers.get(name).and_then(|v| v.to_str().ok()))
    })?;
    let total: u64 = route.backends.iter().map(|b| u64::from(b.weight)).sum();
    // Validation guarantees a non-zero total; fall back to the first subset.
    let mut ticket = if total == 0 { 0 } else { rand::random_range(0..total) };
    let backend = route
        .backends
        .iter()
        .find(|b| {
            let w = u64::from(b.weight);
            if ticket < w {
                true
            } else {
                ticket -= w;
                false
            }
        })
        .unwrap_or(&route.backends[0]);
    Some(RoutedTo { label: route.label(index), labels: backend.labels.clone() })
}

/// Request bodies up to this size (with a declared length) are buffered so
/// the request can be re-sent to another node.
const MAX_BUFFERED_BODY: u64 = 64 * 1024;

/// A request body, buffered if it is small enough to be re-sent.
enum RequestBody {
    /// Fully buffered (possibly empty); can be sent any number of times.
    Replayable(Bytes),
    /// Streamed from the client; can be sent once.
    Stream(Option<Incoming>),
}

impl RequestBody {
    async fn prepare(body: Incoming) -> Result<Self, hyper::Error> {
        if body.is_end_stream() {
            return Ok(Self::Replayable(Bytes::new()));
        }
        if body.size_hint().upper().is_some_and(|n| n <= MAX_BUFFERED_BODY) {
            return Ok(Self::Replayable(body.collect().await?.to_bytes()));
        }
        Ok(Self::Stream(Some(body)))
    }

    fn is_replayable(&self) -> bool {
        matches!(self, Self::Replayable(_))
    }

    /// The body to send on the next attempt, or `None` if a streamed body
    /// has already been used.
    fn for_attempt(&mut self) -> Option<ProxyBody> {
        match self {
            Self::Replayable(bytes) => {
                Some(Full::new(bytes.clone()).map_err(|never| match never {}).boxed())
            }
            Self::Stream(body) => body.take().map(|b| b.boxed()),
        }
    }
}

/// Statuses that mean "this node couldn't serve it, another might".
fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Methods that are safe to send twice (RFC 9110 §9.2.2).
fn is_idempotent(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::PUT | Method::DELETE | Method::TRACE
    )
}

/// Fraction of a retry each request earns.
const RETRY_BUDGET_RATIO: f64 = 0.2;

/// Most retries that can be banked; the budget starts full so a quiet
/// service can still retry its first failures.
const RETRY_BUDGET_MAX: f64 = 20.0;

/// Caps status-driven retries at about [`RETRY_BUDGET_RATIO`] of request
/// volume (plus a burst of [`RETRY_BUDGET_MAX`]), so retries can't multiply
/// load on a backend that is already failing.
struct RetryBudget {
    tokens: std::sync::Mutex<f64>,
}

impl RetryBudget {
    fn new() -> Self {
        Self { tokens: std::sync::Mutex::new(RETRY_BUDGET_MAX) }
    }

    /// Credit one request.
    fn deposit(&self) {
        let mut t = self.tokens.lock().unwrap();
        *t = (*t + RETRY_BUDGET_RATIO).min(RETRY_BUDGET_MAX);
    }

    /// Spend one retry, if the budget allows.
    fn try_withdraw(&self) -> bool {
        let mut t = self.tokens.lock().unwrap();
        if *t >= 1.0 {
            *t -= 1.0;
            true
        } else {
            false
        }
    }
}

enum Outcome {
    Response {
        resp: Response<Incoming>,
        slot: super::balancer::Slot,
        node_id: String,
    },
    Error(StatusCode, &'static str),
}

fn error_response(status: StatusCode, reason: &'static str) -> Response<ProxyBody> {
    let mut resp = Response::new(
        Full::new(Bytes::from(format!("{reason}\n")))
            .map_err(|never| match never {})
            .boxed(),
    );
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(ERROR_HEADER, HeaderValue::from_static(reason));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    resp
}

/// Bounded label for a request method, so arbitrary methods can't blow up
/// metric cardinality.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// Headers that describe a single hop, never forwarded (RFC 9110 §7.6.1).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// True if the request asks to switch protocols (e.g. WebSocket).
fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE) && connection_tokens(headers).any(|t| t == "upgrade")
}

fn connection_tokens(headers: &HeaderMap) -> impl Iterator<Item = String> + '_ {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
}

/// Remove hop-by-hop headers, including any named in `Connection`. For an
/// upgrade, `Upgrade` is kept and `Connection: upgrade` re-added.
fn strip_hop_by_hop(headers: &mut HeaderMap, keep_upgrade: bool) {
    let named: Vec<String> = connection_tokens(headers).collect();
    let upgrade = headers.get(header::UPGRADE).cloned();
    for name in HOP_BY_HOP.iter().copied().map(str::to_string).chain(named) {
        headers.remove(name.as_str());
    }
    if keep_upgrade {
        if let Some(value) = upgrade {
            headers.insert(header::UPGRADE, value);
            headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        }
    }
}

// ---------------------------------------------------------------------------
// W3C trace context
// ---------------------------------------------------------------------------

/// The parts of a W3C `traceparent` header the proxy carries forward.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TraceContext {
    trace_id: String,
    span_id: String,
    flags: String,
}

impl TraceContext {
    /// Parse a version-00-compatible `traceparent`, or `None` if absent or
    /// invalid (in which case a new trace is started, per the spec).
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let value = headers.get("traceparent")?.to_str().ok()?.trim();
        let mut parts = value.split('-');
        let (version, trace_id, span_id, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        // Future versions may append fields; version 00 has exactly four.
        if version == "00" && parts.next().is_some() {
            return None;
        }
        let is_hex = |s: &str, len: usize| {
            s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        let valid = is_hex(version, 2)
            && version != "ff"
            && is_hex(trace_id, 32)
            && trace_id.bytes().any(|b| b != b'0')
            && is_hex(span_id, 16)
            && span_id.bytes().any(|b| b != b'0')
            && is_hex(flags, 2);
        valid.then(|| Self {
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            flags: flags.to_string(),
        })
    }

    /// Start a new, sampled trace.
    fn new_root() -> Self {
        Self {
            trace_id: random_hex(16),
            span_id: random_hex(8),
            flags: "01".to_string(),
        }
    }

    /// Same trace, new span id: the proxy's hop to the upstream.
    fn child(&self) -> Self {
        Self { span_id: random_hex(8), ..self.clone() }
    }

    fn to_header(&self) -> String {
        format!("00-{}-{}-{}", self.trace_id, self.span_id, self.flags)
    }
}

/// `bytes` random bytes as lowercase hex, never all zeros.
fn random_hex(bytes: usize) -> String {
    loop {
        let raw: Vec<u8> = (0..bytes).map(|_| rand::random::<u8>()).collect();
        if raw.iter().any(|&b| b != 0) {
            return raw.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream connections over tunnels
// ---------------------------------------------------------------------------

/// Suffix of the synthetic per-node authority used as the pool key.
const NODE_HOST_SUFFIX: &str = "stouter";

/// Upstream URI addressing `node_id`. The authority only keys the connection
/// pool (one pool per node); it is never resolved. The node id is hex-encoded
/// (split into DNS-sized labels) so any id makes a valid authority.
fn node_uri(node_id: &str, path_and_query: &str) -> Option<Uri> {
    let hex: String = node_id.bytes().map(|b| format!("{b:02x}")).collect();
    let labels: Vec<&str> = hex
        .as_bytes()
        .chunks(62)
        .map(|c| std::str::from_utf8(c).expect("hex is ASCII"))
        .collect();
    format!("http://{}.{NODE_HOST_SUFFIX}{path_and_query}", labels.join("."))
        .parse()
        .ok()
}

/// Inverse of the authority encoding in [`node_uri`].
fn node_id_from_uri(uri: &Uri) -> Option<String> {
    let host = uri.host()?.strip_suffix(NODE_HOST_SUFFIX)?.strip_suffix('.')?;
    let hex: String = host.split('.').collect();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

/// Connects the HTTP client to a node by opening a tunnel to the service.
#[derive(Clone)]
struct TunnelConnector {
    service_name: String,
    state: Arc<SharedState>,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl tower_service::Service<Uri> for TunnelConnector {
    type Response = TunnelIo;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<TunnelIo, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let service_name = self.service_name.clone();
        let state = self.state.clone();
        Box::pin(async move {
            let node_id = node_id_from_uri(&uri).ok_or("invalid upstream node address")?;
            let node = state
                .known_nodes
                .load()
                .get(&node_id)
                .cloned()
                .ok_or_else(|| format!("unknown node {node_id}"))?;
            let tunnel = establish_tunnel(&node, service_name, &state).await?;
            Ok(TunnelIo(TokioIo::new(tunnel)))
        })
    }
}

/// A tunnel adapted to hyper's I/O traits.
struct TunnelIo(TokioIo<Tunnel>);

impl hyper::rt::Read for TunnelIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for TunnelIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

impl Connection for TunnelIo {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn node_uri_round_trips_any_node_id() {
        for id in ["n1", "host-2.example.com", "a_b", &"x".repeat(100), "ünïcode"] {
            let uri = node_uri(id, "/a/b?c=d").expect("valid uri");
            assert_eq!(uri.path_and_query().unwrap().as_str(), "/a/b?c=d");
            assert_eq!(node_id_from_uri(&uri).as_deref(), Some(id));
        }
    }

    #[test]
    fn strips_hop_by_hop_and_connection_named_headers() {
        let mut h = headers(&[
            ("connection", "keep-alive, x-secret"),
            ("keep-alive", "timeout=5"),
            ("x-secret", "1"),
            ("transfer-encoding", "chunked"),
            ("host", "api"),
            ("x-keep", "1"),
        ]);
        strip_hop_by_hop(&mut h, false);
        let mut names: Vec<&str> = h.keys().map(|k| k.as_str()).collect();
        names.sort();
        assert_eq!(names, ["host", "x-keep"]);
    }

    #[test]
    fn upgrade_requests_keep_upgrade_headers() {
        let mut h = headers(&[("connection", "Upgrade"), ("upgrade", "websocket"), ("te", "x")]);
        assert!(is_upgrade(&h));
        strip_hop_by_hop(&mut h, true);
        assert_eq!(h["upgrade"], "websocket");
        assert_eq!(h["connection"], "upgrade");
        assert!(!h.contains_key("te"));
    }

    #[test]
    fn not_an_upgrade_without_connection_token() {
        assert!(!is_upgrade(&headers(&[("upgrade", "websocket")])));
    }

    #[test]
    fn traceparent_parse_and_propagate() {
        let h = headers(&[("traceparent", "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")]);
        let tc = TraceContext::from_headers(&h).unwrap();
        assert_eq!(tc.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        let child = tc.child();
        assert_eq!(child.trace_id, tc.trace_id);
        assert_eq!(child.flags, "01");
        assert_ne!(child.span_id, tc.span_id);
        let header = child.to_header();
        assert!(header.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));
        assert!(header.ends_with("-01"));
        assert_eq!(header.len(), 55);
    }

    #[test]
    fn invalid_traceparent_rejected() {
        for bad in [
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-4bf92f-00f067aa0ba902b7-01",
            "garbage",
        ] {
            let h = headers(&[("traceparent", bad)]);
            assert!(TraceContext::from_headers(&h).is_none(), "{bad}");
        }
        // A future version may carry extra fields.
        let h = headers(&[("traceparent", "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra")]);
        assert!(TraceContext::from_headers(&h).is_some());
    }

    #[test]
    fn retry_budget_caps_retries_at_ratio_after_burst() {
        let budget = RetryBudget::new();
        // The initial burst allowance.
        let burst = (0..100).take_while(|_| budget.try_withdraw()).count();
        assert_eq!(burst, RETRY_BUDGET_MAX as usize);
        // Then about one retry per 1/RATIO requests.
        let mut retries = 0;
        for _ in 0..1000 {
            budget.deposit();
            if budget.try_withdraw() {
                retries += 1;
            }
        }
        assert!((199..=200).contains(&retries), "{retries}");
    }

    #[test]
    fn retry_budget_banks_at_most_max() {
        let budget = RetryBudget::new();
        for _ in 0..10_000 {
            budget.deposit();
        }
        let banked = (0..1000).take_while(|_| budget.try_withdraw()).count();
        assert_eq!(banked, RETRY_BUDGET_MAX as usize);
    }

    #[test]
    fn only_idempotent_methods_and_gateway_statuses_retry() {
        assert!(is_idempotent(&Method::GET));
        assert!(is_idempotent(&Method::PUT));
        assert!(!is_idempotent(&Method::POST));
        assert!(!is_idempotent(&Method::PATCH));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
    }

    fn routes(json: serde_json::Value) -> Vec<Route> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn first_matching_route_wins_and_unmatched_uses_default() {
        let rs = routes(serde_json::json!([
            {"name": "canary", "service": "api", "match": {"headers": {"x-canary": "true"}},
             "backends": [{"labels": {"version": "v2"}}]},
            {"name": "v2-path", "service": "api", "match": {"pathPrefix": "/v2"},
             "backends": [{"labels": {"version": "v2"}}]},
            {"name": "v1-path", "service": "api", "match": {"pathPrefix": "/v2/legacy"},
             "backends": [{"labels": {"version": "v1"}}]},
        ]));
        let get = Method::GET;
        let canary = headers(&[("x-canary", "true")]);
        assert_eq!(select_route(&rs, &get, "/x", &canary).unwrap().label, "canary");
        let none = HeaderMap::new();
        assert_eq!(select_route(&rs, &get, "/v2/legacy/a", &none).unwrap().label, "v2-path");
        assert!(select_route(&rs, &get, "/other", &none).is_none());
    }

    #[test]
    fn route_backends_split_by_weight() {
        let rs = routes(serde_json::json!([
            {"service": "api", "backends": [
                {"labels": {"version": "v1"}, "weight": 9},
                {"labels": {"version": "v2"}, "weight": 1},
                {"labels": {"version": "v3"}, "weight": 0}
            ]}
        ]));
        let mut v2 = 0;
        for _ in 0..10_000 {
            let picked = select_route(&rs, &Method::GET, "/", &HeaderMap::new()).unwrap();
            assert_eq!(picked.label, "#0");
            match picked.labels["version"].as_str() {
                "v2" => v2 += 1,
                "v1" => {}
                other => panic!("weight-0 subset {other} picked"),
            }
        }
        assert!((800..1200).contains(&v2), "{v2}");
    }

    #[test]
    fn new_root_is_valid() {
        let tc = TraceContext::new_root();
        let h = headers(&[("traceparent", &tc.to_header())]);
        assert_eq!(TraceContext::from_headers(&h), Some(tc));
    }
}
