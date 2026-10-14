//! Placement scheduler: which node(s) host a deployment, from the project's regions plus live mesh
//! state.
//!
//! An eligible node is healthy, runs an isolation backend (Firecracker, or Litebox while it
//! advertises the current runtime-artifact protocol; Mock is never treated as isolated) and meets
//! `MEM_FLOOR_MB`. Explicit regions yield one target per region; no regions yields the capable node
//! nearest the coordinator, the coordinator itself winning when it qualifies. An empty result means
//! no target was chosen: the caller hosts locally, except where a hard capability need (GPU,
//! Wasmer, a repository build) makes it refuse the deploy.

use std::collections::HashMap;
use std::sync::Arc;

use hive_edge::{haversine_km, NodeInfo};

use crate::state::CloudState;

/// Minimum total memory (MB) for a node to be auto-eligible.
const MEM_FLOOR_MB: u64 = 1024;
const BUILD_EXECUTOR_PROTOCOL_V1: u16 = 1;
const DEFAULT_DISK_FLOOR_GB: u64 = 20;
const UNKNOWN_GEO_DISTANCE_KM: f64 = f64::MAX;

/// A placement target: `admin` (HTTP admin URL) and `iroh` (id, addr) are every transport known for
/// the node, HTTP tried first; both `None` means THIS node (host locally).
#[derive(Clone, Debug)]
pub struct Target {
    pub node: String,
    pub admin: Option<String>,
    pub iroh: Option<(String, String)>,
}

fn eligible(n: &NodeInfo) -> bool {
    let isolated =
        n.backend == "firecracker" || (n.backend == "litebox" && runtime_artifact_capable(n, true));
    n.healthy && isolated && n.mem_total_mb >= MEM_FLOOR_MB
}

/// Served hosts per node, the load proxy: self from the local gateway, peers from the gossiped
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

