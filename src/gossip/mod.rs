pub mod messages;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::config::NodeInfo;
use crate::state::{SharedState, NODE_EXPIRE_AFTER};
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
    let payload = encode_message(msg)?;
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

/// Encode `msg` as MessagePack with structs written as field-name maps.
///
/// Named encoding keeps the wire format compatible across versions: a reader
/// ignores fields it does not know and fills defaults for ones the sender
/// omitted. Positional (array) encoding would break both whenever a field is
/// added or skipped via `skip_serializing_if`. Readers accept either form.
pub(crate) fn encode_message(msg: &Message) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(msg).context("serialize Message")
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
///
/// Only the handling of that message counts as in-flight work for a drain:
/// until it arrives the connection is idle (subscribers keep warm ones open
/// until they need a tunnel), and at shutdown an idle one is just closed.
pub async fn dispatch_connection(mut stream: MeshStream, peer_addr: String, state: Arc<SharedState>) {
    let first = tokio::select! {
        result = recv_message(&mut stream) => result,
        _ = state.shutdown.cancelled() => return,
    };
    let msg = match first {
        Ok(msg) => msg,
        Err(e) => {
            debug!("failed to receive message from {peer_addr}: {e}");
            return;
        }
    };
    // A mux session lasts as long as the peer keeps it, mostly idle, so it
    // isn't tracked as a whole; each of its streams is.
    let _in_flight = (!matches!(msg, Message::MuxTunnel {})).then(|| state.in_flight.track());
    match msg {
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
    }
}

// ---------------------------------------------------------------------------
// Gossip message handling
// ---------------------------------------------------------------------------

async fn handle_gossip_message(mut stream: MeshStream, peer_addr: String, msg: Message, state: Arc<SharedState>) {
    match msg {
        Message::Sync { config, nodes } => {
            // No `contact_addr`: an inbound connection arrives from an
            // ephemeral source port, which won't match the peer's advertised
            // address, so it can't be used as a liveness signal here. The
            // outbound sync loop confirms every peer independently.
            let source = SyncSource {
                cn: stream.peer_cn(),
                host: host_of(&peer_addr),
                contact_addr: None,
            };
            merge_sync(&state, config, nodes, &source).await;

            // Reply with our current state.
            let reply = build_sync_msg(&state).await;
            if let Err(e) = send_message(&mut stream, &reply).await {
                debug!("failed to send sync reply: {e}");
            }
        }
        other => {
            let peer_cn = stream.peer_cn().map(str::to_string);
            apply_message(&state, other, peer_cn.as_deref()).await;
        }
    }
}

async fn handle_admin_request(
    mut stream: MeshStream,
    op: messages::AdminOp,
    state: Arc<SharedState>,
) {
    use crate::config::{Mode, MutateErr};
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
            AdminOp::ConfigPush { config, base_version } => accept_push(&state, &dc, config, base_version),
            _ if state.config.require_signed_config => Err(MutateErr::Unauthorized(
                "this node accepts only signed config changes (requireSignedConfig): make the change \
                 with a stouter CLI that signs it, using an admin certificate"
                    .into(),
            )),
            op => {
                // Mutate a copy, so a refused change leaves nothing behind.
                let mut next = dc.clone();
                op.apply(&mut next).and_then(|_| check_change(&dc, next)).map(|mut next| {
                    next.changed_by = state.config.node_id.clone();
                    // An edit invalidates any signature the config carried.
                    next.signature = None;
                    next
                })
            }
        };
        match result {
            Ok(next) => {
                let new_version = next.version;
                *dc = next;
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
        Err(err) => Msg::AdminResponse {
            result: AdminResult::Err { code: AdminErrCode::from(&err), message: err.to_string() },
        },
    };

    if let Err(e) = send_message(&mut stream, &response).await {
        debug!("admin: failed to send response: {e}");
    }
}

/// `next` if changing `current` into it introduces no config problem (a
/// domain claimed twice, routes left on a removed service, ...); problems
/// `current` already had don't count.
fn check_change(
    current: &crate::config::DynamicConfig,
    next: crate::config::DynamicConfig,
) -> Result<crate::config::DynamicConfig, crate::config::MutateErr> {
    match crate::config::new_problem(current, &next) {
        Some(problem) => Err(crate::config::MutateErr::Invalid(problem)),
        None => Ok(next),
    }
}

/// The config to adopt for a pushed one: it must be signed by an admin, made
/// to the version we're at (else `Conflict`, and the CLI retries), and not
/// break the config. It is adopted exactly as signed.
fn accept_push(
    state: &SharedState,
    current: &crate::config::DynamicConfig,
    pushed: crate::config::DynamicConfig,
    base_version: u64,
) -> Result<crate::config::DynamicConfig, crate::config::MutateErr> {
    use crate::config::MutateErr;
    let tls = state
        .tls
        .as_deref()
        .ok_or_else(|| MutateErr::Unauthorized("no TLS context to check signatures with".into()))?;
    let (verified, signer) = crate::signing::verify(&pushed, tls, &state.config.admin_cns)
        .map_err(|e| MutateErr::Unauthorized(format!("{e:#}")))?;
    if current.version != base_version {
        return Err(MutateErr::Conflict { current: current.version });
    }
    if verified.version <= current.version {
        return Err(MutateErr::Invalid(format!(
            "pushed config version {} is not above the current version {}",
            verified.version, current.version
        )));
    }
    let verified = check_change(current, verified)?;
    info!("accepting config version {} signed by {signer}", verified.version);
    Ok(verified)
}

async fn handle_status_request(mut stream: MeshStream, state: Arc<SharedState>) {
    let response = Message::StatusResponse {
        mode: format!("{:?}", state.config.mode).to_lowercase(),
        node_id: state.config.node_id.clone(),
        bind: state.config.bind.clone(),
        dynamic_config: state.dynamic_config.read().await.clone(),
        known_nodes: snapshot_nodes(&state),
        require_signed_config: state.config.require_signed_config,
    };

    if let Err(e) = send_message(&mut stream, &response).await {
        debug!("status: failed to send response: {e}");
    }
}

// ---------------------------------------------------------------------------
// Sync loop
// ---------------------------------------------------------------------------

