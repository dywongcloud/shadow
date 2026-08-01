//! Public-port allocation for raw TCP/UDP/gRPC deployments.
//!
//! HTTP-family services are reached through the shared 80/443 gateway (Host
//! routing); before this module the ONLY raw public ports the platform bound
//! were the two hardcoded DB-gateway listeners (`db_gateway.rs`, :5432/:6379).
//! A deployment speaking a raw protocol (Postgres wire, Minecraft, gRPC) has no
//! Host header to route on — each such service needs its OWN public port a
//! client can connect to. This module owns that port space:
//!
//! * **Range** — `HIVE_RAW_PORT_RANGE` (default `20000-29999`). The range
//!   should be reserved for the platform on every node (firewall/lockdown
//!   scripts open it, nothing else binds inside it); a port that something
//!   else DOES hold is defensively skipped by a bind probe before claiming.
//! * **Unit of allocation** — one public port per raw-protocol [`PortSpec`] a
//!   deployment declares (`FunctionConfig::ports`), so a multi-port service
//!   (Minecraft game+rcon+query) gets one public port per spec. A function
//!   with an EMPTY `ports` list gets nothing even when its own protocol is
//!   raw: an un-exposed compose sidecar (private Postgres) deliberately has no
//!   spec (see `build_compose_manifest`'s `x-shadw-expose` opt-in), while the
//!   single-container paths always bridge their real port into `ports`
//!   (`container_manifest` via `PortSpec::from_legacy_port`).
//! * **Stability across redeploys** — the claim is keyed by (project,
//!   function, container_port, protocol), NOT by deployment id, so a redeploy
//!   (a NEW deployment record of the same project/service) is re-stamped with
//!   the SAME public port — mirroring how the redeploy path already preserves
//!   the port/protocol spec itself (`image_port_spec_for_project_fleet`).
//! * **Race safety, FLEET-wide** — the mutex below only makes ONE node's
//!   claims atomic against itself; the port space is fleet-global (every
//!   node binds every allocation, `raw_proxy`/`udp_relay`), so a node-local
//!   claim alone is not enough — two parallel first-deploys landing on
//!   DIFFERENT nodes both scanning from an empty local registry could both
//!   claim the same low port for two different tenants. The actual claim
//!   DECISION is therefore leader-coordinated: [`allocate_raw_ports_coordinated`]
//!   (the deploy-time entry point every call site uses) runs the claim
//!   locally only when this node IS the control-plane leader
//!   (`CloudState::is_control_plane_leader`); otherwise it forwards the
//!   manifest to the leader over the mesh (`POST /v1/raw-ports/allocate`,
//!   `gossip::dispatch`) and adopts the leader's answer into this node's
//!   registry as a read cache. This mirrors the SAME single-writer-forward
//!   pattern `admin_ingress` already uses for admin mutations, billing
//!   metering and ACME/DNS — one designated writer, every other node a
//!   cache. The single-node local claim path ([`allocate_raw_ports`]) stays
//!   the correct primitive FOR the leader (and for the RPC handler that runs
//!   on it): every new claim in a batch is inserted under ONE lock hold and
//!   durably saved before any port escapes (no check-then-allocate TOCTOU
//!   window on that one writer), and a node restart restores every claim.
//!   The stamped [`PortSpec::public_port`] on the deployment record is a
//!   second durable copy — boot restore re-adopts those stamps
//!   ([`adopt_record`]) so a lost registry file self-heals from the records.
//! * **Release** — [`release_raw_ports`] returns a project's ports to the
//!   pool; hooked into project deletion (`purge_project_resources`) and
//!   last-deployment deletion. A RELOCATION (the scheduler moving a
//!   project's serving node) ALSO releases on the OLD host: `git.rs`'s
//!   `cleanup_non_targets` broadcasts a single-hop `dispatch_project_delete`
//!   to every non-target node, and the receiving `project_delete` handler
//!   calls `purge_project_resources` → `release_raw_ports` unconditionally
//!   of its `cascade=false` query param (that param only suppresses a
//!   FURTHER re-broadcast, never the local teardown) — this comment
//!   previously claimed the opposite and was wrong. Release is
//!   leader-coordinated the same way allocation is
//!   ([`release_raw_ports_coordinated`]): a non-leader host still clears its
//!   own local cache immediately (so its `lookup` never serves a stale
//!   claim) AND forwards the release to the leader's authoritative registry,
//!   so a relocation's old-host release and new-host (re)allocation are
//!   never split across two different un-synced local registries.
//!
//! The LISTENER on an allocated port is the generic raw proxy's job
//! (`crate::raw_proxy`), not this module's — this is pure allocation/
//! persistence. Consumers: [`allocate_raw_ports`] (deploy time),
//! [`release_raw_ports`] (deletion), [`lookup`] (the proxy layer's
//! most-authoritative port→allocation source); the fleet-wide read side is
//! the stamped bindings themselves, surfaced as
//! `fluid_core::DeploymentInfo::raw_ports` via
//! `fluid_core::Manifest::raw_port_bindings` and gossiped with the
//! deployment list.