/// `place` with lease-holder stickiness: a redeploy stays on the node holding the project's live
/// container lease (its state lives there) while that node is healthy, [`reachable`] (the SAME
/// predicate `place` filters on, so a NAT'd holder nobody can dial is never chosen), in an allowed
/// region and meets the same capability needs as `place`; otherwise it falls through to `place`
/// with the same arguments.
pub fn place_for_project(
    cloud: &Arc<CloudState>,
    project: &str,
    regions: &[String],
    is_container: bool,
    stateful: bool,
    needs_gpu: bool,
    needs_interpreter: impl Into<InterpreterNeeds>,
    needs_runtime_artifact: bool,
    needs_build_isolation: bool,
    needs_build_toolchain: Option<&str>,
) -> Vec<Target> {
    let needs_interpreter = needs_interpreter.into();
    if let Some(holder) = cloud.leases.owner_of(project) {
        let nodes = cloud.registry.nodes();
        if let Some(n) = nodes.iter().find(|n| n.name == holder) {
            let region_ok = regions.is_empty()
                || regions
                    .iter()
                    .any(|r| r.trim().eq_ignore_ascii_case(&n.region));
            let reachable = reachable(cloud, n);
            let gpu_ok = !needs_gpu || n.gpu_count > 0;
            let wasm_ok = wasm_capable(n, needs_interpreter.wasm);
            let bun_ok = bun_capable(n, needs_interpreter.bun);
            let runtime_artifact_ok = runtime_artifact_capable(n, needs_runtime_artifact);
            let build_isolation_ok = build_isolation_capable(n, needs_build_isolation);
            let build_toolchain_ok = build_toolchain_capable(n, needs_build_toolchain);
            if n.healthy
                && region_ok
                && reachable
                && gpu_ok
                && wasm_ok
                && bun_ok
                && runtime_artifact_ok
                && build_isolation_ok
                && build_toolchain_ok
            {
                tracing::info!(project = %project, holder = %holder, "placement: sticking with current lease holder for redeploy");
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
        needs_interpreter,
        needs_runtime_artifact,
        needs_build_isolation,
        needs_build_toolchain,
    )
}

/// May `n` host a `Runtime::Wasmer` deployment? Only a node reporting `Some(true)`: `None` is NOT
/// CAPABLE, unlike `disk_free_gb == 0`, because a peer that does not report it predates Wasmer
/// support and would fail every cold start.
pub fn wasm_capable(n: &NodeInfo, needs_wasm: bool) -> bool {
    !needs_wasm || n.wasm_runtime == Some(true)
}

/// May `n` host a `Runtime::Bun` deployment? Same rule as [`wasm_capable`]: only a node reporting
/// `Some(true)`.
pub fn bun_capable(n: &NodeInfo, needs_bun: bool) -> bool {
    !needs_bun || n.bun_runtime == Some(true)
}

/// Interpreter-runtime needs of one placement; `From<bool>` keeps a bare `bool` argument meaning
/// `wasm` only (`bun: false`).
#[derive(Clone, Copy, Debug, Default)]
pub struct InterpreterNeeds {
    pub wasm: bool,
    pub bun: bool,
}

impl From<bool> for InterpreterNeeds {
    fn from(wasm: bool) -> Self {
        InterpreterNeeds { wasm, bun: false }
    }
}

/// A source-built function needs a node advertising runtime-artifact protocol v1 (the split
/// host-static-root / guest-workdir contract); a pre-upgrade peer reads it differently, so
/// placement and lease stickiness fail closed.
pub fn runtime_artifact_capable(n: &NodeInfo, needed: bool) -> bool {
    !needed || n.runtime_artifact_protocol == Some(hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION)
}

/// May `n`'s build lane run a build whose package manager is `manager`? Unlike `wasm_capable`, an
/// unreported toolchain set (`None`) is ADMITTED: a missing manager has a substitute (`npm` ships
/// with node), and excluding unreported peers would empty the candidate set mid-rollout. A reported
/// set needs the manager or a substitute; `manager: None` gates nothing.
pub fn build_toolchain_capable(n: &NodeInfo, manager: Option<&str>) -> bool {
    let Some(manager) = manager else {
        return true;
    };
    let Some(have) = n.build_toolchains.as_ref() else {
        return true;
    };
    have.iter().any(|t| t == manager)
        || crate::resources::package_manager_substitute(manager, have).is_some()
}

const TOOLCHAIN_EXACT: u8 = 0;
/// An unreported node ranks here: unknown, never preferred over a node known to carry the manager.
const TOOLCHAIN_SUBSTITUTE_OR_UNREPORTED: u8 = 1;
const TOOLCHAIN_CANNOT_BUILD: u8 = 2;

/// Ordering rank for `place` (lower first): a bun build prefers a node that has bun without ever
/// refusing a deploy that would succeed degraded. See [`build_toolchain_capable`].
pub fn build_toolchain_rank(n: &NodeInfo, manager: Option<&str>) -> u8 {
    let Some(manager) = manager else {
        return TOOLCHAIN_EXACT;
    };
    let Some(have) = n.build_toolchains.as_ref() else {
        return TOOLCHAIN_SUBSTITUTE_OR_UNREPORTED;
    };
    if have.iter().any(|t| t == manager) {
        TOOLCHAIN_EXACT
    } else if crate::resources::package_manager_substitute(manager, have).is_some() {
        TOOLCHAIN_SUBSTITUTE_OR_UNREPORTED
    } else {
        TOOLCHAIN_CANNOT_BUILD
    }
}

fn host_exec_backend(n: &NodeInfo) -> bool {
    matches!(n.backend.as_str(), "mock" | "litebox")
}

/// Repository build commands need a node whose boot probe proved BuildExecutor v1, or a host-exec
/// backend (mock/litebox) that builds on the host and never needed the microVM-era executor.
pub fn build_isolation_capable(n: &NodeInfo, needed: bool) -> bool {
    !needed
        || n.build_isolation_protocol == Some(BUILD_EXECUTOR_PROTOCOL_V1)
        || host_exec_backend(n)
}

/// Minimum free disk (GiB) for new placement, above the ~3 GiB per-cold-start floor
/// (`HIVE_DISK_FLOOR_MIB`, hive-backend) so an admitted node fits this deployment and the next;
/// `HIVE_PLACEMENT_DISK_FLOOR_GB` overrides.
fn disk_floor_gb() -> u64 {
    std::env::var("HIVE_PLACEMENT_DISK_FLOOR_GB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_DISK_FLOOR_GB)
}

/// A hard filter, not a score term: a full node stays full until something is deleted. Only a
/// positive reading under the floor rejects, because `disk_free_gb == 0` is UNKNOWN (a pre-upgrade
/// peer).
fn below_disk_floor(n: &NodeInfo, floor_gb: u64) -> bool {
    n.disk_free_gb > 0 && n.disk_free_gb < floor_gb
}

/// Whether `n` advertises a public address: [`crate::cluster::Cluster::addressable`], the one
/// predicate the control-plane election and `strict_chain_owner` use (an empty or blank string is
/// no address).
fn has_public_address(n: &NodeInfo) -> bool {
    crate::cluster::Cluster::addressable(n)
}

/// Dispatch route for `n`: self is `(None, None)`, otherwise every transport it has, because HTTP
/// admin and iroh fail independently and `fanout_remote` falls back from HTTP to iroh.
fn target_of(cloud: &Arc<CloudState>, n: &NodeInfo) -> Target {
    if n.name == cloud.node_name {
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
}

/// True when some path to `n` can carry traffic: a public address, a relay in its iroh address, or
/// a live trunk to us. A peer id plus an iroh address alone is no evidence (a NAT'd node may
/// advertise only RFC1918).
fn dialable(n: &NodeInfo) -> bool {
    if has_public_address(n) {
        return true;
    }
    if n.iroh_addr
        .as_deref()
        .is_some_and(|a| a.contains("\"Relay\""))
    {
        return true;
    }
    n.peer_id
        .as_deref()
        .is_some_and(|id| hive_p2p::establish::peer_conns(id) > 0)
}

/// Dispatchable: this node, a node with a known HTTP admin URL, or a peer with an iroh id and a
/// `dialable` address.
fn reachable(cloud: &Arc<CloudState>, n: &NodeInfo) -> bool {
    n.name == cloud.node_name
        || cloud.node_admins.read().contains_key(&n.name)
        || (n.peer_id.is_some() && n.iroh_addr.is_some() && dialable(n))
}

fn is_mock_backend(n: &NodeInfo) -> bool {
    n.backend == "mock"
}

/// Capability filter: the hard needs, the disk floor, then an eligible backend or a healthy Mock
/// node (Mock builds and runs directly on the host, so it needs neither BuildExecutor nor the split
/// runtime-artifact contract `eligible` enforces for Firecracker, and a region made only of mock
/// nodes can still place any deployment). A container also needs a public address: it is served
/// from the node's own host, never through the microVM guest network.
fn capable(
    n: &NodeInfo,
    is_container: bool,
    needs_gpu: bool,
    needs_interpreter: InterpreterNeeds,
    needs_runtime_artifact: bool,
    needs_build_isolation: bool,
    needs_build_toolchain: Option<&str>,
) -> bool {
    if needs_gpu && n.gpu_count == 0 {
        return false;
    }
    if !wasm_capable(n, needs_interpreter.wasm) {
        return false;
    }
    if !bun_capable(n, needs_interpreter.bun) {
        return false;
    }
    if !runtime_artifact_capable(n, needs_runtime_artifact) {
        return false;
    }
    if !build_isolation_capable(n, needs_build_isolation) {
        return false;
    }
    if !build_toolchain_capable(n, needs_build_toolchain) {
        return false;
    }
    let floor_gb = disk_floor_gb();
    if below_disk_floor(n, floor_gb) {
        return false;
    }
    if is_container {
        has_public_address(n) && (eligible(n) || (n.healthy && is_mock_backend(n)))
    } else {
        eligible(n) || (n.healthy && is_mock_backend(n))
    }
}

fn coordinator_geo(nodes: &[NodeInfo], me: &str) -> Option<(f64, f64)> {
    nodes
        .iter()
        .find(|n| n.name == me)
        .and_then(|n| match (n.lat, n.lon) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        })
}

fn distance_km(from: Option<(f64, f64)>, n: &NodeInfo) -> f64 {
    match (from, n.lat, n.lon) {
        (Some(s), Some(a), Some(b)) => haversine_km(s, (a, b)),
        _ => UNKNOWN_GEO_DISTANCE_KM,
    }
}

/// The coordinator itself when it is among `candidates` (a local build has full log fidelity and no
/// mesh dependency; pure load-sorting would send every deploy to whichever peer is idlest), else the
/// first. `candidates` must not be empty.
fn prefer_coordinator<'a>(candidates: &[&'a NodeInfo], me: &str) -> &'a NodeInfo {
    candidates
        .iter()
        .copied()
        .find(|n| n.name == me)
        .unwrap_or(candidates[0])
}

