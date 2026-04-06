use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::debug;

use crate::gossip;
use crate::gossip::messages::Message;
use crate::node::tunnel::{connect_local_service, send_tunnel_error};
use crate::state::SharedState;

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
}

impl MuxHandle {
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
    pub async fn open_stream(&self, addr: &str) -> Option<yamux::Stream> {
        let handle = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(addr)?.clone()
        };

        match handle.open_stream().await {
            Ok(stream) => Some(stream),
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

    /// Check if a session exists for `addr`.
    pub fn contains(&self, addr: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(addr)
    }

    /// Remove a session entry.
    #[allow(dead_code)]
    pub fn remove(&self, addr: &str) {
        self.sessions.lock().unwrap().remove(addr);
    }
}

// ---------------------------------------------------------------------------
// Server side (Node): accept a yamux mux session
// ---------------------------------------------------------------------------

/// Accept a yamux multiplexed session on a TCP connection.
///
/// The `MuxTunnel` message has already been read by the dispatcher.
/// Each incoming yamux stream carries one tunnel lifecycle:
/// TunnelRequest → status byte → bidirectional proxy.
pub async fn handle_mux_session(stream: TcpStream, state: Arc<SharedState>) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();

    let compat_stream = stream.compat();
    let config = yamux::Config::default();
    let mut connection = yamux::Connection::new(compat_stream, config, yamux::Mode::Server);

    debug!("mux session started from {peer}");

    loop {
        let next = poll_fn(|cx| connection.poll_next_inbound(cx)).await;
        match next {
            Some(Ok(yamux_stream)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    let compat = yamux_stream.compat();
                    if let Err(e) = handle_mux_stream(compat, &state).await {
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
    state: &SharedState,
) -> Result<()> {
    let secret = &state.config.cluster_secret;
    let msg = gossip::recv_message(&mut stream, secret)
        .await
        .context("recv tunnel request on mux stream")?;

    match msg {
        Message::TunnelRequest {
            node_id: _,
            service_name,
            timestamp_ms,
        } => {
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

            // Connect to local service
            let backend = match connect_local_service(&service_name, state).await {
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

/// Open a TCP connection to `addr`, send a signed `MuxTunnel` message,
/// and upgrade to yamux client mode.
///
/// Returns a [`MuxHandle`] for opening streams. A background task is spawned
/// to drive the yamux connection.
pub async fn establish_mux_session(addr: &str, secret: &str) -> Result<MuxHandle> {
    let mut tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("mux connect to {addr}"))?;

    crate::io::configure_stream(&tcp);

    // Send the MuxTunnel handshake via the existing signed message protocol.
    gossip::send_message(&mut tcp, &Message::MuxTunnel {}, secret)
        .await
        .context("send MuxTunnel")?;

    // Upgrade to yamux client mode.
    let compat = tcp.compat();
    let config = yamux::Config::default();
    let connection = yamux::Connection::new(compat, config, yamux::Mode::Client);

    // Create the channel for outbound stream requests.
    let (tx, rx) = mpsc::channel(64);

    let addr_owned = addr.to_string();
    tokio::spawn(async move {
        drive_client_connection(connection, rx, &addr_owned).await;
    });

    Ok(MuxHandle { tx })
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
