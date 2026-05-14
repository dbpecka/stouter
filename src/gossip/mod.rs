pub mod messages;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time;
use tracing::{debug, error, info};

use crate::config::NodeInfo;
use crate::state::SharedState;
use crate::tls::MeshStream;
use messages::Message;

/// Maximum allowed message size: 1 MiB.
const MAX_MSG_BYTES: u32 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// Serialize `msg` to MessagePack and write the frame as `[4-byte len][payload]`.
///
/// Authentication is provided by the mTLS layer; no application-level MAC is added.
pub async fn send_message<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    msg: &Message,
) -> Result<()> {
    let payload = rmp_serde::to_vec(msg).context("serialize Message")?;
    let len = u32::try_from(payload.len()).context("message too large")?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .context("write length prefix")?;
    stream
        .write_all(&payload)
        .await
        .context("write payload")?;
    stream.flush().await.context("flush message")?;
    Ok(())
}

/// Read a binary-framed message from `stream` and deserialize the MessagePack payload.
///
/// Wire format: `[4-byte payload len][payload]`.
pub async fn recv_message<R: AsyncReadExt + Unpin>(stream: &mut R) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("read length prefix")?;
    let len = u32::from_be_bytes(len_buf);

    if len > MAX_MSG_BYTES {
        bail!("message too large: {len} bytes");
    }

    let mut payload = vec![0u8; len as usize];
    stream
        .read_exact(&mut payload)
        .await
        .context("read payload")?;

    let msg: Message = rmp_serde::from_slice(&payload).context("deserialize Message")?;
    Ok(msg)
}

// ---------------------------------------------------------------------------
// Connection dispatch
// ---------------------------------------------------------------------------

