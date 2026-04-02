pub mod messages;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time;
use tracing::{debug, error, info};

use crate::config::NodeInfo;
use crate::state::SharedState;
use messages::{GossipMessage, SignedMessage};

/// Connection-type byte sent as the first octet of every TCP connection.
pub const CONN_GOSSIP: u8 = b'G';
/// Connection-type byte for tunnel connections.
pub const CONN_TUNNEL: u8 = b'T';
/// Connection-type byte for status queries.
pub const CONN_STATUS: u8 = b'S';
/// Connection-type byte for reverse connections from NAT'd nodes.
pub const CONN_REVERSE: u8 = b'R';

/// Maximum allowed gossip message size: 1 MiB.
const MAX_MSG_BYTES: u32 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// Serialize `msg`, sign it with `secret`, and write it to `stream` using a
/// 4-byte big-endian length prefix.
pub async fn send_message(
    stream: &mut TcpStream,
    msg: &GossipMessage,
    secret: &str,
) -> Result<()> {
    let payload = serde_json::to_string(msg).context("serialize GossipMessage")?;
    let signature = crate::crypto::sign(secret, &payload);
    let signed = SignedMessage { payload, signature };
    let frame = serde_json::to_vec(&signed).context("serialize SignedMessage")?;

    let len = u32::try_from(frame.len()).context("message too large")?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .context("write length prefix")?;
    stream.write_all(&frame).await.context("write frame")?;
    Ok(())
}

/// Read a length-prefixed gossip message from `stream`, verify its HMAC
/// signature, and return the inner [`GossipMessage`].
pub async fn recv_message(stream: &mut TcpStream, secret: &str) -> Result<GossipMessage> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("read length prefix")?;
    let len = u32::from_be_bytes(len_buf);

    if len > MAX_MSG_BYTES {
        bail!("gossip message too large: {len} bytes");
    }

    let mut frame = vec![0u8; len as usize];
    stream
        .read_exact(&mut frame)
        .await
        .context("read message frame")?;

    let signed: SignedMessage =
        serde_json::from_slice(&frame).context("deserialize SignedMessage")?;

    if !crate::crypto::verify(secret, &signed.payload, &signed.signature) {
        bail!("gossip message signature verification failed");
    }

    let msg: GossipMessage =
        serde_json::from_str(&signed.payload).context("deserialize GossipMessage")?;
    Ok(msg)
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

