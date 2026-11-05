use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::{Backoff, SharedState};

/// How many relays a NAT'd node registers reverse connections with. Every
/// tunnel to it goes through one of them (or a reverse connection it holds
/// to the subscriber), so a few give redundancy; registering with every
/// node would make each node's idle connections grow with the cluster.
pub const RELAY_COUNT: usize = 3;

/// Background task for NAT'd nodes: maintains a pool of idle reverse
/// connections to each of up to [`RELAY_COUNT`] reachable peers, its relays
/// (see [`choose_relays`]).
///
/// Spawns `pool_size` persistent worker tasks per relay. Each worker loops:
/// open connection → wait for tunnel dispatch → handle tunnel → reconnect.
/// A relay's workers stop when it is no longer one (it left, stopped
/// answering, or moved to another address), so they don't keep dialling a
/// dead address forever.
pub async fn run_reverse_pool_loop(state: Arc<SharedState>) {
    let pool_size = state.config.pool_size();
    // Relay id -> its address and the token stopping its workers.
    let mut relays: HashMap<String, (String, CancellationToken)> = HashMap::new();
    let mut interval = time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }

        let known = state.known_nodes.load();
        let now = std::time::Instant::now();
        let candidates = relay_candidates(&known, &state.config.node_id, now);
        let current: Vec<(&str, &str)> = relays.iter().map(|(id, (addr, _))| (id.as_str(), addr.as_str())).collect();
        let chosen: Vec<(String, String)> = choose_relays(&state.config.node_id, &candidates, &current, RELAY_COUNT)
            .into_iter()
            .map(|(id, addr)| (id.to_string(), addr.to_string()))
            .collect();

        relays.retain(|id, (addr, workers)| {
            let keep = chosen.iter().any(|(c_id, c_addr)| c_id == id && c_addr == addr);
            if !keep {
                info!("stopping reverse connections to relay {id} at {addr}: no longer reachable there");
                workers.cancel();
            }
            keep
        });

        for (id, addr) in chosen {
            if relays.contains_key(&id) {
                continue;
            }
            info!("registering reverse connections with relay {id} at {addr}");
            let workers = state.shutdown.child_token();
            for slot in 0..pool_size {
                let (state, addr, stop) = (state.clone(), addr.clone(), workers.clone());
                tokio::spawn(async move { reverse_worker(state, addr, slot, stop).await });
            }
            relays.insert(id, (addr, workers));
        }
    }
}

/// The nodes (id, address) a NAT'd node `own_id` may pick as relays: live
/// ones it can reach. Not subscribers, while there are other nodes: a
/// subscriber is often a client machine, and as a relay it would carry other
/// subscribers' traffic. (A cluster of only subscribers and NAT'd nodes still
/// works.)
fn relay_candidates<'a>(
    known: &'a HashMap<String, crate::config::NodeInfo>,
    own_id: &str,
    now: std::time::Instant,
) -> Vec<(&'a str, &'a str)> {
    let reachable: Vec<&crate::config::NodeInfo> = known
        .values()
        .filter(|n| n.relay.is_none() && !n.addr.is_empty() && n.id != own_id)
        .filter(|n| crate::state::node_is_live(n, now))
        .collect();
    let prefer_nodes = reachable.iter().any(|n| !n.subscriber);
    reachable
        .into_iter()
        .filter(|n| !(prefer_nodes && n.subscriber))
        .map(|n| (n.id.as_str(), n.addr.as_str()))
        .collect()
}

/// The relays (id, address) a NAT'd node `nat_id` should use, out of the
/// live reachable `candidates`: the `current` ones still among them, so the
/// set (which goes into its signed record, and which subscribers hold mux
/// sessions to) doesn't churn as nodes come and go; then, to fill up to
/// `count`, the candidates ranking highest for `nat_id` by rendezvous
/// hashing, which spreads NAT'd nodes evenly over the cluster.
fn choose_relays<'a>(
    nat_id: &str,
    candidates: &[(&'a str, &'a str)],
    current: &[(&str, &str)],
    count: usize,
) -> Vec<(&'a str, &'a str)> {
    let mut chosen: Vec<(&str, &str)> =
        candidates.iter().copied().filter(|c| current.contains(c)).collect();
    chosen.sort_by_key(|(id, _)| std::cmp::Reverse(rendezvous_score(nat_id, id)));
    chosen.truncate(count);
    let mut rest: Vec<(&str, &str)> = candidates.iter().copied().filter(|c| !chosen.contains(c)).collect();
    rest.sort_by_key(|(id, _)| std::cmp::Reverse(rendezvous_score(nat_id, id)));
    let missing = count.saturating_sub(chosen.len());
    chosen.extend(rest.into_iter().take(missing));
    chosen
}

