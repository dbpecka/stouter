use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Operating mode of this daemon instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    #[default]
    Node,
    Subscribe,
    NodeSubscribe,
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
    /// Free-form key/value labels that routes select instances by
    /// (e.g. `version: v2`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Free-form key/value metadata for integrations, kept as-is and served
    /// by the subscribe REST API (e.g. `traefik.scheme: h2c` for the Traefik
    /// plugin). Stouter itself does not interpret it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
    /// Fixed `127.0.0.1` port subscribers open this service's proxy on,
    /// instead of a random one. Every instance that sets it must agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_port: Option<u16>,
}

/// True if `labels` include every key/value in `selector`.
pub fn labels_match(labels: &BTreeMap<String, String>, selector: &BTreeMap<String, String>) -> bool {
    selector.iter().all(|(k, v)| labels.get(k) == Some(v))
}

/// An L7 routing rule for an `http` service.
///
/// A service's routes are tried in order; the first whose `match` fits the
/// request decides which instances may serve it. Requests matching no route
/// go to all instances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    /// Optional name, shown in access logs and metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Service this route applies to.
    pub service: String,
    /// Conditions a request must meet; all must hold. Empty matches all.
    #[serde(default, rename = "match")]
    pub matches: RouteMatch,
    /// Instance subsets to send matching requests to, split by weight.
    pub backends: Vec<RouteBackend>,
}

impl Route {
    /// Label for logs and metrics: the name, or the route's position.
    pub fn label(&self, index: usize) -> String {
        self.name.clone().unwrap_or_else(|| format!("#{index}"))
    }
}

/// Request conditions for a [`Route`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteMatch {
    /// Path prefix, matched on segment boundaries: `/v2` matches `/v2` and
    /// `/v2/users` but not `/v20`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Exact request method, e.g. `GET`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Headers that must be present with exactly these values (names are
    /// case-insensitive).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl RouteMatch {
    /// Whether a request with this method, path and headers matches.
    /// `header` looks up a header value by (lowercase) name.
    pub fn matches<'a>(
        &self,
        method: &str,
        path: &str,
        header: impl Fn(&str) -> Option<&'a str>,
    ) -> bool {
        if let Some(prefix) = &self.path_prefix {
            if !path_has_prefix(path, prefix) {
                return false;
            }
        }
        if let Some(m) = &self.method {
            if m != method {
                return false;
            }
        }
        self.headers
            .iter()
            .all(|(name, value)| header(&name.to_ascii_lowercase()) == Some(value.as_str()))
    }
}

/// Segment-aware prefix match: `/v2` matches `/v2` and `/v2/x`, not `/v20`.
fn path_has_prefix(path: &str, prefix: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || prefix.ends_with('/') || rest.starts_with('/'),
        None => false,
    }
}

/// One weighted instance subset of a [`Route`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteBackend {
    /// Instances carrying all of these labels form the subset. Empty selects
    /// every instance.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Share of matching requests sent to this subset.
    #[serde(default = "default_weight", skip_serializing_if = "is_default_weight")]
    pub weight: u32,
}

/// Check a service's route list for mistakes that would make it misbehave.
pub fn validate_routes(service: &str, routes: &[Route]) -> std::result::Result<(), String> {
    for (i, route) in routes.iter().enumerate() {
        let at = format!("route {} of service '{service}'", route.label(i));
        if route.service != service {
            return Err(format!("{at} names service '{}'", route.service));
        }
        if route.backends.is_empty() {
            return Err(format!("{at} has no backends"));
        }
        if route.backends.iter().all(|b| b.weight == 0) {
            return Err(format!("{at} has no backend with a non-zero weight"));
        }
        if let Some(prefix) = &route.matches.path_prefix {
            if !prefix.starts_with('/') {
                return Err(format!("{at}: pathPrefix '{prefix}' must start with '/'"));
            }
        }
        if let Some(method) = &route.matches.method {
            if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(format!("{at}: method '{method}' must be an uppercase token like GET"));
            }
        }
        for name in route.matches.headers.keys() {
            if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(format!("{at}: '{name}' is not a valid header name"));
            }
        }
    }
    Ok(())
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

/// Largest encoded config (signature included) accepted as a change: a
/// quarter of a gossip message, leaving the rest for the node list.
pub const MAX_CONFIG_BYTES: usize = 2 * 1024 * 1024;

/// Versioned, gossip-replicated configuration shared across the cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicConfig {
    pub version: u64,
    pub service_groups: Vec<ServiceGroup>,
    /// L7 routing rules for `http` services, in evaluation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
    /// Node (or CLI host) that made this version. Settles which of two
    /// different changes made at the same version wins, the same way on
    /// every node (see [`DynamicConfig::supersedes`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub changed_by: String,
    /// An admin's signature over this config, when it has one. Nodes with
    /// `requireSignedConfig` accept a change only if it is signed (see
    /// `crate::signing`); everyone else keeps and relays it as is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ConfigSignature>,
}

/// An admin's signature over a [`DynamicConfig`].
///
/// It carries the exact JSON text that was signed, so it verifies no matter
/// how the config was re-encoded on the way (an older node may drop fields
/// it doesn't know). A verified config is always decoded from that text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSignature {
    /// The signed config (with no `signature`), as JSON text.
    pub config_json: String,
    /// The signer's certificate (DER, hex). It must chain to the cluster CA
    /// and name an admin (`adminCns`).
    pub signer_cert: String,
    /// ECDSA P-256/SHA-256 signature (DER, hex) over a fixed prefix and
    /// `config_json`.
    pub signature: String,
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
    /// Not signed (validly, by an admin) where a signature is required.
    Unauthorized(String),
    /// A pushed config was made to an older version than `current`.
    Conflict { current: u64 },
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
            MutateErr::Invalid(msg) | MutateErr::Unauthorized(msg) => write!(f, "{msg}"),
            MutateErr::Conflict { current } => {
                write!(f, "the config changed (now version {current}) since this change was made to it")
            }
        }
    }
}

impl std::error::Error for MutateErr {}

impl DynamicConfig {
    /// Whether `self` and `other` configure the same services and routes,
    /// whatever their version, author and signature.
    pub fn same_settings(&self, other: &DynamicConfig) -> bool {
        let bare = |dc: &DynamicConfig| DynamicConfig {
            version: 0,
            changed_by: String::new(),
            signature: None,
            ..dc.clone()
        };
        bare(self) == bare(other)
    }

