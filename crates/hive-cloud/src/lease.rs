//! Container placement leases — consensus-free single-owner coordination for
//! **stateful container deployments** (NOT functions/JS sites, which are
//! stateless and need no coordination).
//!
//! Design (see the RAFT analysis): we deliberately avoid a global Raft/etcd.
//! Instead, every node agrees on the *preferred owner* of a container via
//! **rendezvous (highest-random-weight) hashing** over the set of live nodes that
//! hold it — deterministic, so no election round-trip. A **fenced lease** (a
//! monotonic `epoch`) then guarantees safety: only the highest-epoch owner is
//! authoritative, so a partitioned old owner can't split-brain. On owner death
//! the lease expires and the next preferred live holder takes over with a bumped
//! epoch (automatic failover). State converges via gossip (highest epoch wins).

use std::collections::HashMap;

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// A fenced ownership lease for one container deployment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContainerLease {
    /// Deployment subdomain / project key.
    pub key: String,
    /// Owning node id.
    pub owner: String,
    /// Monotonic fencing token — bumped on every ownership change.
    pub epoch: u64,
    pub region: String,
    pub acquired_ms: u64,
    pub expires_ms: u64,
}

impl ContainerLease {
    pub fn is_live(&self, now: u64) -> bool {
        self.expires_ms > now
    }
}

pub struct LeaseStore {
    map: RwLock<HashMap<String, ContainerLease>>,
}

