use serde::{Deserialize, Serialize};

use crate::config::{DynamicConfig, NodeInfo};

/// All message types exchanged between cluster members over TCP.
///
/// Every TCP connection starts with a single `Message` sent via the binary
/// message framing (4-byte length prefix + 32-byte HMAC + MessagePack payload).
/// The variant of that first message determines how the connection is handled.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Message {
    /// Full state synchronisation: send our current config and known peer list.
    Sync {
        config: DynamicConfig,
        nodes: Vec<NodeInfo>,
    },
    /// Announce an updated dynamic configuration.
    ConfigUpdate { config: DynamicConfig },
    /// Announce that a new node has joined the cluster.
    NodeJoin { id: String, addr: String },
    /// Announce that a node has left the cluster.
    NodeLeave { id: String },
    /// Request to tunnel traffic to a service on a specific node.
    TunnelRequest {
        node_id: String,
        service_name: String,
        timestamp_ms: u64,
    },
    /// NAT'd node registering a reverse connection with a relay.
    ReverseRegistration { node_id: String, timestamp_ms: u64 },
    /// Request daemon status.
    StatusRequest,
    /// Daemon status response.
    StatusResponse {
        mode: String,
        node_id: String,
        bind: String,
        dynamic_config: DynamicConfig,
        known_nodes: Vec<NodeInfo>,
    },
    /// Upgrade this TCP connection to a yamux multiplexed tunnel session.
    MuxTunnel {},
    /// Mutating admin RPC. Authorized via `adminCns` allowlist on the receiver.
    AdminRequest { op: AdminOp },
    /// Reply to an `AdminRequest`.
    AdminResponse { result: AdminResult },
}

