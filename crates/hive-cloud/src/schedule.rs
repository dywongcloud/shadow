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

/// A chosen placement target. Dispatch route, in preference order:
///   * `admin = Some(url)` → POST the deploy to that HTTP admin URL.
///   * `admin = None, iroh = Some((id, addr))` → dispatch over the iroh mesh (a NAT'd
///     coordinator has no HTTP path to FC nodes; the SSH tunnels were cut).
///   * `admin = None, iroh = None` → THIS node (host locally).
#[derive(Clone, Debug)]
pub struct Target {
    pub node: String,
    pub admin: Option<String>,
    pub iroh: Option<(String, String)>,
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

/// `place` with lease-holder stickiness for REDEPLOYS (audit proposal step 11):
/// when the project already has a LIVE container lease (`cloud.leases`, keyed by
/// project), prefer the current holder — a redeploy of an existing stateful
/// container should land where its state/lease already lives instead of being
/// blindly re-placed (gratuitous migration: new node cold-starts, old node's
/// lease lingers until expiry, any node-local volume state is left behind). The
/// holder is honored only while healthy + reachable and (for region-pinned
/// projects) inside an allowed region; otherwise this falls through to the
/// normal placement policy unchanged.
pub fn place_for_project(
    cloud: &Arc<CloudState>,
    project: &str,
    regions: &[String],
    is_container: bool,
) -> Vec<Target> {
    if let Some(holder) = cloud.leases.owner_of(project) {
        let nodes = cloud.registry.nodes();
        if let Some(n) = nodes.iter().find(|n| n.name == holder) {
            let region_ok = regions.is_empty()
                || regions.iter().any(|r| r.trim().eq_ignore_ascii_case(&n.region));
            let reachable = n.name == cloud.node_name
                || cloud.node_admins.read().contains_key(&n.name)
                || (n.peer_id.is_some() && n.iroh_addr.is_some());
            if n.healthy && region_ok && reachable {
                tracing::info!(project = %project, holder = %holder, "placement: sticking with current lease holder for redeploy");
                let target = if n.name == cloud.node_name {
                    Target { node: n.name.clone(), admin: None, iroh: None }
                } else if let Some(a) = cloud.node_admins.read().get(&n.name).cloned() {
                    Target { node: n.name.clone(), admin: Some(a), iroh: None }
                } else {
                    let iroh = match (n.peer_id.clone(), n.iroh_addr.clone()) {
                        (Some(id), Some(addr)) => Some((id, addr)),
                        _ => None,
                    };
                    Target { node: n.name.clone(), admin: None, iroh }
                };
                return vec![target];
            }
        }
    }
    place(cloud, regions, is_container)
}

/// Choose placement targets. See module docs for the policy. `is_container` routes
/// CONTAINER deployments (`__container__`/podman) to container-CAPABLE nodes (the
/// mock/podman backend) — Firecracker nodes can't run them, so placing a container
/// there fails every cold start with "no capacity / No such file or directory".
pub fn place(cloud: &Arc<CloudState>, regions: &[String], is_container: bool) -> Vec<Target> {
    let nodes = cloud.registry.nodes(); // self first
    let me = cloud.node_name.clone();
    let load = load_map(cloud);
    let load_of = |name: &str| -> usize { load.get(name).copied().unwrap_or(0) };
    // Build the dispatch route for a chosen node: self (both None), HTTP admin URL, or
    // iroh (id, addr) when no HTTP path exists (NAT'd coordinator → FC over the mesh).
    let target_of = |n: &NodeInfo| -> Target {
        if n.name == me {
            return Target { node: n.name.clone(), admin: None, iroh: None };
        }
        if let Some(a) = cloud.node_admins.read().get(&n.name).cloned() {
            return Target { node: n.name.clone(), admin: Some(a), iroh: None };
        }
        let iroh = match (n.peer_id.clone(), n.iroh_addr.clone()) {
            (Some(id), Some(addr)) => Some((id, addr)),
            _ => None,
        };
        Target { node: n.name.clone(), admin: None, iroh }
    };
    // A node is dispatchable if it's us, we know its HTTP admin URL, OR we can reach it
    // over the iroh mesh (has a peer id + dialable address). The last case is what lets
    // a NAT'd coordinator place deploys on FC nodes after the SSH tunnels were cut.
    let reachable = |n: &NodeInfo| -> bool {
        n.name == me
            || cloud.node_admins.read().contains_key(&n.name)
            || (n.peer_id.is_some() && n.iroh_addr.is_some())
    };
    // Capability filter. Firecracker nodes now run CONTAINERS via host podman
    // (outside the microVM), so a container is eligible on any healthy real node —
    // a Firecracker node (preferred: more resources) OR the mock/podman backend.
    // Non-container functions still want a Firecracker microVM node.
    let capable = |n: &NodeInfo| -> bool {
        if is_container {
            eligible(n) || (n.healthy && n.backend == "mock")
        } else {
            eligible(n)
        }
    };

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
            let eligibles: Vec<&NodeInfo> = cands.iter().copied().filter(|n| capable(n)).collect();
            let mut pool = if eligibles.is_empty() { cands } else { eligibles };
            pool.sort_by_key(|n| load_of(&n.name));
            // Prefer the COORDINATOR itself when it's a valid candidate for this
            // region: a local build has full log fidelity and zero cross-node
            // dispatch/mirror dependency, whereas dispatching to a remote node
            // (esp. an iroh-only one) hinges on the mesh round-trip both
            // delivering the deploy AND streaming the build log back -- if that
            // mirror stalls, the dashboard shows a build stuck at "dispatching"
            // even though the deployment succeeded remotely. Pure load-sorting
            // otherwise sends every deploy to whichever peer is idlest (a fresh
            // node with 0 deployments always wins), which is exactly how a
            // brand-new node became the sink for every build. Self only wins the
            // region it's actually IN; other regions still pick the least-loaded
            // node there.
            let chosen = pool.iter().copied().find(|n| n.name == me).unwrap_or(pool[0]);
            if seen.insert(chosen.name.clone()) {
                targets.push(target_of(chosen));
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
    let mut elig: Vec<&NodeInfo> = nodes.iter().filter(|n| capable(n) && reachable(n)).collect();
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
    // Prefer the coordinator itself when it's eligible: `self` is always the
    // nearest node (distance 0 to its own geo), so this only overrides the
    // load-tiebreak that otherwise ships every deploy to whichever same-region
    // peer is idlest — which is how a fresh 0-deployment node became the sink
    // for every build, then failed each one on the fragile cross-node dispatch/
    // mirror path. A local build has full log fidelity and no mesh dependency.
    // When `self` isn't eligible (e.g. a mock/LA coordinator), the nearest
    // eligible remote still wins.
    let chosen = elig.iter().copied().find(|n| n.name == me).unwrap_or(elig[0]);
    vec![target_of(chosen)]
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
            public_ip: None,
            public_ip6: None,
            peer_id: None,
            iroh_addr: None,
            guardian_iroh_addr: None,
            relay_url: None,
            cp_epoch: 0,
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
    // `admins` = nodes we have an HTTP admin URL for; a node is ALSO reachable if it has
    // an iroh address (peer_id + iroh_addr) — mirrors the real `reachable` so a NAT'd
    // coordinator can place on FC nodes over the mesh.
    fn pick_with(nodes: &[NodeInfo], me: &str, regions: &[&str], admins: &std::collections::HashSet<String>) -> Vec<String> {
        let reachable = |n: &NodeInfo| {
            n.name == me || admins.contains(&n.name) || (n.peer_id.is_some() && n.iroh_addr.is_some())
        };
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

    // Back-compat wrapper: every non-self node has an HTTP admin URL.
    fn pick(nodes: &[NodeInfo], me: &str, regions: &[&str]) -> Vec<String> {
        let admins: std::collections::HashSet<String> =
            nodes.iter().filter(|n| n.name != me).map(|n| n.name.clone()).collect();
        pick_with(nodes, me, regions, &admins)
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

    #[test]
    fn iroh_reachable_fc_node_selected_when_no_http_admin() {
        // The preview-strand bug: from the NAT'd coordinator the SSH tunnels are gone,
        // so node_admins (HTTP) is EMPTY — yet the FC nodes are reachable over iroh.
        // They must still be chosen (dispatch over the mesh), NOT stranded locally.
        let mut nodes = mesh();
        for n in nodes.iter_mut().filter(|n| n.backend == "firecracker") {
            n.peer_id = Some(format!("id-{}", n.name));
            n.iroh_addr = Some("{\"id\":\"x\",\"addrs\":[]}".into());
        }
        let no_admins = std::collections::HashSet::new(); // no HTTP admin URLs at all
        let got = pick_with(&nodes, "node-a", &["virginia", "bangkok"], &no_admins);
        assert_eq!(got, vec!["fc-virginia", "fc-bangkok"], "iroh-reachable FC nodes are placed, not stranded");
        // Without iroh AND without admins, those regions are unreachable → empty (caller
        // then hosts locally — the old, broken behavior we fixed).
        let plain = mesh();
        assert!(pick_with(&plain, "node-a", &["virginia", "bangkok"], &no_admins).is_empty());
    }
}