/// Handle an inbound connection by reading the first message and dispatching
/// to the appropriate handler based on its variant.
pub async fn dispatch_connection(mut stream: MeshStream, peer_addr: String, state: Arc<SharedState>) {
    match recv_message(&mut stream).await {
        Err(e) => {
            debug!("failed to receive message from {peer_addr}: {e}");
        }
        Ok(msg) => match msg {
            Message::Sync { .. }
            | Message::ConfigUpdate { .. }
            | Message::NodeJoin { .. }
            | Message::NodeLeave { .. } => {
                metrics::counter!("stouter_connections_total", "type" => "gossip").increment(1);
                handle_gossip_message(stream, peer_addr, msg, state).await;
            }
            Message::TunnelRequest {
                node_id,
                service_name,
                timestamp_ms,
            } => {
                metrics::counter!("stouter_connections_total", "type" => "tunnel").increment(1);
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
                metrics::counter!("stouter_connections_total", "type" => "reverse").increment(1);
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
                metrics::counter!("stouter_connections_total", "type" => "mux").increment(1);
                crate::mux::handle_mux_session(stream, state).await;
            }
            Message::AdminRequest { op } => {
                metrics::counter!("stouter_connections_total", "type" => "admin").increment(1);
                handle_admin_request(stream, op, state).await;
            }
            Message::AdminResponse { .. } => {
                debug!("unexpected AdminResponse from {peer_addr}");
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Gossip message handling
// ---------------------------------------------------------------------------

async fn handle_gossip_message(mut stream: MeshStream, peer_addr: String, msg: Message, state: Arc<SharedState>) {
    match msg {
        Message::Sync { config, mut nodes } => {
            if !peer_addr.is_empty() {
                fix_unroutable_addrs(&mut nodes, &peer_addr);
            }
            // No `contact_addr`: an inbound connection arrives from an
            // ephemeral source port, which won't match the peer's advertised
            // address, so it can't be used as a liveness signal here. The
            // outbound sync loop confirms every peer independently.
            merge_sync(&state, config, nodes, None).await;

            // Reply with our current state.
            let reply = build_sync_msg(&state).await;
            if let Err(e) = send_message(&mut stream, &reply).await {
                debug!("failed to send sync reply: {e}");
            }
        }
        other => {
            apply_message(&state, other).await;
        }
    }
}

async fn handle_admin_request(
    mut stream: MeshStream,
    op: messages::AdminOp,
    state: Arc<SharedState>,
) {
    use crate::config::{Mode, MutateErr, Service};
    use messages::{AdminErrCode, AdminOp, AdminResult, Message as Msg};

    // Authorization: peer's TLS CN must be in the configured allowlist.
    let allowed = match stream.peer_cn() {
        Some(cn) => state.config.admin_cns.iter().any(|a| a == cn),
        None => false,
    };
    if !allowed {
        let _ = send_message(
            &mut stream,
            &Msg::AdminResponse {
                result: AdminResult::Err {
                    code: AdminErrCode::Unauthorized,
                    message: "client cert CN not in adminCns allowlist".into(),
                },
            },
        )
        .await;
        return;
    }

    if matches!(state.config.mode, Mode::Subscribe) {
        let _ = send_message(
            &mut stream,
            &Msg::AdminResponse {
                result: AdminResult::Err {
                    code: AdminErrCode::Validation,
                    message: "admin RPC not supported in subscribe-only mode".into(),
                },
            },
        )
        .await;
        return;
    }

    // Apply mutation. Hold the write lock across persist so concurrent
    // admin RPCs serialize on version, and the file is updated before any
    // other writer can see the in-memory state.
    let outcome: Result<(u64, crate::config::DynamicConfig), MutateErr> = {
        let mut dc = state.dynamic_config.write().await;
        let result = match op {
            AdminOp::ServiceAdd { group, name, owner_node_id, port, domains } => {
                let svc = Service {
                    name,
                    node_id: owner_node_id,
                    node_port: port,
                    domains,
                };
                Ok(dc.upsert_service(&group, svc))
            }
            AdminOp::ServiceRename { group, owner_node_id, from, to } => {
                dc.rename_service(&group, &owner_node_id, &from, &to)
            }
            AdminOp::ServiceRemove { group, owner_node_id, name } => {
                dc.remove_service(&group, &owner_node_id, &name)
            }
        };
        match result {
            Ok(new_version) => {
                persist_state_with(&state, &dc);
                Ok((new_version, dc.clone()))
            }
            Err(err) => Err(err),
        }
    };

    let response = match outcome {
        Ok((new_version, new_dc)) => {
            state.rebuild_service_index().await;
            broadcast_message(&state, Msg::ConfigUpdate { config: new_dc }).await;
            Msg::AdminResponse { result: AdminResult::Ok { new_version } }
        }
        Err(err) => {
            let code = match err {
                MutateErr::GroupNotFound { .. } | MutateErr::ServiceNotFound { .. } => {
                    AdminErrCode::NotFound
                }
                MutateErr::DuplicateService { .. } => AdminErrCode::Validation,
            };
            Msg::AdminResponse {
                result: AdminResult::Err { code, message: err.to_string() },
            }
        }
    };

    if let Err(e) = send_message(&mut stream, &response).await {
        debug!("admin: failed to send response: {e}");
    }
}

async fn handle_status_request(mut stream: MeshStream, state: Arc<SharedState>) {
    let response = Message::StatusResponse {
        mode: format!("{:?}", state.config.mode).to_lowercase(),
        node_id: state.config.node_id.clone(),
        bind: state.config.bind.clone(),
        dynamic_config: state.dynamic_config.read().await.clone(),
        known_nodes: state.known_nodes.load().values().cloned().collect(),
    };

    if let Err(e) = send_message(&mut stream, &response).await {
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

        let (current_version, groups_match) = {
            let dc = state.dynamic_config.read().await;
            (
                dc.version,
                dc.service_groups == new_cfg.dynamic_config.service_groups,
            )
        };
        debug!(
            "config poll: file version={}, in-memory version={}, groups_match={}",
            new_cfg.dynamic_config.version, current_version, groups_match
        );

        let mut new_dc = if new_cfg.dynamic_config.version > current_version {
            info!(
                "config file version advanced: {} -> {}",
                current_version, new_cfg.dynamic_config.version
            );
            new_cfg.dynamic_config
        } else if !groups_match {
            let bumped = current_version + 1;
            info!(
                "manual config edit detected; auto-bumping version {} -> {}",
                current_version, bumped
            );
            new_cfg.dynamic_config
        } else {
            continue;
        };
        // Ensure the version we store and broadcast is monotonic relative to
        // in-memory state, so peers accept the update.
        if new_dc.version <= current_version {
            new_dc.version = current_version + 1;
        }

        {
            let mut dc = state.dynamic_config.write().await;
            *dc = new_dc.clone();
        }
        persist_state(&state).await;
        state.rebuild_service_index().await;
        print_dynamic_config(&state).await;
        broadcast_message(
            &state,
            Message::ConfigUpdate { config: new_dc },
        )
        .await;
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
            let start = std::time::Instant::now();
            match time::timeout(Duration::from_secs(10), sync_with_peer(&state, &addr)).await {
                Ok(Err(e)) => {
                    metrics::counter!("stouter_gossip_syncs_total", "result" => "error").increment(1);
                    debug!("sync with {addr} failed: {e}");
                }
                Err(_) => {
                    metrics::counter!("stouter_gossip_syncs_total", "result" => "timeout").increment(1);
                    debug!("sync with {addr} timed out");
                }
                Ok(Ok(())) => {
                    metrics::counter!("stouter_gossip_syncs_total", "result" => "ok").increment(1);
                    metrics::histogram!("stouter_gossip_sync_duration_seconds")
                        .record(start.elapsed().as_secs_f64());
                }
            }
        }
    }
}

async fn sync_with_peer(state: &Arc<SharedState>, addr: &str) -> Result<()> {
    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to {addr}"))?;
    tcp.set_nodelay(true).ok();
    let mut stream = state.tls_connect(tcp).await?;

    let our_sync = build_sync_msg(state).await;
    send_message(&mut stream, &our_sync).await?;

    let reply = recv_message(&mut stream).await?;
    if let Message::Sync { config, mut nodes } = reply {
        fix_unroutable_addrs(&mut nodes, addr);
        // A successful outbound sync is direct evidence the peer at `addr` is
        // reachable, so pass it as the liveness `contact_addr`.
        merge_sync(state, config, nodes, Some(addr)).await;
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

pub async fn broadcast_message(state: &Arc<SharedState>, msg: Message) {
    let peers = peer_addrs(state).await;

    for addr in peers {
        match time::timeout(
            Duration::from_secs(5),
            send_to_peer(&addr, &msg, state),
        )
        .await
        {
            Ok(Err(e)) => debug!("broadcast to {addr} failed: {e}"),
            Err(_) => debug!("broadcast to {addr} timed out"),
            Ok(Ok(())) => {}
        }
    }
}

async fn send_to_peer(addr: &str, msg: &Message, state: &Arc<SharedState>) -> Result<()> {
    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to {addr}"))?;
    tcp.set_nodelay(true).ok();
    let mut stream = state.tls_connect(tcp).await?;
    send_message(&mut stream, msg).await
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
/// fields (e.g. `bind`) are preserved.  The `known_nodes`
/// list in the config file is left untouched — it contains bootstrap peer
/// addresses, not the gossip-discovered member list.
async fn persist_state(state: &Arc<SharedState>) {
    let dc = state.dynamic_config.read().await.clone();
    persist_state_with(state, &dc);
}

/// Same as `persist_state`, but accepts the dynamic config directly so the
/// caller can persist while still holding a write lock on
/// `state.dynamic_config` — `persist_state` itself takes a read lock and would
/// deadlock under such a caller.
fn persist_state_with(state: &Arc<SharedState>, dc: &crate::config::DynamicConfig) {
    let mut cfg = match crate::config::Config::load(&state.config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to load config for persist: {e}");
            return;
        }
    };
    cfg.dynamic_config = dc.clone();
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
///
/// `contact_addr` is the address of the peer we are in direct contact with,
/// when known (i.e. an outbound sync we initiated). The node matching that
/// address has its `last_seen` liveness marker refreshed.
async fn merge_sync(
    state: &Arc<SharedState>,
    config: crate::config::DynamicConfig,
    nodes: Vec<NodeInfo>,
    contact_addr: Option<&str>,
) {
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
        for mut node in nodes {
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
                // Give a freshly discovered node an initial liveness grace
                // period; the sync loop will confirm or expire it shortly.
                node.last_seen = Some(Instant::now());
                kn.insert(node.id.clone(), node);
                changed = true;
            }
        }
        // Refresh the liveness marker for the peer we just synced with directly.
        if let Some(addr) = contact_addr {
            for node in kn.values_mut() {
                if !node.addr.is_empty() && node.addr == addr {
                    node.last_seen = Some(Instant::now());
                }
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
                    kn.insert(
                        id.clone(),
                        NodeInfo {
                            id,
                            addr,
                            relay: None,
                            last_seen: Some(Instant::now()),
                        },
                    );
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
            merge_sync(state, config, nodes, None).await;
        }
        _ => {}
    }
}

/// Periodically logs the number of known member nodes.
pub async fn run_member_log(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(300));
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
                last_seen: None,
            });
        }
    }

    Message::Sync { config, nodes }
}
