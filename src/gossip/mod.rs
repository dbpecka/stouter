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

/// Largest message sent or accepted: room for a large signed config and
/// thousands of signed node records. (A config alone is capped well below
/// this; see `config::MAX_CONFIG_BYTES`.)
const MAX_MSG_BYTES: u32 = 8 * 1024 * 1024;

/// Warn about an oversized message at most this often: a peer that sends
/// one does so every sync round.
const SIZE_WARNING_EVERY: Duration = Duration::from_secs(60);

/// Log `message` at warn level, at most once per [`SIZE_WARNING_EVERY`].
fn warn_message_size(message: std::fmt::Arguments<'_>) {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    warn_rarely(&LAST, message);
}

/// Log `message` at warn level if `last` (one per kind of warning) is
/// over a minute ago, else at debug: for conditions that repeat every round.
fn warn_rarely(last: &std::sync::Mutex<Option<std::time::Instant>>, message: std::fmt::Arguments<'_>) {
    let mut last = last.lock().unwrap();
    if last.is_none_or(|at| at.elapsed() >= SIZE_WARNING_EVERY) {
        *last = Some(std::time::Instant::now());
        warn!("{message}");
    } else {
        debug!("{message}");
    }
}

/// Most connections one peer may hold here waiting for their first
/// message. Subscribers keep a few warm (8 per node), so this only stops a
/// peer piling up idle connections until we run out of file descriptors.
const MAX_IDLE_PER_PEER: usize = 64;

/// Most mux sessions one peer may hold here (subscribers keep one).
const MAX_MUX_SESSIONS_PER_PEER: usize = 16;

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
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    if len > MAX_MSG_BYTES {
        // The receiver would refuse it; say so here, where it can be fixed.
        warn_message_size(format_args!(
            "not sending a {len}-byte message: over the {MAX_MSG_BYTES}-byte limit (a very large config or cluster?)"
        ));
        bail!("message too large to send: {len} bytes");
    }
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
        warn_message_size(format_args!(
            "refusing a {len}-byte message: over the {MAX_MSG_BYTES}-byte limit"
        ));
        bail!("message too large: {len} bytes");
    }

    // Grow the buffer as data arrives rather than trusting the length
    // prefix: a peer can't make us allocate a whole message it never sends.
    let mut payload = Vec::with_capacity((len as usize).min(64 * 1024));
    (&mut *stream)
        .take(u64::from(len))
        .read_to_end(&mut payload)
        .await
        .context("read payload")?;
    if payload.len() != len as usize {
        bail!("read payload: connection closed after {} of {len} bytes", payload.len());
    }

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
    static LAST_LIMIT_WARNING: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let peer = stream.peer_cn().unwrap_or_default().to_string();
    let Some(idle) = state.try_peer_slot("idle", &peer, MAX_IDLE_PER_PEER) else {
        warn_rarely(
            &LAST_LIMIT_WARNING,
            format_args!("closing a connection from '{peer}' ({peer_addr}): it already holds {MAX_IDLE_PER_PEER} idle ones"),
        );
        return;
    };
    let first = tokio::select! {
        result = recv_message(&mut stream) => result,
        _ = state.shutdown.cancelled() => return,
    };
    drop(idle);
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
            let Some(_session) = state.try_peer_slot("mux", &peer, MAX_MUX_SESSIONS_PER_PEER) else {
                warn_rarely(
                    &LAST_LIMIT_WARNING,
                    format_args!("refusing a mux session from '{peer}': it already holds {MAX_MUX_SESSIONS_PER_PEER}"),
                );
                return;
            };
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
        Message::Sync { config, nodes, beats, have, config_elided } => {
            // No `contact_addr`: an inbound connection arrives from an
            // ephemeral source port, which won't match the peer's advertised
            // address, so it can't be used as a liveness signal here. The
            // outbound sync loop confirms every peer independently.
            let source = SyncSource {
                cn: stream.peer_cn(),
                host: host_of(&peer_addr),
                contact_addr: None,
            };
            let config = (!config_elided).then_some(config);
            merge_sync(&state, config, nodes, beats, &source).await;

            // Reply with what the peer lacks (or, to an older peer that
            // sends no digest, with everything).
            let reply = match &have {
                Some(have) => build_sync_reply(&state, have).await,
                None => build_sync_msg(&state, false).await,
            };
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
    let mut addrs: Vec<String> = Vec::new();
    let members = state.known_nodes.load();
    let candidates = state.bootstrap_peers.iter().chain(members.values().map(|n| &n.addr));
    for addr in candidates {
        if dialable(state, addr) && !addrs.contains(addr) {
            addrs.push(addr.clone());
        }
    }
    addrs
}

