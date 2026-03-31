use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Operating mode of this daemon instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Node,
    Subscribe,
}

impl Default for Mode {
    fn default() -> Self {
        Self::Node
    }
}

/// A single service exposed by a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub name: String,
    pub node_id: String,
    pub node_port: u16,
}

/// A named collection of services.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceGroup {
    pub name: String,
    pub services: Vec<Service>,
}

/// Versioned, gossip-replicated configuration shared across the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicConfig {
    pub version: u64,
    pub service_groups: Vec<ServiceGroup>,
}

impl Default for DynamicConfig {
    fn default() -> Self {
        Self {
            version: 0,
            service_groups: Vec::new(),
        }
    }
}

/// Identity and address of a known peer node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub id: String,
    pub addr: String,
}

/// Top-level on-disk configuration for a stouter instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub cluster_secret: String,
    pub mode: Mode,
    /// Bind address for the listener, e.g. `"0.0.0.0:8080"`.
    pub bind: String,
    /// This node's identity (typically the hostname).
    pub node_id: String,
    pub local_secret: String,
    /// Peer addresses in `"host:port"` form.
    pub known_nodes: Vec<String>,
    pub dynamic_config: DynamicConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cluster_secret: String::new(),
            mode: Mode::default(),
            bind: String::from("0.0.0.0:8080"),
            node_id: String::new(),
            local_secret: String::new(),
            known_nodes: Vec::new(),
            dynamic_config: DynamicConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from a JSON file at `path`.
    pub fn load(path: &str) -> Result<Self> {
        let data = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&data)?;
        Ok(config)
    }

    /// Persist configuration as pretty-printed JSON to `path`.
    pub fn save(&self, path: &str) -> Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

/// Retrieve the system hostname, falling back to `"unknown"` on error.
pub fn get_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("unknown"))
}
