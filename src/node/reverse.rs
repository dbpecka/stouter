use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time;
use tracing::debug;

use crate::gossip::CONN_REVERSE;
use crate::state::SharedState;

/// Registration request sent to a relay node.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReverseRegistration {
    node_id: String,
    timestamp_ms: u64,
    token: String,
}

/// Background task for NAT'd nodes: maintains a pool of idle reverse
/// connections to all known reachable peers.
///
/// Spawns `pool_size` persistent worker tasks per peer. Each worker loops:
/// open connection → wait for tunnel dispatch → handle tunnel → reconnect.
pub async fn run_reverse_pool_loop(state: Arc<SharedState>) {
    let pool_size = state.config.pool_size();
    let mut spawned: HashSet<(String, usize)> = HashSet::new();
    let mut interval = time::interval(Duration::from_secs(5));

    loop {
        interval.tick().await;

        let peers: Vec<String> = state
            .known_nodes
            .read()
            .await
            .iter()
            .filter(|n| n.relay.is_none() && !n.addr.is_empty())
            .map(|n| n.addr.clone())
            .collect();

        for peer_addr in &peers {
            for slot in 0..pool_size {
                let key = (peer_addr.clone(), slot);
                if spawned.contains(&key) {
                    continue;
                }
                spawned.insert(key);

                let state = state.clone();
                let addr = peer_addr.clone();
                tokio::spawn(async move {
                    reverse_worker(state, addr, slot).await;
                });
            }
        }
    }
}

/// A single reverse connection worker. Loops forever: connect → register →
/// wait for tunnel → handle → reconnect after a brief delay.
async fn reverse_worker(state: Arc<SharedState>, relay_addr: String, slot: usize) {
    loop {
        match open_reverse_connection(&state, &relay_addr).await {
            Ok(()) => {
                debug!("reverse connection to {relay_addr} slot {slot} completed (tunnel handled)");
            }
            Err(e) => {
                debug!("reverse connection to {relay_addr} slot {slot}: {e}");
            }
        }
        // Brief delay before reconnecting to avoid tight loops on failure.
        time::sleep(Duration::from_secs(2)).await;
    }
}

/// Open a single reverse connection to a relay peer and wait for a tunnel
/// dispatch.
///
/// The connection sits idle until the relay sends a tunnel request frame,
/// at which point we handle it like a normal inbound tunnel.
async fn open_reverse_connection(state: &Arc<SharedState>, relay_addr: &str) -> Result<()> {
    let mut stream = TcpStream::connect(relay_addr)
        .await
        .with_context(|| format!("connect to relay {relay_addr}"))?;

    // Send connection type byte.
    stream
        .write_all(&[CONN_REVERSE])
        .await
        .context("write CONN_REVERSE byte")?;

    // Send registration.
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let node_id = state.config.node_id.clone();
    let token_data = format!("reverse:{node_id}:{timestamp_ms}");
    let token = crate::crypto::sign(&state.config.cluster_secret, &token_data);

    let reg = ReverseRegistration {
        node_id,
        timestamp_ms,
        token,
    };

    let reg_bytes = serde_json::to_vec(&reg).context("serialize ReverseRegistration")?;
    let len = reg_bytes.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .context("write registration length")?;
    stream
        .write_all(&reg_bytes)
        .await
        .context("write registration body")?;

    debug!("reverse connection established to {relay_addr}");

    // Wait for a tunnel dispatch from the relay.
    // The relay will forward a raw tunnel request frame (4-byte length + body).
    // We handle it using the same tunnel logic as a direct connection.
    super::tunnel::handle_tunnel(stream, Arc::clone(state)).await
}
