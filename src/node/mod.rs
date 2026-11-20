pub mod relay;
pub mod reverse;
pub mod tunnel;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use crate::gossip;
use crate::state::SharedState;

/// Run the node daemon.
///
/// If `outbound_only` is set, the node skips binding a listener and instead
/// maintains reverse connections to reachable peers for tunnel relay.
/// Otherwise, binds to `state.config.bind` and accepts incoming connections.
pub async fn run_node(state: Arc<SharedState>, metrics: metrics_exporter_prometheus::PrometheusHandle) -> Result<()> {
    tokio::spawn(crate::metrics::serve_metrics(state.config.api_port(), metrics, state.clone()));
    tokio::spawn(gossip::run_sync_loop(state.clone()));
    tokio::spawn(gossip::run_member_log(state.clone()));
    tokio::spawn(gossip::run_config_poll_loop(state.clone()));
    tokio::spawn(crate::health::run_health_checks(state.clone()));

    if state.config.outbound_only {
        info!(
            "Node running in outbound-only mode (NAT'd), node_id={}",
            state.config.node_id
        );
        reverse::run_reverse_pool_loop(state).await;
        Ok(())
    } else {
        run_listener(state).await
    }
}

/// Bind the TCP listener and accept connections until shutdown.
async fn run_listener(state: Arc<SharedState>) -> Result<()> {
    let listener = TcpListener::bind(&state.config.bind)
        .await
        .with_context(|| format!("bind to {}", state.config.bind))?;

    info!("Node listening on {}", state.config.bind);
    serve_mesh(listener, state, "listener").await;
    Ok(())
}

/// Most TLS handshakes in progress at once, in all and from one address
/// group (an IPv4 address, or an IPv6 /64, which one host often holds).
/// Until its handshake completes a connection is anyone's, and each may take
/// the handshake timeout (10s): uncapped, a host with no certificate could
/// hold every file descriptor and stop the node accepting anything. Per
/// group generously, since many NAT'd nodes may share one address, each
/// opening its reverse connections at once.
const MAX_HANDSHAKES: usize = 512;
const MAX_HANDSHAKES_PER_IP: usize = 128;

/// Accept mesh (mTLS) connections on `listener` until shutdown, handing each
/// to gossip dispatch once its handshake completes. Accept errors (such as
/// running out of file descriptors) are waited out, never fatal. Handshakes
/// are capped (see [`Handshakes`]).
pub(crate) async fn serve_mesh(listener: TcpListener, state: Arc<SharedState>, what: &'static str) {
    let handshakes = Arc::new(Handshakes::default());
    let mut backoff = crate::io::AcceptBackoff::default();
    let (mut refused, mut evicted): (u64, u64) = (0, 0);

    loop {
        let (stream, peer_addr) = tokio::select! {
            result = listener.accept() => match result {
                Ok(accepted) => {
                    backoff.succeeded(what);
                    accepted
                }
                Err(e) => {
                    if backoff.failed(what, &e, &state.shutdown).await {
                        continue;
                    }
                    break;
                }
            },
            _ = state.shutdown.cancelled() => break,
        };
        let slot = match handshakes.take(peer_addr.ip()) {
            Some((slot, evicting)) => {
                if evicting {
                    evicted += 1;
                    if evicted.is_power_of_two() {
                        warn!("{what}: too many TLS handshakes in progress; dropped {evicted} stalled one(s) for newer connections");
                    }
                }
                slot
            }
            None => {
                refused += 1;
                if refused.is_power_of_two() {
                    warn!("{what}: too many TLS handshakes in progress from one address; refused {refused} connection(s), latest from {peer_addr}");
                }
                continue;
            }
        };
        // Keepalive, so a peer that vanished without closing is noticed.
        crate::io::configure_stream(&stream);
        debug!("accepted connection from {peer_addr}");

        let state = state.clone();
        let peer = peer_addr.to_string();
        // Not tracked as in-flight here: peers hold idle connections open
        // (warm pool, mux sessions). `dispatch_connection` tracks the work.
        tokio::spawn(async move {
            let accepted = tokio::select! {
                accepted = state.tls_accept(stream) => accepted,
                () = slot.evicted() => Err(anyhow::anyhow!("dropped for a newer connection (too many handshakes in progress)")),
            };
            drop(slot);
            match accepted {
                Ok(mesh) => gossip::dispatch_connection(mesh, peer, state).await,
                Err(e) => debug!("TLS accept from {peer}: {e}"),
            }
        });
    }
    info!("{what} shutting down");
}

/// The address group a connection counts against: an IPv4 address (an
/// IPv4-mapped IPv6 one as such), or an IPv6 /64.
fn address_group(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(std::net::Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
        },
    }
}

/// TLS handshakes in progress, by address group, oldest first.
///
/// A group at [`MAX_HANDSHAKES_PER_IP`] gets no more. With [`MAX_HANDSHAKES`]
/// in progress, a new connection is still let in: the oldest handshake of
/// the group with the most is dropped for it. Hosts that open connections
/// and send nothing (each held for the handshake timeout) then only displace
/// one another, while a peer's handshake, done in milliseconds from a group
/// with few in progress, gets through. Refusing new connections instead
/// would let a few such hosts lock every peer out.
#[derive(Default)]
struct Handshakes {
    inner: Mutex<HandshakesInner>,
}