/// Remote nodes to retry a build on after every placed target was UNREACHED (nothing ran): healthy,
/// dispatchable, `capable` of this exact deployment (never `place`'s explicit-region widening), not
/// in `exclude`, not this node. Ordered by configured region, proximity, toolchain rank, load, most
/// free disk, then name, so every observer agrees on the sequence; a region-pinned project whose
/// only in-region node is down lands out of region instead of failing.
pub fn dispatch_fallbacks(
    cloud: &Arc<CloudState>,
    regions: &[String],
    is_container: bool,
    needs_gpu: bool,
    needs_interpreter: impl Into<InterpreterNeeds>,
    needs_runtime_artifact: bool,
    needs_build_isolation: bool,
    needs_build_toolchain: Option<&str>,
    exclude: &[String],
) -> Vec<Target> {
    let needs_interpreter = needs_interpreter.into();
    let nodes = cloud.registry.nodes();
    let me = cloud.node_name.clone();
    let load = load_map(cloud);
    let load_of = |name: &str| -> usize { load.get(name).copied().unwrap_or(0) };
    let regions: Vec<String> = regions
        .iter()
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| !r.is_empty())
        .collect();
    let region_rank = |n: &NodeInfo| -> usize {
        regions
            .iter()
            .position(|r| n.region.eq_ignore_ascii_case(r))
            .unwrap_or(regions.len())
    };
    let self_geo = coordinator_geo(&nodes, &me);
    let dist = |n: &NodeInfo| -> f64 { distance_km(self_geo, n) };
    let mut cands: Vec<&NodeInfo> = nodes
        .iter()
        .filter(|n| {
            !n.is_self
                && n.name != me
                && n.healthy
                && !exclude.iter().any(|x| x.eq_ignore_ascii_case(&n.name))
                && reachable(cloud, n)
                && capable(
                    n,
                    is_container,
                    needs_gpu,
                    needs_interpreter,
                    needs_runtime_artifact,
                    needs_build_isolation,
                    needs_build_toolchain,
                )
        })
        .collect();
    cands.sort_by(|a, b| {
        region_rank(a)
            .cmp(&region_rank(b))
            .then_with(|| {
                dist(a)
                    .partial_cmp(&dist(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                build_toolchain_rank(a, needs_build_toolchain)
                    .cmp(&build_toolchain_rank(b, needs_build_toolchain))
            })
            .then_with(|| load_of(&a.name).cmp(&load_of(&b.name)))
            .then_with(|| b.disk_free_gb.cmp(&a.disk_free_gb))
            .then_with(|| a.name.cmp(&b.name))
    });
    cands.into_iter().map(|n| target_of(cloud, n)).collect()
}

/// Choose placement targets (module docs: policy); empty means no target chosen. `is_container`
/// needs a public address; `stateful` (single-writer data in a per-node volume with no sync)
/// constrains an explicit multi-region request to its first region, since fan-out would fork the
/// data. Each `needs_*` is a hard capability: an explicit region is never widened past it, because
/// an empty placement beats a node that fails every cold start. `needs_build_toolchain` is also
/// ranked, so a bun build prefers a bun node.
pub fn place(
    cloud: &Arc<CloudState>,
    regions: &[String],
    is_container: bool,
    stateful: bool,
    needs_gpu: bool,
    needs_interpreter: impl Into<InterpreterNeeds>,
    needs_runtime_artifact: bool,
    needs_build_isolation: bool,
    needs_build_toolchain: Option<&str>,
) -> Vec<Target> {
    let needs_interpreter = needs_interpreter.into();
    let nodes = cloud.registry.nodes();
    let me = cloud.node_name.clone();
    let load = load_map(cloud);
    let load_of = |name: &str| -> usize { load.get(name).copied().unwrap_or(0) };
    let target_of = |n: &NodeInfo| -> Target { target_of(cloud, n) };
    let reachable = |n: &NodeInfo| -> bool { reachable(cloud, n) };
    let capable = |n: &NodeInfo| -> bool {
        capable(
            n,
            is_container,
            needs_gpu,
            needs_interpreter,
            needs_runtime_artifact,
            needs_build_isolation,
            needs_build_toolchain,
        )
    };

    let regions: Vec<String> = regions
        .iter()
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| !r.is_empty())
        .collect();

    if !regions.is_empty() {
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
        let mut targets: Vec<Target> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for region in regions {
            let healthy_in_region: Vec<&NodeInfo> = nodes
                .iter()
                .filter(|n| n.healthy && n.region.eq_ignore_ascii_case(region) && reachable(n))
                .collect();
            if healthy_in_region.is_empty() {
                continue;
            }
            let capable_here: Vec<&NodeInfo> = healthy_in_region
                .iter()
                .copied()
                .filter(|n| capable(n))
                .collect();
            if needs_gpu && capable_here.is_empty() {
                tracing::warn!(region = %region, "placement: no GPU-capable node in this region — not widening (gpu request)");
                continue;
            }
            if needs_interpreter.wasm && capable_here.is_empty() {
                tracing::warn!(region = %region, "placement: no wasm-capable node in this region — not widening (wasmer runtime)");
                continue;
            }
            if needs_interpreter.bun && capable_here.is_empty() {
                tracing::warn!(region = %region, "placement: no bun-capable node in this region — not widening (bun runtime)");
                continue;
            }
            if needs_runtime_artifact && capable_here.is_empty() {
                tracing::warn!(region = %region, "placement: no runtime-artifact-v1 node in this region — not widening (source build)");
                continue;
            }
            if needs_build_isolation && capable_here.is_empty() {
                tracing::warn!(region = %region, "placement: no build-isolation-v1 node in this region — not widening (repository build)");
                continue;
            }
            let mut pool = if capable_here.is_empty() {
                healthy_in_region
            } else {
                capable_here
            };
            pool.sort_by_key(|n| {
                (
                    build_toolchain_rank(n, needs_build_toolchain),
                    load_of(&n.name),
                    std::cmp::Reverse(n.disk_free_gb),
                )
            });
            let chosen = prefer_coordinator(&pool, &me);
            if seen.insert(chosen.name.clone()) {
                targets.push(target_of(chosen));
            }
        }
        return targets;
    }

    let self_geo = coordinator_geo(&nodes, &me);
    let mut elig: Vec<&NodeInfo> = nodes
        .iter()
        .filter(|n| capable(n) && reachable(n))
        .collect();
    if elig.is_empty() {
        if needs_gpu {
            tracing::warn!("placement: gpu requested but no healthy GPU-capable node is reachable");
        }
        if needs_interpreter.wasm {
            tracing::warn!(
                "placement: wasmer runtime requested but no healthy wasm-capable node is reachable"
            );
        }
        if needs_interpreter.bun {
            tracing::warn!(
                "placement: bun runtime requested but no healthy bun-capable node is reachable"
            );
        }
        if needs_runtime_artifact {
            tracing::warn!(
                "placement: source build requested but no healthy runtime-artifact-v1 node is reachable"
            );
        }
        if needs_build_isolation {
            tracing::warn!(
                "placement: repository build requested but no healthy build-isolation-v1 node is reachable"
            );
        }
        return Vec::new();
    }
    let dist = |n: &NodeInfo| -> f64 { distance_km(self_geo, n) };
    elig.sort_by(|a, b| {
        dist(a)
            .partial_cmp(&dist(b))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                build_toolchain_rank(a, needs_build_toolchain)
                    .cmp(&build_toolchain_rank(b, needs_build_toolchain))
            })
            .then_with(|| load_of(&a.name).cmp(&load_of(&b.name)))
    });
    vec![target_of(prefer_coordinator(&elig, &me))]
}

