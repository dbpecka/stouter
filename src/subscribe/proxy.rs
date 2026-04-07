use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::info;

use crate::config::NodeInfo;
use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::SharedState;

/// Timeout for waiting on a reverse pool connection when the node has no
/// direct or relay address (NAT-only).
const REVERSE_POOL_TIMEOUT: Duration = Duration::from_secs(5);

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

            let idx = counter.fetch_add(1, Ordering::Relaxed) % nodes.len();
            let node = &nodes[idx];

            metrics::counter!("stouter_tunnel_requests_total", "service" => service_name.clone(), "node_id" => node.id.clone()).increment(1);
            let start = Instant::now();

            if let Err(e) = handle_connection(client, node, service_name.clone(), &state).await {
                metrics::counter!("stouter_tunnel_errors_total", "service" => service_name, "reason" => "connection_error").increment(1);
                tracing::warn!("proxy connection error: {e}");
            } else {
                metrics::histogram!("stouter_proxy_duration_seconds", "service" => service_name)
                    .record(start.elapsed().as_secs_f64());
            }
        });
    }
}

/// Find all reachable nodes hosting a given service.
///
/// Scans the service index for entries matching `service_name` and resolves
/// each node_id to its current `NodeInfo` from the gossip-discovered member
/// list.
fn find_service_nodes(service_name: &str, state: &SharedState) -> Vec<NodeInfo> {
    let idx = state.service_index.load();
    let kn = state.known_nodes.load();

    idx.iter()
        .filter(|((_, svc), _)| svc == service_name)
        .filter_map(|((node_id, _), _)| kn.get(node_id).cloned())
        .collect()
}

/// Send a tunnel request on a stream, read the status byte, and bail on error.
async fn send_and_check_tunnel<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    msg: &Message,
    secret: &str,
) -> Result<()> {
    gossip::send_message(stream, msg, secret)
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

/// Forward a single client connection through an authenticated tunnel to the
/// remote node.
///
/// Connection acquisition priority:
/// 1. Mux pool (open stream on existing yamux session)
/// 2. Reverse pool (instant check — free path to NAT'd nodes)
/// 3. Reverse pool with wait (for NAT-only nodes with no address)
/// 4. Cold TCP connect as last resort
async fn handle_connection(
    client: TcpStream,
    node: &NodeInfo,
    service_name: String,
    state: &Arc<SharedState>,
) -> Result<()> {
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
            send_and_check_tunnel(&mut compat, &msg, &state.config.cluster_secret).await?;
            metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => "mux")
                .record(connect_start.elapsed().as_secs_f64());
            crate::io::proxy_bidirectional(client, compat)
                .await
                .context("mux proxy_bidirectional")?;
            return Ok(());
        }
    }

    // --- Fallback: legacy single-connection path ---
    let method;
    let mut node_stream = if let Some(stream) = state.reverse_pool.try_take(&node.id) {
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
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "node {} not reachable: no address and no reverse connections",
                    node.id
                )
            })?
    } else {
        method = "cold";
        let stream = TcpStream::connect(connect_addr)
            .await
            .with_context(|| format!("connect to {connect_addr}"))?;

        stream.set_nodelay(true).ok();
        stream
    };

    send_and_check_tunnel(&mut node_stream, &msg, &state.config.cluster_secret).await?;
    metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => method)
        .record(connect_start.elapsed().as_secs_f64());

    crate::io::proxy_bidirectional(client, node_stream)
        .await
        .context("proxy_bidirectional")?;

    Ok(())
}
