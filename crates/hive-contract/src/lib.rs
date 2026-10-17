//! `hive-contract`: an additive contract + execution-transparency layer for managed database
//! operations on the shadow network.
//!
//! Modelled on Vitra / Execution Transparency (pfrazee/vitra, whitepaper
//! `docs/whitepaper.md`): participants append operations to an oplog, a single executor applies them
//! through a pure `apply` into an index, and any third party can replay the log and detect a
//! dishonest executor as a reproducible breach. No blockchain, no consensus, no global ordering --
//! honesty is enforced by audit, not by agreement.
//!
//! What this crate is NOT:
//!   * It is not an authorization system. Tenancy stays exactly where the platform puts it -- the
//!     cryptographically-bound JWT/API-key claim resolved by `admin::tenant` and the ownership check
//!     in `admin::require_project`. [`contract::Caller`] is the seam: this crate consumes that
//!     verdict and refuses to act without it.
//!   * It is not a replacement for `state.json` + `store_sync`. It records a tamper-evident history
//!     of operations the platform already authorized; the canonical state is still the platform's.
//!   * It is not a GuardianDB replacement. The relational mirror stays a read-only projection.

pub mod audit;
pub mod contract;
pub mod engine;
pub mod exec;
pub mod oplog;

pub use audit::{verify, Breach};
pub use contract::{managed_db_contract, AuthPolicy, Caller, Contract, Effect, OpError, OpSpec};
pub use engine::{Engine, SubmitResult, DEFAULT_SHARDS};
pub use exec::{apply, index_hash, ExecError, Executor, Index, Mutation};
pub use oplog::{Ack, LogEntry, Op, OpLog, OP_DOMAIN};
