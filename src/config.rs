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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
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
    /// If set, this node is behind NAT and reachable only via this relay address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
}

/// Top-level on-disk configuration for a stouter instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub cluster_secret: String,
    pub mode: Mode,
    /// Bind address for the listener, e.g. `"0.0.0.0:8080"`.
    pub bind: String,
    /// Routable address peers should use to reach this node.
    /// Falls back to `bind` when not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise_addr: Option<String>,
    /// This node's identity (typically the hostname).
    pub node_id: String,
    pub local_secret: String,
    /// When true, the node only makes outbound connections (behind NAT).
    #[serde(default)]
    pub outbound_only: bool,
    /// Number of idle reverse connections to maintain per relay peer (default 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_pool_size: Option<u8>,
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
            advertise_addr: None,
            node_id: String::new(),
            local_secret: String::new(),
            outbound_only: false,
            reverse_pool_size: None,
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

    /// Return the address that peers should use to reach this node.
    ///
    /// Uses `advertise_addr` if set, otherwise falls back to `bind`.
    pub fn peer_addr(&self) -> &str {
        self.advertise_addr.as_deref().unwrap_or(&self.bind)
    }

    /// Number of idle reverse connections to maintain per relay peer.
    pub fn pool_size(&self) -> usize {
        self.reverse_pool_size.unwrap_or(4) as usize
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_values() {
        let cfg = Config::default();
        assert_eq!(cfg.bind, "0.0.0.0:8080");
        assert!(cfg.advertise_addr.is_none());
        assert!(cfg.known_nodes.is_empty());
        assert_eq!(cfg.dynamic_config.version, 0);
        assert!(cfg.dynamic_config.service_groups.is_empty());
    }

    #[test]
    fn peer_addr_falls_back_to_bind() {
        let cfg = Config {
            bind: "10.0.0.1:9090".into(),
            advertise_addr: None,
            ..Config::default()
        };
        assert_eq!(cfg.peer_addr(), "10.0.0.1:9090");
    }

    #[test]
    fn peer_addr_uses_advertise_when_set() {
        let cfg = Config {
            bind: "0.0.0.0:8080".into(),
            advertise_addr: Some("1.2.3.4:9090".into()),
            ..Config::default()
        };
        assert_eq!(cfg.peer_addr(), "1.2.3.4:9090");
    }

    #[test]
    fn config_serde_round_trip() {
        let cfg = Config {
            cluster_secret: "test-secret".into(),
            node_id: "node1".into(),
            bind: "10.0.0.1:8080".into(),
            advertise_addr: Some("external:8080".into()),
            known_nodes: vec!["peer1:8080".into()],
            ..Config::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.cluster_secret, "test-secret");
        assert_eq!(restored.node_id, "node1");
        assert_eq!(restored.advertise_addr.as_deref(), Some("external:8080"));
        assert_eq!(restored.known_nodes, vec!["peer1:8080"]);
    }

    #[test]
    fn config_serde_omits_advertise_addr_when_none() {
        let cfg = Config::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("advertiseAddr"));
    }

    #[test]
    fn config_save_and_load() {
        let path = std::env::temp_dir().join(format!("stouter_test_{}.json", std::process::id()));
        let path_str = path.to_str().unwrap();

        let cfg = Config {
            cluster_secret: "s3cret".into(),
            node_id: "host1".into(),
            dynamic_config: DynamicConfig {
                version: 5,
                service_groups: vec![ServiceGroup {
                    name: "web".into(),
                    services: vec![Service {
                        name: "api".into(),
                        node_id: "host1".into(),
                        node_port: 3000,
                        domains: vec!["api.example.com".into(), "www.api.example.com".into()],
                    }],
                }],
            },
            ..Config::default()
        };
        cfg.save(path_str).unwrap();
        let loaded = Config::load(path_str).unwrap();
        assert_eq!(loaded.cluster_secret, "s3cret");
        assert_eq!(loaded.dynamic_config.version, 5);
        assert_eq!(loaded.dynamic_config.service_groups[0].services[0].name, "api");

        let _ = std::fs::remove_file(&path);
    }
}
