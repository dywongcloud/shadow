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

/// Stable markers a backend embeds in its `anyhow` error text so the gateway can classify a lease
/// failure by CAUSE instead of by catch-all. The lease path erases types: every backend fault
/// reaches `fluid-gateway::classify_lease_error` as a flat error string, and anything it does not
/// name is reported as `CAPACITY_EXHAUSTED` (a missing base rootfs was once published as "the host
/// is out of capacity" on a node with 923 GB free). These markers are the contract between the two
/// crates and the ONLY thing keeping the classifier off substring-matching prose that a reword
/// silently breaks, so treat each as public API: keep it in the error text and keep the matching
/// arm in the classifier. Both crates depend on `hive-core` and neither on the other, so this is
/// the one place both can name.
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
    /// The interpreter/runtime this deployment declares is not installed on the filesystem this
    /// NODE execs cells against. Operator remedy: provision the runtime (for Firecracker the GUEST
    /// rootfs image, not the host). Not app breakage, not host exhaustion. Placement should make
    /// this unreachable (`schedule::place` hard-filters on `NodeInfo::wasm_runtime` like on
    /// `gpu_count`), so reaching it means the capability probe and the real filesystem disagree (a
    /// rootfs replaced under a running node, a stale gossiped record, `HIVE_WASM_RUNTIME=1` on a
    /// node without the binary); it exists so that disagreement surfaces as an operator-actionable
    /// node fault, never as `CAPACITY_EXHAUSTED` or as advice to the tenant to debug their own
    /// entrypoint.
    pub const NODE_RUNTIME_MISSING: &str = "NodeRuntimeMissing";
    /// The DEPLOYMENT's own process never reached a listening state (it exited, or never bound its
    /// port within the start budget): an app fault (bad entrypoint, missing env, a crash on boot).
    /// Distinct from the node markers above, and needed even though the pool circuits on a streak
    /// of these: the circuit opens only on the THIRD failure, so without a marker the first two
    /// were published as `CAPACITY_EXHAUSTED`, blaming the host for an app-level fault.
    pub const DEPLOYMENT_START_FAILED: &str = "DeploymentStartFailed";
}
