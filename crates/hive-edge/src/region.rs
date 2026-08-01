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
    /// This node's OWN embedded iroh-relay server URL (e.g. `http://1.2.3.4:3341`),
    /// so peers can dial it as a relay/NAT-traversal fallback instead of routing
    /// through n0's public relay or a single central backstop. `None` until the
    /// embedded relay listener has bound (best-effort, filled in post-boot — see
    /// `NodeRegistry::set_self_relay_url`) or when the node has no reachable public
    /// address to advertise (NAT'd, same rule as `public_ip`). `#[serde(default)]`
    /// so older gossiped/persisted records without this field deserialize fine.
    #[serde(default)]
    pub relay_url: Option<String>,
    /// This node answers authoritative DNS (Seer) on port 53, i.e. it is a
    /// usable NAMESERVER for zones the platform delegates to itself. Set at
    /// boot from `HIVE_DNS_ADDR` (only when the bind is a real public
    /// `:53`, never a loopback/dev bind) and gossiped, so the DNS reconciler
    /// can derive the NS/glue record set for the geo zone from the same
    /// health-damped registry every other record already comes from instead
    /// of a hand-maintained nameserver list. `None` = not a nameserver.
    /// `#[serde(default)]` so pre-upgrade peers deserialize fine.
    ///
    /// This is a statement of INTENT, not of reachability: it is read out of
    /// this node's OWN environment and proves only that it meant to bind. Two
    /// live nameservers were advertised from it while being unusable from the
    /// public internet — one silently firewalled upstream of the host, one
    /// answering with zero records. What actually gates advertisement is
    /// `dns_attest` below.
    #[serde(default)]
    pub dns_ns: Option<String>,
    /// This node's Seer also serves the platform API zone (`api.{platform}`)
    /// geo/health-aware — advertised only by binaries that actually carry that
    /// answering path. Gates the `api` label's NS delegation at the parent so
    /// the NS set can never name a nameserver that would NXDOMAIN the zone
    /// (an older binary with `dns_ns` set answers the deploy zone fine but
    /// knows nothing of the api zone). `#[serde(default)]`: pre-upgrade peers
    /// deserialize `false` and are simply never eligible.
    #[serde(default)]
    pub dns_api: bool,
    /// **Peer attestation**: names of OTHER nodes this node has just PROVEN
    /// answer authoritative DNS on their public `:53`, by querying them over
    /// the public internet from this host (`hive-cloud`'s `dns_probe`).
    /// Gossiped so the DNS reconciler can require independent, off-host
    /// evidence from several regions before publishing a node as a nameserver
    /// — the thing `dns_ns` alone cannot give, since no check performed ON a
    /// host can see an inbound block upstream of it, or an answer that is only
    /// wrong for other people's clients. Never contains this node's own name:
    /// self-attestation is precisely the assumption this field replaces. Empty
    /// from pre-upgrade peers (`serde(default)`), which attests nothing —
    /// safe, because it WITHHOLDS advertisement rather than granting it.
    #[serde(default)]
    pub dns_attest: Vec<String>,
    /// This node currently serves the platform DASHBOARD acceptably: its local
    /// dashboard upstream (`HIVE_DASHBOARD_UPSTREAM`, the node's own `:3002`)
    /// answered a live probe within budget. Gossiped so the DNS reconciler can
    /// gate the apex/`www` A-set on nodes that will actually render the
    /// dashboard promptly — a node whose SSR takes seconds (witnessed:
    /// sao-paulo at 4–6s) otherwise degrades ~1/N of all first visits merely
    /// by being healthy. A MEASUREMENT, not boot intent: refreshed by a probe
    /// loop, `false` while the upstream is down/slow/absent. `serde(default)`
    /// = pre-upgrade peers read `false`, and the reconciler falls back to the
    /// full set while fewer than two nodes claim it (same floor discipline as
    /// the NS set), so a part-rolled fleet behaves exactly as before.
    #[serde(default)]
    pub dashboard: bool,
    /// Control-plane ownership epoch as witnessed by this node (monotonic; bumps
    /// on every owner promotion/failover). Gossiped so the whole fleet converges
    /// on the highest epoch — the fencing token that lets a node reject admin
    /// mutations forwarded by a peer whose view of ownership is stale. `0` from
    /// pre-upgrade peers (`serde(default)`), which fences nothing (safe).
    #[serde(default)]
    pub cp_epoch: u64,
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
    /// LIVE free disk on this node's data volume, GiB — refreshed on a timer, not
    /// a boot-time constant like `disk_total_gb` above.
    ///
    /// Exists because placement was disk-blind: `schedule.rs` filtered on health,
    /// region and GPU but never on space, so it kept choosing a node that was
    /// already full. Witnessed 2026-07-31 — fc-sanjose reached 100% (0 bytes
    /// free) and took down 9 customer deployments with "host disk critically low
    /// ... after GC", while fc-frankfurt and both CVM nodes sat below 10% used
    /// with ~920 GiB free each. Capacity in this fleet is also wildly
    /// heterogeneous (493 GiB to 1000 GiB), so a percentage alone is not enough
    /// to compare nodes; the absolute figure is what admission needs.
    ///
    /// `0` means "not reported" (a peer running an older build, or a probe that
    /// failed) and MUST be read as unknown rather than as a full disk — treating
    /// unknown as full would exclude every pre-upgrade peer from placement during
    /// a rollout.
    #[serde(default)]
    pub disk_free_gb: u64,
    /// MEASURED free VRAM across this node's GPUs, MiB (driver-reported), or
    /// `None` on a host with no usable `nvidia-smi`.
    ///
    /// The GPU pool otherwise infers free VRAM arithmetically from a count of
    /// live serverless GPU instances, which drifts from the hardware in both
    /// directions: measured 2026-07-31, fc-sanjose-gpu-1 was reported with
    /// 30 GiB reserved while its cards held 105 MiB each, and
    /// fc-sanjose-gpu-2's real `llama-server` allocation was invisible because
    /// an inference endpoint is not a serverless function instance. Fit
    /// decisions ("does this model need `--rpc` members?") were being made
    /// against a number matching neither the hardware nor the workload.
    ///
    /// `None` = not reported (pre-upgrade peer or no probe); callers fall back
    /// to the reservation estimate rather than reading it as zero free.
    #[serde(default)]
    pub gpu_free_mb: Option<u64>,
    /// Isolation backend this node runs: "firecracker" (real microVMs) or "mock"
    /// (sandboxed child processes — local/dev). The placement scheduler only
    /// auto-targets firecracker nodes; mock/local nodes host only when a region is
    /// explicitly selected for them.
    #[serde(default)]
    pub backend: String,
    /// GPUs physically present AND driver-visible on this host (`nvidia-smi`
    /// probe at boot — see `hive-cloud`'s `detect_gpus`). 0 = no GPUs, which is
    /// also what every pre-upgrade peer deserializes to (`serde(default)`), so
    /// old nodes simply read as GPU-less rather than erroring. Placement only
    /// targets `gpu_count > 0` nodes for functions that request a GPU; these
    /// fields are informational for everything else (dashboard, capacity sums).
    #[serde(default)]
    pub gpu_count: u32,
    /// Marketing model name of the first GPU (e.g. "Tesla T4"); hosts are
    /// homogeneous in practice, and a mixed host still reports a usable name.
    #[serde(default)]
    pub gpu_model: Option<String>,
    /// Total VRAM across all GPUs on the host, MiB.
    #[serde(default)]
    pub gpu_vram_mb: u64,
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

