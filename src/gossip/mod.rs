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
use messages::Message;

/// Maximum allowed message size: 1 MiB.
const MAX_MSG_BYTES: u32 = 1024 * 1024;

/// Size of the HMAC-SHA256 signature prepended to each frame.
const HMAC_LEN: usize = 32;

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// Serialize `msg` to MessagePack, sign the payload with HMAC-SHA256, and
/// write the frame as `[4-byte payload len][32-byte HMAC][payload]`.
pub async fn send_message<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    msg: &Message,
    secret: &str,
) -> Result<()> {
    let payload = rmp_serde::to_vec(msg).context("serialize Message")?;
    let hmac = crate::crypto::sign_bytes(secret, &payload);

    let len = u32::try_from(payload.len()).context("message too large")?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .context("write length prefix")?;
    stream.write_all(&hmac).await.context("write HMAC")?;
    stream
        .write_all(&payload)
        .await
        .context("write payload")?;
    stream.flush().await.context("flush message")?;
    Ok(())
}

/// Read a binary-framed message from `stream`, verify its HMAC signature,
/// and deserialize the MessagePack payload.
///
/// Wire format: `[4-byte payload len][32-byte HMAC][payload]`.
pub async fn recv_message<R: AsyncReadExt + Unpin>(stream: &mut R, secret: &str) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("read length prefix")?;
    let len = u32::from_be_bytes(len_buf);

    if len > MAX_MSG_BYTES {
        bail!("message too large: {len} bytes");
    }

    let mut hmac = [0u8; HMAC_LEN];
    stream
        .read_exact(&mut hmac)
        .await
        .context("read HMAC")?;

    let mut payload = vec![0u8; len as usize];
    stream
        .read_exact(&mut payload)
        .await
        .context("read payload")?;

    if !crate::crypto::verify_bytes(secret, &payload, &hmac) {
        bail!("message signature verification failed");
    }

    let msg: Message = rmp_serde::from_slice(&payload).context("deserialize Message")?;
    Ok(msg)
}

// ---------------------------------------------------------------------------
// Connection dispatch
// ---------------------------------------------------------------------------

