//! The shared HTTP listener (`httpListen`).
//!
//! One port for every `http` service: each request is routed by its URL
//! host (absolute-form, as clients send it when `HTTP_PROXY` points here) or
//! else its `Host` header, matched against service names and then their
//! `domains`. The request is then handled by that service's own HTTP proxy,
//! so balancing, routes, retries and tracing apply exactly as on the
//! service's local port.

use std::sync::Arc;

use anyhow::{Context, Result};
use http::{header, Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper::service::service_fn;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use super::http::{error_response, serve_http1, HttpProxies, ProxyBody};
use super::proxy::service_is_http;
use crate::config::DynamicConfig;
use crate::state::SharedState;

/// Accept connections on `addr` until shutdown.
pub(super) async fn run_http_listener(
    addr: String,
    state: Arc<SharedState>,
    proxies: Arc<HttpProxies>,
) -> Result<()> {
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind httpListen {addr}"))?;
    let bound = listener.local_addr()?;
    if !bound.ip().is_loopback() {
        warn!("httpListen {bound} is not loopback: anything that can reach it can call every http service in the mesh");
    }
    info!("HTTP listener on {bound} (route by URL host or Host header)");

    loop {
        let (client, _) = tokio::select! {
            accepted = listener.accept() => accepted.context("httpListen accept")?,
            _ = state.shutdown.cancelled() => return Ok(()),
        };
        client.set_nodelay(true).ok();
        let in_flight = state.in_flight.track();
        let shutdown = state.shutdown.clone();
        let state = state.clone();
        let proxies = proxies.clone();
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let service = service_fn(move |req| {
                let state = state.clone();
                let proxies = proxies.clone();
                async move { Ok::<_, std::convert::Infallible>(dispatch(req, &state, &proxies).await) }
            });
            if let Err(e) = serve_http1(client, service, &shutdown).await {
                debug!("httpListen connection error: {e}");
            }
        });
    }
}

async fn dispatch(req: Request<Incoming>, state: &SharedState, proxies: &HttpProxies) -> Response<ProxyBody> {
    if req.method() == Method::CONNECT {
        // HTTPS through HTTP_PROXY: the payload is encrypted end to end, so
        // there's nothing to route on. Callers should list external HTTPS
        // hosts in NO_PROXY.
        return reject(&req, StatusCode::NOT_IMPLEMENTED, "connect_not_supported");
    }
    let Some(host) = request_host(&req) else {
        return reject(&req, StatusCode::BAD_REQUEST, "missing_host");
    };
    let service = {
        let dc = state.dynamic_config.read().await;
        resolve_service(&host, &dc, state.config.subscribe_filter.as_ref())
    };
    let Some(service) = service else {
        return reject(&req, StatusCode::NOT_FOUND, "unknown_service");
    };
    if !service_is_http(&service, state) {
        return reject(&req, StatusCode::NOT_IMPLEMENTED, "service_not_http");
    }
    proxies.get(&service).handle(req).await
}

/// Answer a request the listener can't route, logging it like a proxied one.
fn reject(req: &Request<Incoming>, status: StatusCode, reason: &'static str) -> Response<ProxyBody> {
    metrics::counter!("stouter_http_listener_rejected_total", "reason" => reason).increment(1);
    info!(
        target: "stouter::access",
        service = "-",
        method = %req.method(),
        host = request_host(req).as_deref().unwrap_or("-"),
        path = %req.uri().path(),
        status = status.as_u16(),
        error = reason,
        "request"
    );
    error_response(status, reason)
}

/// The host a request is for: the URL's host in absolute-form, else the
/// `Host` header. Lowercased, without port or trailing dot.
fn request_host<B>(req: &Request<B>) -> Option<String> {
    let raw = match req.uri().host() {
        Some(h) => h.to_string(),
        None => {
            let value = req.headers().get(header::HOST)?.to_str().ok()?;
            match value.strip_prefix('[') {
                // Bracketed IPv6 literal, optionally followed by `:port`.
                Some(rest) => format!("[{}]", rest.split(']').next()?),
                None => value.split(':').next()?.to_string(),
            }
        }
    };
    let host = raw.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// The service `host` names: a service name first, else a service whose
/// `domains` include it. Services excluded by `subscribeFilter` don't match.
fn resolve_service(
    host: &str,
    dc: &DynamicConfig,
    filter: Option<&crate::config::SubscribeFilter>,
) -> Option<String> {
    let visible = || {
        dc.service_groups.iter().flat_map(|g| {
            g.services
                .iter()
                .filter(move |s| filter.is_none_or(|f| f.is_empty() || f.allows(&g.name, &s.name)))
        })
    };
    if let Some(svc) = visible().find(|s| s.name.eq_ignore_ascii_case(host)) {
        return Some(svc.name.clone());
    }
    visible()
        .find(|s| s.domains.iter().any(|d| d.trim_end_matches('.').eq_ignore_ascii_case(host)))
        .map(|s| s.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SubscribeFilter;

    fn dc(json: serde_json::Value) -> DynamicConfig {
        serde_json::from_value(json).unwrap()
    }

    fn sample() -> DynamicConfig {
        dc(serde_json::json!({
            "version": 1,
            "serviceGroups": [
                {"name": "app", "services": [
                    {"name": "inventory", "nodeId": "n2", "nodePort": 5000, "domains": ["stock.example.com"]},
                    {"name": "orders", "nodeId": "n1", "nodePort": 3000}
                ]},
                {"name": "ops", "services": [
                    {"name": "logs", "nodeId": "n3", "nodePort": 8000, "domains": ["Inventory"]}
                ]}
            ]
        }))
    }

    #[test]
    fn resolves_names_before_domains_case_insensitively() {
        let dc = sample();
        assert_eq!(resolve_service("inventory", &dc, None).as_deref(), Some("inventory"));
        assert_eq!(resolve_service("ORDERS", &dc, None).as_deref(), Some("orders"));
        // "logs" also claims the domain "Inventory"; the service name wins.
        assert_eq!(resolve_service("stock.example.com", &dc, None).as_deref(), Some("inventory"));
        assert_eq!(resolve_service("api.github.com", &dc, None), None);
    }

    #[test]
    fn subscribe_filter_hides_services() {
        let dc = sample();
        let filter = SubscribeFilter { service_groups: vec!["ops".into()], services: vec![] };
        assert_eq!(resolve_service("inventory", &dc, Some(&filter)).as_deref(), Some("logs"), "via logs' domain");
        assert_eq!(resolve_service("orders", &dc, Some(&filter)), None);
        let empty = SubscribeFilter::default();
        assert_eq!(resolve_service("orders", &dc, Some(&empty)).as_deref(), Some("orders"));
    }

    fn req(uri: &str, host: Option<&str>) -> Request<()> {
        let mut b = Request::builder().uri(uri);
        if let Some(h) = host {
            b = b.header(header::HOST, h);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn request_host_prefers_absolute_url_then_host_header() {
        assert_eq!(request_host(&req("http://Inventory:80/items", Some("other"))).as_deref(), Some("inventory"));
        assert_eq!(request_host(&req("/items", Some("inventory:15001"))).as_deref(), Some("inventory"));
        assert_eq!(request_host(&req("/items", Some("stock.example.com."))).as_deref(), Some("stock.example.com"));
        assert_eq!(request_host(&req("/items", Some("[::1]:15001"))).as_deref(), Some("[::1]"));
        assert_eq!(request_host(&req("/items", None)), None);
    }
}
