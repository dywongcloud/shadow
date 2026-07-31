//! Serverless GPU pool abstraction — the foundation for GPU pooling.
//!
//! Groups every healthy node with `gpu_count > 0` into a named pool BY REGION
//! (region already exists on `NodeInfo`; no new grouping key — currently
//! `"san-jose"` covers all 5 GPU/CVM nodes). Reports aggregate + per-node free
//! VRAM: `NodeInfo::gpu_vram_mb` is the boot-time `nvidia-smi` TOTAL (see its
//! own doc comment), never current availability, so free VRAM has to be
//! derived from what's actually held right now.
//!
//! ## Allocate/release discipline
//!
//! Rather than bolt on a second, easily-desynced reservation counter, this
//! reuses the SAME allocate/release accounting the platform already
//! maintains for GPU-holding instances: `fluid_compute::Fluid`'s per-node
//! instance registry. A GPU-requesting function pool's `instances` count
//! (`FunctionStats::instances`, gated by `FunctionStats::gpu`) is incremented
//! on a successful cold start and decremented the moment that instance is
//! torn down (idle-drained, crash-reaped, or recycled) — the exact same live
//! gauge that already drives GPU wall-time billing (`main.rs`'s metering
//! loop: `if s.gpu { t.gpu_ms += s.fluid_ms }`). "A deployment holding a GPU
//! slot decrements the pool's free count, a torn-down one gives it back" is
//! therefore already true of that counter; this module reads it rather than
//! re-implementing it, the same way `admin::resources_get` sums `NodeInfo`'s
//! already-gossiped static capacity instead of re-probing every node.
//!
//! This mirrors two existing disciplines directly, per AGENTS.md:
//!   * the podman lock pool ("Containers: the podman lock pool") — a FIXED
//!     per-host pool, decremented on allocate, given back on release, and a
//!     missed release leaks the whole host's capacity;
//!   * `schedule::load_map` — a live per-node count built from THIS node's
//!     own local state plus every peer's gossiped/fetched state, never a
//!     single node's parochial view.
//!
//! Until per-instance fractional GPU binding exists (today's launch grants a
//! container ALL of a host's GPUs via CDI `nvidia.com/gpu=all` — see
//! AGENTS.md "Serverless GPU"), one live GPU instance is modeled as holding
//! one whole GPU's worth of VRAM (`gpu_vram_mb / gpu_count`), reserved-mb
//! capped at the node's total so an oversubscribed node reports 0 free
//! rather than underflowing.
//!
//! ## Node-local state, fleet-wide read
//!
//! Per AGENTS.md's round-robin doc: live instance counts are NODE-LOCAL
//! in-process state (`Fluid`'s registry), and the public `api.<domain>` /
//! dashboard hosts are round-robin DNS across every node — so a GET that
//! trusted only its own `c.fluid.stats()` would silently under-report every
//! OTHER node's reservations depending on which node answered the request.
//! `snapshot` instead fans out to every GPU-capable peer for its live count,
//! exactly like `admin::fleet_function_stats` does for `/v1/functions` (the
//! same `fetch_from_host` used there, hitting the SAME unauthenticated-by-
//! team, fully-unfiltered `/v1/functions?local=true` internal view). Pool
//! CAPACITY (`vram_total_mb`) needs no fan-out at all — like
//! `admin::resources_get`, it's read straight off `NodeRegistry::nodes()`,
//! which is already fleet-wide via the existing `/v1/nodes` gossip.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::state::CloudState;

#[derive(Clone, Serialize)]
pub struct GpuPoolNode {
    pub name: String,
    pub gpu_model: Option<String>,
    pub vram_total_mb: u64,
    pub vram_free_mb: u64,
    /// Measured RTT from the SERVING node to this member (registry health
    /// probes; 0 = self). Region no longer implies locality — two "san-jose"
    /// CVM nodes turned out to live in Ashburn, 65ms from the rest of their
    /// region, which turned an intra-DC 11GB pooled-model load into ~2 hours.
    /// Surfacing the real link cost here is what lets an operator see that
    /// before a pool does.
    pub latency_ms: u64,
}

#[derive(Clone, Serialize)]
pub struct GpuPoolRegion {
    pub region: String,
    pub nodes: Vec<GpuPoolNode>,
    pub pool_vram_total_mb: u64,
    pub pool_vram_free_mb: u64,
}

/// VRAM one live GPU-holding instance reserves on `n`: the node's total VRAM
/// apportioned evenly across its physical GPUs. See module doc for why this
/// (not per-instance metadata, which doesn't exist yet) is the right unit.
fn per_instance_reserve_mb(n: &hive_edge::NodeInfo) -> u64 {
    if n.gpu_count == 0 {
        return 0;
    }
    n.gpu_vram_mb / n.gpu_count as u64
}

