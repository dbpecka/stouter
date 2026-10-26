use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt};
use tracing::info;

use crate::tls::MeshStream;

use super::balancer::{Backend, Balancer};
use crate::config::NodeInfo;
use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::SharedState;

/// Timeout for waiting on a reverse pool connection when the node has no
/// direct or relay address (NAT-only).
const REVERSE_POOL_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of distinct nodes to try for a single client connection
/// before giving up. Bounds retry amplification during a hosting outage.
const MAX_TUNNEL_ATTEMPTS: usize = 3;

/// A node not confirmed reachable within this window is treated as unhealthy
/// and excluded from load balancing. The gossip sync loop probes every peer
/// every 10s, so this tolerates roughly three missed cycles before ejection.
const NODE_STALE_AFTER: Duration = Duration::from_secs(30);

/// Run a local TCP proxy for a service, routing across all available nodes.
///
/// Binds on `127.0.0.1:0`, sends the assigned port back through `port_tx`,
/// then accepts connections and forwards each one through an authenticated
/// tunnel to a node hosting the service. Nodes are selected by smooth
/// weighted round-robin, and nodes whose tunnel setup keeps failing are
/// ejected from rotation for a while (see [`Balancer`]).
pub async fn run_proxy(
    service_name: String,
    state: Arc<SharedState>,
    port_tx: oneshot::Sender<u16>,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind proxy listener")?;

    let local_port = listener.local_addr()?.port();
    let _ = port_tx.send(local_port);

    info!("Proxy for {} on 127.0.0.1:{}", service_name, local_port);

    let balancer = Arc::new(Balancer::new());

    loop {
        let (client, _) = listener.accept().await.context("proxy accept")?;
        client.set_nodelay(true).ok();
        let service_name = service_name.clone();
        let state = state.clone();
        let balancer = balancer.clone();

        tokio::spawn(async move {
            serve_client(client, service_name, state, balancer).await;
        });
    }
}

/// Forward one client connection, trying up to [`MAX_TUNNEL_ATTEMPTS`]
/// distinct nodes until a tunnel is established.
async fn serve_client(
    client: TcpStream,
    service_name: String,
    state: Arc<SharedState>,
    balancer: Arc<Balancer>,
) {
    let backends = find_service_nodes(&service_name, &state);
    if backends.is_empty() {
        metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "no_nodes").increment(1);
        tracing::warn!("no nodes available for service {service_name}");
        return;
    }

    let max_attempts = backends.len().min(MAX_TUNNEL_ATTEMPTS);
    let start = Instant::now();
    let mut tried: Vec<String> = Vec::with_capacity(max_attempts);
    let mut last_err: Option<anyhow::Error> = None;

    // The client stream is not touched until a tunnel is up, so a failed
    // setup can safely be retried against another node.
    while tried.len() < max_attempts {
        let Some(idx) = balancer.pick(&backends, &tried, Instant::now()) else {
            break;
        };
        let node = &backends[idx].node;
        tried.push(node.id.clone());

        metrics::counter!("stouter_tunnel_requests_total", "service" => service_name.clone(), "node_id" => node.id.clone()).increment(1);

        let tunnel = match establish_tunnel(node, service_name.clone(), &state).await {
            Ok(tunnel) => tunnel,
            Err(e) => {
                tracing::debug!("tunnel to node {} failed, trying another node: {e}", node.id);
                if let Some(duration) = balancer.record_failure(&node.id, Instant::now()) {
                    metrics::counter!("stouter_outlier_ejections_total", "service" => service_name.clone(), "node_id" => node.id.clone()).increment(1);
                    tracing::warn!(
                        "ejecting node {} from service {service_name} for {}s after repeated tunnel failures: {e}",
                        node.id,
                        duration.as_secs()
                    );
                }
                last_err = Some(e);
                continue;
            }
        };
        balancer.record_success(&node.id);

        let result = match tunnel {
            Tunnel::Mux(stream) => crate::io::proxy_bidirectional(client, stream).await,
            Tunnel::Mesh(stream) => crate::io::proxy_bidirectional(client, stream).await,
        };
        match result {
            Ok(()) => {
                metrics::histogram!("stouter_proxy_duration_seconds", "service" => service_name.clone())
                    .record(start.elapsed().as_secs_f64());
            }
            Err(e) => {
                metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "connection_error").increment(1);
                tracing::warn!("proxy connection error to node {}: {e}", node.id);
            }
        }
        return;
    }

    metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "node_unreachable").increment(1);
    match last_err {
        Some(e) => tracing::warn!(
            "all {} tunnel attempt(s) failed for service {service_name}: {e}",
            tried.len()
        ),
        None => tracing::warn!("no tunnel attempts made for service {service_name}"),
    }
}

