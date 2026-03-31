mod api;
mod dns;
mod proxy;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::time;
use tracing::{debug, info};

use crate::config::Service;
use crate::gossip;
use crate::state::SharedState;

/// A running local proxy for a single service.
struct ProxyHandle {
    #[allow(dead_code)]
    local_port: u16,
    abort: tokio::task::AbortHandle,
}

/// Run the subscribe daemon.
///
/// Starts the gossip sync loop, DNS server, local proxy manager, and a TCP
/// listener that accepts inbound gossip connections.
pub async fn run_subscribe(state: Arc<SharedState>) -> Result<()> {
    tokio::spawn(gossip::run_sync_loop(state.clone()));
    tokio::spawn(gossip::run_member_log(state.clone()));
    tokio::spawn(gossip::run_config_poll_loop(state.clone()));

    let service_ports: Arc<RwLock<HashMap<String, u16>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let dns_ports = service_ports.clone();
    tokio::spawn(async move {
        if let Err(e) = dns::run_dns(dns_ports, 5380).await {
            tracing::error!("DNS server exited: {e}");
        }
    });

    let api_ports = service_ports.clone();
    tokio::spawn(async move {
        if let Err(e) = api::run_api(api_ports, 5381).await {
            tracing::error!("REST API server exited: {e}");
        }
    });

    let listener = TcpListener::bind(&state.config.bind)
        .await
        .with_context(|| format!("bind gossip listener to {}", state.config.bind))?;

    info!("Subscribe daemon listening on {}", state.config.bind);

    tokio::spawn(manage_proxies(state.clone(), service_ports));

    loop {
        let (mut stream, peer_addr) = listener.accept().await.context("accept connection")?;
        let state = state.clone();
        tokio::spawn(async move {
            let mut type_buf = [0u8; 1];
            if let Err(e) = stream.read_exact(&mut type_buf).await {
                debug!("failed to read connection type from {peer_addr}: {e}");
                return;
            }
            match type_buf[0] {
                gossip::CONN_GOSSIP => {
                    gossip::handle_gossip_connection(stream, state).await;
                }
                gossip::CONN_STATUS => {
                    gossip::handle_status_connection(stream, state).await;
                }
                byte => {
                    debug!("unknown connection type 0x{byte:02x} from {peer_addr}");
                }
            }
        });
    }
}

/// Background task that reconciles running proxies with the current set of
/// known services.
///
/// Every 2 seconds it computes the desired set of (service, node_addr) pairs
/// from `state.dynamic_config` and `state.known_nodes`, starts proxies for
/// new services, and aborts proxies for services that have disappeared.
async fn manage_proxies(
    state: Arc<SharedState>,
    service_ports: Arc<RwLock<HashMap<String, u16>>>,
) {
    let mut proxies: HashMap<String, ProxyHandle> = HashMap::new();
    let mut interval = time::interval(Duration::from_secs(2));

    loop {
        interval.tick().await;

        // Snapshot current desired state.
        let dc = state.dynamic_config.read().await.clone();
        let nodes = state.known_nodes.read().await.clone();
        let cluster_secret = state.config.cluster_secret.clone();

        // Build a map: service_name -> (Service, node_addr).
        let mut desired: HashMap<String, (Service, String)> = HashMap::new();
        for group in &dc.service_groups {
            for svc in &group.services {
                if let Some(node) = nodes.iter().find(|n| n.id == svc.node_id) {
                    desired.insert(svc.name.clone(), (svc.clone(), node.addr.clone()));
                }
            }
        }

        // Abort proxies for services that are no longer desired.
        let removed: Vec<String> = proxies
            .keys()
            .filter(|name| !desired.contains_key(*name))
            .cloned()
            .collect();

        for name in removed {
            if let Some(handle) = proxies.remove(&name) {
                handle.abort.abort();
                service_ports.write().await.remove(&name);
                info!("Stopped proxy for {name}");
            }
        }

        // Start proxies for newly desired services.
        for (name, (svc, node_addr)) in desired {
            if proxies.contains_key(&name) {
                continue;
            }

            let (port_tx, port_rx) = tokio::sync::oneshot::channel();
            let join_handle = tokio::spawn(proxy::run_proxy(
                svc,
                node_addr,
                cluster_secret.clone(),
                port_tx,
            ));
            let abort = join_handle.abort_handle();

            match port_rx.await {
                Ok(local_port) => {
                    service_ports.write().await.insert(name.clone(), local_port);
                    proxies.insert(name.clone(), ProxyHandle { local_port, abort });
                    info!("Started proxy for {name} on 127.0.0.1:{local_port}");
                }
                Err(_) => {
                    // run_proxy exited before sending the port (bind failure etc.).
                    abort.abort();
                    tracing::warn!("Proxy for {name} failed to start");
                }
            }
        }
    }
}
