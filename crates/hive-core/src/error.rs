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