/// Background task: periodically read the config file and adopt its
/// `dynamic_config` when it is newer than what is in memory, then broadcast a
/// [`Message::ConfigUpdate`] to all peers.
///
/// The file counts as newer when its version is higher, or when it has the
/// same version as memory but different services or routes (a hand edit that didn't
/// bump the version; it is bumped here). A file at a *lower* version is
/// stale, not an edit: it is what's left when persisting failed (e.g. a
/// read-only mount), and adopting it would roll back every change made since
/// across the whole cluster.
pub async fn run_config_poll_loop(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(10));
    // The (file, memory) versions last warned about, so a file stuck behind
    // memory is reported once per change rather than on every poll.
    let mut warned_stale: Option<(u64, u64)> = None;
    // The unsigned file content last warned about (requireSignedConfig).
    let mut warned_unsigned: Option<String> = None;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }

        // Read the file under the lock every persist also holds, so the file
        // and memory are compared as one consistent snapshot.
        let new_dc = {
            let mut dc = state.dynamic_config.write().await;
            let file_dc = match crate::config::Config::load(&state.config_path) {
                Ok(c) => c.dynamic_config,
                Err(e) => {
                    debug!("config poll: failed to read {}: {e}", state.config_path);
                    continue;
                }
            };
            let current_version = dc.version;
            debug!(
                "config poll: file version={}, in-memory version={}",
                file_dc.version, current_version
            );

            if state.config.require_signed_config {
                // Only an admin-signed config counts: the CLI's local path
                // writes one, a hand edit can't. It is adopted exactly as
                // signed (no version bump, no changedBy).
                let content = |c: &crate::config::DynamicConfig| (c.service_groups.clone(), c.routes.clone());
                match crate::signing::admit(&state, file_dc.clone()) {
                    Ok(signed) if signed.supersedes(&dc) => {
                        info!("config file has a newer signed config: version {} -> {}", current_version, signed.version);
                        *dc = signed.clone();
                        persist_state_with(&state, &dc);
                        drop(dc);
                        state.rebuild_service_index().await;
                        print_dynamic_config(&state).await;
                        broadcast_message(&state, Message::ConfigUpdate { config: signed }).await;
                        continue;
                    }
                    verdict => {
                        // Edited by hand: unsigned, or a signature that
                        // covers something else than the file now says.
                        let edited = match &verdict {
                            Ok(signed) => content(signed) != content(&file_dc),
                            Err(_) => content(&file_dc) != content(&dc),
                        };
                        let key = serde_json::to_string(&content(&file_dc)).unwrap_or_default();
                        if edited && warned_unsigned.as_ref() != Some(&key) {
                            warn!(
                                "ignoring the edit to {}: this node accepts only signed config changes \
                                 (requireSignedConfig); push it with `stouter config push` using an admin certificate",
                                state.config_path
                            );
                            warned_unsigned = Some(key);
                        }
                        continue;
                    }
                }
            }

            let mut new_dc = if file_dc.version > current_version {
                info!(
                    "config file version advanced: {} -> {}",
                    current_version, file_dc.version
                );
                file_dc
            } else if file_dc.version == current_version
                && (file_dc.service_groups != dc.service_groups || file_dc.routes != dc.routes)
            {
                info!(
                    "manual config edit detected; auto-bumping version {} -> {}",
                    current_version,
                    current_version + 1
                );
                file_dc
            } else {
                if file_dc.version < current_version
                    && warned_stale != Some((file_dc.version, current_version))
                {
                    warn!(
                        "config file {} is at version {}, behind the running config (version {}); \
                         ignoring it. Persisting may be failing; to apply a manual edit, set its \
                         version above {}",
                        state.config_path, file_dc.version, current_version, current_version
                    );
                    warned_stale = Some((file_dc.version, current_version));
                }
                continue;
            };
            // Keep the version monotonic so peers accept the update.
            if new_dc.version <= current_version {
                new_dc.version = current_version + 1;
            }
            // The change entered the cluster here, unsigned.
            new_dc.changed_by = state.config.node_id.clone();
            new_dc.signature = None;
            *dc = new_dc.clone();
            persist_state_with(&state, &dc);
            new_dc
        };
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
///
/// Our own addresses are left out (a shared bootstrap list often names every
/// node, this one included): syncing with ourselves only wastes connections.
async fn peer_addrs(state: &Arc<SharedState>) -> Vec<String> {
    let own = [state.config.peer_addr().to_string(), state.config.bind.clone(), state.config.local_target()];
    let mut addrs: Vec<String> = Vec::new();
    let members = state.known_nodes.load();
    let candidates = state.bootstrap_peers.iter().chain(members.values().map(|n| &n.addr));
    for addr in candidates {
        if !addr.is_empty() && !own.contains(addr) && !addrs.contains(addr) {
            addrs.push(addr.clone());
        }
    }
    addrs
}

/// Background task: periodically push a Sync message to every known peer and
/// merge their reply.
pub async fn run_sync_loop(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(10));
    let mut round: u64 = 0;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }
        round += 1;

        // All at once: one peer that stopped answering must not delay
        // (and so make stale) every peer after it.
        let mut peers = peer_addrs(&state).await;
        // Now and then, nodes we forgot for silence too: they may only have
        // been cut off, and if neither side lists the other as a bootstrap
        // peer, this is how the two find each other once the network heals.
        if round.is_multiple_of(FORGOTTEN_RETRY_ROUNDS) {
            for addr in state.forgotten_addrs() {
                if !peers.contains(&addr) {
                    peers.push(addr);
                }
            }
        }
        futures::future::join_all(peers.iter().map(|addr| {
            let state = &state;
            async move {
                let start = std::time::Instant::now();
                match time::timeout(Duration::from_secs(10), sync_with_peer(state, addr)).await {
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
        }))
        .await;

        expire_silent_nodes(&state).await;
    }
}

/// Sync rounds between retries of forgotten nodes' last addresses (about a
/// minute).
const FORGOTTEN_RETRY_ROUNDS: u64 = 6;

/// Forget nodes with no sign of life for [`NODE_EXPIRE_AFTER`]: no direct
/// contact and no rising heartbeat. They are buried so a stale copy still
/// circulating in gossip doesn't bring them back, and their last address is
/// retried for a while (see `run_sync_loop`) in case they were only cut off.
///
/// A node that has never sent a heartbeat (an older version) and has no
/// address is kept: there is no way to tell whether it is alive.
async fn expire_silent_nodes(state: &Arc<SharedState>) {
    let now = Instant::now();
    state
        .update_known_nodes(|kn| {
            kn.retain(|id, node| {
                let silent = node.heard_at.is_none_or(|t| now.duration_since(t) > NODE_EXPIRE_AFTER);
                let can_tell = node.heartbeat > 0 || !node.addr.is_empty();
                if silent && can_tell {
                    info!("forgetting node {id}: no sign of life for {}s", NODE_EXPIRE_AFTER.as_secs());
                    state.bury(id, node.heartbeat);
                    state.remember_forgotten(id, &node.addr);
                    false
                } else {
                    true
                }
            })
        })
        .await;
}

async fn sync_with_peer(state: &Arc<SharedState>, addr: &str) -> Result<()> {
    let mut stream = state.connect_mesh(addr, None).await?;

    let our_sync = build_sync_msg(state).await;
    send_message(&mut stream, &our_sync).await?;

    let reply = recv_message(&mut stream).await?;
    if let Message::Sync { config, nodes } = reply {
        // A successful outbound sync is direct evidence the peer at `addr` is
        // reachable, so pass it as the liveness `contact_addr`.
        let source = SyncSource { cn: stream.peer_cn(), host: host_of(addr), contact_addr: Some(addr) };
        merge_sync(state, config, nodes, &source).await;
    }
    Ok(())
}

/// Who a `Sync` came from.
#[derive(Default)]
struct SyncSource<'a> {
    /// The peer's certificate CN (authenticated by mTLS): its node id.
    cn: Option<&'a str>,
    /// The host we reached it at, or it reached us from.
    host: Option<&'a str>,
    /// The address we dialled, when we started the sync: proof that the
    /// node advertising it is reachable.
    contact_addr: Option<&'a str>,
}

