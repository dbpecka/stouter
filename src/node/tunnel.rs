use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::state::SharedState;
use crate::tls::MeshStream;

/// Maximum timestamp skew allowed for tunnel authentication.
const MAX_SKEW_MS: u64 = 30_000;

/// Write a tunnel error response: `0x01` status byte, 4-byte big-endian message
/// length, then the UTF-8 message bytes.
pub(crate) async fn send_tunnel_error<W: AsyncWrite + Unpin>(stream: &mut W, msg: &str) {
    let _ = stream.write_all(&[0x01]).await;
    let mut end = msg.len().min(MAX_TUNNEL_ERROR_LEN);
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    let b = &msg.as_bytes()[..end];
    let _ = stream
        .write_all(&(b.len() as u32).to_be_bytes())
        .await;
    let _ = stream.write_all(b).await;
    let _ = stream.flush().await;
}

/// Longest tunnel error message read from a peer. Its length comes first,
/// as four bytes the peer chooses: taken on trust, a peer could make us
/// allocate 4 GiB, and a failed allocation aborts the daemon.
pub(crate) const MAX_TUNNEL_ERROR_LEN: usize = 64 * 1024;

/// How long a peer gets to send the rest of a tunnel error once it has
/// said the tunnel failed.
const TUNNEL_ERROR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Read a tunnel error message (after its status byte): its length, then
/// that many bytes, within [`TUNNEL_ERROR_TIMEOUT`] and at most
/// [`MAX_TUNNEL_ERROR_LEN`] long.
pub(crate) async fn read_tunnel_error<R: AsyncRead + Unpin>(stream: &mut R) -> Result<String> {
    tokio::time::timeout(TUNNEL_ERROR_TIMEOUT, async {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.context("read tunnel error length")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_TUNNEL_ERROR_LEN {
            anyhow::bail!("tunnel error message of {len} bytes, over the {MAX_TUNNEL_ERROR_LEN}-byte limit");
        }
        let mut bytes = vec![0u8; len];
        stream.read_exact(&mut bytes).await.context("read tunnel error message")?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
    .await
    .map_err(|_| anyhow::anyhow!("tunnel error message not sent in time"))?
}

/// Most tunnels one peer may have open through this node at once, unless
/// the config's `maxTunnelsPerPeer` says otherwise: on plain connections
/// and mux streams alike, served here or relayed, for as long as each is
/// open. Each holds memory (buffers, and data up to its window) and,
/// relayed, a connection to the NAT'd node, whether or not anything reads
/// it: uncapped, one member could make a node hold gigabytes.
pub(crate) const MAX_TUNNELS_PER_PEER: usize = 4096;

/// A place among `peer`'s open tunnels (see [`MAX_TUNNELS_PER_PEER`]),
/// held for the tunnel's whole life; or why there is none.
pub(crate) fn tunnel_slot(state: &Arc<SharedState>, peer: &str) -> std::result::Result<crate::state::PeerSlot, String> {
    let limit = state.config.max_tunnels_per_peer.unwrap_or(MAX_TUNNELS_PER_PEER);
    state.try_peer_slot("tunnel", peer, limit).ok_or_else(|| {
        format!("'{peer}' already has {limit} tunnels open through this node (its maxTunnelsPerPeer)")
    })
}

/// How many times a peer's own tunnel limit may be open through one of this
/// node's relays: they come from every caller using that relay.
pub(crate) const RELAYED_TUNNELS_FACTOR: usize = 4;

/// A place among the tunnels open through `relay`, which this node keeps
/// reverse connections at; or why there is none. Without it, a relay could
/// send a request down each reverse connection, and since the node
/// registers a new one at once, have it open tunnels (and backend
/// connections) without end.
pub(crate) fn relayed_tunnel_slot(state: &Arc<SharedState>, relay: &str) -> std::result::Result<crate::state::PeerSlot, String> {
    let limit = state.config.max_tunnels_per_peer.unwrap_or(MAX_TUNNELS_PER_PEER).saturating_mul(RELAYED_TUNNELS_FACTOR);
    state.try_peer_slot("relayed tunnel", relay, limit).ok_or_else(|| {
        format!("this node already has {limit} tunnels open through relay '{relay}' ({RELAYED_TUNNELS_FACTOR}x maxTunnelsPerPeer)")
    })
}

/// Why a tunnel to `target` that `caller` asks this node to relay is
/// refused, if it is: one back to the caller itself, down the reverse
/// connections it keeps here. Never needed (a node reaches its own
/// services locally), and a way for a node to hold tunnels here that
/// nothing on its side reads.
pub(crate) fn self_relay(caller: Option<&str>, target: &str) -> Option<String> {
    (caller == Some(target)).then(|| format!("node {target} asked to be relayed a tunnel to itself"))
}

/// Refuse a new tunnel once shutdown has begun, so a draining node only
/// finishes the work it already has. The subscriber sees the error and tries
/// another node.
pub(crate) async fn refuse_if_draining<W: AsyncWrite + Unpin>(stream: &mut W, state: &SharedState) -> Result<()> {
    if state.shutdown.is_cancelled() {
        let msg = "node is shutting down";
        send_tunnel_error(stream, msg).await;
        bail!("{msg}");
    }
    Ok(())
}

/// Handle a tunnel request with already-parsed fields.
///
/// Protocol (after the initial Message has been read by the dispatcher):
/// 1. Verify the timestamp is within ±30 seconds of now.
/// 2. Locate the requested service on this node (or relay if targeting another node).
/// 3. Open a TCP connection to `127.0.0.1:{node_port}`.
/// 4. Write `0x00` (success) and then transparently copy bytes in both directions.
///
/// `caller` is the request's signature, if the caller signed it; with
/// `via_relay`, the request came on a reverse connection this node keeps at
/// a relay, so the certificate at the other end is the relay's and only a
/// signature says who asks (see `crate::authz`).
pub async fn handle_tunnel(
    mut client: MeshStream,
    node_id: String,
    service_name: String,
    timestamp_ms: u64,
    caller: Option<crate::config::SignedCaller>,
    via_relay: bool,
    state: Arc<SharedState>,
) -> Result<()> {
    refuse_if_draining(&mut client, &state).await?;

    // --- Verify timestamp ---
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let skew = now_ms.abs_diff(timestamp_ms);
    if skew > MAX_SKEW_MS {
        let msg = format!("tunnel request timestamp skew too large: {skew}ms");
        send_tunnel_error(&mut client, &msg).await;
        bail!("{msg}");
    }

    // Counted against the peer's tunnels while open. One that came on a
    // reverse connection this node keeps at a relay is counted against the
    // relay instead, as it arrives (`reverse::open_reverse_connection`):
    // the certificate at the other end of it is the relay's.
    let _open = if via_relay {
        None
    } else {
        match tunnel_slot(&state, client.peer_cn().unwrap_or_default()) {
            Ok(slot) => Some(slot),
            Err(why) => {
                send_tunnel_error(&mut client, &why).await;
                bail!("{why}");
            }
        }
    };

    // --- Check if this tunnel targets us or needs relaying ---
    let our_node_id = &state.config.node_id;
    if !node_id.is_empty() && node_id != *our_node_id {
        if let Some(why) = self_relay(client.peer_cn(), &node_id) {
            send_tunnel_error(&mut client, &why).await;
            bail!("{why}");
        }
        return super::relay::relay_tunnel(client, &node_id, &service_name, timestamp_ms, caller, None, &state)
            .await;
    }

    // --- Only a caller its policy allows ---
    let via = if via_relay { crate::authz::Via::Relay } else { crate::authz::Via::Direct(client.peer_cn()) };
    let who = match crate::authz::authorize(&state, &service_name, timestamp_ms, caller.as_ref(), via) {
        Ok(who) => who,
        Err(why) => {
            send_tunnel_error(&mut client, &why).await;
            bail!("{why}");
        }
    };

    // --- Locate service and connect ---
    let backend = match connect_local_service(&service_name, &state).await {
        Ok(s) => s,
        Err(msg) => {
            send_tunnel_error(&mut client, &msg).await;
            bail!("{msg}");
        }
    };

    // --- Success: signal readiness and start transparent proxy ---
    client
        .write_all(&[0x00])
        .await
        .context("write tunnel success byte")?;

    // Until it ends, or the policies stop allowing it.
    let served = state.serve_tunnel(&service_name, who);
    tokio::select! {
        biased;
        () = served.refused() => bail!("tunnel to '{service_name}' closed: the policy no longer allows it"),
        proxied = crate::io::proxy_bidirectional(client, backend) => proxied.context("proxy_bidirectional")?,
    }

    Ok(())
}

/// Look up a service on this node and connect to its local port.
/// Returns the connected `TcpStream` or an error message string.
pub(crate) async fn connect_local_service(
    service_name: &str,
    state: &SharedState,
) -> std::result::Result<TcpStream, String> {
    let our_node_id = &state.config.node_id;
    let service = {
        let idx = state.service_index.load();
        idx.get(&(our_node_id.clone(), service_name.to_string()))
            .cloned()
    };

    let service = match service {
        Some(s) => s,
        None => {
            return Err(format!("service '{}' not found on this node", service_name));
        }
    };

    let local_addr = format!("127.0.0.1:{}", service.node_port);
    // Bounded: a service whose accept queue is full would otherwise hold
    // the tunnel for as long as TCP retries (about two minutes), and a
    // drain with it. Well under the subscriber's own setup timeout (15 s),
    // so it hears why.
    match tokio::time::timeout(LOCAL_CONNECT_TIMEOUT, TcpStream::connect(&local_addr)).await {
        Ok(Ok(s)) => {
            s.set_nodelay(true).ok();
            Ok(s)
        }
        Ok(Err(e)) => Err(format!("failed to connect to local service at {local_addr}: {e}")),
        Err(_) => Err(format!(
            "the local service at {local_addr} didn't accept a connection within {}s",
            LOCAL_CONNECT_TIMEOUT.as_secs()
        )),
    }
}

/// How long connecting to a service on this node may take.
const LOCAL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tunnel_cap_can_be_set_per_node() {
        let cfg = crate::config::Config { max_tunnels_per_peer: Some(2), ..Default::default() };
        let state = SharedState::new(cfg, String::new(), tokio_util::sync::CancellationToken::new(), None);
        let held = [tunnel_slot(&state, "p").unwrap(), tunnel_slot(&state, "p").unwrap()];
        let Err(why) = tunnel_slot(&state, "p") else { panic!("over the cap") };
        assert!(why.contains("already has 2 tunnels"), "{why}");
        assert!(tunnel_slot(&state, "q").is_ok(), "per peer");
        drop(held);
        assert!(tunnel_slot(&state, "p").is_ok());
    }

    #[tokio::test]
    async fn a_peer_cant_make_us_allocate_what_it_likes_for_an_error() {
        // A status of failure, then a length of 4 GiB.
        let mut huge: &[u8] = &[0xff, 0xff, 0xff, 0xff, b'x'];
        let err = read_tunnel_error(&mut huge).await.expect_err("refused");
        assert!(err.to_string().contains("over the"), "{err}");

        // An ordinary error reads fine, and an overlong one is cut when sent.
        let mut sent = Vec::new();
        send_tunnel_error(&mut sent, "service 'api' not found on this node").await;
        let mut reader = &sent[1..];
        assert_eq!(read_tunnel_error(&mut reader).await.unwrap(), "service 'api' not found on this node");
        let mut long = Vec::new();
        send_tunnel_error(&mut long, &"é".repeat(MAX_TUNNEL_ERROR_LEN)).await;
        let mut reader = &long[1..];
        assert!(read_tunnel_error(&mut reader).await.unwrap().len() <= MAX_TUNNEL_ERROR_LEN);
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_never_finishes_its_error_is_given_up_on() {
        let (mut ours, mut theirs) = tokio::io::duplex(64);
        theirs.write_all(&10u32.to_be_bytes()).await.unwrap();
        // ...and then nothing.
        let err = read_tunnel_error(&mut ours).await.expect_err("timed out");
        assert!(err.to_string().contains("in time"), "{err}");
        drop(theirs);
    }

    /// A service whose accept queue is full: the connection doesn't wait for
    /// TCP to give up (about two minutes), but fails after a few seconds.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn connecting_to_a_service_that_isnt_accepting_times_out() {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let port = listener.local_addr().unwrap().port();
        // Fill its accept queue (never accepted); further connects hang.
        let mut held = Vec::new();
        for _ in 0..4 {
            if let Ok(Ok(c)) = tokio::time::timeout(std::time::Duration::from_millis(200), TcpStream::connect(("127.0.0.1", port))).await {
                held.push(c);
            }
        }
        let dc: crate::config::DynamicConfig = serde_json::from_value(serde_json::json!({
            "version": 1, "serviceGroups": [{"name": "g", "services": [{"name": "api", "nodeId": "n1", "nodePort": port}]}],
        }))
        .unwrap();
        let cfg = crate::config::Config { node_id: "n1".into(), dynamic_config: dc, ..Default::default() };
        let state = SharedState::new(cfg, String::new(), tokio_util::sync::CancellationToken::new(), None);
        let started = std::time::Instant::now();
        let err = connect_local_service("api", &state).await.expect_err("timed out");
        assert!(err.contains("didn't accept"), "{err}");
        assert!(started.elapsed() < LOCAL_CONNECT_TIMEOUT + std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        drop((listener, held));
    }
}
