//! Control-plane ownership: WHO is the single writer, and the fencing that keeps
//! it single during transitions.
//!
//! Two mechanisms, one resolution order (`control_plane_owner`):
//!
//! 1. **Operator-curated owner chain** (`HIVE_CP_OWNER_CHAIN`, comma-separated
//!    node names, e.g. `fc-sanjose,fc-bangkok,fc-virginia`): the FIRST entry that
//!    is currently healthy + cryptographically identified + publicly addressable
//!    is the control-plane owner. No open election over fleet membership — the
//!    candidate set is a short list the operator controls, entry order IS the
//!    failover order. A NAT'd dev laptop joining the mesh can never win.
//! 2. **Identity election fallback** (`billing_leader`): only when no chain is
//!    configured (single-node/dev, pre-migration deploys) or every chain entry is
//!    dark (availability beats strict staticness — logged loudly). Lowest healthy
//!    ed25519 `peer_id` wins, publicly-addressable nodes preferred; the legacy
//!    `HIVE_CP_LEADER` single pin is honored here.
//!
//! The [`Cluster`] instance tracks the observed owner and a monotonic **epoch**
//! that bumps on every ownership change (promotion/failover). The epoch is
//! gossiped fleet-wide via `NodeInfo.cp_epoch` (max-merge on ingest) and rides
//! every forwarded admin mutation as `x-hive-cp-epoch` — a fencing token: a node
//! whose view of ownership is stale (its epoch is behind the receiver's) gets its
//! forwarded writes refused instead of silently applied. This reuses the pattern
//! `lease.rs` already proves for container leases, applied to the control plane
//! per the architecture-audit proposal (static owner + curated backup chain +
//! epoch fencing).
//!
//! Placement/replication/failover of CONTAINERS stays on the dynamic HRW+lease
//! machinery in `lease.rs`/`schedule.rs` — dynamic election is the right tool
//! there and only there. `elect_among` is the shared, parameterized election
//! helper for per-candidate-set votes (used by `world_queue`'s per-tenant
//! primary), so hand-rolled reimplementations can't drift.

use parking_lot::RwLock;
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Serialize)]
pub struct ClusterStatus {
    /// Monotonic control-plane epoch: bumps on every ownership change. (Field
    /// keeps its legacy wire name `term` for dashboard compatibility; unlike the
    /// old decorative term, this value now fences real writes.)
    pub term: u64,
    /// The current control-plane owner (single write authority).
    pub leader: String,
    pub is_leader: bool,
    pub members: Vec<String>,
    pub consensus: &'static str,
}

pub struct Cluster {
    me: String,
    /// Monotonic ownership epoch. Starts at 1; bumps whenever the OBSERVED owner
    /// changes; max-merges with epochs gossiped by peers so the whole fleet
    /// converges on the highest epoch any node has witnessed.
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

    /// Record the currently-resolved owner. Bumps the epoch iff ownership
    /// CHANGED since the last observation (a promotion/failover event). Returns
    /// the current epoch. Called on every resolution (gossip-round cadence via
    /// `CloudState::control_plane_leader`), so the epoch tracks real transitions
    /// without any extra loop.
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

    /// Max-merge a peer's gossiped epoch. Keeps every node's fencing token at
    /// the highest value witnessed anywhere, so a write forwarded with a stale
    /// (lower) epoch is rejectable even before the local view fully converges.
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

    /// Resolve the control-plane owner: first healthy+identified+addressable
    /// chain entry, else (chain unset or fully dark) the identity election with
    /// the legacy pin. THE single resolution point for every single-writer role
    /// (admin mutations, billing meter, ACME, Vercel DNS) — see the module doc.
    pub fn control_plane_owner(
        chain: &[String],
        pref: Option<&str>,
        nodes: &[hive_edge::NodeInfo],
    ) -> Option<String> {
        let eligible = |name: &str| {
            nodes.iter().any(|n| {
                n.name == name
                    && n.healthy
                    && n.peer_id.is_some()
                    && (n.public_ip.as_deref().is_some_and(|ip| !ip.is_empty())
                        || n.public_ip6.as_deref().is_some_and(|ip| !ip.is_empty()))
            })
        };
        if !chain.is_empty() {
            for entry in chain {
                if eligible(entry) {
                    return Some(entry.clone());
                }
            }
            // Every curated candidate is dark: availability beats strict
            // staticness (the audit's HIVE_DNS_LEADER_NODE freeze is the
            // cautionary tale for wedging here) — but say so loudly, this is an
            // operator-attention condition, not a normal path.
            tracing::warn!(
                chain = ?chain,
                "control-plane owner chain has NO eligible (healthy+public) entry; falling back to identity election"
            );
        }
        Self::billing_leader_with_pref(pref, nodes)
    }