    /// Whether this config should replace `current`: it has a higher
    /// version, or the same version with different content that wins the
    /// tie-break (higher `changedBy`, then the longer serialized form, then
    /// the larger one).
    ///
    /// Without the tie-break, two nodes changing the config at once both
    /// make version N+1, each node keeps whichever arrived first, and the
    /// cluster stays split until the next change. Comparing `changedBy`
    /// first also keeps a newer node's config from losing to the copy an
    /// older node relays without the fields it doesn't know (and without
    /// `changedBy`). Then the longer form: one change, as made, against a
    /// copy of it relayed by a node that dropped fields it doesn't know (a
    /// signature, a newer setting). The fuller copy must win, or a rolling
    /// upgrade would erase what the newer nodes added, cluster-wide.
    pub fn supersedes(&self, current: &DynamicConfig) -> bool {
        if self.version != current.version {
            return self.version > current.version;
        }
        if self == current {
            return false;
        }
        let key = |dc: &DynamicConfig| {
            let bytes = serde_json::to_vec(dc).unwrap_or_default();
            (dc.changed_by.clone(), bytes.len(), bytes)
        };
        key(self) > key(current)
    }

    /// Every structural problem with this config, as messages. Empty means
    /// valid. Messages are deterministic, so two configs' lists can be
    /// compared (see [`new_problem`]).
    pub fn problems(&self) -> Vec<String> {
        let mut problems: Vec<String> = Vec::new();
        let mut report = |problem: String| {
            if !problems.contains(&problem) {
                problems.push(problem);
            }
        };

        let mut group_names = HashSet::new();
        // A domain may be shared by instances of one service, but not
        // claimed by two different services.
        let mut domain_owner: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        // Instances of one service share a group: everything else (routes,
        // proxies, the REST API, httpListen) knows a service by name only.
        let mut service_group: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();

        for group in &self.service_groups {
            if !group_names.insert(group.name.as_str()) {
                report(format!("duplicate service group name: '{}'", group.name));
            }

            // The same name on several nodes is one service with several
            // instances; only the same name twice on one node is a duplicate.
            let mut instances = HashSet::new();
            for svc in &group.services {
                if !instances.insert((svc.node_id.as_str(), svc.name.as_str())) {
                    report(format!(
                        "duplicate service '{}' on node '{}' in group '{}'",
                        svc.name, svc.node_id, group.name
                    ));
                }
                match service_group.insert(svc.name.as_str(), group.name.as_str()) {
                    Some(other) if other != group.name => report(format!(
                        "service '{}' is in both group '{other}' and group '{}'; all instances of a service must be in one group",
                        svc.name, group.name
                    )),
                    _ => {}
                }
                if let Some(path) = &svc.health_path {
                    if let Err(e) = validate_health_path(path) {
                        report(format!("service '{}/{}' on node '{}': {e}", group.name, svc.name, svc.node_id));
                    }
                }
                for domain in &svc.domains {
                    match domain_owner.insert(domain.as_str(), svc.name.as_str()) {
                        Some(owner) if owner != svc.name => report(format!(
                            "domain '{}' is claimed by both '{}' and '{}/{}'",
                            domain, owner, group.name, svc.name
                        )),
                        _ => {}
                    }
                }
            }
        }

        let mut routed: Vec<&str> = self.routes.iter().map(|r| r.service.as_str()).collect();
        routed.sort();
        routed.dedup();
        for service in routed {
            if let Err(e) = self.validate_route_target(service) {
                report(e);
            }
            let routes: Vec<Route> = self.routes.iter().filter(|r| r.service == service).cloned().collect();
            if let Err(e) = validate_routes(service, &routes) {
                report(e);
            }
        }

        // Fixed local ports: consistent per service and distinct across
        // services. (Clashes with a daemon's own listeners depend on its
        // static config; `Config::validate` checks those.)
        let mut port_owner: std::collections::HashMap<u16, &str> = std::collections::HashMap::new();
        for name in self.service_names() {
            match self.local_port_for(name) {
                Err(e) => report(e),
                Ok(Some(0)) => report(format!("localPort of '{name}' must not be 0")),
                Ok(Some(port)) => {
                    if let Some(other) = port_owner.insert(port, name) {
                        report(format!("localPort {port} of '{name}' is already used by {other}"));
                    }
                }
                Ok(None) => {}
            }
        }

        // It must fit in the gossip messages that carry it (with room for
        // the node list), or it would never reach the rest of the cluster.
        let size = rmp_serde::to_vec_named(self).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_CONFIG_BYTES {
            report(format!(
                "the config is too large to gossip: {size} bytes encoded, limit {MAX_CONFIG_BYTES} \
                 (a signed config counts its signed copy too)"
            ));
        }
        problems
    }

