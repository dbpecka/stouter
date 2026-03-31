mod tunnel;

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use crate::gossip;
use crate::state::SharedState;

/// Run the node daemon.
///
/// Binds to `state.config.bind`, launches the gossip sync loop, and accepts
/// incoming connections.  The first byte of each connection determines whether
/// it is a gossip or tunnel connection.
pub async fn run_node(state: Arc<SharedState>) -> Result<()> {
    let listener = TcpListener::bind(&state.config.bind)
        .await
        .with_context(|| format!("bind to {}", state.config.bind))?;

    info!("Node listening on {}", state.config.bind);

    tokio::spawn(gossip::run_sync_loop(state.clone()));
    tokio::spawn(gossip::run_member_log(state.clone()));
    tokio::spawn(gossip::run_config_poll_loop(state.clone()));

    loop {
        let (mut stream, peer_addr) = listener.accept().await.context("accept connection")?;
        debug!("accepted connection from {peer_addr}");

        let state = state.clone();

        tokio::spawn(async move {
            let mut type_buf = [0u8; 1];
            if let Err(e) = stream.read_exact(&mut type_buf).await {
                debug!("failed to read connection type from {peer_addr}: {e}");
                return;
            }

            match type_buf[0] {
                gossip::CONN_GOSSIP => {
                    gossip::handle_gossip_connection(stream, state).await;
                }
                gossip::CONN_TUNNEL => {
                    if let Err(e) = tunnel::handle_tunnel(stream, state).await {
                        error!("{e}");
                    }
                }
                gossip::CONN_STATUS => {
                    gossip::handle_status_connection(stream, state).await;
                }
                byte => {
                    debug!("Unknown connection type: 0x{byte:02x} from {peer_addr}");
                }
            }
        });
    }
}