    /// Elect the BILLING coordinator from live mesh membership — web3-style: no
    /// hardcoded privileged node. Deterministic over every node's converged view:
    /// among HEALTHY nodes with a cryptographic iroh identity, the lowest ed25519
    /// `peer_id` wins (identity-ordered, not name-ordered, so the election is over
    /// unforgeable keys). Returns the winner's node NAME. Auto-failover is free:
    /// when the leader dies, health flips within the probe interval and every
    /// node's next evaluation converges on the next-lowest identity.
    ///
    /// PUBLICLY ADDRESSABLE nodes are preferred: this election also seats the
    /// control-plane WRITE authority, and the AdminAPI ingress forwards mutations
    /// to the leader over HTTPS pinned to the leader's PUBLIC IP — a NAT'd node
    /// (e.g. a dev laptop that joins the mesh with a low peer_id) winning the
    /// election 503s every forwarded write (`/v1/token` mints included, which
    /// blanks the whole dashboard) while looking perfectly healthy in gossip.
    /// Only when NO public candidate exists (single-node dev, all-LAN mesh) does
    /// the election fall back to every healthy identity, preserving dev behavior.
    pub fn billing_leader(nodes: &[hive_edge::NodeInfo]) -> Option<String> {
        let pref = std::env::var("HIVE_CP_LEADER").ok();
        Self::billing_leader_with_pref(pref.as_deref(), nodes)
    }

    /// `billing_leader` with an explicit operator preference (no env read) — the
    /// deterministic core. `HIVE_CP_LEADER=<node name>` pins the coordinator to a
    /// specific node (e.g. the one nearest the operator, since mutations serialize
    /// through it) — honored ONLY while that node is healthy and publicly
    /// addressable, so a dead or NAT'd pin falls back to the election instead of
    /// wedging the control plane. Must be set consistently fleet-wide.
    pub fn billing_leader_with_pref(
        pref: Option<&str>,
        nodes: &[hive_edge::NodeInfo],
    ) -> Option<String> {
        let addressable = |n: &hive_edge::NodeInfo| {
            n.public_ip.as_deref().is_some_and(|ip| !ip.is_empty())
                || n.public_ip6.as_deref().is_some_and(|ip| !ip.is_empty())
        };
        if let Some(p) = pref.filter(|p| !p.is_empty()) {
            if nodes
                .iter()
                .any(|n| n.name == p && n.healthy && n.peer_id.is_some() && addressable(n))
            {
                return Some(p.to_string());
            }
        }
        let elect = |require_public: bool| {
            nodes
                .iter()
                .filter(|n| n.healthy)
                .filter(|n| !require_public || addressable(n))
                .filter_map(|n| n.peer_id.as_ref().map(|p| (p.clone(), n.name.clone())))
                .min_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, name)| name)
        };
        elect(true).or_else(|| elect(false))
    }

    /// Shared, parameterized election over an explicit CANDIDATE LIST: among the
    /// named candidates that are healthy with a cryptographic identity, lowest
    /// `peer_id` wins; when none has an electable identity yet (e.g. iroh not
    /// bound), deterministic name-order fallback so a primary always exists.
    /// This is the one implementation per-candidate-set votes (per-tenant queue
    /// primaries, etc.) must call — the audit flagged the hand-rolled duplicate
    /// in `world_queue.rs` as a drift risk.
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
            last_oom_ms: None,
            backend: "mock".into(),
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
        let nodes = vec![
            public(node("sj", Some("ccc"), false), "170.106.158.151"), // primary dead
            public(node("bkk", Some("aaa"), true), "43.152.247.70"),
            public(node("va", Some("bbb"), true), "43.166.206.175"),
        ];
        let chain = vec!["sj".to_string(), "bkk".to_string(), "va".to_string()];
        assert_eq!(
            Cluster::control_plane_owner(&chain, None, &nodes).as_deref(),
            Some("bkk")
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
    fn owner_chain_all_dark_falls_back_to_election() {
        let nodes = vec![
            public(node("sj", Some("ccc"), false), "170.106.158.151"),
            public(node("other", Some("aaa"), true), "1.2.3.4"), // not in chain
        ];
        let chain = vec!["sj".to_string()];
        assert_eq!(
            Cluster::control_plane_owner(&chain, None, &nodes).as_deref(),
            Some("other")
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