/// Whether `addr` is somewhere to sync with: not empty, not a wildcard
/// (which says where a node listens, not where to reach it), not us.
fn dialable(state: &SharedState, addr: &str) -> bool {
    let own = [state.config.peer_addr(), state.config.bind.as_str(), &state.config.local_target()];
    !addr.is_empty() && !is_wildcard(addr) && !own.contains(&addr)
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
                if dialable(&state, &addr) && !peers.contains(&addr) {
                    peers.push(addr);
                }
            }
        }
        // Every peer would send the same beats: ask only a few of them.
        let beat_sources: std::collections::HashSet<&String> = {
            use rand::seq::IndexedRandom;
            peers.choose_multiple(&mut rand::rng(), BEAT_SOURCES).collect()
        };
        futures::future::join_all(peers.iter().map(|addr| {
            let state = &state;
            let want_beats = beat_sources.contains(addr);
            async move {
                let start = std::time::Instant::now();
                match time::timeout(Duration::from_secs(10), sync_with_peer(state, addr, want_beats)).await {
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

/// Peers asked for beats each round (see `SyncDigest::want_beats`): enough
/// that one not answering doesn't leave us without, few enough that a beat
/// reaches each node about twice a round rather than once per peer.
const BEAT_SOURCES: usize = 2;

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

async fn sync_with_peer(state: &Arc<SharedState>, addr: &str, want_beats: bool) -> Result<()> {
    let mut stream = state.connect_mesh(addr, None).await?;

    let our_sync = sync_msg_for(state, addr, want_beats).await;
    send_message(&mut stream, &our_sync).await?;

    let reply = recv_message(&mut stream).await?;
    if let Message::Sync { config, nodes, beats, have, config_elided } = reply {
        // A digest in the reply means the peer speaks delta sync: from now
        // on, send it compact syncs. (An older peer never sends one.)
        if have.is_some() {
            state.delta_peers.lock().unwrap().insert(addr.to_string());
        } else {
            state.delta_peers.lock().unwrap().remove(addr);
        }
        // A successful outbound sync is direct evidence the peer at `addr` is
        // reachable, so pass it as the liveness `contact_addr`.
        let source = SyncSource { cn: stream.peer_cn(), host: host_of(addr), contact_addr: Some(addr) };
        merge_sync(state, (!config_elided).then_some(config), nodes, beats, &source).await;
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
pub(crate) fn is_wildcard(addr: &str) -> bool {
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
    // What changed is in our own record, so a compact sync carries it to
    // peers that speak delta sync; older ones get the full one.
    let (full, compact) = (build_sync_msg(state, false).await, build_compact_sync(state, false).await);
    let peers = peer_addrs(state).await;
    let delta_peers = state.delta_peers.lock().unwrap().clone();
    futures::future::join_all(peers.iter().map(|addr| {
        let msg = if delta_peers.contains(addr) { &compact } else { &full };
        async move {
            match time::timeout(Duration::from_secs(5), send_to_peer(addr, msg, state)).await {
                Ok(Err(e)) => debug!("broadcast to {addr} failed: {e}"),
                Err(_) => debug!("broadcast to {addr} timed out"),
                Ok(Ok(())) => {}
            }
        }
    }))
    .await;
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

/// Persist `dc` to the config file.
///
/// Loads the current file first so that external changes to static fields
/// (e.g. `bind`) are preserved; its `knownNodes` (bootstrap peers, not the
/// gossip-discovered members) is left untouched. Callers hold the
/// `state.dynamic_config` lock (write, for a change), so the file is written
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
    config: Option<crate::config::DynamicConfig>,
    nodes: Vec<NodeInfo>,
    beats: Vec<crate::config::SignedBeat>,
    source: &SyncSource<'_>,
) {
    // `None`: the sender left its config out, as no newer than ours.
    let updated = match config {
        Some(config) => {
            let mut dc = state.dynamic_config.write().await;
            adopt_if_newer(state, &mut dc, config, "dynamic config updated")
        }
        None => false,
    };

    if updated {
        state.rebuild_service_index().await;
        print_dynamic_config(state).await;
    }

    let own_id = state.config.node_id.as_str();
    let strict = state.config.require_signed_config;
    let now = Instant::now();
    // Node info isn't persisted (the config file only holds bootstrap
    // peers), so a change here writes nothing to disk.
    state.update_known_nodes(|kn| {
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
                // heartbeat newer than the one it departed with, or by
                // talking to us itself.
                if state.is_buried(&node.id, node.heartbeat) && source.cn != Some(node.id.as_str()) {
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
        for beat in beats {
            if beat.id != own_id {
                merge_beat(kn, beat, now);
            }
        }
        // A sync from the node itself (its certificate says so) means it is
        // alive, even if we can't reach it. Its record no longer changes
        // every round, so without this a node that can reach us but not the
        // other way round would be forgotten. Only reaching its address
        // makes it routable, below.
        if let Some(node) = source.cn.and_then(|cn| kn.get_mut(cn)) {
            node.heard_at = Some(now);
        }
        // Refresh the liveness marker for the peer we just synced with
        // directly: the node at that address, if it's the one whose
        // certificate answered. (Another node now at a dead node's old
        // address must not keep it alive.)
        if let Some(addr) = source.contact_addr {
            for node in kn.values_mut() {
                if !node.addr.is_empty() && node.addr == addr && source.cn.is_none_or(|cn| cn == node.id) {
                    node.last_seen = Some(now);
                    node.heard_at = Some(now);
                }
            }
        }
        changed
    }).await;
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
    let held = kn.get(&node.id).filter(|n| n.signed.is_some()).map(|n| n.record_version);
    // A record that says only "wherever you reached me" (a wildcard): where
    // to reach the node can improve without its record changing, so check
    // before skipping a record we already hold.
    let mut addr_changed = false;
    if let Some(existing) = kn.get_mut(&node.id).filter(|n| n.signed.is_some()) {
        let better = if source.cn == Some(node.id.as_str()) && is_wildcard(&node.addr) {
            // From the node itself: where we reached it, or it reached us.
            source.host.and_then(|host| rewrite_wildcard(&node.addr, host))
        } else if is_wildcard(&existing.addr) {
            relayed_address(&node.addr, &existing.addr)
        } else {
            None
        };
        if let Some(addr) = better.filter(|addr| *addr != existing.addr) {
            info!("node {} address updated: {} -> {addr}", node.id, existing.addr);
            existing.addr = addr;
            addr_changed = true;
        }
    }
    match crate::signing::claimed_heartbeat(&signed) {
        Some(claimed) if held.is_none_or(|h| claimed > h) => {}
        _ => return addr_changed,
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

    // Its last record: the node left. Forget it, and keep the record to
    // pass on (see `build_sync_reply`), so the departure reaches nodes the
    // leave message didn't, NAT'd ones included.
    if record.left {
        let last = kn.remove(&record.id).map_or(0, |n| n.heartbeat);
        if last > 0 {
            info!("node left: {}", record.id);
        }
        state.bury_with(&record.id, last.max(record.heartbeat), Some((record.heartbeat, signed)));
        return true;
    }

    // A wildcard address means "wherever you reached me": the node itself
    // tells us best. Relayed, keep an address we already have, or else take
    // the one the relaying node reaches it at (see `relayed_address`).
    let mut addr = record.addr.clone();
    if is_wildcard(&addr) {
        if source.cn == Some(record.id.as_str()) {
            if let Some(rewritten) = source.host.and_then(|host| rewrite_wildcard(&addr, host)) {
                addr = rewritten;
            }
        } else if let Some(known) = kn.get(&record.id).filter(|n| !n.addr.is_empty() && !is_wildcard(&n.addr)) {
            addr = known.addr.clone();
        } else if let Some(relayed) = relayed_address(&node.addr, &record.addr) {
            addr = relayed;
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
            existing.heartbeat = existing.heartbeat.max(record.heartbeat);
            existing.record_version = record.heartbeat;
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
            // newer record than the one held when it departed, or by talking
            // to us itself: its record changes only when its address, health
            // or relays do, so one forgotten while cut off may well send the
            // same record once the network heals. (One that left sent a
            // final record saying so, handled above.)
            if state.is_buried(&record.id, record.heartbeat) && source.cn != Some(record.id.as_str()) {
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
                    record_version: record.heartbeat,
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

/// For a node whose signed address is the wildcard `signed`, the address a
/// relaying node gave with its record (`envelope`: where that node reaches
/// it), if it's a real one on the same port. Without it, a node learning of
/// a wildcard-bound node through others would have nowhere to reach it.
///
/// The relaying node could name the wrong host, but no more: a tunnel or
/// sync there must be answered by the node's own certificate, and only
/// that counts it as live. The node's own word replaces it once it syncs.
fn relayed_address(envelope: &str, signed: &str) -> Option<String> {
    let port = |addr: &str| addr.rsplit_once(':').map(|(_, port)| port.to_string());
    (!envelope.is_empty() && !is_wildcard(envelope) && port(envelope).is_some() && port(envelope) == port(signed))
        .then(|| envelope.to_string())
}

/// Take `beat` for the node it names if it's newer than what we have and
/// signed with the key of that node's (already verified) record. A rising
/// beat is the only sign of life from a node with no address.
fn merge_beat(kn: &mut std::collections::HashMap<String, NodeInfo>, beat: crate::config::SignedBeat, now: Instant) {
    let Some(node) = kn.get_mut(&beat.id) else { return };
    if beat.beat <= node.heartbeat {
        return;
    }
    let Some(record) = &node.signed else { return };
    if let Err(e) = crate::signing::verify_beat(&beat, &record.cert) {
        debug!("ignoring a beat for node {}: {e:#}", beat.id);
        return;
    }
    node.heartbeat = beat.beat;
    node.heard_at = Some(now);
    if node.addr.is_empty() {
        node.last_seen = Some(now);
    }
    node.beat = Some(beat);
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
            state.update_known_nodes(|kn| {
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
        }
        Message::NodeLeave { id, heartbeat, record } => {
            if peer_cn != Some(id.as_str()) {
                warn!("ignoring NodeLeave for {id} sent by {}", peer_cn.unwrap_or("an unnamed peer"));
                return;
            }
            // Its signed "left" record, kept to relay to nodes this message
            // can't reach (it goes only to nodes with an address).
            let left = record.and_then(|signed| {
                let tls = state.tls.as_deref()?;
                let verified = crate::signing::verify_node(&signed, tls).ok()?;
                (verified.left && verified.id == id).then_some((verified.heartbeat, signed))
            });
            state.update_known_nodes(|kn| {
                let last = kn.remove(&id).map(|n| n.heartbeat);
                if last.is_some() {
                    info!("node left: {id}");
                }
                // Bury it so gossip still carrying it doesn't re-add it; a
                // restart sends a newer heartbeat and is let back in.
                let version = left.as_ref().map_or(0, |(v, _)| *v);
                state.bury_with(&id, heartbeat.max(last.unwrap_or(0)).max(version), left);
            }).await;
        }
        Message::Sync { config, nodes, beats, config_elided, .. } => {
            let source = SyncSource { cn: peer_cn, ..Default::default() };
            merge_sync(state, (!config_elided).then_some(config), nodes, beats, &source).await;
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

/// A full Sync: our whole config and every node we know, as older nodes
/// expect. Also carries our digest (so a newer peer can reply with just a
/// delta, beats included if `want_beats`) and our own beat.
///
/// Always includes a `NodeInfo` for the sender itself so receivers can
/// populate the `hostname → address` mapping that `manage_proxies` requires
/// to match services (which store `node_id = hostname`) to connectable peers.
async fn build_sync_msg(state: &Arc<SharedState>, want_beats: bool) -> Message {
    let config = state.dynamic_config.read().await.clone();
    let have = Some(digest(state, &config, want_beats));
    let nodes = snapshot_nodes(state);
    Message::Sync { config, nodes, beats: own_beat(state).into_iter().collect(), have, config_elided: false }
}

/// A compact Sync for a peer that speaks delta sync: our own record and
/// beat, and our digest. The peer's reply fills in whatever we're missing; it
/// learns what we have that it lacks when it syncs with us in turn. (A peer
/// with an address hears every NAT'd node's beat from the node itself.)
async fn build_compact_sync(state: &Arc<SharedState>, want_beats: bool) -> Message {
    let config = state.dynamic_config.read().await.clone();
    let have = Some(digest(state, &config, want_beats));
    let nodes = own_entry(state).into_iter().collect();
    Message::Sync {
        config: placeholder_config(&config),
        nodes,
        beats: own_beat(state).into_iter().collect(),
        have,
        config_elided: true,
    }
}

/// The Sync to send `addr`: compact if it speaks delta sync, else full.
async fn sync_msg_for(state: &Arc<SharedState>, addr: &str, want_beats: bool) -> Message {
    if state.delta_peers.lock().unwrap().contains(addr) {
        build_compact_sync(state, want_beats).await
    } else {
        build_sync_msg(state, want_beats).await
    }
}

/// Our reply to a Sync whose sender holds `have`: only the records it
/// lacks or holds older (departed nodes' "left" records included), beats,
/// and our config only if it's newer than the sender's.
async fn build_sync_reply(state: &Arc<SharedState>, have: &messages::SyncDigest) -> Message {
    let config = state.dynamic_config.read().await.clone();
    let held: std::collections::HashMap<&str, u64> = have.records.iter().map(|(id, v)| (id.as_str(), *v)).collect();
    let newer = |id: &str, version: u64| held.get(id).is_none_or(|v| version > *v);
    let mut nodes: Vec<NodeInfo> = snapshot_nodes(state)
        .into_iter()
        .filter(|n| match &n.signed {
            Some(_) => newer(&n.id, n.record_version),
            // Unsigned (older) nodes carry no version: always include them.
            None => true,
        })
        .collect();
    for (id, version, record) in state.departed_records() {
        if held.contains_key(id.as_str()) && newer(&id, version) {
            nodes.push(NodeInfo { id, signed: Some(record), ..Default::default() });
        }
    }
    // Ours is newer if its version is, or at one version if it differs (the
    // sender then settles it with `supersedes`).
    let ours_newer = config.version > have.config_version
        || (config.version == have.config_version && config_hash(&config) != have.config_hash);
    let (config, config_elided) = if ours_newer { (config, false) } else { (placeholder_config(&config), true) };
    // Beats only if asked, and only ones newer than the sender's.
    let beats = if have.want_beats {
        let held: std::collections::HashMap<&str, u64> = have.beats.iter().map(|(id, b)| (id.as_str(), *b)).collect();
        beats_to_relay(state).into_iter().filter(|b| held.get(b.id.as_str()).is_none_or(|h| b.beat > *h)).collect()
    } else {
        Vec::new()
    };
    Message::Sync {
        config,
        nodes,
        beats,
        // Our digest isn't needed (the sender learns from what's here), but
        // its presence tells the sender we speak delta sync.
        have: Some(messages::SyncDigest::default()),
        config_elided,
    }
}

/// A stand-in for an elided config: right version, nothing else. Only ever
/// sent to nodes that know to ignore it (`config_elided`).
fn placeholder_config(config: &crate::config::DynamicConfig) -> crate::config::DynamicConfig {
    crate::config::DynamicConfig { version: config.version, ..Default::default() }
}

/// What we hold: every signed node record's version (ours and departed
/// nodes' included) and our config's version and hash.
fn digest(state: &SharedState, config: &crate::config::DynamicConfig, want_beats: bool) -> messages::SyncDigest {
    let mut records: Vec<(String, u64)> = state
        .known_nodes
        .load()
        .values()
        .filter(|n| n.signed.is_some())
        .map(|n| (n.id.clone(), n.record_version))
        .collect();
    records.extend(state.departed_records().into_iter().map(|(id, v, _)| (id, v)));
    if let Some(own) = state.own_record.lock().unwrap().as_ref() {
        records.push((own.record.id.clone(), own.record.heartbeat));
    }
    // The beat held for each node with no address (its heartbeat is its
    // latest beat), so a peer sends only newer ones.
    let beats = if want_beats {
        state
            .known_nodes
            .load()
            .values()
            .filter(|n| n.addr.is_empty() && n.signed.is_some())
            .map(|n| (n.id.clone(), n.heartbeat))
            .collect()
    } else {
        Vec::new()
    };
    messages::SyncDigest { records, config_version: config.version, config_hash: config_hash(config), want_beats, beats }
}

/// FNV-1a over the config's JSON: stable across processes, enough to tell
/// two configs at one version apart.
fn config_hash(config: &crate::config::DynamicConfig) -> u64 {
    let bytes = serde_json::to_vec(config).unwrap_or_default();
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// Beats to pass on: ours (if we have no address) and the latest one held
/// for each node with no address.
fn beats_to_relay(state: &SharedState) -> Vec<crate::config::SignedBeat> {
    let mut beats: Vec<crate::config::SignedBeat> =
        state.known_nodes.load().values().filter(|n| n.addr.is_empty()).filter_map(|n| n.beat.clone()).collect();
    beats.extend(own_beat(state));
    beats
}

/// How often a node with no address issues a new beat. Its peers count it
/// alive while beats keep rising within `NODE_STALE_AFTER` (30s).
const BEAT_EVERY: Duration = Duration::from_secs(5);

/// This node's current beat, if it has no address (others reach it only
/// through relays, so they can't see it's alive by contacting it).
fn own_beat(state: &SharedState) -> Option<crate::config::SignedBeat> {
    if !state.config.outbound_only {
        return None;
    }
    let signer = state.signer.get()?;
    let mut cache = state.own_beat.lock().unwrap();
    if let Some((beat, at)) = cache.as_ref() {
        if at.elapsed() < BEAT_EVERY {
            return Some(beat.clone());
        }
    }
    let beat = match crate::signing::sign_beat(&state.config.node_id, state.next_heartbeat(), signer) {
        Ok(beat) => beat,
        Err(e) => {
            warn!("cannot sign this node's beat: {e:#}");
            return None;
        }
    };
    *cache = Some((beat.clone(), std::time::Instant::now()));
    Some(beat)
}

/// What this node says about itself (at `addr`), signed so no other node
/// can change it on the way: peers believe it only with our signature. A
/// new version only when something in it changes (health, relays, address,
/// leaving), so peers that have it are spared resending and re-checking it;
/// liveness travels separately (direct contact, or beats).
fn own_record(state: &SharedState, addr: String) -> (crate::config::NodeRecord, Option<crate::config::SignedNode>) {
    let health = (**state.local_health.load()).clone();
    let relays = if state.config.outbound_only { state.current_relays() } else { Vec::new() };
    let left = state.leaving.load(std::sync::atomic::Ordering::SeqCst);
    let mut cache = state.own_record.lock().unwrap();
    if let Some(own) = cache.as_ref() {
        let r = &own.record;
        if r.addr == addr && r.health == health && r.relays == relays && r.left == left {
            return (own.record.clone(), own.signed.clone());
        }
    }
    let record = crate::config::NodeRecord {
        id: state.config.node_id.clone(),
        addr,
        heartbeat: state.next_heartbeat(),
        health,
        relays,
        left,
    };
    let signed = state.signer.get().and_then(|signer| match crate::signing::sign_node(&record, signer) {
        Ok(signed) => Some(signed),
        Err(e) => {
            warn!("cannot sign this node's record: {e:#}");
            None
        }
    });
    *cache = Some(crate::state::OwnRecord { record: record.clone(), signed: signed.clone() });
    (record, signed)
}

/// Mark this node as leaving and return its final record's version and
/// signature (a record saying it left), for the leave message.
pub(crate) fn final_record(state: &SharedState) -> (u64, Option<crate::config::SignedNode>) {
    state.leaving.store(true, std::sync::atomic::Ordering::SeqCst);
    match own_entry(state) {
        Some(own) => (own.record_version, own.signed),
        None => (state.next_heartbeat(), None),
    }
}

/// Our own entry as gossiped (`None` without a node id).
fn own_entry(state: &SharedState) -> Option<NodeInfo> {
    if state.config.node_id.is_empty() {
        return None;
    }
    let self_addr = if state.config.outbound_only { String::new() } else { state.config.peer_addr().to_owned() };
    let (record, signed) = own_record(state, self_addr);
    Some(NodeInfo {
        id: record.id,
        addr: record.addr,
        health: record.health,
        heartbeat: record.heartbeat,
        record_version: record.heartbeat,
        signed,
        ..Default::default()
    })
}

/// Known nodes plus an up-to-date entry for ourselves (address and health).
fn snapshot_nodes(state: &SharedState) -> Vec<NodeInfo> {
    let mut nodes: Vec<NodeInfo> = state.known_nodes.load().values().cloned().collect();
    if let Some(own) = own_entry(state) {
        nodes.retain(|n| n.id != own.id);
        nodes.push(own);
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

        merge_sync(&state, Some(config.clone()), vec![node("a", 1, &[]), node("me", 1, &["x"])], vec![], &SyncSource::default()).await;
        merge_sync(&state, Some(config), vec![node("a", 2, &["api"]), node("me", 9, &["x"])], vec![], &SyncSource::default()).await;

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
        merge_sync(&state, Some(dc.clone()), vec![natd("natd", 100)], vec![], &SyncSource { contact_addr: Some("relay:1"), ..Default::default() }).await;
        age(&state, "natd", Duration::from_secs(31)).await;

        // The same heartbeat relayed again is no sign of life.
        merge_sync(&state, Some(dc.clone()), vec![natd("natd", 100)], vec![], &SyncSource { contact_addr: Some("relay:1"), ..Default::default() }).await;
        assert!(!crate::state::node_is_live(&state.known_nodes.load()["natd"], Instant::now()));

        merge_sync(&state, Some(dc), vec![natd("natd", 101)], vec![], &SyncSource { contact_addr: Some("relay:1"), ..Default::default() }).await;
        let node = state.known_nodes.load()["natd"].clone();
        assert!(crate::state::node_is_live(&node, Instant::now()));
        assert_eq!(node.heartbeat, 101);
    }

    #[tokio::test]
    async fn silent_nodes_are_forgotten_and_stale_gossip_cannot_revive_them() {
        let state = state_for("sub");
        let dc = DynamicConfig::default();
        let direct = NodeInfo { id: "dead".into(), addr: "dead:1".into(), heartbeat: 50, ..Default::default() };
        merge_sync(&state, Some(dc.clone()), vec![direct.clone(), natd("alive", 7)], vec![], &SyncSource::default()).await;
        age(&state, "dead", NODE_EXPIRE_AFTER + Duration::from_secs(1)).await;
        expire_silent_nodes(&state).await;
        let kn = state.known_nodes.load();
        assert!(!kn.contains_key("dead"));
        assert!(kn.contains_key("alive"), "recently heard from");

        // A peer that hasn't noticed relays its old copy: still gone.
        merge_sync(&state, Some(dc.clone()), vec![direct.clone()], vec![], &SyncSource::default()).await;
        assert!(!state.known_nodes.load().contains_key("dead"));
        // A restarted node gossips a newer heartbeat: welcome back.
        merge_sync(&state, Some(dc), vec![NodeInfo { heartbeat: 51, ..direct }], vec![], &SyncSource::default()).await;
        assert!(state.known_nodes.load().contains_key("dead"));
    }

    #[tokio::test]
    async fn old_natd_nodes_without_heartbeats_are_kept() {
        // An older NAT'd node sends no heartbeat and can't be contacted, so
        // silence proves nothing about it.
        let state = state_for("sub");
        merge_sync(&state, Some(DynamicConfig::default()), vec![natd("old", 0)], vec![], &SyncSource::default()).await;
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

        apply_message(&state, Message::NodeLeave { id: "a".into(), heartbeat: 9, record: None }, Some("mallory")).await;
        assert!(state.known_nodes.load().contains_key("a"), "leave for someone else");
        apply_message(&state, Message::NodeLeave { id: "a".into(), heartbeat: 9, record: None }, Some("a")).await;
        assert!(!state.known_nodes.load().contains_key("a"));

        // Gossip still carrying its pre-leave heartbeat doesn't re-add it.
        let relayed = NodeInfo { id: "a".into(), addr: "a:1".into(), heartbeat: 9, ..Default::default() };
        merge_sync(&state, Some(DynamicConfig::default()), vec![relayed], vec![], &SyncSource::default()).await;
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
        merge_sync(&state, Some(DynamicConfig::default()), nodes, vec![], &source).await;
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
        let merge = |nodes: Vec<NodeInfo>| merge_sync(&sub, Some(DynamicConfig::default()), nodes, vec![], &relayed);
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
        merge_sync(&c.node, Some(DynamicConfig::default()), vec![old], vec![], &SyncSource::default()).await;
        assert!(!c.node.known_nodes.load().contains_key("old"));
    }

    #[tokio::test]
    async fn forgotten_nodes_are_retried_until_rediscovered() {
        let state = state_for("sub");
        let dead = NodeInfo { id: "dead".into(), addr: "dead:1".into(), heartbeat: 5, ..Default::default() };
        merge_sync(&state, Some(DynamicConfig::default()), vec![dead.clone()], vec![], &SyncSource::default()).await;
        age(&state, "dead", NODE_EXPIRE_AFTER + Duration::from_secs(1)).await;
        expire_silent_nodes(&state).await;
        assert_eq!(state.forgotten_addrs(), ["dead:1"]);

        // It answers again (a newer heartbeat): known again, no longer retried.
        merge_sync(&state, Some(DynamicConfig::default()), vec![NodeInfo { heartbeat: 6, ..dead }], vec![], &SyncSource::default()).await;
        assert!(state.known_nodes.load().contains_key("dead"));
        assert!(state.forgotten_addrs().is_empty());

        // A node that left on purpose isn't retried.
        apply_message(&state, Message::NodeLeave { id: "dead".into(), heartbeat: 7, record: None }, Some("dead")).await;
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

    #[tokio::test]
    async fn oversized_and_truncated_messages_are_refused() {
        // Too big to send.
        let mut dc = named_services(&["api"]);
        dc.service_groups[0].services[0].meta.insert("blob".into(), "x".repeat(MAX_MSG_BYTES as usize));
        let (mut a, _b) = tokio::io::duplex(1 << 16);
        let err = send_message(&mut a, &Message::ConfigUpdate { config: dc }).await.unwrap_err();
        assert!(err.to_string().contains("too large to send"), "{err}");

        // A length prefix over the limit, and one the peer never delivers.
        for (len, sent, expect) in [(MAX_MSG_BYTES + 1, 0, "too large"), (1000, 10, "closed after 10 of 1000")] {
            let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
            tx.write_all(&len.to_be_bytes()).await.unwrap();
            tx.write_all(&vec![0u8; sent]).await.unwrap();
            drop(tx);
            let err = format!("{:#}", recv_message(&mut rx).await.unwrap_err());
            assert!(err.contains(expect), "{err}");
        }
    }

    #[tokio::test]
    async fn one_peer_cannot_pile_up_idle_connections() {
        use crate::state::test_support::{mesh_pair, tls_state};
        let dir = std::env::temp_dir().join(format!("stouter_idle_{}_{}", std::process::id(), rand::random::<u64>()));
        let node = tls_state(&dir, "node");
        let peer = tls_state(&dir, "peer");
        let other = tls_state(&dir, "other");
        let open = || async {
            let (client, server) = mesh_pair(&peer, &node).await;
            (client, tokio::spawn(dispatch_connection(server, "peer".into(), node.clone())))
        };
        let mut held = Vec::new();
        for _ in 0..MAX_IDLE_PER_PEER {
            held.push(open().await);
        }
        tokio::task::yield_now().await;
        let (_extra_client, extra) = open().await;
        tokio::time::timeout(Duration::from_secs(5), extra).await.expect("closed at once").unwrap();
        assert!(held.iter().all(|(_, task)| !task.is_finished()), "the first ones still wait");

        // Another peer isn't affected, and closing one frees a place.
        let (_c, other_task) = {
            let (client, server) = mesh_pair(&other, &node).await;
            (client, tokio::spawn(dispatch_connection(server, "other".into(), node.clone())))
        };
        let (client, task) = held.pop().unwrap();
        drop(client);
        task.await.unwrap();
        let (_again_client, again) = open().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!again.is_finished() && !other_task.is_finished());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn digest_of(records: &[(&str, u64)], config: &DynamicConfig) -> messages::SyncDigest {
        messages::SyncDigest {
            records: records.iter().map(|(id, v)| (id.to_string(), *v)).collect(),
            config_version: config.version,
            config_hash: config_hash(config),
            ..Default::default()
        }
    }

    fn reply_ids(reply: &Message) -> (Vec<String>, bool) {
        let Message::Sync { nodes, config_elided, .. } = reply else { panic!("{reply:?}") };
        let mut ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        ids.sort();
        (ids, *config_elided)
    }

    #[tokio::test]
    async fn a_delta_reply_carries_only_what_the_initiator_lacks() {
        let (dir, sub, x, m) = signed_nodes_setup();
        let relayed = SyncSource::default();
        merge_sync(&sub, None, vec![record("x", "x:1", 10, &x), record("m", "m:1", 20, &m)], vec![], &relayed).await;
        sub.dynamic_config.write().await.version = 3;
        let config = sub.dynamic_config.read().await.clone();
        let own_version = own_entry(&sub).unwrap().record_version;

        // The initiator has x and us current, m at an older version.
        let have = digest_of(&[("x", 10), ("m", 19), ("sub", own_version)], &config);
        assert_eq!(reply_ids(&build_sync_reply(&sub, &have).await), (vec!["m".to_string()], true));

        // It has everything: nothing but the digest marker comes back.
        let have = digest_of(&[("x", 10), ("m", 20), ("sub", own_version)], &config);
        assert_eq!(reply_ids(&build_sync_reply(&sub, &have).await), (vec![], true));

        // Its config is older: ours goes along.
        let mut have = digest_of(&[("x", 10), ("m", 20), ("sub", own_version)], &config);
        have.config_version = 2;
        assert!(!reply_ids(&build_sync_reply(&sub, &have).await).1, "config included");

        // A compact sync is our own record, a digest and a placeholder config.
        let Message::Sync { nodes, have, config_elided, .. } = build_compact_sync(&sub, false).await else { unreachable!() };
        assert_eq!(nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["sub"]);
        assert!(have.unwrap().records.len() >= 3 && config_elided);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn beats_keep_a_natd_node_live_only_with_its_signature() {
        let (dir, sub, x, m) = signed_nodes_setup();
        let relayed = SyncSource::default();
        // x has no address: only beats tell us it's alive.
        merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![], &relayed).await;
        age(&sub, "x", Duration::from_secs(31)).await;
        let live = |sub: &SharedState| crate::state::node_is_live(&sub.known_nodes.load()["x"], Instant::now());
        assert!(!live(&sub));

        // A beat signed by someone else, or over a different value, is ignored.
        let forged = crate::signing::sign_beat("x", 50, &m).unwrap();
        let mut altered = crate::signing::sign_beat("x", 50, &x).unwrap();
        altered.beat = 60;
        merge_sync(&sub, None, vec![], vec![forged, altered], &relayed).await;
        assert!(!live(&sub));
        assert_eq!(sub.known_nodes.load()["x"].heartbeat, 10);

        merge_sync(&sub, None, vec![], vec![crate::signing::sign_beat("x", 50, &x).unwrap()], &relayed).await;
        assert!(live(&sub));
        assert_eq!(beats_to_relay(&sub).len(), 1, "and it is passed on");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn wildcard_bound_nodes_get_an_address_peers_can_reach() {
        let (dir, sub, x, _) = signed_nodes_setup();
        let a = crate::state::test_support::tls_state(&dir, "a");
        let addr_of = |state: &SharedState| state.known_nodes.load()["x"].addr.clone();
        // x binds 0.0.0.0:8080 (the default) with no advertiseAddr. a syncs
        // with x itself, so it knows where x is.
        let from_x = SyncSource { cn: Some("x"), host: Some("10.0.0.5"), contact_addr: None };
        merge_sync(&a, None, vec![record("x", "0.0.0.0:8080", 10, &x)], vec![], &from_x).await;
        assert_eq!(addr_of(&a), "10.0.0.5:8080");

        // `sub` hears of x only through a, and takes the address a reaches
        // it at, so it can sync with x and tunnel to it.
        let Message::Sync { nodes, .. } = build_sync_reply(&a, &messages::SyncDigest::default()).await else { unreachable!() };
        let from_a = SyncSource { cn: Some("a"), host: Some("10.0.0.1"), contact_addr: None };
        merge_sync(&sub, None, nodes, vec![], &from_a).await;
        assert_eq!(addr_of(&sub), "10.0.0.5:8080");
        assert!(peer_addrs(&sub).await.contains(&"10.0.0.5:8080".to_string()));

        // x itself has the last word, even with a record `sub` already has.
        let from_x_elsewhere = SyncSource { cn: Some("x"), host: Some("192.168.1.5"), contact_addr: None };
        merge_sync(&sub, None, vec![record("x", "0.0.0.0:8080", 10, &x)], vec![], &from_x_elsewhere).await;
        assert_eq!(addr_of(&sub), "192.168.1.5:8080");
        // Relayed, a different address doesn't override it.
        let relayed = NodeInfo { addr: "10.6.6.6:8080".into(), ..record("x", "0.0.0.0:8080", 10, &x) };
        merge_sync(&sub, None, vec![relayed], vec![], &from_a).await;
        assert_eq!(addr_of(&sub), "192.168.1.5:8080");

        // With nothing better than the wildcard (or a relayed address on
        // another port), there's nowhere to dial: no sync with 0.0.0.0.
        let other = crate::state::test_support::tls_state(&dir, "other");
        let wrong_port = NodeInfo { addr: "10.0.0.5:9999".into(), ..record("x", "0.0.0.0:8080", 10, &x) };
        merge_sync(&other, None, vec![wrong_port], vec![], &from_a).await;
        assert_eq!(addr_of(&other), "0.0.0.0:8080");
        assert!(peer_addrs(&other).await.iter().all(|addr| !is_wildcard(addr)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn only_the_node_that_answers_at_an_address_counts_as_live() {
        let (dir, sub, x, _) = signed_nodes_setup();
        merge_sync(&sub, None, vec![record("x", "10.0.0.9:8080", 10, &x)], vec![], &SyncSource::default()).await;
        age(&sub, "x", NODE_EXPIRE_AFTER + Duration::from_secs(1)).await;
        let live = |sub: &SharedState| crate::state::node_is_live(&sub.known_nodes.load()["x"], Instant::now());

        // x died; y now has its IP and port. Reaching y there says nothing
        // about x, which is forgotten as it should be.
        let y_there = SyncSource { cn: Some("y"), host: Some("10.0.0.9"), contact_addr: Some("10.0.0.9:8080") };
        merge_sync(&sub, None, vec![], vec![], &y_there).await;
        assert!(!live(&sub));
        expire_silent_nodes(&sub).await;
        assert!(!sub.known_nodes.load().contains_key("x"));

        // Whereas x answering there is x alive.
        merge_sync(&sub, None, vec![record("x", "10.0.0.9:8080", 11, &x)], vec![], &SyncSource::default()).await;
        age(&sub, "x", Duration::from_secs(60)).await;
        let x_there = SyncSource { cn: Some("x"), host: Some("10.0.0.9"), contact_addr: Some("10.0.0.9:8080") };
        merge_sync(&sub, None, vec![], vec![], &x_there).await;
        assert!(live(&sub));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn beats_go_only_to_peers_that_ask_and_lack_them() {
        let (dir, sub, x, _) = signed_nodes_setup();
        let relayed = SyncSource::default();
        merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![crate::signing::sign_beat("x", 50, &x).unwrap()], &relayed).await;
        let config = sub.dynamic_config.read().await.clone();
        let beats_in = |reply: &Message| {
            let Message::Sync { beats, .. } = reply else { unreachable!() };
            beats.iter().map(|b| (b.id.clone(), b.beat)).collect::<Vec<_>>()
        };
        let asking = |held: &[(&str, u64)]| messages::SyncDigest {
            want_beats: true,
            beats: held.iter().map(|(id, b)| (id.to_string(), *b)).collect(),
            ..digest_of(&[], &config)
        };

        // Not asked: none.
        assert!(beats_in(&build_sync_reply(&sub, &digest_of(&[], &config)).await).is_empty());
        // Asked: only what the peer lacks or holds older.
        assert_eq!(beats_in(&build_sync_reply(&sub, &asking(&[])).await), [("x".to_string(), 50)]);
        assert_eq!(beats_in(&build_sync_reply(&sub, &asking(&[("x", 40)])).await), [("x".to_string(), 50)]);
        assert!(beats_in(&build_sync_reply(&sub, &asking(&[("x", 50)])).await).is_empty());
        // What we send unprompted carries only our own beat, and we have none.
        assert!(beats_in(&build_compact_sync(&sub, true).await).is_empty());
        assert!(beats_in(&build_sync_msg(&sub, true).await).is_empty());
        let Message::Sync { have: Some(have), .. } = build_compact_sync(&sub, true).await else { unreachable!() };
        assert_eq!(have.beats, [("x".to_string(), 50)], "the digest says which beat we hold");
        // A digest from a node predating the field asks for all of them.
        let old: messages::SyncDigest = rmp_serde::from_slice(&rmp_serde::to_vec_named(&serde_json::json!({
            "records": [], "configVersion": 0, "configHash": 0,
        })).unwrap()).unwrap();
        assert!(old.want_beats);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_departure_reaches_nodes_the_leave_message_does_not() {
        let (dir, sub, x, _) = signed_nodes_setup();
        let relayed = SyncSource::default();
        merge_sync(&sub, None, vec![record("x", "x:1", 10, &x)], vec![], &relayed).await;

        // x leaves: its leave message reaches `sub`, carrying its signed
        // last record.
        let left = crate::config::NodeRecord { id: "x".into(), addr: "x:1".into(), heartbeat: 11, left: true, ..Default::default() };
        let last = crate::signing::sign_node(&left, &x).unwrap();
        apply_message(&sub, Message::NodeLeave { id: "x".into(), heartbeat: 11, record: Some(last) }, Some("x")).await;
        assert!(!sub.known_nodes.load().contains_key("x"));

        // A NAT'd node that still has x syncs with `sub`: the reply tells it.
        let config = sub.dynamic_config.read().await.clone();
        let reply = build_sync_reply(&sub, &digest_of(&[("x", 10)], &config)).await;
        let Message::Sync { nodes, .. } = &reply else { unreachable!() };
        let relayed_left = nodes.iter().find(|n| n.id == "x").expect("the left record is relayed");
        let rec = crate::signing::verify_node(relayed_left.signed.as_ref().unwrap(), sub.tls.as_deref().unwrap()).unwrap();
        assert!(rec.left);

        // And a node merging it forgets x.
        let other = crate::state::test_support::tls_state(&dir, "other");
        merge_sync(&other, None, vec![record("x", "x:1", 10, &x)], vec![], &relayed).await;
        assert!(other.known_nodes.load().contains_key("x"));
        merge_sync(&other, None, vec![relayed_left.clone()], vec![], &relayed).await;
        assert!(!other.known_nodes.load().contains_key("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_node_forgotten_while_cut_off_comes_back_by_talking_to_us() {
        let (dir, sub, x, _) = signed_nodes_setup();
        let relayed = SyncSource::default();
        merge_sync(&sub, None, vec![record("x", "x:1", 10, &x)], vec![], &relayed).await;
        // Cut off for long enough, x is forgotten.
        sub.update_known_nodes(|kn| kn.remove("x")).await;
        sub.bury("x", 10);

        // Its record hasn't changed since (it only changes with its
        // address, health or relays): relayed, it stays forgotten...
        merge_sync(&sub, None, vec![record("x", "x:1", 10, &x)], vec![], &relayed).await;
        assert!(!sub.known_nodes.load().contains_key("x"));
        // ...but from x itself, it's back.
        let direct = SyncSource { cn: Some("x"), ..Default::default() };
        merge_sync(&sub, None, vec![record("x", "x:1", 10, &x)], vec![], &direct).await;
        assert!(sub.known_nodes.load().contains_key("x"));

        // And a sync x sends us counts as a sign of life, even with the same
        // record: one that can reach us but not the other way round isn't
        // forgotten.
        let long_ago = Instant::now() - Duration::from_secs(600);
        sub.update_known_nodes(|kn| kn.get_mut("x").unwrap().heard_at = Some(long_ago)).await;
        merge_sync(&sub, None, vec![record("x", "x:1", 10, &x)], vec![], &direct).await;
        assert!(sub.known_nodes.load()["x"].heard_at.unwrap() > long_ago);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn older_peers_get_full_syncs() {
        let state = state_for("me");
        // No digest from the initiator: reply with everything, config included.
        let Message::Sync { config_elided, have, .. } = build_sync_msg(&state, false).await else { unreachable!() };
        assert!(!config_elided && have.is_some());
        // Until a peer's reply carries a digest, it gets full syncs.
        let Message::Sync { config_elided, .. } = sync_msg_for(&state, "old:1", false).await else { unreachable!() };
        assert!(!config_elided);
        state.delta_peers.lock().unwrap().insert("new:1".into());
        let Message::Sync { config_elided, .. } = sync_msg_for(&state, "new:1", false).await else { unreachable!() };
        assert!(config_elided);
    }
}
