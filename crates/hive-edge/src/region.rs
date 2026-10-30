//! Regions and the node registry: each node has a region and an iroh endpoint id; nodes learn
//! about peers and route to the nearest region.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A node in the cloud (this machine or a peer MacBook).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    pub region: String,
    /// Public gateway URL (for same-LAN / direct addressing).
    pub public_url: String,
    /// Reachable public IPv4 for Seer A records (`HIVE_PUBLIC_IP`); `None` for NAT'd nodes, never
    /// the bind address.
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
    /// GuardianDB's own dialable iroh address — a separate identity from `iroh_addr`; `None` until
    /// its client has bound.
    #[serde(default)]
    pub guardian_iroh_addr: Option<String>,
    /// This node's embedded iroh-relay URL (`set_self_relay_url`); `None` until bound or when the
    /// node has no public address.
    #[serde(default)]
    pub relay_url: Option<String>,
    /// Intent to answer authoritative DNS (Seer) on a public `:53`, set from `HIVE_DNS_ADDR`;
    /// `None` = not a nameserver. Reachability is proven only by `dns_attest`.
    #[serde(default)]
    pub dns_ns: Option<String>,
    /// This node's Seer also serves the platform API zone; gates the `api` label's NS delegation
    /// (pre-upgrade peers read `false`).
    #[serde(default)]
    pub dns_api: bool,
    /// Other nodes this node has PROVEN answer DNS on their public `:53` (`dns_probe`); never
    /// contains this node's own name.
    #[serde(default)]
    pub dns_attest: Vec<String>,
    /// Measured: the local dashboard upstream (`HIVE_DASHBOARD_UPSTREAM`) answered a probe within
    /// budget; gates the apex/`www` A-set.
    #[serde(default)]
    pub dashboard: bool,
    /// Control-plane ownership epoch as witnessed by this node (monotonic) — the fence that rejects
    /// stale forwarded mutations. `0` fences nothing.
    #[serde(default)]
    pub cp_epoch: u64,
    pub last_seen_ms: u64,
    #[serde(default)]
    pub is_self: bool,
    /// Measured round-trip latency to this node (ms). Zero is local for self
    /// and unknown for every non-self peer.
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
    /// Live free disk on the data volume (GiB), refreshed on a timer; `0` = not reported and must be
    /// read as UNKNOWN, never as full.
    #[serde(default)]
    pub disk_free_gb: u64,
    /// Driver-measured free VRAM across this node's GPUs (MiB); `None` = not reported, callers fall
    /// back to the reservation estimate.
    #[serde(default)]
    pub gpu_free_mb: Option<u64>,
    /// Isolation backend: "firecracker" (microVMs), "litebox" (userspace Linux ABI) or "mock"
    /// (sandboxed processes); `schedule::eligible`/`capable` in hive-cloud decide which nodes
    /// placement targets.
    #[serde(default)]
    pub backend: String,
    /// GPUs present AND driver-visible (`detect_gpus`); `0` for GPU-less and pre-upgrade peers.
    /// Placement requires `gpu_count > 0` for GPU functions.
    #[serde(default)]
    pub gpu_count: u32,
    /// Whether `wasmer` exists on the filesystem the cell execs against
    /// (`resources::RuntimeCapabilitySource` in hive-cloud); `None` = NOT CAPABLE, never
    /// unknown-so-admit.
    #[serde(default)]
    pub wasm_runtime: Option<bool>,
    /// Whether `bun` exists on the filesystem the cell execs against
    /// (`resources::RuntimeCapabilitySource` in hive-cloud; litebox always `false`); `None` = NOT
    /// CAPABLE.
    #[serde(default)]
    pub bun_runtime: Option<bool>,
    /// Toolchains the BUILD lane can exec, in `resources::BUILD_TOOLCHAINS` order (host PATH for
    /// mock/litebox, `None` on firecracker); `None` = UNKNOWN and ADMITTED by placement, since npm
    /// substitutes a missing manager.
    #[serde(default)]
    pub build_toolchains: Option<Vec<String>>,
    /// Runtime-artifact staging protocol served by this node; `None` (pre-upgrade) cannot host
    /// source-built functions.
    #[serde(default)]
    pub runtime_artifact_protocol: Option<u16>,
    /// Repository-command isolation protocol live-probed here; `None`/unknown versions never receive
    /// a source build.
    #[serde(default)]
    pub build_isolation_protocol: Option<u16>,
    /// A usable `musqld` was PROBED here (`HIVE_SQLITE_ENGINE=musql`); `None` = NOT CAPABLE.
    #[serde(default)]
    pub musql_engine: Option<bool>,
    /// Sealed runtime-artifact TRANSFER protocol served by this node's receiver; `None` keeps the
    /// coordinator on legacy per-target dispatch.
    #[serde(default)]
    pub artifact_transfer_protocol: Option<u16>,
    /// Marketing model name of the first GPU (e.g. "Tesla T4"); hosts are
    /// homogeneous in practice, and a mixed host still reports a usable name.
    #[serde(default)]
    pub gpu_model: Option<String>,
    /// Total VRAM across all GPUs on the host, MiB.
    #[serde(default)]
    pub gpu_vram_mb: u64,
    /// Epoch-ms this PROCESS started — the fleet-visible "is that node cycling?" signal; `0` = not
    /// reported (UNKNOWN).
    #[serde(default)]
    pub started_ms: u64,
    /// OOM-kill restarts in the last 24h (`restart_audit`); `0` also means pre-upgrade peer — a
    /// floor, never proof of health.
    #[serde(default)]
    pub oom_restarts_24h: u32,
    /// Restarts in the last 24h whose predecessor died without a graceful shutdown — the only
    /// counter that sees involuntary crashes; `0` also means unknown.
    #[serde(default)]
    pub unclean_restarts_24h: u32,
    /// Epoch-ms of the most recent boot whose predecessor was OOM-killed.
    /// `None` = never observed (or not reported).
    #[serde(default)]
    pub last_oom_ms: Option<u64>,
    /// Cloud provider (`HIVE_CLOUD_PROVIDER`, e.g. "tencent"); `None` never qualifies for the
    /// Tencent-CCN private path in either direction.
    #[serde(default)]
    pub provider: Option<String>,
    /// Tencent VPC-private `ip:port` for CCN traffic (`HIVE_PRIVATE_ADDR`, never sniffed); never
    /// folded into `iroh_addr`/`public_ip`. `None` = never a private dial target.
    #[serde(default)]
    pub private_addr: Option<String>,
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