/// The host part of `addr` (`host:port`, `[v6]:port`).
fn host_of(addr: &str) -> Option<&str> {
    addr.rsplit_once(':').map(|(host, _)| host).filter(|h| !h.is_empty())
}

/// Whether `addr` is a wildcard (`0.0.0.0:port`, `[::]:port`): what a node
/// bound to all interfaces advertises if it has no `advertiseAddr`.
fn is_wildcard(addr: &str) -> bool {
    addr.starts_with("0.0.0.0:") || addr.starts_with("[::]:")
}

/// A wildcard `addr` with its host replaced by `host` (where we actually
/// reached the node), keeping the port; `None` if `addr` isn't a wildcard.
fn rewrite_wildcard(addr: &str, host: &str) -> Option<String> {
    let port = addr.strip_prefix("0.0.0.0:").or_else(|| addr.strip_prefix("[::]:"))?;
    Some(format!("{host}:{port}"))
}

// ---------------------------------------------------------------------------
// Broadcast
// ---------------------------------------------------------------------------

/// Push our current state to every peer without waiting for the sync loop.
pub(crate) async fn broadcast_sync(state: &Arc<SharedState>) {
    let msg = build_sync_msg(state).await;
    broadcast_message(state, msg).await;
}

pub async fn broadcast_message(state: &Arc<SharedState>, msg: Message) {
    let peers = peer_addrs(state).await;
    // All at once, so one unreachable peer doesn't hold up the rest (or an
    // admin RPC waiting on this).
    let msg = &msg;
    futures::future::join_all(peers.iter().map(|addr| async move {
        match time::timeout(Duration::from_secs(5), send_to_peer(addr, msg, state)).await {
            Ok(Err(e)) => debug!("broadcast to {addr} failed: {e}"),
            Err(_) => debug!("broadcast to {addr} timed out"),
            Ok(Ok(())) => {}
        }
    }))
    .await;
}

async fn send_to_peer(addr: &str, msg: &Message, state: &Arc<SharedState>) -> Result<()> {
    let mut stream = state.connect_mesh(addr, None).await?;
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
    // Hold the lock while writing: every change persists under the write
    // lock, so this write can't land on top of a newer version's.
    let dc = state.dynamic_config.read().await;
    persist_state_with(state, &dc);
}

/// Same as `persist_state`, but accepts the dynamic config directly so the
/// caller can persist while still holding a write lock on
/// `state.dynamic_config` — `persist_state` itself takes a read lock and would
/// deadlock under such a caller. Every change to `state.dynamic_config`
/// persists this way before releasing the lock, which keeps the file written
/// in version order (the config poll loop relies on that).
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
    source: &SyncSource<'_>,
) {
    let updated = {
        let mut dc = state.dynamic_config.write().await;
        adopt_if_newer(state, &mut dc, config, "dynamic config updated")
    };

    if updated {
        state.rebuild_service_index().await;
        print_dynamic_config(state).await;
    }

    let own_id = state.config.node_id.as_str();
    let strict = state.config.require_signed_config;
    let now = Instant::now();
    let nodes_changed = state.update_known_nodes(|kn| {
        let mut changed = false;
        for mut node in nodes {
            if !is_valid_node_id(&node.id) {
                debug!("ignoring node with invalid id: {:?}", node.id);
                continue;
            }
            // We know our own state first-hand; a relayed copy of it is
            // stale by definition, and we are not our own peer.
            if node.id == own_id {
                continue;
            }
            // A node that signs its own record is believed only through it.
            if let Some(signed) = node.signed.take() {
                changed |= merge_signed(state, kn, &node, signed, source, now);
                continue;
            }
            // Unsigned (an older node, or relayed by one): never let it
            // override what a node signed about itself, and with
            // `requireSignedConfig` (every node signs) ignore it altogether.
            if strict || kn.get(&node.id).is_some_and(|n| n.signed.is_some()) {
                debug!("ignoring unsigned gossip about node {}", node.id);
                continue;
            }
            // Only the node itself knows where it was reached from.
            if source.cn == Some(node.id.as_str()) {
                if let Some(addr) = source.host.and_then(|host| rewrite_wildcard(&node.addr, host)) {
                    node.addr = addr;
                }
            } else if is_wildcard(&node.addr)
                && kn.get(&node.id).is_some_and(|n| !n.addr.is_empty() && !is_wildcard(&n.addr))
            {
                // A relayed wildcard says nothing new; keep the address we
                // learned from the node itself.
                node.addr = kn[&node.id].addr.clone();
            }
            // Health changes are not `changed`: that flag drives persisting
            // config, and health is runtime-only state.
            merge_health(kn.get_mut(&node.id), &node);
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
                // A rising heartbeat, however it reached us, means the node
                // is alive. For a node with no address it's also the only
                // liveness signal there is: it can't be contacted directly.
                if node.heartbeat > existing.heartbeat {
                    existing.heartbeat = node.heartbeat;
                    existing.heard_at = Some(now);
                    if existing.addr.is_empty() {
                        existing.last_seen = Some(now);
                    }
                }
            } else {
                // A node that left or was forgotten comes back only with a
                // heartbeat newer than the one it departed with.
                if state.is_buried(&node.id, node.heartbeat) {
                    debug!("ignoring stale gossip about departed node {}", node.id);
                    continue;
                }
                state.unbury(&node.id);
                info!("discovered new node: {} @ {}", node.id, node.addr);
                // Give a freshly discovered node an initial liveness grace
                // period; the sync loop will confirm or expire it shortly.
                node.last_seen = Some(now);
                node.heard_at = Some(now);
                kn.insert(node.id.clone(), node);
                changed = true;
            }
        }
        // Refresh the liveness marker for the peer we just synced with directly.
        if let Some(addr) = source.contact_addr {
            for node in kn.values_mut() {
                if !node.addr.is_empty() && node.addr == addr {
                    node.last_seen = Some(now);
                    node.heard_at = Some(now);
                }
            }
        }
        changed
    }).await;

    if nodes_changed {
        persist_state(state).await;
    }
}

/// Replace `dc` with `incoming` if it supersedes it and this node may adopt
/// it (with `requireSignedConfig`, only an admin-signed config, adopted as
/// signed). Persists under the caller's lock. Returns whether it was adopted.
fn adopt_if_newer(
    state: &Arc<SharedState>,
    dc: &mut crate::config::DynamicConfig,
    incoming: crate::config::DynamicConfig,
    what: &str,
) -> bool {
    if !incoming.supersedes(dc) {
        return false;
    }
    let version = incoming.version;
    match crate::signing::admit(state, incoming) {
        // The signed text decides; the envelope around it may claim more.
        Ok(config) if config.supersedes(dc) => {
            log_adoption(dc, &config, what);
            *dc = config;
            persist_state_with(state, dc);
            true
        }
        Ok(_) => false,
        Err(e) => {
            crate::signing::log_refusal(state, version, &e);
            false
        }
    }
}

