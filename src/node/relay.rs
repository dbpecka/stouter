use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info};

use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::SharedState;
use crate::tls::MeshStream;

/// Maximum time to wait for an idle reverse connection before failing.
const POOL_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Most idle reverse connections one node may hold here (and so the most
/// `reversePoolSize` can usefully be).
pub const MAX_IDLE_REVERSE_PER_NODE: usize = 1024;

/// Reverse connections tried for one tunnel before giving up.
const RELAY_ATTEMPTS: usize = 3;

/// Time allowed for a NAT'd node to answer a forwarded tunnel request.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum timestamp skew allowed for reverse registration authentication.
const MAX_SKEW_MS: u64 = 30_000;

/// Accept a reverse connection from a NAT'd node.
///
/// The initial `ReverseRegistration` message has already been parsed by the
/// dispatcher. This function verifies the timestamp, updates known_nodes,
/// and stores the connection in the reverse pool.
pub async fn handle_reverse_registration(
    stream: MeshStream,
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

    // Only node X may register reverse connections for X: tunnels to X are
    // sent down them, so anyone else registering could take X's traffic.
    if stream.peer_cn() != Some(node_id.as_str()) {
        bail!(
            "reverse registration for node {node_id} from a certificate for {}",
            stream.peer_cn().unwrap_or("no one")
        );
    }

    // A NAT'd node keeps `reversePoolSize` idle connections here; more than
    // this is one hogging file descriptors.
    let idle = state.reverse_pool.count(&node_id);
    if idle >= MAX_IDLE_REVERSE_PER_NODE {
        bail!("node {node_id} already holds {idle} idle reverse connections here (limit {MAX_IDLE_REVERSE_PER_NODE})");
    }

    // --- Update known_nodes relay field ---
    {
        // Where peers reach us for this node. Bound to all interfaces with
        // no `advertiseAddr`, our own address is a wildcard nobody else can
        // dial: use the one the NAT'd node just reached us on instead.
        let mut our_addr = state.config.peer_addr().to_owned();
        if gossip::is_wildcard(&our_addr) {
            if let Ok(local) = stream.local_addr() {
                our_addr = local.to_string();
            }
        }
        let nid = node_id.clone();
        let relay_addr = our_addr.clone();
        // Whether we weren't already relaying for it: worth one line at
        // info; every further connection (one per tunnel) only at debug.
        let newly = state
            .update_known_nodes(|kn| {
                let now = Instant::now();
                if let Some(node) = kn.get_mut(&nid) {
                    let newly = node.relay.as_deref() != Some(our_addr.as_str());
                    node.relay = Some(our_addr);
                    // A reverse registration is direct contact with the NAT'd
                    // node, so it counts as a liveness signal.
                    node.last_seen = Some(now);
                    node.heard_at = Some(now);
                    newly
                } else {
                    // Direct contact: it's here, whatever gossip said before.
                    state.unbury(&nid);
                    kn.insert(
                        nid.clone(),
                        crate::config::NodeInfo {
                            id: nid,
                            relay: Some(our_addr),
                            last_seen: Some(now),
                            heard_at: Some(now),
                            ..Default::default()
                        },
                    );
                    true
                }
            })
            .await;
        if newly {
            info!("relaying for node {node_id} (reverse connections registered; peers reach it via {relay_addr})");
        } else {
            debug!("reverse connection registered for node {node_id}");
        }
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
    let msg = Message::TunnelRequest {
        node_id: node_id.to_string(),
        service_name: service_name.to_string(),
        timestamp_ms,
    };

    // Forward the request down a reverse connection and read the NAT'd
    // node's status byte. A connection can die after the pool's liveness
    // check (or be half-dead behind a NAT), so on failure try another: no
    // client data has been sent yet, so retrying is safe.
    let mut last_err: Option<anyhow::Error> = None;
    let mut ready = None;
    for _ in 0..RELAY_ATTEMPTS {
        let Some(mut stream) = state.reverse_pool.take(node_id, POOL_WAIT_TIMEOUT).await else {
            break;
        };
        let attempt = async {
            gossip::send_message(&mut stream, &msg)
                .await
                .context("forward tunnel request to reverse connection")?;
            let mut status = [0u8; 1];
            stream
                .read_exact(&mut status)
                .await
                .context("read tunnel status from reverse connection")?;
            anyhow::Ok(status)
        };
        match tokio::time::timeout(STATUS_TIMEOUT, attempt).await {
            Ok(Ok(status)) => {
                ready = Some((stream, status));
                break;
            }
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => last_err = Some(anyhow::anyhow!("no answer on reverse connection")),
        }
        debug!("reverse connection for node {node_id} unusable, trying another");
    }
    let Some((mut reverse_stream, status)) = ready else {
        let msg = match last_err {
            Some(e) => format!("no usable reverse connection for node {node_id}: {e:#}"),
            None => format!("no idle reverse connections for node {node_id}"),
        };
        crate::node::tunnel::send_tunnel_error(&mut subscriber, &msg).await;
        bail!("{msg}");
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::{mesh_pair, tls_state};

    fn now_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
    }

    #[tokio::test]
    async fn only_a_node_itself_can_register_its_reverse_connections() {
        let dir = std::env::temp_dir().join(format!("stouter_relay_{}_{}", std::process::id(), rand::random::<u64>()));
        let relay = tls_state(&dir, "relay");
        let mallory = tls_state(&dir, "mallory");

        let (_client, server_end) = mesh_pair(&mallory, &relay).await;
        let err = handle_reverse_registration(server_end, "victim".into(), now_ms(), relay.clone()).await.unwrap_err();
        assert!(err.to_string().contains("certificate for mallory"), "{err}");
        assert_eq!(relay.reverse_pool.count("victim"), 0);
        assert!(!relay.known_nodes.load().contains_key("victim"));

        let (_client, server_end) = mesh_pair(&mallory, &relay).await;
        let reached_at = server_end.local_addr().unwrap().to_string();
        handle_reverse_registration(server_end, "mallory".into(), now_ms(), relay.clone()).await.unwrap();
        assert_eq!(relay.reverse_pool.count("mallory"), 1);
        // The relay binds 0.0.0.0 with no advertiseAddr: it records where
        // it was actually reached, not the unroutable wildcard.
        assert!(gossip::is_wildcard(relay.config.peer_addr()));
        assert_eq!(relay.known_nodes.load()["mallory"].relay.as_deref(), Some(reached_at.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
