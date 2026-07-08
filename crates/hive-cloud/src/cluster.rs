//! Internal cluster coordination (hidden from the dashboard; surfaced only as a
//! "coordinated cluster" status in the Network view).
//!
//! This is a lightweight Raft-style coordinator: nodes carry a monotonic `term`
//! and converge on a single `leader` for the cloud, used to decide which node
//! owns cross-region actions (e.g. which node performs a deployment's canonical
//! write). Leader selection is deterministic over the live mesh membership
//! (highest term, then lowest node id), refreshed each gossip round — equivalent
//! to a stable Raft leader without elections churn for this study build.
//!
//! For production-grade replicated consensus, enable the `raft` cargo feature to
//! route through [openraft] (a full Raft log + state machine). This module is the
//! always-on default so single-node and small meshes work out of the box.
//!
//! [openraft]: https://github.com/databendlabs/openraft

use parking_lot::RwLock;
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Serialize)]
pub struct ClusterStatus {
    pub term: u64,
    pub leader: String,
    pub is_leader: bool,
    pub members: Vec<String>,
    pub consensus: &'static str,
}

pub struct Cluster {
    me: String,
    term: RwLock<u64>,
    leader: RwLock<String>,
}

impl Cluster {
    pub fn new(me: String) -> Arc<Cluster> {
        Arc::new(Cluster {
            term: RwLock::new(1),
            leader: RwLock::new(me.clone()),
            me,
        })
    }

    /// Recompute the leader from the current mesh membership. Deterministic:
    /// lowest node id wins, and the term bumps whenever the leader changes (the
    /// observable effect of a Raft election).
    pub fn reconcile(&self, mut members: Vec<String>) {
        if !members.contains(&self.me) {
            members.push(self.me.clone());
        }
        members.sort();
        members.dedup();
        let new_leader = members.first().cloned().unwrap_or_else(|| self.me.clone());
        let mut leader = self.leader.write();
        if *leader != new_leader {
            *self.term.write() += 1;
            tracing::debug!(term = *self.term.read(), leader = %new_leader, "cluster leader changed");
            *leader = new_leader;
        }
    }

    pub fn is_leader(&self) -> bool {
        *self.leader.read() == self.me
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
    pub fn billing_leader_with_pref(pref: Option<&str>, nodes: &[hive_edge::NodeInfo]) -> Option<String> {
        let addressable = |n: &hive_edge::NodeInfo| {
            n.public_ip.as_deref().is_some_and(|ip| !ip.is_empty())
                || n.public_ip6.as_deref().is_some_and(|ip| !ip.is_empty())
        };
        if let Some(p) = pref.filter(|p| !p.is_empty()) {
            if nodes.iter().any(|n| n.name == p && n.healthy && n.peer_id.is_some() && addressable(n)) {
                return Some(p.to_string());
            }
        }
        let elect = |require_public: bool| {
            nodes
                .iter()
                .filter(|n| n.healthy)
                .filter(|n| {
                    !require_public
                        || n.public_ip.as_deref().is_some_and(|ip| !ip.is_empty())
                        || n.public_ip6.as_deref().is_some_and(|ip| !ip.is_empty())
                })
                .filter_map(|n| n.peer_id.as_ref().map(|p| (p.clone(), n.name.clone())))
                .min_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, name)| name)
        };
        elect(true).or_else(|| elect(false))
    }

    pub fn status(&self, members: Vec<String>) -> ClusterStatus {
        ClusterStatus {
            term: *self.term.read(),
            leader: self.leader.read().clone(),
            is_leader: self.is_leader(),
            members,
            consensus: if cfg!(feature = "raft") { "openraft" } else { "coordinator" },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, peer_id: Option<&str>, healthy: bool) -> hive_edge::NodeInfo {
        hive_edge::NodeInfo {
            id: name.into(),
            name: name.into(),
            region: "test".into(),
            public_url: String::new(),
            public_ip: None,
            public_ip6: None,
            peer_id: peer_id.map(|s| s.to_string()),
            iroh_addr: None,
            guardian_iroh_addr: None,
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
            backend: "mock".into(),
        }
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

    fn public(mut n: hive_edge::NodeInfo, ip: &str) -> hive_edge::NodeInfo {
        n.public_ip = Some(ip.into());
        n
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
        assert_eq!(Cluster::billing_leader_with_pref(Some("sj"), &nodes).as_deref(), Some("sj"));
    }

    #[test]
    fn leader_pref_falls_back_when_pin_is_dead_or_natted() {
        let dead = vec![
            public(node("sj", Some("ccc"), false), "170.106.158.151"), // pinned but dead
            public(node("bkk", Some("aaa"), true), "43.152.247.70"),
        ];
        assert_eq!(Cluster::billing_leader_with_pref(Some("sj"), &dead).as_deref(), Some("bkk"));
        let natted = vec![
            node("laptop", Some("aaa"), true), // pinned but no public_ip
            public(node("bkk", Some("bbb"), true), "43.152.247.70"),
        ];
        assert_eq!(Cluster::billing_leader_with_pref(Some("laptop"), &natted).as_deref(), Some("bkk"));
        // Unknown pin name → election.
        assert_eq!(Cluster::billing_leader_with_pref(Some("ghost"), &dead).as_deref(), Some("bkk"));
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
}
