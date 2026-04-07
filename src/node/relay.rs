use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::info;

use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::SharedState;

/// Maximum time to wait for an idle reverse connection before failing.
const POOL_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum timestamp skew allowed for reverse registration authentication.
const MAX_SKEW_MS: u64 = 30_000;

/// Accept a reverse connection from a NAT'd node.
///
/// The initial `ReverseRegistration` message has already been parsed by the
/// dispatcher. This function verifies the timestamp, updates known_nodes,
/// and stores the connection in the reverse pool.
pub async fn handle_reverse_registration(
    stream: TcpStream,
    node_id: String,
    timestamp_ms: u64,
    state: Arc<SharedState>,
) -> Result<()> {
    // --- Verify timestamp ---
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let skew = now_ms.abs_diff(timestamp_ms);
    if skew > MAX_SKEW_MS {
        bail!(
            "reverse registration timestamp skew too large: {skew}ms (node {})",
            node_id
        );
    }

    info!(
        "reverse connection registered for node {} (relay via {})",
        node_id,
        state.config.peer_addr()
    );

    // --- Update known_nodes relay field ---
    {
        let our_addr = state.config.peer_addr().to_owned();
        let nid = node_id.clone();
        state
            .update_known_nodes(|kn| {
                if let Some(node) = kn.get_mut(&nid) {
                    node.relay = Some(our_addr);
                } else {
                    kn.insert(
                        nid.clone(),
                        crate::config::NodeInfo {
                            id: nid,
                            addr: String::new(),
                            relay: Some(our_addr),
                        },
                    );
                }
            })
            .await;
    }

    // --- Store in reverse pool ---
    state.reverse_pool.push(node_id, stream);

    Ok(())
}

/// Relay a tunnel request to a NAT'd node via a reverse connection.
///
/// Takes an idle reverse connection from the pool, forwards the tunnel request
/// as a [`Message::TunnelRequest`], reads the response, and bridges the two
/// streams. Generic over the subscriber stream type so it works with both
/// raw TCP and yamux streams.
pub async fn relay_tunnel<S>(
    mut subscriber: S,
    node_id: &str,
    service_name: &str,
    timestamp_ms: u64,
    state: &Arc<SharedState>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reverse_stream = match state
        .reverse_pool
        .take(node_id, POOL_WAIT_TIMEOUT)
        .await
    {
        Some(s) => s,
        None => {
            let msg = format!("no idle reverse connections for node {node_id}");
            crate::node::tunnel::send_tunnel_error(&mut subscriber, &msg).await;
            bail!("{msg}");
        }
    };

    // Forward the tunnel request to the NAT'd node.
    let msg = Message::TunnelRequest {
        node_id: node_id.to_string(),
        service_name: service_name.to_string(),
        timestamp_ms,
    };
    gossip::send_message(&mut reverse_stream, &msg, &state.config.cluster_secret)
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
    subscriber
        .flush()
        .await
        .context("flush tunnel status to subscriber")?;

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
        subscriber
            .flush()
            .await
            .context("flush tunnel error to subscriber")?;

        bail!("NAT'd node rejected tunnel");
    }

    // Bridge the subscriber and reverse connection streams.
    crate::io::proxy_bidirectional(subscriber, reverse_stream)
        .await
        .context("relay proxy_bidirectional")?;

    Ok(())
}
