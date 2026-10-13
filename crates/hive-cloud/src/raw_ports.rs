//! Public-port allocation for raw TCP/UDP/gRPC deployments. HTTP-family services ride the shared
//! 80/443 gateway (Host routing); a service speaking a raw protocol (Postgres wire, Minecraft,
//! gRPC) has no Host header, so each needs its OWN public port, and this module owns that port
//! space. The LISTENER on an allocated port is `crate::raw_proxy`'s job: this is pure allocation
//! and persistence.
//!
//! - **Range**: `HIVE_RAW_PORT_RANGE` (default `20000-29999`), reserved for the platform on every
//! node (the firewall/lockdown scripts open it); a port something else DOES hold is skipped by a
//! bind probe before claiming.
//! - **Unit**: one public port per raw-protocol [`PortSpec`] a deployment declares
//! (`FunctionConfig::ports`), so a multi-port service gets one per spec. An EMPTY `ports` list gets
//! nothing even when the protocol is raw: an un-exposed compose sidecar (private Postgres)
//! deliberately has no spec (`build_compose_manifest`'s `x-shadw-expose` opt-in), while
//! single-container paths always bridge their real port into `ports`
//! (`PortSpec::from_legacy_port`).
//! - **Stable across redeploys**: the claim is keyed by (project, function, container_port,
//! protocol), NOT deployment id, so a redeploy is re-stamped with the SAME public port.
//! - **Race safety, FLEET-wide**: the port space is fleet-global (every node binds every
//! allocation), so a node-local claim is not enough: two first-deploys on DIFFERENT nodes could
//! both claim the same low port for two tenants. The claim DECISION is therefore
//! leader-coordinated: [`allocate_raw_ports_coordinated`] claims locally only on the control-plane
//! leader, otherwise forwards the manifest over the mesh (`POST /v1/raw-ports/allocate`) and adopts
//! the leader's answer as a read cache, the same single-writer pattern as `admin_ingress`, billing
//! metering and ACME/DNS. [`allocate_raw_ports`] is the primitive FOR the leader (see `claim_local`
//! for its atomicity); the stamped [`PortSpec::public_port`] on the deployment record is a second
//! durable copy that [`adopt_record`] re-adopts at boot, so a lost registry file self-heals.
//! - **Permanent retirement**: [`release_raw_ports`] removes a project's live claim but QUARANTINES
//! every former number forever (a stale entry node can retain old routes, and re-granting the
//! number to another tenant would turn them into a cross-tenant splice), leader-coordinated through
//! [`release_raw_ports_coordinated`]. Relocation does NOT retire the stable project claim: it uses
//! the scoped `/reap-deployments` arm.
//!
//! Consumers: [`allocate_raw_ports`] (deploy), [`release_raw_ports`] (deletion) and [`lookup`] (the
//! proxy layer's most-authoritative port-to-allocation source); the fleet-wide read side is the
//! stamped bindings themselves (`fluid_core::DeploymentInfo::raw_ports` via
//! `Manifest::raw_port_bindings`), gossiped with the deployment list.

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

/// One durable public-port claim; `protocol` serializes as its kebab-case wire string
/// ("tcp"/"udp"/"grpc"), with the lenient-on-read semantics of everywhere the enum is stored.
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
    /// Port numbers whose former binding may still exist in an entry node's
    /// stale deployment/cache state. They are intentionally never re-granted.
    #[serde(default)]
    quarantined: BTreeSet<u16>,
}

struct Registry {
    /// claim key ([`alloc_key`]) → allocation. BTreeMap for deterministic
    /// file output.
    by_key: BTreeMap<String, RawPortAllocation>,
    /// Every public port currently claimed OR quarantined — the O(log n)
    /// membership set the claim scan checks.
    in_use: BTreeSet<u16>,
    /// Durable non-reuse set: a deleted or migrated binding moves here before its key disappears,
    /// so a stale node can never route this number for one tenant after the leader granted it to
    /// another.
    quarantined: BTreeSet<u16>,
    /// Rotating scan offset into the range so successive claims don't all
    /// re-probe from the bottom.
    cursor: u32,
}

/// The stable claim identity: same key means same public port across redeploys. `\u{1f}` (ASCII
/// unit separator) cannot appear in project/function names, so the key is collision-free without
/// escaping.
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

