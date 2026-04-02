use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tracing::info;

use crate::config::NodeInfo;
use crate::gossip::CONN_TUNNEL;
use crate::state::SharedState;

/// Authentication request sent to a node daemon to open a tunnel.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TunnelRequest {
    /// Target node ID. Needed for relay dispatch; redundant for direct connections.
    node_id: String,
    service_name: String,
    timestamp_ms: u64,
    /// HMAC-SHA256(cluster_secret, "{service_name}:{timestamp_ms}") in hex.
    token: String,
}

/// Run a local TCP proxy for a single service.
///
/// Binds on `127.0.0.1:0`, sends the assigned port back through `port_tx`,
/// then accepts connections and forwards each one through an authenticated
/// tunnel to the node. If the node has a relay set, connections go through
/// the relay instead.
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

    loop {
        let (client, _) = listener.accept().await.context("proxy accept")?;
        let node = node.clone();
        let service_name = service.name.clone();
        let state = state.clone();

        tokio::spawn(async move {
            if let Err(e) =
                handle_connection(client, &node, service_name, &state).await
            {
                tracing::warn!("proxy connection error: {e}");
            }
        });
    }
}

/// Forward a single client connection through an authenticated tunnel to the
/// remote node. Routes through a relay, or uses a reverse connection from the
/// local pool for outbound-only nodes.
async fn handle_connection(
    mut client: TcpStream,
    node: &NodeInfo,
    service_name: String,
    state: &Arc<SharedState>,
) -> Result<()> {
    let cluster_secret = &state.config.cluster_secret;

    // Build the tunnel request.
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let token_data = format!("{service_name}:{timestamp_ms}");
    let token = crate::crypto::sign(cluster_secret, &token_data);

    let req = TunnelRequest {
        node_id: node.id.clone(),
        service_name,
        timestamp_ms,
        token,
    };

    let req_bytes = serde_json::to_vec(&req).context("serialize TunnelRequest")?;
    let len = req_bytes.len() as u32;

    // Get a stream to the node. For outbound-only nodes with no relay, use
    // a reverse connection from our local pool. Otherwise connect to the
    // relay or node address directly.
    let mut node_stream = if let Some(stream) = try_take_reverse(&node.id, state).await {
        stream
    } else {
        let connect_addr = node.relay.as_deref().unwrap_or(&node.addr);
        if connect_addr.is_empty() {
            bail!(
                "node {} is not reachable: no address, no relay, and no reverse connections",
                node.id
            );
        }
        let mut stream = TcpStream::connect(connect_addr)
            .await
            .with_context(|| format!("connect to {connect_addr}"))?;

        // Identify this connection as a tunnel connection (only needed for
        // outbound TCP connections; reverse connections already expect a
        // tunnel frame directly).
        stream
            .write_all(&[CONN_TUNNEL])
            .await
            .context("write CONN_TUNNEL byte")?;

        stream
    };

    // Send the tunnel request frame.
    node_stream
        .write_all(&len.to_be_bytes())
        .await
        .context("write tunnel request length")?;
    node_stream
        .write_all(&req_bytes)
        .await
        .context("write tunnel request body")?;

    // Read status byte.
    let mut status = [0u8; 1];
    node_stream
        .read_exact(&mut status)
        .await
        .context("read tunnel status byte")?;

    if status[0] != 0x00 {
        // Read the error message from the node.
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
    tokio::io::copy_bidirectional(&mut client, &mut node_stream)
        .await
        .context("copy_bidirectional")?;

    Ok(())
}

/// Try to pop an idle reverse connection for `node_id` from the pool.
async fn try_take_reverse(node_id: &str, state: &Arc<SharedState>) -> Option<TcpStream> {
    let mut pool = state.reverse_pool.write().await;
    let conns = pool.get_mut(node_id)?;
    if conns.is_empty() {
        None
    } else {
        Some(conns.swap_remove(conns.len() - 1))
    }
}
