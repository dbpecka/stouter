use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::{Backoff, SharedState};

/// Background task for NAT'd nodes: maintains a pool of idle reverse
/// connections to all known reachable peers.
///
/// Spawns `pool_size` persistent worker tasks per peer. Each worker loops:
/// open connection → wait for tunnel dispatch → handle tunnel → reconnect.
/// A peer's workers stop when it is no longer a known peer (it left, was
/// forgotten, or moved to another address), so they don't keep dialling a
/// dead address forever.
pub async fn run_reverse_pool_loop(state: Arc<SharedState>) {
    let pool_size = state.config.pool_size();
    // Workers per relay address; cancelling the token stops them.
    let mut relays: HashMap<String, CancellationToken> = HashMap::new();
    let mut interval = time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }

        let peers: HashSet<String> = state
            .known_nodes
            .load()
            .values()
            .filter(|n| n.relay.is_none() && !n.addr.is_empty())
            .map(|n| n.addr.clone())
            .collect();

        relays.retain(|addr, workers| {
            let keep = peers.contains(addr);
            if !keep {
                info!("stopping reverse connections to {addr}: no longer a known peer");
                workers.cancel();
            }
            keep
        });

        for peer_addr in peers {
            if relays.contains_key(&peer_addr) {
                continue;
            }
            let workers = state.shutdown.child_token();
            for slot in 0..pool_size {
                let (state, addr, stop) = (state.clone(), peer_addr.clone(), workers.clone());
                tokio::spawn(async move { reverse_worker(state, addr, slot, stop).await });
            }
            relays.insert(peer_addr, workers);
        }
    }
}

/// A single reverse connection worker. Loops until `stop` (shutdown, or the
/// relay going away): connect → register → wait for tunnel dispatch → spawn
/// tunnel handler → immediately reconnect. Failures back off exponentially
/// (with jitter, so a pool's workers don't retry in lockstep).
///
/// The tunnel handler runs in a separate task so the worker can replenish
/// the pool without waiting for the in-flight tunnel to complete. This
/// prevents burst traffic from draining the pool for the duration of every
/// active connection.
async fn reverse_worker(state: Arc<SharedState>, relay_addr: String, slot: usize, stop: CancellationToken) {
    let mut backoff = Backoff::new(Duration::from_secs(2), Duration::from_secs(60));
    while !stop.is_cancelled() {
        match open_reverse_connection(&state, &relay_addr, &stop).await {
            Ok(()) => {
                backoff.succeeded(&relay_addr);
                debug!("reverse connection to {relay_addr} slot {slot} completed (tunnel handled)");
            }
            Err(e) => {
                let wait = backoff.failed(&relay_addr, time::Instant::now());
                debug!("reverse connection to {relay_addr} slot {slot}: {e:#}; retrying in {}s", wait.as_secs());
                tokio::select! {
                    _ = time::sleep(wait) => {}
                    _ = stop.cancelled() => {}
                }
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
async fn open_reverse_connection(state: &Arc<SharedState>, relay_addr: &str, stop: &CancellationToken) -> Result<()> {
    let mut stream = state.connect_mesh(relay_addr, state.node_at(relay_addr).as_deref()).await?;

    // Send registration.
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let msg = Message::ReverseRegistration {
        node_id: state.config.node_id.clone(),
        timestamp_ms,
    };
    gossip::send_message(&mut stream, &msg).await?;

    // While this connection waits at the relay, the relay is one we use: it
    // goes into our signed node record, the only list of relays peers will
    // send our tunnels through. Who the relay is comes from its certificate.
    let _registration = stream.peer_cn().map(|relay| state.relay_registered(relay));

    debug!("reverse connection established to {relay_addr}");

    // Wait for a tunnel dispatch from the relay. Idle until then, so at
    // shutdown the connection is simply closed.
    let msg = tokio::select! {
        msg = gossip::recv_message(&mut stream) => msg?,
        _ = stop.cancelled() => return Ok(()),
    };
    match msg {
        Message::TunnelRequest {
            node_id,
            service_name,
            timestamp_ms,
        } => {
            // Spawn tunnel handling in a background task so this worker can
            // immediately reconnect and replenish the pool.
            let state = Arc::clone(state);
            let in_flight = state.in_flight.track();
            tokio::spawn(async move {
                let _in_flight = in_flight;
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
