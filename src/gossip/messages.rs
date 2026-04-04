use serde::{Deserialize, Serialize};

use crate::config::{DynamicConfig, NodeInfo};

/// A message with its HMAC-SHA256 signature.
///
/// The `payload` field contains the JSON serialization of a [`Message`].
/// The `signature` field contains the hex-encoded HMAC-SHA256 of `payload` keyed
/// with the cluster secret.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedMessage {
    /// Raw JSON encoding of the inner [`Message`].
    pub payload: String,
    /// Hex-encoded HMAC-SHA256(cluster_secret, payload).
    pub signature: String,
}

/// All message types exchanged between cluster members over TCP.
///
/// Every TCP connection starts with a single `Message` sent via the signed
/// message framing (4-byte length prefix + JSON [`SignedMessage`]).  The
/// variant of that first message determines how the connection is handled.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DynamicConfig, NodeInfo, Service, ServiceGroup};

    #[test]
    fn node_join_serde_round_trip() {
        let msg = Message::NodeJoin {
            id: "n1".into(),
            addr: "1.2.3.4:8080".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"nodeJoin""#));

        let restored: Message = serde_json::from_str(&json).unwrap();
        match restored {
            Message::NodeJoin { id, addr } => {
                assert_eq!(id, "n1");
                assert_eq!(addr, "1.2.3.4:8080");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn node_leave_serde_round_trip() {
        let msg = Message::NodeLeave { id: "n1".into() };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"nodeLeave""#));

        let restored: Message = serde_json::from_str(&json).unwrap();
        match restored {
            Message::NodeLeave { id } => assert_eq!(id, "n1"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn config_update_serde_round_trip() {
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
        let json = serde_json::to_string(&msg).unwrap();
        let restored: Message = serde_json::from_str(&json).unwrap();
        match restored {
            Message::ConfigUpdate { config } => {
                assert_eq!(config.version, 42);
                assert_eq!(config.service_groups[0].services[0].name, "api");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn sync_serde_round_trip() {
        let msg = Message::Sync {
            config: DynamicConfig { version: 1, service_groups: vec![] },
            nodes: vec![NodeInfo { id: "n1".into(), addr: "10.0.0.1:8080".into(), relay: None }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: Message = serde_json::from_str(&json).unwrap();
        match restored {
            Message::Sync { config, nodes } => {
                assert_eq!(config.version, 1);
                assert_eq!(nodes.len(), 1);
                assert_eq!(nodes[0].id, "n1");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tunnel_request_serde_round_trip() {
        let msg = Message::TunnelRequest {
            node_id: "host1".into(),
            service_name: "web".into(),
            timestamp_ms: 1234567890,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"tunnelRequest""#));
        let restored: Message = serde_json::from_str(&json).unwrap();
        match restored {
            Message::TunnelRequest { node_id, service_name, timestamp_ms } => {
                assert_eq!(node_id, "host1");
                assert_eq!(service_name, "web");
                assert_eq!(timestamp_ms, 1234567890);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn reverse_registration_serde_round_trip() {
        let msg = Message::ReverseRegistration {
            node_id: "nat-host".into(),
            timestamp_ms: 1234567890,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"reverseRegistration""#));
        let restored: Message = serde_json::from_str(&json).unwrap();
        match restored {
            Message::ReverseRegistration { node_id, timestamp_ms } => {
                assert_eq!(node_id, "nat-host");
                assert_eq!(timestamp_ms, 1234567890);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn status_request_serde_round_trip() {
        let msg = Message::StatusRequest;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"statusRequest""#));
        let restored: Message = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored, Message::StatusRequest));
    }

    #[test]
    fn status_response_serde_round_trip() {
        let msg = Message::StatusResponse {
            mode: "node".into(),
            node_id: "host1".into(),
            bind: "0.0.0.0:8080".into(),
            dynamic_config: DynamicConfig { version: 1, service_groups: vec![] },
            known_nodes: vec![],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: Message = serde_json::from_str(&json).unwrap();
        match restored {
            Message::StatusResponse { mode, node_id, .. } => {
                assert_eq!(mode, "node");
                assert_eq!(node_id, "host1");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn signed_message_serde_round_trip() {
        let sm = SignedMessage {
            payload: r#"{"type":"nodeLeave","id":"x"}"#.into(),
            signature: "abc123def456".into(),
        };
        let json = serde_json::to_string(&sm).unwrap();
        let restored: SignedMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.payload, sm.payload);
        assert_eq!(restored.signature, sm.signature);
    }
}
