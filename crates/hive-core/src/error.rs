use thiserror::Error;

#[derive(Debug, Error)]
pub enum HiveError {
    #[error("no capacity: cannot place a cell satisfying {0:?}")]
    NoCapacity(crate::job::ResourceSpec),

    #[error("cell {0} not found")]
    CellNotFound(crate::ids::CellId),

    #[error("job {0} not found")]
    JobNotFound(crate::ids::JobId),

    #[error("illegal cell transition {from:?} -> {to:?}")]
    IllegalTransition {
        from: crate::state::CellState,
        to: crate::state::CellState,
    },

    #[error("backend error: {0}")]
    Backend(String),

    #[error(transparent)]
    Other(#[from] anyhow_like::AnyError),
}

/// Minimal stand-in so hive-core stays dependency-light but downstream crates
/// can convert their own errors in via `HiveError::Backend`.
pub mod anyhow_like {
    #[derive(Debug)]
    pub struct AnyError(pub String);
    impl std::fmt::Display for AnyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for AnyError {}
}

pub type Result<T> = std::result::Result<T, HiveError>;

/// Stable markers a backend embeds in its `anyhow` error text so the gateway can
/// classify a lease failure by CAUSE instead of by catch-all.
///
/// The lease path erases types: every backend fault reaches
/// `fluid-gateway::classify_lease_error` as a flat `anyhow::Error` string, and
/// anything that string does not name is reported to the user as
/// `CAPACITY_EXHAUSTED`. That is how a missing base rootfs on fc-sanjose-cvm-2
/// was published as "the host is out of capacity" while the node held 923 GB
/// free. These markers are the contract between the two crates — the ONLY thing
/// keeping the classifier off substring-matching prose that a later reword
/// silently breaks — so treat each as public API: keep it in the error text, and
/// keep the matching arm in the classifier.
///
/// Both crates depend on `hive-core` and neither depends on the other, so this is
/// the single place both can name.
pub mod fault {
    /// A cell artifact this NODE must have is absent (per-image rootfs, shared
    /// base rootfs, guest kernel). Operator remedy: reprovision the node's
    /// images. Not app breakage, not host exhaustion.
    pub const NODE_IMAGE_MISSING: &str = "NodeImageMissing";
    /// This NODE's isolation backend cannot run cells at all (no `/dev/kvm`, no
    /// firecracker binary, wrong OS).
    pub const NODE_BACKEND_UNAVAILABLE: &str = "NodeBackendUnavailable";
    /// This NODE's container lock pool is empty and nothing was reclaimable, so
    /// no container can start here until `num_locks` is raised + renumbered.
    pub const NODE_LOCK_POOL_EXHAUSTED: &str = "NodeLockPoolExhausted";
    /// The interpreter/runtime this deployment declares is not installed on the
    /// filesystem this NODE execs cells against. Operator remedy: provision the
    /// runtime (for Firecracker that means the GUEST rootfs image, not the
    /// host). Not app breakage, not host exhaustion.
    ///
    /// Placement is supposed to make this unreachable — `schedule::place`
    /// hard-filters on `NodeInfo::wasm_runtime` exactly like it does on
    /// `gpu_count` — so reaching it means the capability probe and the real
    /// filesystem disagree (a rootfs replaced under a running node, a stale
    /// gossiped record, `HIVE_WASM_RUNTIME=1` set on a node without the
    /// binary). It exists because that disagreement must surface as an
    /// operator-actionable node fault rather than as `CAPACITY_EXHAUSTED` or,
    /// worse, as advice to the tenant to go debug their own entrypoint.
    pub const NODE_RUNTIME_MISSING: &str = "NodeRuntimeMissing";
    /// The DEPLOYMENT's own process never reached a listening state — it exited,
    /// or it never bound its port inside the start budget. An app fault: bad
    /// entrypoint, missing env, a crash on boot.
    ///
    /// Distinct from the node markers above, and needed even though the pool
    /// circuits on a streak of these: the circuit only opens on the THIRD
    /// failure, so without a marker the first two were published as
    /// `CAPACITY_EXHAUSTED` — the exact "blames the host for an app-level fault"
    /// case the failure taxonomy exists to prevent. Witnessed live: a microVM
    /// whose function never bound its port reported CAPACITY_EXHAUSTED on a node
    /// with a healthy rootfs and free disk.
    pub const DEPLOYMENT_START_FAILED: &str = "DeploymentStartFailed";
}