use fluid_core::Manifest;
use fluid_core::ServiceProtocol;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::state::CloudState;

/// Default public range for raw services — clear of well-known ports, the
/// 30000+ NodePort-style space, and the ephemeral range on most kernels.
const DEFAULT_RANGE: (u16, u16) = (20_000, 29_999);

/// One durable public-port claim. `protocol` serializes as its kebab-case wire
/// string ("tcp"/"udp"/"grpc") — same lenient-on-read semantics as everywhere
/// else the enum is stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawPortAllocation {
    pub project: String,
    pub function: String,
    pub container_port: u16,
    pub protocol: ServiceProtocol,
    pub public_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub allocated_ms: u64,
}

/// On-disk shape of `$HIVE_DATA/raw_ports.json`.
#[derive(Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    allocs: Vec<RawPortAllocation>,
}

struct Registry {
    /// claim key ([`alloc_key`]) → allocation. BTreeMap for deterministic
    /// file output.
    by_key: BTreeMap<String, RawPortAllocation>,
    /// Every public port currently claimed (derived from `by_key`, kept in
    /// lockstep) — the O(log n) membership set the claim scan checks.
    in_use: BTreeSet<u16>,
    /// Rotating scan offset into the range so successive claims don't all
    /// re-probe from the bottom (and a just-released port isn't instantly
    /// re-handed to the next unrelated deploy while old clients still dial it).
    cursor: u32,
}

/// The stable claim identity: same key ⇒ same public port, across redeploys.
/// `\u{1f}` (ASCII unit separator) cannot appear in project/function names, so
/// the key is collision-free without escaping.
fn alloc_key(
    project: &str,
    function: &str,
    container_port: u16,
    protocol: ServiceProtocol,
) -> String {
    format!(
        "{project}\u{1f}{function}\u{1f}{container_port}\u{1f}{}",
        protocol.as_str()
    )
}

fn file_path() -> PathBuf {
    crate::persist::data_dir().join("raw_ports.json")
}

/// The configured public range (inclusive), from `HIVE_RAW_PORT_RANGE`
/// ("lo-hi"), defaulting to 20000-29999. A malformed value warns and defaults
/// rather than silently disabling raw ingress.
fn port_range() -> (u16, u16) {
    match std::env::var("HIVE_RAW_PORT_RANGE") {
        Ok(raw) if !raw.trim().is_empty() => parse_range(&raw).unwrap_or_else(|| {
            tracing::warn!(
                value = %raw,
                "HIVE_RAW_PORT_RANGE is not a valid \"lo-hi\" port range; using the default {}-{}",
                DEFAULT_RANGE.0,
                DEFAULT_RANGE.1
            );
            DEFAULT_RANGE
        }),
        _ => DEFAULT_RANGE,
    }
}

