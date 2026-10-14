//! Storage broker: the single admission point for tenant-facing storage operations (snapshot create
//! and delete for now; dataset and quota operations join it here as they are built, not as separate
//! unguarded paths).
//!
//! # Why its own module
//!
//! Every check below is the same shape of bug already found and fixed twice: a tenant-controlled
//! string reaching a filesystem operation without validation. `container_volume_cfg` is safe by
//! construction (sanitize, then prefix) and `snapshot::safe_component` was built that way from the
//! start. The broker is where those checks compose (ownership, then identifier safety, then
//! capacity), as ONE module so a third call site (dataset create, quota set, whatever comes next)
//! inherits the same order by calling it instead of re-deriving it inline, which is how the first
//! two nearly diverged.
//!
//! # Order matters
//!
//! 1. **Ownership**: is the caller's tenant the deployment's tenant, through the
//! [`crate::admin::project_owned_by`] every other tenant-facing route trusts. Checked FIRST: an
//! attacker probing a real project gets the same 403 as one probing a nonexistent one, not a
//! different error that confirms the project exists.
//! 2. **Identifier safety**: [`hive_backend::snapshot::snapshot_image`]'s own `safe_component`
//! gate, invoked here too so a caller of THIS module gets the same rejection before any filesystem
//! call, not just inside the primitive.
//! 3. **Quota**: a snapshot-COUNT ceiling per deployment (the caller passes the size of that
//! deployment's snapshot directory), deliberately not bytes: there is no cheap way to attribute
//! reflink-shared bytes to one tenant (two snapshots share extents, so "bytes used by a snapshot"
//! is not well defined once anything has diverged), and an unbounded count is still a real
//! resource-exhaustion vector (podman lock-pool exhaustion is the standing example of what an
//! uncapped per-tenant resource does to a shared host).

use std::sync::Arc;

use crate::state::CloudState;

/// Default ceiling on live snapshots per deployment; `HIVE_SNAPSHOT_QUOTA` tunes it fleet-wide.
/// There is no per-tenant override yet: add one the way the `team_plan`-driven limits work
/// elsewhere before it is needed.
fn snapshot_quota() -> usize {
    std::env::var("HIVE_SNAPSHOT_QUOTA")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(20)
}

#[derive(Debug)]
pub enum BrokerError {
    /// Caller's tenant does not own this project. Message is deliberately
    /// generic (see module doc point 1) — do not add detail here.
    NotOwner,
    /// The snapshot id itself is unsafe (see `snapshot::safe_component`).
    InvalidId(String),
    /// The deployment is already at its snapshot ceiling.
    QuotaExceeded { limit: usize, have: usize },
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokerError::NotOwner => write!(f, "project belongs to a different team"),
            BrokerError::InvalidId(id) => write!(f, "invalid snapshot id: {id}"),
            BrokerError::QuotaExceeded { limit, have } => {
                write!(f, "snapshot quota exceeded: {have}/{limit}")
            }
        }
    }
}

/// Authorizes a snapshot CREATE for `project` on behalf of `caller_tenant`. `Ok(())` means the
/// caller may proceed; the filesystem work (`hive_backend::snapshot::snapshot_image`) stays the
/// caller's, since this function only decides and never touches a disk, so it can be reasoned about
/// independent of any node.
pub fn authorize_create(
    cloud: &Arc<CloudState>,
    caller_tenant: &str,
    project: &str,
    snapshot_id: &str,
    existing_count: usize,
) -> Result<(), BrokerError> {
    if !crate::admin::project_owned_by(cloud, project, caller_tenant) {
        return Err(BrokerError::NotOwner);
    }
    if !snapshot_id_is_safe(snapshot_id) {
        return Err(BrokerError::InvalidId(snapshot_id.to_string()));
    }
    let limit = snapshot_quota();
    if existing_count >= limit {
        return Err(BrokerError::QuotaExceeded {
            limit,
            have: existing_count,
        });
    }
    Ok(())
}

/// Authorizes a snapshot DELETE: no quota check (deleting only reduces usage), but the same
/// ownership and identifier gates, because a delete path that trusts an id less than the create
/// path is a real, exploitable asymmetry (an attacker who cannot CREATE a traversal id could still
/// DELETE via one if some other component ever wrote a file at that path).
pub fn authorize_delete(
    cloud: &Arc<CloudState>,
    caller_tenant: &str,
    project: &str,
    snapshot_id: &str,
) -> Result<(), BrokerError> {
    if !crate::admin::project_owned_by(cloud, project, caller_tenant) {
        return Err(BrokerError::NotOwner);
    }
    if !snapshot_id_is_safe(snapshot_id) {
        return Err(BrokerError::InvalidId(snapshot_id.to_string()));
    }
    Ok(())
}

/// Mirrors `hive_backend::snapshot::safe_component` exactly, duplicated rather than imported
/// because the crates' dependency direction does not run that way. The RULE must never drift: any
/// change to `safe_component` must be mirrored here in the same commit, or the broker could
/// authorize an id the primitive then rejects (safe: fails closed) or, the dangerous direction, the
/// primitive could be loosened without the broker noticing.
fn snapshot_id_is_safe(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != ".."
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        && !s.contains("..")
}
