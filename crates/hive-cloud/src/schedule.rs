//! Placement scheduler — decides which node(s) should HOST a deployment, given
//! the project's configured regions (or none) plus live mesh state.
//!
//! Policy (see the placement plan):
//!   * eligible node = healthy + Firecracker backend + meets a memory floor. This
//!     excludes the resource-poor local/mock Mac nodes from *automatic* selection.
//!   * explicit regions → one target per selected region; the explicit choice
//!     overrides the eligibility filter (so picking `los-angeles` deploys to the
//!     local nodes on purpose). Ties broken by least current load.
//!   * no regions → the eligible node geographically NEAREST the coordinator (this
//!     node, which is the user's own machine — so "nearest me" ≈ "nearest you").
//!   * if nothing is eligible/reachable, returns empty → the caller falls back to
//!     hosting locally so deploys never silently fail.

use std::collections::HashMap;
use std::sync::Arc;

use hive_edge::{haversine_km, NodeInfo};

use crate::state::CloudState;

/// Minimum total memory (MB) for a node to be auto-eligible.
const MEM_FLOOR_MB: u64 = 1024;

/// A chosen placement target. `admin` is `None` when the target is THIS node
/// (host locally); otherwise it's the node's admin URL to dispatch the deploy to.
#[derive(Clone, Debug)]
pub struct Target {
    pub node: String,
    pub admin: Option<String>,
}

fn eligible(n: &NodeInfo) -> bool {
    n.healthy && n.backend == "firecracker" && n.mem_total_mb >= MEM_FLOOR_MB
}

/// Per-node count of hosted deployments (a cheap "load" proxy): how many serve
/// hosts each node owns — self from the local gateway, peers from the gossiped
/// routing table.
fn load_map(cloud: &Arc<CloudState>) -> HashMap<String, usize> {
    let mut m: HashMap<String, usize> = HashMap::new();
    m.insert(cloud.node_name.clone(), cloud.gw.served_hosts().len());
    for routes in cloud.peer_routes.read().values() {
        for r in routes {
            *m.entry(r.node_id.clone()).or_insert(0) += 1;
        }
    }
    m
}