fn parse_range(s: &str) -> Option<(u16, u16)> {
    let (a, b) = s.trim().split_once('-')?;
    let lo: u16 = a.trim().parse().ok()?;
    let hi: u16 = b.trim().parse().ok()?;
    // Port 0 is "kernel picks" and 1-1023 are privileged/well-known — neither
    // is a sane raw-ingress range; reject so the default kicks in loudly.
    if lo < 1024 || hi < lo {
        return None;
    }
    Some((lo, hi))
}

/// The process-wide registry, loaded from disk exactly once. All mutation goes
/// through this single mutex — that lock IS the atomic-claim mechanism.
fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut by_key: BTreeMap<String, RawPortAllocation> = BTreeMap::new();
        let mut in_use: BTreeSet<u16> = BTreeSet::new();
        match std::fs::read_to_string(file_path()) {
            Ok(s) => match serde_json::from_str::<RegistryFile>(&s) {
                Ok(f) => {
                    for a in f.allocs {
                        let key = alloc_key(&a.project, &a.function, a.container_port, a.protocol);
                        // A (corrupt) duplicate port must never load as two
                        // live claims — first entry wins, the rest are dropped
                        // loudly and will re-allocate on their next deploy.
                        if in_use.insert(a.public_port) {
                            by_key.insert(key, a);
                        } else {
                            tracing::warn!(project = %a.project, port = a.public_port, "raw-port registry: dropped duplicate port claim on load");
                        }
                    }
                }
                Err(e) => {
                    // Unreadable file ⇒ start empty; the stamped deployment
                    // records re-adopt their claims at restore (`adopt_record`),
                    // so established services keep their ports.
                    tracing::warn!(error = %e, "raw-port registry unreadable; starting empty (deployment-record stamps re-adopt at restore)");
                }
            },
            Err(_) => {} // no file yet — first boot
        }
        Mutex::new(Registry { by_key, in_use, cursor: 0 })
    })
}

fn lock() -> MutexGuard<'static, Registry> {
    // A poisoned lock only means another thread panicked mid-claim; the data
    // is a plain map + set that is never left half-updated across an unwind
    // point we control (claims roll back on error) — recover rather than
    // wedging every future deploy.
    registry().lock().unwrap_or_else(|p| p.into_inner())
}

/// Durably write the registry (temp + fsync + rename, mirroring
/// `persist::save`'s discipline). Called while the claim lock is held so the
/// on-disk set can never lag behind a port that has already escaped.
fn save(reg: &Registry) -> std::io::Result<()> {
    let dir = crate::persist::data_dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join("raw_ports.json.tmp");
    let file = RegistryFile {
        allocs: reg.by_key.values().cloned().collect(),
    };
    let json = serde_json::to_string_pretty(&file).unwrap_or_else(|_| "{}".into());
    {
        let f = std::fs::File::create(&tmp)?;
        use std::io::Write;
        let mut w = std::io::BufWriter::new(&f);
        w.write_all(json.as_bytes())?;
        w.flush()?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, file_path())
}

/// Defensive probe: can THIS node still bind the candidate on both families?
/// The range is supposed to be reserved, but anything squatting a port
/// (leftover process, manual admin binding) must be skipped rather than handed
/// to a deployment whose proxy listener would then fail. Probing both TCP and
/// UDP keeps the claim protocol-agnostic (the same port number is claimed for
/// whichever family the spec speaks; the pool is one shared number space).
/// The probe sockets drop immediately — the real listener is the raw proxy's.
fn probe_bindable(port: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
        && std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok()
}

