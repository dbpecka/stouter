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
pub(crate) fn warn_rarely(last: &std::sync::Mutex<Option<std::time::Instant>>, message: std::fmt::Arguments<'_>) {
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
pub(crate) const MAX_IDLE_PER_PEER: usize = 64;

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
    recv_message_limited(stream, MAX_MSG_BYTES).await
}

/// [`recv_message`] for a message of at most `limit` bytes: one longer is
/// refused on its length prefix, before any of it is read.
pub(crate) async fn recv_message_limited<R: AsyncReadExt + Unpin>(stream: &mut R, limit: u32) -> Result<Message> {
    recv_message_within(stream, limit, None).await
}

/// [`recv_message_limited`], the message's body (once its length has
/// arrived) within `body_within`, if given. The wait for the length is
/// unbounded (an idle connection is fine); the body isn't: a peer can't
/// hold a message of up to `limit` bytes here half sent for ever.
pub(crate) async fn recv_message_within<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    limit: u32,
    body_within: Option<Duration>,
) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("read length prefix")?;
    match body_within {
        Some(within) => time::timeout(within, recv_body(stream, len_buf, limit))
            .await
            .map_err(|_| anyhow::anyhow!("message body not sent within {}s", within.as_secs()))?,
        None => recv_body(stream, len_buf, limit).await,
    }
}