/// The configured public range (inclusive) from `HIVE_RAW_PORT_RANGE` ("lo-hi"), default
/// 20000-29999. A malformed value, or one that reaches port 0 ("kernel picks") or the privileged
/// 1-1023, is not a sane raw-ingress range: it warns and defaults rather than silently disabling
/// raw ingress.
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
    if lo < 1024 || hi < lo {
        return None;
    }
    Some((lo, hi))
}

/// The process-wide registry, loaded from disk exactly once. All mutation goes
/// through this single mutex — that lock IS the atomic-claim mechanism.
///
/// A (corrupt) duplicate or quarantined port never loads as a live claim (the first durable state
/// wins and the rejected record re-allocates on its next deploy); an unreadable file starts empty,
/// since the stamped deployment records re-adopt their claims at restore (`adopt_record`) and
/// established services keep their ports.
fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut by_key: BTreeMap<String, RawPortAllocation> = BTreeMap::new();
        let mut in_use: BTreeSet<u16> = BTreeSet::new();
        let mut quarantined: BTreeSet<u16> = BTreeSet::new();
        match std::fs::read_to_string(file_path()) {
            Ok(s) => match serde_json::from_str::<RegistryFile>(&s) {
                Ok(f) => {
                    quarantined = f.quarantined;
                    in_use.extend(quarantined.iter().copied());
                    for a in f.allocs {
                        let key = alloc_key(&a.project, &a.function, a.container_port, a.protocol);
                        if in_use.insert(a.public_port) {
                            by_key.insert(key, a);
                        } else {
                            tracing::warn!(project = %a.project, port = a.public_port, "raw-port registry: dropped duplicate/quarantined port claim on load");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "raw-port registry unreadable; starting empty (deployment-record stamps re-adopt at restore)");
                }
            },
            Err(_) => {}
        }
        Mutex::new(Registry {
            by_key,
            in_use,
            quarantined,
            cursor: 0,
        })
    })
}

/// Locks the registry, recovering from poison: a panic mid-claim leaves the plain map + set never
/// half-updated (claims roll back on error), and wedging every future deploy would be worse.
fn lock() -> MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(|p| p.into_inner())
}

/// Durably writes the registry (temp + fsync + rename, mirroring `persist::save`). Called while the
/// claim lock is held, so the on-disk set can never lag behind a port that has already escaped.
fn save(reg: &Registry) -> std::io::Result<()> {
    let dir = crate::persist::data_dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join("raw_ports.json.tmp");
    let file = RegistryFile {
        allocs: reg.by_key.values().cloned().collect(),
        quarantined: reg.quarantined.clone(),
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

/// Fixed public ports the platform or the host OS owns on fleet nodes, with the owner of each.
const RESERVED_PORTS: &[(u16, &str)] = &[
    (3000, "hive-ui dashboard"),
    (3340, "embedded iroh relay (plaintext captive-portal + tls)"),
    (3343, "embedded iroh relay (plaintext captive-portal + tls)"),
    (3350, "discovery DNS (pkarr relay)"),
    (5432, "db-gateway Postgres listener"),
    (6379, "db-gateway Redis listener"),
    (8786, "node admin / gateway HTTP"),
    (8787, "node admin / gateway HTTP"),
    (50052, "llama.cpp rpc-server (GPU pooling)"),
    (9090, "relay metrics (hive-lockdown LOCKED_PORTS)"),
];

/// Reserved public port ranges, with the owner of each.
const RESERVED_PORT_RANGES: &[(std::ops::RangeInclusive<u16>, &str)] = &[
    (7799..=7804, "hive-rt-node workers (no-auth, 0.0.0.0)"),
    (50_100..=50_999, "managed-inference llama-server range"),
];

/// Public ports the platform itself (or the host OS) owns on fleet nodes: a tenant's publish
/// request may never take one, whatever their compose file says. Everything below 1024 is refused
/// wholesale (22/53/80/443 live there, binding them needs root, and hijacking a well-known port is
/// never what a tenant legitimately wants from `ports:`).
fn reserved_public_port(p: u16) -> bool {
    p < 1024
        || RESERVED_PORTS.iter().any(|(port, _)| *port == p)
        || RESERVED_PORT_RANGES
            .iter()
            .any(|(range, _)| range.contains(&p))
}

/// Defensive probe: can THIS node still bind the candidate on both TCP and UDP? The range is meant
/// to be reserved, but anything squatting a port (a leftover process, a manual binding) must be
/// skipped rather than handed to a deployment whose proxy listener would then fail. Probing both
/// families keeps the claim protocol-agnostic (one shared number space); the probe sockets drop at
/// once, the real listener is the raw proxy's.
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
         widen HIVE_RAW_PORT_RANGE; retired numbers are intentionally never reused"
    )
}