#[derive(Default)]
struct HandshakesInner {
    next_id: u64,
    total: usize,
    by_group: HashMap<IpAddr, std::collections::VecDeque<(u64, tokio_util::sync::CancellationToken)>>,
}

impl Handshakes {
    /// A slot for a handshake from `ip`, and whether another was dropped for
    /// it; `None` if its group already has its most.
    fn take(self: &Arc<Self>, ip: IpAddr) -> Option<(HandshakeSlot, bool)> {
        let group = address_group(ip);
        let mut inner = self.inner.lock().unwrap();
        if inner.by_group.get(&group).is_some_and(|held| held.len() >= MAX_HANDSHAKES_PER_IP) {
            return None;
        }
        let mut evicting = false;
        if inner.total >= MAX_HANDSHAKES {
            // The group with the most in progress; of equals, the one holding
            // the oldest.
            let victim = inner
                .by_group
                .iter()
                .max_by_key(|(_, held)| (held.len(), std::cmp::Reverse(held.front().map_or(u64::MAX, |(id, _)| *id))))
                .map(|(group, _)| *group);
            if let Some(victim) = victim {
                let held = inner.by_group.get_mut(&victim).unwrap();
                if let Some((_, cancel)) = held.pop_front() {
                    cancel.cancel();
                    inner.total -= 1;
                    evicting = true;
                }
                if inner.by_group.get(&victim).is_some_and(|held| held.is_empty()) {
                    inner.by_group.remove(&victim);
                }
            }
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let cancel = tokio_util::sync::CancellationToken::new();
        inner.by_group.entry(group).or_default().push_back((id, cancel.clone()));
        inner.total += 1;
        Some((HandshakeSlot { handshakes: self.clone(), group, id, cancel }, evicting))
    }
}

/// A TLS handshake in progress, counted (see [`Handshakes`]) until dropped.
struct HandshakeSlot {
    handshakes: Arc<Handshakes>,
    group: IpAddr,
    id: u64,
    cancel: tokio_util::sync::CancellationToken,
}

impl HandshakeSlot {
    /// Resolves when this handshake is dropped for a newer connection.
    async fn evicted(&self) {
        self.cancel.cancelled().await
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        let mut inner = self.handshakes.inner.lock().unwrap();
        let Some(held) = inner.by_group.get_mut(&self.group) else { return };
        // Not there if it was evicted: its count went with it.
        if let Some(at) = held.iter().position(|(id, _)| *id == self.id) {
            held.remove(at);
            let empty = held.is_empty();
            inner.total -= 1;
            if empty {
                inner.by_group.remove(&self.group);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshakes_in_progress_are_capped_per_address_group() {
        let handshakes = Arc::new(Handshakes::default());
        let flood: IpAddr = "10.0.0.66".parse().unwrap();
        let held: Vec<_> = (0..MAX_HANDSHAKES_PER_IP).map(|_| handshakes.take(flood).unwrap().0).collect();
        assert!(handshakes.take(flood).is_none(), "one address can't take more");
        let other: IpAddr = "10.0.0.7".parse().unwrap();
        assert!(handshakes.take(other).is_some(), "others still get in");
        drop(held);
        let inner = handshakes.inner.lock().unwrap();
        assert!(inner.by_group.is_empty() && inner.total == 0, "finished handshakes free their slots");
        drop(inner);

        // One host's /64 is one group; IPv4-mapped addresses count as IPv4.
        let v6 = |i: u16| IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, i));
        let held: Vec<_> = (0..MAX_HANDSHAKES_PER_IP as u16).map(|i| handshakes.take(v6(i)).unwrap().0).collect();
        assert!(handshakes.take(v6(9999)).is_none());
        assert!(handshakes.take(IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 2, 0, 0, 0, 1))).is_some());
        assert_eq!(address_group("::ffff:10.0.0.66".parse().unwrap()), flood);
        drop(held);
    }

    #[test]
    fn stalled_handshakes_cant_lock_peers_out() {
        let handshakes = Arc::new(Handshakes::default());
        // Four hosts fill every slot with connections that send nothing.
        let flood = |i: usize| IpAddr::from([10, 0, 0, 60 + (i % 4) as u8]);
        let mut stalled: Vec<HandshakeSlot> = (0..MAX_HANDSHAKES).map(|i| handshakes.take(flood(i)).unwrap().0).collect();

        // A peer still gets in, and one of theirs is dropped for it.
        let (peer, evicting) = handshakes.take("10.0.1.1".parse().unwrap()).unwrap();
        assert!(evicting);
        assert_eq!(stalled.iter().filter(|s| s.cancel.is_cancelled()).count(), 1);
        // However fast they reopen, they only displace their own.
        for i in 0..10_000 {
            stalled.retain(|s| !s.cancel.is_cancelled());
            if let Some((slot, _)) = handshakes.take(flood(i)) {
                stalled.push(slot);
            }
        }
        assert!(!peer.cancel.is_cancelled(), "the peer's handshake is never the one dropped");
        assert_eq!(handshakes.inner.lock().unwrap().total, MAX_HANDSHAKES);
        drop(peer);
        drop(stalled);
        assert_eq!(handshakes.inner.lock().unwrap().total, 0);
    }
}
