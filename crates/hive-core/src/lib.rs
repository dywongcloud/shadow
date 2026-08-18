//! `hive-core` — shared vocabulary for the Hive reimplementation.
//!
//! This crate has no async/runtime dependencies on purpose: it is the set of
//! types every other crate agrees on (ids, jobs, lifecycle states, wire
//! messages). It maps 1:1 onto the concepts in Vercel's Hive write-up:
//!
//! * [`ids`]   — `Hive` > `Box` > `Cell`, plus `Job`.
//! * [`job`]   — a `BuildJob` and its `ResourceSpec`.
//! * [`state`] — the cell & job lifecycle state machines the control plane runs.
//! * [`proto`] — messages exchanged between control plane / daemons / API / CLI.
//! * [`logcap`] — bounded capture of tenant subprocess output (the one place
//!   this crate touches I/O, and only through `std::io`/an optional
//!   `tokio::io` trait — still no runtime, no executor).

pub mod error;
pub mod ids;
pub mod job;
pub mod logcap;
pub mod proto;
pub mod state;
pub mod time;

pub use error::{fault, HiveError, Result};
pub use ids::{BoxId, CellId, HiveId, JobId};
pub use job::{BuildJob, ResourceSpec};
pub use logcap::{CappedLine, LOG_CAP_STATS, MAX_LOG_LINE_BYTES};
pub use proto::*;
pub use state::{CellState, JobState};
pub use time::now_ms;
