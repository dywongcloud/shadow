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
    /// Auto-detected geographic location (from IP geolocation at startup), so the
    /// node reports its real-world position for the regions map + region picker.
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, region: &str, latency: u64, healthy: bool) -> NodeInfo {
        NodeInfo {
            id: id.into(),
            name: id.into(),
            region: region.into(),
            public_url: format!("http://{id}:8787"),
            peer_id: None,
            last_seen_ms: now_ms(),
            is_self: false,
            latency_ms: latency,
            healthy,
            lat: None,
            lon: None,
            city: None,
            country: None,
        }
    }

    #[test]
    fn registry_tracks_self_and_peers() {
        let reg = NodeRegistry::new(node("self", "iad1", 0, true));
        assert_eq!(reg.nodes().len(), 1);
        reg.upsert_peer(node("peer-sfo", "sfo1", 20, true));
        reg.upsert_peer(node("peer-fra", "fra1", 80, true));
        assert_eq!(reg.nodes().len(), 3);
        assert_eq!(reg.regions(), vec!["fra1", "iad1", "sfo1"]);
    }

    #[test]
    fn anycast_prefers_region_then_lowest_latency() {
        let reg = NodeRegistry::new(node("self", "iad1", 0, true));
        reg.upsert_peer(node("sfo", "sfo1", 10, true));
        reg.upsert_peer(node("fra", "fra1", 5, true));
        // Preferred region wins even if not the lowest latency.
        assert_eq!(reg.anycast(Some("sfo1")).unwrap().region, "sfo1");
        // No preference → lowest latency healthy node (self at 0ms).
        assert_eq!(reg.anycast(None).unwrap().id, "self");
    }

    #[test]
    fn anycast_fails_over_past_unhealthy_nodes() {
        let reg = NodeRegistry::new(node("self", "iad1", 0, true));
        reg.upsert_peer(node("sfo", "sfo1", 10, true));
        // Mark the preferred region's node unhealthy → anycast skips it.
        reg.set_health("sfo", 10, false);
        let pick = reg.anycast(Some("sfo1")).unwrap();
        assert!(pick.healthy);
        assert_ne!(pick.id, "sfo");
    }
}
