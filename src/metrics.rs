use std::sync::Arc;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::state::SharedState;

/// How long a gauge keeps being reported after it was last set. Every gauge
/// is set by [`run_gauge_loop`] every 5 seconds while what it describes
/// exists, so one not set for this long describes something gone (most
/// often a node that left or was forgotten, whose per-node gauges would
/// otherwise report their last value forever).
const GAUGE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Install the Prometheus metrics recorder globally.
///
/// Returns a handle that can render the `/metrics` scrape endpoint.
pub fn install() -> PrometheusHandle {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new()
        .idle_timeout(metrics_util::MetricKindMask::GAUGE, Some(GAUGE_IDLE_TIMEOUT));
    builder
        .install_recorder()
        .expect("failed to install Prometheus recorder")
}

/// Serve `GET /metrics` on `127.0.0.1:port` until shutdown: for `node`-mode
/// daemons, which run no REST API (subscribers serve `/metrics` there), so
/// what they record (tunnels relayed and served, gossip) can be scraped.
/// A port already in use (say by a subscriber on the same host, on the
/// default port) is a warning, not a reason to stop the node.
pub async fn serve_metrics(port: u16, handle: PrometheusHandle, state: Arc<SharedState>) {
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let handle = handle.clone();
            async move { handle.render() }
        }),
    );
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::warn!("not serving /metrics: can't bind 127.0.0.1:{port} ({e}); set apiPort to a free port");
            return;
        }
    };
    tracing::info!("metrics on http://127.0.0.1:{port}/metrics");
    let shutdown = state.shutdown.clone();
    if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(async move { shutdown.cancelled().await }).await {
        tracing::warn!("metrics endpoint stopped: {e}");
    }
}

/// Background task that periodically publishes gauge values from shared state.
///
/// Gauges like pool sizes and node counts can't be emitted at callsites
/// because they represent point-in-time snapshots. This task samples them
/// every 5 seconds.
pub async fn run_gauge_loop(state: Arc<SharedState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }

        // In-flight connections
        metrics::gauge!("stouter_in_flight_connections")
            .set(state.in_flight.current() as f64);

        // Known nodes
        let nodes = state.known_nodes.load();
        metrics::gauge!("stouter_known_nodes").set(nodes.len() as f64);

        // Config version
        let version = state.dynamic_config.read().await.version;
        metrics::gauge!("stouter_config_version").set(version as f64);

        // Pool sizes per node
        for node in nodes.values() {
            if let Some((connect_addr, _)) = crate::subscribe::tunnel_route(node, &state) {
                metrics::gauge!("stouter_tunnel_pool_size", "addr" => connect_addr.clone())
                    .set(state.tunnel_pool.count(&connect_addr) as f64);
            }
            metrics::gauge!("stouter_reverse_pool_size", "node_id" => node.id.clone())
                .set(state.reverse_pool.count(&node.id) as f64);
        }

        // Service health as this daemon sees it: from the local checker for
        // our own services, from gossip for everyone else's.
        // For instances on nodes that are here (or this one): a node that
        // left still has its services in the config, and its gauges must
        // go quiet so they expire (see `GAUGE_IDLE_TIMEOUT`).
        for (node_id, service) in state.service_index.load().keys() {
            if *node_id != state.config.node_id && !nodes.contains_key(node_id) {
                continue;
            }
            let healthy = !state.service_unhealthy(node_id, service);
            metrics::gauge!("stouter_service_healthy", "service" => service.clone(), "node_id" => node_id.clone())
                .set(if healthy { 1.0 } else { 0.0 });
        }

        // Mux sessions
        let mux_count = state.mux_pool.session_count();
        metrics::gauge!("stouter_mux_sessions").set(mux_count as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn get(port: u16, path: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn a_node_serves_its_metrics_and_a_taken_port_doesnt_stop_it() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("stouter_connections_total", "type" => "tunnel").increment(3);
        });
        let state = SharedState::new(crate::config::Config::default(), String::new(), tokio_util::sync::CancellationToken::new(), None);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let server = tokio::spawn(serve_metrics(port, handle.clone(), state.clone()));
        let mut body = String::new();
        for _ in 0..50 {
            if let Ok(s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                drop(s);
                body = get(port, "/metrics").await;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(body.starts_with("HTTP/1.1 200"), "{body}");
        assert!(body.contains("stouter_connections_total{type=\"tunnel\"} 3"), "{body}");

        // The port is taken now: a second server warns and returns.
        tokio::time::timeout(std::time::Duration::from_secs(2), serve_metrics(port, handle, state.clone())).await.expect("returns");
        state.shutdown.cancel();
        server.await.unwrap();
    }
}
