use serde::{Deserialize, Serialize};

use crate::config::{DynamicConfig, NodeInfo};

/// A gossip message with its HMAC-SHA256 signature.
///
/// The `payload` field contains the JSON serialization of a [`GossipMessage`].
/// The `signature` field contains the hex-encoded HMAC-SHA256 of `payload` keyed
/// with the cluster secret.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedMessage {
    /// Raw JSON encoding of the inner [`GossipMessage`].
    pub payload: String,
    /// Hex-encoded HMAC-SHA256(cluster_secret, payload).
    pub signature: String,
}

/// Typed gossip messages exchanged between cluster members.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum GossipMessage {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DynamicConfig, NodeInfo, Service, ServiceGroup};

    #[test]
    fn node_join_serde_round_trip() {
        let msg = GossipMessage::NodeJoin {
            id: "n1".into(),
            addr: "1.2.3.4:8080".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"nodeJoin""#));

        let restored: GossipMessage = serde_json::from_str(&json).unwrap();
        match restored {
            GossipMessage::NodeJoin { id, addr } => {
                assert_eq!(id, "n1");
                assert_eq!(addr, "1.2.3.4:8080");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn node_leave_serde_round_trip() {
        let msg = GossipMessage::NodeLeave { id: "n1".into() };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"nodeLeave""#));

        let restored: GossipMessage = serde_json::from_str(&json).unwrap();
        match restored {
            GossipMessage::NodeLeave { id } => assert_eq!(id, "n1"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn config_update_serde_round_trip() {
        let msg = GossipMessage::ConfigUpdate {
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
        let restored: GossipMessage = serde_json::from_str(&json).unwrap();
        match restored {
            GossipMessage::ConfigUpdate { config } => {
                assert_eq!(config.version, 42);
                assert_eq!(config.service_groups[0].services[0].name, "api");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn sync_serde_round_trip() {
        let msg = GossipMessage::Sync {
            config: DynamicConfig { version: 1, service_groups: vec![] },
            nodes: vec![NodeInfo { id: "n1".into(), addr: "10.0.0.1:8080".into(), relay: None }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: GossipMessage = serde_json::from_str(&json).unwrap();
        match restored {
            GossipMessage::Sync { config, nodes } => {
                assert_eq!(config.version, 1);
                assert_eq!(nodes.len(), 1);
                assert_eq!(nodes[0].id, "n1");
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
