use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::debug;

use crate::gossip;
use crate::gossip::messages::Message;
use crate::node::tunnel::{connect_local_service, send_tunnel_error};
use crate::state::SharedState;
use crate::tls::MeshStream;

/// Maximum timestamp skew allowed for tunnel authentication.
const MAX_SKEW_MS: u64 = 30_000;

/// How long to wait for a new stream on a session. A stream is acknowledged
/// only once the other end first answers on it, and yamux opens no more
/// while 256 are unacknowledged: tunnels to one slow destination (a NAT'd
/// node with no reverse connection ready, a hung service) can hold up every
/// other tunnel on the session. Past this, the tunnel takes a pooled or new
/// connection instead.
const OPEN_STREAM_TIMEOUT: Duration = Duration::from_secs(1);

/// After a stream couldn't be opened in time, how long new tunnels skip the
/// session rather than each wait out [`OPEN_STREAM_TIMEOUT`] too.
const SATURATED_FOR: Duration = Duration::from_secs(2);

/// Largest first message on a mux stream: a tunnel request, a few KiB even
/// signed (the caller's certificate travels with it). Plain connections
/// allow 8 MiB, as a sync can be that large; a stream never carries one.
pub(crate) const MAX_TUNNEL_REQUEST_BYTES: u32 = 16 * 1024;

/// How long a mux stream may take to send its tunnel request. The side
/// opening it sends the request at once, so a stream still silent after
/// this is stalled on purpose, or dead.
pub(crate) const TUNNEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Most mux streams one peer may have waiting for their tunnel to be set up,
/// across all its sessions: from when a stream opens until its tunnel
/// request has been read and the service (or the NAT'd node, through a
/// reverse connection) is connected. Until then nothing reads what the peer
/// sends on it, so each may hold its full window (256 KiB): this bounds
/// that at 128 MiB a peer, where its 16 sessions' windows would allow 2 GiB.
/// Once connected, a stream's data is read as fast as the service takes it.
pub(crate) const MAX_PENDING_STREAMS_PER_PEER: usize = 512;

/// Most data one mux session may have buffered unread, across its streams:
/// yamux's default is 1 GiB. This is the least yamux allows for its default
/// of 512 streams (256 KiB each), and still far more than one stream needs.
/// (The stream limit stays at yamux's default: reaching it closes the whole
/// session, on either side, so a busy session must never meet it.)
const SESSION_RECEIVE_WINDOW: usize = 512 * 256 * 1024;

/// The yamux settings for both ends of a mux session.
fn session_config() -> yamux::Config {
    let mut config = yamux::Config::default();
    config.set_max_connection_receive_window(Some(SESSION_RECEIVE_WINDOW));
    config
}

// ---------------------------------------------------------------------------
// MuxHandle: cloneable handle for opening outbound yamux streams
// ---------------------------------------------------------------------------

type StreamResult = std::result::Result<yamux::Stream, yamux::ConnectionError>;

/// A cloneable handle to a yamux session for opening outbound streams.
///
/// Internally communicates with a driver task via a channel.
#[derive(Clone)]
pub struct MuxHandle {
    tx: mpsc::Sender<oneshot::Sender<StreamResult>>,
    /// The CN of the server's certificate: the node at the other end.
    peer: Option<String>,
    /// That certificate's SHA-256 fingerprint.
    peer_fingerprint: Option<[u8; 32]>,
    /// Until when new tunnels skip this session (see [`SATURATED_FOR`]).
    saturated_until: Arc<Mutex<Option<Instant>>>,
}

impl MuxHandle {
    /// Whether the session has ended (its driver task stopped).
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Open a new yamux stream on the underlying session.
    pub async fn open_stream(&self) -> StreamResult {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx
            .send(resp_tx)
            .await
            .map_err(|_| yamux::ConnectionError::Closed)?;
        resp_rx
            .await
            .map_err(|_| yamux::ConnectionError::Closed)?
    }
}

