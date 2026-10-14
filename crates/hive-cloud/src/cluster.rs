//! Control-plane ownership: WHO is the single writer, and the fencing that keeps it single. Two
//! mechanisms, never mixed (`control_plane_owner_with_source`):
//!
//! 1. **Owner chain** (`HIVE_CP_OWNER_CHAIN`): the STRICT owner is the first entry PRESENT in
//! `registry.nodes()`, identified and publicly addressable; the observer's own `healthy` flag is
//! never consulted. No qualifying entry is `None`, a HOLD, never the identity election (a chain
//! dark to this observer is a partitioned minority's view), and the
//! `HIVE_CP_LEADER`/`HIVE_DNS_LEADER_NODE` pins are ignored.
//! 2. **Identity election** (`billing_leader`): only when no chain is set: the lowest healthy
//! ed25519 `peer_id`, public nodes preferred, the legacy `HIVE_CP_LEADER` pin honored.
//!
//! Background jobs ask `leadership::may_act`, request-path forwarding
//! `CloudState::control_plane_leader`. [`Cluster`] tracks the observed owner and a monotonic
//! **epoch** (bumped on every ownership change, gossiped as `NodeInfo.cp_epoch`, max-merged) that
//! rides forwarded admin mutations as `x-hive-cp-epoch`: a fencing token, so a node whose epoch is
//! behind has its forwarded writes refused. `elect_among` is the shared election for
//! per-candidate-set votes.

use parking_lot::RwLock;
use serde::Serialize;
use std::sync::Arc;

/// Minimum spacing between two "chain has no present entry: HOLDING" warnings.
const HOLD_WARN_INTERVAL_MS: u64 = 5 * 60 * 1000;

/// Where [`Cluster::control_plane_owner_with_source`] found its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnerSource {
    /// The strict owner of a configured `HIVE_CP_OWNER_CHAIN`.
    Chain,
    /// The identity election — only ever used when NO chain is configured.
    Election,
    /// Chain configured, no strict owner: a best-effort forwarding target,
    /// never an authority (see the resolver's doc).
    Fallback,
}

impl OwnerSource {
    /// Whether this answer names a real owner (a write authority and a
    /// wholesale-adoption source), as opposed to a forwarding guess.
    pub fn is_authority(self) -> bool {
        matches!(self, OwnerSource::Chain | OwnerSource::Election)
    }