/// Why `place` returned no target for a repository build (the refusal in `git.rs`): every count
/// uses the predicates `place` filters on, so it names the constraint that actually emptied the
/// candidate set.
#[derive(Debug, Default, serde::Serialize)]
pub struct BuildPlacementDiagnosis {
    /// Nodes that can run repository builds at all (BuildExecutor v1, or a host-exec mock/litebox
    /// backend); health and region ignored.
    pub capable_total: usize,
    /// `capable_total` narrowed to the project's configured region(s); every
    /// node when the project configures none.
    pub capable_in_region: usize,
    /// `capable_in_region` that `place` would return (healthy, reachable, through every `capable()`
    /// filter): the figure that explains an empty placement.
    pub placeable_in_region: usize,
    /// One counted reason per other `capable_in_region` node (`"2 unhealthy"`), in a stable order.
    pub blockers: Vec<String>,
}

/// The first `place` predicate `n` fails (checked in `capable`'s order, so the refusal names the
/// constraint that bit), or `None` when `place` would accept it.
fn placement_blocker(
    cloud: &Arc<CloudState>,
    n: &NodeInfo,
    is_container: bool,
    needs_gpu: bool,
    needs_interpreter: InterpreterNeeds,
    needs_runtime_artifact: bool,
    needs_build_toolchain: Option<&str>,
) -> Option<String> {
    if !n.healthy {
        return Some("unhealthy (its mesh health probe is failing)".to_string());
    }
    if !reachable(cloud, n) {
        if n.peer_id.is_none() || n.iroh_addr.is_none() {
            return Some(
                "unreachable from this coordinator (no admin URL and no mesh address)".to_string(),
            );
        }
        return Some(
            "advertises no dialable path (no public address, no relay, and no live mesh connection)".to_string(),
        );
    }
    if needs_gpu && n.gpu_count == 0 {
        return Some("no serverless GPU".to_string());
    }
    if !wasm_capable(n, needs_interpreter.wasm) {
        return Some("no wasmer runtime".to_string());
    }
    if !bun_capable(n, needs_interpreter.bun) {
        return Some("no bun runtime".to_string());
    }
    if !runtime_artifact_capable(n, needs_runtime_artifact) {
        return Some("no runtime-artifact v1".to_string());
    }
    if let Some(manager) = needs_build_toolchain {
        if !build_toolchain_capable(n, needs_build_toolchain) {
            return Some(format!(
                "cannot run a `{manager}` build (neither that package manager nor a substitute)"
            ));
        }
    }
    let floor_gb = disk_floor_gb();
    if below_disk_floor(n, floor_gb) {
        return Some(format!(
            "below the {floor_gb} GiB placement disk floor ({} GiB free)",
            n.disk_free_gb
        ));
    }
    if is_container && !has_public_address(n) {
        return Some(
            "no public address (a container is served from the node's own host)".to_string(),
        );
    }
    if !eligible(n) && n.backend != "mock" {
        if n.mem_total_mb < MEM_FLOOR_MB {
            return Some(format!(
                "below the {} MiB placement memory floor ({} MiB)",
                MEM_FLOOR_MB, n.mem_total_mb
            ));
        }
        return Some(format!(
            "backend \"{}\" is neither an isolation backend nor a host-exec mock node",
            n.backend
        ));
    }
    None
}

