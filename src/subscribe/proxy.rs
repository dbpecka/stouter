use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tracing::info;

use crate::config::NodeInfo;
use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::SharedState;

/// Timeout for waiting on a reverse pool connection when the node has no
/// direct or relay address (NAT-only).
const REVERSE_POOL_TIMEOUT: Duration = Duration::from_secs(5);

/// Run a local TCP proxy for a single service.
///
/// Binds on `127.0.0.1:0`, sends the assigned port back through `port_tx`,
/// then accepts connections and forwards each one through an authenticated
/// tunnel to the node.
pub async fn run_proxy(
    service: crate::config::Service,
    node: NodeInfo,
    state: Arc<SharedState>,
    port_tx: oneshot::Sender<u16>,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind proxy listener")?;

    let local_port = listener.local_addr()?.port();
    let _ = port_tx.send(local_port);

    info!(
        "Proxy for {} on 127.0.0.1:{}",
        service.name, local_port
    );

    let node_id = node.id.clone();

    loop {
        let (mut client, _) = listener.accept().await.context("proxy accept")?;
        client.set_nodelay(true).ok();
        let service_name = service.name.clone();
        let state = state.clone();
        let node_id = node_id.clone();

        tokio::spawn(async move {
            let current_node = {
                let kn = state.known_nodes.load();
                kn.get(&node_id).cloned()
            };
            let Some(node) = current_node else {
                tracing::warn!("proxy connection error: node {node_id} no longer known");
                return;
            };
            if let Err(e) = handle_connection(&mut client, &node, service_name, &state).await {
                tracing::warn!("proxy connection error: {e}");
            }
        });
    }
}

/// Forward a single client connection through an authenticated tunnel to the
/// remote node.
///
/// Connection acquisition priority:
/// 1. Reverse pool (instant check — free path to NAT'd nodes)
/// 2. Tunnel pool (pre-established warm connection to node/relay)
/// 3. Reverse pool with wait (for NAT-only nodes with no address)
/// 4. Cold TCP connect as last resort
async fn handle_connection(
    client: &mut TcpStream,
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

    let mut node_stream = if let Some(stream) = state.reverse_pool.try_take(&node.id) {
        stream
    } else if let Some(stream) = state.tunnel_pool.try_take(connect_addr) {
        stream
    } else if connect_addr.is_empty() {
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
        let stream = TcpStream::connect(connect_addr)
            .await
            .with_context(|| format!("connect to {connect_addr}"))?;

        stream.set_nodelay(true).ok();
        stream
    };

    // Send the tunnel request via the unified message protocol.
    gossip::send_message(&mut node_stream, &msg, &state.config.cluster_secret)
        .await
        .context("send tunnel request")?;

    // Read status byte.
    let mut status = [0u8; 1];
    node_stream
        .read_exact(&mut status)
        .await
        .context("read tunnel status byte")?;

    if status[0] != 0x00 {
        let mut len_buf = [0u8; 4];
        node_stream
            .read_exact(&mut len_buf)
            .await
            .context("read tunnel error length")?;
        let err_len = u32::from_be_bytes(len_buf) as usize;
        let mut err_bytes = vec![0u8; err_len];
        node_stream
            .read_exact(&mut err_bytes)
            .await
            .context("read tunnel error message")?;
        let msg = String::from_utf8_lossy(&err_bytes);
        bail!("node rejected tunnel: {msg}");
    }

    // Transparent bidirectional proxy.
    crate::io::proxy_bidirectional(client, &mut node_stream)
        .await
        .context("proxy_bidirectional")?;

    Ok(())
}
