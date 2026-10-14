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

/// Stable markers a backend embeds in its `anyhow` error text so the gateway classifies a lease
/// failure by CAUSE: the lease path erases types, so every fault reaches
/// `fluid-gateway::classify_lease_error` as a flat string and anything unnamed is reported as
/// `CAPACITY_EXHAUSTED`. Each marker is public API (the contract between two crates that share only
/// `hive-core`): keep it in the error text and keep the matching arm in the classifier.
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
    /// The runtime this deployment declares is not installed on the filesystem this NODE execs
    /// cells against (for Firecracker, the GUEST rootfs). Operator remedy: provision it; not app
    /// breakage, not host exhaustion. Placement (`schedule::wasm_capable`/`bun_capable`) should
    /// make it unreachable, so reaching it means the capability probe and the real filesystem
    /// disagree (a rootfs replaced under a running node, a stale gossiped record): a node fault,
    /// never `CAPACITY_EXHAUSTED` or advice to debug the entrypoint.
    pub const NODE_RUNTIME_MISSING: &str = "NodeRuntimeMissing";
    /// The DEPLOYMENT's own process never reached a listening state (it exited, or never bound its
    /// port within the start budget): an app fault (bad entrypoint, missing env, a crash on boot).
    /// Distinct from the node markers above, and needed even though the pool circuits on a streak
    /// of these: the circuit opens only on the THIRD failure, so without a marker the first two
    /// were published as `CAPACITY_EXHAUSTED`, blaming the host for an app-level fault.
    pub const DEPLOYMENT_START_FAILED: &str = "DeploymentStartFailed";
}
