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
pub async fn handle_tunnel(
    mut client: MeshStream,
    node_id: String,
    service_name: String,
    timestamp_ms: u64,
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

    // --- Check if this tunnel targets us or needs relaying ---
    let our_node_id = &state.config.node_id;
    if !node_id.is_empty() && node_id != *our_node_id {
        return super::relay::relay_tunnel(client, &node_id, &service_name, timestamp_ms, &state)
            .await;
    }

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

    crate::io::proxy_bidirectional(client, backend)
        .await
        .context("proxy_bidirectional")?;

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
    match TcpStream::connect(&local_addr).await {
        Ok(s) => {
            s.set_nodelay(true).ok();
            Ok(s)
        }
        Err(e) => Err(format!("failed to connect to local service at {local_addr}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
