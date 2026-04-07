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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DynamicConfig, NodeInfo, Service, ServiceGroup};

    /// Helper: round-trip a Message through MessagePack serialization.
    fn msgpack_round_trip(msg: &Message) -> Message {
        let bytes = rmp_serde::to_vec(msg).expect("serialize");
        rmp_serde::from_slice(&bytes).expect("deserialize")
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
                service_groups: vec![ServiceGroup {
                    name: "web".into(),
                    services: vec![Service {
                        name: "api".into(),
                        node_id: "host1".into(),
                        node_port: 3000,
                        domains: Vec::new(),
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
            config: DynamicConfig { version: 1, service_groups: vec![] },
            nodes: vec![NodeInfo { id: "n1".into(), addr: "10.0.0.1:8080".into(), relay: None }],
        };
        match msgpack_round_trip(&msg) {
            Message::Sync { config, nodes } => {
                assert_eq!(config.version, 1);
                assert_eq!(nodes.len(), 1);
                assert_eq!(nodes[0].id, "n1");
            }
            _ => panic!("wrong variant"),
        }
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
            dynamic_config: DynamicConfig { version: 1, service_groups: vec![] },
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
}
