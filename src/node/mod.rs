pub mod relay;
mod reverse;
pub mod tunnel;

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tracing::{debug, info};

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
        let (stream, peer_addr) = listener.accept().await.context("accept connection")?;
        stream.set_nodelay(true).ok();
        debug!("accepted connection from {peer_addr}");

        let state = state.clone();
        tokio::spawn(gossip::dispatch_connection(stream, state));
    }
}