    /// Distinct service names, sorted.
    fn service_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.service_groups.iter().flat_map(|g| &g.services).map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        names
    }

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

    /// The fixed local proxy port for `service`: the `localPort` its
    /// instances set, `None` if none set one, or an error if they disagree.
    pub fn local_port_for(&self, service: &str) -> std::result::Result<Option<u16>, String> {
        let mut ports: Vec<u16> = self
            .service_groups
            .iter()
            .flat_map(|g| &g.services)
            .filter(|s| s.name == service)
            .filter_map(|s| s.local_port)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        match ports.as_slice() {
            [] => Ok(None),
            [port] => Ok(Some(*port)),
            many => Err(format!("instances of '{service}' set different localPorts: {many:?}")),
        }
    }

    /// Check that `service` can carry routes: it exists and every instance
    /// speaks HTTP.
    pub fn validate_route_target(&self, service: &str) -> std::result::Result<(), String> {
        let instances: Vec<&Service> = self
            .service_groups
            .iter()
            .flat_map(|g| &g.services)
            .filter(|s| s.name == service)
            .collect();
        if instances.is_empty() {
            return Err(format!("routes reference unknown service '{service}'"));
        }
        if instances.iter().any(|s| s.protocol != Protocol::Http) {
            return Err(format!("routes for '{service}' require every instance to have protocol: http"));
        }
        Ok(())
    }

    /// Replace all routes for `service` with `routes` (empty clears them).
    /// Bumps `version` and returns the new version.
    pub fn set_routes(&mut self, service: &str, routes: Vec<Route>) -> Result<u64, MutateErr> {
        if !routes.is_empty() {
            self.validate_route_target(service).map_err(MutateErr::Invalid)?;
        }
        validate_routes(service, &routes).map_err(MutateErr::Invalid)?;
        // Keep other services' routes where they were.
        let insert_at = self.routes.iter().position(|r| r.service == service);
        self.routes.retain(|r| r.service != service);
        let at = insert_at.unwrap_or(self.routes.len()).min(self.routes.len());
        self.routes.splice(at..at, routes);
        self.version += 1;
        Ok(self.version)
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

/// The first problem `after` has that `before` didn't. Changes are checked
/// with this: one is refused for what it breaks, not for problems the config
/// already had (which would otherwise block every unrelated change).
pub fn new_problem(before: &DynamicConfig, after: &DynamicConfig) -> Option<String> {
    let existing = before.problems();
    after.problems().into_iter().find(|p| !existing.contains(p))
}

/// Identity and address of a known peer node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub id: String,
    pub addr: String,
    /// If set, this node is behind NAT and reachable only via this relay address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// Local-only liveness marker for load balancing: when this node was last
    /// confirmed reachable (a successful outbound gossip sync or a
    /// reverse-connection registration; for a node with no address, which
    /// can't be contacted directly, a rising `heartbeat`). Never gossiped or
    /// persisted. `None` means not yet confirmed.
    #[serde(skip)]
    pub last_seen: Option<Instant>,
    /// Health of the services this node hosts, as reported by the node itself.
    #[serde(default, skip_serializing_if = "ServiceHealth::is_unset")]
    pub health: ServiceHealth,
    /// Liveness counter only the node itself advances (its incarnation, then
    /// a counter; see `SharedState::next_heartbeat`), relayed by everyone
    /// else. A rising value proves the node is alive even to peers that
    /// can't reach it. `0`: a node too old to send one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub heartbeat: u64,
    /// Local-only: when we last had any evidence the node is alive (direct
    /// contact or a rising `heartbeat`). A node silent for long enough is
    /// forgotten.
    #[serde(skip)]
    pub heard_at: Option<Instant>,
    /// The node's own signed account of itself, relayed as is. When present
    /// (and valid), `addr`, `heartbeat`, `health` and `relays` come from it
    /// and nothing anyone else says about the node can change them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed: Option<SignedNode>,
    /// Local-only, from the signed record: the relays (node ids) a NAT'd
    /// node holds reverse connections to, the only ones its tunnels may go
    /// through.
    #[serde(skip)]
    pub relays: Vec<String>,
    /// Local-only: what its signed record says of it (`NodeRecord::subscriber`).
    #[serde(skip)]
    pub subscriber: bool,
    /// Local-only: this entry was made by a reverse registration here (we
    /// relay for the node), so registrations show whether it's alive and
    /// it can expire like any other (see `gossip::expire_silent_nodes`).
    #[serde(skip)]
    pub registered_here: bool,
    /// Local-only: the version (`heartbeat`) of the signed record held.
    #[serde(skip)]
    pub record_version: u64,
    /// Local-only: the latest beat verified for a node with no address,
    /// relayed so peers that can't reach it know it's alive.
    #[serde(skip)]
    pub beat: Option<SignedBeat>,
}

/// Proof that a node (one with no address, which peers can't contact) was
/// alive at `beat`, signed by it. Much smaller than its record, which
/// changes only when something in it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedBeat {
    pub id: String,
    /// Ordered like heartbeats.
    pub beat: u64,
    /// ECDSA P-256/SHA-256 signature (DER, hex) by the key of the node's
    /// record certificate.
    pub signature: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What a node says about itself; see [`NodeInfo::signed`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRecord {
    pub id: String,
    /// Where peers connect to it; empty for an `outboundOnly` node.
    pub addr: String,
    /// Orders records: a node's newer record always has a higher one. It
    /// changes only when the record does (or the node restarts); liveness
    /// comes from direct contact or, for a node with no address, beats.
    pub heartbeat: u64,
    #[serde(default)]
    pub health: ServiceHealth,
    /// Node ids of the relays it holds reverse connections to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relays: Vec<String>,
    /// The node's last record: it left the cluster. Relayed like any other,
    /// so the departure reaches nodes the leave message didn't.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub left: bool,
    /// A subscribe-mode node: it uses services, hosts none, and is often
    /// a client machine, so NAT'd nodes don't pick it as a relay (other
    /// subscribers' traffic would go through it) while there are others.
    /// Absent from older nodes' records: they may be anything.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subscriber: bool,
}

/// A [`NodeRecord`] signed by its node: the exact JSON text, the node's
/// certificate (DER, hex) and an ECDSA P-256/SHA-256 signature (DER, hex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedNode {
    pub record_json: String,
    pub cert: String,
    pub signature: String,
}

/// A node's self-reported health for the services it hosts.
///
/// Only the hosting node writes this; everyone else relays it through gossip.
/// `version` orders reports so a relayed copy never overwrites a fresher one.
/// It is ordered like heartbeats, so a restarted node's reports still
/// supersede the ones peers held from before the restart.
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
    /// OpenTelemetry span export for `http` services. Unset: no export
    /// (trace context is still propagated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracing: Option<TracingConfig>,
    /// Subscribe modes: one HTTP listener (e.g. `127.0.0.1:15001`) that routes
    /// each request to an `http` service by its URL host or `Host` header, so
    /// callers can use `HTTP_PROXY` instead of per-service ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_listen: Option<String>,
    /// Accept a config change (from gossip, admin RPC or this node's own
    /// config file) only if a certificate named in `adminCns` signed it, so
    /// no other node can rewrite the cluster's config. Enable it on every
    /// node once all run a version that supports it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_signed_config: bool,
    /// Settings this version doesn't know: most likely a typo (a
    /// `requireSignedConfg` would leave signing off without a word), or one
    /// from a newer version. Kept as they are when the file is rewritten;
    /// the daemon warns of them, and `stouter check` fails on them.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, serde_json::Value>,
}

/// Where and how much to export as OpenTelemetry spans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TracingConfig {
    /// OTLP/HTTP collector base URL, e.g. `http://127.0.0.1:4318`; spans are
    /// POSTed as JSON to `{otlpEndpoint}/v1/traces`. Plain HTTP only: point it
    /// at a local OpenTelemetry Collector to reach TLS backends.
    pub otlp_endpoint: String,
    /// Fraction of new traces (requests arriving without a `traceparent`)
    /// to sample, from 0.0 to 1.0. Requests that carry a `traceparent` follow
    /// its sampled flag instead.
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: f64,
    /// `service.name` resource attribute on exported spans.
    #[serde(default = "default_tracing_service_name")]
    pub service_name: String,
}

fn default_sample_ratio() -> f64 {
    1.0
}

fn default_tracing_service_name() -> String {
    "stouter".to_string()
}

