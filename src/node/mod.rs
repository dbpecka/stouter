pub mod relay;
mod reverse;
pub mod tunnel;

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use crate::gossip;
use crate::state::SharedState;

/// Run the node daemon.
///
/// If `outbound_only` is set, the node skips binding a listener and instead
/// maintains reverse connections to reachable peers for tunnel relay.
/// Otherwise, binds to `state.config.bind` and accepts incoming connections.
pub async fn run_node(state: Arc<SharedState>) -> Result<()> {
    tokio::spawn(gossip::run_sync_loop(state.clone()));
    tokio::spawn(gossip::run_member_log(state.clone()));
    tokio::spawn(gossip::run_config_poll_loop(state.clone()));

    if state.config.outbound_only {
        info!(
            "Node running in outbound-only mode (NAT'd), node_id={}",
            state.config.node_id
        );
        // No listener — maintain reverse connections to peers instead.
        reverse::run_reverse_pool_loop(state).await;
        Ok(())
    } else {
        run_listener(state).await
    }
}

/// Bind the TCP listener and accept connections.
async fn run_listener(state: Arc<SharedState>) -> Result<()> {
    let listener = TcpListener::bind(&state.config.bind)
        .await
        .with_context(|| format!("bind to {}", state.config.bind))?;

    info!("Node listening on {}", state.config.bind);

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
                gossip::CONN_REVERSE => {
                    if let Err(e) = relay::handle_reverse_registration(stream, state).await {
                        debug!("reverse registration from {peer_addr} failed: {e}");
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
