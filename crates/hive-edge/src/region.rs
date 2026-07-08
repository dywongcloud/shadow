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
    /// Reachable PUBLIC IPv4 — the address a *browser* can hit over HTTPS. Used by
    /// the client-facing authoritative DNS (Seer) for A records. `None` for NAT'd
    /// nodes (no inbound-reachable address) → they're excluded from client DNS.
    /// Set from `HIVE_PUBLIC_IP`; NEVER the `--listen` bind addr and never 0.0.0.0.
    #[serde(default)]
    pub public_ip: Option<String>,
    /// Reachable PUBLIC IPv6 (Seer AAAA records). `None` if the node has none.
    /// Set from `HIVE_PUBLIC_IP6`.
    #[serde(default)]
    pub public_ip6: Option<String>,
    /// iroh endpoint id, for P2P reachability across networks.
    pub peer_id: Option<String>,
    /// iroh dialable address (JSON: direct socket addrs + relay), so peers can
    /// open a QUIC tunnel to this node directly. Populated when P2P is bound.
    #[serde(default)]
    pub iroh_addr: Option<String>,
    /// GuardianDB's OWN dialable iroh address (JSON `EndpointAddr`) — a SEPARATE
    /// identity/endpoint from `iroh_addr` above (the request-routing mesh).
    /// GuardianDB opens its own independent iroh client per node; a peer's mesh
    /// address is a different, unrelated identity, and feeding one in place of
    /// the other makes GuardianDB's automatic peer-discovery try to dial an
    /// EndpointId nothing is actually listening as. `None` until this node's
    /// GuardianDB client has finished its own bind (best-effort, filled in by a
    /// later gossip round once ready — never blocks boot).
    #[serde(default)]
    pub guardian_iroh_addr: Option<String>,
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
    /// Static host capacity (for real cluster resource totals = sum over nodes).
    #[serde(default)]
    pub cpu_cores: u32,
    #[serde(default)]
    pub mem_total_mb: u64,
    #[serde(default)]
    pub disk_total_gb: u64,
    /// Isolation backend this node runs: "firecracker" (real microVMs) or "mock"
    /// (sandboxed child processes — local/dev). The placement scheduler only
    /// auto-targets firecracker nodes; mock/local nodes host only when a region is
    /// explicitly selected for them.
    #[serde(default)]
    pub backend: String,
}

/// Great-circle distance (km) between two lat/lon points — for "nearest node".
pub fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let r = 6371.0_f64;
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}

/// Classify a geographic coordinate into a continent name. Used to auto-assign a
/// node's region/zone to the right continent (e.g. a node in Los Angeles → "North
/// America") for the regions catalog, purely from the lat/lon the node reports —
/// no hard-coded region tables. Coarse bounding boxes, ordered most-specific
/// first; good enough to bucket a real node into the correct continent.
pub fn continent_of(lat: f64, lon: f64) -> &'static str {
    // Antarctica: everything far south.
    if lat < -60.0 {
        return "Antarctica";
    }
    // Oceania: Australia / New Zealand / Pacific (south of the equator, east of ~110°E).
    if lat < 0.0 && lon >= 110.0 && lon <= 180.0 {
        return "Oceania";
    }
    // Europe: includes western Russia; north of the Mediterranean.
    if lat >= 36.0 && lat <= 72.0 && lon >= -25.0 && lon <= 60.0 {
        return "Europe";
    }
    // Africa + Middle East band below Europe.
    if lat >= -35.0 && lat < 36.0 && lon >= -20.0 && lon <= 52.0 {
        return "Africa";
    }
    // Asia: east of Europe/Africa.
    if lon > 52.0 && lon <= 180.0 {
        return "Asia";
    }
    // South America: south of ~13°N in the western hemisphere.
    if lat < 13.0 && lon >= -82.0 && lon <= -34.0 {
        return "South America";
    }
    // North America: the rest of the western hemisphere.
    if lon >= -170.0 && lon <= -34.0 {
        return "North America";
    }
    "Other"
}

pub struct NodeRegistry {
    me: RwLock<NodeInfo>,
    peers: RwLock<HashMap<String, NodeInfo>>,
}

impl NodeRegistry {
    pub fn new(me: NodeInfo) -> Arc<NodeRegistry> {
        Arc::new(NodeRegistry {
            me: RwLock::new(me),
            peers: RwLock::new(HashMap::new()),
        })
    }

    pub fn me(&self) -> NodeInfo {
        let mut me = self.me.read().clone();
        me.is_self = true;
        me.last_seen_ms = now_ms();
        me.latency_ms = 0;
        me.healthy = true;
        me
    }

