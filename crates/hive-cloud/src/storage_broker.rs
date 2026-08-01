//! Storage broker: the single admission point for tenant-facing storage
//! operations (snapshot create/delete for now — dataset/quota operations join
//! it here as they're built, not as separate unguarded paths).
//!
//! # Why this exists as its own module rather than inline in the route handler
//!
//! Every one of the checks below is the SAME shape of bug this session already
//! found and fixed twice: a tenant-controlled string reaching a filesystem
//! operation without validation. `container_volume_cfg` was safe by
//! construction (sanitize-then-prefix); `snapshot::safe_component` was built
//! that way from the start. A storage broker is where those checks compose —
//! ownership, then identifier safety, then capacity — and it exists as one
//! module specifically so a THIRD call site (dataset create, quota set,
//! whatever comes next) inherits the same order of checks by calling this
//! instead of re-deriving it inline, which is how the first two nearly
//! diverged from each other.
//!
//! # Order matters
//!
//! 1. **Ownership** — is the caller's tenant the deployment's tenant, via the
//!    same [`crate::admin::project_owned_by`] every other tenant-facing route
//!    already trusts. Checked FIRST: an attacker probing a real project should
//!    get the same 403 an attacker probing a nonexistent one gets, not a
//!    different error that confirms the project exists.
//! 2. **Identifier safety** — [`hive_backend::snapshot::snapshot_image`]'s own
//!    `safe_component` gate, invoked here too so a caller of THIS module gets
//!    the same rejection before any filesystem call, not just inside the
//!    primitive.
//! 3. **Quota** — a per-tenant snapshot-count ceiling. Deliberately COUNT, not
//!    bytes: this fleet has no cheap way to attribute reflink-shared bytes to
//!    one tenant (the whole point of CoW is that two snapshots share extents,
//!    so "bytes used by snapshot X" is not well-defined once anything has
//!    diverged), and an unbounded count is still a real resource-exhaustion
//!    vector — the podman lock-pool incident is the standing example of what
//!    an uncapped per-tenant resource does to a shared host.

use std::sync::Arc;

use crate::state::CloudState;

/// Default ceiling on live snapshots per project. `HIVE_SNAPSHOT_QUOTA` tunes
/// it fleet-wide; there is no per-tenant override yet — add one the same way
/// `team_plan`-driven limits already work elsewhere before this needs it.
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
    /// The project is already at its snapshot ceiling.
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

/// Authorize a snapshot CREATE for `project` on behalf of tenant `caller_tenant`.
///
/// Returns `Ok(())` when the caller may proceed; the actual filesystem work
/// (via `hive_backend::snapshot::snapshot_image`) stays the caller's — this
/// function only decides whether it's allowed to happen, so it never touches a
/// disk itself and can be unit-reasoned-about independent of any specific node.
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

/// Authorize a snapshot DELETE. No quota check (deleting can only reduce
/// usage), but the same ownership + identifier gates apply — a delete path
/// that trusts an id less than the create path is a real, exploitable
/// asymmetry (an attacker who cannot CREATE a traversal id could otherwise
/// still DELETE via one, if some other component ever wrote a file at that
/// path for an unrelated reason).
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

/// Mirrors `hive_backend::snapshot::safe_component` exactly. Duplicated rather
/// than imported across the hive-backend/hive-cloud boundary because the two
/// crates' dependency direction doesn't currently run that way — but the RULE
/// must never drift between them, so the doc comment states the invariant
/// explicitly: any change to `safe_component`'s definition must be mirrored
/// here in the same commit, or the broker could authorize an id the primitive
/// then rejects (safe: fails closed) or — the dangerous direction — the
/// primitive could one day be loosened without the broker noticing.
fn snapshot_id_is_safe(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != ".."
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        && !s.contains("..")
}