// ---------------------------------------------------------------------------
// MuxPool: pool of yamux sessions keyed by connect address
// ---------------------------------------------------------------------------

/// Pool of yamux sessions keyed by connect address.
pub struct MuxPool {
    sessions: Mutex<HashMap<String, MuxHandle>>,
}

impl MuxPool {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Try to open a new yamux stream on an existing session for `addr`.
    ///
    /// Returns `None` if no session exists. Removes dead sessions on error.
    /// With the stream comes a handle to its session: hold it as long as
    /// the stream, since a session ends once nothing holds a handle to it
    /// (the pool may drop its own: see [`MuxPool::retain`]).
    ///
    /// With `expect`, only if the session is with that node: sessions are
    /// keyed by address, and a tunnel must reach the node it is meant for
    /// whatever gossip says about who is at that address.
    pub async fn open_stream(&self, addr: &str, expect: Option<&str>) -> Option<(yamux::Stream, MuxHandle)> {
        let handle = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(addr)?.clone()
        };
        if let Some(expected) = expect {
            if handle.peer.as_deref() != Some(expected) {
                debug!("mux session to {addr} is with {:?}, not {expected}", handle.peer);
                return None;
            }
        }
        if handle.saturated_until.lock().unwrap().is_some_and(|until| Instant::now() < until) {
            return None;
        }

        match tokio::time::timeout(OPEN_STREAM_TIMEOUT, handle.open_stream()).await {
            Err(_) => {
                debug!("mux session to {addr} has no stream free in time; using another connection");
                *handle.saturated_until.lock().unwrap() = Some(Instant::now() + SATURATED_FOR);
                None
            }
            Ok(Ok(stream)) => Some((stream, handle)),
            Ok(Err(e)) => {
                debug!("mux open_stream to {addr} failed: {e}, removing session");
                self.sessions.lock().unwrap().remove(addr);
                None
            }
        }
    }

    /// Register a new yamux session for `addr`.
    pub fn insert(&self, addr: String, handle: MuxHandle) {
        self.sessions.lock().unwrap().insert(addr, handle);
    }

    /// Check if a live session exists for `addr` (an ended one is dropped,
    /// so it gets re-established instead of waiting for a request to fail).
    pub fn contains(&self, addr: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(addr) {
            Some(handle) if handle.is_closed() => {
                sessions.remove(addr);
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Drop sessions that have ended.
    pub fn prune(&self) {
        self.sessions.lock().unwrap().retain(|_, handle| !handle.is_closed());
    }

    /// Number of active sessions in the pool.
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// Drop the sessions with peers whose certificate is revoked. (Their
    /// connections fail at the next read or write in any case.)
    pub fn drop_revoked(&self, revocations: &crate::tls::Revocations) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|_, handle| match (&handle.peer_fingerprint, &handle.peer) {
            (Some(fp), cn) => !revocations.is_revoked(cn.as_deref(), fp),
            (None, Some(cn)) => !revocations.name_revoked(cn),
            (None, None) => true,
        });
        before - sessions.len()
    }

    /// Drop the sessions for addresses `keep` rejects. A session still
    /// carrying tunnels lives on until they finish (each holds a handle),
    /// then closes; an idle one closes now.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|addr, _| keep(addr));
        before - sessions.len()
    }
}

// ---------------------------------------------------------------------------
// Server side (Node): accept a yamux mux session
// ---------------------------------------------------------------------------