    /// Update this node's own `guardian_iroh_addr` once GuardianDB's
    /// independent iroh client has finished binding (it isn't ready at boot,
    /// when `me` is first constructed — see the field's own doc comment).
    /// Best-effort, idempotent, safe to call every gossip round: a no-op
    /// write when the value hasn't changed, picked up by the very next
    /// outgoing gossip broadcast since `nodes()`/`me()` always read fresh.
    pub fn set_self_guardian_addr(&self, addr: Option<String>) {
        let mut me = self.me.write();
        if me.guardian_iroh_addr != addr {
            me.guardian_iroh_addr = addr;
        }
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

    pub fn region(&self) -> String {
        self.me.read().region.clone()
    }

    /// Record/refresh a peer learned over gossip. Crucially, this does NOT bump
    /// `last_seen` to local-now: a node's liveness is whether ITS OWN emitted record is
    /// still recent (origin timestamp), not whether some peer just re-mentioned a cached
    /// copy. Re-stamping on every relay is exactly what kept a dead node alive forever as
    /// a "healthy zombie" — its record was perpetually refreshed second-hand and never
    /// aged out. Keeping the origin timestamp means a dead node's record freezes and
    /// drops mesh-wide via the 30s staleness window in `nodes()` (clocks are NTP-synced,
    /// skew ≪ window). Direct probes (`set_health`) still bump freshness for OUR peers.
    pub fn upsert_peer(&self, mut peer: NodeInfo) {
        peer.is_self = false;
        let mut peers = self.peers.write();
        if let Some(existing) = peers.get(&peer.id) {
            // Health + latency are owned by our OWN direct probes, never second-hand gossip.
            peer.healthy = existing.healthy;
            peer.latency_ms = existing.latency_ms;
            // Keep the freshest origin timestamp seen (a relayed copy may arrive stale).
            peer.last_seen_ms = peer.last_seen_ms.max(existing.last_seen_ms);
            // Never regress guardian_iroh_addr to None. A peer's OWN direct
            // announce carries its current value; a THIRD peer's relayed
            // /v1/nodes response (this same round, processed concurrently)
            // may carry an older, not-yet-propagated copy of that same peer's
            // entry — still None if the relaying peer hasn't itself learned
            // the address yet. Without this, whichever response is processed
            // LAST this round wins regardless of freshness, so a stale relay
            // arriving after the peer's own direct announce silently erases
            // it (observed live: guardian_iroh_addr flapping Some -> None on
            // the very next round with no code path that should clear it).
            if peer.guardian_iroh_addr.is_none() {
                peer.guardian_iroh_addr = existing.guardian_iroh_addr.clone();
            }
        }
        peers.insert(peer.id.clone(), peer);
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
            public_ip: None,
            public_ip6: None,
            peer_id: None,
            iroh_addr: None,
            guardian_iroh_addr: None,
            last_seen_ms: now_ms(),
            is_self: false,
            latency_ms: latency,
            healthy,
            lat: None,
            lon: None,
            city: None,
            country: None,
            cpu_cores: 0,
            mem_total_mb: 0,
            disk_total_gb: 0,
            backend: String::new(),
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
    fn continent_classification() {
        assert_eq!(continent_of(34.05, -118.24), "North America"); // Los Angeles
        assert_eq!(continent_of(38.9, -77.0), "North America"); // Washington, D.C.
        assert_eq!(continent_of(51.5, -0.1), "Europe"); // London
        assert_eq!(continent_of(50.1, 8.7), "Europe"); // Frankfurt
        assert_eq!(continent_of(35.7, 139.7), "Asia"); // Tokyo
        assert_eq!(continent_of(1.35, 103.8), "Asia"); // Singapore
        assert_eq!(continent_of(-23.5, -46.6), "South America"); // São Paulo
        assert_eq!(continent_of(-33.9, 151.2), "Oceania"); // Sydney
        assert_eq!(continent_of(-1.3, 36.8), "Africa"); // Nairobi
        assert_eq!(continent_of(-82.0, 0.0), "Antarctica");
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

    #[test]
    fn upsert_does_not_resurrect_directly_failed_health() {
        let reg = NodeRegistry::new(node("self", "iad1", 0, true));
        reg.upsert_peer(node("peer", "sfo1", 10, true));
        // Our own direct probe failed → unhealthy.
        reg.set_health("peer", 0, false);
        // A second node re-gossips it as healthy=true (the zombie path)...
        reg.upsert_peer(node("peer", "sfo1", 10, true));
        // ...but locally-observed health wins: it stays unhealthy until OUR probe succeeds.
        let p = reg.nodes().into_iter().find(|n| n.id == "peer").unwrap();
        assert!(!p.healthy, "second-hand gossip must not resurrect a directly-failed node");
        // A real direct success does bring it back.
        reg.set_health("peer", 5, true);
        let p = reg.nodes().into_iter().find(|n| n.id == "peer").unwrap();
        assert!(p.healthy, "direct probe success restores health");
    }
}