/// Log that `incoming` is replacing `current`.
fn log_adoption(current: &crate::config::DynamicConfig, incoming: &crate::config::DynamicConfig, what: &str) {
    if incoming.version == current.version {
        warn!(
            "{what}: two different configs at version {}; keeping the one from '{}' over '{}' (the same choice on every node)",
            incoming.version, incoming.changed_by, current.changed_by
        );
    } else {
        info!("{what}: version {} -> {}", current.version, incoming.version);
    }
}

/// Merge `signed`, the record node `node.id` signed about itself (relayed
/// by `source`), into `kn`. Returns whether its address or relays changed.
///
/// Records no newer than the one held are skipped before verifying (every
/// peer relays every record each round). A record that doesn't verify, or
/// that some other node signed, is dropped: only node X speaks for X.
fn merge_signed(
    state: &SharedState,
    kn: &mut std::collections::HashMap<String, NodeInfo>,
    node: &NodeInfo,
    signed: crate::config::SignedNode,
    source: &SyncSource<'_>,
    now: Instant,
) -> bool {
    let held = kn.get(&node.id).filter(|n| n.signed.is_some()).map(|n| n.heartbeat);
    match crate::signing::claimed_heartbeat(&signed) {
        Some(claimed) if held.is_none_or(|h| claimed > h) => {}
        _ => return false,
    }
    let Some(tls) = state.tls.as_deref() else { return false };
    let record = match crate::signing::verify_node(&signed, tls) {
        Ok(record) if record.id == node.id => record,
        Ok(record) => {
            debug!("ignoring {}'s record relayed as node {}", record.id, node.id);
            return false;
        }
        Err(e) => {
            debug!("ignoring a record for node {} that doesn't verify: {e:#}", node.id);
            return false;
        }
    };

    // A wildcard address means "wherever you reached me": only the node
    // itself can tell us that. Relayed, keep the address we already have.
    let mut addr = record.addr.clone();
    if is_wildcard(&addr) {
        if source.cn == Some(record.id.as_str()) {
            if let Some(rewritten) = source.host.and_then(|host| rewrite_wildcard(&addr, host)) {
                addr = rewritten;
            }
        } else if let Some(known) = kn.get(&record.id).filter(|n| !n.addr.is_empty() && !is_wildcard(&n.addr)) {
            addr = known.addr.clone();
        }
    }

    match kn.get_mut(&record.id) {
        Some(existing) => {
            let mut changed = false;
            if existing.addr != addr {
                info!("node {} address updated: {} -> {}", record.id, existing.addr, addr);
                changed = true;
            }
            if existing.relays != record.relays {
                debug!("node {} relays: {:?} -> {:?}", record.id, existing.relays, record.relays);
                changed = true;
            }
            if existing.health.unhealthy != record.health.unhealthy {
                info!(
                    "node {} health updated: unhealthy services {:?} -> {:?}",
                    record.id, existing.health.unhealthy, record.health.unhealthy
                );
            }
            existing.addr = addr;
            existing.heartbeat = record.heartbeat;
            existing.health = record.health;
            existing.relays = record.relays;
            existing.signed = Some(signed);
            // Its relays are the ones it signed; drop any hint unsigned
            // gossip left (a relay of ours sets it again for itself).
            if existing.relay.is_some() && !existing.addr.is_empty() {
                existing.relay = None;
            }
            existing.heard_at = Some(now);
            // A node with no address can't be contacted directly; its
            // heartbeat is the only liveness signal there is.
            if existing.addr.is_empty() {
                existing.last_seen = Some(now);
            }
            changed
        }
        None => {
            // A node that left or was forgotten comes back only with a
            // heartbeat newer than the one it departed with.
            if state.is_buried(&record.id, record.heartbeat) {
                debug!("ignoring stale gossip about departed node {}", record.id);
                return false;
            }
            state.unbury(&record.id);
            info!("discovered new node: {} @ {}", record.id, addr);
            kn.insert(
                record.id.clone(),
                NodeInfo {
                    id: record.id,
                    addr,
                    heartbeat: record.heartbeat,
                    health: record.health,
                    relays: record.relays,
                    signed: Some(signed),
                    // An initial liveness grace period; the sync loop will
                    // confirm or expire it shortly.
                    last_seen: Some(now),
                    heard_at: Some(now),
                    ..Default::default()
                },
            );
            true
        }
    }
}

/// Adopt `incoming`'s health report if it is newer than what `existing` holds.
fn merge_health(existing: Option<&mut NodeInfo>, incoming: &NodeInfo) {
    let Some(existing) = existing else { return };
    if incoming.health.version <= existing.health.version {
        return;
    }
    if existing.health.unhealthy != incoming.health.unhealthy {
        info!(
            "node {} health updated: unhealthy services {:?} -> {:?}",
            incoming.id, existing.health.unhealthy, incoming.health.unhealthy
        );
    }
    existing.health = incoming.health.clone();
}

/// Apply a non-Sync gossip message to local state. `peer_cn` names the
/// sender's certificate: a node may only announce its own join or leave.
async fn apply_message(state: &Arc<SharedState>, msg: Message, peer_cn: Option<&str>) {
    match msg {
        Message::ConfigUpdate { config } => {
            let updated = {
                let mut dc = state.dynamic_config.write().await;
                adopt_if_newer(state, &mut dc, config, "config update received")
            };
            if updated {
                state.rebuild_service_index().await;
                print_dynamic_config(state).await;
            }
        }
        Message::NodeJoin { id, addr } => {
            if !is_valid_node_id(&id) || id == state.config.node_id {
                debug!("ignoring NodeJoin with invalid id: {:?}", id);
                return;
            }
            if peer_cn != Some(id.as_str()) {
                warn!("ignoring NodeJoin for {id} sent by {}", peer_cn.unwrap_or("an unnamed peer"));
                return;
            }
            state.unbury(&id);
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
                    let now = Instant::now();
                    kn.insert(
                        id.clone(),
                        NodeInfo { id, addr, last_seen: Some(now), heard_at: Some(now), ..Default::default() },
                    );
                    true
                }
            }).await;
            if changed {
                persist_state(state).await;
            }
        }
        Message::NodeLeave { id, heartbeat } => {
            if peer_cn != Some(id.as_str()) {
                warn!("ignoring NodeLeave for {id} sent by {}", peer_cn.unwrap_or("an unnamed peer"));
                return;
            }
            state.update_known_nodes(|kn| {
                let last = kn.remove(&id).map(|n| n.heartbeat);
                if last.is_some() {
                    info!("node left: {id}");
                }
                // Bury it so gossip still carrying it doesn't re-add it; a
                // restart sends a newer heartbeat and is let back in.
                state.bury(&id, heartbeat.max(last.unwrap_or(0)));
            }).await;
        }
        Message::Sync { config, nodes } => {
            let source = SyncSource { cn: peer_cn, ..Default::default() };
            merge_sync(state, config, nodes, &source).await;
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
    let nodes = snapshot_nodes(state);
    Message::Sync { config, nodes }
}

/// A new heartbeat (and signature) at most this often while nothing else
/// about this node changes. Every record a peer hasn't seen is verified, so
/// one per sync message sent would cost each node a check per node per
/// message; this keeps it to a couple per node per round.
const OWN_RECORD_REFRESH: Duration = Duration::from_secs(5);