/// Handle an inbound TCP connection by reading the first message and
/// dispatching to the appropriate handler based on its variant.
pub async fn dispatch_connection(mut stream: TcpStream, state: Arc<SharedState>) {
    let peer_addr = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let secret = state.config.cluster_secret.clone();

    match recv_message(&mut stream, &secret).await {
        Err(e) => {
            debug!("failed to receive message from {peer_addr}: {e}");
        }
        Ok(msg) => match msg {
            Message::Sync { .. }
            | Message::ConfigUpdate { .. }
            | Message::NodeJoin { .. }
            | Message::NodeLeave { .. } => {
                handle_gossip_message(stream, msg, state).await;
            }
            Message::TunnelRequest {
                node_id,
                service_name,
                timestamp_ms,
            } => {
                if let Err(e) = crate::node::tunnel::handle_tunnel(
                    stream,
                    node_id,
                    service_name,
                    timestamp_ms,
                    state,
                )
                .await
                {
                    error!("tunnel from {peer_addr}: {e}");
                }
            }
            Message::ReverseRegistration {
                node_id,
                timestamp_ms,
            } => {
                if let Err(e) = crate::node::relay::handle_reverse_registration(
                    stream, node_id, timestamp_ms, state,
                )
                .await
                {
                    debug!("reverse registration from {peer_addr} failed: {e}");
                }
            }
            Message::StatusRequest => {
                handle_status_request(stream, state).await;
            }
            Message::StatusResponse { .. } => {
                debug!("unexpected StatusResponse from {peer_addr}");
            }
            Message::MuxTunnel {} => {
                crate::mux::handle_mux_session(stream, state).await;
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Gossip message handling
// ---------------------------------------------------------------------------

/// Handle an already-parsed gossip message (Sync, ConfigUpdate, NodeJoin, NodeLeave).
///
/// If the message is a [`Message::Sync`], the state is merged and a Sync
/// reply is sent back so the peer can also update itself.
async fn handle_gossip_message(mut stream: TcpStream, msg: Message, state: Arc<SharedState>) {
    let peer_addr = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();

    match msg {
        Message::Sync { config, mut nodes } => {
            if !peer_addr.is_empty() {
                fix_unroutable_addrs(&mut nodes, &peer_addr);
            }
            merge_sync(&state, config, nodes).await;

            // Reply with our current state.
            let reply = build_sync_msg(&state).await;
            let secret = &state.config.cluster_secret;
            if let Err(e) = send_message(&mut stream, &reply, secret).await {
                debug!("failed to send sync reply: {e}");
            }
        }
        other => {
            apply_message(&state, other).await;
        }
    }
}

/// Handle a status request by replying with the daemon's current state.
async fn handle_status_request(mut stream: TcpStream, state: Arc<SharedState>) {
    let response = Message::StatusResponse {
        mode: format!("{:?}", state.config.mode).to_lowercase(),
        node_id: state.config.node_id.clone(),
        bind: state.config.bind.clone(),
        dynamic_config: state.dynamic_config.read().await.clone(),
        known_nodes: state.known_nodes.load().values().cloned().collect(),
    };

    let secret = &state.config.cluster_secret;
    if let Err(e) = send_message(&mut stream, &response, secret).await {
        debug!("status: failed to send response: {e}");
    }
}

// ---------------------------------------------------------------------------
// Sync loop
// ---------------------------------------------------------------------------

/// Background task: periodically read the config file and, if `dynamic_config`
/// has a higher version than what is in memory, update local state and
/// broadcast a [`Message::ConfigUpdate`] to all peers.
pub async fn run_config_poll_loop(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }

        let new_cfg = match crate::config::Config::load(&state.config_path) {
            Ok(c) => c,
            Err(e) => {
                debug!("config poll: failed to read {}: {e}", state.config_path);
                continue;
            }
        };

        let current_version = state.dynamic_config.read().await.version;
        debug!("config poll: file version={}, in-memory version={}", new_cfg.dynamic_config.version, current_version);
        if new_cfg.dynamic_config.version > current_version {
            info!(
                "config file changed: version {} -> {}",
                current_version, new_cfg.dynamic_config.version
            );
            {
                let mut dc = state.dynamic_config.write().await;
                *dc = new_cfg.dynamic_config.clone();
            }
            state.rebuild_service_index().await;
            print_dynamic_config(&state).await;
            broadcast_message(
                &state,
                Message::ConfigUpdate {
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
    let members = state.known_nodes.load();
    for node in members.values() {
        if !node.addr.is_empty() && !addrs.contains(&node.addr) {
            addrs.push(node.addr.clone());
        }
    }
    addrs
}

/// Background task: periodically push a Sync message to every known peer and
/// merge their reply.
pub async fn run_sync_loop(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }

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

    stream.set_nodelay(true).ok();

    let secret = &state.config.cluster_secret;
    let our_sync = build_sync_msg(state).await;
    send_message(&mut stream, &our_sync, secret).await?;

    let reply = recv_message(&mut stream, secret).await?;
    if let Message::Sync { config, mut nodes } = reply {
        fix_unroutable_addrs(&mut nodes, addr);
        merge_sync(state, config, nodes).await;
    }
    Ok(())
}

/// Replace `0.0.0.0` addresses in `nodes` with the actual peer address we
/// connected to, preserving the port from the original address.
fn fix_unroutable_addrs(nodes: &mut [NodeInfo], connected_addr: &str) {
    for node in nodes.iter_mut() {
        if let Some(port) = node.addr.strip_prefix("0.0.0.0:") {
            if let Some(host) = connected_addr.rsplit_once(':').map(|(h, _)| h) {
                let new_addr = format!("{host}:{port}");
                debug!(
                    "replacing unroutable address {} with {} for node {}",
                    node.addr, new_addr, node.id
                );
                node.addr = new_addr;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Broadcast
// ---------------------------------------------------------------------------

/// Send `msg` to every known peer node.  Connection errors are logged at
/// `debug` level because peers may be temporarily unreachable.
pub async fn broadcast_message(state: &Arc<SharedState>, msg: Message) {
    let peers = peer_addrs(state).await;

    let secret = state.config.cluster_secret.clone();

    for addr in peers {
        match time::timeout(Duration::from_secs(5), send_to_peer(&addr, &msg, &secret)).await {
            Ok(Err(e)) => debug!("broadcast to {addr} failed: {e}"),
            Err(_) => debug!("broadcast to {addr} timed out"),
            Ok(Ok(())) => {}
        }
    }
}

/// Open a connection to `addr` and send one message.
async fn send_to_peer(addr: &str, msg: &Message, secret: &str) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to {addr}"))?;

    stream.set_nodelay(true).ok();
    send_message(&mut stream, msg, secret).await
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

/// Returns `true` if `id` looks like a valid node identifier (hostname).
///
/// Rejects values that look like socket addresses (`host:port`) or raw IP
/// addresses with ports — these are addresses, not identities.
fn is_valid_node_id(id: &str) -> bool {
    !id.is_empty() && !id.contains(':')
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
        state.rebuild_service_index().await;
        print_dynamic_config(state).await;
        persist_state(state).await;
    }

    let nodes_changed = state.update_known_nodes(|kn| {
        let mut changed = false;
        for node in nodes {
            if !is_valid_node_id(&node.id) {
                debug!("ignoring node with invalid id: {:?}", node.id);
                continue;
            }
            if let Some(existing) = kn.get_mut(&node.id) {
                let has_addr = !node.addr.is_empty();
                if existing.addr != node.addr {
                    info!("node {} address updated: {} -> {}", node.id, existing.addr, node.addr);
                    existing.addr = node.addr;
                    changed = true;
                }
                if existing.relay != node.relay
                    && (node.relay.is_some() || has_addr)
                {
                    info!(
                        "node {} relay updated: {:?} -> {:?}",
                        node.id, existing.relay, node.relay
                    );
                    existing.relay = node.relay;
                    changed = true;
                }
            } else {
                info!("discovered new node: {} @ {}", node.id, node.addr);
                kn.insert(node.id.clone(), node);
                changed = true;
            }
        }
        changed
    }).await;

    if nodes_changed {
        persist_state(state).await;
    }
}

/// Apply a non-Sync gossip message to local state.
async fn apply_message(state: &Arc<SharedState>, msg: Message) {
    match msg {
        Message::ConfigUpdate { config } => {
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
                state.rebuild_service_index().await;
                print_dynamic_config(state).await;
                persist_state(state).await;
            }
        }
        Message::NodeJoin { id, addr } => {
            if !is_valid_node_id(&id) {
                debug!("ignoring NodeJoin with invalid id: {:?}", id);
                return;
            }
            let changed = state.update_known_nodes(|kn| {
                if let Some(existing) = kn.get_mut(&id) {
                    if existing.addr != addr {
                        info!("node {id} address updated: {} -> {addr}", existing.addr);
                        existing.addr = addr;
                        true
                    } else {
                        false
                    }
                } else {
                    info!("node joined: {id} @ {addr}");
                    kn.insert(id.clone(), NodeInfo { id, addr, relay: None });
                    true
                }
            }).await;
            if changed {
                persist_state(state).await;
            }
        }
        Message::NodeLeave { id } => {
            state.update_known_nodes(|kn| {
                if kn.remove(&id).is_some() {
                    info!("node left: {id}");
                }
            }).await;
        }
        Message::Sync { config, nodes } => {
            merge_sync(state, config, nodes).await;
        }
        _ => {}
    }
}

/// Periodically logs the number of known member nodes.
pub async fn run_member_log(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(60));
    interval.tick().await; // skip the immediate first tick
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }
        let nodes = state.known_nodes.load();
        info!("{} member node(s) in cluster", nodes.len());
        for node in nodes.values() {
            info!("  {} @ {}", node.id, node.addr);
        }
    }
}

/// Snapshot current state as a Sync message.
///
/// Always includes a `NodeInfo` for the sender itself so receivers can
/// populate the `hostname → address` mapping that `manage_proxies` requires
/// to match services (which store `node_id = hostname`) to connectable peers.
async fn build_sync_msg(state: &Arc<SharedState>) -> Message {
    let config = state.dynamic_config.read().await.clone();
    let mut nodes: Vec<NodeInfo> = state.known_nodes.load().values().cloned().collect();

    if !state.config.node_id.is_empty() {
        let self_addr = if state.config.outbound_only {
            String::new()
        } else {
            state.config.peer_addr().to_owned()
        };

        if let Some(existing) = nodes.iter_mut().find(|n| n.id == state.config.node_id) {
            existing.addr = self_addr;
        } else {
            nodes.push(NodeInfo {
                id: state.config.node_id.clone(),
                addr: self_addr,
                relay: None,
            });
        }
    }

    Message::Sync { config, nodes }
}