/// The rest of a message whose length prefix was `len_buf`.
async fn recv_body<R: AsyncReadExt + Unpin>(stream: &mut R, len_buf: [u8; 4], limit: u32) -> Result<Message> {
    let len = u32::from_be_bytes(len_buf);

    if len > limit {
        warn_message_size(format_args!("refusing a {len}-byte message: over the {limit}-byte limit"));
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
        result = recv_message_within(&mut stream, MAX_MSG_BYTES, Some(FIRST_MESSAGE_BODY_WITHIN)) => result,
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
    // isn't tracked as a whole; each of its streams is. A kept sync
    // connection likewise: each sync on it is.
    let _in_flight = (!matches!(msg, Message::MuxTunnel {} | Message::Sync { .. })).then(|| state.in_flight.track());
    match msg {
        Message::Sync { .. } => {
            metrics::counter!("stouter_connections_total", "type" => "gossip").increment(1);
            serve_syncs(stream, peer_addr, msg, state, peer).await;
        }
        Message::ConfigUpdate { .. }
        | Message::NodeJoin { .. }
        | Message::NodeLeave { .. } => {
            metrics::counter!("stouter_connections_total", "type" => "gossip").increment(1);
            handle_gossip_message(stream, peer_addr, msg, state).await;
        }
        Message::TunnelRequest {
            node_id,
            service_name,
            timestamp_ms,
            caller,
        } => {
            metrics::counter!("stouter_connections_total", "type" => "tunnel").increment(1);
            if let Err(e) = crate::node::tunnel::handle_tunnel(
                stream,
                node_id,
                service_name,
                timestamp_ms,
                caller,
                false,
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
        Message::HistoryRequest { version } => {
            handle_history_request(stream, version, state).await;
        }
        Message::CertRequest { csr } => {
            crate::renew::handle_cert_request(stream, csr, state).await;
        }
        Message::CertResponse { .. } => {
            debug!("unexpected CertResponse from {peer_addr}");
        }
        Message::HistoryResponse { .. } => {
            debug!("unexpected HistoryResponse from {peer_addr}");
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

/// How long the body of a connection's first message may take, once its
/// length has arrived: a sync (the largest, up to 8 MiB) is given up on by
/// its sender after 10 seconds, and an admin request by the CLI after 30.
/// Past it, the connection, and what it sent, is let go.
const FIRST_MESSAGE_BODY_WITHIN: Duration = Duration::from_secs(30);

/// How long a sync connection is kept open for the peer's next sync (sent
/// every 10 seconds).
const GOSSIP_KEEP_OPEN: Duration = Duration::from_secs(25);

/// Sync connections one peer may keep open here at once (one, and another
/// while it replaces it).
const MAX_KEPT_SYNCS_PER_PEER: usize = 2;

/// Answer a peer's syncs on one connection: the first, and, if it asks to
/// keep the connection (and has no more kept here than it should), each
/// later one it sends on it, until it stops or [`GOSSIP_KEEP_OPEN`] passes
/// with none. Each round otherwise costs an mTLS handshake per peer, both
/// ways. Waiting between syncs isn't work in flight: shutdown doesn't wait
/// for it.
async fn serve_syncs(mut stream: MeshStream, peer_addr: String, mut msg: Message, state: Arc<SharedState>, peer: String) {
    let slot = state.try_peer_slot("sync", &peer, MAX_KEPT_SYNCS_PER_PEER);
    loop {
        let asked = matches!(&msg, Message::Sync { have: Some(have), .. } if have.keep_open);
        let keep = asked && slot.is_some() && !state.shutdown.is_cancelled();
        {
            let _in_flight = state.in_flight.track();
            if !answer_sync(&mut stream, &peer_addr, msg, &state, keep).await || !keep {
                return;
            }
        }
        msg = tokio::select! {
            next = time::timeout(GOSSIP_KEEP_OPEN, recv_message(&mut stream)) => match next {
                Ok(Ok(next @ Message::Sync { .. })) => next,
                _ => return,
            },
            _ = state.shutdown.cancelled() => return,
        };
    }
}

/// Merge a peer's sync and reply to it, saying whether we keep the
/// connection open (`keep`). Returns whether the reply was sent.
async fn answer_sync(stream: &mut MeshStream, peer_addr: &str, msg: Message, state: &Arc<SharedState>, keep: bool) -> bool {
    let Message::Sync { config, nodes, beats, have, config_elided } = msg else { return false };
    // No `contact_addr`: an inbound connection arrives from an ephemeral
    // source port, which won't match the peer's advertised address, so it
    // can't be used as a liveness signal here. The outbound sync loop
    // confirms every peer independently.
    let source = SyncSource { cn: stream.peer_cn(), host: host_of(peer_addr), contact_addr: None };
    let config = (!config_elided).then_some(config);
    merge_sync(state, config, nodes, beats, &source).await;
    // It's up: if we had given up dialling it for a while, don't wait.
    if let Some(node) = source.cn.and_then(|cn| state.known_nodes.load().get(cn).cloned()) {
        state.sync_backoff.lock().unwrap().succeeded(&node.addr);
    }

    // Reply with what the peer lacks (or, to an older peer that sends no
    // digest, with everything).
    let mut reply = match &have {
        Some(have) => build_sync_reply(state, have).await,
        None => build_sync_msg(state, false).await,
    };
    if let Message::Sync { have: Some(ours), .. } = &mut reply {
        ours.keep_open = keep;
    }
    match send_message(stream, &reply).await {
        Ok(()) => true,
        Err(e) => {
            debug!("failed to send sync reply: {e}");
            false
        }
    }
}

async fn handle_gossip_message(mut stream: MeshStream, peer_addr: String, msg: Message, state: Arc<SharedState>) {
    match msg {
        Message::Sync { .. } => {
            answer_sync(&mut stream, &peer_addr, msg, &state, false).await;
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

    // Authorization: peer's TLS CN must be in the configured allowlist, or
    // be this node's own: the CLI on this host making a change to the config
    // here (which it could make by editing the file anyway). With
    // requireSignedConfig, the change must still be admin-signed.
    let allowed = match stream.peer_cn() {
        Some(cn) => state.config.admin_cns.iter().any(|a| a == cn) || cn == state.config.node_id,
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
                    next.signed_at = None;
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
    // Signed by an admin; or, where changes needn't be signed, also by this
    // node itself (the CLI on this host, as for any other admin request).
    let mut signers = state.config.admin_cns.clone();
    if !state.config.require_signed_config {
        signers.push(state.config.node_id.clone());
    }
    let (verified, signer) =
        crate::signing::verify(&pushed, tls, &signers).map_err(|e| MutateErr::Unauthorized(format!("{e:#}")))?;
    // A push is a change made now: its signer's certificate must be valid
    // now, not just as of the date the config claims.
    if let Some((who, until)) = crate::signing::expired_signer(&verified) {
        return Err(MutateErr::Unauthorized(crate::signing::expired_signer_refusal(&verified, &who, until)));
    }
    if let Some(why) =
        crate::signing::signed_too_early(&verified, current).or_else(|| crate::config::too_far_ahead(&verified, current))
    {
        return Err(MutateErr::Invalid(why));
    }
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
        cert_expires: state.tls.as_ref().and_then(|tls| crate::pki::validity(tls.own_cert().as_ref())).map(|(_, until)| until),
        issues_certs: state.issuer.get().is_some(),
    };

    if let Err(e) = send_message(&mut stream, &response).await {
        debug!("status: failed to send response: {e}");
    }
}

/// Answer a `HistoryRequest` from this node's config history: every version
/// recorded (without the configs), or just `version` (with its config).
async fn handle_history_request(mut stream: MeshStream, version: Option<u64>, state: Arc<SharedState>) {
    let mut entries = crate::history::load(&state.config_path);
    match version {
        Some(v) => entries.retain(|e| e.version == v),
        None => entries.iter_mut().for_each(|e| e.config = None),
    }
    if let Err(e) = send_message(&mut stream, &Message::HistoryResponse { entries }).await {
        debug!("history: failed to send response: {e}");
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
    // The edit last refused for breaking the config.
    let mut warned_problem: Option<String> = None;
    // Why the file last couldn't be read.
    let mut warned_unreadable: Option<String> = None;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }
        // An edit made while the daemon was stopped waits until it has
        // caught up: if the cluster moved on meanwhile, the newer config
        // arrives first and the edit is kept aside (see persist_state_with)
        // instead of meeting the cluster's change at the same version.
        if state.edited_offline.load(std::sync::atomic::Ordering::Relaxed) {
            if !state.caught_up() {
                continue;
            }
            state.edited_offline.store(false, std::sync::atomic::Ordering::Relaxed);
        }

        // Read the file under the lock every persist also holds, so the file
        // and memory are compared as one consistent snapshot.
        let new_dc = {
            let mut dc = state.dynamic_config.write().await;
            let file_dc = match crate::config::Config::load(&state.config_path) {
                Ok(c) => c.dynamic_config,
                Err(e) => {
                    // A hand edit that doesn't parse: say so (once per
                    // error), or nothing shows until the next start fails.
                    let why = format!("{e:#}");
                    if warned_unreadable.as_ref() != Some(&why) {
                        warn!("ignoring {}: {why}", state.config_path);
                        warned_unreadable = Some(why);
                    }
                    continue;
                }
            };
            let current_version = dc.version;
            debug!(
                "config poll: file version={}, in-memory version={}",
                file_dc.version, current_version
            );
            // Taken from the file (below, if adopted): no longer an edit
            // waiting there.
            let taken = |state: &SharedState, file_dc: &crate::config::DynamicConfig| {
                *state.file_config.lock().unwrap() = Some(config_text(file_dc));
                for unknown in crate::config::Config::unknown_nested_settings_in(&state.config_path) {
                    warn!("{}: {unknown}", state.config_path);
                }
            };

            if state.config.require_signed_config {
                // Only an admin-signed config counts: the CLI's local path
                // writes one, a hand edit can't. It is adopted exactly as
                // signed (no version bump, no changedBy).
                match crate::signing::admit(&state, file_dc.clone(), &dc) {
                    Ok(signed) if signed.supersedes(&dc) => {
                        info!("config file has a newer signed config: version {} -> {}", current_version, signed.version);
                        taken(&state, &file_dc);
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
                            Ok(signed) => !signed.same_settings(&file_dc),
                            Err(_) => !file_dc.same_settings(&dc),
                        };
                        let key = config_text(&file_dc);
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

            let file_text = file_dc.clone();
            let mut new_dc = if file_dc.version > current_version {
                info!(
                    "config file version advanced: {} -> {}",
                    current_version, file_dc.version
                );
                file_dc
            } else if file_dc.version == current_version && !file_dc.same_settings(&dc) {
                info!(
                    "manual config edit detected; auto-bumping version {} -> {}",
                    current_version,
                    current_version.saturating_add(1)
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
            let version = if new_dc.version <= current_version {
                dc.next_version().map_err(|e| e.to_string())
            } else {
                Ok(new_dc.version)
            };
            // The same checks a change from the CLI or the admin RPC gets: an
            // edit that breaks the config (two services claiming a domain, a
            // route to a service that isn't there), or that peers would
            // refuse for its version, isn't sent cluster-wide.
            let problem = match &version {
                Err(e) => Some(e.clone()),
                Ok(_) => crate::config::too_far_ahead(&new_dc, &dc).or_else(|| crate::config::new_problem(&dc, &new_dc)),
            };
            let (None, Ok(version)) = (problem.clone(), version) else {
                let key = config_text(&new_dc);
                if warned_problem.as_ref() != Some(&key) {
                    warn!("ignoring the edit to {}: {}", state.config_path, problem.unwrap_or_default());
                    warned_problem = Some(key);
                }
                continue;
            };
            new_dc.version = version;
            // The change entered the cluster here, unsigned.
            new_dc.changed_by = state.config.node_id.clone();
            new_dc.signature = None;
            new_dc.signed_at = None;
            taken(&state, &file_text);
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
    !addr.is_empty() && !is_wildcard(addr) && !own.contains(&addr) && !state.own_addrs.lock().unwrap().contains(addr)
}

/// Background task: periodically push a Sync message to every known peer and
/// merge their reply.
pub async fn run_sync_loop(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_secs(10));
    let mut round: u64 = 0;
    // When a sync last worked (or the loop started), and when that last
    // went unsaid for too long: see SYNCS_FAILING_WARN_AFTER.
    let mut last_synced = time::Instant::now();
    let mut warned_at: Option<time::Instant> = None;
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
        // What we know of peers no longer about is let go (in a cluster
        // without signed records, anyone could make up any number).
        let forgotten = state.forgotten_addrs();
        state.delta_peers.lock().unwrap().retain(|addr, _| peers.contains(addr) || forgotten.contains(addr));
        // Peers that haven't answered lately wait out their backoff: a dead
        // address (or a made-up one) is dialled every few minutes, not
        // every round.
        {
            let now = time::Instant::now();
            let mut backoff = state.sync_backoff.lock().unwrap();
            backoff.retain(|addr| peers.iter().any(|p| p == addr) || forgotten.iter().any(|f| f == addr));
            peers.retain(|addr| backoff.ready(addr, now));
        }
        // Connections kept for peers we no longer sync with are closed.
        state.gossip_conns.lock().unwrap().retain(|addr, _| peers.contains(addr));
        // Every peer would send the same beats: ask only a few of them.
        let beat_sources: std::collections::HashSet<&String> = {
            use rand::seq::IndexedRandom;
            peers.choose_multiple(&mut rand::rng(), BEAT_SOURCES).collect()
        };
        let failures = futures::future::join_all(peers.iter().map(|addr| {
            let state = &state;
            let want_beats = beat_sources.contains(addr);
            async move {
                let start = std::time::Instant::now();
                let result = time::timeout(Duration::from_secs(10), sync_with_peer(state, addr, want_beats)).await;
                let failure = match &result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{addr}: {e:#}")),
                    Err(_) => Some(format!("{addr}: timed out")),
                };
                match &result {
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
                let mut backoff = state.sync_backoff.lock().unwrap();
                if matches!(result, Ok(Ok(()))) {
                    backoff.succeeded(addr);
                } else {
                    backoff.failed(addr, time::Instant::now());
                }
                failure
            }
        }))
        .await;
        // Every sync failing for a while (each logged only at debug) is
        // what a node cut off from its cluster looks like: certificates
        // from another CA, say. Said at warn, now and then.
        if failures.iter().any(Option::is_none) {
            last_synced = time::Instant::now();
            warned_at = None;
        } else if let Some(Some(last)) = failures.last() {
            let due = warned_at.is_none_or(|at| at.elapsed() >= SYNCS_FAILING_WARN_EVERY);
            if last_synced.elapsed() >= SYNCS_FAILING_WARN_AFTER && due {
                warn!(
                    "no sync with a peer has worked for {}s (last: {last}); errors such as BadSignature or \
                     UnknownIssuer mean this node's certificates come from another CA than its peers'",
                    last_synced.elapsed().as_secs()
                );
                warned_at = Some(time::Instant::now());
            }
        }

        expire_silent_nodes(&state).await;
    }
}

/// How long every sync may fail before the sync loop says so at warn, and
/// how often it says it again while they do.
const SYNCS_FAILING_WARN_AFTER: Duration = Duration::from_secs(60);
const SYNCS_FAILING_WARN_EVERY: Duration = Duration::from_secs(600);

/// Most nodes held that are known only from unsigned gossip (see
/// `merge_sync`).
const MAX_UNSIGNED_NODES: usize = 1024;

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
                let can_tell = node.heartbeat > 0 || !node.addr.is_empty() || node.registered_here;
                if silent && can_tell {
                    info!("forgetting node {id}: no sign of life for {}s", NODE_EXPIRE_AFTER.as_secs());
                    state.bury_forgotten(node);
                    state.remember_forgotten(id, &node.addr);
                    false
                } else {
                    true
                }
            })
        })
        .await;
}

/// How long a sync on a kept connection may take before it is given up for
/// a new connection: one kept from the last round may have died quietly.
const KEPT_SYNC_TIMEOUT: Duration = Duration::from_secs(3);

async fn sync_with_peer(state: &Arc<SharedState>, addr: &str, want_beats: bool) -> Result<()> {
    let mut our_sync = sync_msg_for(state, addr, want_beats).await;
    // Ask the peer to keep the connection for the next round.
    if let Message::Sync { have: Some(have), .. } = &mut our_sync {
        have.keep_open = true;
    }
    async fn exchange(stream: &mut MeshStream, msg: &Message) -> Result<Message> {
        send_message(stream, msg).await?;
        recv_message(stream).await
    }
    // The connection the peer kept from the last round, if it still works;
    // else a new one.
    let kept = state.gossip_conns.lock().unwrap().remove(addr);
    let kept = match kept {
        Some(mut stream) => match time::timeout(KEPT_SYNC_TIMEOUT, exchange(&mut stream, &our_sync)).await {
            Ok(Ok(reply)) => Some((stream, reply)),
            _ => None,
        },
        None => None,
    };
    let (stream, reply) = match kept {
        Some(kept) => kept,
        None => {
            let mut stream = state.connect_mesh(addr, None).await?;
            // Our own certificate answered: this address is us (a shared
            // `knownNodes` list naming our real IP while we bind 0.0.0.0).
            if stream.peer_cn() == Some(state.config.node_id.as_str()) {
                debug!("{addr} is this node; not syncing with it");
                state.own_addrs.lock().unwrap().insert(addr.to_string());
                return Ok(());
            }
            let reply = exchange(&mut stream, &our_sync).await?;
            (stream, reply)
        }
    };

    if let Message::Sync { config, nodes, beats, have, config_elided } = reply {
        let keep = have.as_ref().is_some_and(|have| have.keep_open);
        // A digest in the reply means the peer speaks delta sync: from now
        // on, send it compact syncs. (An older peer never sends one.) It
        // also says which config the peer has, so if ours is newer the next
        // sync carries it: a compact sync leaves the config out, and a node
        // no peer can dial has no other way to pass a change on.
        match &have {
            Some(peer) => {
                state.delta_peers.lock().unwrap().insert(addr.to_string(), (peer.config_version, peer.config_hash));
            }
            None => {
                state.delta_peers.lock().unwrap().remove(addr);
            }
        }
        // A successful outbound sync is direct evidence the peer at `addr` is
        // reachable, so pass it as the liveness `contact_addr`.
        let source = SyncSource { cn: stream.peer_cn(), host: host_of(addr), contact_addr: Some(addr) };
        merge_sync(state, (!config_elided).then_some(config), nodes, beats, &source).await;
        // The peer waits for our next sync on this connection.
        if keep {
            state.gossip_conns.lock().unwrap().insert(addr.to_string(), stream);
        }
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
    let full = build_sync_msg(state, false).await;
    let (compact, compact_with_config) =
        (build_compact_sync(state, false, false).await, build_compact_sync(state, false, true).await);
    let peers = peer_addrs(state).await;
    let delta_peers = state.delta_peers.lock().unwrap().clone();
    let config = state.dynamic_config.read().await.clone();
    futures::future::join_all(peers.iter().map(|addr| {
        let msg = match delta_peers.get(addr) {
            Some(peer) if config_newer_than(&config, *peer) => &compact_with_config,
            Some(_) => &compact,
            None => &full,
        };
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

/// Print the current dynamic config as pretty JSON to stdout. Failing to
/// (stdout closed) is no reason to stop: `println!` would panic, ending
/// the task that adopted the config, the sync or config poll loop.
async fn print_dynamic_config(state: &Arc<SharedState>) {
    use std::io::Write;
    let dc = state.dynamic_config.read().await.clone();
    match serde_json::to_string_pretty(&dc) {
        Ok(json) => {
            let _ = writeln!(std::io::stdout().lock(), "{json}");
        }
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
    // Every config change comes through here, with the config's lock held:
    // index it at once, in the same step.
    state.index_config(dc);
    let path = &state.config_path;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            error!("failed to read {path} to persist the config: {e}");
            return;
        }
    };
    let mut cfg: crate::config::Config = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to load config for persist: invalid JSON in {path}: {e}");
            return;
        }
    };
    // An edit to the file this node hasn't applied (made since it last wrote
    // or read the file, or one it refused): writing over it would lose it
    // without a word. Keep a copy, and say so.
    let on_disk = config_text(&cfg.dynamic_config);
    let new = config_text(dc);
    let mut known = state.file_config.lock().unwrap();
    // (Not when the file already says what's being written, as when an edit
    // made there was just pushed and comes back signed.)
    if known.as_ref().is_some_and(|known| *known != on_disk) && on_disk != new && !cfg.dynamic_config.same_settings(dc) {
        let aside = crate::config::unused_path(&format!("{path}.unapplied"));
        match crate::config::write_atomic(std::path::Path::new(&aside), text.as_bytes()) {
            Ok(()) => warn!(
                "{path} had an edit not applied here (version {}); it is saved in {aside}, and {path} now holds \
                 version {} from the cluster: make the edit again on top of that{}",
                cfg.dynamic_config.version,
                dc.version,
                if state.config.require_signed_config { " and push it (`stouter config push`)" } else { "" }
            ),
            Err(e) => {
                error!("{path} had an edit not applied here, and it can't be saved to {aside}: {e}; not writing over it");
                return;
            }
        }
    }
    cfg.dynamic_config = dc.clone();
    match cfg.save(path) {
        Ok(()) => {
            *known = Some(new);
            crate::history::record(path, dc);
        }
        Err(e) => error!("failed to persist state to {path}: {e}"),
    }
}

/// A dynamic config as the JSON the config file would hold, for telling
/// whether the file still says what this node last wrote there.
fn config_text(dc: &crate::config::DynamicConfig) -> String {
    serde_json::to_string(dc).unwrap_or_default()
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
        Some(config) => adopt_from_peer(state, config, "dynamic config updated", source.cn).await,
        None => false,
    };

    if updated {
        state.rebuild_service_index().await;
        print_dynamic_config(state).await;
    }

    let own_id = state.config.node_id.as_str();
    let strict = state.config.require_signed_config;
    let now = Instant::now();
    // Signatures are checked first, without the node list's lock: checking
    // is the slow part, and any peer can send junk to check.
    let Some((nodes, beats, checked)) = check_signatures_off_thread(state, nodes, beats, source.cn).await else { return };
    // Node info isn't persisted (the config file only holds bootstrap
    // peers), so a change here writes nothing to disk.
    state.update_known_nodes(|kn| {
        let mut changed = false;
        // Counted once, then kept up to date: counting for every entry would
        // make a sync of many made-up nodes cost the square of their number,
        // with the node list locked.
        let mut unsigned = kn.values().filter(|n| n.signed.is_none()).count();
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
                changed |= merge_signed(state, kn, &node, signed, &checked, source, now);
                continue;
            }
            // Unsigned (an older node, or relayed by one): never let it
            // override what a node signed about itself, and with
            // `requireSignedConfig` (every node signs) ignore it altogether.
            if strict || kn.get(&node.id).is_some_and(|n| n.signed.is_some()) {
                debug!("ignoring unsigned gossip about node {}", node.id);
                continue;
            }
            // Revoked by name: nothing about it is believed.
            if state.tls.as_ref().is_some_and(|tls| tls.revocations().name_revoked(&node.id)) {
                debug!("ignoring gossip about revoked node {}", node.id);
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
                // One that signed its records, now departed: unsigned gossip
                // about it is no more believed than while it was here.
                if source.cn != Some(node.id.as_str()) && state.buried_signed(&node.id) {
                    debug!("ignoring unsigned gossip about departed node {}", node.id);
                    continue;
                }
                // A node that left or was forgotten comes back only with a
                // heartbeat newer than the one it departed with, or by
                // talking to us itself.
                if state.is_buried(&node.id, node.heartbeat) && source.cn != Some(node.id.as_str()) {
                    debug!("ignoring stale gossip about departed node {}", node.id);
                    continue;
                }
                // Without signed records anyone can make up nodes, each one
                // an address every node dials: hold only so many that never
                // signed (an older version's), plenty for any real cluster.
                if unsigned >= MAX_UNSIGNED_NODES {
                    if !state.unsigned_cap_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        warn!(
                            "already {MAX_UNSIGNED_NODES} nodes known only from unsigned gossip; ignoring more (latest: {} from {})",
                            node.id,
                            source.cn.unwrap_or("an unnamed peer")
                        );
                    }
                    continue;
                }
                state.unbury(&node.id);
                info!("discovered new node: {} @ {}", node.id, node.addr);
                // Give a freshly discovered node an initial liveness grace
                // period; the sync loop will confirm or expire it shortly.
                node.last_seen = Some(now);
                node.heard_at = Some(now);
                kn.insert(node.id.clone(), node);
                unsigned += 1;
                changed = true;
            }
        }
        for beat in beats {
            if beat.id == own_id {
                continue;
            }
            if !checked.beats.contains(&(beat.id.clone(), beat.beat, beat.signature.clone())) {
                continue;
            }
            // A NAT'd node we forgot for silence is back if it beats again.
            if !kn.contains_key(&beat.id) {
                if let Some(mut node) = state.revive(&beat) {
                    info!("node {} is back: a new beat from it", beat.id);
                    node.heartbeat = beat.beat;
                    node.heard_at = Some(now);
                    node.last_seen = Some(now);
                    node.beat = Some(beat);
                    kn.insert(node.id.clone(), node);
                    changed = true;
                    continue;
                }
            }
            merge_beat(kn, beat, now);
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
    state.synced.store(true, std::sync::atomic::Ordering::Relaxed);
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
    match crate::signing::admit(state, incoming, dc) {
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

/// [`adopt_if_newer`] for a config a peer sent. With `requireSignedConfig`,
/// its signature is checked without holding the config's lock: decoding and
/// verifying a large signed config takes tens of milliseconds, and every
/// reader of the config (each `httpListen` request, every sync, status)
/// waited behind it, so any member could stall a node by sending bogus ones.
/// So, outside the lock: one too large to be the cluster's is refused
/// unread, one refused lately isn't checked again for a while, and a peer
/// has one checked at a time (more are dropped: it offers them again next
/// sync). Only the cheap checks, and adopting, are under the lock.
async fn adopt_from_peer(
    state: &Arc<SharedState>,
    incoming: crate::config::DynamicConfig,
    what: &str,
    peer: Option<&str>,
) -> bool {
    if !state.config.require_signed_config {
        let mut dc = state.dynamic_config.write().await;
        return adopt_if_newer(state, &mut dc, incoming, what);
    }
    if !incoming.supersedes(&*state.dynamic_config.read().await) {
        return false;
    }
    let version = incoming.version;
    let verified = match verify_unlocked(state, incoming, peer).await {
        Some(Ok(config)) => config,
        Some(Err(e)) => {
            crate::signing::log_refusal(state, version, &e);
            return false;
        }
        None => return false,
    };
    let mut dc = state.dynamic_config.write().await;
    match crate::signing::admit_verified(state, verified, &dc) {
        Ok(config) if config.supersedes(&dc) => {
            log_adoption(&dc, &config, what);
            *dc = config;
            persist_state_with(state, &dc);
            true
        }
        Ok(_) => false,
        Err(e) => {
            crate::signing::log_refusal(state, version, &e);
            false
        }
    }
}

/// How long a refused signed config isn't checked again.
const REFUSED_CONFIG_FOR: Duration = Duration::from_secs(600);

/// Refused signed configs remembered (see [`adopt_from_peer`]).
const REFUSED_CONFIGS_KEPT: usize = 64;

/// Signed configs one peer may have checked at once (see
/// [`adopt_from_peer`]).
const MAX_CONFIG_CHECKS_PER_PEER: usize = 1;

/// `incoming` decoded from what an admin signed and checked (see
/// `signing::verify`), on a blocking thread, holding no lock. `None` if it
/// wasn't checked: refused lately, or `peer` already has one being checked.
async fn verify_unlocked(
    state: &Arc<SharedState>,
    incoming: crate::config::DynamicConfig,
    peer: Option<&str>,
) -> Option<anyhow::Result<crate::config::DynamicConfig>> {
    use std::hash::BuildHasher;
    let Some(sig) = incoming.signature.as_ref() else {
        return Some(Err(anyhow::anyhow!("config version {} is not signed", incoming.version)));
    };
    if sig.config_json.len() > crate::config::MAX_CONFIG_BYTES {
        return Some(Err(anyhow::anyhow!(
            "config version {} is signed over {} bytes, more than a config may be ({})",
            incoming.version,
            sig.config_json.len(),
            crate::config::MAX_CONFIG_BYTES
        )));
    }
    let key = state.refused_hasher.hash_one((&sig.config_json, &sig.signer_cert, &sig.signature));
    {
        let mut refused = state.refused_configs.lock().unwrap();
        refused.retain(|(_, at)| at.elapsed() < REFUSED_CONFIG_FOR);
        if refused.iter().any(|(k, _)| *k == key) {
            debug!("ignoring config version {}: refused lately", incoming.version);
            return None;
        }
    }
    let _turn = state.try_peer_slot("config-check", peer.unwrap_or_default(), MAX_CONFIG_CHECKS_PER_PEER)?;
    let tls = state.tls.clone()?;
    let admins = state.config.admin_cns.clone();
    let checked = tokio::task::spawn_blocking(move || crate::signing::verify(&incoming, &tls, &admins).map(|(config, _)| config))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("signature check failed: {e}")));
    if checked.is_err() {
        let mut refused = state.refused_configs.lock().unwrap();
        if refused.len() >= REFUSED_CONFIGS_KEPT {
            refused.pop_front();
        }
        refused.push_back((key, std::time::Instant::now()));
    }
    Some(checked)
}

/// Log that `incoming` is replacing `current`.
fn log_adoption(current: &crate::config::DynamicConfig, incoming: &crate::config::DynamicConfig, what: &str) {
    if incoming.version == current.version {
        warn!(
            "{what}: two different configs at version {}; keeping the one from '{}' over '{}' (the same choice on every node)",
            incoming.version, incoming.changed_by, current.changed_by
        );
        // A tie drops what the other config said: worth saying loudly when
        // that is a revocation, which nobody meant to undo.
        let dropped: Vec<&str> = (current.revoked.nodes.iter().filter(|n| !incoming.revoked.nodes.contains(n)))
            .chain(current.revoked.certs.iter().filter(|c| !incoming.revoked.certs.contains(c)))
            .map(String::as_str)
            .collect();
        if !dropped.is_empty() {
            warn!(
                "{what}: the config dropped at version {} revoked {} too, which the one kept doesn't: revoke {} again \
                 if that should stand",
                incoming.version,
                dropped.join(", "),
                if dropped.len() == 1 { "it" } else { "them" }
            );
        }
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
    checked: &Checked,
    source: &SyncSource<'_>,
    now: Instant,
) -> bool {
    let held = kn.get(&node.id).filter(|n| n.signed.is_some()).map(|n| n.record_version);
    // A record that says only "wherever you reached me" (a wildcard): where
    // to reach the node can improve without its record changing, so check
    // before skipping a record we already hold.
    let mut addr_changed = false;
    if let Some(existing) = kn.get_mut(&node.id).filter(|n| n.signed.is_some()) {
        let direct = source.cn == Some(node.id.as_str()) && is_wildcard(&node.addr);
        let better = if direct {
            // From the node itself: where we reached it, or it reached us.
            source.host.and_then(|host| rewrite_wildcard(&node.addr, host))
        } else if is_wildcard(&existing.addr) || (existing.addr_relayed && !crate::state::node_is_live(existing, now)) {
            // Nothing better yet, or what a relayer said doesn't work (a
            // wrong relayer, or the node moved): another relayer's address.
            relayed_address(&node.addr, &existing.addr)
        } else {
            None
        };
        if let Some(addr) = better {
            if addr != existing.addr {
                info!("node {} address updated: {} -> {addr}", node.id, existing.addr);
                existing.addr = addr;
                addr_changed = true;
            }
            existing.addr_relayed = !direct;
        }
    }
    match crate::signing::claimed_heartbeat(&signed) {
        Some(claimed) if held.is_none_or(|h| claimed > h) => {}
        _ => return addr_changed,
    }
    // Checked already (see `check_signatures`): this exact record, by this
    // node, or it isn't taken.
    let record = match checked.records.get(&node.id) {
        Some((json, record)) if *json == signed.record_json => record.clone(),
        _ => return addr_changed,
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
    let mut addr_relayed = false;
    if is_wildcard(&addr) {
        if source.cn == Some(record.id.as_str()) {
            if let Some(rewritten) = source.host.and_then(|host| rewrite_wildcard(&addr, host)) {
                addr = rewritten;
            }
        } else if let Some(known) = kn.get(&record.id).filter(|n| {
            // Keep what we have, unless it's a relayer's and doesn't work.
            !n.addr.is_empty() && !is_wildcard(&n.addr) && !(n.addr_relayed && !crate::state::node_is_live(n, now))
        }) {
            addr = known.addr.clone();
            addr_relayed = known.addr_relayed;
        } else if let Some(relayed) = relayed_address(&node.addr, &record.addr) {
            addr = relayed;
            addr_relayed = true;
        }
    }

    // A newer record from the node (it restarted, or moved) is news worth
    // dialling it for at once, even if it stopped answering us a while ago.
    // (Only a signed record counts: unsigned gossip could be anyone's.)
    if !addr.is_empty() {
        state.sync_backoff.lock().unwrap().succeeded(&addr);
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
            existing.addr_relayed = addr_relayed;
            // Beats may have raised a signed node's heartbeat past its record
            // version, so keep the higher. But the first signed record
            // replaces what unsigned gossip said: anyone could have claimed
            // a huge heartbeat for it, which would then shut out its beats.
            existing.heartbeat = if existing.signed.is_some() {
                existing.heartbeat.max(record.heartbeat)
            } else {
                record.heartbeat
            };
            existing.record_version = record.heartbeat;
            existing.health = record.health;
            existing.relays = record.relays;
            existing.subscriber = record.subscriber;
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
                    addr_relayed,
                    heartbeat: record.heartbeat,
                    record_version: record.heartbeat,
                    health: record.health,
                    relays: record.relays,
                    subscriber: record.subscriber,
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

/// Records and beats in one Sync whose signatures checked out (see
/// [`check_signatures`]).
#[derive(Default)]
struct Checked {
    /// Node id -> (the signed text, the record it says).
    records: std::collections::HashMap<String, (String, crate::config::NodeRecord)>,
    /// (node id, beat, signature) of each beat that verified.
    beats: std::collections::HashSet<(String, u64, String)>,
    /// How many beat signatures were checked, verified or not.
    beat_checks: usize,
}

/// Most records in one Sync checked for nodes we don't know yet. A real
/// cluster grows by a few nodes at a time; this keeps a peer sending junk
/// (each record claiming to be new) from making us check thousands per
/// message. Nodes we know are checked only when their record is newer.
const MAX_NEW_RECORDS_PER_SYNC: usize = 256;

/// Most beats checked in one Sync. Only nodes with no address beat, and
/// each is checked once per sync (its newest beat), so a cluster sends
/// fewer; past this, which ones get checked is left to chance, so that no
/// node's beats are the ones dropped every time.
const MAX_BEATS_PER_SYNC: usize = 4096;

/// Check, without holding the node list's lock, the signatures that
/// merging `nodes` and `beats` would need: each node's record if it claims
/// to be newer than the one we hold (at most [`MAX_NEW_RECORDS_PER_SYNC`] of
/// them for nodes we don't know), and each node's newest beat if it is newer
/// than what we have (at most [`MAX_BEATS_PER_SYNC`]), against its node's
/// record (or, for one we forgot, the record kept in its tombstone).
///
/// Slow (a signature check each), so run off the async workers: see
/// [`check_signatures_off_thread`].
fn check_signatures(state: &SharedState, nodes: &[NodeInfo], beats: &[crate::config::SignedBeat]) -> Checked {
    let mut checked = Checked::default();
    let Some(tls) = state.tls.as_deref() else { return checked };
    let known = state.known_nodes.load();
    let mut new_ids = 0;
    // One check per node per sync, whatever its outcome: a peer repeating
    // one id thousands of times would otherwise have each copy checked.
    let mut tried: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for node in nodes {
        let Some(signed) = &node.signed else { continue };
        if !tried.insert(node.id.as_str()) {
            continue;
        }
        let held = known.get(&node.id).filter(|n| n.signed.is_some()).map(|n| n.record_version);
        match crate::signing::claimed_heartbeat(signed) {
            Some(claimed) if held.is_none_or(|h| claimed > h) => {}
            _ => continue,
        }
        if held.is_none() {
            new_ids += 1;
            if new_ids > MAX_NEW_RECORDS_PER_SYNC {
                continue;
            }
        }
        match crate::signing::verify_node(signed, tls) {
            Ok(record) if record.id == node.id => {
                checked.records.insert(node.id.clone(), (signed.record_json.clone(), record));
            }
            Ok(record) => debug!("ignoring {}'s record relayed as node {}", record.id, node.id),
            Err(e) => debug!("ignoring a record for node {} that doesn't verify: {e:#}", node.id),
        }
    }
    // The certificate of each record just received that checked out (it
    // may be newer than the one held, with a new key).
    let mut received: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for node in nodes {
        if let (Some(signed), Some((json, _))) = (&node.signed, checked.records.get(&node.id)) {
            if signed.record_json == *json {
                received.entry(node.id.as_str()).or_insert(signed.cert.as_str());
            }
        }
    }
    // The newest beat for each node that has something to check it
    // against: one we hold with no address (nodes with one don't beat:
    // they're seen alive by being reached), one we forgot (kept in its
    // tombstone), or one whose record just came in.
    let mut newest: std::collections::HashMap<&str, (&crate::config::SignedBeat, Option<String>)> =
        std::collections::HashMap::new();
    for beat in beats {
        if newest.get(beat.id.as_str()).is_some_and(|(b, _)| b.beat >= beat.beat) {
            continue;
        }
        let held = match known.get(&beat.id) {
            Some(node) if node.addr.is_empty() && beat.beat > node.heartbeat => {
                node.signed.as_ref().map(|s| s.cert.clone())
            }
            Some(_) => continue,
            None => state.forgotten_cert(&beat.id).filter(|(last, _)| beat.beat > *last).map(|(_, cert)| cert),
        };
        if held.is_none() && !received.contains_key(beat.id.as_str()) {
            continue;
        }
        newest.insert(beat.id.as_str(), (beat, held));
    }
    let mut todo: Vec<_> = newest.into_values().collect();
    if todo.len() > MAX_BEATS_PER_SYNC {
        use rand::seq::SliceRandom;
        todo.shuffle(&mut rand::rng());
        todo.truncate(MAX_BEATS_PER_SYNC);
    }
    for (beat, held) in todo {
        let received = received.get(beat.id.as_str()).copied();
        let mut last_err = None;
        // A record held from before its certificate was revoked vouches for nothing.
        let certs = held.as_deref().into_iter().chain(received);
        // Nor one whose certificate has expired since: its key keeping a
        // node alive (or bringing it back) would outlast the certificate.
        for cert in certs.filter(|cert| !tls.revocations().cert_hex_revoked(cert) && !crate::signing::cert_hex_expired(cert)) {
            checked.beat_checks += 1;
            match crate::signing::verify_beat(beat, cert) {
                Ok(()) => {
                    checked.beats.insert((beat.id.clone(), beat.beat, beat.signature.clone()));
                    last_err = None;
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(e) = last_err {
            debug!("ignoring a beat for node {}: {e:#}", beat.id);
        }
    }
    checked
}

/// [`check_signatures`] on a blocking thread, a few syncs at a time (as
/// many as there are CPUs), so that peers sending signatures to check can't
/// hold up the async workers (and with them tunnels and admin requests).
/// Hands `nodes` and `beats` back with the result.
///
/// A sync with nothing to check skips the queue; one peer may have at most
/// [`MAX_CHECKING_SYNCS_PER_PEER`] syncs being checked at once (more are
/// dropped: the next round sends them again), so a peer sending junk to
/// check can't queue ahead of everyone else's syncs.
async fn check_signatures_off_thread(
    state: &Arc<SharedState>,
    nodes: Vec<NodeInfo>,
    beats: Vec<crate::config::SignedBeat>,
    peer: Option<&str>,
) -> Option<(Vec<NodeInfo>, Vec<crate::config::SignedBeat>, Checked)> {
    static CHECKING: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    static LAST_LIMIT_WARNING: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    if !needs_checks(state, &nodes, &beats) {
        return Some((nodes, beats, Checked::default()));
    }
    let peer = peer.unwrap_or("");
    let Some(_turn) = state.try_peer_slot("sync-check", peer, MAX_CHECKING_SYNCS_PER_PEER) else {
        warn_rarely(
            &LAST_LIMIT_WARNING,
            format_args!(
                "dropping a sync from '{peer}': it already has {MAX_CHECKING_SYNCS_PER_PEER} syncs having their \
                 signatures checked"
            ),
        );
        return None;
    };
    let slots = CHECKING.get_or_init(|| {
        tokio::sync::Semaphore::new(std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get))
    });
    let _slot = slots.acquire().await.ok()?;
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let checked = check_signatures(&state, &nodes, &beats);
        (nodes, beats, checked)
    })
    .await
    .ok()
}

/// Most syncs from one peer having their signatures checked at once: an
/// honest peer has one it started and one we did.
const MAX_CHECKING_SYNCS_PER_PEER: usize = 2;

/// Whether [`check_signatures`] would check anything in `nodes` and `beats`
/// (cheaply, with no signature checked): a record claiming to be newer than
/// the one held, or a beat newer than the last from a node that beats.
fn needs_checks(state: &SharedState, nodes: &[NodeInfo], beats: &[crate::config::SignedBeat]) -> bool {
    if state.tls.is_none() {
        return false;
    }
    let known = state.known_nodes.load();
    let newer_record = nodes.iter().any(|node| {
        node.signed.as_ref().is_some_and(|signed| {
            let held = known.get(&node.id).filter(|n| n.signed.is_some()).map(|n| n.record_version);
            matches!(crate::signing::claimed_heartbeat(signed), Some(claimed) if held.is_none_or(|h| claimed > h))
        })
    });
    newer_record
        || beats.iter().any(|beat| match known.get(&beat.id) {
            Some(node) => node.addr.is_empty() && node.signed.is_some() && beat.beat > node.heartbeat,
            None => state.forgotten_cert(&beat.id).is_some_and(|(last, _)| beat.beat > last),
        })
}

/// Take `beat` for the node it names if it's newer than what we have and
/// signed with the key of that node's (already verified) record. A rising
/// beat is the only sign of life from a node with no address.
fn merge_beat(kn: &mut std::collections::HashMap<String, NodeInfo>, beat: crate::config::SignedBeat, now: Instant) {
    let Some(node) = kn.get_mut(&beat.id) else { return };
    if beat.beat <= node.heartbeat || node.signed.is_none() {
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
            let updated = adopt_from_peer(state, config, "config update received", peer_cn).await;
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
                let removed = kn.remove(&id);
                let last = removed.as_ref().map(|n| n.heartbeat);
                if last.is_some() {
                    info!("node left: {id}");
                }
                // Bury it so gossip still carrying it doesn't re-add it; a
                // restart sends a newer heartbeat and is let back in.
                let version = left.as_ref().map_or(0, |(v, _)| *v);
                state.bury_with(&id, heartbeat.max(last.unwrap_or(0)).max(version), left);
                if removed.is_some_and(|n| n.signed.is_some()) {
                    state.mark_buried_signed(&id);
                }
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
/// beat, and our digest, and our config if `with_config` (the peer is
/// behind). The peer's reply fills in whatever we're missing; it learns
/// what we have that it lacks when it syncs with us in turn. (A peer with
/// an address hears every NAT'd node's beat from the node itself.)
async fn build_compact_sync(state: &Arc<SharedState>, want_beats: bool, with_config: bool) -> Message {
    let config = state.dynamic_config.read().await.clone();
    let have = Some(digest(state, &config, want_beats));
    let nodes = own_entry(state).into_iter().collect();
    let (config, config_elided) = if with_config { (config, false) } else { (placeholder_config(), true) };
    Message::Sync { config, nodes, beats: own_beat(state).into_iter().collect(), have, config_elided }
}

/// The Sync to send `addr`: compact if it speaks delta sync (with our
/// config if it was behind as of its last reply), else full.
async fn sync_msg_for(state: &Arc<SharedState>, addr: &str, want_beats: bool) -> Message {
    let peer = state.delta_peers.lock().unwrap().get(addr).copied();
    match peer {
        Some(peer) => {
            let behind = config_newer_than(&*state.dynamic_config.read().await, peer);
            build_compact_sync(state, want_beats, behind).await
        }
        None => build_sync_msg(state, want_beats).await,
    }
}

/// Whether `config` is newer than the one a peer reported as (version,
/// hash): a higher version, or the same one with different content (the
/// peer then settles it with `supersedes`).
fn config_newer_than(config: &crate::config::DynamicConfig, (version, hash): (u64, u64)) -> bool {
    config.version > version || (config.version == version && config_hash(config) != hash)
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
    // Which config we have, for the sender to tell whether we're behind.
    let our_config = messages::SyncDigest {
        config_version: config.version,
        config_hash: config_hash(&config),
        ..Default::default()
    };
    let (config, config_elided) = if ours_newer { (config, false) } else { (placeholder_config(), true) };
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
        // Its presence tells the sender we speak delta sync; what's in it,
        // which config we have.
        have: Some(our_config),
        config_elided,
    }
}

/// A stand-in for an elided config: empty, at version 0. Meant only for
/// nodes that know to ignore it (`config_elided`), but a node too old to
/// know that may answer at an address remembered as a newer one's (after a
/// rollback, or a reused address): at version 0 it is never newer than
/// anything, so it can't empty that node's config.
fn placeholder_config() -> crate::config::DynamicConfig {
    crate::config::DynamicConfig::default()
}

/// What we hold: every signed node record's version (ours and departed
/// nodes' included) and our config's version and hash.
fn digest(state: &SharedState, config: &crate::config::DynamicConfig, want_beats: bool) -> messages::SyncDigest {
    let now = Instant::now();
    let mut records: Vec<(String, u64)> = state
        .known_nodes
        .load()
        .values()
        .filter(|n| n.signed.is_some())
        // A node reached at an address a relayer gave, which doesn't work:
        // left out, so peers send its record again with where they reach it
        // (a record we hold is otherwise never resent, and a NAT'd node
        // never hears from the node itself).
        .filter(|n| !(n.addr_relayed && !crate::state::node_is_live(n, now)))
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
    messages::SyncDigest { records, config_version: config.version, config_hash: config_hash(config), want_beats, beats, keep_open: false }
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
    let subscriber = matches!(state.config.mode, crate::config::Mode::Subscribe);
    let mut cache = state.own_record.lock().unwrap();
    if let Some(own) = cache.as_ref() {
        let r = &own.record;
        if r.addr == addr && r.health == health && r.relays == relays && r.left == left && r.subscriber == subscriber {
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
        subscriber,
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

    /// No config can take the cluster to a version so high that no change
    /// could follow: one at most `MAX_VERSION_STEP` past the one held is
    /// taken, from a peer or from the file; one past that is not.
    #[tokio::test(start_paused = true)]
    async fn no_config_takes_the_cluster_to_the_last_version() {
        use crate::config::MAX_VERSION_STEP;
        let state = state_for("me");
        let at = |version: u64| DynamicConfig { version, changed_by: "x".into(), ..named_services(&["api"]) };
        {
            let mut dc = state.dynamic_config.write().await;
            // Holding nothing yet, a node takes any version changes can
            // follow (the bound is absolute there), but not the last ones.
            assert!(!adopt_if_newer(&state, &mut dc, at(u64::MAX), "test"), "a jump to the end");
            assert!(adopt_if_newer(&state, &mut dc, at(1), "test"));
            // Holding one, at most MAX_VERSION_STEP past it.
            assert!(!adopt_if_newer(&state, &mut dc, at(MAX_VERSION_STEP + 2), "test"));
            assert!(adopt_if_newer(&state, &mut dc, at(MAX_VERSION_STEP + 1), "test"), "far, but within the bound");
            assert!(!adopt_if_newer(&state, &mut dc, at(u64::MAX), "test"));
            assert_eq!(dc.version, MAX_VERSION_STEP + 1);
        }

        // A hand edit to the file, the same.
        let memory = DynamicConfig { version: 5, ..named_services(&["old"]) };
        let (taken, _) = poll_once(DynamicConfig { version: u64::MAX, ..named_services(&["edited"]) }, memory.clone()).await;
        assert_eq!(taken, memory, "refused");
        // And a config already at the last version (written by hand on
        // every node): changes to it are refused, not wrapped to version 0.
        let last = DynamicConfig { version: u64::MAX, ..named_services(&["old"]) };
        let (taken, _) = poll_once(DynamicConfig { version: u64::MAX, ..named_services(&["edited"]) }, last.clone()).await;
        assert_eq!(taken, last);
    }

    /// A hand edit at the same version is taken whatever it changes, not
    /// only services and routes: a revocation, a policy.
    #[tokio::test(start_paused = true)]
    async fn poll_takes_a_hand_edit_to_any_setting() {
        let memory = DynamicConfig { version: 5, ..named_services(&["api"]) };
        let mut file = memory.clone();
        file.revoked.nodes.push("x".into());
        let (taken, _) = poll_once(file, memory.clone()).await;
        assert_eq!((taken.version, taken.revoked.nodes.clone()), (6, vec!["x".to_string()]));

        let mut file = memory.clone();
        file.policies.push(crate::config::Policy { service: "api".into(), allow: vec!["n2".into()] });
        let (taken, _) = poll_once(file, memory).await;
        assert_eq!((taken.version, taken.policies.len()), (6, 1));
    }

    /// The daemon ran version 5; while it was stopped, its file was edited.
    /// It starts on version 5 (see `edited_while_stopped` in main.rs), and
    /// its poll takes the edit only once caught up with the cluster.
    async fn started_after_an_offline_edit(edit: DynamicConfig) -> (Arc<SharedState>, std::path::PathBuf) {
        started_after_an_offline_edit_knowing(edit, vec!["10.9.9.9:1".into()]).await
    }

    /// [`started_after_an_offline_edit`], with these `knownNodes`.
    async fn started_after_an_offline_edit_knowing(edit: DynamicConfig, known_nodes: Vec<String>) -> (Arc<SharedState>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("stouter_offline_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stouter.json");
        let path_s = path.to_str().unwrap().to_string();
        let ran = DynamicConfig { version: 5, ..named_services(&["api"]) };
        crate::history::record(&path_s, &ran);
        let cfg = Config { node_id: "me".into(), known_nodes, dynamic_config: edit, ..Config::default() };
        cfg.save(&path_s).unwrap();
        let state = SharedState::new(Config { dynamic_config: ran, ..cfg }, path_s, CancellationToken::new(), None);
        state.edited_offline.store(true, std::sync::atomic::Ordering::Relaxed);
        (state, dir)
    }

    async fn poll_for_a_while(state: &Arc<SharedState>) {
        let poll = tokio::spawn(run_config_poll_loop(state.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        poll.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn an_offline_edit_is_taken_once_caught_up_or_kept_aside() {
        let edit = DynamicConfig { version: 5, ..named_services(&["api", "edited"]) };
        let file_of = |state: &SharedState| Config::load(&state.config_path).unwrap().dynamic_config;

        // In step with the cluster: nothing until the first sync, then the
        // edit as version 6, by this node.
        let (state, dir) = started_after_an_offline_edit(edit.clone()).await;
        poll_for_a_while(&state).await;
        assert_eq!(state.dynamic_config.read().await.version, 5, "not before catching up");
        assert_eq!(service_names(&file_of(&state)), ["api", "edited"], "the edit stays in the file");
        state.synced.store(true, std::sync::atomic::Ordering::Relaxed);
        poll_for_a_while(&state).await;
        let dc = state.dynamic_config.read().await.clone();
        assert_eq!((dc.version, service_names(&dc), dc.changed_by.as_str()), (6, vec!["api".into(), "edited".into()], "me"));
        let _ = std::fs::remove_dir_all(&dir);

        // The cluster moved on while it was stopped: its config arrives
        // first, and the edit is kept aside, not dropped or sent out.
        let (state, dir) = started_after_an_offline_edit(edit.clone()).await;
        let v9 = DynamicConfig { version: 9, changed_by: "a".into(), ..named_services(&["api", "from-a"]) };
        {
            let mut dc = state.dynamic_config.write().await;
            assert!(adopt_if_newer(&state, &mut dc, v9, "test"));
        }
        state.synced.store(true, std::sync::atomic::Ordering::Relaxed);
        poll_for_a_while(&state).await;
        let dc = state.dynamic_config.read().await.clone();
        assert_eq!((dc.version, service_names(&dc)), (9, vec!["api".into(), "from-a".into()]));
        let aside = Config::load(&format!("{}.unapplied", state.config_path)).unwrap().dynamic_config;
        assert_eq!(service_names(&aside), ["api", "edited"]);
        let _ = std::fs::remove_dir_all(&dir);

        // An edit that breaks the config gets the checks any edit gets.
        let mut broken = DynamicConfig { version: 5, ..named_services(&["api"]) };
        broken.routes.push(serde_json::from_value(serde_json::json!({"service": "nope", "backends": [{}]})).unwrap());
        let (state, dir) = started_after_an_offline_edit(broken).await;
        state.synced.store(true, std::sync::atomic::Ordering::Relaxed);
        poll_for_a_while(&state).await;
        assert_eq!(state.dynamic_config.read().await.version, 5, "refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A node with no `knownNodes` (the seed the others list) knows no peer
    /// when it starts, but they are on their way: its offline edit still
    /// waits for a sync, so a cluster that moved on keeps it aside.
    #[tokio::test(start_paused = true)]
    async fn a_seed_nodes_offline_edit_waits_for_a_sync_too() {
        let edit = DynamicConfig { version: 5, ..named_services(&["api", "edited"]) };
        let (state, dir) = started_after_an_offline_edit_knowing(edit.clone(), vec![]).await;
        poll_for_a_while(&state).await;
        assert_eq!(state.dynamic_config.read().await.version, 5, "knowing no peer yet is no reason to take it");
        let v9 = DynamicConfig { version: 9, changed_by: "a".into(), ..named_services(&["api", "from-a"]) };
        {
            let mut dc = state.dynamic_config.write().await;
            assert!(adopt_if_newer(&state, &mut dc, v9, "test"));
        }
        state.synced.store(true, std::sync::atomic::Ordering::Relaxed);
        poll_for_a_while(&state).await;
        assert_eq!(state.dynamic_config.read().await.version, 9);
        let aside = Config::load(&format!("{}.unapplied", state.config_path)).unwrap().dynamic_config;
        assert_eq!(service_names(&aside), ["api", "edited"], "kept aside, not lost");
        let _ = std::fs::remove_dir_all(&dir);

        // Alone for good: taken once the catch-up time is over.
        let (state, dir) = started_after_an_offline_edit_knowing(edit, vec![]).await;
        *state.catch_up_until.lock().unwrap() = std::time::Instant::now();
        poll_for_a_while(&state).await;
        let dc = state.dynamic_config.read().await.clone();
        assert_eq!((dc.version, service_names(&dc)), (6, vec!["api".into(), "edited".into()]));
        let _ = std::fs::remove_dir_all(&dir);
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
        dc.upsert_service("g", api.clone()).unwrap();
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

    /// Configs claiming a newer version, large, with junk signatures (a
    /// member stalling the node): checked without the config's lock, so its
    /// readers don't wait behind them; one refused isn't checked again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bogus_signed_configs_dont_hold_up_the_configs_readers() {
        let c = signed_cluster();
        let bogus = |n: usize| {
            let mut dc = DynamicConfig { version: 2, ..named_services(&["api"]) };
            dc.service_groups[0].services[0].meta.insert("pad".into(), format!("{n}{}", "x".repeat(1_500_000)));
            let config_json = serde_json::to_string(&dc).unwrap();
            dc.signature = Some(crate::config::ConfigSignature { config_json, signer_cert: "00".into(), signature: "00".into() });
            dc
        };
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut senders = Vec::new();
        for i in 0..8 {
            let (node, stop) = (c.node.clone(), stop.clone());
            let peer = format!("m{i}");
            senders.push(tokio::spawn(async move {
                let mut n = i * 1000;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    n += 1;
                    apply_message(&node, Message::ConfigUpdate { config: bogus(n) }, Some(&peer)).await;
                }
            }));
        }
        let mut slowest = Duration::ZERO;
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            let asked = std::time::Instant::now();
            drop(c.node.dynamic_config.read().await);
            slowest = slowest.max(asked.elapsed());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for sender in senders {
            sender.await.unwrap();
        }
        assert!(slowest < Duration::from_millis(50), "a reader waited {slowest:?}");
        assert_eq!(c.node.dynamic_config.read().await.version, 1, "none taken");

        // The same one again isn't checked again.
        let again = bogus(0);
        apply_message(&c.node, Message::ConfigUpdate { config: again.clone() }, Some("m")).await;
        let refused = c.node.refused_configs.lock().unwrap().len();
        apply_message(&c.node, Message::ConfigUpdate { config: again }, Some("m")).await;
        assert_eq!(c.node.refused_configs.lock().unwrap().len(), refused, "remembered, not checked again");
        let _ = std::fs::remove_dir_all(&c.dir);
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

    /// A config keeps spreading after the certificate that signed it
    /// expires: a node that hadn't taken it yet (new, restarted, cut off)
    /// still does. (It used to be refused, splitting the cluster.)
    #[tokio::test]
    async fn a_signed_config_keeps_spreading_after_its_signers_certificate_expires() {
        let c = signed_cluster();
        let admin_short = crate::pki::load_or_create_with(c.dir.to_str().unwrap(), "admin", ::time::Duration::seconds(2)).unwrap();
        let v2 = signed_by(&admin_short, &DynamicConfig { version: 2, changed_by: "admin".into(), ..named_services(&["api", "new"]) });
        apply_message(&c.node, Message::ConfigUpdate { config: v2.clone() }, None).await;
        assert_eq!(c.node.dynamic_config.read().await.version, 2);

        // A second node of the cluster, still at version 1.
        let path2 = c.dir.join("n2.json").to_str().unwrap().to_string();
        let cfg2 = Config {
            node_id: "n2".into(),
            admin_cns: vec!["admin".into()],
            require_signed_config: true,
            dynamic_config: DynamicConfig { version: 1, ..named_services(&["api"]) },
            ..Config::default()
        };
        cfg2.save(&path2).unwrap();
        let n2 = crate::state::test_support::tls_state_with(&c.dir, cfg2, path2);
        tokio::time::sleep(Duration::from_secs(3)).await;
        let relayed = c.node.dynamic_config.read().await.clone();
        apply_message(&n2, Message::ConfigUpdate { config: relayed }, None).await;
        assert_eq!(n2.dynamic_config.read().await.version, 2, "n2 takes the cluster's config");

        // But the expired key signs nothing new that counts: dated now, its
        // certificate isn't valid...
        let v3 = signed_by(&admin_short, &DynamicConfig { version: 3, changed_by: "admin".into(), ..named_services(&["api"]) });
        apply_message(&n2, Message::ConfigUpdate { config: v3 }, None).await;
        assert_eq!(n2.dynamic_config.read().await.version, 2);
        // ...and dated back into its life, a node that has caught up after
        // starting refuses it, as does a push.
        let signer = crate::signing::Signer::from_material(&admin_short).unwrap();
        let signed_v2_at = n2.dynamic_config.read().await.signed_at.unwrap();
        let back = DynamicConfig { version: 3, changed_by: "admin".into(), ..named_services(&["api", "backdoor"]) };
        let back = crate::signing::sign_at(&back, &signer, signed_v2_at).unwrap();
        *n2.catch_up_until.lock().unwrap() = std::time::Instant::now();
        apply_message(&n2, Message::ConfigUpdate { config: back.clone() }, None).await;
        assert_eq!(n2.dynamic_config.read().await.version, 2, "a running node refuses it");
        let current = n2.dynamic_config.read().await.clone();
        let pushed = accept_push(&n2, &current, back.clone(), 2).expect_err("refused");
        assert!(matches!(pushed, crate::config::MutateErr::Unauthorized(ref why) if why.contains("expired")), "{pushed:?}");
    }

    /// Without requireSignedConfig or any admins, `config push` from this
    /// host (signed with the node's own certificate) is taken, as any other
    /// change from the CLI here is; another member's isn't. With
    /// requireSignedConfig, only an admin's.
    #[test]
    fn a_push_from_the_nodes_own_cli_is_taken_where_changes_neednt_be_signed() {
        let dir = std::env::temp_dir().join(format!("stouter_ownpush_{}_{}", std::process::id(), rand::random::<u64>()));
        let open = crate::state::test_support::tls_state_with(
            &dir,
            Config { node_id: "n1".into(), dynamic_config: DynamicConfig { version: 1, ..named_services(&["api"]) }, ..Config::default() },
            String::new(),
        );
        let material = |id: &str| crate::pki::load_or_create(dir.to_str().unwrap(), id).unwrap();
        let v2 = DynamicConfig { version: 2, ..named_services(&["api", "web"]) };
        let current = DynamicConfig { version: 1, ..named_services(&["api"]) };
        assert!(accept_push(&open, &current, signed_by(&material("n1"), &v2), 1).is_ok());
        assert!(matches!(
            accept_push(&open, &current, signed_by(&material("mallory"), &v2), 1),
            Err(crate::config::MutateErr::Unauthorized(_))
        ));

        let c = signed_cluster();
        let current = c.node.dynamic_config.try_read().unwrap().clone();
        let own = crate::pki::load_or_create(c.dir.to_str().unwrap(), "n1").unwrap();
        assert!(matches!(
            accept_push(&c.node, &current, signed_by(&own, &v2), 1),
            Err(crate::config::MutateErr::Unauthorized(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
        // Even an admin's signature can't take the config to a version so
        // high no change could follow (say, made with a leaked key).
        let last = DynamicConfig { version: u64::MAX, changed_by: "admin".into(), ..named_services(&[]) };
        assert_eq!(rpc(push(signed_by(&c.admin, &last), 2)).await, Err(AdminErrCode::Validation));
        assert_eq!(c.node.dynamic_config.read().await.version, 2);
    }

    /// Two admins sign different configs at one version, hours apart (say,
    /// on either side of a partition). Every node ends up with the same one,
    /// whichever it had first: the later, as a node holding it refuses the
    /// earlier as signed too early.
    #[tokio::test]
    async fn a_tie_between_configs_signed_hours_apart_converges_on_the_one_revoking_more() {
        let c = signed_cluster();
        let path2 = c.dir.join("n2.json").to_str().unwrap().to_string();
        let cfg2 = Config {
            node_id: "n2".into(),
            admin_cns: vec!["admin".into()],
            require_signed_config: true,
            dynamic_config: DynamicConfig { version: 1, ..named_services(&["api"]) },
            ..Config::default()
        };
        cfg2.save(&path2).unwrap();
        let n2 = crate::state::test_support::tls_state_with(&c.dir, cfg2, path2);
        let signer = crate::signing::Signer::from_material(&c.admin).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let mut x = DynamicConfig { version: 2, changed_by: "admin".into(), ..named_services(&["api"]) };
        x.revoked.nodes.push("mallory".into());
        let x = crate::signing::sign_at(&x, &signer, now - 7200).unwrap();
        let y = DynamicConfig { version: 2, changed_by: "admin".into(), ..named_services(&["api", "web"]) };
        let y = crate::signing::sign_at(&y, &signer, now).unwrap();
        // n1 got y first, n2 got x first; then each hears the other.
        apply_message(&c.node, Message::ConfigUpdate { config: y.clone() }, None).await;
        apply_message(&n2, Message::ConfigUpdate { config: x.clone() }, None).await;
        apply_message(&c.node, Message::ConfigUpdate { config: x.clone() }, None).await;
        apply_message(&n2, Message::ConfigUpdate { config: y.clone() }, None).await;
        let (a, b) = (c.node.dynamic_config.read().await.clone(), n2.dynamic_config.read().await.clone());
        assert_eq!(a, b, "one config at version 2 everywhere");
        // The signer's certificate is valid, so neither is refused as
        // signed too early: the tie goes to the revocation, as any does.
        assert_eq!(a.revoked.nodes, ["mallory"], "the one revoking more");
    }

    /// After a partition heals: side A made v2 and v3 early, side B made its
    /// own v2 hours later. B's nodes take A's v3, though it was signed well
    /// before the v2 they hold (the signer's certificate is valid).
    #[tokio::test]
    async fn a_higher_version_signed_hours_earlier_is_taken() {
        let c = signed_cluster();
        let signer = crate::signing::Signer::from_material(&c.admin).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let side_b = DynamicConfig { version: 2, changed_by: "admin".into(), ..named_services(&["api", "b"]) };
        let side_a = DynamicConfig { version: 3, changed_by: "admin".into(), ..named_services(&["api", "a", "a2"]) };
        apply_message(&c.node, Message::ConfigUpdate { config: crate::signing::sign_at(&side_b, &signer, now).unwrap() }, None).await;
        apply_message(&c.node, Message::ConfigUpdate { config: crate::signing::sign_at(&side_a, &signer, now - 7200).unwrap() }, None).await;
        let held = c.node.dynamic_config.read().await.clone();
        assert_eq!((held.version, service_names(&held)), (3, vec!["api".to_string(), "a".into(), "a2".into()]));
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
        let Message::Sync { nodes, have, config_elided, .. } = build_compact_sync(&sub, false, false).await else { unreachable!() };
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
    async fn a_hand_edit_that_breaks_the_config_isnt_taken() {
        let dir = std::env::temp_dir().join(format!("stouter_edit_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stouter.json").to_str().unwrap().to_string();
        let with = |version: u64, domains: [&str; 2]| -> DynamicConfig {
            serde_json::from_value(serde_json::json!({"version": version, "serviceGroups": [{"name": "g", "services": [
                {"name": "a", "nodeId": "n1", "nodePort": 1, "domains": [domains[0]]},
                {"name": "b", "nodeId": "n1", "nodePort": 2, "domains": [domains[1]]},
            ]}]}))
            .unwrap()
        };
        let cfg = Config { node_id: "n1".into(), dynamic_config: with(1, ["a.example", "b.example"]), ..Config::default() };
        cfg.save(&path).unwrap();
        let state = SharedState::new(cfg.clone(), path.clone(), CancellationToken::new(), None);
        // An edit that has both services claim one domain.
        Config { dynamic_config: with(2, ["a.example", "a.example"]), ..cfg }.save(&path).unwrap();
        let poll = tokio::spawn(run_config_poll_loop(state.clone()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(state.dynamic_config.read().await.version, 1, "refused, like the same change from the CLI");
        state.shutdown.cancel();
        let _ = poll.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_in_the_config_file_is_never_silently_written_over() {
        let dir = std::env::temp_dir().join(format!("stouter_persist_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stouter.json");
        let path_str = path.to_str().unwrap().to_string();
        let config_at = |version: u64, port: u16| -> DynamicConfig {
            serde_json::from_value(serde_json::json!({
                "version": version,
                "serviceGroups": [{"name": "g", "services": [{"name": "api", "nodeId": "n1", "nodePort": port}]}],
            }))
            .unwrap()
        };
        let cfg = Config { node_id: "n1".into(), dynamic_config: config_at(5, 3000), ..Config::default() };
        cfg.save(&path_str).unwrap();
        let state = SharedState::new(cfg.clone(), path_str.clone(), CancellationToken::new(), None);
        let aside = dir.join("stouter.json.unapplied");
        let file_port = || crate::config::Config::load(&path_str).unwrap().dynamic_config.service_groups[0].services[0].node_port;

        // Nothing changed in the file: a newer config is simply written.
        persist_state_with(&state, &config_at(6, 3000));
        assert!(!aside.exists());

        // Someone edits the file (by hand, or a CLI on a stale copy), and a
        // newer config arrives before it's applied: it's kept, not lost.
        let mut edited = crate::config::Config::load(&path_str).unwrap();
        edited.dynamic_config.service_groups[0].services[0].node_port = 4444;
        edited.save(&path_str).unwrap();
        persist_state_with(&state, &config_at(7, 3000));
        assert_eq!(file_port(), 3000, "the file holds the cluster's config");
        let kept = crate::config::Config::load(aside.to_str().unwrap()).unwrap();
        assert_eq!(kept.dynamic_config.service_groups[0].services[0].node_port, 4444, "and the edit is kept aside");
        std::fs::remove_file(&aside).unwrap();

        // A file that already says what's being written (the CLI saved the
        // config it pushed) is no edit to keep.
        let mut same = crate::config::Config::load(&path_str).unwrap();
        same.dynamic_config = config_at(8, 3000);
        same.save(&path_str).unwrap();
        persist_state_with(&state, &config_at(8, 3000));
        assert!(!aside.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn one_sync_gets_only_so_many_new_records_checked() {
        let (dir, sub, _, _) = signed_nodes_setup();
        let signer = |id: &str| {
            crate::signing::Signer::from_material(&crate::pki::load_or_create(dir.to_str().unwrap(), id).unwrap()).unwrap()
        };
        let count = MAX_NEW_RECORDS_PER_SYNC + 20;
        let nodes: Vec<NodeInfo> =
            (0..count).map(|i| record(&format!("n{i}"), &format!("10.1.0.{}:1", i % 250), 10, &signer(&format!("n{i}")))).collect();
        merge_sync(&sub, None, nodes.clone(), vec![], &SyncSource::default()).await;
        assert_eq!(sub.known_nodes.load().len(), MAX_NEW_RECORDS_PER_SYNC, "the rest wait for the next sync");
        merge_sync(&sub, None, nodes, vec![], &SyncSource::default()).await;
        assert_eq!(sub.known_nodes.load().len(), count);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_config_is_indexed_in_the_same_step_it_is_taken() {
        let state = state_for("me");
        let newer: DynamicConfig = serde_json::from_value(serde_json::json!({
            "version": 9, "serviceGroups": [{"name": "g", "services": [{"name": "api", "nodeId": "n1", "nodePort": 1}]}],
        }))
        .unwrap();
        {
            let mut dc = state.dynamic_config.write().await;
            assert!(adopt_if_newer(&state, &mut dc, newer, "test"));
        }
        // Nothing else runs (as when a sync is cut off by its timeout right
        // here): the index already has it.
        assert!(state.service_index.load().contains_key(&("n1".to_string(), "api".to_string())));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn syncs_reuse_one_connection_round_after_round() {
        use crate::state::test_support::{mesh_pair, tls_state};
        let dir = std::env::temp_dir().join(format!("stouter_kept_{}_{}", std::process::id(), rand::random::<u64>()));
        let (a, b) = (tls_state(&dir, "a"), tls_state(&dir, "b"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let (b, accepted) = (b.clone(), accepted.clone());
            tokio::spawn(async move {
                loop {
                    let (tcp, peer) = listener.accept().await.unwrap();
                    accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let b = b.clone();
                    tokio::spawn(async move {
                        if let Ok(mesh) = b.tls_accept(tcp).await {
                            dispatch_connection(mesh, peer.to_string(), b).await;
                        }
                    });
                }
            });
        }
        let connections = || accepted.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..3 {
            sync_with_peer(&a, &addr, false).await.unwrap();
        }
        assert_eq!(connections(), 1, "three rounds, one connection");
        assert_eq!(b.in_flight.current(), 0, "a kept connection waiting isn't work in flight");

        // One that died since (the peer restarted, say): a new one, at once.
        let (dead, other_end) = mesh_pair(&a, &b).await;
        drop(other_end);
        a.gossip_conns.lock().unwrap().insert(addr.clone(), dead);
        sync_with_peer(&a, &addr, false).await.unwrap();
        assert_eq!(connections(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn made_up_nodes_are_held_only_so_many() {
        let state = state_for("me");
        // A peer, in a cluster without signed records, invents nodes.
        let fakes: Vec<NodeInfo> = (0..MAX_UNSIGNED_NODES + 100)
            .map(|i| NodeInfo { id: format!("fake-{i}"), addr: format!("10.9.{}.{}:1", i / 256, i % 256), heartbeat: 1, ..Default::default() })
            .collect();
        merge_sync(&state, None, fakes, vec![], &SyncSource { cn: Some("m"), ..Default::default() }).await;
        assert_eq!(state.known_nodes.load().len(), MAX_UNSIGNED_NODES);

        // Forgetting them all keeps only so many addresses to retry.
        for i in 0..1000 {
            state.remember_forgotten(&format!("fake-{i}"), &format!("10.8.{}.{}:1", i / 256, i % 256));
        }
        assert!(state.forgotten_addrs().len() <= 512);
    }

    #[tokio::test]
    async fn unsigned_gossip_cant_shut_out_a_nodes_beats() {
        let (dir, sub, x, _) = signed_nodes_setup();
        // Before `sub` has seen x signed (say, just after a restart), a peer
        // claims x's heartbeat is as high as it goes.
        let claim = NodeInfo { id: "x".into(), heartbeat: u64::MAX, ..Default::default() };
        merge_sync(&sub, None, vec![claim], vec![], &SyncSource { cn: Some("m"), ..Default::default() }).await;
        // x's own record replaces that, so its beats count.
        merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![], &SyncSource::default()).await;
        assert_eq!(sub.known_nodes.load()["x"].heartbeat, 10);
        age(&sub, "x", Duration::from_secs(31)).await;
        merge_sync(&sub, None, vec![], vec![crate::signing::sign_beat("x", 50, &x).unwrap()], &SyncSource::default()).await;
        assert!(crate::state::node_is_live(&sub.known_nodes.load()["x"], Instant::now()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_forgotten_natd_node_comes_back_when_it_beats() {
        let (dir, sub, x, m) = signed_nodes_setup();
        let beat = |n: u64, who: &crate::signing::Signer| crate::signing::sign_beat("x", n, who).unwrap();
        merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![beat(50, &x)], &SyncSource::default()).await;
        // Out of touch for longer than the expiry: forgotten.
        age(&sub, "x", NODE_EXPIRE_AFTER + Duration::from_secs(1)).await;
        expire_silent_nodes(&sub).await;
        assert!(!sub.known_nodes.load().contains_key("x"));

        // Its record hasn't changed, so relayed it doesn't bring it back.
        let relayed = SyncSource { cn: Some("b"), ..Default::default() };
        merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![], &relayed).await;
        assert!(!sub.known_nodes.load().contains_key("x"));
        // Nor does an old beat, or one someone else signed.
        merge_sync(&sub, None, vec![], vec![beat(50, &x), beat(90, &m)], &relayed).await;
        assert!(!sub.known_nodes.load().contains_key("x"));
        // A newer beat of its own does, live at once.
        merge_sync(&sub, None, vec![], vec![beat(60, &x)], &relayed).await;
        let back = sub.known_nodes.load()["x"].clone();
        assert_eq!(back.heartbeat, 60);
        assert!(crate::state::node_is_live(&back, Instant::now()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_newer_config_reaches_a_peer_from_a_node_nobody_dials() {
        let (a, b) = (state_for("a"), state_for("b"));
        a.dynamic_config.write().await.version = 6;
        b.dynamic_config.write().await.version = 5;
        // a syncs with b, which says (in its reply) which config it has.
        a.delta_peers.lock().unwrap().insert("b:1".into(), (0, 0));
        let Message::Sync { have, .. } = sync_msg_for(&a, "b:1", false).await else { unreachable!() };
        let Message::Sync { have: Some(b_has), .. } = build_sync_reply(&b, have.as_ref().unwrap()).await else {
            unreachable!()
        };
        assert_eq!(b_has.config_version, 5);
        a.delta_peers.lock().unwrap().insert("b:1".into(), (b_has.config_version, b_has.config_hash));

        // So a's next sync carries its config, and b adopts it.
        let Message::Sync { config, nodes, beats, config_elided, .. } = sync_msg_for(&a, "b:1", false).await else {
            unreachable!()
        };
        assert!(!config_elided);
        merge_sync(&b, Some(config), nodes, beats, &SyncSource { cn: Some("a"), ..Default::default() }).await;
        assert_eq!(b.dynamic_config.read().await.version, 6);
        // Once b has caught up, syncs leave the config out again.
        let caught_up = b.dynamic_config.read().await.clone();
        a.delta_peers.lock().unwrap().insert("b:1".into(), (caught_up.version, config_hash(&caught_up)));
        let Message::Sync { config_elided, .. } = sync_msg_for(&a, "b:1", false).await else { unreachable!() };
        assert!(config_elided);
    }

    #[tokio::test]
    async fn an_older_node_never_takes_the_placeholder_for_a_config() {
        // What a node from before delta sync reads from a Sync.
        #[derive(serde::Deserialize)]
        #[serde(tag = "type", rename_all = "camelCase")]
        enum OldMessage {
            Sync { config: DynamicConfig },
        }
        let a = state_for("a");
        *a.dynamic_config.write().await = serde_json::from_value(serde_json::json!({
            "version": 9, "serviceGroups": [{"name": "g", "services": [{"name": "api", "nodeId": "a", "nodePort": 1}]}],
        }))
        .unwrap();
        let bytes = encode_message(&build_compact_sync(&a, false, false).await).unwrap();
        let OldMessage::Sync { config } = rmp_serde::from_slice(&bytes).unwrap();
        // An older node at version 8 keeps its own.
        let theirs = DynamicConfig { version: 8, ..a.dynamic_config.read().await.clone() };
        assert!(!config.supersedes(&theirs));
        assert_eq!(config.version, 0);
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
        assert!(beats_in(&build_compact_sync(&sub, true, false).await).is_empty());
        assert!(beats_in(&build_sync_msg(&sub, true).await).is_empty());
        let Message::Sync { have: Some(have), .. } = build_compact_sync(&sub, true, false).await else { unreachable!() };
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
        state.delta_peers.lock().unwrap().insert("new:1".into(), (0, config_hash(&DynamicConfig::default())));
        let Message::Sync { config_elided, .. } = sync_msg_for(&state, "new:1", false).await else { unreachable!() };
        assert!(config_elided);
    }
    /// A NAT'd observer (which x can never sync with) learns wildcard-bound
    /// x's address only from relayers. One that turns out not to work (a
    /// wrong relayer, or x moved) gives way to another relayer's, and while
    /// it doesn't work the observer's digest leaves x out, so peers resend
    /// x's record with where they reach it.
    #[tokio::test]
    async fn a_relayed_address_that_doesnt_work_gives_way() {
        let (dir, _sub, x, _m) = signed_nodes_setup();
        let natd = crate::state::test_support::tls_state_with(
            &dir,
            Config { node_id: "natd".into(), outbound_only: true, ..Config::default() },
            String::new(),
        );
        let addr_of = || natd.known_nodes.load()["x"].addr.clone();
        let in_digest = || digest(&natd, &DynamicConfig::default(), false).records.iter().any(|(id, _)| id == "x");
        let rec = record("x", "0.0.0.0:8080", 10, &x);
        let from_m = SyncSource { cn: Some("m"), host: Some("10.0.0.66"), contact_addr: None };
        let from_b = SyncSource { cn: Some("b"), host: Some("10.0.0.2"), contact_addr: None };
        let relayed = |addr: &str, rec: &NodeInfo| NodeInfo { addr: addr.into(), ..rec.clone() };

        merge_sync(&natd, None, vec![relayed("10.6.6.6:8080", &rec)], vec![], &from_m).await;
        assert_eq!(addr_of(), "10.6.6.6:8080");
        assert!(in_digest(), "x is live (its grace period): its record isn't asked for again");
        merge_sync(&natd, None, vec![relayed("10.0.0.5:8080", &rec)], vec![], &from_b).await;
        assert_eq!(addr_of(), "10.6.6.6:8080", "a working address isn't swapped for another relayer's");

        // Nothing answers there.
        age(&natd, "x", Duration::from_secs(31)).await;
        assert!(!in_digest(), "x's record is asked for again");
        merge_sync(&natd, None, vec![relayed("10.0.0.5:8080", &rec)], vec![], &from_b).await;
        assert_eq!(addr_of(), "10.0.0.5:8080", "another relayer's address replaces it");

        // x restarts elsewhere: a newer record, relayed with its new address,
        // replaces the old one once that stops working.
        age(&natd, "x", Duration::from_secs(31)).await;
        let rec2 = record("x", "0.0.0.0:8080", 20, &x);
        merge_sync(&natd, None, vec![relayed("10.0.0.9:8080", &rec2)], vec![], &from_b).await;
        assert_eq!(addr_of(), "10.0.0.9:8080");
        assert_eq!(natd.known_nodes.load()["x"].record_version, 20);

        // An address from x itself is never replaced by a relayer's.
        merge_sync(&natd, None, vec![rec2.clone()], vec![], &SyncSource { cn: Some("x"), host: Some("10.0.0.10"), contact_addr: None }).await;
        assert_eq!(addr_of(), "10.0.0.10:8080");
        age(&natd, "x", Duration::from_secs(31)).await;
        assert!(in_digest());
        merge_sync(&natd, None, vec![relayed("10.6.6.6:8080", &rec2)], vec![], &from_m).await;
        assert_eq!(addr_of(), "10.0.0.10:8080");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without requireSignedConfig: once a node with signed records has left,
    /// unsigned gossip about it (which anyone could make up) doesn't bring it
    /// back; only its own word does.
    #[tokio::test]
    async fn unsigned_gossip_doesnt_bring_back_a_departed_signed_node() {
        let (dir, sub, x, _) = signed_nodes_setup();
        let fake = |hb: u64| NodeInfo { id: "x".into(), relay: Some("10.6.6.6:8080".into()), heartbeat: hb, ..Default::default() };
        let from_m = SyncSource { cn: Some("m"), ..Default::default() };

        // It leaves with a signed last record, or (an older build) without.
        for with_record in [true, false] {
            merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![], &SyncSource::default()).await;
            assert!(sub.known_nodes.load()["x"].signed.is_some());
            let left = crate::config::NodeRecord { id: "x".into(), heartbeat: 11, left: true, ..Default::default() };
            let last = with_record.then(|| crate::signing::sign_node(&left, &x).unwrap());
            apply_message(&sub, Message::NodeLeave { id: "x".into(), heartbeat: 11, record: last }, Some("x")).await;
            assert!(!sub.known_nodes.load().contains_key("x"));

            merge_sync(&sub, None, vec![fake(12)], vec![], &from_m).await;
            assert!(!sub.known_nodes.load().contains_key("x"), "made-up gossip doesn't revive it");
            assert!(sub.buried_signed("x"));
            assert_eq!(sub.has_left("x"), with_record);
            assert_eq!(sub.departed_records().len(), usize::from(with_record), "its departure is still passed on");

            // Back for real: its own newer record.
            merge_sync(&sub, None, vec![record("x", "", 20, &x)], vec![], &SyncSource::default()).await;
            assert!(sub.known_nodes.load()["x"].signed.is_some());
            assert!(!sub.has_left("x"));
            sub.update_known_nodes(|kn| {
                kn.remove("x");
            })
            .await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record repeated under one id is checked once per sync, however many
    /// copies a peer sends (each check is a chain and signature verification).
    #[tokio::test]
    async fn copies_of_one_nodes_record_are_checked_once() {
        let (dir, sub, x, m) = signed_nodes_setup();
        merge_sync(&sub, None, vec![record("x", "x:1", 10, &x)], vec![], &SyncSource::default()).await;
        // m's genuine record, claiming a huge heartbeat, relayed as x's.
        let junk = |id: &str| NodeInfo { id: id.into(), ..record("m", "m:1", u64::MAX, &m) };
        let copies: Vec<NodeInfo> = (0..3000).map(|_| junk("x")).collect();
        let distinct: Vec<NodeInfo> = (0..MAX_NEW_RECORDS_PER_SYNC).map(|i| junk(&format!("u{i}"))).collect();
        let t = std::time::Instant::now();
        assert!(check_signatures(&sub, &copies, &[]).records.is_empty());
        let copies_took = t.elapsed();
        let t = std::time::Instant::now();
        assert!(check_signatures(&sub, &distinct, &[]).records.is_empty());
        let distinct_took = t.elapsed();
        // One check against MAX_NEW_RECORDS_PER_SYNC of them.
        assert!(copies_took < distinct_took, "{copies_took:?} for 3000 copies, {distinct_took:?} for {MAX_NEW_RECORDS_PER_SYNC} ids");

        // The record x signed still gets through, wherever it is in the list.
        let newer = record("x", "x:2", 20, &x);
        merge_sync(&sub, None, vec![newer, junk("x")], vec![], &SyncSource::default()).await;
        assert_eq!(sub.known_nodes.load()["x"].addr, "x:2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A beat signed with the key of a certificate that has expired since
    /// the node's record was taken keeps nothing alive.
    #[tokio::test]
    async fn a_beat_from_an_expired_certificate_counts_for_nothing() {
        let (dir, sub, _, _) = signed_nodes_setup();
        let short = crate::pki::load_or_create_with(dir.to_str().unwrap(), "x", ::time::Duration::seconds(2)).unwrap();
        let x = crate::signing::Signer::from_material(&short).unwrap();
        merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![], &SyncSource::default()).await;
        let beat = |n: u64| crate::signing::sign_beat("x", n, &x).unwrap();
        let checked = |b: crate::config::SignedBeat| {
            let key = (b.id.clone(), b.beat, b.signature.clone());
            check_signatures(&sub, &[], &[b]).beats.contains(&key)
        };
        assert!(checked(beat(11)), "while the certificate is valid");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!checked(beat(12)), "not once it has expired");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Beats cost a signature check each, so a sync of junk beats gets few
    /// checked: the newest per node, only for nodes that beat (those with no
    /// address), and at most MAX_BEATS_PER_SYNC.
    #[tokio::test]
    async fn a_sync_full_of_beats_gets_few_checked() {
        let (dir, sub, x, m) = signed_nodes_setup();
        // x has no address (it beats); m has one (it doesn't).
        merge_sync(&sub, None, vec![record("x", "", 10, &x), record("m", "m:1", 10, &m)], vec![], &SyncSource::default())
            .await;
        let real = crate::signing::sign_beat("x", 20_000, &x).unwrap();
        let junk = |id: &str, beat: u64| crate::config::SignedBeat { id: id.into(), beat, ..real.clone() };
        let mut beats: Vec<_> = (11..20_000).map(|b| junk("x", b)).collect();
        beats.push(real.clone());
        beats.extend((11..10_000).map(|b| junk("m", b)));
        beats.extend((0..10_000).map(|i| junk(&format!("u{i}"), 11)));
        let checked = check_signatures(&sub, &[], &beats);
        assert_eq!(checked.beat_checks, 1, "only x's newest");
        assert!(checked.beats.contains(&(real.id.clone(), real.beat, real.signature.clone())));

        // Many nodes that beat, each with a beat to check: capped.
        sub.update_known_nodes(|kn| {
            let x_now = kn["x"].clone();
            for i in 0..MAX_BEATS_PER_SYNC + 500 {
                kn.insert(format!("n{i}"), NodeInfo { id: format!("n{i}"), ..x_now.clone() });
            }
        })
        .await;
        let beats: Vec<_> = (0..MAX_BEATS_PER_SYNC + 500).map(|i| junk(&format!("n{i}"), 30_000)).collect();
        assert_eq!(check_signatures(&sub, &[], &beats).beat_checks, MAX_BEATS_PER_SYNC);

        // Through merge_sync (checked off the async workers), x's beat is taken.
        let newer = crate::signing::sign_beat("x", 20_001, &x).unwrap();
        merge_sync(&sub, None, vec![], vec![newer], &SyncSource::default()).await;
        assert_eq!(sub.known_nodes.load()["x"].heartbeat, 20_001);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A member floods a node with syncs full of records to check: only two
    /// of its syncs are checked at once (the rest are dropped), so another
    /// peer's sync isn't queued behind them all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_peers_junk_syncs_dont_hold_up_anothers() {
        let (dir, sub, x, m) = signed_nodes_setup();
        // A junk record: m's valid cluster certificate, a signature over other text.
        let junk = |i: usize| {
            let mut r = record(&format!("j{i}"), "10.9.9.9:1", 10, &m);
            r.signed.as_mut().unwrap().record_json = format!("{{\"id\":\"j{i}\",\"addr\":\"\",\"heartbeat\":10}}");
            r
        };
        let nodes: Vec<NodeInfo> = (0..MAX_NEW_RECORDS_PER_SYNC).map(junk).collect();
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
        let mut tasks = Vec::new();
        for _ in 0..50 * cpus {
            let (sub, nodes) = (sub.clone(), nodes.clone());
            tasks.push(tokio::spawn(async move {
                merge_sync(&sub, None, nodes, vec![], &SyncSource { cn: Some("m"), ..Default::default() }).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let started = std::time::Instant::now();
        merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![], &SyncSource { cn: Some("x"), ..Default::default() }).await;
        let honest = started.elapsed();
        for task in tasks {
            task.await.unwrap();
        }
        assert!(honest < Duration::from_secs(1), "an honest merge waited {honest:?}");
        assert!(sub.known_nodes.load().contains_key("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn only_syncs_with_something_to_check_wait_their_turn() {
        let (dir, sub, x, _) = signed_nodes_setup();
        merge_sync(&sub, None, vec![record("x", "", 10, &x)], vec![], &SyncSource::default()).await;
        let beat = |n: u64| crate::signing::sign_beat("x", n, &x).unwrap();
        assert!(!needs_checks(&sub, &[record("x", "", 10, &x)], &[]), "a record already held");
        assert!(needs_checks(&sub, &[record("x", "", 11, &x)], &[]), "a newer one");
        assert!(needs_checks(&sub, &[], &[beat(20)]), "a new beat from a node that beats");
        assert!(!needs_checks(&sub, &[], &[beat(5)]), "an old one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A node told to sync with its own address (a shared knownNodes list
    /// naming its real IP while it binds 0.0.0.0) notices its own
    /// certificate answering, and stops dialling that address.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_doesnt_sync_with_itself() {
        use crate::state::test_support::tls_state;
        let dir = std::env::temp_dir().join(format!("stouter_self_{}_{}", std::process::id(), rand::random::<u64>()));
        let a = tls_state(&dir, "a");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let synced = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let (a, synced) = (a.clone(), synced.clone());
            tokio::spawn(async move {
                loop {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let (a, synced) = (a.clone(), synced.clone());
                    tokio::spawn(async move {
                        if let Ok(mut mesh) = a.tls_accept(tcp).await {
                            if recv_message(&mut mesh).await.is_ok() {
                                synced.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                    });
                }
            });
        }
        assert!(dialable(&a, &addr));
        sync_with_peer(&a, &addr, false).await.unwrap();
        assert!(!dialable(&a, &addr), "its own address isn't dialled again");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(synced.load(std::sync::atomic::Ordering::SeqCst), 0, "no sync sent to itself");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn a_revoked_node_is_forgotten_and_gossip_cant_bring_it_back() {
        let (dir, sub, x, _m) = signed_nodes_setup();
        let revoke = |nodes: &[&str], certs: &[String]| {
            let sub = sub.clone();
            let revoked = crate::config::Revoked { nodes: nodes.iter().map(|n| n.to_string()).collect(), certs: certs.to_vec() };
            async move {
                let mut dc = sub.dynamic_config.read().await.clone();
                dc.revoked = revoked;
                sub.index_config(&dc);
                *sub.dynamic_config.write().await = dc;
                sub.rebuild_service_index().await;
            }
        };
        let known = || sub.known_nodes.load().contains_key("x");
        let from_m = SyncSource { cn: Some("m"), ..Default::default() };
        merge_sync(&sub, None, vec![record("x", "x:1", 10, &x)], vec![], &SyncSource::default()).await;
        assert!(known());

        // Revoked by name: forgotten, and nothing about it is believed.
        revoke(&["x"], &[]).await;
        assert!(!known(), "forgotten");
        merge_sync(&sub, None, vec![record("x", "x:1", 20, &x)], vec![], &from_m).await;
        assert!(!known(), "not even a newer record it signed");
        let unsigned = NodeInfo { id: "x".into(), addr: "x:1".into(), heartbeat: 30, ..Default::default() };
        merge_sync(&sub, None, vec![unsigned], vec![], &from_m).await;
        assert!(!known(), "nor unsigned gossip");

        // Unrevoked: back with its next record.
        revoke(&[], &[]).await;
        merge_sync(&sub, None, vec![record("x", "x:1", 40, &x)], vec![], &from_m).await;
        assert!(known());

        // Revoked by certificate: forgotten, and its beats count for nothing...
        let cert = sub.known_nodes.load()["x"].signed.as_ref().unwrap().cert.clone();
        let der: Vec<u8> = (0..cert.len()).step_by(2).map(|i| u8::from_str_radix(&cert[i..i + 2], 16).unwrap()).collect();
        revoke(&[], &[crate::tls::fingerprint_hex(&der)]).await;
        assert!(!known());
        merge_sync(&sub, None, vec![record("x", "x:1", 50, &x)], vec![], &from_m).await;
        assert!(!known(), "a record on the revoked certificate isn't believed");
        // ...but a newer record on a new certificate for x is.
        let reissued = crate::signing::Signer::from_material(&crate::pki::load_or_create(dir.to_str().unwrap(), "x").unwrap()).unwrap();
        merge_sync(&sub, None, vec![record("x", "x:1", 60, &reissued)], vec![], &from_m).await;
        assert!(known(), "back on a new certificate");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
