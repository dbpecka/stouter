use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::net::TcpStream;
use tokio::time;
use tracing::debug;

use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::SharedState;

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
            .load()
            .values()
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
/// wait for tunnel dispatch → spawn tunnel handler → immediately reconnect.
///
/// The tunnel handler runs in a separate task so the worker can replenish
/// the pool without waiting for the in-flight tunnel to complete. This
/// prevents burst traffic from draining the pool for the duration of every
/// active connection.
async fn reverse_worker(state: Arc<SharedState>, relay_addr: String, slot: usize) {
    loop {
        match open_reverse_connection(&state, &relay_addr).await {
            Ok(()) => {
                debug!("reverse connection to {relay_addr} slot {slot} completed (tunnel handled)");
            }
            Err(e) => {
                debug!("reverse connection to {relay_addr} slot {slot}: {e}");
                time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Open a single reverse connection to a relay peer and wait for a tunnel
/// dispatch.
///
/// Sends a [`Message::ReverseRegistration`] to register, then waits for the
/// relay to forward a [`Message::TunnelRequest`]. The tunnel is handled in
/// a spawned task so the caller can immediately open a replacement connection,
/// keeping the pool full during bursts.
async fn open_reverse_connection(state: &Arc<SharedState>, relay_addr: &str) -> Result<()> {
    let mut stream = TcpStream::connect(relay_addr)
        .await
        .with_context(|| format!("connect to relay {relay_addr}"))?;

    crate::io::configure_stream(&stream);

    // Send registration.
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let msg = Message::ReverseRegistration {
        node_id: state.config.node_id.clone(),
        timestamp_ms,
    };
    gossip::send_message(&mut stream, &msg, &state.config.cluster_secret).await?;

    debug!("reverse connection established to {relay_addr}");

    // Wait for a tunnel dispatch from the relay.
    let msg = gossip::recv_message(&mut stream, &state.config.cluster_secret).await?;
    match msg {
        Message::TunnelRequest {
            node_id,
            service_name,
            timestamp_ms,
        } => {
            // Spawn tunnel handling in a background task so this worker can
            // immediately reconnect and replenish the pool.
            let state = Arc::clone(state);
            tokio::spawn(async move {
                if let Err(e) =
                    super::tunnel::handle_tunnel(stream, node_id, service_name, timestamp_ms, state)
                        .await
                {
                    debug!("reverse tunnel error: {e}");
                }
            });
            Ok(())
        }
        other => bail!("unexpected message on reverse connection: {other:?}"),
    }
}
