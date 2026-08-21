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
///
/// `stateful` (see [`place`]'s doc for the full hazard) is threaded through to
/// the fallback `place` call so a fresh (no-lease-yet) stateful deployment is
/// ALSO protected, not just redeploys of an already-leased one.
pub fn place_for_project(
    cloud: &Arc<CloudState>,
    project: &str,
    regions: &[String],
    is_container: bool,
    stateful: bool,
    needs_gpu: bool,
    needs_wasm: bool,
) -> Vec<Target> {
    if let Some(holder) = cloud.leases.owner_of(project) {
        let nodes = cloud.registry.nodes();
        if let Some(n) = nodes.iter().find(|n| n.name == holder) {
            let region_ok = regions.is_empty()
                || regions
                    .iter()
                    .any(|r| r.trim().eq_ignore_ascii_case(&n.region));
            let reachable = n.name == cloud.node_name
                || cloud.node_admins.read().contains_key(&n.name)
                || (n.peer_id.is_some() && n.iroh_addr.is_some());
            let gpu_ok = !needs_gpu || n.gpu_count > 0;
            // Stickiness must not out-rank capability: a lease held from before
            // the project switched to `runtime: "wasmer"` would otherwise pin
            // every redeploy to a node that cannot execute it. Same predicate
            // `place` uses, by construction — see `wasm_capable`.
            let wasm_ok = wasm_capable(n, needs_wasm);
            if n.healthy && region_ok && reachable && gpu_ok && wasm_ok {
                tracing::info!(project = %project, holder = %holder, "placement: sticking with current lease holder for redeploy");
                // Carry BOTH transports whenever both are known. They are
                // complementary, not alternatives: a security group that blocks
                // 8786/8787 kills HTTP admin dispatch while the node stays
                // perfectly healthy over iroh, and a degraded mesh path fails the
                // reverse way. Filling only one (this used to set `iroh: None` the
                // moment an admin URL existed, and vice versa) left the dispatcher
                // with no second option, so a single transport hiccup failed the
                // whole deploy — witnessed as "✗ fc-sanjose-gpu-1: iroh dispatch
                // failed" on a node that was otherwise healthy. `dispatch_deploy`
                // decides preference and falls back.
                let target = if n.name == cloud.node_name {
                    Target {
                        node: n.name.clone(),
                        admin: None,
                        iroh: None,
                    }
                } else {
                    Target {
                        node: n.name.clone(),
                        admin: cloud.node_admins.read().get(&n.name).cloned(),
                        iroh: match (n.peer_id.clone(), n.iroh_addr.clone()) {
                            (Some(id), Some(addr)) => Some((id, addr)),
                            _ => None,
                        },
                    }
                };
                return vec![target];
            }
        }
    }
    place(
        cloud,
        regions,
        is_container,
        stateful,
        needs_gpu,
        needs_wasm,
    )
}

/// May `n` host a deployment whose functions run on `Runtime::Wasmer`?
///
/// ONE definition, called from both `place`'s capability filter and
/// `place_for_project`'s lease-stickiness fast path, because those two answering
/// differently is precisely how a capability gate springs a leak: stickiness
/// would keep pinning redeploys to the node a project was leased to before it
/// switched runtimes, routing around the filter entirely.
///
/// `None` is NOT CAPABLE, deliberately unlike the `disk_free_gb == 0`
/// unknown-so-admit rule. A peer that does not report the field is running a
/// binary from before Wasmer support existed, on a rootfs built before wasmer
/// was ever staged into it — known-incapable, not unknown. Admitting it hands
/// the deployment to a node guaranteed to fail every cold start forever; an
/// empty candidate set and an honest refusal is strictly better, the same call
/// `gpu_count == 0` already makes.
pub fn wasm_capable(n: &NodeInfo, needs_wasm: bool) -> bool {
    !needs_wasm || n.wasm_runtime == Some(true)
}

