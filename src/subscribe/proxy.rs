use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::info;

use crate::tls::MeshStream;

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
/// tunnel to a node hosting the service. Nodes are selected via round-robin.
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

    let counter = Arc::new(AtomicUsize::new(0));

    loop {
        let (client, _) = listener.accept().await.context("proxy accept")?;
        client.set_nodelay(true).ok();
        let service_name = service_name.clone();
        let state = state.clone();
        let counter = counter.clone();

        tokio::spawn(async move {
            let nodes = find_service_nodes(&service_name, &state);
            if nodes.is_empty() {
                metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "no_nodes").increment(1);
                tracing::warn!("no nodes available for service {service_name}");
                return;
            }

            let mut client = client;
            let max_attempts = nodes.len().min(MAX_TUNNEL_ATTEMPTS);
            let start = Instant::now();
            let mut last_err: Option<anyhow::Error> = None;

            // Round-robin across nodes, advancing to the next one when a tunnel
            // fails to establish. The client stream is not touched until a
            // tunnel is up, so retrying it against another node is safe.
            for _ in 0..max_attempts {
                let idx = counter.fetch_add(1, Ordering::Relaxed) % nodes.len();
                let node = &nodes[idx];

                metrics::counter!("stouter_tunnel_requests_total", "service" => service_name.clone(), "node_id" => node.id.clone()).increment(1);

                match handle_connection(client, node, service_name.clone(), &state).await {
                    ConnectOutcome::Done(Ok(())) => {
                        metrics::histogram!("stouter_proxy_duration_seconds", "service" => service_name.clone())
                            .record(start.elapsed().as_secs_f64());
                        return;
                    }
                    ConnectOutcome::Done(Err(e)) => {
                        metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "connection_error").increment(1);
                        tracing::warn!("proxy connection error to node {}: {e}", node.id);
                        return;
                    }
                    ConnectOutcome::Retry(returned_client, e) => {
                        tracing::debug!("tunnel to node {} failed, trying another node: {e}", node.id);
                        client = returned_client;
                        last_err = Some(e);
                    }
                }
            }

            metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "node_unreachable").increment(1);
            match last_err {
                Some(e) => tracing::warn!(
                    "all {max_attempts} tunnel attempt(s) failed for service {service_name}: {e}"
                ),
                None => tracing::warn!("no tunnel attempts made for service {service_name}"),
            }
        });
    }
}

/// Find the nodes hosting a given service, ordered and health-filtered.
///
/// Scans the service index for entries matching `service_name`, resolves each
/// node_id to its current `NodeInfo`, sorts by node id so the round-robin
/// counter maps to a stable node across connections, and drops nodes whose
/// gossip liveness marker is stale.
///
/// Fails open: if the staleness filter would drop every node (e.g. a
/// cluster-wide gossip hiccup), the unfiltered set is returned so per-
/// connection retry can still find a working backend.
fn find_service_nodes(service_name: &str, state: &SharedState) -> Vec<NodeInfo> {
    let idx = state.service_index.load();
    let kn = state.known_nodes.load();

    let mut nodes: Vec<NodeInfo> = idx
        .iter()
        .filter(|((_, svc), _)| svc == service_name)
        .filter_map(|((node_id, _), _)| kn.get(node_id).cloned())
        .collect();

    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let now = Instant::now();
    let healthy: Vec<NodeInfo> = nodes
        .iter()
        .filter(|n| {
            n.last_seen
                .is_some_and(|seen| now.duration_since(seen) <= NODE_STALE_AFTER)
        })
        .cloned()
        .collect();

    if healthy.is_empty() {
        nodes
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

/// Outcome of a single tunnel attempt to one node.
enum ConnectOutcome {
    /// A tunnel was established and the client stream was handed to it. The
    /// client has been consumed regardless of the inner result, so the attempt
    /// must not be retried.
    Done(Result<()>),
    /// The tunnel could not be established and the client stream was never
    /// touched, so it is handed back for a retry against a different node.
    Retry(TcpStream, anyhow::Error),
}

/// Forward a single client connection through an authenticated tunnel to the
/// remote node.
///
/// Connection acquisition priority:
/// 1. Mux pool (open stream on existing yamux session)
/// 2. Reverse pool (instant check — free path to NAT'd nodes)
/// 3. Reverse pool with wait (for NAT-only nodes with no address)
/// 4. Cold TCP connect as last resort
///
/// Any failure before the client stream is touched returns
/// [`ConnectOutcome::Retry`] with the untouched `client` so the caller can try
/// another node.
async fn handle_connection(
    client: TcpStream,
    node: &NodeInfo,
    service_name: String,
    state: &Arc<SharedState>,
) -> ConnectOutcome {
    let timestamp_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(e) => {
            return ConnectOutcome::Retry(
                client,
                anyhow::anyhow!("system time before Unix epoch: {e}"),
            );
        }
    };

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
            if let Err(e) = send_and_check_tunnel(&mut compat, &msg).await {
                return ConnectOutcome::Retry(client, e);
            }
            metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => "mux")
                .record(connect_start.elapsed().as_secs_f64());
            return ConnectOutcome::Done(
                crate::io::proxy_bidirectional(client, compat)
                    .await
                    .context("mux proxy_bidirectional"),
            );
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
        match state
            .reverse_pool
            .take(&node.id, REVERSE_POOL_TIMEOUT)
            .await
        {
            Some(stream) => stream,
            None => {
                return ConnectOutcome::Retry(
                    client,
                    anyhow::anyhow!(
                        "node {} not reachable: no address and no reverse connections",
                        node.id
                    ),
                );
            }
        }
    } else {
        method = "cold";
        let tcp = match TcpStream::connect(connect_addr).await {
            Ok(tcp) => tcp,
            Err(e) => {
                return ConnectOutcome::Retry(
                    client,
                    anyhow::Error::new(e).context(format!("connect to {connect_addr}")),
                );
            }
        };
        tcp.set_nodelay(true).ok();
        match state.tls_connect(tcp).await {
            Ok(stream) => stream,
            Err(e) => {
                return ConnectOutcome::Retry(
                    client,
                    e.context(format!("TLS connect to {connect_addr}")),
                );
            }
        }
    };

    if let Err(e) = send_and_check_tunnel(&mut node_stream, &msg).await {
        return ConnectOutcome::Retry(client, e);
    }
    metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => method)
        .record(connect_start.elapsed().as_secs_f64());

    ConnectOutcome::Done(
        crate::io::proxy_bidirectional(client, node_stream)
            .await
            .context("proxy_bidirectional"),
    )
}
