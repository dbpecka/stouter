mod api;
mod balancer;
mod http;
mod listener;
mod proxy;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::info;

use crate::gossip;
use crate::state::SharedState;

/// Port and metadata for a locally proxied service.
pub(crate) struct ServiceInfo {
    pub port: u16,
    pub domains: Vec<String>,
    /// Integration metadata from the service config (e.g. `traefik.scheme`).
    pub meta: std::collections::BTreeMap<String, String>,
}

/// A service a proxy should be running for, as of the current config.
struct DesiredService {
    name: String,
    domains: Vec<String>,
    meta: std::collections::BTreeMap<String, String>,
    local_port: Option<u16>,
}

/// A running local proxy for a single service.
struct ProxyHandle {
    /// The bare service name, used as the key in `service_ports`.
    service_name: String,
    /// The configured `localPort` this proxy was started for (`None`: random).
    requested_port: Option<u16>,
    abort: tokio::task::AbortHandle,
}

/// Run the subscribe daemon.
///
/// Starts the gossip sync loop, REST API, local proxy manager, and a TCP
/// listener that accepts inbound connections.
pub async fn run_subscribe(state: Arc<SharedState>, metrics_handle: PrometheusHandle) -> Result<()> {
    tokio::spawn(gossip::run_sync_loop(state.clone()));
    tokio::spawn(gossip::run_member_log(state.clone()));
    tokio::spawn(gossip::run_config_poll_loop(state.clone()));
    if matches!(state.config.mode, crate::config::Mode::NodeSubscribe) {
        tokio::spawn(crate::health::run_health_checks(state.clone()));
    }

    let service_ports: Arc<RwLock<HashMap<String, ServiceInfo>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let api_ports = service_ports.clone();
    let api_state = state.clone();
    let api_port = state.config.api_port();
    tokio::spawn(async move {
        if let Err(e) = api::run_api(api_ports, api_state, metrics_handle, api_port).await {
            tracing::error!("REST API server exited: {e}");
        }
    });

    let http_proxies = http::HttpProxies::new(state.clone());
    tokio::spawn(manage_proxies(state.clone(), service_ports, http_proxies.clone()));
    if let Some(addr) = state.config.http_listen.clone() {
        let (state, proxies) = (state.clone(), http_proxies.clone());
        tokio::spawn(async move {
            if let Err(e) = listener::run_http_listener(addr, state, proxies).await {
                tracing::error!("HTTP listener exited: {e:#}");
            }
        });
    }
    tokio::spawn(fill_tunnel_pool(state.clone()));
    tokio::spawn(maintain_mux_sessions(state.clone()));

    if state.config.outbound_only {
        info!(
            "Subscribe daemon running in outbound-only mode (NAT'd), node_id={}",
            state.config.node_id
        );
        crate::node::reverse::run_reverse_pool_loop(state).await;
    } else {
        let listener = TcpListener::bind(&state.config.bind)
            .await
            .with_context(|| format!("bind gossip listener to {}", state.config.bind))?;

        info!("Subscribe daemon listening on {}", state.config.bind);

        loop {
            let (stream, peer_addr) = tokio::select! {
                result = listener.accept() => result.context("accept connection")?,
                _ = state.shutdown.cancelled() => {
                    info!("subscribe listener shutting down");
                    break;
                }
            };
            stream.set_nodelay(true).ok();
            let state = state.clone();
            let peer = peer_addr.to_string();
            let guard = state.in_flight.track();
            tokio::spawn(async move {
                match state.tls_accept(stream).await {
                    Ok(mesh) => gossip::dispatch_connection(mesh, peer, state).await,
                    Err(e) => tracing::debug!("TLS accept from {peer}: {e}"),
                }
                drop(guard);
            });
        }
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
    http_proxies: Arc<http::HttpProxies>,
) {
    let mut proxies: HashMap<String, ProxyHandle> = HashMap::new();
    // Last start error per service key, so a port stuck in use is reported
    // once rather than on every pass.
    let mut start_errors: HashMap<String, String> = HashMap::new();
    // Last localPort conflict per service key, likewise reported once.
    let mut port_conflicts: HashMap<String, String> = HashMap::new();
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
        // node is reachable, with what proxy setup and the REST API need.
        let mut desired: HashMap<String, DesiredService> = HashMap::new();
        let mut seen_svc_keys: HashSet<String> = HashSet::new();
        for group in &dc.service_groups {
            for svc in &group.services {
                // In nodeSubscribe mode, services this node owns are already running
                // locally — tunneling to self would be a pointless double-hop.
                if matches!(state.config.mode, crate::config::Mode::NodeSubscribe)
                    && svc.node_id == state.config.node_id
                {
                    continue;
                }
                if let Some(filter) = &state.config.subscribe_filter {
                    if !filter.is_empty() && !filter.allows(&group.name, &svc.name) {
                        continue;
                    }
                }
                let key = format!("{}/{}", group.name, svc.name);
                if nodes.contains_key(&svc.node_id) && seen_svc_keys.insert(key.clone()) {
                    let local_port = match dc.local_port_for(&svc.name) {
                        Ok(port) => {
                            port_conflicts.remove(&key);
                            port
                        }
                        Err(e) => {
                            if port_conflicts.insert(key.clone(), e.clone()).as_ref() != Some(&e) {
                                tracing::warn!("{e}; using a random port for {key}");
                            }
                            None
                        }
                    };
                    desired.insert(
                        key,
                        DesiredService {
                            name: svc.name.clone(),
                            domains: svc.domains.clone(),
                            meta: svc.meta.clone(),
                            local_port,
                        },
                    );
                }
            }
        }

        // Abort proxies for services that are no longer desired, or whose
        // configured localPort changed (a listener can't move ports).
        let removed: Vec<String> = proxies
            .iter()
            .filter(|(key, handle)| {
                desired.get(*key).is_none_or(|d| d.local_port != handle.requested_port)
            })
            .map(|(key, _)| key.clone())
            .collect();

        for key in removed {
            if let Some(handle) = proxies.remove(&key) {
                handle.abort.abort();
                service_ports.write().await.remove(&handle.service_name);
                info!("Stopped proxy for {key}");
            }
        }

        let configured: HashSet<&str> =
            dc.service_groups.iter().flat_map(|g| &g.services).map(|s| s.name.as_str()).collect();
        http_proxies.retain(|name| configured.contains(name));

        // Start proxies for newly desired services.
        for (key, DesiredService { name: svc_name, domains: svc_domains, meta: svc_meta, local_port: requested_port }) in desired {
            if let Some(handle) = proxies.get(&key) {
                // Keep the API's view current when a running service's
                // domains or meta change, without restarting its proxy.
                let mut ports = service_ports.write().await;
                if let Some(info) = ports.get_mut(&handle.service_name) {
                    info.domains = svc_domains;
                    info.meta = svc_meta;
                }
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
                http_proxies.clone(),
                requested_port,
                port_tx,
            ));
            let abort = join_handle.abort_handle();

            match port_rx.await {
                Ok(Ok(local_port)) => {
                    start_errors.remove(&key);
                    service_ports.write().await.insert(svc_name.clone(), ServiceInfo { port: local_port, domains: svc_domains, meta: svc_meta });
                    proxies.insert(key.clone(), ProxyHandle { service_name: svc_name, requested_port, abort });
                    info!("Started proxy for {key} on 127.0.0.1:{local_port}");
                }
                Ok(Err(e)) => {
                    abort.abort();
                    // Retried every pass; reported once per distinct error.
                    if start_errors.insert(key.clone(), e.clone()).as_ref() != Some(&e) {
                        tracing::warn!("Proxy for {key} failed to start: {e}; retrying");
                    }
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
                Ok(tcp) => {
                    crate::io::configure_stream(&tcp);
                    match state.tls_connect(tcp).await {
                        Ok(stream) => state.tunnel_pool.push(addr.clone(), stream),
                        Err(e) => tracing::debug!("TLS connect to {addr} failed: {e}"),
                    }
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
            match crate::mux::establish_mux_session(addr, &state).await {
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