/// Accept a yamux multiplexed session on a `MeshStream`.
///
/// The `MuxTunnel` message has already been read by the dispatcher.
/// Each incoming yamux stream carries one tunnel lifecycle:
/// TunnelRequest → status byte → bidirectional proxy.
pub async fn handle_mux_session(stream: MeshStream, state: Arc<SharedState>) {
    static LAST_LIMIT_WARNING: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    // Who opened the session: the caller of every tunnel on it.
    let peer: Arc<str> = Arc::from(stream.peer_cn().unwrap_or(""));
    let compat_stream = stream.compat();
    let mut connection = yamux::Connection::new(compat_stream, session_config(), yamux::Mode::Server);

    debug!("mux session started from {peer}");

    loop {
        let next = poll_fn(|cx| connection.poll_next_inbound(cx)).await;
        match next {
            Some(Ok(yamux_stream)) => {
                // Every stream counts against its peer's open tunnels for
                // its whole life (see MAX_TUNNELS_PER_PEER).
                let open = match crate::node::tunnel::tunnel_slot(&state, &peer) {
                    Ok(slot) => slot,
                    Err(why) => {
                        gossip::warn_rarely(&LAST_LIMIT_WARNING, format_args!("refusing a mux stream: {why}"));
                        drop(yamux_stream);
                        continue;
                    }
                };
                // Until its tunnel is set up, a stream counts against its
                // peer's limit across all sessions (see
                // MAX_PENDING_STREAMS_PER_PEER): otherwise a peer could stall
                // thousands of streams, each holding a full window.
                let Some(pending) = state.try_peer_slot("mux-stream", &peer, MAX_PENDING_STREAMS_PER_PEER) else {
                    gossip::warn_rarely(
                        &LAST_LIMIT_WARNING,
                        format_args!(
                            "refusing a mux stream from '{peer}': it already has {MAX_PENDING_STREAMS_PER_PEER} \
                             streams waiting for their tunnel to be set up"
                        ),
                    );
                    drop(yamux_stream);
                    continue;
                };
                let state = state.clone();
                let peer = peer.clone();
                tokio::spawn(async move {
                    let _open = open;
                    let compat = yamux_stream.compat();
                    if let Err(e) = handle_mux_stream(compat, &peer, pending, state).await {
                        debug!("mux stream error: {e}");
                    }
                });
            }
            Some(Err(e)) => {
                debug!("mux session from {peer} ended with error: {e}");
                break;
            }
            None => {
                debug!("mux session from {peer} closed");
                break;
            }
        }
    }
}