/// How strongly node `chooser` prefers relay `relay_id`: a hash of the
/// pair, the same on every node and across restarts. A NAT'd node picks its
/// relays by it, and a subscriber which of a NAT'd node's relays to use.
pub(crate) fn rendezvous_score(chooser: &str, relay_id: &str) -> u64 {
    let bytes = chooser.bytes().chain([0]).chain(relay_id.bytes());
    let mut h = bytes.fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    // FNV-1a barely mixes its last bytes: finish with an avalanche
    // (MurmurHash3's), so ids differing only at the end score unrelated.
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
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
    // Shutting down (perhaps since the connect began): a registration now
    // would reach the relay after our leave message.
    if stop.is_cancelled() {
        return Ok(());
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(chosen: &[(&str, &str)]) -> Vec<String> {
        let mut ids: Vec<String> = chosen.iter().map(|(id, _)| id.to_string()).collect();
        ids.sort();
        ids
    }

    #[test]
    fn a_natd_node_picks_a_few_relays_and_keeps_them() {
        let names: Vec<String> = (0..10).map(|i| format!("node-{i}")).collect();
        let addrs: Vec<String> = (0..10).map(|i| format!("10.0.0.{i}:7000")).collect();
        let all: Vec<(&str, &str)> = names.iter().map(String::as_str).zip(addrs.iter().map(String::as_str)).collect();

        let first = choose_relays("nat-a", &all, &[], RELAY_COUNT);
        assert_eq!(first.len(), RELAY_COUNT);
        // The same pick whatever order the candidates come in.
        let mut reversed = all.clone();
        reversed.reverse();
        assert_eq!(ids(&choose_relays("nat-a", &reversed, &[], RELAY_COUNT)), ids(&first));

        // A node joining that would rank higher doesn't displace them.
        let mut more = all.clone();
        let extra: Vec<(String, String)> =
            (10..40).map(|i| (format!("node-{i}"), format!("10.0.0.{i}:7000"))).collect();
        more.extend(extra.iter().map(|(id, addr)| (id.as_str(), addr.as_str())));
        assert_eq!(ids(&choose_relays("nat-a", &more, &first, RELAY_COUNT)), ids(&first));

        // One that stops being live is replaced; the others stay.
        let lost = first[0];
        let without: Vec<(&str, &str)> = all.iter().copied().filter(|c| *c != lost).collect();
        let next = choose_relays("nat-a", &without, &first, RELAY_COUNT);
        assert_eq!(next.len(), RELAY_COUNT);
        assert!(!next.contains(&lost));
        assert!(first[1..].iter().all(|r| next.contains(r)));

        // One that moved is a different relay: it's picked afresh.
        let moved: Vec<(&str, &str)> =
            all.iter().map(|&(id, addr)| if id == lost.0 { (id, "10.9.9.9:7000") } else { (id, addr) }).collect();
        assert!(!choose_relays("nat-a", &moved, &first, RELAY_COUNT).contains(&lost));

        // Fewer candidates than wanted: all of them.
        assert_eq!(choose_relays("nat-a", &all[..2], &[], RELAY_COUNT).len(), 2);
    }

    #[test]
    fn subscribers_are_relays_only_when_there_is_nothing_else() {
        let now = std::time::Instant::now();
        let node = |id: &str, subscriber: bool| crate::config::NodeInfo {
            id: id.into(),
            addr: format!("{id}:1"),
            last_seen: Some(now),
            subscriber,
            ..Default::default()
        };
        let mut known: HashMap<String, crate::config::NodeInfo> =
            [node("n1", false), node("n2", false), node("laptop", true)].into_iter().map(|n| (n.id.clone(), n)).collect();
        let mut ids: Vec<&str> = relay_candidates(&known, "nat", now).into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, ["n1", "n2"]);
        // Only subscribers left: better one of them than no relay at all.
        known.retain(|id, _| id == "laptop");
        assert_eq!(relay_candidates(&known, "nat", now), [("laptop", "laptop:1")]);
    }

    #[test]
    fn natd_nodes_spread_over_the_relays() {
        let names: Vec<String> = (0..10).map(|i| format!("node-{i}.example.com")).collect();
        let all: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "addr")).collect();
        let mut load: HashMap<&str, usize> = HashMap::new();
        for nat in 0..200 {
            for (id, _) in choose_relays(&format!("nat-{nat}.example.com"), &all, &[], RELAY_COUNT) {
                *load.entry(id).or_default() += 1;
            }
        }
        // 600 registrations over 10 relays: about 60 each.
        assert_eq!(load.len(), 10, "every relay gets some: {load:?}");
        assert!(load.values().all(|&n| (30..=90).contains(&n)), "{load:?}");
    }
}