impl LeaseStore {
    pub fn new() -> LeaseStore {
        LeaseStore {
            map: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &str) -> Option<ContainerLease> {
        self.map.read().get(key).cloned()
    }

    pub fn list(&self) -> Vec<ContainerLease> {
        self.map.read().values().cloned().collect()
    }

    /// The current (live) owner of a key, if any.
    pub fn owner_of(&self, key: &str) -> Option<String> {
        let now = now_ms();
        self.map
            .read()
            .get(key)
            .filter(|l| l.is_live(now))
            .map(|l| l.owner.clone())
    }

    pub fn is_owner(&self, key: &str, node: &str) -> bool {
        self.owner_of(key).as_deref() == Some(node)
    }

    /// Merge a lease learned from a peer. Higher epoch always wins; on an epoch
    /// tie, the later expiry (a fresher renewal) wins. This is how lease state
    /// converges across the mesh without a consensus log.
    pub fn merge(&self, incoming: ContainerLease) {
        let mut m = self.map.write();
        match m.get(&incoming.key) {
            Some(cur)
                if cur.epoch > incoming.epoch
                    || (cur.epoch == incoming.epoch && cur.expires_ms >= incoming.expires_ms) => {}
            _ => {
                m.insert(incoming.key.clone(), incoming);
            }
        }
    }

    /// Acquire or renew the lease for `self_node`. Returns the lease iff we own it.
    /// - If we already own it → renew (same epoch, extend expiry).
    /// - If it's held by a *live* other owner → refuse (None).
    /// - If it's free or expired → take over with `epoch + 1` (the fencing bump).
    pub fn acquire_or_renew(
        &self,
        key: &str,
        self_node: &str,
        region: &str,
        ttl_ms: u64,
    ) -> Option<ContainerLease> {
        let now = now_ms();
        let mut m = self.map.write();
        match m.get(key) {
            Some(cur) if cur.owner == self_node => {
                let renewed = ContainerLease {
                    expires_ms: now + ttl_ms,
                    acquired_ms: cur.acquired_ms,
                    ..cur.clone()
                };
                m.insert(key.to_string(), renewed.clone());
                Some(renewed)
            }
            Some(cur) if cur.is_live(now) => None, // someone else holds a live lease
            other => {
                let epoch = other.map(|c| c.epoch + 1).unwrap_or(1);
                let lease = ContainerLease {
                    key: key.to_string(),
                    owner: self_node.to_string(),
                    epoch,
                    region: region.to_string(),
                    acquired_ms: now,
                    expires_ms: now + ttl_ms,
                };
                m.insert(key.to_string(), lease.clone());
                Some(lease)
            }
        }
    }

    /// Voluntarily give up a lease we hold (so the preferred node can take it).
    /// Keeps the record (expired) so the epoch keeps climbing on next acquire.
    pub fn release(&self, key: &str, self_node: &str) {
        let mut m = self.map.write();
        if let Some(cur) = m.get_mut(key) {
            if cur.owner == self_node {
                cur.expires_ms = 0; // mark expired immediately
            }
        }
    }
}

impl Default for LeaseStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic FNV-1a (stable across std versions, unlike DefaultHasher) so all
/// nodes compute identical rendezvous weights.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Rendezvous (HRW) hashing: the preferred owner of `key` among `nodes` is the
/// node maximizing hash(key+node). Deterministic → every node agrees without an
/// election. When a node leaves the set, only keys it owned move (to the next
/// highest), giving stable, minimal-churn failover.
pub fn hrw_owner(key: &str, nodes: &[String]) -> Option<String> {
    nodes
        .iter()
        .max_by_key(|n| fnv1a(&format!("{key}\u{0}{n}")))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrw_is_deterministic_and_stable() {
        let nodes = vec![
            "node-a".to_string(),
            "node-b".to_string(),
            "node-c".to_string(),
        ];
        let o1 = hrw_owner("proj-x", &nodes).unwrap();
        let o2 = hrw_owner("proj-x", &nodes).unwrap();
        assert_eq!(o1, o2, "HRW must be deterministic");
        assert!(nodes.contains(&o1));
        // Removing a non-owner doesn't change the owner.
        let without_other: Vec<String> = nodes
            .iter()
            .filter(|n| **n != "node-c" || o1 == "node-c")
            .cloned()
            .collect();
        if o1 != "node-c" {
            assert_eq!(hrw_owner("proj-x", &without_other).unwrap(), o1);
        }
    }

    #[test]
    fn lease_fencing_and_takeover() {
        let s = LeaseStore::new();
        // A acquires fresh → epoch 1.
        let a = s.acquire_or_renew("x", "node-a", "iad1", 10_000).unwrap();
        assert_eq!(a.epoch, 1);
        assert!(s.is_owner("x", "node-a"));
        // B cannot steal a live lease.
        assert!(s.acquire_or_renew("x", "node-b", "sfo1", 10_000).is_none());
        // A renews → same epoch.
        let a2 = s.acquire_or_renew("x", "node-a", "iad1", 10_000).unwrap();
        assert_eq!(a2.epoch, 1);
        // A releases (dies) → B takes over with a bumped (fencing) epoch.
        s.release("x", "node-a");
        let b = s.acquire_or_renew("x", "node-b", "sfo1", 10_000).unwrap();
        assert_eq!(b.epoch, 2, "takeover must bump the fencing epoch");
        assert!(s.is_owner("x", "node-b"));
    }

    #[test]
    fn merge_prefers_higher_epoch() {
        let s = LeaseStore::new();
        s.merge(ContainerLease {
            key: "x".into(),
            owner: "a".into(),
            epoch: 1,
            region: "iad1".into(),
            acquired_ms: 0,
            expires_ms: u64::MAX,
        });
        s.merge(ContainerLease {
            key: "x".into(),
            owner: "b".into(),
            epoch: 2,
            region: "sfo1".into(),
            acquired_ms: 0,
            expires_ms: u64::MAX,
        });
        assert_eq!(s.get("x").unwrap().owner, "b");
        // A stale lower-epoch update is ignored.
        s.merge(ContainerLease {
            key: "x".into(),
            owner: "a".into(),
            epoch: 1,
            region: "iad1".into(),
            acquired_ms: 0,
            expires_ms: u64::MAX,
        });
        assert_eq!(s.get("x").unwrap().owner, "b");
    }
}
