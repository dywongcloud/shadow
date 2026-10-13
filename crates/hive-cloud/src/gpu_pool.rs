//! Serverless GPU pool abstraction: groups every healthy node with `gpu_count > 0` into a named
//! pool BY REGION (the region already on `NodeInfo`; no new grouping key) and reports aggregate and
//! per-node free VRAM. `NodeInfo::gpu_vram_mb` is the boot-time `nvidia-smi` TOTAL, never current
//! availability, so free VRAM is derived from what is held right now.
//!
//! ## Allocate/release discipline
//!
//! Rather than bolt on a second, easily desynced reservation counter, this reuses the
//! allocate/release accounting the platform already keeps for GPU-holding instances:
//! `fluid_compute::Fluid`'s per-node instance registry. A GPU-requesting function pool's
//! `instances` count (`FunctionStats::instances`, gated by `FunctionStats::gpu`) goes up on a
//! successful cold start and down the moment that instance is torn down (idle-drained, crash-reaped
//! or recycled), the same live gauge that drives GPU wall-time billing (`main.rs`'s metering loop).
//! A deployment holding a GPU slot therefore already decrements the pool's free count and a
//! torn-down one gives it back; this module reads that counter instead of re-implementing it.
//!
//! It mirrors two existing disciplines: the podman lock pool (a FIXED per-host pool, decremented on
//! allocate, given back on release, where a missed release leaks the whole host's capacity) and
//! `schedule::load_map` (a live per-node count built from this node's own state plus every peer's
//! gossiped or fetched state, never one node's parochial view).
//!
//! Until per-instance fractional GPU binding exists (a launch grants a container ALL of a host's
//! GPUs via CDI `nvidia.com/gpu=all`), one live GPU instance is modeled as holding one whole GPU's
//! worth of VRAM (`gpu_vram_mb / gpu_count`), with the reservation capped at the node's total so an
//! oversubscribed node reports 0 free rather than underflowing.
//!
//! ## Node-local state, fleet-wide read
//!
//! Live instance counts are NODE-LOCAL in-process state (`Fluid`'s registry) while the public
//! `api.<domain>` and dashboard hosts are round-robin DNS across every node, so a GET trusting only
//! its own `c.fluid.stats()` would under-report every OTHER node's reservations depending on which
//! node answered. [`snapshot`] therefore fans out to every GPU-capable peer for its live count, as
//! `admin::fleet_function_stats` does for `/v1/functions` (the same `fetch_from_host`, hitting the
//! unfiltered `/v1/functions?local=true` internal view). Pool CAPACITY (`vram_total_mb`) needs no
//! fan-out: like `admin::resources_get`, it is read off `NodeRegistry::nodes()`, already fleet-wide
//! through the `/v1/nodes` gossip.

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
    /// Measured RTT from the SERVING node to this member (registry health probes; 0 = self). Region
    /// does not imply locality: two nodes of one region can sit in different datacenters, which
    /// once turned an intra-DC pooled-model load into hours, and surfacing the real link cost here
    /// lets an operator see that before a pool does.
    pub latency_ms: u64,
}

#[derive(Clone, Serialize)]
pub struct GpuPoolRegion {
    pub region: String,
    pub nodes: Vec<GpuPoolNode>,
    pub pool_vram_total_mb: u64,
    pub pool_vram_free_mb: u64,
}

/// VRAM one live GPU-holding instance reserves on `n`: the node's total VRAM apportioned evenly
/// across its physical GPUs (see the module doc for why this, not per-instance metadata, which does
/// not exist yet, is the unit).
fn per_instance_reserve_mb(n: &hive_edge::NodeInfo) -> u64 {
    if n.gpu_count == 0 {
        return 0;
    }
    n.gpu_vram_mb / n.gpu_count as u64
}

/// Sum of `instances` across every `gpu: true` function pool in `stats` — the
/// live count of GPU-holding instances the reporting node currently has warm.
fn live_gpu_instances(stats: &[fluid_compute::FunctionStats]) -> u64 {
    stats
        .iter()
        .filter(|s| s.gpu)
        .map(|s| s.instances as u64)
        .sum()
}

/// Free VRAM on a node: the MINIMUM of what the DRIVER reports (`NodeInfo::gpu_free_mb`) and what
/// the instance count implies. The reservation estimate drifts from the hardware in both directions
/// (phantom instances reserve VRAM the card shows free, and an inference endpoint is not a
/// serverless function instance, so its allocation is never counted), so fit decisions made on it
/// alone, such as whether a model needs `--rpc` members at all, matched neither the hardware nor
/// the workload. The measurement alone is not enough either: a reservation the driver has not yet
/// materialised (an instance mid cold-start, before CUDA has allocated) is real pending demand, and
/// trusting the momentarily empty card would let two workloads both be told they fit. `None` (no
/// probe, or a pre-upgrade peer) falls back to the estimate, so a part-rolled fleet behaves as
/// before.
fn free_vram_mb(measured: Option<u64>, estimated: u64) -> u64 {
    match measured {
        Some(measured) => measured.min(estimated),
        None => estimated,
    }
}

/// Builds the current GPU pool snapshot, one entry per region with at least one healthy GPU node.
/// This node's live count comes from its in-process pool; every other GPU node is fanned out to
/// individually, deliberately NOT merged into one fleet-wide total as `fleet_function_stats` does,
/// because free VRAM needs the PER-NODE breakdown. See the module doc for the fan-out and
/// allocate/release rationale.
pub async fn snapshot(cloud: &Arc<CloudState>) -> Vec<GpuPoolRegion> {
    let gpu_nodes: Vec<hive_edge::NodeInfo> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.healthy && n.gpu_count > 0)
        .collect();

    let mut live_by_node: HashMap<String, u64> = HashMap::new();
    live_by_node.insert(
        cloud.node_name.clone(),
        live_gpu_instances(&cloud.fluid.stats()),
    );
    let peer_names: Vec<String> = gpu_nodes
        .iter()
        .filter(|n| n.name != cloud.node_name)
        .map(|n| n.name.clone())
        .collect();
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
        let reserved_mb = per_instance_reserve_mb(n)
            .saturating_mul(live)
            .min(n.gpu_vram_mb);
        let estimated_free_mb = n.gpu_vram_mb.saturating_sub(reserved_mb);
        let vram_free_mb = free_vram_mb(n.gpu_free_mb, estimated_free_mb);
        by_region
            .entry(n.region.clone())
            .or_default()
            .push(GpuPoolNode {
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
            GpuPoolRegion {
                region,
                nodes,
                pool_vram_total_mb,
                pool_vram_free_mb,
            }
        })
        .collect();
    regions.sort_by(|a, b| a.region.cmp(&b.region));
    regions
}