/// Handle an inbound gossip TCP connection.
///
/// Reads the first message; if it is a [`GossipMessage::Sync`] the state is
/// merged and a Sync reply is sent back so the peer can also update itself.
/// All other message types update local state without a reply.
pub async fn handle_gossip_connection(mut stream: TcpStream, state: Arc<SharedState>) {
    let secret = state.config.cluster_secret.clone();
    match recv_message(&mut stream, &secret).await {
        Err(e) => {
            debug!("failed to receive gossip message: {e}");
        }
        Ok(GossipMessage::Sync { config, nodes }) => {
            merge_sync(&state, config, nodes).await;

            // Reply with our current state.
            let reply = build_sync_msg(&state).await;
            if let Err(e) = send_message(&mut stream, &reply, &secret).await {
                debug!("failed to send sync reply: {e}");
            }
        }
        Ok(msg) => {
            apply_message(&state, msg).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Sync loop
// ---------------------------------------------------------------------------

/// Background task: periodically read the config file and, if `dynamic_config`
/// has a higher version than what is in memory, update local state and
/// broadcast a [`GossipMessage::ConfigUpdate`] to all peers.
pub async fn run_config_poll_loop(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;

        let new_cfg = match crate::config::Config::load(&state.config_path) {
            Ok(c) => c,
            Err(e) => {
                debug!("config poll: failed to read {}: {e}", state.config_path);
                continue;
            }
        };

        let current_version = state.dynamic_config.read().await.version;
        if new_cfg.dynamic_config.version > current_version {
            info!(
                "config file changed: version {} -> {}",
                current_version, new_cfg.dynamic_config.version
            );
            {
                let mut dc = state.dynamic_config.write().await;
                *dc = new_cfg.dynamic_config.clone();
            }
            print_dynamic_config(&state).await;
            broadcast_message(
                &state,
                GossipMessage::ConfigUpdate {
                    config: new_cfg.dynamic_config,
                },
            )
            .await;
        }
    }
}

/// Collect all unique peer addresses to contact: bootstrap peers + known member addrs.
async fn peer_addrs(state: &Arc<SharedState>) -> Vec<String> {
    let mut addrs: Vec<String> = state.bootstrap_peers.clone();
    let members = state.known_nodes.read().await;
    for n in members.iter() {
        if !n.addr.is_empty() && !addrs.contains(&n.addr) {
            addrs.push(n.addr.clone());
        }
    }
    addrs
}

/// Background task: periodically push a Sync message to every known peer and
/// merge their reply.
pub async fn run_sync_loop(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;

        let peers = peer_addrs(&state).await;

        for addr in peers {
            match time::timeout(Duration::from_secs(10), sync_with_peer(&state, &addr)).await {
                Ok(Err(e)) => debug!("sync with {addr} failed: {e}"),
                Err(_) => debug!("sync with {addr} timed out"),
                Ok(Ok(())) => {}
            }
        }
    }
}

/// Attempt a single sync round-trip with `addr`.
async fn sync_with_peer(state: &Arc<SharedState>, addr: &str) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to {addr}"))?;

    stream
        .write_all(&[CONN_GOSSIP])
        .await
        .context("write connection type")?;

    let secret = &state.config.cluster_secret;
    let our_sync = build_sync_msg(state).await;
    send_message(&mut stream, &our_sync, secret).await?;

    let reply = recv_message(&mut stream, secret).await?;
    if let GossipMessage::Sync { config, nodes } = reply {
        merge_sync(state, config, nodes).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Broadcast
// ---------------------------------------------------------------------------

/// Send `msg` to every known peer node.  Connection errors are logged at
/// `debug` level because peers may be temporarily unreachable.
pub async fn broadcast_message(state: &Arc<SharedState>, msg: GossipMessage) {
    let peers = peer_addrs(state).await;

    let secret = state.config.cluster_secret.clone();
    let payload = match serde_json::to_string(&msg) {
        Ok(p) => p,
        Err(e) => {
            error!("failed to serialize broadcast message: {e}");
            return;
        }
    };

    for addr in peers {
        match time::timeout(Duration::from_secs(5), send_to_peer(&addr, &payload, &secret)).await {
            Ok(Err(e)) => debug!("broadcast to {addr} failed: {e}"),
            Err(_) => debug!("broadcast to {addr} timed out"),
            Ok(Ok(())) => {}
        }
    }
}

/// Open a connection to `addr` and send one pre-serialized gossip payload.
async fn send_to_peer(addr: &str, payload: &str, secret: &str) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to {addr}"))?;

    stream
        .write_all(&[CONN_GOSSIP])
        .await
        .context("write connection type")?;

    let msg: GossipMessage =
        serde_json::from_str(payload).context("deserialize for re-send")?;
    send_message(&mut stream, &msg, secret).await
}

// ---------------------------------------------------------------------------
// State helpers
// ---------------------------------------------------------------------------

/// Print the current dynamic config as pretty JSON to stdout.
async fn print_dynamic_config(state: &Arc<SharedState>) {
    let dc = state.dynamic_config.read().await.clone();
    match serde_json::to_string_pretty(&dc) {
        Ok(json) => println!("{json}"),
        Err(e) => error!("failed to serialize dynamic config for display: {e}"),
    }
}

/// Persist dynamic config to disk.
///
/// Loads the current config file first so that external changes to static
/// fields (e.g. `cluster_secret`, `bind`) are preserved.  The `known_nodes`
/// list in the config file is left untouched — it contains bootstrap peer
/// addresses, not the gossip-discovered member list.
async fn persist_state(state: &Arc<SharedState>) {
    let mut cfg = match crate::config::Config::load(&state.config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to load config for persist: {e}");
            return;
        }
    };
    cfg.dynamic_config = state.dynamic_config.read().await.clone();
    if let Err(e) = cfg.save(&state.config_path) {
        error!("failed to persist state to {}: {e}", state.config_path);
    }
}

/// Merge an incoming Sync payload into local state.
async fn merge_sync(state: &Arc<SharedState>, config: crate::config::DynamicConfig, nodes: Vec<NodeInfo>) {
    let updated = {
        let mut dc = state.dynamic_config.write().await;
        if config.version > dc.version {
            info!(
                "dynamic config updated: version {} -> {}",
                dc.version, config.version
            );
            *dc = config;
            true
        } else {
            false
        }
    };

    if updated {
        print_dynamic_config(state).await;
        persist_state(state).await;
    }

    let nodes_changed = {
        let mut kn = state.known_nodes.write().await;
        let mut changed = false;
        for node in nodes {
            // Skip entries with no identity — these are malformed.
            if node.id.is_empty() {
                continue;
            }
            if let Some(existing) = kn.iter_mut().find(|n| n.id == node.id) {
                if existing.addr != node.addr {
                    info!("node {} address updated: {} -> {}", node.id, existing.addr, node.addr);
                    existing.addr = node.addr;
                    changed = true;
                }
                // Update relay info if the peer has newer relay data.
                if existing.relay != node.relay {
                    info!(
                        "node {} relay updated: {:?} -> {:?}",
                        node.id, existing.relay, node.relay
                    );
                    existing.relay = node.relay;
                    changed = true;
                }
            } else {
                info!("discovered new node: {} @ {}", node.id, node.addr);
                kn.push(node);
                changed = true;
            }
        }
        changed
    };

    if nodes_changed {
        persist_state(state).await;
    }
}