/// Minimum free disk (GiB) a node must report to be eligible for new placement.
///
/// Sized above hive-backend's per-cold-start `FLOOR_BYTES` (3 GiB) on purpose:
/// admitting a node with barely one deployment's worth of space just defers the
/// failure to the cold start, which then reports it as the customer's problem.
/// `HIVE_PLACEMENT_DISK_FLOOR_GB` tunes it per fleet.
fn disk_floor_gb() -> u64 {
    std::env::var("HIVE_PLACEMENT_DISK_FLOOR_GB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(20)
}

/// Choose placement targets. See module docs for the policy. `is_container` routes
/// CONTAINER deployments (`__container__`/podman) to container-CAPABLE nodes (the
/// mock/podman backend) — Firecracker nodes can't run them, so placing a container
/// there fails every cold start with "no capacity / No such file or directory".
///
/// `stateful` is the multi-region-fanout safety guard: a container deployment gets
/// an automatic, durable, PER-NODE persistent volume (`container_volume_cfg` in
/// `git.rs`) — a fresh, independent volume on every node it's placed on, with NO
/// data-sync or leader-election between them. For a stateless function that's fine
/// (every replica is interchangeable). For a stateful single-writer service (a
/// Postgres database, a Minecraft world save — anything whose
/// `FunctionConfig::needs_raw_proxy()` is true, or that is otherwise a container
/// per the existing single-owner lease model in `lease.rs`) it is NOT: fanning out
/// to multiple regions would silently create independent, diverging volumes the
/// moment more than one region is selected — e.g. two Minecraft world saves
/// silently forking apart, or two Postgres instances both accepting independent
/// writes with no replication between them (a split-brain). When `stateful` is
/// true, the explicit-region branch below is constrained to a single region (with
/// a clear warning logged) instead of fanning out one target per region — matching
/// this module's existing convention of degrading a placement request to a safe
/// default with a logged explanation rather than hard-failing the deploy (see the
/// module doc's "if nothing is eligible/reachable" fallback, and
/// `place_for_project`'s lease-holder stickiness, for the same pattern).
pub fn place(
    cloud: &Arc<CloudState>,
    regions: &[String],
    is_container: bool,
    stateful: bool,
    // The project's functions request a serverless GPU: only nodes ADVERTISING
    // GPUs (NodeInfo::gpu_count > 0, from the boot nvidia-smi probe) are
    // capable. There is deliberately NO silent fallback to a CPU node — a GPU
    // workload placed on a GPU-less host would cold-start into CUDA errors,
    // which is strictly worse than the explicit empty-placement failure the
    // caller already handles for "nothing eligible".
    needs_gpu: bool,
    // The project's functions run on `Runtime::Wasmer`: only nodes ADVERTISING
    // a reachable wasmer binary (`NodeInfo::wasm_runtime == Some(true)`, from
    // the boot `detect_wasm_runtime` probe) are capable. Same rule and same
    // reasoning as `needs_gpu` directly above, and it is not hypothetical: the
    // first cut of Wasmer support installed the binary on the HOST while every
    // fleet node is Firecracker, which execs `start_cmd` inside the microVM
    // GUEST — so every placement was onto a node guaranteed to ENOENT on every
    // cold start, forever, and the tenant was told to debug their own app.
    // Empty placement plus an honest error is strictly better.
    needs_wasm: bool,
) -> Vec<Target> {
    let nodes = cloud.registry.nodes(); // self first
    let me = cloud.node_name.clone();
    let load = load_map(cloud);
    let load_of = |name: &str| -> usize { load.get(name).copied().unwrap_or(0) };
    // Build the dispatch route for a chosen node: self (both None), else EVERY
    // transport that node currently has — HTTP admin URL and/or iroh (id, addr).
    //
    // Both are filled when both are known. They fail independently (a security
    // group blocking 8786/8787 kills HTTP while iroh is fine; a degraded mesh path
    // fails the other way), so populating only the preferred one left the
    // dispatcher no second option and turned one transport hiccup into a failed
    // deployment. `dispatch_deploy` prefers HTTP and falls back to iroh.
    let target_of = |n: &NodeInfo| -> Target {
        if n.name == me {
            return Target {
                node: n.name.clone(),
                admin: None,
                iroh: None,
            };
        }
        Target {
            node: n.name.clone(),
            admin: cloud.node_admins.read().get(&n.name).cloned(),
            iroh: match (n.peer_id.clone(), n.iroh_addr.clone()) {
                (Some(id), Some(addr)) => Some((id, addr)),
                _ => None,
            },
        }
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
        if needs_gpu && n.gpu_count == 0 {
            return false;
        }
        // Wasmer capability, same hard-filter shape as the GPU gate above.
        // See `wasm_capable` for why `None` excludes rather than admits.
        if !wasm_capable(n, needs_wasm) {
            return false;
        }
        // DISK ADMISSION FLOOR. Placement used to be entirely disk-blind: it
        // filtered on health/region/GPU and then sorted by deployment COUNT, a
        // metric that says nothing about space. So the node with the most free
        // capacity and the node with none scored identically, and a full node
        // kept winning. Witnessed 2026-07-31 — fc-sanjose hit 0 bytes free and
        // took 9 customer deployments down ("host disk critically low ... after
        // GC") while fc-frankfurt and both CVM nodes sat under 10% used with
        // ~920 GiB free each.
        //
        // A HARD filter, not another term in the score, because disk is not like
        // CPU or memory: it does not drain on its own once a deployment lands,
        // so a node that is out of space is out until something is deleted. A
        // weighted score would still let it win when peers look busy.
        //
        // The floor is deliberately larger than the per-cold-start requirement
        // (`FLOOR_BYTES`, 3 GiB in hive-backend): placement must leave room for
        // the deployment it is about to create PLUS the next one, or it just
        // hands the node to the very check that will reject it.
        //
        // `disk_free_gb == 0` means UNKNOWN, not full — a pre-upgrade peer does
        // not report it. Excluding those would empty the candidate set during a
        // rollout, so unknown is admitted and only a positive, genuinely-low
        // reading rejects.
        let floor_gb = disk_floor_gb();
        if n.disk_free_gb > 0 && n.disk_free_gb < floor_gb {
            return false;
        }
        if is_container {
            // A container deployment is served on this node's own public host —
            // never through the microVM guest network — so a node with NO public
            // address can win placement (it's `reachable()` for build DISPATCH via
            // admin/iroh) but the resulting deployment is then unreachable by any
            // real client: only 127.0.0.1. Witnessed as a live risk when the local
            // Mac dev nodes moved region to san-jose — the mock-backend widening
            // below exists so containers can run on them at all, but a
            // region-pinned container could land there and silently never serve.
            // Require a public address for EITHER family, on every backend
            // (firecracker included — a NAT'd FC node has the identical problem,
            // this is not mock-specific), so an unreachable node simply isn't a
            // candidate rather than winning and quietly failing to serve.
            let has_public = n.public_ip.is_some() || n.public_ip6.is_some();
            has_public && (eligible(n) || (n.healthy && n.backend == "mock"))
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
        // One target per selected region — EXCEPT for a stateful/single-writer
        // deployment, which is forced to a single region (see this fn's doc for the
        // diverging-replica hazard this closes). Silently constrain + log rather
        // than reject the deploy outright: consistent with every other placement
        // fallback in this module (never hard-fail on a placement/region choice).
        let regions: &[String] = if stateful && regions.len() > 1 {
            tracing::warn!(
                requested_regions = ?regions,
                constrained_to = %regions[0],
                "placement: stateful/single-writer deployment requested multi-region fanout \
                 across {} regions; no data-sync or leader-election exists between fanout \
                 replicas, so this would silently create independent, diverging volumes per \
                 region (e.g. split-brain Postgres, forked Minecraft world saves). \
                 Constraining to a single region ({}) instead of fanning out.",
                regions.len(),
                regions[0],
            );
            &regions[..1]
        } else {
            &regions[..]
        };
        // One target per selected region.
        let mut targets: Vec<Target> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for region in regions {
            let cands: Vec<&NodeInfo> = nodes
                .iter()
                .filter(|n| n.healthy && n.region.eq_ignore_ascii_case(region) && reachable(n))
                .collect();
            if cands.is_empty() {
                continue; // no reachable node in that region
            }
            // Prefer eligible nodes; if none are eligible, honor the explicit
            // region choice with any healthy node there (e.g. los-angeles → local).
            //
            // EXCEPT for a GPU request: widening here would hand the deployment to
            // a node with no GPU, and the launch then fails on the CDI device that
            // host cannot resolve ("unresolvable CDI devices nvidia.com/gpu=all").
            // Witnessed live — this widening is what silently defeated the
            // gpu_count filter and put a gpu deployment on a CPU node. Skipping the
            // region entirely is correct: `targets` staying empty is the signal the
            // caller turns into an explicit failure.
            let eligibles: Vec<&NodeInfo> = cands.iter().copied().filter(|n| capable(n)).collect();
            if needs_gpu && eligibles.is_empty() {
                tracing::warn!(region = %region, "placement: no GPU-capable node in this region — not widening (gpu request)");
                continue;
            }
            // Same rule, same reason, for the wasm runtime. The widening below
            // deliberately falls back to `cands` (health/region only) when
            // nothing passes `capable()`, which is right for a node that is
            // merely resource-poor — but WRONG for a hard capability: widening
            // hands the deployment to a node with no `wasmer` binary at all,
            // which is exactly the empty-placement-beats-guaranteed-failure rule
            // the gpu arm above encodes. Without this the `capable()` filter was
            // decorative on this path: it removed the incapable nodes and then
            // the fallback put them straight back.
            if needs_wasm && eligibles.is_empty() {
                tracing::warn!(region = %region, "placement: no wasm-capable node in this region — not widening (wasmer runtime)");
                continue;
            }
            let mut pool = if eligibles.is_empty() {
                cands
            } else {
                eligibles
            };
            // Order by load, then by MOST free disk. The disk term is what
            // actively drains a filling node instead of merely refusing it
            // once it is already over the floor (Reverse => larger first).
            pool.sort_by_key(|n| (load_of(&n.name), std::cmp::Reverse(n.disk_free_gb)));
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
            let chosen = pool
                .iter()
                .copied()
                .find(|n| n.name == me)
                .unwrap_or(pool[0]);
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
    let mut elig: Vec<&NodeInfo> = nodes
        .iter()
        .filter(|n| capable(n) && reachable(n))
        .collect();
    if elig.is_empty() {
        // Empty = "no target chosen". For an ordinary deployment the caller hosts
        // locally, which is the long-standing safe default. For a GPU request the
        // caller MUST instead fail the deploy (see `deploy_targets_or_fail` in
        // git.rs) — hosting locally would put it on a GPU-less node.
        if needs_gpu {
            tracing::warn!("placement: gpu requested but no healthy GPU-capable node is reachable");
        }
        if needs_wasm {
            tracing::warn!(
                "placement: wasmer runtime requested but no healthy wasm-capable node is reachable"
            );
        }
        return Vec::new();
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
    let chosen = elig
        .iter()
        .copied()
        .find(|n| n.name == me)
        .unwrap_or(elig[0]);
    vec![target_of(chosen)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_edge::NodeInfo;

    fn node(
        name: &str,
        region: &str,
        backend: &str,
        mem: u64,
        lat: f64,
        lon: f64,
        healthy: bool,
    ) -> NodeInfo {
        NodeInfo {
            gpu_count: 0,
            // These placement fixtures are all non-Wasmer, so `None` — the
            // value a node that never ran the probe reports — is the honest
            // one, and it keeps them on the not-capable path. A test that
            // means "this node CAN run wasm" must set `Some(true)` explicitly,
            // the same rule `disk_free_gb` already carries.
            wasm_runtime: None,
            gpu_model: None,
            gpu_vram_mb: 0,
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
            dns_ns: None,
            dns_api: false,
            dns_attest: Vec::new(),
            dashboard: false,
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
            // Non-zero so these fixtures are ABOVE `disk_floor_gb()` and stay
            // eligible: 0 means UNKNOWN (a pre-upgrade peer), not "full", and
            // `capable()` deliberately admits unknown rather than excluding the
            // fleet mid-rollout — a fixture at 0 would therefore exercise the
            // unknown path, not the has-space path these placement tests mean.
            disk_free_gb: 500,
            gpu_free_mb: None,
            started_ms: 0,
            oom_restarts_24h: 0,
            last_oom_ms: None,
            backend: backend.into(),
            provider: None,
            private_addr: None,
        }
    }

    // Pure-logic mirror of `place` for unit testing without a full CloudState.
    // `admins` = nodes we have an HTTP admin URL for; a node is ALSO reachable if it has
    // an iroh address (peer_id + iroh_addr) — mirrors the real `reachable` so a NAT'd
    // coordinator can place on FC nodes over the mesh.
    fn pick_with(
        nodes: &[NodeInfo],
        me: &str,
        regions: &[&str],
        admins: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let reachable = |n: &NodeInfo| {
            n.name == me
                || admins.contains(&n.name)
                || (n.peer_id.is_some() && n.iroh_addr.is_some())
        };
        let regions: Vec<String> = regions.iter().map(|r| r.to_lowercase()).collect();
        if !regions.is_empty() {
            let mut out = Vec::new();
            for r in &regions {
                let cands: Vec<&NodeInfo> = nodes
                    .iter()
                    .filter(|n| n.healthy && n.region == *r && reachable(n))
                    .collect();
                if cands.is_empty() {
                    continue;
                }
                let elig: Vec<&NodeInfo> = cands.iter().copied().filter(|n| eligible(n)).collect();
                let pool = if elig.is_empty() { cands } else { elig };
                out.push(pool[0].name.clone());
            }
            return out;
        }
        let self_geo = nodes
            .iter()
            .find(|n| n.name == me)
            .and_then(|n| Some((n.lat?, n.lon?)));
        let mut elig: Vec<&NodeInfo> = nodes
            .iter()
            .filter(|n| eligible(n) && reachable(n))
            .collect();
        if elig.is_empty() {
            return vec![];
        }
        elig.sort_by(|a, b| {
            let da = match (self_geo, a.lat, a.lon) {
                (Some(s), Some(x), Some(y)) => haversine_km(s, (x, y)),
                _ => f64::MAX,
            };
            let db = match (self_geo, b.lat, b.lon) {
                (Some(s), Some(x), Some(y)) => haversine_km(s, (x, y)),
                _ => f64::MAX,
            };
            da.partial_cmp(&db).unwrap()
        });
        vec![elig[0].name.clone()]
    }

    // Back-compat wrapper: every non-self node has an HTTP admin URL.
    fn pick(nodes: &[NodeInfo], me: &str, regions: &[&str]) -> Vec<String> {
        let admins: std::collections::HashSet<String> = nodes
            .iter()
            .filter(|n| n.name != me)
            .map(|n| n.name.clone())
            .collect();
        pick_with(nodes, me, regions, &admins)
    }

    fn mesh() -> Vec<NodeInfo> {
        vec![
            node("node-a", "los-angeles", "mock", 16000, 34.05, -118.24, true),
            node(
                "fc-sanjose",
                "san-jose",
                "firecracker",
                63000,
                37.35,
                -121.95,
                true,
            ),
            node(
                "fc-virginia",
                "virginia",
                "firecracker",
                63000,
                39.04,
                -77.48,
                true,
            ),
            node(
                "fc-bangkok",
                "bangkok",
                "firecracker",
                96000,
                13.75,
                100.5,
                true,
            ),
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
        assert_eq!(
            got,
            vec!["fc-virginia", "fc-bangkok"],
            "iroh-reachable FC nodes are placed, not stranded"
        );
        // Without iroh AND without admins, those regions are unreachable → empty (caller
        // then hosts locally — the old, broken behavior we fixed).
        let plain = mesh();
        assert!(pick_with(&plain, "node-a", &["virginia", "bangkok"], &no_admins).is_empty());
    }
}
