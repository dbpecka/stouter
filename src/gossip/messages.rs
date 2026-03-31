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