/// Fixed relay backstop used by [`select_relay_hint`] when no peer-owned relay is known. It must be
/// a REAL iroh-relay endpoint: HTTPS on **3343** under a host the `*.relay.shadw.app` wildcard
/// certificate covers (a port-less `relay.shadw.cloud` URL reaches a fleet node's :443,
/// hive-cloud's public edge, which answers `/ping` with 404 `host-rejected`).
pub const CENTRAL_RELAY_URL: &str = "https://fc-virginia.relay.shadw.app:3343";

/// Relay URL to hint when dialing `target`: its own `relay_url`, else the geographically nearest
/// other peer's, else [`CENTRAL_RELAY_URL`] (never `None`).
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
    for n in others {
        if let Some(r) = present(&n.relay_url) {
            return r.to_string();
        }
    }
    CENTRAL_RELAY_URL.to_string()
}

/// Coarse bounding-box classification of a coordinate into a continent name, most-specific box
/// first.
pub fn continent_of(lat: f64, lon: f64) -> &'static str {
    if lat < -60.0 {
        return "Antarctica";
    }
    if lat < 0.0 && lon >= 110.0 && lon <= 180.0 {
        return "Oceania";
    }
    if lat >= 36.0 && lat <= 72.0 && lon >= -25.0 && lon <= 60.0 {
        return "Europe";
    }
    if lat >= -35.0 && lat < 36.0 && lon >= -20.0 && lon <= 52.0 {
        return "Africa";
    }
    if lon > 52.0 && lon <= 180.0 {
        return "Asia";
    }
    if lat < 13.0 && lon >= -82.0 && lon <= -34.0 {
        return "South America";
    }
    if lon >= -170.0 && lon <= -34.0 {
        return "North America";
    }
    "Other"
}

/// Maximum tolerated sender/receiver clock skew for gossip liveness; a farther-future timestamp
/// is not freshness evidence.
pub const MAX_GOSSIP_FUTURE_SKEW_MS: u64 = 5_000;