/// Find the next claimable port. Caller holds the registry lock and inserts
/// the claim itself (this only scans; `in_use` is the claim set).
fn next_free(reg: &mut Registry) -> anyhow::Result<u16> {
    let (lo, hi) = port_range();
    let span = (hi as u32) - (lo as u32) + 1;
    for i in 0..span {
        let cand = (lo as u32 + (reg.cursor.wrapping_add(i)) % span) as u16;
        if reg.in_use.contains(&cand) {
            continue;
        }
        if !probe_bindable(cand) {
            tracing::debug!(port = cand, "raw-port allocator: skipping unbindable port (something else holds it on this node)");
            continue;
        }
        reg.cursor = ((cand as u32 - lo as u32) + 1) % span;
        return Ok(cand);
    }
    anyhow::bail!(
        "raw public-port range {lo}-{hi} is exhausted (or nothing in it is bindable) — \
         widen HIVE_RAW_PORT_RANGE or delete unused raw TCP/UDP deployments"
    )
}

/// (function idx, port idx) of every spec in `manifest` that needs public raw
/// ingress, in manifest order. Pure and deterministic over `manifest`'s
/// content alone — the SAME computation run independently on the calling
/// node (to know whether an RPC is even needed) and on the control-plane
/// leader (inside the RPC handler) always agrees on target count and order,
/// which is what lets [`allocate_raw_ports_coordinated`] line up the
/// leader's per-target response against its own `manifest` without shipping
/// the whole mutated manifest back over the wire.
fn raw_targets(manifest: &Manifest) -> Vec<(usize, usize)> {
    manifest
        .functions
        .iter()
        .enumerate()
        .flat_map(|(fi, f)| {
            f.ports
                .iter()
                .enumerate()
                .filter(|(_, spec)| spec.protocol.needs_raw_proxy())
                .map(move |(pi, _)| (fi, pi))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Claim/reuse a public port for every `targets` entry against THIS node's
/// LOCAL registry, durably saved before returning. Only correct to call on a
/// node acting as the allocation's single writer (the control-plane leader,
/// via [`allocate_raw_ports_coordinated`], or the RPC handler running on it)
/// — calling this on a non-leader races that peer's own local registry
/// against every OTHER node doing the same, which is exactly the fleet-global
/// race [`allocate_raw_ports_coordinated`] exists to prevent.
///
/// Atomic per call: every new claim in the batch is inserted under ONE lock
/// hold and durably saved before any port escapes; if the range exhausts or
/// the save fails mid-batch, every claim made by THIS call is rolled back and
/// an error is returned — no partial multi-port deployment, no unclaimed port
/// ever handed out.
fn claim_local(
    project: &str,
    manifest: &Manifest,
    targets: &[(usize, usize)],
) -> anyhow::Result<Vec<RawPortAllocation>> {
    let mut allocs: Vec<RawPortAllocation> = Vec::with_capacity(targets.len());
    let mut reg = lock();
    // Claims inserted by THIS call — the rollback set on any failure.
    let mut newly: Vec<(String, u16)> = Vec::new();
    let rollback = |reg: &mut Registry, newly: &[(String, u16)]| {
        for (k, p) in newly {
            reg.by_key.remove(k);
            reg.in_use.remove(p);
        }
    };
    for &(fi, pi) in targets {
        let f = &manifest.functions[fi];
        let spec = &f.ports[pi];
        let key = alloc_key(project, &f.name, spec.container_port, spec.protocol);
        let alloc = match reg.by_key.get(&key) {
            // Redeploy of a known service: the SAME public port, no probe —
            // the service's own live listener legitimately holds it.
            Some(existing) => existing.clone(),
            None => {
                let p = match next_free(&mut reg) {
                    Ok(p) => p,
                    Err(e) => {
                        rollback(&mut reg, &newly);
                        return Err(e);
                    }
                };
                let a = RawPortAllocation {
                    project: project.to_string(),
                    function: f.name.clone(),
                    container_port: spec.container_port,
                    protocol: spec.protocol,
                    public_port: p,
                    label: spec.label.clone(),
                    allocated_ms: hive_core::now_ms(),
                };
                reg.in_use.insert(p);
                reg.by_key.insert(key.clone(), a.clone());
                newly.push((key, p));
                a
            }
        };
        allocs.push(alloc);
    }
    // Durability BEFORE any new port escapes this function: a claim that
    // isn't on disk could be re-handed to a parallel deploy after a crash.
    if !newly.is_empty() {
        if let Err(e) = save(&reg) {
            rollback(&mut reg, &newly);
            anyhow::bail!("could not persist raw-port claim ({e}) — refusing to hand out unclaimed public ports");
        }
    }
    Ok(allocs)
}

/// Stamp `targets[n]`'s `public_port` from `allocs[n]` — the two slices must
/// be the same length and in the same order (always true: both are produced
/// from the same [`raw_targets`] computation).
fn stamp(manifest: &mut Manifest, targets: &[(usize, usize)], allocs: &[RawPortAllocation]) {
    for (n, &(fi, pi)) in targets.iter().enumerate() {
        manifest.functions[fi].ports[pi].public_port = Some(allocs[n].public_port);
    }
}

/// Deploy-time entry point (LEADER-COORDINATED): the actual claim decision
/// always runs on the control-plane leader, closing the fleet-global port
/// race a node-local claim can't see (two different nodes' local registries
/// both starting empty at the range floor for two different tenants' first
/// deploys). This node's own registry is then updated as a READ CACHE from
/// the leader's answer ([`adopt_allocations`]) so `lookup`/`udp_allocations`
/// on THIS node see the claim immediately, without waiting for the next
/// gossiped deployment-list round. Mirrors the `admin_ingress` forward
/// pattern already used for admin mutations/billing/ACME/DNS: one designated
/// writer, every other node a cache.
///
/// A deployment relocating to a different node re-resolves the SAME
/// coordinated port (the leader's registry hit on the stable `alloc_key`),
/// instead of a fresh local allocation — the host-move port-instability this
/// fixes as a side effect of fixing the race (see module doc).
pub async fn allocate_raw_ports_coordinated(
    cloud: &Arc<CloudState>,
    project: &str,
    manifest: &mut Manifest,
) -> anyhow::Result<Vec<u16>> {
    let targets = raw_targets(manifest);
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    if cloud.is_control_plane_leader() {
        let allocs = claim_local(project, manifest, &targets)?;
        let ports = allocs.iter().map(|a| a.public_port).collect();
        stamp(manifest, &targets, &allocs);
        return Ok(ports);
    }
    let leader = cloud.control_plane_leader();
    let peer = cloud.registry.nodes().into_iter().find(|n| {
        n.name == leader && !n.is_self && n.healthy && n.peer_id.is_some() && n.iroh_addr.is_some()
    });
    let Some(peer) = peer else {
        anyhow::bail!(
            "raw-port allocation: control-plane leader ({leader}) has no reachable mesh address"
        );
    };
    let (peer_id, peer_addr) = (
        peer.peer_id.clone().unwrap(),
        peer.iroh_addr.clone().unwrap(),
    );
    let req_body =
        serde_json::to_vec(&serde_json::json!({ "project": project, "manifest": manifest }))
            .unwrap_or_default();
    let resp = crate::gossip::request_to(
        cloud,
        &peer_id,
        &peer_addr,
        hive_p2p::GOSSIP_POST,
        "/v1/raw-ports/allocate",
        &req_body,
        15,
    )
    .await
    .ok_or_else(|| {
        anyhow::anyhow!("raw-port allocation: control-plane leader ({leader}) did not respond")
    })?;
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        allocs: Vec<RawPortAllocation>,
        error: Option<String>,
    }
    let parsed: Resp = serde_json::from_slice(&resp).map_err(|e| {
        anyhow::anyhow!("raw-port allocation: malformed response from leader ({leader}): {e}")
    })?;
    if let Some(err) = parsed.error {
        anyhow::bail!("raw-port allocation refused by leader ({leader}): {err}");
    }
    if parsed.allocs.len() != targets.len() {
        anyhow::bail!(
            "raw-port allocation: leader ({leader}) returned {} allocation(s) for {} requested target(s)",
            parsed.allocs.len(),
            targets.len()
        );
    }
    adopt_allocations(&parsed.allocs);
    let ports = parsed.allocs.iter().map(|a| a.public_port).collect();
    stamp(manifest, &targets, &parsed.allocs);
    Ok(ports)
}

/// Leader-side RPC handler entry point (`gossip::dispatch`'s
/// `/v1/raw-ports/allocate` arm): claim/reuse a public port for every raw
/// spec in `manifest` against THIS node's registry (only correct to call
/// when this node IS the leader — the RPC arm checks that first) and return
/// the full [`RawPortAllocation`] records (not just bare port numbers) so the
/// requesting node can populate its own local cache ([`adopt_allocations`])
/// without a second round trip. Does NOT stamp `manifest` — the caller (on a
/// DIFFERENT node, working from its own copy) does that itself with the
/// returned records, mirroring [`allocate_raw_ports_coordinated`]'s local
/// (already-leader) branch.
pub fn allocate_raw_ports_records(
    project: &str,
    manifest: &Manifest,
) -> anyhow::Result<Vec<RawPortAllocation>> {
    let targets = raw_targets(manifest);
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    claim_local(project, manifest, &targets)
}

/// Deploy-deletion entry point (LEADER-COORDINATED): the mirror of
/// [`allocate_raw_ports_coordinated`] for release. Always clears THIS node's
/// own local cache first (idempotent no-op if it holds none of `project`'s
/// claims), so `lookup`/`udp_allocations` here never serve a stale claim
/// regardless of leader status; when this node is NOT the leader, also
/// forwards the release to the leader's authoritative registry so a claim
/// freed here doesn't silently linger there forever (which would otherwise
/// leak the port — a deleted project's key would never re-clear from the
/// leader's `by_key`, since only the leader's own registry is authoritative).
/// Best-effort on the forward: a failed release-forward leaks a claim (the
/// safe failure direction — never a double allocation), logged rather than
/// surfaced as a deletion error.
pub async fn release_raw_ports_coordinated(cloud: &Arc<CloudState>, project: &str) -> Vec<u16> {
    let released = release_raw_ports(project);
    if cloud.is_control_plane_leader() {
        return released;
    }
    let leader = cloud.control_plane_leader();
    let peer = cloud.registry.nodes().into_iter().find(|n| {
        n.name == leader && !n.is_self && n.healthy && n.peer_id.is_some() && n.iroh_addr.is_some()
    });
    let Some(peer) = peer else {
        tracing::warn!(project, leader = %leader, "raw-port release: control-plane leader has no reachable mesh address — leader-side claim may leak until the next release");
        return released;
    };
    let (peer_id, peer_addr) = (
        peer.peer_id.clone().unwrap(),
        peer.iroh_addr.clone().unwrap(),
    );
    let req_body =
        serde_json::to_vec(&serde_json::json!({ "project": project })).unwrap_or_default();
    if crate::gossip::request_to(
        cloud,
        &peer_id,
        &peer_addr,
        hive_p2p::GOSSIP_POST,
        "/v1/raw-ports/release",
        &req_body,
        15,
    )
    .await
    .is_none()
    {
        tracing::warn!(project, leader = %leader, "raw-port release: forward to control-plane leader failed — leader-side claim may leak until the next release");
    }
    released
}

/// Release every public port allocated to `project` back to the pool. Returns
/// the released ports (empty when the project held none). Hooked into project
/// deletion and last-deployment deletion; idempotent.
pub fn release_raw_ports(project: &str) -> Vec<u16> {
    let mut reg = lock();
    let doomed: Vec<String> = reg
        .by_key
        .iter()
        .filter(|(_, a)| a.project == project)
        .map(|(k, _)| k.clone())
        .collect();
    if doomed.is_empty() {
        return Vec::new();
    }
    let mut released = Vec::with_capacity(doomed.len());
    for k in &doomed {
        if let Some(a) = reg.by_key.remove(k) {
            reg.in_use.remove(&a.public_port);
            released.push(a.public_port);
        }
    }
    // Best-effort save: on failure the FILE still claims the ports — the safe
    // failure direction (a leaked claim, never a double allocation); any later
    // successful save converges the file.
    if let Err(e) = save(&reg) {
        tracing::warn!(project, error = %e, "raw-port release not persisted (ports stay claimed on disk until the next save)");
    }
    released
}

/// Shared dedup core for adopting an externally-sourced allocation into the
/// local registry (never the allocation DECISION — only bookkeeping so this
/// node's own `lookup`/`udp_allocations` see a claim made elsewhere): skips a
/// key the registry already holds (registry claim wins) and refuses to adopt
/// a second claim onto a port another key already occupies. Returns whether
/// it actually inserted anything.
fn adopt_one(reg: &mut Registry, a: &RawPortAllocation) -> bool {
    let key = alloc_key(&a.project, &a.function, a.container_port, a.protocol);
    if reg.by_key.contains_key(&key) {
        return false; // registry claim wins
    }
    if reg.in_use.contains(&a.public_port) {
        tracing::warn!(project = %a.project, function = %a.function, port = a.public_port, "raw-port registry: not adopting — port already claimed by a different key");
        return false;
    }
    reg.in_use.insert(a.public_port);
    reg.by_key.insert(key, a.clone());
    true
}

/// Boot-restore self-heal: re-adopt the public-port stamps a persisted
/// deployment record carries into the registry, so an allocation survives even
/// a lost/corrupt `raw_ports.json` (the record and the registry are two
/// durable copies of the same claim; the registry wins on conflict since it is
/// the set claims are checked against).
pub fn adopt_record(rec: &fluid_core::DeployRecord) {
    let allocs: Vec<RawPortAllocation> = rec
        .manifest
        .functions
        .iter()
        .flat_map(|f| {
            f.ports.iter().filter_map(move |spec| {
                let public_port = spec.public_port?;
                Some(RawPortAllocation {
                    project: rec.project.clone(),
                    function: f.name.clone(),
                    container_port: spec.container_port,
                    protocol: spec.protocol,
                    public_port,
                    label: spec.label.clone(),
                    allocated_ms: rec.created_at_ms,
                })
            })
        })
        .collect();
    adopt_allocations(&allocs);
}

/// Merge externally-sourced allocations (the leader-coordinated RPC's
/// response) into this node's LOCAL registry as a read cache — see
/// [`adopt_one`]. Persists once for the whole batch when anything actually
/// changed; a save failure is logged, not surfaced, since the caller already
/// has its answer (the manifest is stamped either way) and the next boot's
/// `adopt_record` self-heals from the deployment record's own stamps.
fn adopt_allocations(allocs: &[RawPortAllocation]) {
    let mut reg = lock();
    let mut changed = false;
    for a in allocs {
        if adopt_one(&mut reg, a) {
            changed = true;
        }
    }
    if changed {
        if let Err(e) = save(&reg) {
            tracing::warn!(error = %e, "raw-port registry save failed after adopting coordinated allocation");
        }
    }
}

/// Read side for the raw-proxy layer (`crate::raw_proxy::resolve_binding`'s
/// most-authoritative source): which allocation (project / function /
/// container port / protocol) does a public port belong to right now?
pub fn lookup(public_port: u16) -> Option<RawPortAllocation> {
    lock()
        .by_key
        .values()
        .find(|a| a.public_port == public_port)
        .cloned()
}

/// Every UDP claim in this node's durable registry — the UDP relay's
/// most-authoritative desired-listener source (`crate::udp_relay`): the
/// allocator node knows a claim the moment it is made, before the stamped
/// deployment record is Ready or has gossiped fleet-wide.
pub fn udp_allocations() -> Vec<RawPortAllocation> {
    lock()
        .by_key
        .values()
        .filter(|a| a.protocol == ServiceProtocol::Udp)
        .cloned()
        .collect()
}
