use std::collections::HashSet;
use std::fmt;
use std::time::Instant;

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
    /// Relative share of new connections this instance receives among all
    /// instances of the same service. `0` drains it (no new connections).
    #[serde(default = "default_weight", skip_serializing_if = "is_default_weight")]
    pub weight: u32,
    /// Maximum concurrent connections each subscriber opens to this instance.
    /// When every instance is at its limit, new connections are rejected
    /// rather than queued. `None` means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    /// How subscribers proxy this service. A service is proxied as HTTP only
    /// when every instance says `http`.
    #[serde(default, skip_serializing_if = "Protocol::is_tcp")]
    pub protocol: Protocol,
    /// HTTP only: time allowed for this instance to send response headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// Path the hosting node GETs to health-check this instance; any non-2xx
    /// status counts as a failure. Unset: a TCP connect check only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_path: Option<String>,
}

/// Check that `path` can be sent verbatim as an HTTP request target.
pub fn validate_health_path(path: &str) -> std::result::Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("healthPath '{path}' must start with '/'"));
    }
    if path.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || !b.is_ascii()) {
        return Err(format!("healthPath '{path}' may not contain whitespace, control or non-ASCII characters"));
    }
    Ok(())
}

/// Wire protocol a service speaks, which decides how subscribers proxy it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "camelCase")]
pub enum Protocol {
    /// Opaque byte stream: one backend per client connection.
    #[default]
    Tcp,
    /// HTTP/1.1: requests are balanced, timed, retried and logged individually.
    Http,
}

impl Protocol {
    pub fn is_tcp(&self) -> bool {
        *self == Protocol::Tcp
    }
}

/// Weight assigned to a service instance when none is configured.
pub const DEFAULT_WEIGHT: u32 = 1;

pub(crate) fn default_weight() -> u32 {
    DEFAULT_WEIGHT
}

fn is_default_weight(w: &u32) -> bool {
    *w == DEFAULT_WEIGHT
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

/// Failure modes for in-memory mutations of `DynamicConfig`.
///
/// Used by the local CLI mutation path and by the remote admin RPC handler so
/// both report identical errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutateErr {
    GroupNotFound { group: String },
    ServiceNotFound { group: String, node: String, name: String },
    DuplicateService { group: String, node: String, name: String },
    Invalid(String),
}