/// Is an origin timestamp plausibly clocked and younger than `max_age_ms`? Shared by registry
/// expiry and health demotion/restoration.
pub fn gossip_timestamp_fresh(last_seen_ms: u64, now_ms: u64, max_age_ms: u64) -> bool {
    last_seen_ms <= now_ms.saturating_add(MAX_GOSSIP_FUTURE_SKEW_MS)
        && now_ms.saturating_sub(last_seen_ms) < max_age_ms
}

/// Stable identity of a registry peer: `name` is the mutable lookup label, `endpoint_id` the
/// immutable transport identity.
#[derive(Clone, Debug)]
pub struct PeerIdentity {
    pub name: String,
    pub endpoint_id: Option<String>,
}

/// Receiver-time evidence from a peer's own direct gossip record. Health probes
/// never update this clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerGossipEvidence {
    pub endpoint_id: String,
    pub last_received_ago: Duration,
}

/// Atomic outcome of a guarded health withdrawal.
#[derive(Clone, Debug)]
pub enum PeerDemotion {
    Missing,
    GossipAlive(PeerIdentity),
    /// Gossip-stale, but the caller's commit gate declined the withdrawal
    /// (the staleness has not yet persisted long enough); nothing written.
    Deferred(PeerIdentity),
    Demoted(PeerIdentity),
}

/// Sort key for a non-self peer latency. Zero and the restored sentinel mean
/// "alive but unmeasured", so both follow every finite measured RTT.
pub fn peer_latency_sort_key(latency_ms: u64) -> (u8, u64) {
    if latency_ms == 0 || latency_ms >= NodeRegistry::RESTORED_LATENCY_MS {
        (1, latency_ms)
    } else {
        (0, latency_ms)
    }
}

pub struct NodeRegistry {
    me: RwLock<NodeInfo>,
    peers: RwLock<HashMap<String, NodeInfo>>,
    gossip_received: RwLock<HashMap<String, Instant>>,
}

