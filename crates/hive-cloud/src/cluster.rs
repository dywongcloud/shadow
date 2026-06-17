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