/// Find the nodes hosting a given service, ordered and health-filtered.
///
/// Scans the service index for entries matching `service_name`, resolves each
/// node_id to its current `NodeInfo`, sorts by node id so balancer state maps
/// to a stable node across connections, and drops nodes whose gossip liveness
/// marker is stale.
///
/// Fails open: if the staleness filter would drop every node (e.g. a
/// cluster-wide gossip hiccup), the unfiltered set is returned so per-
/// connection retry can still find a working backend.
fn find_service_nodes(service_name: &str, state: &SharedState) -> Vec<Backend> {
    let idx = state.service_index.load();
    let kn = state.known_nodes.load();

    let mut backends: Vec<Backend> = idx
        .iter()
        .filter(|((_, svc), _)| svc == service_name)
        .filter_map(|((node_id, _), svc)| {
            kn.get(node_id).map(|node| Backend { node: node.clone(), weight: svc.weight })
        })
        .collect();

    backends.sort_by(|a, b| a.node.id.cmp(&b.node.id));

    let now = Instant::now();
    let healthy: Vec<Backend> = backends
        .iter()
        .filter(|b| {
            b.node
                .last_seen
                .is_some_and(|seen| now.duration_since(seen) <= NODE_STALE_AFTER)
        })
        .cloned()
        .collect();

    if healthy.is_empty() {
        backends
    } else {
        healthy
    }
}

/// Send a tunnel request on a stream, read the status byte, and bail on error.
async fn send_and_check_tunnel<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    msg: &Message,
) -> Result<()> {
    gossip::send_message(stream, msg)
        .await
        .context("send tunnel request")?;

    let mut status = [0u8; 1];
    stream
        .read_exact(&mut status)
        .await
        .context("read tunnel status byte")?;

    if status[0] != 0x00 {
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .context("read tunnel error length")?;
        let err_len = u32::from_be_bytes(len_buf) as usize;
        let mut err_bytes = vec![0u8; err_len];
        stream
            .read_exact(&mut err_bytes)
            .await
            .context("read tunnel error message")?;
        let msg = String::from_utf8_lossy(&err_bytes);
        bail!("node rejected tunnel: {msg}");
    }

    Ok(())
}

/// An established tunnel to a remote node, ready for proxying.
enum Tunnel {
    /// A stream on a pooled yamux session.
    Mux(Compat<yamux::Stream>),
    /// A dedicated mTLS connection (boxed: a TLS stream is large).
    Mesh(Box<MeshStream>),
}

/// Establish an authenticated tunnel to `service_name` on `node`.
///
/// Connection acquisition priority:
/// 1. Mux pool (open stream on existing yamux session)
/// 2. Reverse pool (instant check — free path to NAT'd nodes)
/// 3. Reverse pool with wait (for NAT-only nodes with no address)
/// 4. Cold TCP connect as last resort
async fn establish_tunnel(
    node: &NodeInfo,
    service_name: String,
    state: &Arc<SharedState>,
) -> Result<Tunnel> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let msg = Message::TunnelRequest {
        node_id: node.id.clone(),
        service_name,
        timestamp_ms,
    };

    let connect_addr = node.relay.as_deref().unwrap_or(&node.addr);
    let connect_start = Instant::now();

    // --- Try mux pool first (multiplexed yamux stream) ---
    if !connect_addr.is_empty() {
        if let Some(yamux_stream) = state.mux_pool.open_stream(connect_addr).await {
            let mut compat = yamux_stream.compat();
            send_and_check_tunnel(&mut compat, &msg).await?;
            metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => "mux")
                .record(connect_start.elapsed().as_secs_f64());
            return Ok(Tunnel::Mux(compat));
        }
    }

    // --- Fallback: legacy single-connection path ---
    let method;
    let mut node_stream: MeshStream = if let Some(stream) = state.reverse_pool.try_take(&node.id) {
        method = "reverse";
        stream
    } else if let Some(stream) = state.tunnel_pool.try_take(connect_addr) {
        method = "pool";
        stream
    } else if connect_addr.is_empty() {
        method = "reverse";
        state
            .reverse_pool
            .take(&node.id, REVERSE_POOL_TIMEOUT)
            .await
            .with_context(|| {
                format!("node {} not reachable: no address and no reverse connections", node.id)
            })?
    } else {
        method = "cold";
        let tcp = TcpStream::connect(connect_addr)
            .await
            .with_context(|| format!("connect to {connect_addr}"))?;
        tcp.set_nodelay(true).ok();
        state
            .tls_connect(tcp)
            .await
            .with_context(|| format!("TLS connect to {connect_addr}"))?
    };

    send_and_check_tunnel(&mut node_stream, &msg).await?;
    metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => method)
        .record(connect_start.elapsed().as_secs_f64());

    Ok(Tunnel::Mesh(Box::new(node_stream)))
}