/// Sum of `instances` across every `gpu: true` function pool in `stats` — the
/// live count of GPU-holding instances the reporting node currently has warm.
fn live_gpu_instances(stats: &[fluid_compute::FunctionStats]) -> u64 {
    stats.iter().filter(|s| s.gpu).map(|s| s.instances as u64).sum()
}

/// Build the current GPU pool snapshot, one entry per region that has at
/// least one healthy GPU node. See module doc for the fan-out + allocate/
/// release rationale.
pub async fn snapshot(cloud: &Arc<CloudState>) -> Vec<GpuPoolRegion> {
    let gpu_nodes: Vec<hive_edge::NodeInfo> =
        cloud.registry.nodes().into_iter().filter(|n| n.healthy && n.gpu_count > 0).collect();

    // Per-node live GPU-instance count: self from the in-process pool
    // directly (no network hop needed for our own state), every other GPU
    // node fanned out to individually — deliberately NOT merged into one
    // fleet-wide total like `fleet_function_stats` does, because free VRAM
    // needs the PER-NODE breakdown, not just a sum.
    let mut live_by_node: HashMap<String, u64> = HashMap::new();
    live_by_node.insert(cloud.node_name.clone(), live_gpu_instances(&cloud.fluid.stats()));
    let peer_names: Vec<String> =
        gpu_nodes.iter().filter(|n| n.name != cloud.node_name).map(|n| n.name.clone()).collect();
    let fetched = futures::future::join_all(peer_names.iter().map(|name| async move {
        let v = crate::admin::fetch_from_host(cloud, name, "/v1/functions?local=true", "").await;
        (name.clone(), v)
    }))
    .await;
    for (name, v) in fetched {
        let count = v
            .and_then(|v| serde_json::from_value::<Vec<fluid_compute::FunctionStats>>(v).ok())
            .map(|stats| live_gpu_instances(&stats))
            .unwrap_or(0);
        live_by_node.insert(name, count);
    }

    let mut by_region: HashMap<String, Vec<GpuPoolNode>> = HashMap::new();
    for n in &gpu_nodes {
        let live = live_by_node.get(&n.name).copied().unwrap_or(0);
        let reserved_mb = per_instance_reserve_mb(n).saturating_mul(live).min(n.gpu_vram_mb);
        let estimated_free_mb = n.gpu_vram_mb.saturating_sub(reserved_mb);
        // Prefer what the DRIVER reports over what the instance count implies.
        //
        // The reservation estimate drifts from the hardware in both directions,
        // measured live 2026-07-31: fc-sanjose-gpu-1 was reported with 30 GiB
        // reserved (2 phantom instances) while nvidia-smi showed 105 MiB per
        // card actually in use, and fc-sanjose-gpu-2's real llama-server
        // allocation was invisible entirely, because an inference endpoint is
        // not a serverless function instance and so is never counted. Fit
        // decisions — whether a model needs `--rpc` members at all — were being
        // made against a number matching neither the hardware nor the workload.
        //
        // Take the MINIMUM of the two rather than the measurement alone: a
        // reservation the driver has not yet materialised (an instance mid
        // cold-start, before CUDA has allocated) is real pending demand, and
        // trusting the momentarily-empty card would let two workloads both be
        // told they fit. `None` (no probe / pre-upgrade peer) falls back to the
        // estimate unchanged, so a part-rolled fleet behaves exactly as before.
        let vram_free_mb = match n.gpu_free_mb {
            Some(measured) => measured.min(estimated_free_mb),
            None => estimated_free_mb,
        };
        by_region.entry(n.region.clone()).or_default().push(GpuPoolNode {
            name: n.name.clone(),
            gpu_model: n.gpu_model.clone(),
            vram_total_mb: n.gpu_vram_mb,
            vram_free_mb,
            latency_ms: if n.is_self { 0 } else { n.latency_ms },
        });
    }

    let mut regions: Vec<GpuPoolRegion> = by_region
        .into_iter()
        .map(|(region, mut nodes)| {
            nodes.sort_by(|a, b| a.name.cmp(&b.name));
            let pool_vram_total_mb = nodes.iter().map(|n| n.vram_total_mb).sum();
            let pool_vram_free_mb = nodes.iter().map(|n| n.vram_free_mb).sum();
            GpuPoolRegion { region, nodes, pool_vram_total_mb, pool_vram_free_mb }
        })
        .collect();
    regions.sort_by(|a, b| a.region.cmp(&b.region));
    regions
}