    /// Whether a follower may REPLACE a whole store with `owner`'s snapshot: only the chain HEAD or
    /// the identity election, since a backup owner differs per observer and would hand out a stale
    /// fork; failover owners are merge-only.
    pub fn may_adopt_wholesale_from(self, owner: &str, chain: &[String]) -> bool {
        match self {
            OwnerSource::Election => true,
            OwnerSource::Chain => chain.first().map(String::as_str) == Some(owner),
            OwnerSource::Fallback => false,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct ClusterStatus {
    /// Monotonic control-plane epoch, bumped on every ownership change. Keeps its legacy wire name
    /// `term` for dashboard compatibility.
    pub term: u64,
    /// The current control-plane owner (single write authority).
    pub leader: String,
    pub is_leader: bool,
    pub members: Vec<String>,
    pub consensus: &'static str,
}

pub struct Cluster {
    me: String,
    /// Monotonic ownership epoch: starts at 1, bumps whenever the OBSERVED owner changes, and
    /// max-merges with gossiped epochs so the fleet converges on the highest any node witnessed.
    epoch: RwLock<u64>,
    /// The owner as last observed by this node.
    owner: RwLock<String>,
}

impl Cluster {
    pub fn new(me: String) -> Arc<Cluster> {
        Arc::new(Cluster {
            epoch: RwLock::new(1),
            owner: RwLock::new(me.clone()),
            me,
        })
    }

    /// Record the resolved owner; bumps the epoch iff ownership CHANGED since the last observation,
    /// and returns the current epoch.
    pub fn observe_owner(&self, owner: &str) -> u64 {
        {
            let mut cur = self.owner.write();
            if *cur != owner {
                let mut e = self.epoch.write();
                *e += 1;
                tracing::info!(epoch = *e, owner = %owner, previous = %cur, "control-plane owner changed");
                *cur = owner.to_string();
            }
        }
        *self.epoch.read()
    }

    /// Max-merge a peer's gossiped epoch, so a write forwarded with a stale (lower) epoch is
    /// rejectable even before the local view converges.
    pub fn adopt_epoch(&self, remote: u64) {
        if remote > *self.epoch.read() {
            *self.epoch.write() = remote;
        }
    }

    pub fn epoch(&self) -> u64 {
        *self.epoch.read()
    }

    /// The operator-curated owner chain from `HIVE_CP_OWNER_CHAIN` (empty when
    /// unset — callers then fall back to the identity election).
    pub fn owner_chain_from_env() -> Vec<String> {
        std::env::var("HIVE_CP_OWNER_CHAIN")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The STRICT chain owner: the first `chain` entry present in `nodes` with an identity and a
    /// public address; `None` (HOLD) when none qualifies.
    pub fn strict_chain_owner(chain: &[String], nodes: &[hive_edge::NodeInfo]) -> Option<String> {
        chain
            .iter()
            .find(|entry| {
                nodes.iter().any(|n| {
                    &n.name == *entry
                        && n.peer_id.as_deref().is_some_and(|id| !id.is_empty())
                        && Self::addressable(n)
                })
            })
            .cloned()
    }

    /// THE "has a public address" predicate (an empty or blank string is no address): the strict
    /// chain owner, the identity election and placement (`schedule::has_public_address`) all ask
    /// it, so no caller can count a node the others reject.
    pub(crate) fn addressable(n: &hive_edge::NodeInfo) -> bool {
        n.public_ip
            .as_deref()
            .is_some_and(|ip| !ip.trim().is_empty())
            || n.public_ip6
                .as_deref()
                .is_some_and(|ip| !ip.trim().is_empty())
    }

    /// Resolve the control-plane owner: the strict chain owner (or `None`) with a chain, the
    /// identity election with the legacy pin without one. THE owner every single-writer role sits
    /// on.
    pub fn control_plane_owner(
        chain: &[String],
        pref: Option<&str>,
        nodes: &[hive_edge::NodeInfo],
    ) -> Option<String> {
        Self::control_plane_owner_with_source(chain, pref, nodes, "")
            .filter(|(_, source)| source.is_authority())
            .map(|(owner, _)| owner)
    }

    /// [`Self::control_plane_owner`] plus its source and, while a chain is set with no strict
    /// owner, a FORWARDING-only target ([`OwnerSource::Fallback`]): never an authority, nothing is
    /// adopted from it and it never makes `me` the owner.
    pub fn control_plane_owner_with_source(
        chain: &[String],
        pref: Option<&str>,
        nodes: &[hive_edge::NodeInfo],
        me: &str,
    ) -> Option<(String, OwnerSource)> {
        if chain.is_empty() {
            return Self::billing_leader_with_pref(pref, nodes)
                .map(|owner| (owner, OwnerSource::Election));
        }
        if let Some(owner) = Self::strict_chain_owner(chain, nodes) {
            return Some((owner, OwnerSource::Chain));
        }
        static LAST_HOLD_WARN_MS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let now = hive_core::now_ms();
        let last = LAST_HOLD_WARN_MS.load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) >= HOLD_WARN_INTERVAL_MS {
            LAST_HOLD_WARN_MS.store(now, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                chain = ?chain,
                "control-plane owner chain has NO present, identified, public entry in this \
                 node's gossip view -- HOLDING (no owner; leader-only jobs pause everywhere this \
                 view holds, forwarded writes answer a retryable 503). Never falls back to the \
                 identity election while a chain is set."
            );
        }
        chain
            .iter()
            .find(|entry| *entry != me && nodes.iter().any(|n| &n.name == *entry))
            .map(|entry| (entry.clone(), OwnerSource::Fallback))
    }

    /// Elect the BILLING coordinator: among HEALTHY nodes with an iroh identity the lowest
    /// `peer_id` wins and its NAME is returned. Public nodes are preferred, because the ingress
    /// forwards mutations to the leader's public IP and a NAT'd winner would 503 every forwarded
    /// write; with no public candidate every healthy identity competes.
    pub fn billing_leader(nodes: &[hive_edge::NodeInfo]) -> Option<String> {
        let pref = std::env::var("HIVE_CP_LEADER").ok();
        Self::billing_leader_with_pref(pref.as_deref(), nodes)
    }

    /// `billing_leader` with an explicit operator preference (no env read). `HIVE_CP_LEADER=<node>`
    /// is honored ONLY while that node is healthy and public, so a dead or NAT'd pin falls back to
    /// the election.
    pub fn billing_leader_with_pref(
        pref: Option<&str>,
        nodes: &[hive_edge::NodeInfo],
    ) -> Option<String> {
        if let Some(p) = pref.filter(|p| !p.is_empty()) {
            if nodes
                .iter()
                .any(|n| n.name == p && n.healthy && n.peer_id.is_some() && Self::addressable(n))
            {
                return Some(p.to_string());
            }
        }
        let elect = |require_public: bool| {
            nodes
                .iter()
                .filter(|n| n.healthy)
                .filter(|n| !require_public || Self::addressable(n))
                .filter_map(|n| n.peer_id.as_ref().map(|p| (p.clone(), n.name.clone())))
                .min_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, name)| name)
        };
        elect(true).or_else(|| elect(false))
    }

    /// Election over an explicit CANDIDATE LIST: the lowest `peer_id` among healthy identified
    /// candidates, else the first by name, so a primary always exists. Per-candidate-set votes must
    /// call this so copies cannot drift.
    pub fn elect_among(candidates: &[String], nodes: &[hive_edge::NodeInfo]) -> Option<String> {
        if candidates.is_empty() {
            return None;
        }
        let winner = nodes
            .iter()
            .filter(|n| candidates.contains(&n.name) && n.healthy)
            .filter_map(|n| n.peer_id.as_ref().map(|p| (p.clone(), n.name.clone())))
            .min_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, name)| name);
        winner.or_else(|| {
            let mut names = candidates.to_vec();
            names.sort();
            names.first().cloned()
        })
    }

    pub fn status(&self, members: Vec<String>) -> ClusterStatus {
        let owner = self.owner.read().clone();
        ClusterStatus {
            term: *self.epoch.read(),
            is_leader: owner == self.me,
            leader: owner,
            members,
            consensus: if cfg!(feature = "raft") {
                "openraft"
            } else if !Self::owner_chain_from_env().is_empty() {
                "owner-chain"
            } else {
                "coordinator"
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, peer_id: Option<&str>, healthy: bool) -> hive_edge::NodeInfo {
        hive_edge::NodeInfo {
            gpu_count: 0,
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
            region: "test".into(),
            public_url: String::new(),
            public_ip: None,
            public_ip6: None,
            peer_id: peer_id.map(|s| s.to_string()),
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
            lat: None,
            lon: None,
            city: None,
            country: None,
            cpu_cores: 1,
            mem_total_mb: 1024,
            disk_total_gb: 10,
            disk_free_gb: 0,
            gpu_free_mb: None,
            started_ms: 0,
            oom_restarts_24h: 0,
            unclean_restarts_24h: 0,
            last_oom_ms: None,
            backend: "mock".into(),
            provider: None,
            private_addr: None,
        }
    }

    fn public(mut n: hive_edge::NodeInfo, ip: &str) -> hive_edge::NodeInfo {
        n.public_ip = Some(ip.into());
        n
    }

    #[test]
    fn billing_leader_is_lowest_healthy_identity() {
        let nodes = vec![
            node("va", Some("ccc"), true),
            node("bkk", Some("aaa"), true),
            node("sj", Some("bbb"), true),
        ];
        assert_eq!(Cluster::billing_leader(&nodes).as_deref(), Some("bkk"));
    }

    #[test]
    fn billing_leader_fails_over_when_leader_unhealthy() {
        // Auto-failover: the lowest identity is DOWN → next-lowest healthy wins.
        let nodes = vec![
            node("bkk", Some("aaa"), false), // dead
            node("sj", Some("bbb"), true),
            node("va", Some("ccc"), true),
        ];
        assert_eq!(Cluster::billing_leader(&nodes).as_deref(), Some("sj"));
    }

    #[test]
    fn billing_leader_ignores_nodes_without_identity() {
        // No cryptographic identity (no iroh) → not electable; none at all → None.
        let nodes = vec![node("x", None, true), node("y", Some("zzz"), true)];
        assert_eq!(Cluster::billing_leader(&nodes).as_deref(), Some("y"));
        assert_eq!(Cluster::billing_leader(&[node("x", None, true)]), None);
    }

    #[test]
    fn billing_leader_prefers_publicly_addressable_nodes() {
        // A NAT'd node with the LOWEST identity must lose to a public one: the
        // control-plane ingress can only forward writes to a public IP.
        let nodes = vec![
            node("laptop", Some("aaa"), true), // lowest id, but no public_ip
            public(node("bkk", Some("bbb"), true), "43.152.247.70"),
            public(node("sj", Some("ccc"), true), "170.106.158.151"),
        ];
        assert_eq!(Cluster::billing_leader(&nodes).as_deref(), Some("bkk"));
    }

    #[test]
    fn billing_leader_falls_back_when_no_public_candidate() {
        // All-private mesh (local dev): the old lowest-healthy-identity election.
        let nodes = vec![node("a", Some("bbb"), true), node("b", Some("aaa"), true)];
        assert_eq!(Cluster::billing_leader(&nodes).as_deref(), Some("b"));
        // Empty public_ip string is NOT addressable — still falls back.
        let mut c = node("c", Some("aaa"), true);
        c.public_ip = Some(String::new());
        assert_eq!(Cluster::billing_leader(&[c]).as_deref(), Some("c"));
    }

    #[test]
    fn leader_pref_pins_healthy_public_node_over_lower_identity() {
        let nodes = vec![
            public(node("bkk", Some("aaa"), true), "43.152.247.70"), // lowest id
            public(node("sj", Some("ccc"), true), "170.106.158.151"),
        ];
        assert_eq!(
            Cluster::billing_leader_with_pref(Some("sj"), &nodes).as_deref(),
            Some("sj")
        );
    }

    #[test]
    fn leader_pref_falls_back_when_pin_is_dead_or_natted() {
        let dead = vec![
            public(node("sj", Some("ccc"), false), "170.106.158.151"), // pinned but dead
            public(node("bkk", Some("aaa"), true), "43.152.247.70"),
        ];
        assert_eq!(
            Cluster::billing_leader_with_pref(Some("sj"), &dead).as_deref(),
            Some("bkk")
        );
        let natted = vec![
            node("laptop", Some("aaa"), true), // pinned but no public_ip
            public(node("bkk", Some("bbb"), true), "43.152.247.70"),
        ];
        assert_eq!(
            Cluster::billing_leader_with_pref(Some("laptop"), &natted).as_deref(),
            Some("bkk")
        );
        // Unknown pin name → election.
        assert_eq!(
            Cluster::billing_leader_with_pref(Some("ghost"), &dead).as_deref(),
            Some("bkk")
        );
    }

    #[test]
    fn billing_leader_skips_unhealthy_public_nodes() {
        // A dead public node must not shadow a healthy private-only mesh.
        let nodes = vec![
            public(node("bkk", Some("aaa"), false), "43.152.247.70"), // dead
            node("laptop", Some("bbb"), true),
        ];
        assert_eq!(Cluster::billing_leader(&nodes).as_deref(), Some("laptop"));
    }

    // ---- owner chain ----

    #[test]
    fn owner_chain_first_eligible_entry_wins() {
        let nodes = vec![
            public(node("sj", Some("ccc"), true), "170.106.158.151"),
            public(node("bkk", Some("aaa"), true), "43.152.247.70"), // lower id — must NOT matter
        ];
        let chain = vec!["sj".to_string(), "bkk".to_string()];
        assert_eq!(
            Cluster::control_plane_owner(&chain, None, &nodes).as_deref(),
            Some("sj")
        );
    }

    #[test]
    fn owner_chain_dark_primary_promotes_next_backup() {
        // "Dark" = absent from the gossip-fresh set, not this observer's
        // `healthy` verdict: a present-but-locally-unhealthy primary (a
        // transport-only fault from this observer) stays the owner.
        let backups = vec![
            public(node("bkk", Some("aaa"), true), "43.152.247.70"),
            public(node("va", Some("bbb"), true), "43.166.206.175"),
        ];
        let chain = vec!["sj".to_string(), "bkk".to_string(), "va".to_string()];
        assert_eq!(
            Cluster::control_plane_owner(&chain, None, &backups).as_deref(),
            Some("bkk")
        );
        let mut with_unhealthy_primary = backups.clone();
        with_unhealthy_primary.push(public(node("sj", Some("ccc"), false), "170.106.158.151"));
        assert_eq!(
            Cluster::control_plane_owner(&chain, None, &with_unhealthy_primary).as_deref(),
            Some("sj")
        );
    }

    #[test]
    fn owner_chain_natted_entry_is_skipped() {
        // A chain entry without a public address can't receive forwarded writes —
        // skipped exactly like a dead one.
        let nodes = vec![
            node("laptop", Some("aaa"), true), // in-chain but NAT'd
            public(node("bkk", Some("bbb"), true), "43.152.247.70"),
        ];
        let chain = vec!["laptop".to_string(), "bkk".to_string()];
        assert_eq!(
            Cluster::control_plane_owner(&chain, None, &nodes).as_deref(),
            Some("bkk")
        );
    }

    #[test]
    fn owner_chain_all_dark_holds_instead_of_electing() {
        let nodes = vec![
            public(node("other", Some("aaa"), true), "1.2.3.4"), // not in chain
        ];
        let chain = vec!["sj".to_string()];
        assert_eq!(
            Cluster::control_plane_owner(&chain, Some("other"), &nodes),
            None
        );
        assert_eq!(
            Cluster::control_plane_owner_with_source(&chain, None, &nodes, "other"),
            None
        );
    }

    #[test]
    fn empty_chain_preserves_election_behavior() {
        let nodes = vec![
            public(node("bkk", Some("aaa"), true), "43.152.247.70"),
            public(node("sj", Some("ccc"), true), "170.106.158.151"),
        ];
        assert_eq!(
            Cluster::control_plane_owner(&[], None, &nodes).as_deref(),
            Some("bkk")
        );
        // Legacy single pin still honored on the fallback path.
        assert_eq!(
            Cluster::control_plane_owner(&[], Some("sj"), &nodes).as_deref(),
            Some("sj")
        );
    }

    // ---- epoch fencing ----

    #[test]
    fn epoch_bumps_only_on_ownership_change_and_max_merges() {
        let c = Cluster::new("me".into());
        assert_eq!(c.epoch(), 1);
        assert_eq!(c.observe_owner("me"), 1); // unchanged → no bump
        assert_eq!(c.observe_owner("bkk"), 2); // promotion → bump
        assert_eq!(c.observe_owner("bkk"), 2); // stable → no bump
        assert_eq!(c.observe_owner("sj"), 3); // failover → bump
        c.adopt_epoch(10); // peer witnessed later transitions
        assert_eq!(c.epoch(), 10);
        c.adopt_epoch(4); // stale gossip never regresses the fence
        assert_eq!(c.epoch(), 10);
    }

    // ---- shared candidate-set election ----

    #[test]
    fn elect_among_picks_lowest_identity_among_named_healthy_candidates() {
        let nodes = vec![
            node("a", Some("ccc"), true),
            node("b", Some("aaa"), true), // lowest id but NOT a candidate
            node("c", Some("bbb"), true),
        ];
        let cands = vec!["a".to_string(), "c".to_string()];
        assert_eq!(Cluster::elect_among(&cands, &nodes).as_deref(), Some("c"));
    }

    #[test]
    fn elect_among_falls_back_to_name_order_without_identities() {
        // No electable identity known yet (iroh not bound) → deterministic
        // name-order so a primary still exists.
        let nodes = vec![node("zeta", None, true)];
        let cands = vec!["zeta".to_string(), "alpha".to_string()];
        assert_eq!(
            Cluster::elect_among(&cands, &nodes).as_deref(),
            Some("alpha")
        );
        assert_eq!(Cluster::elect_among(&[], &nodes), None);
    }
}
