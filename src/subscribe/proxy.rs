use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tracing::info;

use crate::gossip::CONN_TUNNEL;

/// Authentication request sent to a node daemon to open a tunnel.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TunnelRequest {
    service_name: String,
    timestamp_ms: u64,
    /// HMAC-SHA256(cluster_secret, "{service_name}:{timestamp_ms}") in hex.
    token: String,
}

/// Run a local TCP proxy for a single service.
///
/// Binds on `127.0.0.1:0`, sends the assigned port back through `port_tx`,
/// then accepts connections and forwards each one through an authenticated
/// tunnel to the node identified by `node_addr`.
pub async fn run_proxy(
    service: crate::config::Service,
    node_addr: String,
    cluster_secret: String,
    port_tx: oneshot::Sender<u16>,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind proxy listener")?;

    let local_port = listener.local_addr()?.port();
    let _ = port_tx.send(local_port);

    info!(
        "Proxy for {} on 127.0.0.1:{}",
        service.name, local_port
    );

    loop {
        let (client, _) = listener.accept().await.context("proxy accept")?;
        let node_addr = node_addr.clone();
        let service_name = service.name.clone();
        let cluster_secret = cluster_secret.clone();

        tokio::spawn(async move {
            if let Err(e) =
                handle_connection(client, node_addr, service_name, cluster_secret).await
            {
                tracing::debug!("proxy connection error: {e}");
            }
        });
    }
}

/// Forward a single client connection through an authenticated tunnel to the
/// remote node.
async fn handle_connection(
    mut client: TcpStream,
    node_addr: String,
    service_name: String,
    cluster_secret: String,
) -> Result<()> {
    let mut node_stream = TcpStream::connect(&node_addr)
        .await
        .with_context(|| format!("connect to node {node_addr}"))?;

    // Identify this connection as a tunnel connection.
    node_stream
        .write_all(&[CONN_TUNNEL])
        .await
        .context("write CONN_TUNNEL byte")?;

    // Build and send the tunnel request.
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let token_data = format!("{service_name}:{timestamp_ms}");
    let token = crate::crypto::sign(&cluster_secret, &token_data);

    let req = TunnelRequest {
        service_name,
        timestamp_ms,
        token,
    };

    let req_bytes = serde_json::to_vec(&req).context("serialize TunnelRequest")?;
    let len = req_bytes.len() as u32;
    node_stream
        .write_all(&len.to_be_bytes())
        .await
        .context("write tunnel request length")?;
    node_stream
        .write_all(&req_bytes)
        .await
        .context("write tunnel request body")?;

    // Read status byte.
    let mut status = [0u8; 1];
    node_stream
        .read_exact(&mut status)
        .await
        .context("read tunnel status byte")?;

    if status[0] != 0x00 {
        // Read the error message from the node.
        let mut len_buf = [0u8; 4];
        node_stream
            .read_exact(&mut len_buf)
            .await
            .context("read tunnel error length")?;
        let err_len = u32::from_be_bytes(len_buf) as usize;
        let mut err_bytes = vec![0u8; err_len];
        node_stream
            .read_exact(&mut err_bytes)
            .await
            .context("read tunnel error message")?;
        let msg = String::from_utf8_lossy(&err_bytes);
        bail!("node rejected tunnel: {msg}");
    }

    // Transparent bidirectional proxy.
    tokio::io::copy_bidirectional(&mut client, &mut node_stream)
        .await
        .context("copy_bidirectional")?;

    Ok(())
}
