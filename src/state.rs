use std::collections::HashMap;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::RwLock;

use crate::config::{Config, DynamicConfig, NodeInfo};

/// Cluster state shared across async tasks within a single daemon process.
pub struct SharedState {
    pub config: Arc<Config>,
    pub dynamic_config: Arc<RwLock<DynamicConfig>>,
    /// Member nodes discovered via gossip, keyed by `node_id`.
    pub known_nodes: Arc<RwLock<Vec<NodeInfo>>>,
    /// Bootstrap peer addresses from config, used only for initiating connections.
    /// These are never treated as member identities.
    pub bootstrap_peers: Vec<String>,
    /// Path to the on-disk config file, used to persist dynamic config updates.
    pub config_path: String,
    /// Pool of idle reverse connections from NAT'd nodes, keyed by node_id.
    /// Used by relay nodes to dispatch tunnel requests to unreachable peers.
    pub reverse_pool: Arc<RwLock<HashMap<String, Vec<TcpStream>>>>,
}

impl SharedState {
    /// Construct a new `SharedState` and wrap it in an `Arc`.
    ///
    /// The `dynamic_config` is seeded from `config.dynamic_config`.
    /// Bootstrap peer addresses are stored separately; the member list
    /// (`known_nodes`) starts empty and is populated by gossip sync.
    pub fn new(config: Config, config_path: String) -> Arc<Self> {
        let dynamic_config = config.dynamic_config.clone();
        let bootstrap_peers = config.known_nodes.clone();

        Arc::new(Self {
            config: Arc::new(config),
            dynamic_config: Arc::new(RwLock::new(dynamic_config)),
            known_nodes: Arc::new(RwLock::new(Vec::new())),
            bootstrap_peers,
            config_path,
            reverse_pool: Arc::new(RwLock::new(HashMap::new())),
        })
    }
}
