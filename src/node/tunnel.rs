use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncWriteExt, AsyncWrite};
use tokio::net::TcpStream;

use crate::state::SharedState;

/// Maximum timestamp skew allowed for tunnel authentication.
const MAX_SKEW_MS: u64 = 30_000;

/// Write a tunnel error response: `0x01` status byte, 4-byte big-endian message
/// length, then the UTF-8 message bytes.
pub(crate) async fn send_tunnel_error<W: AsyncWrite + Unpin>(stream: &mut W, msg: &str) {
    let _ = stream.write_all(&[0x01]).await;
    let b = msg.as_bytes();
    let _ = stream
        .write_all(&(b.len() as u32).to_be_bytes())
        .await;
    let _ = stream.write_all(b).await;
    let _ = stream.flush().await;
}

/// Handle a tunnel request with already-parsed fields.
///
/// Protocol (after the initial Message has been read by the dispatcher):
/// 1. Verify the timestamp is within ±30 seconds of now.
/// 2. Locate the requested service on this node (or relay if targeting another node).
/// 3. Open a TCP connection to `127.0.0.1:{node_port}`.
/// 4. Write `0x00` (success) and then transparently copy bytes in both directions.
pub async fn handle_tunnel(
    mut client: TcpStream,
    node_id: String,
    service_name: String,
    timestamp_ms: u64,
    state: Arc<SharedState>,
) -> Result<()> {
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
        return super::relay::relay_tunnel(
            client,
            &node_id,
            &service_name,
            timestamp_ms,
            &state,
        )
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