/// Diagnose an empty repository-build placement: how many nodes can run repository builds, how many
/// are in the requested region(s), and how many of those `place` would actually return. Every
/// counted node already passes [`build_isolation_capable`], so `needs_build_isolation` is implied.
pub fn build_placement_diagnosis(
    cloud: &Arc<CloudState>,
    regions: &[String],
    is_container: bool,
    needs_gpu: bool,
    needs_interpreter: impl Into<InterpreterNeeds>,
    needs_runtime_artifact: bool,
    needs_build_toolchain: Option<&str>,
) -> BuildPlacementDiagnosis {
    let needs_interpreter = needs_interpreter.into();
    let nodes = cloud.registry.nodes();
    let regions: Vec<String> = regions
        .iter()
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| !r.is_empty())
        .collect();
    let mut diagnosis = BuildPlacementDiagnosis::default();
    let mut blockers: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for n in nodes.iter() {
        if !build_isolation_capable(n, true) {
            continue;
        }
        diagnosis.capable_total += 1;
        if !regions.is_empty() && !regions.iter().any(|r| r.eq_ignore_ascii_case(&n.region)) {
            continue;
        }
        diagnosis.capable_in_region += 1;
        match placement_blocker(
            cloud,
            n,
            is_container,
            needs_gpu,
            needs_interpreter,
            needs_runtime_artifact,
            needs_build_toolchain,
        ) {
            None => diagnosis.placeable_in_region += 1,
            Some(reason) => *blockers.entry(reason).or_insert(0) += 1,
        }
    }
    diagnosis.blockers = blockers
        .into_iter()
        .map(|(reason, count)| format!("{count} {reason}"))
        .collect();
    diagnosis
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
            // These placement fixtures are all non-Wasmer/non-Bun, so `None` —
            // the value a node that never ran the probe reports — is the
            // honest one, and it keeps them on the not-capable path. A test
            // that means "this node CAN run wasm/bun" must set `Some(true)`
            // explicitly, the same rule `disk_free_gb` already carries.
            wasm_runtime: None,
            bun_runtime: None,
            build_toolchains: None,
            runtime_artifact_protocol: None,
            build_isolation_protocol: None,
            artifact_transfer_protocol: None,
            musql_engine: None,
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
            unclean_restarts_24h: 0,
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