/// Apply a non-Sync gossip message to local state.
async fn apply_message(state: &Arc<SharedState>, msg: GossipMessage) {
    match msg {
        GossipMessage::ConfigUpdate { config } => {
            let updated = {
                let mut dc = state.dynamic_config.write().await;
                if config.version > dc.version {
                    info!(
                        "config update received: version {} -> {}",
                        dc.version, config.version
                    );
                    *dc = config;
                    true
                } else {
                    false
                }
            };
            if updated {
                print_dynamic_config(state).await;
                persist_state(state).await;
            }
        }
        GossipMessage::NodeJoin { id, addr } => {
            let changed = {
                let mut kn = state.known_nodes.write().await;
                if let Some(existing) = kn.iter_mut().find(|n| n.id == id) {
                    if existing.addr != addr {
                        info!("node {id} address updated: {} -> {addr}", existing.addr);
                        existing.addr = addr;
                        true
                    } else {
                        false
                    }
                } else {
                    info!("node joined: {id} @ {addr}");
                    kn.push(NodeInfo { id, addr, relay: None });
                    true
                }
            };
            if changed {
                persist_state(state).await;
            }
        }
        GossipMessage::NodeLeave { id } => {
            let mut kn = state.known_nodes.write().await;
            if let Some(pos) = kn.iter().position(|n| n.id == id) {
                info!("node left: {id}");
                kn.swap_remove(pos);
            }
        }
        GossipMessage::Sync { config, nodes } => {
            merge_sync(state, config, nodes).await;
        }
    }
}

/// Periodically logs the number of known member nodes.
pub async fn run_member_log(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(60));
    interval.tick().await; // skip the immediate first tick
    loop {
        interval.tick().await;
        let nodes = state.known_nodes.read().await.clone();
        info!("{} member node(s) in cluster", nodes.len());
        for node in &nodes {
            info!("  {} @ {}", node.id, node.addr);
        }
    }
}

/// Snapshot current state as a Sync message.
///
/// Always includes a `NodeInfo` for the sender itself so receivers can
/// populate the `hostname → address` mapping that `manage_proxies` requires
/// to match services (which store `node_id = hostname`) to connectable peers.
async fn build_sync_msg(state: &Arc<SharedState>) -> GossipMessage {
    let config = state.dynamic_config.read().await.clone();
    let mut nodes = state.known_nodes.read().await.clone();

    if !state.config.node_id.is_empty() {
        let self_addr = if state.config.outbound_only {
            // NAT'd nodes have no directly reachable address.
            // The relay field will be populated by relay nodes.
            String::new()
        } else {
            state.config.peer_addr().to_owned()
        };

        if let Some(existing) = nodes.iter_mut().find(|n| n.id == state.config.node_id) {
            // Always ensure our own entry has the current address.
            existing.addr = self_addr;
        } else {
            nodes.push(NodeInfo {
                id: state.config.node_id.clone(),
                addr: self_addr,
                relay: None,
            });
        }
    }

    GossipMessage::Sync { config, nodes }
}

// ---------------------------------------------------------------------------
// Status handler
// ---------------------------------------------------------------------------

/// Response payload sent in reply to a [`CONN_STATUS`] connection.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    pub mode: String,
    pub node_id: String,
    pub bind: String,
    pub dynamic_config: crate::config::DynamicConfig,
    pub known_nodes: Vec<NodeInfo>,
}

/// Handle an inbound status TCP connection.
///
/// Serialises the daemon's current state and writes it as a 4-byte
/// big-endian length-prefixed JSON frame, then closes the connection.
pub async fn handle_status_connection(mut stream: TcpStream, state: Arc<SharedState>) {
    use tokio::io::AsyncWriteExt;

    let response = StatusResponse {
        mode: format!("{:?}", state.config.mode).to_lowercase(),
        node_id: state.config.node_id.clone(),
        bind: state.config.bind.clone(),
        dynamic_config: state.dynamic_config.read().await.clone(),
        known_nodes: state.known_nodes.read().await.clone(),
    };

    let json = match serde_json::to_vec(&response) {
        Ok(b) => b,
        Err(e) => {
            debug!("status: failed to serialize response: {e}");
            return;
        }
    };

    let len = json.len() as u32;
    let _ = stream.write_all(&len.to_be_bytes()).await;
    let _ = stream.write_all(&json).await;
}