/// What this node says about itself (at `addr`), signed so no other node
/// can change it on the way: peers believe it only with our signature.
/// Reused while fresh and unchanged; a change (health, relays, address)
/// goes out at once.
fn own_record(state: &SharedState, addr: String) -> (crate::config::NodeRecord, Option<crate::config::SignedNode>) {
    let health = (**state.local_health.load()).clone();
    let relays = if state.config.outbound_only { state.current_relays() } else { Vec::new() };
    let mut cache = state.own_record.lock().unwrap();
    if let Some(own) = cache.as_ref() {
        let same = own.record.addr == addr && own.record.health == health && own.record.relays == relays;
        if same && own.at.elapsed() < OWN_RECORD_REFRESH {
            return (own.record.clone(), own.signed.clone());
        }
    }
    let record = crate::config::NodeRecord {
        id: state.config.node_id.clone(),
        addr,
        heartbeat: state.next_heartbeat(),
        health,
        relays,
    };
    let signed = state.signer.get().and_then(|signer| match crate::signing::sign_node(&record, signer) {
        Ok(signed) => Some(signed),
        Err(e) => {
            warn!("cannot sign this node's record: {e:#}");
            None
        }
    });
    *cache = Some(crate::state::OwnRecord { record: record.clone(), signed: signed.clone(), at: std::time::Instant::now() });
    (record, signed)
}

