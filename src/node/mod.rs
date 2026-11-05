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
pub async fn run_node(state: Arc<SharedState>) -> Result<()> {
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

/// Most TLS handshakes in progress at once, in all and from one IP address.
/// Until its handshake completes a connection is anyone's, and each may take
/// the handshake timeout (10s): uncapped, a host with no certificate could
/// hold every file descriptor and stop the node accepting anything. Per IP
/// generously, since many NAT'd nodes may share one address, each opening
/// its reverse connections at once.
const MAX_HANDSHAKES: usize = 512;
const MAX_HANDSHAKES_PER_IP: usize = 128;

/// Accept mesh (mTLS) connections on `listener` until shutdown, handing each
/// to gossip dispatch once its handshake completes. Accept errors (such as
/// running out of file descriptors) are waited out, never fatal; handshakes
/// beyond [`MAX_HANDSHAKES`] (or [`MAX_HANDSHAKES_PER_IP`] from one address)
/// are refused by closing the connection.
pub(crate) async fn serve_mesh(listener: TcpListener, state: Arc<SharedState>, what: &'static str) {
    let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES));
    let per_ip: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::default();
    let mut backoff = crate::io::AcceptBackoff::default();
    let mut refused: u64 = 0;

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
        let Some(slot) = HandshakeSlot::take(&handshakes, &per_ip, peer_addr.ip()) else {
            refused += 1;
            if refused.is_power_of_two() {
                warn!("{what}: too many TLS handshakes in progress; refused {refused} connection(s), latest from {peer_addr}");
            }
            continue;
        };
        // Keepalive, so a peer that vanished without closing is noticed.
        crate::io::configure_stream(&stream);
        debug!("accepted connection from {peer_addr}");

        let state = state.clone();
        let peer = peer_addr.to_string();
        // Not tracked as in-flight here: peers hold idle connections open
        // (warm pool, mux sessions). `dispatch_connection` tracks the work.
        tokio::spawn(async move {
            let accepted = state.tls_accept(stream).await;
            drop(slot);
            match accepted {
                Ok(mesh) => gossip::dispatch_connection(mesh, peer, state).await,
                Err(e) => debug!("TLS accept from {peer}: {e}"),
            }
        });
    }
    info!("{what} shutting down");
}

/// A TLS handshake in progress, counted against [`MAX_HANDSHAKES`] and its
/// address's [`MAX_HANDSHAKES_PER_IP`] until dropped.
struct HandshakeSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl HandshakeSlot {
    fn take(all: &Arc<tokio::sync::Semaphore>, per_ip: &Arc<Mutex<HashMap<IpAddr, usize>>>, ip: IpAddr) -> Option<Self> {
        let permit = all.clone().try_acquire_owned().ok()?;
        let mut counts = per_ip.lock().unwrap();
        let count = counts.entry(ip).or_default();
        if *count >= MAX_HANDSHAKES_PER_IP {
            return None;
        }
        *count += 1;
        Some(Self { _permit: permit, per_ip: per_ip.clone(), ip })
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        let mut counts = self.per_ip.lock().unwrap();
        if let Some(count) = counts.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshakes_in_progress_are_capped_in_all_and_per_address() {
        let all = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES));
        let per_ip: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::default();
        let flood: IpAddr = "10.0.0.66".parse().unwrap();
        let held: Vec<HandshakeSlot> =
            (0..MAX_HANDSHAKES_PER_IP).map(|_| HandshakeSlot::take(&all, &per_ip, flood).unwrap()).collect();
        assert!(HandshakeSlot::take(&all, &per_ip, flood).is_none(), "one address can't take more");
        let other: IpAddr = "10.0.0.7".parse().unwrap();
        assert!(HandshakeSlot::take(&all, &per_ip, other).is_some(), "others still get in");
        drop(held);
        assert!(per_ip.lock().unwrap().is_empty(), "finished handshakes free their slots");

        let many: Vec<HandshakeSlot> = (0..MAX_HANDSHAKES)
            .map(|i| HandshakeSlot::take(&all, &per_ip, IpAddr::from([10, 1, (i / 256) as u8, i as u8])).unwrap())
            .collect();
        assert!(HandshakeSlot::take(&all, &per_ip, other).is_none(), "nor beyond the overall cap");
        drop(many);
        assert!(HandshakeSlot::take(&all, &per_ip, other).is_some());
    }
}