impl NodeRegistry {
    pub fn new(me: NodeInfo) -> Arc<NodeRegistry> {
        Arc::new(NodeRegistry {
            me: RwLock::new(me),
            peers: RwLock::new(HashMap::new()),
            gossip_received: RwLock::new(HashMap::new()),
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

    /// Publish this node's `guardian_iroh_addr` once GuardianDB's client has bound; idempotent,
    /// picked up by the next gossip round.
    pub fn set_self_guardian_addr(&self, addr: Option<String>) {
        let mut me = self.me.write();
        if me.guardian_iroh_addr != addr {
            me.guardian_iroh_addr = addr;
        }
    }

    /// Refresh this node's mesh `iroh_addr` to the endpoint's current knowledge of itself (relay
    /// registration, hole-punch candidates); idempotent.
    pub fn set_self_iroh_addr(&self, addr: String) {
        let mut me = self.me.write();
        if me.iroh_addr.as_deref() != Some(addr.as_str()) {
            me.iroh_addr = Some(addr);
        }
    }

    /// Publish this node's `relay_url` once the embedded relay has bound; idempotent, picked up by
    /// the next gossip round, never blocks boot.
    pub fn set_self_relay_url(&self, url: Option<String>) {
        let mut me = self.me.write();
        if me.relay_url != url {
            me.relay_url = url;
        }
    }

    /// Publish the peers this node ATTESTS as working nameservers. `attest` must be SORTED: the
    /// value is compared and gossiped verbatim.
    pub fn set_self_dns_attest(&self, attest: Vec<String>) {
        let mut me = self.me.write();
        if me.dns_attest != attest {
            me.dns_attest = attest;
        }
    }

    /// Publish whether the local dashboard upstream currently answers within budget.
    pub fn set_self_dashboard(&self, ok: bool) {
        let mut me = self.me.write();
        if me.dashboard != ok {
            me.dashboard = ok;
        }
    }

    /// Refresh this node's gossiped control-plane epoch from the observed-owner tracker.
    pub fn set_self_cp_epoch(&self, epoch: u64) {
        let mut me = self.me.write();
        if me.cp_epoch != epoch {
            me.cp_epoch = epoch;
        }
    }

    /// Update this node's geolocation when a periodic re-check finds it moved; never touches
    /// `region`, which is the node's stable identity.
    pub fn set_self_geo(&self, lat: f64, lon: f64, city: String, country: String) {
        let mut me = self.me.write();
        if me.lat != Some(lat) || me.lon != Some(lon) {
            me.lat = Some(lat);
            me.lon = Some(lon);
            me.city = Some(city);
            me.country = Some(country);
        }
    }

    /// Refresh this node's live free-disk figure so placement can see it.
    pub fn set_self_disk_free(&self, disk_free_gb: u64) {
        let mut me = self.me.write();
        me.disk_free_gb = disk_free_gb;
    }

    /// Refresh this node's MEASURED free-VRAM figure (driver-reported).
    pub fn set_self_gpu_free(&self, gpu_free_mb: Option<u64>) {
        let mut me = self.me.write();
        me.gpu_free_mb = gpu_free_mb;
    }

    /// Replace every backend/image-derived capability under ONE write lock, so gossip never observes
    /// a tuple no single observation produced.
    pub fn set_self_runtime_capabilities(
        &self,
        wasm_runtime: Option<bool>,
        bun_runtime: Option<bool>,
        runtime_artifact_protocol: Option<u16>,
        build_toolchains: Option<Vec<String>>,
    ) {
        let mut me = self.me.write();
        me.wasm_runtime = wasm_runtime;
        me.bun_runtime = bun_runtime;
        me.runtime_artifact_protocol = runtime_artifact_protocol;
        me.build_toolchains = build_toolchains;
    }

    /// Publish (or withdraw with `None`) this node's sealed-artifact transfer receiver capability.
    pub fn set_self_artifact_transfer_protocol(&self, protocol: Option<u16>) {
        self.me.write().artifact_transfer_protocol = protocol;
    }

    /// Publish this node's musql capability; re-published on every refresh tick.
    pub fn set_self_musql_engine(&self, musql_engine: Option<bool>) {
        self.me.write().musql_engine = musql_engine;
    }

    /// Refresh this node's restart-audit counters on a timer, since the 24h window slides.
    pub fn set_self_restart_audit(
        &self,
        started_ms: u64,
        oom_restarts_24h: u32,
        unclean_restarts_24h: u32,
        last_oom_ms: Option<u64>,
    ) {
        let mut me = self.me.write();
        me.started_ms = started_ms;
        me.oom_restarts_24h = oom_restarts_24h;
        me.unclean_restarts_24h = unclean_restarts_24h;
        me.last_oom_ms = last_oom_ms;
    }

    fn peer_name_for(peers: &HashMap<String, NodeInfo>, id: &str) -> Option<String> {
        peers
            .iter()
            .find(|(_, peer)| peer.peer_id.as_deref() == Some(id))
            .map(|(name, _)| name.clone())
            .or_else(|| peers.contains_key(id).then(|| id.to_string()))
    }

    fn identity(peer: &NodeInfo) -> PeerIdentity {
        PeerIdentity {
            name: peer.id.clone(),
            endpoint_id: peer.peer_id.clone().filter(|id| !id.is_empty()),
        }
    }

    pub fn peer_identity(&self, id: &str) -> Option<PeerIdentity> {
        let peers = self.peers.read();
        let name = Self::peer_name_for(&peers, id)?;
        peers.get(&name).map(Self::identity)
    }

    pub fn gossip_evidence_snapshot(&self) -> Vec<PeerGossipEvidence> {
        let received = self.gossip_received.read();
        let now = Instant::now();
        let mut snapshot = received
            .iter()
            .map(|(endpoint_id, at)| PeerGossipEvidence {
                endpoint_id: endpoint_id.clone(),
                last_received_ago: now.saturating_duration_since(*at),
            })
            .collect::<Vec<_>>();
        snapshot.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
        snapshot
    }

    /// A peer's last stored record regardless of freshness (unlike `nodes()`), by registry name or
    /// endpoint id — for stable facts, never for "is it up".
    pub fn peer_record(&self, id: &str) -> Option<NodeInfo> {
        let peers = self.peers.read();
        let name = Self::peer_name_for(&peers, id)?;
        peers.get(&name).cloned()
    }

    /// When this peer was last heard from by gossip, epoch-ms. Accepts either
    /// the current registry name or the canonical endpoint id.
    pub fn peer_last_seen_ms(&self, id: &str) -> Option<u64> {
        let peers = self.peers.read();
        let name = Self::peer_name_for(&peers, id)?;
        peers.get(&name).map(|peer| peer.last_seen_ms)
    }

    /// Latency stamped on a peer restored by indirect gossip. It is an
    /// impossible RTT sentinel and must remain behind every finite measurement.
    pub const RESTORED_LATENCY_MS: u64 = u64::MAX - 1;

    /// Withdraw the peer identity actually attempted after a failed transport observation; a
    /// concurrent refresh wins under the same write lock.
    pub fn demote_if_gossip_stale(
        &self,
        id: &str,
        expected_endpoint_id: Option<&str>,
        observed_last_seen_ms: Option<u64>,
        max_age_ms: u64,
    ) -> PeerDemotion {
        self.demote_if_gossip_stale_gated(
            id,
            expected_endpoint_id,
            observed_last_seen_ms,
            max_age_ms,
            |_| true,
        )
    }

    /// [`Self::demote_if_gossip_stale`] with a caller veto under the same lock: `commit` decides
    /// [`PeerDemotion::Demoted`] or [`PeerDemotion::Deferred`].
    pub fn demote_if_gossip_stale_gated(
        &self,
        id: &str,
        expected_endpoint_id: Option<&str>,
        observed_last_seen_ms: Option<u64>,
        max_age_ms: u64,
        commit: impl FnOnce(&PeerIdentity) -> bool,
    ) -> PeerDemotion {
        let now = now_ms();
        let mut peers = self.peers.write();
        let name = if let Some(expected) = expected_endpoint_id {
            let Some(peer) = peers.get(id) else {
                return PeerDemotion::Missing;
            };
            if peer.peer_id.as_deref() != Some(expected) {
                return PeerDemotion::Missing;
            }
            id.to_string()
        } else {
            let Some(name) = Self::peer_name_for(&peers, id) else {
                return PeerDemotion::Missing;
            };
            name
        };
        let Some(peer) = peers.get_mut(&name) else {
            return PeerDemotion::Missing;
        };
        let identity = Self::identity(peer);
        let current_fresh = gossip_timestamp_fresh(peer.last_seen_ms, now, max_age_ms);
        let observed_fresh =
            observed_last_seen_ms.is_some_and(|seen| gossip_timestamp_fresh(seen, now, max_age_ms));
        if current_fresh || observed_fresh {
            return PeerDemotion::GossipAlive(identity);
        }
        if !commit(&identity) {
            return PeerDemotion::Deferred(identity);
        }
        peer.latency_ms = u64::MAX;
        peer.healthy = false;
        PeerDemotion::Demoted(identity)
    }

    /// Every stored peer (stale ones included, unlike `nodes()`) with whether
    /// its gossiped `last_seen_ms` is fresh within `max_age_ms` right now.
    pub fn gossip_freshness(&self, max_age_ms: u64) -> Vec<(PeerIdentity, bool)> {
        let now = now_ms();
        self.peers
            .read()
            .values()
            .map(|p| {
                (
                    Self::identity(p),
                    gossip_timestamp_fresh(p.last_seen_ms, now, max_age_ms),
                )
            })
            .collect()
    }

    /// Restore every currently-fresh withdrawn peer and capture each immutable
    /// endpoint id while the same registry write lock still protects the change.
    pub fn restore_gossip_fresh(&self, max_age_ms: u64) -> Vec<String> {
        let now = now_ms();
        let mut peers = self.peers.write();
        let mut restored = Vec::new();
        for peer in peers.values_mut() {
            if peer.healthy || !gossip_timestamp_fresh(peer.last_seen_ms, now, max_age_ms) {
                continue;
            }
            let Some(endpoint_id) = peer.peer_id.clone().filter(|id| !id.is_empty()) else {
                continue;
            };
            peer.healthy = true;
            if peer.latency_ms == u64::MAX || peer.latency_ms == 0 {
                peer.latency_ms = Self::RESTORED_LATENCY_MS;
            }
            restored.push(endpoint_id);
        }
        restored
    }

    /// Read-only view of the unhealthy-but-gossip-fresh peers `restore_gossip_fresh` would restore
    /// — the input to the health loop's restore hysteresis.
    pub fn gossip_fresh_unhealthy(&self, max_age_ms: u64) -> Vec<String> {
        let now = now_ms();
        let peers = self.peers.read();
        peers
            .values()
            .filter(|p| !p.healthy && gossip_timestamp_fresh(p.last_seen_ms, now, max_age_ms))
            .filter_map(|p| p.peer_id.clone().filter(|id| !id.is_empty()))
            .collect()
    }

    /// Restore only the named candidates — the hysteresis-gated half of `restore_gossip_fresh` (the
    /// streak gate lives in the health loop).
    pub fn restore_only(&self, ids: &[String]) -> Vec<String> {
        let mut peers = self.peers.write();
        let mut restored = Vec::new();
        for id in ids {
            for peer in peers.values_mut() {
                if peer.peer_id.as_deref() == Some(id.as_str()) && !peer.healthy {
                    peer.healthy = true;
                    if peer.latency_ms == u64::MAX || peer.latency_ms == 0 {
                        peer.latency_ms = Self::RESTORED_LATENCY_MS;
                    }
                    restored.push(id.clone());
                    break;
                }
            }
        }
        restored
    }

    fn update_health(peer: &mut NodeInfo, latency_ms: u64, healthy: bool) {
        peer.latency_ms = if healthy && latency_ms == u64::MAX {
            Self::RESTORED_LATENCY_MS
        } else {
            latency_ms
        };
        peer.healthy = healthy;
        if healthy {
            peer.last_seen_ms = now_ms();
        }
    }

    /// Record measured health by current name or canonical endpoint id.
    pub fn set_health(&self, id: &str, latency_ms: u64, healthy: bool) {
        let mut peers = self.peers.write();
        let Some(name) = Self::peer_name_for(&peers, id) else {
            return;
        };
        if let Some(peer) = peers.get_mut(&name) {
            Self::update_health(peer, latency_ms, healthy);
        }
    }

    /// Record a probe completion only while the captured name still denotes the
    /// captured immutable endpoint.
    pub fn set_health_if_endpoint(
        &self,
        name: &str,
        expected_endpoint_id: &str,
        latency_ms: u64,
        healthy: bool,
    ) -> bool {
        let mut peers = self.peers.write();
        let Some(peer) = peers.get_mut(name) else {
            return false;
        };
        if peer.peer_id.as_deref() != Some(expected_endpoint_id) {
            return false;
        }
        Self::update_health(peer, latency_ms, healthy);
        true
    }

    /// Anycast selection: the lowest-latency healthy node, preferring `preferred` region when given.
    pub fn anycast(&self, preferred: Option<&str>) -> Option<NodeInfo> {
        let mut healthy: Vec<NodeInfo> = self.nodes().into_iter().filter(|n| n.healthy).collect();
        healthy.sort_by_key(|n| {
            if n.is_self {
                (0, 0)
            } else {
                peer_latency_sort_key(n.latency_ms)
            }
        });
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
        nodes.sort_by_key(|n| {
            let latency = if n.is_self {
                (0, 0)
            } else {
                peer_latency_sort_key(n.latency_ms)
            };
            (!n.healthy, latency)
        });
        nodes
    }

    pub fn region(&self) -> String {
        self.me.read().region.clone()
    }

    /// Upsert a peer from its OWN announcement — the only path that may rename (same `peer_id`, new
    /// `id`, old name evicted). Liveness is the record's origin timestamp; a relayed copy never
    /// re-stamps it, so a dead node's record freezes and ages out.
    pub fn upsert_peer_self_report(&self, mut peer: NodeInfo) {
        let now = now_ms();
        if peer.last_seen_ms > now.saturating_add(MAX_GOSSIP_FUTURE_SKEW_MS) {
            tracing::warn!(
                node = %peer.id,
                claimed_last_seen_ms = peer.last_seen_ms,
                receiver_now_ms = now,
                "peer self-report carried an implausibly future liveness timestamp; using direct receiver time"
            );
        } else if now.saturating_sub(peer.last_seen_ms) >= 30_000 {
            tracing::warn!(
                node = %peer.id,
                claimed_last_seen_ms = peer.last_seen_ms,
                receiver_now_ms = now,
                "peer self-report clock is behind the registry freshness window; using direct receiver time"
            );
        }
        peer.last_seen_ms = now;
        if let Some(pid) = peer.peer_id.clone().filter(|s| !s.is_empty()) {
            self.peers.write().retain(|name, existing| {
                if name == &peer.id {
                    existing.peer_id.as_deref() == Some(pid.as_str())
                } else {
                    existing.peer_id.as_deref() != Some(pid.as_str())
                }
            });
            self.upsert_peer(peer);
            let received_at = Instant::now();
            let mut received = self.gossip_received.write();
            received.retain(|_, at| {
                received_at.saturating_duration_since(*at) <= Duration::from_secs(24 * 60 * 60)
            });
            received.insert(pid, received_at);
        } else {
            self.upsert_peer(peer);
        }
    }

    /// Upsert a peer from a RELAYED copy: may update or introduce, never rename (a same-`peer_id`
    /// entry under another `id` makes this a stale echo, dropped).
    pub fn upsert_peer(&self, mut peer: NodeInfo) {
        peer.is_self = false;
        let now = now_ms();
        if peer.last_seen_ms > now.saturating_add(MAX_GOSSIP_FUTURE_SKEW_MS) {
            tracing::warn!(
                node = %peer.id,
                claimed_last_seen_ms = peer.last_seen_ms,
                receiver_now_ms = now,
                "dropping relayed peer record with implausibly future liveness timestamp"
            );
            return;
        }
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
            let existing_endpoint = existing.peer_id.as_deref().filter(|id| !id.is_empty());
            let incoming_endpoint = peer.peer_id.as_deref().filter(|id| !id.is_empty());
            if existing_endpoint.is_some()
                && incoming_endpoint.is_some()
                && existing_endpoint != incoming_endpoint
            {
                return;
            }
            if incoming_endpoint.is_none() {
                peer.peer_id.clone_from(&existing.peer_id);
            }
            peer.healthy = existing.healthy;
            peer.latency_ms = existing.latency_ms;
            let existing_last_seen =
                if existing.last_seen_ms > now.saturating_add(MAX_GOSSIP_FUTURE_SKEW_MS) {
                    tracing::warn!(
                        node = %peer.id,
                        claimed_last_seen_ms = existing.last_seen_ms,
                        receiver_now_ms = now,
                        "normalizing previously-stored future peer timestamp"
                    );
                    now
                } else {
                    existing.last_seen_ms
                };
            if peer.last_seen_ms < existing_last_seen {
                peer.dns_attest = existing.dns_attest.clone();
            }
            peer.last_seen_ms = peer.last_seen_ms.max(existing_last_seen);
            Self::keep_gained_capabilities(&mut peer, existing);
        }
        if peer.healthy && peer.latency_ms == u64::MAX {
            peer.latency_ms = Self::RESTORED_LATENCY_MS;
        }
        peers.insert(peer.id.clone(), peer);
    }

    /// Every capability advertisement only moves `None` → `Some` while a node keeps running, so a
    /// relayed copy older than the peer's own announce must never un-advertise one.
    fn keep_gained_capabilities(peer: &mut NodeInfo, existing: &NodeInfo) {
        if peer.guardian_iroh_addr.is_none() {
            peer.guardian_iroh_addr = existing.guardian_iroh_addr.clone();
        }
        if peer.wasm_runtime.is_none() {
            peer.wasm_runtime = existing.wasm_runtime;
        }
        if peer.bun_runtime.is_none() {
            peer.bun_runtime = existing.bun_runtime;
        }
        if peer.build_toolchains.is_none() {
            peer.build_toolchains = existing.build_toolchains.clone();
        }
        if peer.runtime_artifact_protocol.is_none() {
            peer.runtime_artifact_protocol = existing.runtime_artifact_protocol;
        }
        if peer.build_isolation_protocol.is_none() {
            peer.build_isolation_protocol = existing.build_isolation_protocol;
        }
        if peer.artifact_transfer_protocol.is_none() {
            peer.artifact_transfer_protocol = existing.artifact_transfer_protocol;
        }
    }

    /// All nodes (self first), with stale peers (>30s) dropped.
    pub fn nodes(&self) -> Vec<NodeInfo> {
        let now = now_ms();
        let mut out = vec![self.me()];
        let mut peers: Vec<NodeInfo> = self
            .peers
            .read()
            .values()
            .filter(|p| gossip_timestamp_fresh(p.last_seen_ms, now, 30_000))
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
            // These fixtures exercise region/latency selection, never Wasmer/Bun
            // placement — `None` is the honest value here (it is exactly what a
            // node that never ran the probe reports) and keeps them on the
            // not-capable path, which is what a non-Wasmer/non-Bun test node is.
            wasm_runtime: None,
            bun_runtime: None,
            build_toolchains: None,
            runtime_artifact_protocol: None,
            build_isolation_protocol: None,
            artifact_transfer_protocol: None,
            musql_engine: None,
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
            started_ms: 0,
            oom_restarts_24h: 0,
            unclean_restarts_24h: 0,
            last_oom_ms: None,
            backend: String::new(),
            provider: None,
            private_addr: None,
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
