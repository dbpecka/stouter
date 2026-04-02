use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::info;

use crate::state::SharedState;

/// Maximum allowed reverse registration request size: 64 KiB.
const MAX_REQUEST_BYTES: u32 = 65536;

/// Maximum timestamp skew allowed for reverse registration authentication.
const MAX_SKEW_MS: u64 = 30_000;

/// Registration request sent by a NAT'd node to establish a reverse connection.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseRegistration {
    pub node_id: String,
    pub timestamp_ms: u64,
    /// HMAC-SHA256(cluster_secret, "reverse:{node_id}:{timestamp_ms}") in hex.
    pub token: String,
}

/// Accept a reverse connection from a NAT'd node.
///
/// Protocol:
/// 1. Read 4-byte big-endian length prefix + JSON [`ReverseRegistration`].
/// 2. Verify timestamp is within ±30 seconds.
/// 3. Verify the HMAC token.
/// 4. Store the connection in the reverse pool.
/// 5. Update known_nodes so the NAT'd node's relay field points to us.
pub async fn handle_reverse_registration(
    mut stream: TcpStream,
    state: Arc<SharedState>,
) -> Result<()> {
    // --- Read registration frame ---
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("read reverse registration length")?;
    let len = u32::from_be_bytes(len_buf);

    if len > MAX_REQUEST_BYTES {
        bail!("reverse registration too large: {len} bytes");
    }

    let mut frame = vec![0u8; len as usize];
    stream
        .read_exact(&mut frame)
        .await
        .context("read reverse registration body")?;

    let reg: ReverseRegistration =
        serde_json::from_slice(&frame).context("deserialize ReverseRegistration")?;

    // --- Verify timestamp ---
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let skew = now_ms.abs_diff(reg.timestamp_ms);
    if skew > MAX_SKEW_MS {
        bail!(
            "reverse registration timestamp skew too large: {skew}ms (node {})",
            reg.node_id
        );
    }

    // --- Verify HMAC token ---
    let expected_data = format!("reverse:{}:{}", reg.node_id, reg.timestamp_ms);
    if !crate::crypto::verify(&state.config.cluster_secret, &expected_data, &reg.token) {
        bail!(
            "reverse registration token verification failed (node {})",
            reg.node_id
        );
    }

    info!(
        "reverse connection registered for node {} (relay via {})",
        reg.node_id,
        state.config.peer_addr()
    );

    // --- Update known_nodes relay field ---
    {
        let our_addr = state.config.peer_addr().to_owned();
        let mut kn = state.known_nodes.write().await;
        if let Some(node) = kn.iter_mut().find(|n| n.id == reg.node_id) {
            node.relay = Some(our_addr);
        } else {
            // We don't know this node yet; add it with relay pointing to us.
            // The addr is empty since it's not directly reachable.
            kn.push(crate::config::NodeInfo {
                id: reg.node_id.clone(),
                addr: String::new(),
                relay: Some(our_addr),
            });
        }
    }

    // --- Store in reverse pool ---
    {
        let mut pool = state.reverse_pool.write().await;
        pool.entry(reg.node_id).or_default().push(stream);
    }

    Ok(())
}

/// Relay a tunnel request to a NAT'd node via a reverse connection.
///
/// Takes an idle reverse connection from the pool, forwards the tunnel request
/// frame, reads the response, and bridges the two streams.
///
/// `tunnel_frame` is the raw bytes of the TunnelRequest (length prefix + body)
/// that were already read from the subscriber.
pub async fn relay_tunnel(
    mut subscriber: TcpStream,
    node_id: &str,
    tunnel_frame: &[u8],
    state: &Arc<SharedState>,
) -> Result<()> {
    // Pop a reverse connection from the pool.
    let mut reverse_stream = {
        let mut pool = state.reverse_pool.write().await;
        let conns = pool.get_mut(node_id).context("no reverse connections for node")?;
        if conns.is_empty() {
            bail!("no idle reverse connections for node {node_id}");
        }
        conns.swap_remove(conns.len() - 1)
    };

    // Forward the tunnel request frame to the NAT'd node.
    reverse_stream
        .write_all(tunnel_frame)
        .await
        .context("forward tunnel request to reverse connection")?;

    // Read the status byte from the NAT'd node and forward to subscriber.
    let mut status = [0u8; 1];
    reverse_stream
        .read_exact(&mut status)
        .await
        .context("read tunnel status from reverse connection")?;

    subscriber
        .write_all(&status)
        .await
        .context("forward tunnel status to subscriber")?;

    if status[0] != 0x00 {
        // Forward the error message.
        let mut len_buf = [0u8; 4];
        reverse_stream
            .read_exact(&mut len_buf)
            .await
            .context("read tunnel error length from reverse")?;
        subscriber
            .write_all(&len_buf)
            .await
            .context("forward tunnel error length")?;

        let err_len = u32::from_be_bytes(len_buf) as usize;
        let mut err_bytes = vec![0u8; err_len];
        reverse_stream
            .read_exact(&mut err_bytes)
            .await
            .context("read tunnel error from reverse")?;
        subscriber
            .write_all(&err_bytes)
            .await
            .context("forward tunnel error to subscriber")?;

        bail!("NAT'd node rejected tunnel");
    }

    // Bridge the subscriber and reverse connection streams.
    tokio::io::copy_bidirectional(&mut subscriber, &mut reverse_stream)
        .await
        .context("relay copy_bidirectional")?;

    Ok(())
}
