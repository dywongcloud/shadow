//! `hive-core`: the shared vocabulary every other crate agrees on (ids, jobs, lifecycle states,
//! wire messages), with no async/runtime dependencies on purpose. [`logcap`] is the one place it
//! touches I/O (through `std::io` or an optional `tokio::io` trait: still no runtime, no executor).
//! [`integrity`] is the tamper-EVIDENCE hash-chain vocabulary (see its module doc for the honest
//! scope limits), shared by `hive-cloud` (the authoritative writer, via `deployment_ledger`) and
//! `fluid-compute` (which cannot depend on `hive-cloud`: see `ExecutionObserver`).

pub mod error;
pub mod ids;
pub mod integrity;
pub mod job;
pub mod logcap;
pub mod proto;
pub mod state;
pub mod time;

pub use error::{fault, HiveError, Result};
pub use ids::{BoxId, CellId, HiveId, JobId};
pub use integrity::{ExecutionObserver, IntegrityEntry, IntegrityEntryKind, fold_integrity_chain};
pub use job::{BuildJob, ResourceSpec};
pub use logcap::{CappedLine, LOG_CAP_STATS, MAX_LOG_LINE_BYTES};
pub use proto::*;
pub use state::{CellState, JobState};
pub use time::now_ms;

/// Node loader toggles the platform's own TypeScript launchers emit ahead of the entry module.
/// Neither has eval/exec/stdin semantics. Build-side emitters and launch-side validation share
/// this one contract so an allowlist can never drift away from the flag actually emitted.
pub const NODE_TS_LAUNCH_FLAGS: &[&str] = &[
    NODE_STRIP_TYPES_FLAG,
    NODE_TRANSFORM_TYPES_FLAG,
];

pub const NODE_TRANSFORM_TYPES_FLAG: &str = "--experimental-transform-types";
pub const NODE_STRIP_TYPES_FLAG: &str = "--experimental-strip-types";
