use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{Config, DynamicConfig, NodeInfo};

/// Cluster state shared across async tasks within a single daemon process.
pub struct SharedState {
    pub config: Arc<Config>,
    pub dynamic_config: Arc<RwLock<DynamicConfig>>,
    pub known_nodes: Arc<RwLock<Vec<NodeInfo>>>,
    /// Path to the on-disk config file, used to persist dynamic config updates.
    pub config_path: String,
}

impl SharedState {
    /// Construct a new `SharedState` and wrap it in an `Arc`.
    ///
    /// The `dynamic_config` is seeded from `config.dynamic_config`.
    /// Each address string in `config.known_nodes` becomes a `NodeInfo` whose `id` equals its `addr`.
    pub fn new(config: Config, config_path: String) -> Arc<Self> {
        let dynamic_config = config.dynamic_config.clone();
        let known_nodes = config
            .known_nodes
            .iter()
            .map(|addr| NodeInfo {
                id: addr.clone(),
                addr: addr.clone(),
            })
            .collect();

        Arc::new(Self {
            config: Arc::new(config),
            dynamic_config: Arc::new(RwLock::new(dynamic_config)),
            known_nodes: Arc::new(RwLock::new(known_nodes)),
            config_path,
        })
    }
}