/// Choose placement targets. See module docs for the policy.
pub fn place(cloud: &Arc<CloudState>, regions: &[String]) -> Vec<Target> {
    let nodes = cloud.registry.nodes(); // self first
    let me = cloud.node_name.clone();
    let load = load_map(cloud);
    let load_of = |name: &str| -> usize { load.get(name).copied().unwrap_or(0) };
    let admin_of = |name: &str| -> Option<String> {
        if name == me {
            None
        } else {
            cloud.node_admins.read().get(name).cloned()
        }
    };
    // A node is dispatchable if it's us, or we know its admin URL.
    let reachable = |n: &NodeInfo| -> bool { n.name == me || cloud.node_admins.read().contains_key(&n.name) };

    let regions: Vec<String> = regions
        .iter()
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| !r.is_empty())
        .collect();

    if !regions.is_empty() {
        // One target per selected region.
        let mut targets: Vec<Target> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for region in &regions {
            let cands: Vec<&NodeInfo> = nodes
                .iter()
                .filter(|n| n.healthy && n.region.eq_ignore_ascii_case(region) && reachable(n))
                .collect();
            if cands.is_empty() {
                continue; // no reachable node in that region
            }
            // Prefer eligible nodes; if none are eligible, honor the explicit
            // region choice with any healthy node there (e.g. los-angeles → local).
            let eligibles: Vec<&NodeInfo> = cands.iter().copied().filter(|n| eligible(n)).collect();
            let mut pool = if eligibles.is_empty() { cands } else { eligibles };
            pool.sort_by_key(|n| load_of(&n.name));
            let chosen = pool[0];
            if seen.insert(chosen.name.clone()) {
                targets.push(Target { node: chosen.name.clone(), admin: admin_of(&chosen.name) });
            }
        }
        return targets;
    }

    // Default: the eligible node nearest the coordinator (this node).
    let self_geo = nodes
        .iter()
        .find(|n| n.name == me)
        .and_then(|n| match (n.lat, n.lon) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        });
    let mut elig: Vec<&NodeInfo> = nodes.iter().filter(|n| eligible(n) && reachable(n)).collect();
    if elig.is_empty() {
        return Vec::new(); // caller hosts locally as a fallback
    }
    let dist = |n: &NodeInfo| -> f64 {
        match (self_geo, n.lat, n.lon) {
            (Some(s), Some(a), Some(b)) => haversine_km(s, (a, b)),
            _ => f64::MAX, // unknown geo sorts last
        }
    };
    elig.sort_by(|a, b| {
        dist(a)
            .partial_cmp(&dist(b))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| load_of(&a.name).cmp(&load_of(&b.name)))
    });
    let chosen = elig[0];
    vec![Target { node: chosen.name.clone(), admin: admin_of(&chosen.name) }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_edge::NodeInfo;

    fn node(name: &str, region: &str, backend: &str, mem: u64, lat: f64, lon: f64, healthy: bool) -> NodeInfo {
        NodeInfo {
            id: name.into(),
            name: name.into(),
            region: region.into(),
            public_url: String::new(),
            peer_id: None,
            iroh_addr: None,
            last_seen_ms: 0,
            is_self: false,
            latency_ms: 0,
            healthy,
            lat: Some(lat),
            lon: Some(lon),
            city: None,
            country: None,
            cpu_cores: 4,
            mem_total_mb: mem,
            disk_total_gb: 50,
            backend: backend.into(),
        }
    }

    // Pure-logic mirror of `place` for unit testing without a full CloudState.
    fn pick(nodes: &[NodeInfo], me: &str, regions: &[&str]) -> Vec<String> {
        let admins: std::collections::HashSet<String> =
            nodes.iter().filter(|n| n.name != me).map(|n| n.name.clone()).collect();
        let reachable = |n: &NodeInfo| n.name == me || admins.contains(&n.name);
        let regions: Vec<String> = regions.iter().map(|r| r.to_lowercase()).collect();
        if !regions.is_empty() {
            let mut out = Vec::new();
            for r in &regions {
                let cands: Vec<&NodeInfo> = nodes.iter().filter(|n| n.healthy && n.region == *r && reachable(n)).collect();
                if cands.is_empty() { continue; }
                let elig: Vec<&NodeInfo> = cands.iter().copied().filter(|n| eligible(n)).collect();
                let pool = if elig.is_empty() { cands } else { elig };
                out.push(pool[0].name.clone());
            }
            return out;
        }
        let self_geo = nodes.iter().find(|n| n.name == me).and_then(|n| Some((n.lat?, n.lon?)));
        let mut elig: Vec<&NodeInfo> = nodes.iter().filter(|n| eligible(n) && reachable(n)).collect();
        if elig.is_empty() { return vec![]; }
        elig.sort_by(|a, b| {
            let da = match (self_geo, a.lat, a.lon) { (Some(s), Some(x), Some(y)) => haversine_km(s, (x, y)), _ => f64::MAX };
            let db = match (self_geo, b.lat, b.lon) { (Some(s), Some(x), Some(y)) => haversine_km(s, (x, y)), _ => f64::MAX };
            da.partial_cmp(&db).unwrap()
        });
        vec![elig[0].name.clone()]
    }

    fn mesh() -> Vec<NodeInfo> {
        vec![
            node("node-a", "los-angeles", "mock", 16000, 34.05, -118.24, true),
            node("fc-sanjose", "san-jose", "firecracker", 63000, 37.35, -121.95, true),
            node("fc-virginia", "virginia", "firecracker", 63000, 39.04, -77.48, true),
            node("fc-bangkok", "bangkok", "firecracker", 96000, 13.75, 100.5, true),
        ]
    }

    #[test]
    fn default_picks_nearest_eligible_not_local() {
        // Coordinator = node-a (LA, mock). Nearest eligible firecracker = San Jose.
        let got = pick(&mesh(), "node-a", &[]);
        assert_eq!(got, vec!["fc-sanjose"]);
    }

    #[test]
    fn explicit_region_places_there() {
        assert_eq!(pick(&mesh(), "node-a", &["virginia"]), vec!["fc-virginia"]);
    }

    #[test]
    fn multi_region() {
        let got = pick(&mesh(), "node-a", &["virginia", "bangkok"]);
        assert_eq!(got, vec!["fc-virginia", "fc-bangkok"]);
    }

    #[test]
    fn explicit_local_region_honored_despite_mock() {
        // los-angeles only has the mock node-a — explicit choice overrides eligibility.
        assert_eq!(pick(&mesh(), "node-a", &["los-angeles"]), vec!["node-a"]);
    }

    #[test]
    fn mock_node_never_auto_selected() {
        // Even if node-a is the coordinator, default never picks it (mock).
        let got = pick(&mesh(), "node-a", &[]);
        assert!(!got.contains(&"node-a".to_string()));
    }
}