impl TracingConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        let uri: http::Uri = self
            .otlp_endpoint
            .parse()
            .map_err(|e| format!("tracing.otlpEndpoint '{}' is not a URL: {e}", self.otlp_endpoint))?;
        match uri.scheme_str() {
            Some("http") => {}
            Some("https") => {
                return Err(format!(
                    "tracing.otlpEndpoint '{}': https is not supported; send to a local OpenTelemetry Collector over http and let it forward over TLS",
                    self.otlp_endpoint
                ))
            }
            _ => return Err(format!("tracing.otlpEndpoint '{}' must start with http://", self.otlp_endpoint)),
        }
        if uri.host().is_none() {
            return Err(format!("tracing.otlpEndpoint '{}' has no host", self.otlp_endpoint));
        }
        if !(0.0..=1.0).contains(&self.sample_ratio) {
            return Err(format!("tracing.sampleRatio {} must be between 0.0 and 1.0", self.sample_ratio));
        }
        Ok(())
    }
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
            tracing: None,
            http_listen: None,
            require_signed_config: false,
            unknown: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load configuration from a JSON file at `path`.
    pub fn load(path: &str) -> Result<Self> {
        let data = std::fs::read_to_string(path).with_context(|| format!("failed to read config file '{path}'"))?;
        let config: Self = serde_json::from_str(&data).with_context(|| format!("invalid JSON in '{path}'"))?;
        Ok(config)
    }

    /// Load `path`, or the default config if there is no such file. A file
    /// that exists but can't be read or parsed is an error, never defaults:
    /// a command that went on and saved would replace it, settings and all.
    pub fn load_or_default(path: &str) -> Result<Self> {
        match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            _ => Self::load(path),
        }
    }

    /// Everything wrong with the config: [`Config::validate_local`]'s error,
    /// else the first of [`Config::config_problems`]. What `stouter check`
    /// reports.
    pub fn validate(&self) -> Result<()> {
        self.validate_local()?;
        if let Some(problem) = self.unknown_settings().into_iter().next() {
            return Err(anyhow!(problem));
        }
        match self.config_problems().into_iter().next() {
            Some(problem) => Err(anyhow!(problem)),
            None => Ok(()),
        }
    }

    /// Check this node's own settings (everything but `dynamicConfig`): a
    /// problem here is the operator's to fix, so the daemon won't start
    /// with one.
    pub fn validate_local(&self) -> Result<()> {
        let subscribes = matches!(self.mode, Mode::Subscribe | Mode::NodeSubscribe);
        // Peers know a node by its id (its certificate's CN), and ignore one
        // that looks like an address rather than a name.
        if self.node_id.is_empty() {
            return Err(anyhow!("`nodeId` is empty: set it to this host's name (`stouter init` does)"));
        }
        if self.node_id.contains(':') || self.node_id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(anyhow!(
                "`nodeId` '{}' isn't a name: no ':' (peers would take it for an address and ignore the node), \
                 spaces or control characters",
                self.node_id
            ));
        }
        if let Some(listen) = &self.http_listen {
            let addr: std::net::SocketAddr = listen
                .parse()
                .map_err(|e| anyhow!("httpListen '{listen}' is not an address like 127.0.0.1:15001: {e}"))?;
            if !subscribes {
                return Err(anyhow!("`httpListen` is only valid in subscribe or nodeSubscribe mode"));
            }
            if (addr.ip().is_loopback() || addr.ip().is_unspecified()) && addr.port() == self.api_port() {
                return Err(anyhow!("httpListen port {} is already used by the REST API (apiPort)", addr.port()));
            }
        }

        if let Some(tracing) = &self.tracing {
            tracing.validate().map_err(|e| anyhow!(e))?;
        }

        if usize::from(self.reverse_pool_size.unwrap_or(0)) > crate::node::relay::MAX_IDLE_REVERSE_PER_NODE {
            return Err(anyhow!(
                "reversePoolSize may be at most {}: relays refuse more idle reverse connections from one node",
                crate::node::relay::MAX_IDLE_REVERSE_PER_NODE
            ));
        }

        if self.require_signed_config && self.admin_cns.is_empty() {
            return Err(anyhow!(
                "`requireSignedConfig` needs `adminCns`: without an admin, no change could ever be signed"
            ));
        }

        if self.subscribe_filter.is_some() && !subscribes {
            return Err(anyhow!(
                "`subscribeFilter` is only valid in subscribe or nodeSubscribe mode"
            ));
        }

        Ok(())
    }

    /// Settings in the file this version doesn't know (see [`Config::unknown`]),
    /// as messages.
    pub fn unknown_settings(&self) -> Vec<String> {
        self.unknown
            .keys()
            .map(|key| format!("unknown setting '{key}' (a typo? it is ignored)"))
            .collect()
    }

    /// Problems with the dynamic config, as it stands here: its own (see
    /// [`DynamicConfig::problems`]: e.g. duplicate group, service or domain
    /// names), and fixed `localPort`s that collide with this daemon's own
    /// listeners. The dynamic config is the cluster's, so a daemon starts
    /// with these, logging them: a newer version may already fix them, and
    /// refusing to start would strand the node over someone else's change.
    pub fn config_problems(&self) -> Vec<String> {
        let mut problems = self.dynamic_config.problems();

        // Fixed local ports must stay clear of the daemon's own loopback
        // listeners.
        let mut port_owner: std::collections::HashMap<u16, &str> = std::collections::HashMap::new();
        if matches!(self.mode, Mode::Subscribe | Mode::NodeSubscribe) {
            port_owner.insert(self.api_port(), "the REST API (apiPort)");
            if let Some(Ok(addr)) = self.http_listen.as_deref().map(str::parse::<std::net::SocketAddr>) {
                if addr.ip().is_loopback() || addr.ip().is_unspecified() {
                    port_owner.insert(addr.port(), "httpListen");
                }
            }
        }
        for name in self.dynamic_config.service_names() {
            if let Ok(Some(port)) = self.dynamic_config.local_port_for(name) {
                if let Some(other) = port_owner.get(&port) {
                    problems.push(format!("localPort {port} of '{name}' is already used by {other}"));
                }
            }
        }
        problems
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
    ///
    /// The file is replaced atomically (see [`write_atomic`]), so a crash, a
    /// kill or a full disk mid-write leaves the previous config in place
    /// rather than a truncated file the daemon can't start from.
    pub fn save(&self, path: &str) -> Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        write_atomic(std::path::Path::new(path), data.as_bytes())?;
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

/// `base` if there is no such file, else `base.2`, `base.3`, and so on:
/// where to keep a copy without writing over an earlier one (a second
/// `config pull` used to replace the `.bak` of the first).
pub(crate) fn unused_path(base: &str) -> String {
    if !std::path::Path::new(base).exists() {
        return base.to_string();
    }
    (2u32..).map(|n| format!("{base}.{n}")).find(|path| !std::path::Path::new(path).exists()).expect("some number is free")
}

/// Copies of `path` kept beside it with names starting `path.suffix` (see
/// [`unused_path`]), newest first.
pub(crate) fn kept_copies(path: &str, suffix: &str) -> Vec<String> {
    let path = std::path::Path::new(path);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let prefix = format!("{}.{suffix}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
    let mut copies: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| {
            let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            (modified, entry.path().to_string_lossy().into_owned())
        })
        .collect();
    copies.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    copies.into_iter().map(|(_, path)| path).collect()
}

/// Replace the contents of `path` with `data` atomically: write a temporary
/// file beside it, flush it to disk, then rename it over `path`. Readers and
/// a restarted daemon see the old contents or the new, never a partial file.
///
/// A symlinked `path` is followed, so the link survives and its target is
/// replaced. The file keeps its permissions and, where allowed, its owner,
/// so a daemon running as root doesn't take over a config a user edits.
///
/// Falls back to writing in place (not atomic, as before) where a rename
/// can't work: the directory isn't writable, or `path` is itself a mount
/// point, as with a single-file `docker run -v stouter.json:...` bind mount.
pub(crate) fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::ErrorKind;

    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let name = target.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    // A rename would replace even a file we may not write to; keep refusing
    // those, as a plain write would. (Opening without truncating changes
    // nothing.)
    if target.exists() {
        std::fs::OpenOptions::new().write(true).open(&target)?;
    }
    // Random, so concurrent writers (and a leftover from a crash) don't clash.
    let tmp = dir.join(format!(".{name}.{:016x}.tmp", rand::random::<u64>()));

    let file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
        Ok(file) => file,
        Err(_) => return std::fs::write(&target, data),
    };
    match write_then_rename(file, &tmp, &target, data) {
        Ok(()) => {
            // Make the rename itself durable; best effort (not on Windows).
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            match e.kind() {
                ErrorKind::ResourceBusy | ErrorKind::CrossesDevices | ErrorKind::PermissionDenied => {
                    std::fs::write(&target, data)
                }
                _ => Err(e),
            }
        }
    }
}

