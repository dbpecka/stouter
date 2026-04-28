use std::collections::HashSet;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Operating mode of this daemon instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Node,
    Subscribe,
    NodeSubscribe,
}

impl Default for Mode {
    fn default() -> Self {
        Self::Node
    }
}

/// A single service exposed by a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub name: String,
    pub node_id: String,
    pub node_port: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
}

/// A named collection of services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceGroup {
    pub name: String,
    pub services: Vec<Service>,
}

/// Optional filter limiting which services a subscribe node will proxy.
///
/// If both lists are empty the filter has no effect (all services allowed).
/// Otherwise a service is allowed if its group name is in `service_groups`
/// OR its bare service name is in `services`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeFilter {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<String>,
}

impl SubscribeFilter {
    pub fn is_empty(&self) -> bool {
        self.service_groups.is_empty() && self.services.is_empty()
    }

    pub fn allows(&self, group_name: &str, service_name: &str) -> bool {
        self.service_groups.iter().any(|g| g == group_name)
            || self.services.iter().any(|s| s == service_name)
    }
}

/// Versioned, gossip-replicated configuration shared across the cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Number of idle reverse connections to maintain per relay peer (default 64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_pool_size: Option<u16>,
    /// Peer addresses in `"host:port"` form.
    pub known_nodes: Vec<String>,
    pub dynamic_config: DynamicConfig,
    /// Limits which services a subscribe node will proxy.
    /// Absent or empty → all services are proxied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe_filter: Option<SubscribeFilter>,
    /// Directory holding `ca.crt`, `ca.key` for TLS.
    /// When set, all inter-node connections are mutually authenticated with TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_dir: Option<String>,
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
            subscribe_filter: None,
            tls_dir: None,
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

    /// Validate structural invariants of the dynamic config:
    /// - service group names are unique
    /// - service names within a group are unique
    /// - domain names are globally unique across all services
    pub fn validate(&self) -> Result<()> {
        let mut group_names = HashSet::new();
        let mut domains = HashSet::new();

        for group in &self.dynamic_config.service_groups {
            if !group_names.insert(group.name.as_str()) {
                return Err(anyhow!("duplicate service group name: '{}'", group.name));
            }

            let mut service_names = HashSet::new();
            for svc in &group.services {
                if !service_names.insert(svc.name.as_str()) {
                    return Err(anyhow!(
                        "duplicate service name '{}' in group '{}'",
                        svc.name,
                        group.name
                    ));
                }
                for domain in &svc.domains {
                    if !domains.insert(domain.as_str()) {
                        return Err(anyhow!(
                            "duplicate domain '{}' (service '{}/{}')",
                            domain,
                            group.name,
                            svc.name
                        ));
                    }
                }
            }
        }

        if self.subscribe_filter.is_some()
            && !matches!(self.mode, Mode::Subscribe | Mode::NodeSubscribe)
        {
            return Err(anyhow!(
                "`subscribeFilter` is only valid in subscribe or nodeSubscribe mode"
            ));
        }

        Ok(())
    }

    /// Load a config file and validate its structural invariants.
    /// Reports JSON parse errors and validation errors distinctly.
    pub fn check(path: &str) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file '{path}'"))?;
        let config: Self = serde_json::from_str(&data)
            .with_context(|| format!("invalid JSON in '{path}'"))?;
        config.validate()?;
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
        self.reverse_pool_size.unwrap_or(64) as usize
    }
}

/// Retrieve the system hostname via syscall, falling back to `"unknown"` on error.
pub fn get_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
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
    fn subscribe_filter_is_empty() {
        let f = SubscribeFilter::default();
        assert!(f.is_empty());

        let f = SubscribeFilter {
            service_groups: vec!["web".into()],
            services: vec![],
        };
        assert!(!f.is_empty());

        let f = SubscribeFilter {
            service_groups: vec![],
            services: vec!["api".into()],
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn subscribe_filter_allows() {
        let f = SubscribeFilter {
            service_groups: vec!["web".into()],
            services: vec!["auth".into()],
        };

        assert!(f.allows("web", "anything"));
        assert!(f.allows("other", "auth"));
        assert!(!f.allows("other", "api"));
        assert!(f.allows("web", "auth"));
    }

    #[test]
    fn subscribe_filter_invalid_on_node_mode() {
        let cfg = Config {
            mode: Mode::Node,
            subscribe_filter: Some(SubscribeFilter {
                service_groups: vec!["web".into()],
                services: vec![],
            }),
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn subscribe_filter_valid_on_subscribe_mode() {
        let cfg = Config {
            mode: Mode::Subscribe,
            subscribe_filter: Some(SubscribeFilter {
                service_groups: vec!["web".into()],
                services: vec![],
            }),
            ..Config::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn nodesubscribe_mode_serde_round_trip() {
        let cfg = Config {
            mode: Mode::NodeSubscribe,
            ..Config::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"nodeSubscribe\""));
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored.mode, Mode::NodeSubscribe));
    }

    #[test]
    fn subscribe_filter_valid_on_node_subscribe_mode() {
        let cfg = Config {
            mode: Mode::NodeSubscribe,
            subscribe_filter: Some(SubscribeFilter {
                service_groups: vec!["web".into()],
                services: vec![],
            }),
            ..Config::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn outbound_only_accepted_on_node_subscribe_mode() {
        let cfg = Config {
            mode: Mode::NodeSubscribe,
            outbound_only: true,
            ..Config::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn subscribe_filter_serde_omits_empty_lists() {
        let f = SubscribeFilter {
            service_groups: vec!["web".into()],
            services: vec![],
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("serviceGroups"));
        assert!(!json.contains("services"));
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
