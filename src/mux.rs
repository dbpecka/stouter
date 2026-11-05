use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{SystemTime, UNIX_EPOCH};

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

        match handle.open_stream().await {
            Ok(stream) => Some((stream, handle)),
            Err(e) => {
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
    let compat_stream = stream.compat();
    let peer = String::from("mux-peer");
    let config = yamux::Config::default();
    let mut connection = yamux::Connection::new(compat_stream, config, yamux::Mode::Server);

    debug!("mux session started from {peer}");

    loop {
        let next = poll_fn(|cx| connection.poll_next_inbound(cx)).await;
        match next {
            Some(Ok(yamux_stream)) => {
                let state = state.clone();
                // Each stream is a tunnel; the session itself isn't tracked,
                // so an idle session doesn't hold up a drain.
                let in_flight = state.in_flight.track();
                tokio::spawn(async move {
                    let _in_flight = in_flight;
                    let compat = yamux_stream.compat();
                    if let Err(e) = handle_mux_stream(compat, state).await {
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
async fn handle_mux_stream(
    mut stream: Compat<yamux::Stream>,
    state: Arc<SharedState>,
) -> Result<()> {
    let msg = gossip::recv_message(&mut stream)
        .await
        .context("recv tunnel request on mux stream")?;

    match msg {
        Message::TunnelRequest {
            node_id,
            service_name,
            timestamp_ms,
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
                return crate::node::relay::relay_tunnel(
                    stream,
                    &node_id,
                    &service_name,
                    timestamp_ms,
                    &state,
                )
                .await;
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

    // Upgrade to yamux client mode.
    let compat = stream.compat();
    let config = yamux::Config::default();
    let connection = yamux::Connection::new(compat, config, yamux::Mode::Client);

    // Create the channel for outbound stream requests.
    let (tx, rx) = mpsc::channel(64);

    let addr_owned = addr.to_string();
    tokio::spawn(async move {
        drive_client_connection(connection, rx, &addr_owned).await;
    });

    Ok(MuxHandle { tx, peer })
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
        // Process pending outbound stream request.
        if let Some(ref sender) = pending {
            if sender.is_closed() {
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

    #[tokio::test]
    async fn a_session_serves_only_the_node_it_is_with() {
        let pool = MuxPool::new();
        let (tx, _rx) = mpsc::channel(1);
        pool.insert("10.0.0.1:7000".into(), MuxHandle { tx, peer: Some("m".into()) });
        // Gossip may claim x is at m's address: x's tunnels still don't
        // ride m's session.
        assert!(pool.open_stream("10.0.0.1:7000", Some("x")).await.is_none());
        assert!(pool.contains("10.0.0.1:7000"), "the session itself is fine");
    }
}