impl fmt::Display for MutateErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MutateErr::GroupNotFound { group } => write!(f, "group '{group}' not found"),
            MutateErr::ServiceNotFound { group, node, name } => write!(
                f,
                "service '{name}' on node '{node}' not found in group '{group}'"
            ),
            MutateErr::DuplicateService { group, node, name } => write!(
                f,
                "service '{name}' already exists on node '{node}' in group '{group}'"
            ),
            MutateErr::Invalid(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for MutateErr {}

impl DynamicConfig {
    /// Insert a service into `group`, or replace it in place when an entry
    /// with the same `(name, node_id)` already exists. Bumps `version` on
    /// success and returns the new version.
    pub fn upsert_service(&mut self, group: &str, service: Service) -> u64 {
        if let Some(g) = self.service_groups.iter_mut().find(|g| g.name == group) {
            if let Some(existing) = g
                .services
                .iter_mut()
                .find(|s| s.name == service.name && s.node_id == service.node_id)
            {
                *existing = service;
            } else {
                g.services.push(service);
            }
        } else {
            self.service_groups.push(ServiceGroup {
                name: group.to_string(),
                services: vec![service],
            });
        }
        self.version += 1;
        self.version
    }

    /// Remove the service identified by `(group, owner_node_id, name)`.
    /// Bumps `version` on success and returns the new version.
    pub fn remove_service(
        &mut self,
        group: &str,
        owner_node_id: &str,
        name: &str,
    ) -> Result<u64, MutateErr> {
        let g = self
            .service_groups
            .iter_mut()
            .find(|g| g.name == group)
            .ok_or_else(|| MutateErr::GroupNotFound { group: group.to_string() })?;

        let before = g.services.len();
        g.services
            .retain(|s| !(s.name == name && s.node_id == owner_node_id));
        if g.services.len() == before {
            return Err(MutateErr::ServiceNotFound {
                group: group.to_string(),
                node: owner_node_id.to_string(),
                name: name.to_string(),
            });
        }

        self.version += 1;
        Ok(self.version)
    }

    /// Rename the service identified by `(group, owner_node_id, from)` to
    /// `to`. Bumps `version` on success and returns the new version.
    pub fn rename_service(
        &mut self,
        group: &str,
        owner_node_id: &str,
        from: &str,
        to: &str,
    ) -> Result<u64, MutateErr> {
        let g = self
            .service_groups
            .iter_mut()
            .find(|g| g.name == group)
            .ok_or_else(|| MutateErr::GroupNotFound { group: group.to_string() })?;

        if g.services
            .iter()
            .any(|s| s.name == to && s.node_id == owner_node_id)
        {
            return Err(MutateErr::DuplicateService {
                group: group.to_string(),
                node: owner_node_id.to_string(),
                name: to.to_string(),
            });
        }

        let svc = g
            .services
            .iter_mut()
            .find(|s| s.name == from && s.node_id == owner_node_id)
            .ok_or_else(|| MutateErr::ServiceNotFound {
                group: group.to_string(),
                node: owner_node_id.to_string(),
                name: from.to_string(),
            })?;
        svc.name = to.to_string();

        self.version += 1;
        Ok(self.version)
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
    /// Local-only liveness marker: when this node was last confirmed reachable
    /// (a successful outbound gossip sync, or a reverse-connection
    /// registration). Never gossiped or persisted — each daemon maintains it
    /// independently. `None` means not yet confirmed.
    #[serde(skip)]
    pub last_seen: Option<Instant>,
    /// Health of the services this node hosts, as reported by the node itself.
    #[serde(default, skip_serializing_if = "ServiceHealth::is_unset")]
    pub health: ServiceHealth,
}

/// A node's self-reported health for the services it hosts.
///
/// Only the hosting node writes this; everyone else relays it through gossip.
/// `version` orders reports so a relayed copy never overwrites a fresher one.
/// It is derived from wall-clock milliseconds, so a restarted node's reports
/// still supersede the ones peers held from before the restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceHealth {
    pub version: u64,
    /// Names of hosted services whose backend failed its health check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unhealthy: Vec<String>,
}

impl ServiceHealth {
    /// True if the node never reported health (nothing to gossip).
    pub fn is_unset(&self) -> bool {
        self.version == 0
    }

    pub fn is_unhealthy(&self, service_name: &str) -> bool {
        self.unhealthy.iter().any(|s| s == service_name)
    }
}

/// Top-level on-disk configuration for a stouter instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub mode: Mode,
    /// Bind address for the listener, e.g. `"0.0.0.0:8080"`.
    pub bind: String,
    /// Routable address peers should use to reach this node.
    /// Falls back to `bind` when not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise_addr: Option<String>,
    /// This node's identity (typically the hostname).
    pub node_id: String,
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
    /// Common Names allowed to issue admin RPCs against this daemon.
    /// Empty (the default) disables admin RPC entirely; clients can still
    /// query status with any cert signed by the cluster CA.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admin_cns: Vec<String>,
    /// Port for the subscribe-mode REST API on `127.0.0.1` (default 5381).
    /// Set this to a unique value when running multiple stouter clusters
    /// on the same host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_port: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            bind: String::from("0.0.0.0:8080"),
            advertise_addr: None,
            node_id: String::new(),
            outbound_only: false,
            reverse_pool_size: None,
            known_nodes: Vec::new(),
            dynamic_config: DynamicConfig::default(),
            subscribe_filter: None,
            tls_dir: None,
            admin_cns: Vec::new(),
            api_port: None,
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
        // A domain may be shared by instances of one service, but not
        // claimed by two different services.
        let mut domain_owner: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();

        for group in &self.dynamic_config.service_groups {
            if !group_names.insert(group.name.as_str()) {
                return Err(anyhow!("duplicate service group name: '{}'", group.name));
            }

            // The same name on several nodes is one service with several
            // instances; only the same name twice on one node is a duplicate.
            let mut instances = HashSet::new();
            for svc in &group.services {
                if !instances.insert((svc.node_id.as_str(), svc.name.as_str())) {
                    return Err(anyhow!(
                        "duplicate service '{}' on node '{}' in group '{}'",
                        svc.name,
                        svc.node_id,
                        group.name
                    ));
                }
                if let Some(path) = &svc.health_path {
                    validate_health_path(path).map_err(|e| {
                        anyhow!("service '{}/{}' on node '{}': {e}", group.name, svc.name, svc.node_id)
                    })?;
                }
                for domain in &svc.domains {
                    match domain_owner.insert(domain.as_str(), svc.name.as_str()) {
                        Some(owner) if owner != svc.name => {
                            return Err(anyhow!(
                                "domain '{}' is claimed by both '{}' and '{}/{}'",
                                domain,
                                owner,
                                group.name,
                                svc.name
                            ));
                        }
                        _ => {}
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

    /// Port for the subscribe-mode REST API on `127.0.0.1`.
    pub fn api_port(&self) -> u16 {
        self.api_port.unwrap_or(5381)
    }

    /// Connect target for querying *this* daemon from the local host.
    ///
    /// Wildcard bind addresses (`0.0.0.0`, `[::]`) are not valid connect
    /// targets — they're rewritten to the matching loopback so a local CLI
    /// (e.g. `stouter status`) can reach a daemon that binds on all
    /// interfaces.
    pub fn local_target(&self) -> String {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
        match self.bind.parse::<SocketAddr>() {
            Ok(addr) if addr.ip().is_unspecified() => {
                let loopback = match addr.ip() {
                    IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
                };
                SocketAddr::new(loopback, addr.port()).to_string()
            }
            _ => self.bind.clone(),
        }
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
            node_id: "node1".into(),
            bind: "10.0.0.1:8080".into(),
            advertise_addr: Some("external:8080".into()),
            known_nodes: vec!["peer1:8080".into()],
            ..Config::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
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

    fn group_of(services: Vec<Service>) -> Config {
        Config {
            dynamic_config: DynamicConfig {
                version: 1,
                service_groups: vec![ServiceGroup { name: "web".into(), services }],
            },
            ..Config::default()
        }
    }

    fn instance(name: &str, node: &str, domains: &[&str]) -> Service {
        let json = serde_json::json!({
            "name": name, "nodeId": node, "nodePort": 3000, "domains": domains,
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn validate_accepts_multi_instance_services_sharing_a_domain() {
        let cfg = group_of(vec![
            instance("api", "n1", &["api.example.com"]),
            instance("api", "n2", &["api.example.com"]),
        ]);
        cfg.validate().unwrap();
    }

    #[test]
    fn health_path_validation() {
        assert!(validate_health_path("/healthz?full=1").is_ok());
        for bad in ["healthz", "/a b", "/a\r\nHost: x", "/ü", ""] {
            assert!(validate_health_path(bad).is_err(), "{bad:?}");
        }
        let mut svc = instance("api", "n1", &[]);
        svc.health_path = Some("/x y".into());
        assert!(group_of(vec![svc]).validate().is_err());
    }

    #[test]
    fn validate_rejects_same_service_twice_on_one_node() {
        let cfg = group_of(vec![instance("api", "n1", &[]), instance("api", "n1", &[])]);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_domain_claimed_by_two_services() {
        let cfg = group_of(vec![
            instance("api", "n1", &["x.example.com"]),
            instance("web", "n2", &["x.example.com"]),
        ]);
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
    fn upsert_service_creates_group_when_missing() {
        let mut dc = DynamicConfig::default();
        let v = dc.upsert_service(
            "web",
            Service {
                name: "api".into(),
                node_id: "n1".into(),
                node_port: 3000,
                domains: vec![],
                weight: 1,
                max_connections: None,
                protocol: crate::config::Protocol::Tcp,
                request_timeout_ms: None,
                health_path: None,
            },
        );
        assert_eq!(v, 1);
        assert_eq!(dc.version, 1);
        assert_eq!(dc.service_groups.len(), 1);
        assert_eq!(dc.service_groups[0].name, "web");
        assert_eq!(dc.service_groups[0].services.len(), 1);
    }

    #[test]
    fn upsert_service_appends_to_existing_group() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        let v = dc.upsert_service(
            "web",
            Service { name: "ui".into(), node_id: "n1".into(), node_port: 3001, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        assert_eq!(v, 2);
        assert_eq!(dc.service_groups.len(), 1);
        assert_eq!(dc.service_groups[0].services.len(), 2);
    }

    #[test]
    fn upsert_service_replaces_existing_match_by_name_and_node() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        let v = dc.upsert_service(
            "web",
            Service {
                name: "api".into(),
                node_id: "n1".into(),
                node_port: 9999,
                domains: vec!["x.example".into()],
                weight: 9,
                max_connections: Some(5),
                protocol: crate::config::Protocol::Http,
                request_timeout_ms: Some(100),
                health_path: Some("/h".into()),
            },
        );
        assert_eq!(v, 2);
        assert_eq!(dc.service_groups[0].services.len(), 1);
        let svc = &dc.service_groups[0].services[0];
        assert_eq!(svc.node_port, 9999);
        assert_eq!(svc.domains, vec!["x.example".to_string()]);
        // Every field is replaced, not just port and domains.
        assert_eq!(svc.weight, 9);
        assert_eq!(svc.max_connections, Some(5));
        assert_eq!(svc.protocol, Protocol::Http);
        assert_eq!(svc.request_timeout_ms, Some(100));
        assert_eq!(svc.health_path.as_deref(), Some("/h"));
    }

    #[test]
    fn upsert_service_keeps_same_name_on_different_nodes() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n2".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        assert_eq!(dc.service_groups[0].services.len(), 2);
    }

    #[test]
    fn remove_service_success_bumps_version() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        let v = dc.remove_service("web", "n1", "api").unwrap();
        assert_eq!(v, 2);
        assert!(dc.service_groups[0].services.is_empty());
    }

    #[test]
    fn remove_service_missing_group_returns_err() {
        let mut dc = DynamicConfig::default();
        let err = dc.remove_service("nope", "n1", "api").unwrap_err();
        assert!(matches!(err, MutateErr::GroupNotFound { .. }));
    }

    #[test]
    fn remove_service_missing_service_returns_err() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "ui".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        let err = dc.remove_service("web", "n1", "api").unwrap_err();
        assert!(matches!(err, MutateErr::ServiceNotFound { .. }));
    }

    #[test]
    fn remove_service_leaves_other_services_alone() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n2".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        dc.remove_service("web", "n1", "api").unwrap();
        assert_eq!(dc.service_groups[0].services.len(), 1);
        assert_eq!(dc.service_groups[0].services[0].node_id, "n2");
    }

    #[test]
    fn rename_service_success_bumps_version() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        let v = dc.rename_service("web", "n1", "api", "api2").unwrap();
        assert_eq!(v, 2);
        assert_eq!(dc.service_groups[0].services[0].name, "api2");
    }

    #[test]
    fn rename_service_missing_group_returns_err() {
        let mut dc = DynamicConfig::default();
        let err = dc.rename_service("nope", "n1", "api", "api2").unwrap_err();
        assert!(matches!(err, MutateErr::GroupNotFound { .. }));
    }

    #[test]
    fn rename_service_missing_source_returns_err() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "ui".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        let err = dc.rename_service("web", "n1", "api", "api2").unwrap_err();
        assert!(matches!(err, MutateErr::ServiceNotFound { .. }));
    }

    #[test]
    fn rename_service_conflicts_on_destination() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        dc.upsert_service(
            "web",
            Service { name: "api2".into(), node_id: "n1".into(), node_port: 3001, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None },
        );
        let err = dc.rename_service("web", "n1", "api", "api2").unwrap_err();
        assert!(matches!(err, MutateErr::DuplicateService { .. }));
    }

    #[test]
    fn admin_cns_round_trips_and_omitted_when_empty() {
        let cfg = Config::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("adminCns"));

        let cfg = Config { admin_cns: vec!["admin-cli".into()], ..Config::default() };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"adminCns\":[\"admin-cli\"]"));
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.admin_cns, vec!["admin-cli".to_string()]);
    }

    #[test]
    fn service_weight_defaults_and_omitted_when_default() {
        let svc: Service =
            serde_json::from_str(r#"{"name":"api","nodeId":"n1","nodePort":3000}"#).unwrap();
        assert_eq!(svc.weight, DEFAULT_WEIGHT);
        assert!(!serde_json::to_string(&svc).unwrap().contains("weight"));

        let svc = Service { weight: 0, ..svc };
        let json = serde_json::to_string(&svc).unwrap();
        assert!(json.contains("\"weight\":0"));
        let restored: Service = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.weight, 0);
    }

    #[test]
    fn local_target_rewrites_wildcard_v4() {
        let cfg = Config { bind: "0.0.0.0:60001".into(), ..Config::default() };
        assert_eq!(cfg.local_target(), "127.0.0.1:60001");
    }

    #[test]
    fn local_target_rewrites_wildcard_v6() {
        let cfg = Config { bind: "[::]:60001".into(), ..Config::default() };
        assert_eq!(cfg.local_target(), "[::1]:60001");
    }

    #[test]
    fn local_target_preserves_specific_address() {
        let cfg = Config { bind: "10.0.0.5:60001".into(), ..Config::default() };
        assert_eq!(cfg.local_target(), "10.0.0.5:60001");
    }

    #[test]
    fn local_target_preserves_unparseable_bind() {
        let cfg = Config { bind: "myhost:60001".into(), ..Config::default() };
        assert_eq!(cfg.local_target(), "myhost:60001");
    }

    #[test]
    fn config_save_and_load() {
        let path = std::env::temp_dir().join(format!("stouter_test_{}.json", std::process::id()));
        let path_str = path.to_str().unwrap();

        let cfg = Config {
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
                        weight: 1,
                        max_connections: None,
                        protocol: crate::config::Protocol::Tcp,
                        request_timeout_ms: None,
                        health_path: None,
                    }],
                }],
            },
            ..Config::default()
        };
        cfg.save(path_str).unwrap();
        let loaded = Config::load(path_str).unwrap();
        assert_eq!(loaded.dynamic_config.version, 5);
        assert_eq!(loaded.dynamic_config.service_groups[0].services[0].name, "api");

        let _ = std::fs::remove_file(&path);
    }
}