/// Handle a single yamux stream carrying a tunnel request.
///
/// `pending` is the stream's place among its peer's streams waiting for
/// their tunnel (see [`MAX_PENDING_STREAMS_PER_PEER`]), held until the
/// tunnel is connected (or fails).
async fn handle_mux_stream(
    mut stream: Compat<yamux::Stream>,
    peer: &str,
    pending: crate::state::PeerSlot,
    state: Arc<SharedState>,
) -> Result<()> {
    let msg = tokio::time::timeout(
        TUNNEL_REQUEST_TIMEOUT,
        gossip::recv_message_limited(&mut stream, MAX_TUNNEL_REQUEST_BYTES),
    )
    .await
    .map_err(|_| anyhow::anyhow!("no tunnel request within {}s", TUNNEL_REQUEST_TIMEOUT.as_secs()))?
    .context("recv tunnel request on mux stream")?;
    // Only now is there work in flight, for a drain to wait on: a stream
    // that never sent its request is just closed at shutdown.
    let _in_flight = state.in_flight.track();

    match msg {
        Message::TunnelRequest {
            node_id,
            service_name,
            timestamp_ms,
            caller,
        } => {
            crate::node::tunnel::refuse_if_draining(&mut stream, &state).await?;

            // Verify timestamp
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system time before Unix epoch")?
                .as_millis() as u64;

            let skew = now_ms.abs_diff(timestamp_ms);
            if skew > MAX_SKEW_MS {
                let msg = format!("tunnel request timestamp skew too large: {skew}ms");
                send_tunnel_error(&mut stream, &msg).await;
                bail!("{msg}");
            }

            // Check if this tunnel targets us or needs relaying
            let our_node_id = &state.config.node_id;
            if !node_id.is_empty() && node_id != *our_node_id {
                if let Some(why) = crate::node::tunnel::self_relay(Some(peer), &node_id) {
                    send_tunnel_error(&mut stream, &why).await;
                    bail!("{why}");
                }
                return crate::node::relay::relay_tunnel(
                    stream,
                    &node_id,
                    &service_name,
                    timestamp_ms,
                    caller,
                    Some(pending),
                    &state,
                )
                .await;
            }

            // Only a caller its policy allows.
            let via = crate::authz::Via::Direct(Some(peer).filter(|p| !p.is_empty()));
            if let Err(why) = crate::authz::authorize(&state, &service_name, timestamp_ms, caller.as_ref(), via) {
                send_tunnel_error(&mut stream, &why).await;
                bail!("{why}");
            }

            // Connect to local service
            let backend = match connect_local_service(&service_name, &state).await {
                Ok(s) => s,
                Err(err_msg) => {
                    send_tunnel_error(&mut stream, &err_msg).await;
                    bail!("{err_msg}");
                }
            };

            // Signal success
            stream
                .write_all(&[0x00])
                .await
                .context("write tunnel success byte")?;
            stream.flush().await.context("flush tunnel success byte")?;
            drop(pending);

            // Proxy the yamux stream to the local service
            crate::io::proxy_bidirectional(stream, backend)
                .await
                .context("mux proxy_bidirectional")?;

            Ok(())
        }
        other => {
            bail!("unexpected message on mux stream: {other:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Client side (Subscriber): establish a yamux session
// ---------------------------------------------------------------------------

/// Open a connection to `addr`, TLS-wrap it if configured, send a signed
/// `MuxTunnel` message, and upgrade to yamux client mode.
///
/// Returns a [`MuxHandle`] for opening streams. A background task is spawned
/// to drive the yamux connection.
pub async fn establish_mux_session(
    addr: &str,
    state: &Arc<SharedState>,
) -> Result<MuxHandle> {
    let mut stream = state
        .connect_mesh(addr, state.node_at(addr).as_deref())
        .await
        .context("mux connect")?;

    gossip::send_message(&mut stream, &Message::MuxTunnel {})
        .await
        .context("send MuxTunnel")?;

    let peer = stream.peer_cn().map(str::to_string);
    let peer_fingerprint = stream.peer_fingerprint();

    // Upgrade to yamux client mode.
    let compat = stream.compat();
    let connection = yamux::Connection::new(compat, session_config(), yamux::Mode::Client);

    // Create the channel for outbound stream requests.
    let (tx, rx) = mpsc::channel(64);

    let addr_owned = addr.to_string();
    tokio::spawn(async move {
        drive_client_connection(connection, rx, &addr_owned).await;
    });

    Ok(MuxHandle { tx, peer, peer_fingerprint, saturated_until: Arc::default() })
}

/// Background task that drives a yamux client connection.
///
/// Processes outbound stream requests from the channel while also polling
/// the connection for inbound frames (window updates, pings, etc.).
async fn drive_client_connection<T>(
    mut conn: yamux::Connection<T>,
    mut rx: mpsc::Receiver<oneshot::Sender<StreamResult>>,
    addr: &str,
) where
    T: futures::AsyncRead + futures::AsyncWrite + Unpin,
{
    let mut pending: Option<oneshot::Sender<StreamResult>> = None;

    poll_fn(|cx| {
        // Process pending outbound stream request, dropping it once its
        // caller gave up waiting (registering to hear of that).
        if let Some(ref mut sender) = pending {
            if sender.poll_closed(cx).is_ready() {
                pending = None;
            } else if let Poll::Ready(result) = conn.poll_new_outbound(cx) {
                let _ = pending.take().unwrap().send(result);
            }
        }

        // Accept new outbound requests from the channel.
        if pending.is_none() {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(sender)) => {
                    pending = Some(sender);
                    cx.waker().wake_by_ref();
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => {}
            }
        }

        // Drive the connection (process pings, window updates, inbound streams).
        match conn.poll_next_inbound(cx) {
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => return Poll::Ready(()),
            Poll::Ready(Some(Ok(_))) => {
                // Unexpected inbound stream, drop it. Re-poll.
                cx.waker().wake_by_ref();
            }
            Poll::Pending => {}
        }

        Poll::Pending
    })
    .await;

    debug!("mux client session to {addr} ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tunnels_to_a_slow_destination_dont_hold_up_the_session() {
        use crate::state::test_support::tls_state;
        let dir = std::env::temp_dir().join(format!("stouter_mux_backlog_{}_{}", std::process::id(), rand::random::<u64>()));
        let relay = tls_state(&dir, "relay");
        let sub = tls_state(&dir, "sub");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        {
            let relay = relay.clone();
            tokio::spawn(async move {
                loop {
                    let (tcp, peer) = listener.accept().await.unwrap();
                    let relay = relay.clone();
                    tokio::spawn(async move {
                        if let Ok(mesh) = relay.tls_accept(tcp).await {
                            crate::gossip::dispatch_connection(mesh, peer.to_string(), relay).await;
                        }
                    });
                }
            });
        }
        let pool = MuxPool::new();
        pool.insert(addr.clone(), establish_mux_session(&addr, &sub).await.unwrap());
        let now_ms = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        // 256 tunnels to a NAT'd node relayed here, with no reverse
        // connection free: each waits there, unanswered (so unacknowledged).
        relay.note_relayed_here("x");
        let mut held = vec![];
        for _ in 0..256 {
            let (stream, session) = pool.open_stream(&addr, None).await.unwrap();
            let mut stream = stream.compat();
            let request = Message::TunnelRequest { node_id: "x".into(), service_name: "svc".into(), timestamp_ms: now_ms(), caller: None };
            gossip::send_message(&mut stream, &request).await.unwrap();
            held.push((stream, session));
        }
        // Another tunnel over the session gives up quickly (and goes by
        // another connection), and the next doesn't wait at all.
        let start = Instant::now();
        assert!(pool.open_stream(&addr, None).await.is_none());
        assert!(start.elapsed() < OPEN_STREAM_TIMEOUT + Duration::from_millis(500), "{:?}", start.elapsed());
        let start = Instant::now();
        assert!(pool.open_stream(&addr, None).await.is_none());
        assert!(start.elapsed() < Duration::from_millis(100), "{:?}", start.elapsed());
        assert!(pool.contains(&addr), "the session itself is fine");
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_session_serves_only_the_node_it_is_with() {
        let pool = MuxPool::new();
        let (tx, _rx) = mpsc::channel(1);
        pool.insert("10.0.0.1:7000".into(), MuxHandle { tx, peer: Some("m".into()), peer_fingerprint: None, saturated_until: Arc::default() });
        // Gossip may claim x is at m's address: x's tunnels still don't
        // ride m's session.
        assert!(pool.open_stream("10.0.0.1:7000", Some("x")).await.is_none());
        assert!(pool.contains("10.0.0.1:7000"), "the session itself is fine");
    }
    #[test]
    fn sessions_with_revoked_peers_are_dropped() {
        let pool = MuxPool::new();
        // Each session's driver, here only the receiving end held open.
        let mut drivers = Vec::new();
        let mut handle = |peer: &str, fp: Option<[u8; 32]>| {
            let (tx, rx) = mpsc::channel(1);
            drivers.push(rx);
            MuxHandle { tx, peer: Some(peer.into()), peer_fingerprint: fp, saturated_until: Arc::default() }
        };
        pool.insert("m:1".into(), handle("m", Some([1; 32])));
        pool.insert("n:1".into(), handle("n", Some([2; 32])));
        pool.insert("o:1".into(), handle("o", None));
        let revocations = crate::tls::Revocations::default();
        revocations.set(&crate::config::Revoked { nodes: vec!["o".into()], certs: vec!["02".repeat(32)] });
        assert_eq!(pool.drop_revoked(&revocations), 2);
        assert!(pool.contains("m:1") && !pool.contains("n:1") && !pool.contains("o:1"));
    }
    /// Every tunnel a peer has open through a node counts against it (see
    /// MAX_TUNNELS_PER_PEER) for as long as it is open, connected or not:
    /// once connected it still holds buffers, and data up to its window.
    /// And a node can't have a tunnel relayed back to itself, down its own
    /// reverse connections, on a mux stream or a plain connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_tunnels_count_against_their_peer_and_none_is_relayed_back_to_it() {
        use crate::state::test_support::{mesh_pair, tls_state, tls_state_with};
        use tokio::io::AsyncReadExt as _;
        let dir = std::env::temp_dir().join(format!("stouter_mux_open_{}_{}", std::process::id(), rand::random::<u64>()));
        // A service that never answers, and closes when its caller does.
        let service = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = service.accept().await {
                tokio::spawn(async move {
                    let mut sink = Vec::new();
                    let _ = tcp.read_to_end(&mut sink).await;
                });
            }
        });
        let dc: crate::config::DynamicConfig = serde_json::from_value(serde_json::json!({
            "version": 1, "serviceGroups": [{"name": "g", "services": [{"name": "svc", "nodeId": "node", "nodePort": port}]}],
        }))
        .unwrap();
        let node = tls_state_with(&dir, crate::config::Config { node_id: "node".into(), dynamic_config: dc, ..Default::default() }, String::new());
        let peer = tls_state(&dir, "peer");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        {
            let node = node.clone();
            tokio::spawn(async move {
                loop {
                    let (tcp, from) = listener.accept().await.unwrap();
                    let node = node.clone();
                    tokio::spawn(async move {
                        if let Ok(mesh) = node.tls_accept(tcp).await {
                            crate::gossip::dispatch_connection(mesh, from.to_string(), node).await;
                        }
                    });
                }
            });
        }
        let count = |kind: &'static str| node.peer_slots.lock().unwrap().get(&(kind, "peer".to_string())).copied().unwrap_or(0);
        let now_ms = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let request = |to: &str| Message::TunnelRequest { node_id: to.into(), service_name: "svc".into(), timestamp_ms: now_ms(), caller: None };

        let session = establish_mux_session(&addr, &peer).await.unwrap();
        let mut connected = Vec::new();
        for _ in 0..5 {
            let mut stream = session.open_stream().await.unwrap().compat();
            gossip::send_message(&mut stream, &request("node")).await.unwrap();
            let mut status = [0u8; 1];
            stream.read_exact(&mut status).await.unwrap();
            assert_eq!(status, [0x00], "connected");
            connected.push(stream);
        }
        assert_eq!(count("mux-stream"), 0, "none is waiting for its tunnel any more");
        assert_eq!(count("tunnel"), 5, "but each counts while open");

        // Relayed back to the peer itself: refused, on a stream and on a
        // plain connection alike.
        let mut stream = session.open_stream().await.unwrap().compat();
        gossip::send_message(&mut stream, &request("peer")).await.unwrap();
        let mut status = [0u8; 1];
        stream.read_exact(&mut status).await.unwrap();
        assert_eq!(status, [0x01]);
        let why = crate::node::tunnel::read_tunnel_error(&mut stream).await.unwrap();
        assert!(why.contains("to itself"), "{why}");
        let (mut client, server) = mesh_pair(&peer, &node).await;
        let refused = crate::node::tunnel::handle_tunnel(server, "peer".into(), "svc".into(), now_ms(), None, false, node.clone());
        assert!(format!("{:#}", refused.await.expect_err("refused")).contains("to itself"));
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status, [0x01]);

        // Closed, they count no more.
        drop(connected);
        let freed = async {
            while count("tunnel") > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), freed).await.expect("places freed");

        // At the limit, no more.
        let held: Vec<_> = (0..crate::node::tunnel::MAX_TUNNELS_PER_PEER)
            .map(|_| crate::node::tunnel::tunnel_slot(&node, "peer").unwrap())
            .collect();
        assert!(crate::node::tunnel::tunnel_slot(&node, "peer").is_err());
        let (mut client, server) = mesh_pair(&peer, &node).await;
        let refused = crate::node::tunnel::handle_tunnel(server, "node".into(), "svc".into(), now_ms(), None, false, node.clone());
        assert!(format!("{:#}", refused.await.expect_err("refused")).contains("tunnels open"));
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status, [0x01]);
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A peer can't make a node hold more than a little for streams that
    /// never finish their first message: one longer than a tunnel request is
    /// refused on its length, at most 64 may wait (the same limit as plain
    /// connections), and they aren't work in flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn streams_that_never_finish_their_request_hold_little() {
        use crate::state::test_support::tls_state;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let dir = std::env::temp_dir().join(format!("stouter_mux_hold_{}_{}", std::process::id(), rand::random::<u64>()));
        let node = tls_state(&dir, "node");
        let peer = tls_state(&dir, "peer");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        {
            let node = node.clone();
            tokio::spawn(async move {
                loop {
                    let (tcp, from) = listener.accept().await.unwrap();
                    let node = node.clone();
                    tokio::spawn(async move {
                        if let Ok(mesh) = node.tls_accept(tcp).await {
                            crate::gossip::dispatch_connection(mesh, from.to_string(), node).await;
                        }
                    });
                }
            });
        }
        let session = establish_mux_session(&addr, &peer).await.unwrap();
        let waiting = || node.peer_slots.lock().unwrap().get(&("mux-stream", "peer".to_string())).copied().unwrap_or(0);

        // 128 streams each start an 8 MiB message (the plain-connection
        // limit) and send 2 MiB of it: each is refused on its length, and
        // let go at once (the node keeps none of it).
        let chunk = vec![0x90u8; 64 * 1024];
        let mut refused = 0;
        for _ in 0..128 {
            let mut stream = session.open_stream().await.unwrap().compat();
            let sent = async {
                stream.write_all(&(8u32 << 20).to_be_bytes()).await?;
                for _ in 0..32 {
                    stream.write_all(&chunk).await?;
                }
                stream.flush().await?;
                let mut byte = [0u8; 1];
                stream.read(&mut byte).await
            };
            if !matches!(tokio::time::timeout(Duration::from_secs(5), sent).await, Ok(Ok(n)) if n > 0) {
                refused += 1;
            }
        }
        assert_eq!(refused, 128, "every oversized request is refused");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(waiting(), 0, "and none is still held");

        // Streams that send a few bytes of a small request and stall, over
        // three sessions (yamux lets a session have only 256 streams the
        // other side hasn't acknowledged): at most
        // MAX_PENDING_STREAMS_PER_PEER wait across them all (each could hold
        // a full window), and none counts as work in flight.
        let sessions = [session, establish_mux_session(&addr, &peer).await.unwrap(), establish_mux_session(&addr, &peer).await.unwrap()];
        let mut stalled = Vec::new();
        for i in 0..MAX_PENDING_STREAMS_PER_PEER + 88 {
            let mut stream = sessions[i % 3].open_stream().await.unwrap().compat();
            // Past the budget the node resets the stream, which can reach
            // us before these writes do: they may fail.
            let _ = stream.write_all(&100u32.to_be_bytes()).await;
            let _ = stream.write_all(&[0x80; 10]).await;
            let _ = stream.flush().await;
            stalled.push(stream);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(waiting(), MAX_PENDING_STREAMS_PER_PEER, "at most this many wait, whatever the sessions");
        assert_eq!(node.in_flight.current(), 0, "a stalled stream isn't work a drain waits for");

        // Once they go, their places are free again.
        drop(stalled);
        let freed = async {
            while waiting() > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), freed).await.expect("places freed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