/// A single admin operation. Extend this enum to add new mutating commands
/// without changing the connection-dispatch table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AdminOp {
    ServiceAdd {
        group: String,
        name: String,
        owner_node_id: String,
        port: u16,
        #[serde(default)]
        domains: Vec<String>,
        #[serde(default = "crate::config::default_weight")]
        weight: u32,
        #[serde(default)]
        max_connections: Option<u32>,
        #[serde(default)]
        protocol: crate::config::Protocol,
        #[serde(default)]
        request_timeout_ms: Option<u64>,
        #[serde(default)]
        health_path: Option<String>,
        #[serde(default)]
        labels: std::collections::BTreeMap<String, String>,
    },
    /// Replace every route for `service` (an empty list clears them).
    RoutesSet {
        service: String,
        routes: Vec<crate::config::Route>,
    },
    ServiceRename {
        group: String,
        owner_node_id: String,
        from: String,
        to: String,
    },
    ServiceRemove {
        group: String,
        owner_node_id: String,
        name: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AdminResult {
    Ok { new_version: u64 },
    Err { code: AdminErrCode, message: String },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AdminErrCode {
    Unauthorized,
    Validation,
    NotFound,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DynamicConfig, NodeInfo, Service, ServiceGroup};

    /// Helper: round-trip a Message through the gossip wire encoding.
    fn msgpack_round_trip(msg: &Message) -> Message {
        let bytes = crate::gossip::encode_message(msg).expect("serialize");
        rmp_serde::from_slice(&bytes).expect("deserialize")
    }

    /// `Service` as it was before `weight` existed, for wire-compat tests.
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OldService {
        name: String,
        node_id: String,
        node_port: u16,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        domains: Vec<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OldServiceGroup {
        name: String,
        services: Vec<OldService>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OldDynamicConfig {
        version: u64,
        service_groups: Vec<OldServiceGroup>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "camelCase")]
    enum OldMessage {
        ConfigUpdate { config: OldDynamicConfig },
    }

    fn weighted_config_update() -> Message {
        Message::ConfigUpdate {
            config: DynamicConfig {
                version: 7,
                routes: vec![],
                service_groups: vec![ServiceGroup {
                    name: "web".into(),
                    services: vec![Service {
                        name: "api".into(),
                        node_id: "host1".into(),
                        node_port: 3000,
                        domains: Vec::new(),
                        weight: 4,
                        max_connections: None,
                        protocol: crate::config::Protocol::Tcp,
                        request_timeout_ms: None,
                        health_path: None,
                        labels: Default::default(),
                    }],
                }],
            },
        }
    }

    #[test]
    fn weight_survives_round_trip_with_empty_domains() {
        match msgpack_round_trip(&weighted_config_update()) {
            Message::ConfigUpdate { config } => {
                let svc = &config.service_groups[0].services[0];
                assert!(svc.domains.is_empty());
                assert_eq!(svc.weight, 4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn old_reader_accepts_new_message() {
        let bytes = crate::gossip::encode_message(&weighted_config_update()).unwrap();
        let OldMessage::ConfigUpdate { config } =
            rmp_serde::from_slice(&bytes).expect("old reader decodes new message");
        assert_eq!(config.version, 7);
        assert_eq!(config.service_groups[0].services[0].node_port, 3000);
    }

    #[test]
    fn new_reader_accepts_old_positional_message() {
        // Old senders used positional (array) struct encoding.
        let old = OldMessage::ConfigUpdate {
            config: OldDynamicConfig {
                version: 3,
                service_groups: vec![OldServiceGroup {
                    name: "web".into(),
                    services: vec![OldService {
                        name: "api".into(),
                        node_id: "host1".into(),
                        node_port: 3000,
                        domains: Vec::new(),
                    }],
                }],
            },
        };
        let bytes = rmp_serde::to_vec(&old).unwrap();
        match rmp_serde::from_slice::<Message>(&bytes).expect("new reader decodes old message") {
            Message::ConfigUpdate { config } => {
                let svc = &config.service_groups[0].services[0];
                assert_eq!(svc.node_port, 3000);
                assert_eq!(svc.weight, crate::config::DEFAULT_WEIGHT);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn positional_encoding_breaks_old_readers() {
        // Documents why `encode_message` uses named encoding: a positional
        // `Service` with an extra trailing field is rejected by old readers.
        let bytes = rmp_serde::to_vec(&weighted_config_update()).unwrap();
        assert!(rmp_serde::from_slice::<OldMessage>(&bytes).is_err());
    }

    #[test]
    fn node_join_round_trip() {
        let msg = Message::NodeJoin {
            id: "n1".into(),
            addr: "1.2.3.4:8080".into(),
        };
        match msgpack_round_trip(&msg) {
            Message::NodeJoin { id, addr } => {
                assert_eq!(id, "n1");
                assert_eq!(addr, "1.2.3.4:8080");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn node_leave_round_trip() {
        let msg = Message::NodeLeave { id: "n1".into() };
        match msgpack_round_trip(&msg) {
            Message::NodeLeave { id } => assert_eq!(id, "n1"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn config_update_round_trip() {
        let msg = Message::ConfigUpdate {
            config: DynamicConfig {
                version: 42,
                routes: vec![],
                service_groups: vec![ServiceGroup {
                    name: "web".into(),
                    services: vec![Service {
                        name: "api".into(),
                        node_id: "host1".into(),
                        node_port: 3000,
                        domains: Vec::new(),
                        weight: 1,
                        max_connections: None,
                        protocol: crate::config::Protocol::Tcp,
                        request_timeout_ms: None,
                        health_path: None,
                        labels: Default::default(),
                    }],
                }],
            },
        };
        match msgpack_round_trip(&msg) {
            Message::ConfigUpdate { config } => {
                assert_eq!(config.version, 42);
                assert_eq!(config.service_groups[0].services[0].name, "api");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn sync_round_trip() {
        let msg = Message::Sync {
            config: DynamicConfig { version: 1, service_groups: vec![], routes: vec![] },
            nodes: vec![NodeInfo {
                id: "n1".into(),
                addr: "10.0.0.1:8080".into(),
                relay: None,
                last_seen: None,
                health: crate::config::ServiceHealth {
                    version: 9,
                    unhealthy: vec!["api".into()],
                },
            }],
        };
        match msgpack_round_trip(&msg) {
            Message::Sync { config, nodes } => {
                assert_eq!(config.version, 1);
                assert_eq!(nodes.len(), 1);
                assert_eq!(nodes[0].id, "n1");
                assert_eq!(nodes[0].health.version, 9);
                assert_eq!(nodes[0].health.unhealthy, ["api"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn health_omitted_from_wire_until_reported() {
        let node = NodeInfo {
            id: "n1".into(),
            addr: String::new(),
            relay: None,
            last_seen: None,
            health: Default::default(),
        };
        assert!(!serde_json::to_string(&node).unwrap().contains("health"));
        // A node that recovered still reports, so peers clear the old entry.
        let node = NodeInfo {
            health: crate::config::ServiceHealth { version: 2, unhealthy: vec![] },
            ..node
        };
        assert!(serde_json::to_string(&node).unwrap().contains("\"health\":{\"version\":2}"));
    }

    #[test]
    fn tunnel_request_round_trip() {
        let msg = Message::TunnelRequest {
            node_id: "host1".into(),
            service_name: "web".into(),
            timestamp_ms: 1234567890,
        };
        match msgpack_round_trip(&msg) {
            Message::TunnelRequest { node_id, service_name, timestamp_ms } => {
                assert_eq!(node_id, "host1");
                assert_eq!(service_name, "web");
                assert_eq!(timestamp_ms, 1234567890);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn reverse_registration_round_trip() {
        let msg = Message::ReverseRegistration {
            node_id: "nat-host".into(),
            timestamp_ms: 1234567890,
        };
        match msgpack_round_trip(&msg) {
            Message::ReverseRegistration { node_id, timestamp_ms } => {
                assert_eq!(node_id, "nat-host");
                assert_eq!(timestamp_ms, 1234567890);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn status_request_round_trip() {
        let msg = Message::StatusRequest;
        assert!(matches!(msgpack_round_trip(&msg), Message::StatusRequest));
    }

    #[test]
    fn status_response_round_trip() {
        let msg = Message::StatusResponse {
            mode: "node".into(),
            node_id: "host1".into(),
            bind: "0.0.0.0:8080".into(),
            dynamic_config: DynamicConfig { version: 1, service_groups: vec![], routes: vec![] },
            known_nodes: vec![],
        };
        match msgpack_round_trip(&msg) {
            Message::StatusResponse { mode, node_id, .. } => {
                assert_eq!(mode, "node");
                assert_eq!(node_id, "host1");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn mux_tunnel_round_trip() {
        let msg = Message::MuxTunnel {};
        assert!(matches!(msgpack_round_trip(&msg), Message::MuxTunnel {}));
    }

    #[test]
    fn admin_request_service_add_round_trip() {
        let msg = Message::AdminRequest {
            op: AdminOp::ServiceAdd {
                group: "web".into(),
                name: "api".into(),
                owner_node_id: "n1".into(),
                port: 3000,
                domains: vec!["api.example.com".into()],
                weight: 5,
                max_connections: Some(50),
                protocol: crate::config::Protocol::Http,
                request_timeout_ms: Some(1500),
                health_path: Some("/healthz".into()),
                labels: [("version".to_string(), "v2".to_string())].into(),
            },
        };
        match msgpack_round_trip(&msg) {
            Message::AdminRequest {
                op:
                    AdminOp::ServiceAdd {
                        group,
                        name,
                        owner_node_id,
                        port,
                        domains,
                        weight,
                        max_connections,
                        protocol,
                        request_timeout_ms,
                        health_path,
                        labels,
                    },
            } => {
                assert_eq!(group, "web");
                assert_eq!(name, "api");
                assert_eq!(owner_node_id, "n1");
                assert_eq!(port, 3000);
                assert_eq!(domains, vec!["api.example.com"]);
                assert_eq!(weight, 5);
                assert_eq!(max_connections, Some(50));
                assert_eq!(protocol, crate::config::Protocol::Http);
                assert_eq!(request_timeout_ms, Some(1500));
                assert_eq!(health_path.as_deref(), Some("/healthz"));
                assert_eq!(labels["version"], "v2");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn admin_request_service_rename_round_trip() {
        let msg = Message::AdminRequest {
            op: AdminOp::ServiceRename {
                group: "web".into(),
                owner_node_id: "n1".into(),
                from: "api".into(),
                to: "api2".into(),
            },
        };
        match msgpack_round_trip(&msg) {
            Message::AdminRequest {
                op: AdminOp::ServiceRename { group, owner_node_id, from, to },
            } => {
                assert_eq!(group, "web");
                assert_eq!(owner_node_id, "n1");
                assert_eq!(from, "api");
                assert_eq!(to, "api2");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn admin_request_service_remove_round_trip() {
        let msg = Message::AdminRequest {
            op: AdminOp::ServiceRemove {
                group: "web".into(),
                owner_node_id: "n1".into(),
                name: "api".into(),
            },
        };
        match msgpack_round_trip(&msg) {
            Message::AdminRequest {
                op: AdminOp::ServiceRemove { group, owner_node_id, name },
            } => {
                assert_eq!(group, "web");
                assert_eq!(owner_node_id, "n1");
                assert_eq!(name, "api");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn admin_response_ok_round_trip() {
        let msg = Message::AdminResponse {
            result: AdminResult::Ok { new_version: 42 },
        };
        match msgpack_round_trip(&msg) {
            Message::AdminResponse { result: AdminResult::Ok { new_version } } => {
                assert_eq!(new_version, 42);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn admin_response_err_round_trip() {
        let msg = Message::AdminResponse {
            result: AdminResult::Err {
                code: AdminErrCode::Unauthorized,
                message: "nope".into(),
            },
        };
        match msgpack_round_trip(&msg) {
            Message::AdminResponse {
                result: AdminResult::Err { code, message },
            } => {
                assert_eq!(code, AdminErrCode::Unauthorized);
                assert_eq!(message, "nope");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
