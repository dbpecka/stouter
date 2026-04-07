use std::sync::Arc;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::state::SharedState;

/// Install the Prometheus metrics recorder globally.
///
/// Returns a handle that can render the `/metrics` scrape endpoint.
pub fn install() -> PrometheusHandle {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    builder
        .install_recorder()
        .expect("failed to install Prometheus recorder")
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
            let connect_addr = node.relay.as_deref().unwrap_or(&node.addr);
            if !connect_addr.is_empty() {
                metrics::gauge!("stouter_tunnel_pool_size", "addr" => connect_addr.to_string())
                    .set(state.tunnel_pool.count(connect_addr) as f64);
            }
            metrics::gauge!("stouter_reverse_pool_size", "node_id" => node.id.clone())
                .set(state.reverse_pool.count(&node.id) as f64);
        }

        // Mux sessions
        let mux_count = state.mux_pool.session_count();
        metrics::gauge!("stouter_mux_sessions").set(mux_count as f64);
    }
}
