//! Regions and the node registry — the basis for a multi-node "unified cloud".
//! Each node has a region (e.g. `sfo1`) and an iroh endpoint id; nodes learn
//! about peers and can route to the nearest region.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// A node in the cloud (this machine or a peer MacBook).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    pub region: String,
    /// Public gateway URL (for same-LAN / direct addressing).
    pub public_url: String,
    /// iroh endpoint id, for P2P reachability across networks.
    pub peer_id: Option<String>,
    pub last_seen_ms: u64,
    #[serde(default)]
    pub is_self: bool,
    /// Measured round-trip latency to this node (ms); 0 for self.
    #[serde(default)]
    pub latency_ms: u64,
    /// Health (responding to probes). Anycast skips unhealthy nodes.
    #[serde(default = "crate::default_true_pub")]
    pub healthy: bool,
}

pub struct NodeRegistry {
    me: NodeInfo,
    peers: RwLock<HashMap<String, NodeInfo>>,
}

impl NodeRegistry {
    pub fn new(me: NodeInfo) -> Arc<NodeRegistry> {
        Arc::new(NodeRegistry {
            me,
            peers: RwLock::new(HashMap::new()),
        })
    }

    pub fn me(&self) -> NodeInfo {
        let mut me = self.me.clone();
        me.is_self = true;
        me.last_seen_ms = now_ms();
        me.latency_ms = 0;
        me.healthy = true;
        me
    }

    /// Record a peer's measured latency + health (from probing).
    pub fn set_health(&self, id: &str, latency_ms: u64, healthy: bool) {
        if let Some(p) = self.peers.write().get_mut(id) {
            p.latency_ms = latency_ms;
            p.healthy = healthy;
            if healthy {
                p.last_seen_ms = now_ms();
            }
        }
    }

    /// Anycast selection: the optimal node to serve a request — the lowest-latency
    /// healthy node, preferring `preferred` region when given (automatic failover
    /// falls through to the next-best healthy node).
    pub fn anycast(&self, preferred: Option<&str>) -> Option<NodeInfo> {
        let mut healthy: Vec<NodeInfo> = self.nodes().into_iter().filter(|n| n.healthy).collect();
        healthy.sort_by_key(|n| n.latency_ms);
        if let Some(region) = preferred {
            if let Some(n) = healthy.iter().find(|n| n.region == region) {
                return Some(n.clone());
            }
        }
        healthy.into_iter().next()
    }

    /// The full anycast routing table (healthy first, by latency).
    pub fn routing_table(&self) -> Vec<NodeInfo> {
        let mut nodes = self.nodes();
        nodes.sort_by(|a, b| b.healthy.cmp(&a.healthy).then(a.latency_ms.cmp(&b.latency_ms)));
        nodes
    }

    pub fn region(&self) -> &str {
        &self.me.region
    }

    /// Record/refresh a peer.
    pub fn upsert_peer(&self, mut peer: NodeInfo) {
        peer.last_seen_ms = now_ms();
        peer.is_self = false;
        self.peers.write().insert(peer.id.clone(), peer);
    }

    /// All nodes (self first), with stale peers (>30s) dropped.
    pub fn nodes(&self) -> Vec<NodeInfo> {
        let now = now_ms();
        let mut out = vec![self.me()];
        let mut peers: Vec<NodeInfo> = self
            .peers
            .read()
            .values()
            .filter(|p| now.saturating_sub(p.last_seen_ms) < 30_000)
            .cloned()
            .collect();
        peers.sort_by(|a, b| a.region.cmp(&b.region).then(a.name.cmp(&b.name)));
        out.extend(peers);
        out
    }

    /// Distinct regions across the cloud.
    pub fn regions(&self) -> Vec<String> {
        let mut rs: Vec<String> = self.nodes().into_iter().map(|n| n.region).collect();
        rs.sort();
        rs.dedup();
        rs
    }

    /// Pick a node to serve a request, preferring this node's region, then any.
    pub fn pick_for_region(&self, preferred: Option<&str>) -> Option<NodeInfo> {
        let nodes = self.nodes();
        if let Some(region) = preferred {
            if let Some(n) = nodes.iter().find(|n| n.region == region) {
                return Some(n.clone());
            }
        }
        nodes.into_iter().next()
    }
}