/// The steps of [`write_atomic`] after the temporary file is created.
fn write_then_rename(
    mut file: std::fs::File,
    tmp: &std::path::Path,
    target: &std::path::Path,
    data: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    if let Ok(meta) = std::fs::metadata(target) {
        file.set_permissions(meta.permissions())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Only root may give a file to someone else; otherwise it's ours anyway.
            let _ = std::os::unix::fs::fchown(&file, Some(meta.uid()), Some(meta.gid()));
        }
    }
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, target)
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

    /// The default config, for a node with a name (as every node has).
    fn named() -> Config {
        Config { node_id: "n1".into(), ..Config::default() }
    }

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
            ..named()
        };
        assert_eq!(cfg.peer_addr(), "10.0.0.1:9090");
    }

    #[test]
    fn peer_addr_uses_advertise_when_set() {
        let cfg = Config {
            bind: "0.0.0.0:8080".into(),
            advertise_addr: Some("1.2.3.4:9090".into()),
            ..named()
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
            ..named()
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
            ..named()
        };
        assert!(cfg.validate().is_err());
    }

    fn group_of(services: Vec<Service>) -> Config {
        Config {
            dynamic_config: DynamicConfig {
                version: 1,
                routes: vec![],
                service_groups: vec![ServiceGroup { name: "web".into(), services }],
                ..Default::default()
            },
            ..named()
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

    fn on_port(mut svc: Service, port: u16) -> Service {
        svc.local_port = Some(port);
        svc
    }

    #[test]
    fn local_port_resolves_across_instances() {
        let cfg = group_of(vec![
            on_port(instance("api", "n1", &[]), 15002),
            instance("api", "n2", &[]),
            on_port(instance("api", "n3", &[]), 15002),
        ]);
        assert_eq!(cfg.dynamic_config.local_port_for("api"), Ok(Some(15002)));
        assert_eq!(cfg.dynamic_config.local_port_for("other"), Ok(None));
        cfg.validate().unwrap();

        let cfg = group_of(vec![on_port(instance("api", "n1", &[]), 15002), on_port(instance("api", "n2", &[]), 15003)]);
        assert!(cfg.dynamic_config.local_port_for("api").is_err());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn local_ports_must_not_collide() {
        let two_services = group_of(vec![on_port(instance("api", "n1", &[]), 15002), on_port(instance("web", "n1", &[]), 15002)]);
        assert!(two_services.validate().is_err());

        let subscriber = |svc| Config { mode: Mode::Subscribe, ..group_of(vec![svc]) };
        let with_api = Config { api_port: Some(15002), ..subscriber(on_port(instance("api", "n1", &[]), 15002)) };
        assert!(with_api.validate().is_err());
        // The API's default port counts too, but only where the API runs.
        assert!(subscriber(on_port(instance("api", "n1", &[]), 5381)).validate().is_err());
        assert!(group_of(vec![on_port(instance("api", "n1", &[]), 5381)]).validate().is_ok());

        let with_listener = Config {
            mode: Mode::Subscribe,
            http_listen: Some("127.0.0.1:15002".into()),
            ..group_of(vec![on_port(instance("api", "n1", &[]), 15002)])
        };
        assert!(with_listener.validate().is_err());

        assert!(group_of(vec![on_port(instance("api", "n1", &[]), 0)]).validate().is_err());
    }

    #[test]
    fn http_listen_validation() {
        let ok = Config { mode: Mode::NodeSubscribe, http_listen: Some("127.0.0.1:15001".into()), ..named() };
        ok.validate().unwrap();
        let node_mode = Config { mode: Mode::Node, http_listen: Some("127.0.0.1:15001".into()), ..named() };
        assert!(node_mode.validate().is_err());
        let bad = Config { mode: Mode::Subscribe, http_listen: Some("localhost".into()), ..named() };
        assert!(bad.validate().is_err());
    }

    fn http_instance(name: &str, node: &str, labels: &[(&str, &str)]) -> Service {
        let mut svc = instance(name, node, &[]);
        svc.protocol = Protocol::Http;
        svc.labels = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        svc
    }

    fn route(json: serde_json::Value) -> Route {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn route_json_schema() {
        let r = route(serde_json::json!({
            "name": "canary",
            "service": "api",
            "match": {"pathPrefix": "/v2", "method": "GET", "headers": {"X-Canary": "true"}},
            "backends": [{"labels": {"version": "v2"}, "weight": 9}, {"labels": {"version": "v1"}}]
        }));
        assert_eq!(r.matches.path_prefix.as_deref(), Some("/v2"));
        assert_eq!(r.backends[1].weight, DEFAULT_WEIGHT);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"match\":{"), "{json}");
        assert_eq!(route(serde_json::from_str(&json).unwrap()), r);
        // Routes are omitted from config JSON when there are none.
        assert!(!serde_json::to_string(&DynamicConfig::default()).unwrap().contains("routes"));
    }

    #[test]
    fn path_prefix_matches_on_segment_boundaries() {
        assert!(path_has_prefix("/v2", "/v2"));
        assert!(path_has_prefix("/v2/users", "/v2"));
        assert!(!path_has_prefix("/v20", "/v2"));
        assert!(path_has_prefix("/v2/users", "/v2/"));
        assert!(!path_has_prefix("/v2", "/v2/"));
        assert!(path_has_prefix("/anything", "/"));
    }

    #[test]
    fn route_match_requires_all_conditions() {
        let m = RouteMatch {
            path_prefix: Some("/api".into()),
            method: Some("POST".into()),
            headers: [("X-Canary".to_string(), "true".to_string())].into(),
        };
        let canary = |name: &str| (name == "x-canary").then_some("true");
        let none = |_: &str| None;
        assert!(m.matches("POST", "/api/x", canary));
        assert!(!m.matches("GET", "/api/x", canary));
        assert!(!m.matches("POST", "/other", canary));
        assert!(!m.matches("POST", "/api/x", none));
        assert!(RouteMatch::default().matches("GET", "/", none));
    }

    #[test]
    fn route_validation_errors() {
        let ok = serde_json::json!({"service": "api", "backends": [{"labels": {"v": "1"}}]});
        assert!(validate_routes("api", &[route(ok.clone())]).is_ok());
        let cases = [
            serde_json::json!({"service": "web", "backends": [{}]}),
            serde_json::json!({"service": "api", "backends": []}),
            serde_json::json!({"service": "api", "backends": [{"weight": 0}]}),
            serde_json::json!({"service": "api", "match": {"pathPrefix": "v2"}, "backends": [{}]}),
            serde_json::json!({"service": "api", "match": {"method": "get"}, "backends": [{}]}),
            serde_json::json!({"service": "api", "match": {"headers": {"bad header": "x"}}, "backends": [{}]}),
        ];
        for case in cases {
            assert!(validate_routes("api", &[route(case.clone())]).is_err(), "{case}");
        }
    }

    #[test]
    fn set_routes_replaces_one_service_in_place() {
        let mut dc = DynamicConfig::default();
        for name in ["api", "web"] {
            dc.upsert_service("g", http_instance(name, "n1", &[]));
        }
        let r = |svc: &str, name: &str| route(serde_json::json!({"name": name, "service": svc, "backends": [{}]}));
        dc.set_routes("api", vec![r("api", "a1"), r("api", "a2")]).unwrap();
        dc.set_routes("web", vec![r("web", "w1")]).unwrap();
        dc.set_routes("api", vec![r("api", "a3")]).unwrap();
        let names: Vec<_> = dc.routes.iter().map(|r| r.name.clone().unwrap()).collect();
        assert_eq!(names, ["a3", "w1"]);
        dc.set_routes("api", vec![]).unwrap();
        assert_eq!(dc.routes.len(), 1);
    }

    #[test]
    fn routes_require_existing_http_service() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service("g", instance("tcp-svc", "n1", &[]));
        let r = |svc: &str| route(serde_json::json!({"service": svc, "backends": [{}]}));
        assert!(matches!(dc.set_routes("missing", vec![r("missing")]), Err(MutateErr::Invalid(_))));
        assert!(matches!(dc.set_routes("tcp-svc", vec![r("tcp-svc")]), Err(MutateErr::Invalid(_))));
        // Clearing never needs the service to exist.
        assert!(dc.set_routes("missing", vec![]).is_ok());

        let mut cfg = group_of(vec![http_instance("api", "n1", &[]), instance("api", "n2", &[])]);
        cfg.dynamic_config.routes = vec![r("api")];
        assert!(cfg.validate().is_err(), "a tcp instance must reject routes");
        cfg.dynamic_config.service_groups[0].services[1].protocol = Protocol::Http;
        cfg.validate().unwrap();
    }

    #[test]
    fn labels_match_requires_every_selector_pair() {
        let have: BTreeMap<String, String> = [("version", "v2"), ("zone", "a")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let want = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        assert!(labels_match(&have, &want(&[])));
        assert!(labels_match(&have, &want(&[("version", "v2")])));
        assert!(!labels_match(&have, &want(&[("version", "v1")])));
        assert!(!labels_match(&have, &want(&[("version", "v2"), ("tier", "x")])));
    }

    #[test]
    fn tracing_config_defaults_and_validation() {
        let tc: TracingConfig = serde_json::from_str(r#"{"otlpEndpoint": "http://127.0.0.1:4318"}"#).unwrap();
        assert_eq!(tc.sample_ratio, 1.0);
        assert_eq!(tc.service_name, "stouter");
        tc.validate().unwrap();
        for (endpoint, ratio) in [
            ("https://collector:4318", 1.0),
            ("127.0.0.1:4318", 1.0),
            ("not a url", 1.0),
            ("http://127.0.0.1:4318", 1.5),
            ("http://127.0.0.1:4318", -0.1),
        ] {
            let bad = TracingConfig { otlp_endpoint: endpoint.into(), sample_ratio: ratio, ..tc.clone() };
            assert!(bad.validate().is_err(), "{endpoint} {ratio}");
        }
        let cfg = Config { tracing: Some(TracingConfig { sample_ratio: 2.0, ..tc }), ..named() };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn service_meta_parses_and_is_omitted_when_empty() {
        let svc: Service = serde_json::from_str(
            r#"{"name":"wp-grpc","nodeId":"min1","nodePort":9000,"domains":["wp-grpc.pecka.cloud"],"meta":{"traefik.scheme":"h2c"}}"#,
        )
        .unwrap();
        assert_eq!(svc.meta["traefik.scheme"], "h2c");
        assert!(serde_json::to_string(&svc).unwrap().contains(r#""meta":{"traefik.scheme":"h2c"}"#));

        let plain: Service = serde_json::from_str(r#"{"name":"a","nodeId":"n1","nodePort":1}"#).unwrap();
        assert!(plain.meta.is_empty());
        assert!(!serde_json::to_string(&plain).unwrap().contains("meta"));
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
    fn the_fuller_copy_of_a_change_wins_the_tie_break() {
        // One change, and a copy relayed by a node that drops a field it
        // doesn't know (here localPort): same version, same author.
        let svc: Service =
            serde_json::from_value(serde_json::json!({"name": "api", "nodeId": "n1", "nodePort": 3000, "localPort": 15002}))
                .unwrap();
        let full = DynamicConfig {
            version: 5,
            changed_by: "n1".into(),
            service_groups: vec![ServiceGroup { name: "g".into(), services: vec![svc] }],
            ..Default::default()
        };
        let mut stripped = full.clone();
        stripped.service_groups[0].services[0].local_port = None;
        assert!(full.supersedes(&stripped));
        assert!(!stripped.supersedes(&full));

        // Likewise a signed config against a copy without its signature.
        let signed = DynamicConfig {
            signature: Some(ConfigSignature {
                config_json: serde_json::to_string(&full).unwrap(),
                signer_cert: "00".into(),
                signature: "00".into(),
            }),
            ..full.clone()
        };
        assert!(signed.supersedes(&full));
        assert!(!full.supersedes(&signed));
    }

    #[test]
    fn unknown_settings_are_reported_and_kept() {
        let text = r#"{"mode": "node", "bind": "127.0.0.1:1", "nodeId": "n1", "knownNodes": [],
            "requireSignedConfg": true, "dynamicConfig": {"version": 1, "serviceGroups": []}}"#;
        let cfg: Config = serde_json::from_str(text).unwrap();
        // The typo leaves signing off: say so, and fail `stouter check`.
        assert!(!cfg.require_signed_config);
        assert_eq!(cfg.unknown_settings().len(), 1);
        assert!(cfg.unknown_settings()[0].contains("requireSignedConfg"));
        assert!(cfg.validate().is_err());
        cfg.validate_local().unwrap();
        // And a rewrite of the file keeps it (it may be a newer version's).
        let saved = serde_json::to_value(&cfg).unwrap();
        assert_eq!(saved["requireSignedConfg"], serde_json::json!(true));
    }

    #[test]
    fn a_node_id_must_be_a_name() {
        for bad in ["", "10.0.0.5:8080", "my node", "node\t1"] {
            let cfg = Config { node_id: bad.into(), ..named() };
            assert!(cfg.validate_local().is_err(), "{bad:?}");
        }
        for good in ["n1", "host-2.example.com", "node_3"] {
            Config { node_id: good.into(), ..named() }.validate_local().unwrap();
        }
    }

    #[test]
    fn a_kept_copy_never_replaces_an_earlier_one() {
        let dir = std::env::temp_dir().join(format!("stouter_copies_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stouter.json").to_str().unwrap().to_string();
        let base = format!("{path}.bak");
        assert_eq!(unused_path(&base), base);
        std::fs::write(&base, "first").unwrap();
        assert_eq!(unused_path(&base), format!("{base}.2"));
        std::fs::write(format!("{base}.2"), "second").unwrap();
        assert_eq!(unused_path(&base), format!("{base}.3"));
        assert_eq!(kept_copies(&path, "bak").len(), 2);
        assert!(kept_copies(&path, "unapplied").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_file_that_doesnt_parse_is_never_replaced_by_defaults() {
        let dir = std::env::temp_dir().join(format!("stouter_load_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stouter.json");
        let path_str = path.to_str().unwrap();
        // No file yet: start from defaults (`init`, a first `service add`).
        assert_eq!(Config::load_or_default(path_str).unwrap().node_id, "");
        // A typo: an error, so nothing goes on to save over the file.
        let broken = r#"{"nodeId": "n1", "requireSignedConfig": true, "adminCns": ["admin"],}"#;
        std::fs::write(&path, broken).unwrap();
        let err = Config::load_or_default(path_str).expect_err("refused");
        assert!(format!("{err:#}").contains("invalid JSON"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_nodes_own_settings_stop_it_starting() {
        // A problem in the cluster's config: reported, but not the node's
        // to fix, so it starts.
        let dup = group_of(vec![instance("api", "n1", &[]), instance("api", "n1", &[])]);
        dup.validate_local().unwrap();
        assert_eq!(dup.config_problems().len(), 1, "{:?}", dup.config_problems());
        assert!(dup.validate().is_err(), "`stouter check` still reports it");

        // A fixed localPort on a port this daemon itself listens on: a
        // problem with the config here, so likewise.
        let mut taken = group_of(vec![on_port(instance("api", "n1", &[]), 5381)]);
        taken.mode = Mode::Subscribe;
        taken.validate_local().unwrap();
        assert!(taken.config_problems()[0].contains("apiPort"), "{:?}", taken.config_problems());

        // The node's own settings: refused.
        let mut pool = group_of(vec![]);
        pool.reverse_pool_size = Some(u16::MAX);
        assert!(pool.validate_local().is_err());
        let mut listen = group_of(vec![]);
        listen.mode = Mode::Subscribe;
        listen.http_listen = Some("not an address".into());
        assert!(listen.validate_local().is_err());
        listen.http_listen = Some(format!("127.0.0.1:{}", listen.api_port()));
        assert!(listen.validate_local().is_err(), "httpListen on the API's port");
        listen.http_listen = Some("127.0.0.1:15001".into());
        listen.validate_local().unwrap();
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
            ..named()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn nodesubscribe_mode_serde_round_trip() {
        let cfg = Config {
            mode: Mode::NodeSubscribe,
            ..named()
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
            ..named()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn outbound_only_accepted_on_node_subscribe_mode() {
        let cfg = Config {
            mode: Mode::NodeSubscribe,
            outbound_only: true,
            ..named()
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
                labels: Default::default(),
                meta: Default::default(),
                local_port: None,
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
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
        );
        let v = dc.upsert_service(
            "web",
            Service { name: "ui".into(), node_id: "n1".into(), node_port: 3001, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
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
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
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
                labels: [("version".to_string(), "v2".to_string())].into(),
                meta: [("traefik.scheme".to_string(), "h2c".to_string())].into(),
                local_port: None,
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
        assert_eq!(svc.labels["version"], "v2");
        assert_eq!(svc.meta["traefik.scheme"], "h2c");
    }

    #[test]
    fn upsert_service_keeps_same_name_on_different_nodes() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
        );
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n2".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
        );
        assert_eq!(dc.service_groups[0].services.len(), 2);
    }

    #[test]
    fn remove_service_success_bumps_version() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
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
            Service { name: "ui".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
        );
        let err = dc.remove_service("web", "n1", "api").unwrap_err();
        assert!(matches!(err, MutateErr::ServiceNotFound { .. }));
    }

    #[test]
    fn remove_service_leaves_other_services_alone() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
        );
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n2".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
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
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
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
            Service { name: "ui".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
        );
        let err = dc.rename_service("web", "n1", "api", "api2").unwrap_err();
        assert!(matches!(err, MutateErr::ServiceNotFound { .. }));
    }

    #[test]
    fn rename_service_conflicts_on_destination() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service(
            "web",
            Service { name: "api".into(), node_id: "n1".into(), node_port: 3000, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
        );
        dc.upsert_service(
            "web",
            Service { name: "api2".into(), node_id: "n1".into(), node_port: 3001, domains: vec![], weight: 1, max_connections: None, protocol: Protocol::Tcp, request_timeout_ms: None, health_path: None, labels: Default::default(), meta: Default::default(), local_port: None },
        );
        let err = dc.rename_service("web", "n1", "api", "api2").unwrap_err();
        assert!(matches!(err, MutateErr::DuplicateService { .. }));
    }

    #[test]
    fn admin_cns_round_trips_and_omitted_when_empty() {
        let cfg = Config::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("adminCns"));

        let cfg = Config { admin_cns: vec!["admin-cli".into()], ..named() };
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
        let cfg = Config { bind: "0.0.0.0:60001".into(), ..named() };
        assert_eq!(cfg.local_target(), "127.0.0.1:60001");
    }

    #[test]
    fn local_target_rewrites_wildcard_v6() {
        let cfg = Config { bind: "[::]:60001".into(), ..named() };
        assert_eq!(cfg.local_target(), "[::1]:60001");
    }

    #[test]
    fn local_target_preserves_specific_address() {
        let cfg = Config { bind: "10.0.0.5:60001".into(), ..named() };
        assert_eq!(cfg.local_target(), "10.0.0.5:60001");
    }

    #[test]
    fn local_target_preserves_unparseable_bind() {
        let cfg = Config { bind: "myhost:60001".into(), ..named() };
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
                routes: vec![],
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
                        labels: Default::default(),
                        meta: [("traefik.scheme".to_string(), "h2c".to_string())].into(),
                        local_port: None,
                    }],
                }],
                ..Default::default()
            },
            ..named()
        };
        cfg.save(path_str).unwrap();
        let loaded = Config::load(path_str).unwrap();
        assert_eq!(loaded.dynamic_config.version, 5);
        assert_eq!(loaded.dynamic_config.service_groups[0].services[0].name, "api");
        assert_eq!(loaded.dynamic_config.service_groups[0].services[0].meta["traefik.scheme"], "h2c");

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn save_replaces_through_a_symlink_keeping_mode_and_leaving_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("stouter_save_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.json");
        let link = dir.join("stouter.json");
        Config { node_id: "before".into(), ..named() }.save(real.to_str().unwrap()).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        Config { node_id: "after".into(), ..named() }.save(link.to_str().unwrap()).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "link kept");
        assert_eq!(Config::load(real.to_str().unwrap()).unwrap().node_id, "after");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        let mut names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["real.json", "stouter.json"], "no temp file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("stouter_new_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stouter.json");
        Config { node_id: "n1".into(), ..named() }.save(path.to_str().unwrap()).unwrap();
        assert_eq!(Config::load(path.to_str().unwrap()).unwrap().node_id, "n1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn equal_versions_resolve_to_the_same_winner_everywhere() {
        let mut a = DynamicConfig::default();
        a.upsert_service("g", instance("from-a", "n1", &[]));
        a.changed_by = "node-a".into();
        let mut b = DynamicConfig::default();
        b.upsert_service("g", instance("from-b", "n1", &[]));
        b.changed_by = "node-b".into();
        assert_eq!(a.version, b.version);
        // Exactly one wins, whichever a node holds first.
        assert!(b.supersedes(&a) && !a.supersedes(&b));
        assert!(!a.supersedes(&a.clone()), "identical configs never replace each other");
        // A higher version always wins.
        let mut older_b = b.clone();
        older_b.version -= 1;
        assert!(a.supersedes(&older_b));
        // The copy an older node relays (no changedBy) never beats the
        // newer node's original.
        let stripped = DynamicConfig { changed_by: String::new(), ..b.clone() };
        assert!(!stripped.supersedes(&b) && b.supersedes(&stripped));
    }

    #[test]
    fn a_service_may_not_span_two_groups() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service("web", instance("api", "n1", &[]));
        dc.upsert_service("ops", instance("api", "n2", &[]));
        let problems = dc.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("both group 'web' and group 'ops'"), "{problems:?}");
    }

    #[test]
    fn changes_are_refused_only_for_problems_they_introduce() {
        let mut before = DynamicConfig::default();
        before.upsert_service("g", http_instance("api", "n1", &[]));
        before.set_routes("api", vec![route(serde_json::json!({"service": "api", "backends": [{}]}))]).unwrap();
        assert!(before.problems().is_empty());

        // Removing the only instance of a routed service leaves routes behind.
        let mut after = before.clone();
        after.remove_service("g", "n1", "api").unwrap();
        assert!(new_problem(&before, &after).unwrap().contains("unknown service 'api'"));

        // So does making it TCP.
        let mut after = before.clone();
        after.upsert_service("g", instance("api", "n1", &[]));
        assert!(new_problem(&before, &after).unwrap().contains("protocol: http"));

        // Claiming another service's domain.
        let mut with_domain = before.clone();
        with_domain.upsert_service("g", instance("web", "n1", &["x.example.com"]));
        let mut after = with_domain.clone();
        after.upsert_service("g", instance("other", "n2", &["x.example.com"]));
        assert!(new_problem(&with_domain, &after).unwrap().contains("x.example.com"));

        // A config that's already broken still accepts unrelated changes.
        let broken = after.clone();
        let mut fixed_elsewhere = broken.clone();
        fixed_elsewhere.upsert_service("g", instance("unrelated", "n3", &[]));
        assert_eq!(new_problem(&broken, &fixed_elsewhere), None);
    }

    #[test]
    fn signed_config_requires_an_admin() {
        let cfg = Config { require_signed_config: true, ..named() };
        assert!(cfg.validate().unwrap_err().to_string().contains("adminCns"));
        let cfg = Config { admin_cns: vec!["admin".into()], ..cfg };
        cfg.validate().unwrap();
        // Off by default, and omitted from JSON unless on.
        assert!(!serde_json::to_string(&Config::default()).unwrap().contains("requireSignedConfig"));
    }

    #[test]
    fn configs_too_large_to_gossip_and_oversized_pools_are_refused() {
        let mut dc = DynamicConfig::default();
        dc.upsert_service("g", instance("api", "n1", &[]));
        assert!(dc.problems().is_empty());
        let mut huge = dc.clone();
        huge.service_groups[0].services[0].meta.insert("blob".into(), "x".repeat(MAX_CONFIG_BYTES));
        let problem = new_problem(&dc, &huge).expect("refused");
        assert!(problem.contains("too large to gossip"), "{problem}");

        let cfg = Config { reverse_pool_size: Some(5000), ..named() };
        assert!(cfg.validate().unwrap_err().to_string().contains("reversePoolSize"));
    }
}