/// Known nodes plus an up-to-date entry for ourselves (address and health).
fn snapshot_nodes(state: &SharedState) -> Vec<NodeInfo> {
    let mut nodes: Vec<NodeInfo> = state.known_nodes.load().values().cloned().collect();

    if !state.config.node_id.is_empty() {
        let self_addr = if state.config.outbound_only {
            String::new()
        } else {
            state.config.peer_addr().to_owned()
        };

        let (record, signed) = own_record(state, self_addr);
        nodes.retain(|n| n.id != state.config.node_id);
        nodes.push(NodeInfo {
            id: record.id,
            addr: record.addr,
            health: record.health,
            heartbeat: record.heartbeat,
            signed,
            ..Default::default()
        });
    }

    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DynamicConfig, ServiceHealth};
    use tokio_util::sync::CancellationToken;

    fn node(id: &str, version: u64, unhealthy: &[&str]) -> NodeInfo {
        NodeInfo {
            id: id.into(),
            addr: format!("{id}:1"),
            relay: None,
            last_seen: None,
            health: ServiceHealth {
                version,
                unhealthy: unhealthy.iter().map(|s| s.to_string()).collect(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn merge_health_takes_only_newer_reports() {
        let mut existing = node("a", 5, &["api"]);
        merge_health(Some(&mut existing), &node("a", 4, &[]));
        assert_eq!(existing.health.unhealthy, ["api"]);
        merge_health(Some(&mut existing), &node("a", 5, &[]));
        assert_eq!(existing.health.unhealthy, ["api"]);
        merge_health(Some(&mut existing), &node("a", 6, &[]));
        assert!(existing.health.unhealthy.is_empty());
        assert_eq!(existing.health.version, 6);
    }

    #[test]
    fn merge_health_ignores_unreported_relay() {
        // An old-version peer relays NodeInfo without health (version 0).
        let mut existing = node("a", 5, &["api"]);
        merge_health(Some(&mut existing), &node("a", 0, &[]));
        assert_eq!(existing.health.version, 5);
    }

    #[tokio::test]
    async fn merge_sync_applies_peer_health_but_never_our_own() {
        let cfg = Config { node_id: "me".into(), ..Config::default() };
        let state = SharedState::new(cfg, String::new(), CancellationToken::new(), None);
        let config = DynamicConfig::default();

        merge_sync(&state, config.clone(), vec![node("a", 1, &[]), node("me", 1, &["x"])], &SyncSource::default()).await;
        merge_sync(&state, config, vec![node("a", 2, &["api"]), node("me", 9, &["x"])], &SyncSource::default()).await;

        let kn = state.known_nodes.load();
        assert_eq!(kn["a"].health.unhealthy, ["api"]);
        assert!(!kn.contains_key("me"), "a daemon is not its own peer");
        assert!(!state.service_unhealthy("me", "x"));
    }

    #[test]
    fn snapshot_carries_fresh_local_health() {
        let cfg = Config { node_id: "me".into(), ..Config::default() };
        let state = SharedState::new(cfg, String::new(), CancellationToken::new(), None);
        state.local_health.store(Arc::new(ServiceHealth { version: 3, unhealthy: vec!["api".into()] }));
        let nodes = snapshot_nodes(&state);
        let me = nodes.iter().find(|n| n.id == "me").unwrap();
        assert_eq!(me.health.unhealthy, ["api"]);
    }

    fn named_services(names: &[&str]) -> DynamicConfig {
        let services = names
            .iter()
            .map(|n| serde_json::from_value(serde_json::json!({"name": n, "nodeId": "n1", "nodePort": 1})).unwrap())
            .collect();
        DynamicConfig {
            version: 0,
            service_groups: vec![crate::config::ServiceGroup { name: "g".into(), services }],
            routes: vec![],
            ..Default::default()
        }
    }

    fn service_names(dc: &DynamicConfig) -> Vec<String> {
        dc.service_groups[0].services.iter().map(|s| s.name.clone()).collect()
    }

    /// Run the poll loop for one pass against a config file holding `file`
    /// while memory holds `memory`. Returns memory and the file afterwards.
    async fn poll_once(file: DynamicConfig, memory: DynamicConfig) -> (DynamicConfig, DynamicConfig) {
        let path = std::env::temp_dir().join(format!(
            "stouter_poll_{}_{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path_s = path.to_str().unwrap().to_string();
        let cfg = Config { node_id: "me".into(), dynamic_config: file, ..Config::default() };
        cfg.save(&path_s).unwrap();
        let state = SharedState::new(cfg, path_s.clone(), CancellationToken::new(), None);
        *state.dynamic_config.write().await = memory;

        let poll = tokio::spawn(run_config_poll_loop(state.clone()));
        // The first tick fires at once; give the pass time to finish.
        tokio::time::sleep(Duration::from_millis(100)).await;
        poll.abort();
        let memory = state.dynamic_config.read().await.clone();
        let file = Config::load(&path_s).unwrap().dynamic_config;
        let _ = std::fs::remove_file(&path);
        (memory, file)
    }

    #[tokio::test(start_paused = true)]
    async fn poll_keeps_newer_memory_over_a_stale_file() {
        // Persisting v6 failed (read-only mount, full disk), so the file is
        // still at v5. That's not an edit: memory must stay at v6.
        let file = DynamicConfig { version: 5, ..named_services(&["old"]) };
        let memory = DynamicConfig { version: 6, ..named_services(&["old", "new"]) };
        let (memory, file) = poll_once(file, memory).await;
        assert_eq!(memory.version, 6);
        assert_eq!(service_names(&memory), ["old", "new"]);
        assert_eq!(file.version, 5, "a stale file is left alone, not overwritten");
    }

    #[tokio::test(start_paused = true)]
    async fn poll_adopts_hand_edits_and_newer_files() {
        // Edited without bumping the version: adopted, bumped and persisted.
        let file = DynamicConfig { version: 5, ..named_services(&["old", "edited"]) };
        let memory = DynamicConfig { version: 5, ..named_services(&["old"]) };
        let (memory, file) = poll_once(file, memory).await;
        assert_eq!((memory.version, service_names(&memory)), (6, vec!["old".into(), "edited".into()]));
        assert_eq!(file, memory);

        // A higher file version is adopted as-is.
        let file = DynamicConfig { version: 9, ..named_services(&["cli-added"]) };
        let memory = DynamicConfig { version: 5, ..named_services(&["old"]) };
        let (memory, _) = poll_once(file, memory).await;
        assert_eq!((memory.version, service_names(&memory)), (9, vec!["cli-added".into()]));
    }

    fn natd(id: &str, heartbeat: u64) -> NodeInfo {
        NodeInfo { id: id.into(), relay: Some("relay:1".into()), heartbeat, ..Default::default() }
    }

    fn state_for(id: &str) -> Arc<SharedState> {
        SharedState::new(Config { node_id: id.into(), ..Config::default() }, String::new(), CancellationToken::new(), None)
    }

    /// Set `id`'s liveness markers `ago` into the past.
    async fn age(state: &SharedState, id: &str, ago: Duration) {
        let then = Instant::now() - ago;
        state
            .update_known_nodes(|kn| {
                let n = kn.get_mut(id).unwrap();
                n.last_seen = Some(then);
                n.heard_at = Some(then);
            })
            .await;
    }

    #[tokio::test]
    async fn a_rising_heartbeat_keeps_a_natd_node_live() {
        let state = state_for("sub");
        let dc = DynamicConfig::default();
        merge_sync(&state, dc.clone(), vec![natd("natd", 100)], &SyncSource { contact_addr: Some("relay:1"), ..Default::default() }).await;
        age(&state, "natd", Duration::from_secs(31)).await;

        // The same heartbeat relayed again is no sign of life.
        merge_sync(&state, dc.clone(), vec![natd("natd", 100)], &SyncSource { contact_addr: Some("relay:1"), ..Default::default() }).await;
        assert!(!crate::state::node_is_live(&state.known_nodes.load()["natd"], Instant::now()));

        merge_sync(&state, dc, vec![natd("natd", 101)], &SyncSource { contact_addr: Some("relay:1"), ..Default::default() }).await;
        let node = state.known_nodes.load()["natd"].clone();
        assert!(crate::state::node_is_live(&node, Instant::now()));
        assert_eq!(node.heartbeat, 101);
    }

    #[tokio::test]
    async fn silent_nodes_are_forgotten_and_stale_gossip_cannot_revive_them() {
        let state = state_for("sub");
        let dc = DynamicConfig::default();
        let direct = NodeInfo { id: "dead".into(), addr: "dead:1".into(), heartbeat: 50, ..Default::default() };
        merge_sync(&state, dc.clone(), vec![direct.clone(), natd("alive", 7)], &SyncSource::default()).await;
        age(&state, "dead", NODE_EXPIRE_AFTER + Duration::from_secs(1)).await;
        expire_silent_nodes(&state).await;
        let kn = state.known_nodes.load();
        assert!(!kn.contains_key("dead"));
        assert!(kn.contains_key("alive"), "recently heard from");

        // A peer that hasn't noticed relays its old copy: still gone.
        merge_sync(&state, dc.clone(), vec![direct.clone()], &SyncSource::default()).await;
        assert!(!state.known_nodes.load().contains_key("dead"));
        // A restarted node gossips a newer heartbeat: welcome back.
        merge_sync(&state, dc, vec![NodeInfo { heartbeat: 51, ..direct }], &SyncSource::default()).await;
        assert!(state.known_nodes.load().contains_key("dead"));
    }

    #[tokio::test]
    async fn old_natd_nodes_without_heartbeats_are_kept() {
        // An older NAT'd node sends no heartbeat and can't be contacted, so
        // silence proves nothing about it.
        let state = state_for("sub");
        merge_sync(&state, DynamicConfig::default(), vec![natd("old", 0)], &SyncSource::default()).await;
        age(&state, "old", NODE_EXPIRE_AFTER * 2).await;
        expire_silent_nodes(&state).await;
        assert!(state.known_nodes.load().contains_key("old"));
    }

    #[tokio::test]
    async fn only_a_node_itself_may_announce_its_join_or_leave() {
        let state = state_for("sub");
        let join = |id: &str| Message::NodeJoin { id: id.into(), addr: format!("{id}:1") };
        apply_message(&state, join("a"), Some("mallory")).await;
        assert!(!state.known_nodes.load().contains_key("a"), "join for someone else");
        apply_message(&state, join("a"), Some("a")).await;
        assert!(state.known_nodes.load().contains_key("a"));

        apply_message(&state, Message::NodeLeave { id: "a".into(), heartbeat: 9 }, Some("mallory")).await;
        assert!(state.known_nodes.load().contains_key("a"), "leave for someone else");
        apply_message(&state, Message::NodeLeave { id: "a".into(), heartbeat: 9 }, Some("a")).await;
        assert!(!state.known_nodes.load().contains_key("a"));

        // Gossip still carrying its pre-leave heartbeat doesn't re-add it.
        let relayed = NodeInfo { id: "a".into(), addr: "a:1".into(), heartbeat: 9, ..Default::default() };
        merge_sync(&state, DynamicConfig::default(), vec![relayed], &SyncSource::default()).await;
        assert!(!state.known_nodes.load().contains_key("a"));
    }

    #[test]
    fn own_record_is_reused_until_it_changes() {
        let state = state_for("me");
        let me = |state: &SharedState| snapshot_nodes(state).into_iter().find(|n| n.id == "me").unwrap().heartbeat;
        let first = me(&state);
        assert!(first > 0);
        assert_eq!(me(&state), first, "unchanged and fresh: the same record again");
        state.local_health.store(Arc::new(ServiceHealth { version: 1, unhealthy: vec!["api".into()] }));
        assert!(me(&state) > first, "a health change goes out at once, with a newer heartbeat");
    }

    #[tokio::test]
    async fn peer_list_leaves_out_our_own_address() {
        let cfg = Config {
            node_id: "me".into(),
            bind: "0.0.0.0:7000".into(),
            advertise_addr: Some("10.0.0.1:7000".into()),
            known_nodes: vec!["10.0.0.1:7000".into(), "127.0.0.1:7000".into(), "10.0.0.2:7000".into()],
            ..Config::default()
        };
        let state = SharedState::new(cfg, String::new(), CancellationToken::new(), None);
        assert_eq!(peer_addrs(&state).await, ["10.0.0.2:7000"]);
    }

    #[tokio::test]
    async fn only_the_sender_s_own_wildcard_address_is_rewritten() {
        let state = state_for("me");
        let nodes = vec![
            NodeInfo { id: "peer".into(), addr: "[::]:7001".into(), ..Default::default() },
            NodeInfo { id: "other".into(), addr: "0.0.0.0:7002".into(), ..Default::default() },
        ];
        let source = SyncSource { cn: Some("peer"), host: Some("[fd00::5]"), contact_addr: None };
        merge_sync(&state, DynamicConfig::default(), nodes, &source).await;
        let kn = state.known_nodes.load();
        assert_eq!(kn["peer"].addr, "[fd00::5]:7001", "where we reached it");
        assert_eq!(kn["other"].addr, "0.0.0.0:7002", "not the relaying peer's host");
        assert_eq!(rewrite_wildcard("0.0.0.0:80", "10.0.0.1").as_deref(), Some("10.0.0.1:80"));
        assert_eq!(rewrite_wildcard("10.0.0.2:80", "10.0.0.1"), None);
    }

    #[tokio::test]
    async fn concurrent_changes_at_one_version_converge() {
        let mut a = named_services(&["from-a"]);
        a.version = 5;
        a.changed_by = "node-a".into();
        let b = DynamicConfig { service_groups: named_services(&["from-b"]).service_groups, changed_by: "node-b".into(), ..a.clone() };
        // Two nodes that got the two changes in opposite orders end up the same.
        for order in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let state = state_for("x");
            for config in order {
                apply_message(&state, Message::ConfigUpdate { config }, None).await;
            }
            assert_eq!(*state.dynamic_config.read().await, b);
        }
    }

    #[tokio::test]
    async fn admin_rpc_refuses_a_change_that_breaks_the_config() {
        use crate::state::test_support::{mesh_pair, tls_state, tls_state_with};
        let dir = std::env::temp_dir().join(format!("stouter_admin_{}_{}", std::process::id(), rand::random::<u64>()));
        let path = dir.join("node.json");
        let mut dc = DynamicConfig::default();
        let mut api: crate::config::Service =
            serde_json::from_value(serde_json::json!({"name": "api", "nodeId": "n1", "nodePort": 1, "protocol": "http"})).unwrap();
        dc.upsert_service("g", api.clone());
        dc.set_routes("api", vec![serde_json::from_value(serde_json::json!({"service": "api", "backends": [{}]})).unwrap()]).unwrap();
        let cfg = Config { node_id: "n1".into(), admin_cns: vec!["admin".into()], dynamic_config: dc, ..Config::default() };
        std::fs::create_dir_all(&dir).unwrap();
        cfg.save(path.to_str().unwrap()).unwrap();
        let node = tls_state_with(&dir, cfg, path.to_str().unwrap().into());
        let admin = tls_state(&dir, "admin");

        let rpc = |op: messages::AdminOp| {
            let (node, admin) = (node.clone(), admin.clone());
            async move {
                let (mut client, server) = mesh_pair(&admin, &node).await;
                tokio::spawn(handle_admin_request(server, op, node.clone()));
                match recv_message(&mut client).await.unwrap() {
                    Message::AdminResponse { result } => result,
                    other => panic!("{other:?}"),
                }
            }
        };

        // Removing the routed service would strand its routes: refused.
        let remove = messages::AdminOp::ServiceRemove { group: "g".into(), owner_node_id: "n1".into(), name: "api".into() };
        match rpc(remove).await {
            messages::AdminResult::Err { code, message } => {
                assert_eq!(code, messages::AdminErrCode::Validation);
                assert!(message.contains("unknown service 'api'"), "{message}");
            }
            ok => panic!("{ok:?}"),
        }
        assert_eq!(node.dynamic_config.read().await.version, 2, "nothing changed");

        // A harmless change goes through and is stamped with this node.
        api.weight = 3;
        match rpc(messages::AdminOp::service_add("g".into(), api)).await {
            messages::AdminResult::Ok { new_version } => assert_eq!(new_version, 3),
            err => panic!("{err:?}"),
        }
        assert_eq!(node.dynamic_config.read().await.changed_by, "n1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A node with `requireSignedConfig` (admin: "admin"), its config file,
    /// and TLS material for "admin" and "mallory" from the same CA.
    struct SignedCluster {
        dir: std::path::PathBuf,
        path: String,
        node: Arc<SharedState>,
        admin: crate::pki::PkiMaterial,
        mallory: crate::pki::PkiMaterial,
    }

    impl Drop for SignedCluster {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn signed_cluster() -> SignedCluster {
        let dir = std::env::temp_dir().join(format!("stouter_signed_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node.json").to_str().unwrap().to_string();
        let cfg = Config {
            node_id: "n1".into(),
            admin_cns: vec!["admin".into()],
            require_signed_config: true,
            dynamic_config: DynamicConfig { version: 1, ..named_services(&["api"]) },
            ..Config::default()
        };
        cfg.save(&path).unwrap();
        let node = crate::state::test_support::tls_state_with(&dir, cfg, path.clone());
        let material = |id: &str| crate::pki::load_or_create(dir.to_str().unwrap(), id).unwrap();
        SignedCluster { admin: material("admin"), mallory: material("mallory"), node, path, dir }
    }

    fn signed_by(m: &crate::pki::PkiMaterial, dc: &DynamicConfig) -> DynamicConfig {
        crate::signing::sign(dc, &crate::signing::Signer::from_material(m).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_signing_node_adopts_only_admin_signed_gossip() {
        let c = signed_cluster();
        let v2 = DynamicConfig { version: 2, ..named_services(&["api", "new"]) };
        for refused in [v2.clone(), signed_by(&c.mallory, &v2)] {
            apply_message(&c.node, Message::ConfigUpdate { config: refused }, None).await;
            assert_eq!(c.node.dynamic_config.read().await.version, 1);
        }
        apply_message(&c.node, Message::ConfigUpdate { config: signed_by(&c.admin, &v2) }, None).await;
        let dc = c.node.dynamic_config.read().await.clone();
        assert_eq!((dc.version, service_names(&dc)), (2, vec!["api".into(), "new".into()]));
        assert!(dc.signature.is_some(), "kept, to relay");
        // Persisted with its signature, so it survives a restart.
        assert_eq!(Config::load(&c.path).unwrap().dynamic_config, dc);
    }

    #[tokio::test]
    async fn a_signing_node_takes_only_signed_pushes_on_its_current_version() {
        use crate::state::test_support::{mesh_pair, tls_state};
        let c = signed_cluster();
        let admin_client = tls_state(&c.dir, "admin");
        let rpc = |op: messages::AdminOp| {
            let (node, admin) = (c.node.clone(), admin_client.clone());
            async move {
                let (mut client, server) = mesh_pair(&admin, &node).await;
                tokio::spawn(handle_admin_request(server, op, node.clone()));
                match recv_message(&mut client).await.unwrap() {
                    Message::AdminResponse { result: messages::AdminResult::Err { code, .. } } => Err(code),
                    Message::AdminResponse { result: messages::AdminResult::Ok { new_version } } => Ok(new_version),
                    other => panic!("{other:?}"),
                }
            }
        };
        use messages::{AdminErrCode, AdminOp};
        let remove = AdminOp::ServiceRemove { group: "g".into(), owner_node_id: "n1".into(), name: "api".into() };
        assert_eq!(rpc(remove).await, Err(AdminErrCode::Unauthorized), "an unsigned edit");

        let v2 = DynamicConfig { version: 2, changed_by: "admin".into(), ..named_services(&[]) };
        let push = |config, base_version| AdminOp::ConfigPush { config, base_version };
        assert_eq!(rpc(push(signed_by(&c.mallory, &v2), 1)).await, Err(AdminErrCode::Unauthorized));
        assert_eq!(rpc(push(signed_by(&c.admin, &v2), 0)).await, Err(AdminErrCode::Conflict), "made to an old version");
        assert_eq!(rpc(push(signed_by(&c.admin, &v2), 1)).await, Ok(2));
        let dc = c.node.dynamic_config.read().await.clone();
        assert_eq!(dc.version, 2);
        assert_eq!(dc.changed_by, "admin", "adopted exactly as signed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_signing_node_ignores_hand_edits_but_takes_signed_files() {
        let c = signed_cluster();
        let poll = |node: Arc<SharedState>| async move {
            let task = tokio::spawn(run_config_poll_loop(node));
            tokio::time::sleep(Duration::from_millis(100)).await;
            task.abort();
        };
        // A hand edit at a higher version: still unsigned, so ignored.
        let mut file = Config::load(&c.path).unwrap();
        file.dynamic_config = DynamicConfig { version: 5, ..named_services(&["hand-edited"]) };
        file.save(&c.path).unwrap();
        poll(c.node.clone()).await;
        assert_eq!(c.node.dynamic_config.read().await.version, 1);

        // What the CLI's local path writes: a signed config. Adopted.
        file.dynamic_config = signed_by(&c.admin, &DynamicConfig { version: 6, ..named_services(&["signed"]) });
        file.save(&c.path).unwrap();
        poll(c.node.clone()).await;
        let dc = c.node.dynamic_config.read().await.clone();
        assert_eq!((dc.version, service_names(&dc)), (6, vec!["signed".into()]));
    }

    /// A receiving state with TLS, and signers for "x" and "m", all from one CA.
    fn signed_nodes_setup() -> (std::path::PathBuf, Arc<SharedState>, crate::signing::Signer, crate::signing::Signer) {
        let dir = std::env::temp_dir().join(format!("stouter_records_{}_{}", std::process::id(), rand::random::<u64>()));
        let sub = crate::state::test_support::tls_state(&dir, "sub");
        let signer = |id: &str| {
            crate::signing::Signer::from_material(&crate::pki::load_or_create(dir.to_str().unwrap(), id).unwrap()).unwrap()
        };
        let (x, m) = (signer("x"), signer("m"));
        (dir, sub, x, m)
    }

    fn record(id: &str, addr: &str, heartbeat: u64, signer: &crate::signing::Signer) -> NodeInfo {
        let rec = crate::config::NodeRecord { id: id.into(), addr: addr.into(), heartbeat, ..Default::default() };
        let signed = crate::signing::sign_node(&rec, signer).unwrap();
        NodeInfo { id: id.into(), addr: addr.into(), heartbeat, signed: Some(signed), ..Default::default() }
    }

    #[tokio::test]
    async fn no_node_can_rewrite_what_another_signed_about_itself() {
        let (dir, sub, x, m) = signed_nodes_setup();
        let relayed = SyncSource { cn: Some("m"), ..Default::default() };
        let merge = |nodes: Vec<NodeInfo>| merge_sync(&sub, DynamicConfig::default(), nodes, &relayed);
        let x_now = || sub.known_nodes.load()["x"].clone();

        merge(vec![record("x", "x:1", 10, &x)]).await;
        assert_eq!(x_now().addr, "x:1", "x's own record, relayed by m, is believed");

        // m points x at itself, marks it down and fakes its heartbeat:
        // unsigned, signed by m, or x's signature over edited text.
        let unsigned = NodeInfo {
            id: "x".into(),
            addr: "m:1".into(),
            heartbeat: u64::MAX,
            health: ServiceHealth { version: u64::MAX, unhealthy: vec!["api".into()] },
            ..Default::default()
        };
        let mut by_m = record("x", "m:1", 20, &m);
        by_m.id = "x".into();
        let mut tampered = record("x", "x:1", 30, &x);
        let sig = tampered.signed.as_mut().unwrap();
        sig.record_json = sig.record_json.replace("x:1", "m:1");
        merge(vec![unsigned, by_m, tampered]).await;
        let after = x_now();
        assert_eq!((after.addr.as_str(), after.heartbeat), ("x:1", 10));
        assert!(after.health.unhealthy.is_empty());

        // x's own newer record is taken; an old one replayed is not.
        merge(vec![record("x", "x:2", 11, &x)]).await;
        merge(vec![record("x", "x:1", 10, &x)]).await;
        assert_eq!(x_now().addr, "x:2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn signing_nodes_sign_their_own_record() {
        let (dir, sub, _, _) = signed_nodes_setup();
        let me = snapshot_nodes(&sub).into_iter().find(|n| n.id == "sub").unwrap();
        let rec = crate::signing::verify_node(me.signed.as_ref().unwrap(), sub.tls.as_deref().unwrap()).unwrap();
        assert_eq!((rec.id.as_str(), rec.heartbeat), ("sub", me.heartbeat));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn signing_clusters_ignore_unsigned_gossip_entirely() {
        let c = signed_cluster();
        let old = NodeInfo { id: "old".into(), addr: "old:1".into(), ..Default::default() };
        merge_sync(&c.node, DynamicConfig::default(), vec![old], &SyncSource::default()).await;
        assert!(!c.node.known_nodes.load().contains_key("old"));
    }

    #[tokio::test]
    async fn forgotten_nodes_are_retried_until_rediscovered() {
        let state = state_for("sub");
        let dead = NodeInfo { id: "dead".into(), addr: "dead:1".into(), heartbeat: 5, ..Default::default() };
        merge_sync(&state, DynamicConfig::default(), vec![dead.clone()], &SyncSource::default()).await;
        age(&state, "dead", NODE_EXPIRE_AFTER + Duration::from_secs(1)).await;
        expire_silent_nodes(&state).await;
        assert_eq!(state.forgotten_addrs(), ["dead:1"]);

        // It answers again (a newer heartbeat): known again, no longer retried.
        merge_sync(&state, DynamicConfig::default(), vec![NodeInfo { heartbeat: 6, ..dead }], &SyncSource::default()).await;
        assert!(state.known_nodes.load().contains_key("dead"));
        assert!(state.forgotten_addrs().is_empty());

        // A node that left on purpose isn't retried.
        apply_message(&state, Message::NodeLeave { id: "dead".into(), heartbeat: 7 }, Some("dead")).await;
        assert!(state.forgotten_addrs().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_sync_loop_retries_forgotten_addresses_about_once_a_minute() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = state_for("sub");
        state.remember_forgotten("gone", &addr);
        let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = dials.clone();
        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let sync = tokio::spawn(run_sync_loop(state.clone()));
        tokio::time::sleep(Duration::from_secs(35)).await;
        assert_eq!(dials.load(std::sync::atomic::Ordering::SeqCst), 0, "not every round");
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(dials.load(std::sync::atomic::Ordering::SeqCst) >= 1, "retried within about a minute");
        sync.abort();
    }
}
