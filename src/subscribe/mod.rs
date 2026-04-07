mod api;
mod proxy;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::info;

use crate::gossip;
use crate::state::SharedState;

/// Port and metadata for a locally proxied service.
pub(crate) struct ServiceInfo {
    pub port: u16,
    pub domains: Vec<String>,
}

/// A running local proxy for a single service.
struct ProxyHandle {
    /// The bare service name, used as the key in `service_ports`.
    service_name: String,
    #[allow(dead_code)]
    local_port: u16,
    abort: tokio::task::AbortHandle,
}

/// Run the subscribe daemon.
///
/// Starts the gossip sync loop, REST API, local proxy manager, and a TCP
/// listener that accepts inbound connections.
pub async fn run_subscribe(state: Arc<SharedState>) -> Result<()> {
    tokio::spawn(gossip::run_sync_loop(state.clone()));
    tokio::spawn(gossip::run_member_log(state.clone()));
    tokio::spawn(gossip::run_config_poll_loop(state.clone()));

    let service_ports: Arc<RwLock<HashMap<String, ServiceInfo>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let api_ports = service_ports.clone();
    let api_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = api::run_api(api_ports, api_state, 5381).await {
            tracing::error!("REST API server exited: {e}");
        }
    });

    let listener = TcpListener::bind(&state.config.bind)
        .await
        .with_context(|| format!("bind gossip listener to {}", state.config.bind))?;

    info!("Subscribe daemon listening on {}", state.config.bind);

    tokio::spawn(manage_proxies(state.clone(), service_ports));
    tokio::spawn(fill_tunnel_pool(state.clone()));
    tokio::spawn(maintain_mux_sessions(state.clone()));

    loop {
        let (stream, _) = tokio::select! {
            result = listener.accept() => result.context("accept connection")?,
            _ = state.shutdown.cancelled() => {
                info!("subscribe listener shutting down");
                break;
            }
        };
        stream.set_nodelay(true).ok();
        let state = state.clone();
        let guard = state.in_flight.track();
        tokio::spawn(async move {
            gossip::dispatch_connection(stream, state).await;
            drop(guard);
        });
    }

    Ok(())
}

/// Background task that reconciles running proxies with the current set of
/// known services.
///
/// Every 2 seconds it computes the desired set of services from
/// `state.dynamic_config` and `state.known_nodes`, starts proxies for new
/// services, and aborts proxies for services that have disappeared. Each
/// proxy handles all nodes for its service via round-robin routing.
async fn manage_proxies(
    state: Arc<SharedState>,
    service_ports: Arc<RwLock<HashMap<String, ServiceInfo>>>,
) {
    let mut proxies: HashMap<String, ProxyHandle> = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_secs(2));

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }

        // Snapshot current desired state.
        let dc = state.dynamic_config.read().await.clone();
        let nodes = state.known_nodes.load_full();

        // Build the desired set: "group/service" keys where at least one
        // node is reachable. Track (service_name, domains) for proxy setup.
        let mut desired: HashMap<String, (String, Vec<String>)> = HashMap::new();
        let mut seen_svc_keys: HashSet<String> = HashSet::new();
        for group in &dc.service_groups {
            for svc in &group.services {
                let key = format!("{}/{}", group.name, svc.name);
                if nodes.contains_key(&svc.node_id) && seen_svc_keys.insert(key.clone()) {
                    desired.insert(key, (svc.name.clone(), svc.domains.clone()));
                }
            }
        }

        // Abort proxies for services that are no longer desired.
        let removed: Vec<String> = proxies
            .keys()
            .filter(|name| !desired.contains_key(*name))
            .cloned()
            .collect();

        for key in removed {
            if let Some(handle) = proxies.remove(&key) {
                handle.abort.abort();
                service_ports.write().await.remove(&handle.service_name);
                info!("Stopped proxy for {key}");
            }
        }

        // Start proxies for newly desired services.
        for (key, (svc_name, svc_domains)) in desired {
            if proxies.contains_key(&key) {
                continue;
            }

            // The API can only serve one port per bare service name. If another
            // proxy already owns this name, skip to avoid inconsistency.
            {
                let ports = service_ports.read().await;
                if ports.contains_key(&svc_name) {
                    tracing::warn!(
                        "service name '{}' already proxied, skipping {key}",
                        svc_name
                    );
                    continue;
                }
            }

            let (port_tx, port_rx) = tokio::sync::oneshot::channel();
            let join_handle = tokio::spawn(proxy::run_proxy(
                svc_name.clone(),
                state.clone(),
                port_tx,
            ));
            let abort = join_handle.abort_handle();

            match port_rx.await {
                Ok(local_port) => {
                    service_ports.write().await.insert(svc_name.clone(), ServiceInfo { port: local_port, domains: svc_domains });
                    proxies.insert(key.clone(), ProxyHandle { service_name: svc_name, local_port, abort });
                    info!("Started proxy for {key} on 127.0.0.1:{local_port}");
                }
                Err(_) => {
                    abort.abort();
                    tracing::warn!("Proxy for {key} failed to start");
                }
            }
        }
    }
}

/// Background task that maintains warm TCP connections to known nodes.
///
/// These pre-established connections eliminate TCP handshake latency on
/// the proxy hot path.
async fn fill_tunnel_pool(state: Arc<SharedState>) {
    /// Target number of warm connections per node address.
    const POOL_TARGET: usize = 8;

    while !state.shutdown.is_cancelled() {
        let addrs: Vec<String> = {
            let nodes = state.known_nodes.load();
            nodes
                .values()
                .filter_map(|node| {
                    let addr = node.relay.as_deref().unwrap_or(&node.addr);
                    if addr.is_empty() || state.tunnel_pool.count(addr) >= POOL_TARGET {
                        None
                    } else {
                        Some(addr.to_string())
                    }
                })
                .collect()
        };

        if addrs.is_empty() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        for addr in &addrs {
            match tokio::net::TcpStream::connect(addr.as_str()).await {
                Ok(stream) => {
                    crate::io::configure_stream(&stream);
                    state.tunnel_pool.push(addr.clone(), stream);
                }
                Err(_) => continue,
            }
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Background task that maintains yamux multiplexed sessions to known nodes.
///
/// Checks every 5 seconds for nodes without an active mux session and
/// establishes one. Dead sessions are lazily removed by the MuxPool when
/// `open_stream()` fails.
async fn maintain_mux_sessions(state: Arc<SharedState>) {
    while !state.shutdown.is_cancelled() {
        let addrs: Vec<String> = {
            let nodes = state.known_nodes.load();
            nodes
                .values()
                .filter_map(|node| {
                    let addr = node.relay.as_deref().unwrap_or(&node.addr);
                    if addr.is_empty() || state.mux_pool.contains(addr) {
                        None
                    } else {
                        Some(addr.to_string())
                    }
                })
                .collect()
        };

        for addr in &addrs {
            match crate::mux::establish_mux_session(addr, &state.config.cluster_secret).await {
                Ok(handle) => {
                    info!("mux session established to {addr}");
                    state.mux_pool.insert(addr.clone(), handle);
                }
                Err(e) => {
                    tracing::debug!("failed to establish mux to {addr}: {e}");
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
