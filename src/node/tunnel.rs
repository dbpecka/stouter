use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::state::SharedState;

/// Maximum allowed tunnel request size: 64 KiB.
const MAX_REQUEST_BYTES: u32 = 65536;

/// Maximum timestamp skew allowed for tunnel authentication.
const MAX_SKEW_MS: u64 = 30_000;

/// Framing request sent by a subscribe-side proxy to initiate a tunnel.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TunnelRequest {
    service_name: String,
    timestamp_ms: u64,
    /// HMAC-SHA256(cluster_secret, "{service_name}:{timestamp_ms}") in hex.
    token: String,
}

/// Write a tunnel error response: `0x01` status byte, 4-byte big-endian message
/// length, then the UTF-8 message bytes.
async fn send_tunnel_error(stream: &mut TcpStream, msg: &str) {
    let _ = stream.write_all(&[0x01]).await;
    let b = msg.as_bytes();
    let _ = stream
        .write_all(&(b.len() as u32).to_be_bytes())
        .await;
    let _ = stream.write_all(b).await;
}

/// Handle a tunnel connection from a subscribe daemon.
///
/// Protocol:
/// 1. Read 4-byte big-endian length prefix + JSON [`TunnelRequest`].
/// 2. Verify the timestamp is within ±30 seconds of now.
/// 3. Verify the HMAC token.
/// 4. Locate the requested service on this node.
/// 5. Open a TCP connection to `127.0.0.1:{node_port}`.
/// 6. Write `0x00` (success) and then transparently copy bytes in both directions.
pub async fn handle_tunnel(mut client: TcpStream, state: Arc<SharedState>) -> Result<()> {
    // --- Read request frame ---
    let mut len_buf = [0u8; 4];
    client
        .read_exact(&mut len_buf)
        .await
        .context("read tunnel request length")?;
    let len = u32::from_be_bytes(len_buf);

    if len > MAX_REQUEST_BYTES {
        let msg = format!("tunnel request too large: {len} bytes");
        send_tunnel_error(&mut client, &msg).await;
        bail!("{msg}");
    }

    let mut frame = vec![0u8; len as usize];
    client
        .read_exact(&mut frame)
        .await
        .context("read tunnel request body")?;

    let req: TunnelRequest = match serde_json::from_slice(&frame) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("invalid tunnel request: {e}");
            send_tunnel_error(&mut client, &msg).await;
            bail!("{msg}");
        }
    };

    // --- Verify timestamp ---
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let skew = now_ms.abs_diff(req.timestamp_ms);
    if skew > MAX_SKEW_MS {
        let msg = format!("tunnel request timestamp skew too large: {skew}ms");
        send_tunnel_error(&mut client, &msg).await;
        bail!("{msg}");
    }

    // --- Verify HMAC token ---
    let expected_data = format!("{}:{}", req.service_name, req.timestamp_ms);
    if !crate::crypto::verify(
        &state.config.cluster_secret,
        &expected_data,
        &req.token,
    ) {
        let msg = "tunnel request token verification failed";
        send_tunnel_error(&mut client, msg).await;
        bail!("{msg}");
    }

    // --- Locate service on this node ---
    let node_id = &state.config.node_id;
    let dc = state.dynamic_config.read().await;
    let service = dc
        .service_groups
        .iter()
        .flat_map(|g| g.services.iter())
        .find(|s| s.node_id == *node_id && s.name == req.service_name)
        .cloned();
    drop(dc);

    let service = match service {
        Some(s) => s,
        None => {
            let msg = format!(
                "service '{}' not found on this node",
                req.service_name
            );
            send_tunnel_error(&mut client, &msg).await;
            bail!("{msg}");
        }
    };

    // --- Connect to local service ---
    let local_addr = format!("127.0.0.1:{}", service.node_port);
    let mut backend = match TcpStream::connect(&local_addr).await {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("failed to connect to local service at {local_addr}: {e}");
            send_tunnel_error(&mut client, &msg).await;
            bail!("{msg}");
        }
    };

    // --- Success: signal readiness and start transparent proxy ---
    client
        .write_all(&[0x00])
        .await
        .context("write tunnel success byte")?;

    tokio::io::copy_bidirectional(&mut client, &mut backend)
        .await
        .context("copy_bidirectional")?;

    Ok(())
}