/// (function idx, port idx) of every spec in `manifest` that needs public raw ingress, in manifest
/// order. Pure and deterministic over the manifest's content alone, so the calling node (to know
/// whether an RPC is needed) and the leader (inside the RPC handler) always agree on target count
/// and order, which lets [`allocate_raw_ports_coordinated`] line up the leader's per-target
/// response against its own manifest without shipping the mutated manifest back. A compose PUBLISH
/// request (`preferred_public_port`) makes even an Http-protocol spec a raw target (a published
/// port is reachable on the host and the raw proxy serves it as a plain-TCP passthrough); the
/// purity holds only when both sides run a binary with that field: a pre-upgrade leader computes
/// fewer targets and the caller's response-length check fails the allocation LOUDLY (never a silent
/// partial grant), and the deploy retries once the leader is upgraded.
fn raw_targets(manifest: &Manifest) -> Vec<(usize, usize)> {
    manifest
        .functions
        .iter()
        .enumerate()
        .flat_map(|(fi, f)| {
            f.ports
                .iter()
                .enumerate()
                .filter(|(_, spec)| {
                    spec.protocol.needs_raw_proxy() || spec.preferred_public_port.is_some()
                })
                .map(move |(pi, _)| (fi, pi))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Claims/reuses a public port for every `targets` entry against THIS node's LOCAL registry,
/// durably saved before returning. Only correct on the allocation's single writer (the leader, via
/// [`allocate_raw_ports_coordinated`], or the RPC handler on it): on a non-leader it races that
/// node's own registry against every other's, the fleet-global race the coordinated path prevents.
///
/// Atomic per call: every new claim is inserted under ONE lock hold and durably saved before any
/// port escapes (a claim not on disk could be re-handed to a parallel deploy after a crash); if the
/// range exhausts or the save fails mid-batch, every claim of THIS call is rolled back LIFO,
/// restoring any allocation a migration displaced and un-retiring its number, so memory never
/// diverges from what a reload of the still-current file would produce.
///
/// A compose publish request names its exact public port (`ports: ["9000:9000"]`): claimable only
/// if not platform-reserved, not granted to someone else and still bindable here (the caller logs
/// grant vs preference in the BUILD LOG, so a fallback is never silent). `in_use` is checked before
/// the bind probe (probing a port our OWN proxy listens on fails and would wrongly reject a
/// re-grant), and the probe runs EXACTLY ONCE per target before the branch decision (probing in a
/// match guard and again in the arm let the OS change the answer between them). A redeploy keeps
/// the SAME public port without a probe (its own live listener holds it) unless the author CHANGED
/// the published host port to a claimable number (the compose file is the source of truth).
///
/// A migration QUARANTINES the old number instead of freeing it: every node adopted the old claim
/// into its read cache and nothing invalidates those, so re-granting it to a DIFFERENT tenant would
/// route that tenant's public port to the OLD project on stale entry nodes (a cross-tenant misroute
/// surviving reboots). The number stays in `in_use` ownerless, persisted in the same atomic file as
/// the replacement claim (held only in `in_use` it lost the quarantine on restart): one number out
/// of a 10k range per migration, the safe direction. Consequence: a port SWAP (A:9000 to 9001 while
/// B:9001 to 9000) cannot converge, and a migration back to a quarantined number is refused.
fn claim_local(
    project: &str,
    manifest: &Manifest,
    targets: &[(usize, usize)],
) -> anyhow::Result<Vec<RawPortAllocation>> {
    let mut allocs: Vec<RawPortAllocation> = Vec::with_capacity(targets.len());
    let mut reg = lock();
    let mut newly: Vec<(String, u16, Option<RawPortAllocation>)> = Vec::new();
    let rollback = |reg: &mut Registry, newly: &[(String, u16, Option<RawPortAllocation>)]| {
        for (k, p, displaced) in newly.iter().rev() {
            reg.by_key.remove(k);
            reg.in_use.remove(p);
            if let Some(old) = displaced {
                reg.quarantined.remove(&old.public_port);
                reg.in_use.insert(old.public_port);
                reg.by_key.insert(k.clone(), old.clone());
            }
        }
    };
    for &(fi, pi) in targets {
        let f = &manifest.functions[fi];
        let spec = &f.ports[pi];
        let key = alloc_key(project, &f.name, spec.container_port, spec.protocol);
        let preferred_ok = spec
            .preferred_public_port
            .filter(|&p| !reserved_public_port(p))
            .filter(|&p| !reg.in_use.contains(&p) && probe_bindable(p));
        let alloc = match reg.by_key.get(&key).cloned() {
            Some(existing)
                if preferred_ok.is_none() || preferred_ok == Some(existing.public_port) =>
            {
                existing
            }
            prior => {
                let p = match preferred_ok {
                    Some(p) => p,
                    None => match next_free(&mut reg) {
                        Ok(p) => p,
                        Err(e) => {
                            rollback(&mut reg, &newly);
                            return Err(e);
                        }
                    },
                };
                if let Some(old) = &prior {
                    reg.quarantined.insert(old.public_port);
                    tracing::info!(
                        project,
                        function = %f.name,
                        old_port = old.public_port,
                        new_port = p,
                        "raw-port allocator: publish preference changed — migrating grant \
                         (old number quarantined, never re-granted)"
                    );
                }
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
                newly.push((key, p, prior));
                a
            }
        };
        allocs.push(alloc);
    }
    if !newly.is_empty() {
        if let Err(e) = save(&reg) {
            rollback(&mut reg, &newly);
            anyhow::bail!("could not persist raw-port claim ({e}) — refusing to hand out unclaimed public ports");
        }
    }
    Ok(allocs)
}

/// Stamps `targets[n]`'s `public_port` from `allocs[n]`: the slices are the same length and order
/// (both come from the same [`raw_targets`] computation).
fn stamp(manifest: &mut Manifest, targets: &[(usize, usize)], allocs: &[RawPortAllocation]) {
    for (n, &(fi, pi)) in targets.iter().enumerate() {
        manifest.functions[fi].ports[pi].public_port = Some(allocs[n].public_port);
    }
}

/// Deploy-time entry point (LEADER-COORDINATED): the claim decision always runs on the
/// control-plane leader, closing the fleet-global port race a node-local claim cannot see. This
/// node's registry is then updated as a READ CACHE from the leader's answer
/// ([`adopt_allocations`]), so `lookup`/`udp_allocations` here see the claim at once instead of
/// after the next gossiped deployment-list round. A deployment relocating to another node
/// re-resolves the SAME coordinated port (the leader's registry hit on the stable `alloc_key`),
/// fixing host-move port instability as a side effect (see the module doc).
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
    let Some(leader) = cloud.leader_forward_target() else {
        anyhow::bail!(
            "raw-port allocation: no serving control-plane leader is resolvable from this node right now"
        );
    };
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

/// Leader-side RPC entry point (`gossip::dispatch`'s `/v1/raw-ports/allocate` arm): claims/reuses a
/// public port for every raw spec in `manifest` against THIS node's registry (only correct on the
/// leader, which the arm checks first) and returns the full [`RawPortAllocation`] records, not bare
/// numbers, so the requester can populate its cache ([`adopt_allocations`]) without a second round
/// trip. Does NOT stamp `manifest`: the caller, working from its own copy, does that with the
/// returned records.
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

/// Project-deletion entry point (LEADER-COORDINATED), the mirror of
/// [`allocate_raw_ports_coordinated`] for permanent retirement: the name and `/release` wire path
/// are kept for compatibility, but no number returns to the pool. Always retires THIS node's local
/// claim first, so `lookup`/`udp_allocations` here never serve it, and when this node is not the
/// leader also forwards to the authoritative registry. The forward is best-effort: a failure leaves
/// the leader's live claim unavailable (the safe direction) and deletion reconciliation retries
/// from its durable project tombstone.
pub async fn release_raw_ports_coordinated(cloud: &Arc<CloudState>, project: &str) -> Vec<u16> {
    let retired = release_raw_ports(project);
    if cloud.is_control_plane_leader() {
        return retired;
    }
    let Some(leader) = cloud.leader_forward_target() else {
        tracing::warn!(project, "raw-port retirement: no serving control-plane leader is resolvable from this node — leader-side live claim remains unavailable and will retry from the deletion tombstone");
        return retired;
    };
    let peer = cloud.registry.nodes().into_iter().find(|n| {
        n.name == leader && !n.is_self && n.healthy && n.peer_id.is_some() && n.iroh_addr.is_some()
    });
    let Some(peer) = peer else {
        tracing::warn!(project, leader = %leader, "raw-port retirement: control-plane leader has no reachable mesh address — leader-side live claim remains unavailable and will retry from the deletion tombstone");
        return retired;
    };
    let (peer_id, peer_addr) = (
        peer.peer_id.clone().unwrap(),
        peer.iroh_addr.clone().unwrap(),
    );
    let req_body =
        serde_json::to_vec(&serde_json::json!({ "project": project })).unwrap_or_default();
    match crate::gossip::request_to(
        cloud,
        &peer_id,
        &peer_addr,
        hive_p2p::GOSSIP_POST,
        "/v1/raw-ports/release",
        &req_body,
        15,
    )
    .await
    {
        Some(body) => {
            let confirmed = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .is_some_and(|v| v.get("retired").is_some() || v.get("released").is_some());
            if !confirmed {
                tracing::warn!(project, leader = %leader, "raw-port retirement: leader returned an unconfirmed response — deletion reconciliation will retry");
            }
        }
        None => {
            tracing::warn!(project, leader = %leader, "raw-port retirement: forward to control-plane leader failed — leader-side live claim remains unavailable and will retry from the deletion tombstone");
        }
    }
    retired
}

/// Removes every live allocation for `project` while permanently quarantining each public number;
/// returns the retired ports (empty when the project held none). Hooked into
/// project/last-deployment deletion; idempotent. A stale entry node can retain the old deployment
/// record and route indefinitely, so reusing its number for another tenant would turn that route
/// into a cross-tenant splice. The quarantined numbers stay in `in_use`, unavailable to `next_free`
/// and preferred-port claims forever; the save is best-effort (on failure the FILE still carries
/// the former live claim, equally unavailable, and any later successful save persists the
/// quarantine).
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
    let mut retired = Vec::with_capacity(doomed.len());
    for k in &doomed {
        if let Some(a) = reg.by_key.remove(k) {
            reg.quarantined.insert(a.public_port);
            reg.in_use.insert(a.public_port);
            retired.push(a.public_port);
        }
    }
    if let Err(e) = save(&reg) {
        tracing::warn!(project, error = %e, "raw-port quarantine not persisted (ports remain claimed on disk until the next save)");
    }
    retired
}

/// Shared dedup core for adopting an externally sourced allocation into the local registry (never
/// the allocation DECISION, only bookkeeping so this node's `lookup`/`udp_allocations` see a claim
/// made elsewhere): skips a key the registry already holds (the registry claim wins), refuses a
/// second claim onto a port another key occupies, and returns whether it inserted anything.
fn adopt_one(reg: &mut Registry, a: &RawPortAllocation) -> bool {
    let key = alloc_key(&a.project, &a.function, a.container_port, a.protocol);
    if reg.by_key.contains_key(&key) {
        return false;
    }
    if reg.quarantined.contains(&a.public_port) {
        tracing::warn!(project = %a.project, function = %a.function, port = a.public_port, "raw-port registry: not adopting stale record — port is permanently quarantined");
        return false;
    }
    if reg.in_use.contains(&a.public_port) {
        tracing::warn!(project = %a.project, function = %a.function, port = a.public_port, "raw-port registry: not adopting — port already claimed by a different key");
        return false;
    }
    reg.in_use.insert(a.public_port);
    reg.by_key.insert(key, a.clone());
    true
}

/// Boot-restore self-heal: re-adopts the public-port stamps a persisted deployment record carries,
/// so an allocation survives even a lost or corrupt `raw_ports.json` (the record and the registry
/// are two durable copies of one claim; the registry wins on conflict, being the set claims are
/// checked against).
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

/// Merges externally sourced allocations (the coordinated RPC's response) into this node's LOCAL
/// registry as a read cache ([`adopt_one`]); persists once per batch when anything changed. A save
/// failure is logged, not surfaced: the caller already has its answer (the manifest is stamped
/// either way) and the next boot's `adopt_record` self-heals from the record stamps.
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

/// Every UDP claim in this node's durable registry, the UDP relay's most-authoritative
/// desired-listener source (`crate::udp_relay`): the allocator node knows a claim the moment it is
/// made, before the stamped deployment record is Ready or has gossiped.
pub fn udp_allocations() -> Vec<RawPortAllocation> {
    lock()
        .by_key
        .values()
        .filter(|a| a.protocol == ServiceProtocol::Udp)
        .cloned()
        .collect()
}