/// Central relay backstop — a fixed, always-available relay every node can reach,
/// used by [`select_relay_hint`] when no better (peer-owned or nearby-peer-owned)
/// relay is known yet.
pub const CENTRAL_RELAY_URL: &str = "https://relay.shadw.cloud";

/// Pick the best relay URL to HINT when dialing `target`, given the full live
/// registry snapshot (`nodes`, including `me`) — a pure, deterministic function so
/// it's directly unit-testable with no I/O/network/registry-locking involved.
///
/// Preference order:
///   1. `target`'s OWN `relay_url` (most direct — no extra hop through a third
///      node's relay).
///   2. The geographically closest OTHER peer's `relay_url` (simple squared lat/lon
///      distance — good enough for "nearest", true haversine isn't needed here),
///      excluding `target` and `me` themselves. When `target` (or a candidate) has
///      no lat/lon, falls back to the first other peer with a `relay_url` (order is
///      `nodes`' own order — deterministic, just not distance-ranked).
///   3. [`CENTRAL_RELAY_URL`], the fixed backstop — always returned rather than
///      `None`, so callers always have a usable hint.
pub fn select_relay_hint(target: &NodeInfo, nodes: &[NodeInfo], me: &NodeInfo) -> String {
    fn present(u: &Option<String>) -> Option<&str> {
        u.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    if let Some(r) = present(&target.relay_url) {
        return r.to_string();
    }
    let others = nodes.iter().filter(|n| n.id != target.id && n.id != me.id);
    if let (Some(tlat), Some(tlon)) = (target.lat, target.lon) {
        let mut best: Option<(f64, &str)> = None;
        for n in others.clone() {
            let Some(r) = present(&n.relay_url) else {
                continue;
            };
            let (Some(lat), Some(lon)) = (n.lat, n.lon) else {
                continue;
            };
            let d = (lat - tlat).powi(2) + (lon - tlon).powi(2);
            if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                best = Some((d, r));
            }
        }
        if let Some((_, r)) = best {
            return r.to_string();
        }
    }
    // No coordinates to rank by (either `target` or every candidate lacks
    // lat/lon) — first other peer that has a relay_url, in registry order.
    for n in others {
        if let Some(r) = present(&n.relay_url) {
            return r.to_string();
        }
    }
    CENTRAL_RELAY_URL.to_string()
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

    /// Refresh this node's own MESH `iroh_addr` (the address peers dial for
    /// gossip/forward RPCs — a DIFFERENT iroh identity from GuardianDB's own
    /// client, see `guardian_iroh_addr`'s doc). Mirrors `set_self_guardian_addr`:
    /// best-effort, idempotent, safe every gossip round, picked up by the next
    /// outgoing broadcast. `iroh_addr` used to be captured exactly once at bind
    /// time — before the endpoint had registered with its relay or learned
    /// hole-punch candidates — so it shipped private-addrs-only forever; this
    /// setter is what makes it converge to the endpoint's ACTUAL current
    /// knowledge of itself instead of a stale boot snapshot.
    pub fn set_self_iroh_addr(&self, addr: String) {
        let mut me = self.me.write();
        if me.iroh_addr.as_deref() != Some(addr.as_str()) {
            me.iroh_addr = Some(addr);
        }
    }

    /// Update this node's own `relay_url` once the embedded iroh-relay listener
    /// has bound (see `relay_url`'s own doc comment). Mirrors
    /// `set_self_guardian_addr`: best-effort, idempotent, safe to call every
    /// gossip round — a no-op write when unchanged, picked up by the very next
    /// outgoing `/v1/nodes/announce` gossip broadcast since `nodes()`/`me()`
    /// always read fresh. Never blocks boot.
    pub fn set_self_relay_url(&self, url: Option<String>) {
        let mut me = self.me.write();
        if me.relay_url != url {
            me.relay_url = url;
        }
    }

    /// Publish which peers this node currently ATTESTS as working nameservers
    /// (see `NodeInfo::dns_attest`). Mirrors `set_self_relay_url`: idempotent,
    /// safe every prober round, picked up by the next gossip broadcast.
    ///
    /// The caller is expected to pass a SORTED list — the value is compared for
    /// change and gossiped verbatim, so an unstable order would look like a
    /// fresh value on every round for no reason.
    pub fn set_self_dns_attest(&self, attest: Vec<String>) {
        let mut me = self.me.write();
        if me.dns_attest != attest {
            me.dns_attest = attest;
        }
    }

    /// Publish whether this node's LOCAL dashboard upstream currently answers
    /// within budget (see `NodeInfo::dashboard`). Mirrors `set_self_relay_url`:
    /// idempotent, safe every probe round, picked up by the next gossip
    /// broadcast.
    pub fn set_self_dashboard(&self, ok: bool) {
        let mut me = self.me.write();
        if me.dashboard != ok {
            me.dashboard = ok;
        }
    }

    /// Update this node's gossiped control-plane epoch (see
    /// `NodeInfo.cp_epoch`) — refreshed each gossip round from the cluster's
    /// observed-owner tracker.
    pub fn set_self_cp_epoch(&self, epoch: u64) {
        let mut me = self.me.write();
        if me.cp_epoch != epoch {
            me.cp_epoch = epoch;
        }
    }

    /// Update this node's own geolocation (lat/lon/city/country) when a
    /// periodic re-check finds it moved (see `spawn_geo_refresh` in
    /// hive-cloud). Mirrors `set_self_relay_url`: best-effort, idempotent, no
    /// write when unchanged, picked up by the next gossip round. Deliberately
    /// does NOT touch `region` — a node's region is its stable identity
    /// (baked into DNS names, ACME SANs, placement), while lat/lon only feed
    /// the regions map and DNS-nearest-by-geo; re-deriving `region` from a
    /// drifted geolocation would be the disruptive kind of "fix".
    pub fn set_self_geo(&self, lat: f64, lon: f64, city: String, country: String) {
        let mut me = self.me.write();
        if me.lat != Some(lat) || me.lon != Some(lon) {
            me.lat = Some(lat);
            me.lon = Some(lon);
            me.city = Some(city);
            me.country = Some(country);
        }
    }

    /// Refresh this node's LIVE free-disk figure so placement can see it.
    ///
    /// Deliberately separate from the boot-time capacity fields: free space is
    /// the only capacity number that moves on its own between restarts, and it
    /// is the one placement was missing when a full node kept winning
    /// deployments.
    pub fn set_self_disk_free(&self, disk_free_gb: u64) {
        let mut me = self.me.write();
        me.disk_free_gb = disk_free_gb;
    }

    /// Refresh this node's MEASURED free-VRAM figure (driver-reported).
    pub fn set_self_gpu_free(&self, gpu_free_mb: Option<u64>) {
        let mut me = self.me.write();
        me.gpu_free_mb = gpu_free_mb;
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
        nodes.sort_by(|a, b| {
            b.healthy
                .cmp(&a.healthy)
                .then(a.latency_ms.cmp(&b.latency_ms))
        });
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
    /// Upsert a peer from its OWN announcement (hot-join proof, `/v1/announce`,
    /// or the first — self — entry of the peer's own `/v1/nodes` response).
    ///
    /// A node's `id` is its operator-chosen `--name`, not a stable identity —
    /// `peer_id` (the real iroh endpoint id) is. Renaming a node keeps the
    /// same `peer_id` but changes `id`, so without eviction here the OLD
    /// name's entry never naturally expires: the active health-probe loop
    /// (main.rs spawn_health_loop) dials by `peer_id`/`iroh_addr`, and since
    /// those are unchanged the probe keeps succeeding and keeps refreshing
    /// the OLD entry's `last_seen_ms` forever — a permanent ghost duplicate,
    /// live-witnessed renaming fc-gpu-sj-1 -> fc-sanjose-gpu-1 (both names
    /// stayed `healthy: true` with freshening `last_seen_ms` for minutes,
    /// on every peer in the mesh, with no sign of self-expiry).
    ///
    /// Only a SELF-report may rename: eviction lives here and not in
    /// `upsert_peer` because relayed third-party copies of the old name keep
    /// circulating in other registries' `/v1/nodes` responses for a while —
    /// letting a relay evict-and-rename made the two names fight (last relay
    /// wins each gossip round, also live-witnessed: the mesh settled on the
    /// GHOST name while the process itself was announcing the real one).
    pub fn upsert_peer_self_report(&self, peer: NodeInfo) {
        if let Some(pid) = peer.peer_id.as_deref().filter(|s| !s.is_empty()) {
            let pid = pid.to_string();
            self.peers
                .write()
                .retain(|k, v| k == &peer.id || v.peer_id.as_deref() != Some(pid.as_str()));
        }
        self.upsert_peer(peer);
    }

    /// Upsert a peer from a RELAYED copy (any non-self entry of another
    /// node's `/v1/nodes` response). A relay may update an entry it agrees
    /// with on identity, or introduce a peer never heard from directly — but
    /// it must never RENAME: if an entry with the same `peer_id` already
    /// exists under a different `id`, the relayed copy is a stale echo of a
    /// pre-rename name and is dropped (see `upsert_peer_self_report`).
    pub fn upsert_peer(&self, mut peer: NodeInfo) {
        peer.is_self = false;
        let mut peers = self.peers.write();
        if let Some(pid) = peer.peer_id.as_deref().filter(|s| !s.is_empty()) {
            let renamed_elsewhere = peers
                .iter()
                .any(|(k, v)| k != &peer.id && v.peer_id.as_deref() == Some(pid));
            if renamed_elsewhere {
                return;
            }
        }
        if let Some(existing) = peers.get(&peer.id) {
            // Health + latency are owned by our OWN direct probes, never second-hand gossip.
            peer.healthy = existing.healthy;
            peer.latency_ms = existing.latency_ms;
            // DNS attestations are only as good as the record they rode in on.
            // A STALER relayed copy must not overwrite a fresher self-report:
            // attestations change every prober round, so the same
            // last-relay-wins race that used to flap `guardian_iroh_addr`
            // Some -> None would here silently un-attest a working nameserver
            // and pull it out of a live delegation. Freshness decides.
            if peer.last_seen_ms < existing.last_seen_ms {
                peer.dns_attest = existing.dns_attest.clone();
            }
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
            gpu_count: 0,
            gpu_model: None,
            gpu_vram_mb: 0,
            id: id.into(),
            name: id.into(),
            region: region.into(),
            public_url: format!("http://{id}:8787"),
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
            disk_free_gb: 0,
            gpu_free_mb: None,
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
        assert!(
            !p.healthy,
            "second-hand gossip must not resurrect a directly-failed node"
        );
        // A real direct success does bring it back.
        reg.set_health("peer", 5, true);
        let p = reg.nodes().into_iter().find(|n| n.id == "peer").unwrap();
        assert!(p.healthy, "direct probe success restores health");
    }
}
