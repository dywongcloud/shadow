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
//!
//! The contract itself -- the declared operation kinds, their authorization policy, their effect and
//! their payload schema -- is one declarative table in [`contracts::managed_database`]. Everything
//! else here is the machinery that validates and executes it.

pub mod audit;
pub mod checkpoint;
pub mod contract;
pub mod contracts;
pub mod engine;
pub mod exec;
pub mod fraud;
pub mod merkle;
pub mod oplog;
pub mod persist;
pub mod reconcile;
pub mod replicate;
pub mod signing;

pub use audit::{adjudicate, replay, replay_from, verify, Breach, Dispute, Verdict};
pub use checkpoint::{
    Checkpoint, CheckpointVerifyError, SignedCheckpoint, SignError, TrustedCheckpoint,
    CHECKPOINT_DOMAIN,
};
pub use contract::{record_tenant, AuthPolicy, Caller, Contract, Effect, OpError, OpSpec};
pub use contracts::managed_db_contract;
pub use engine::{Engine, SubmitResult, DEFAULT_SHARDS};
pub use exec::{apply, index_hash, ExecError, Executor, Index, Mutation};
pub use fraud::{
    adjudicate_equivocation, adjudicate_fraud, sign_entry, Equivocation, FraudProof, SignedEntry,
};
pub use merkle::{merkle_prove, merkle_root, merkle_verify, MerklePath, Sibling};
pub use oplog::{Ack, AppendEntryError, LogEntry, Op, OpLog, TruncateError, OP_DOMAIN};
pub use persist::{
    entry_sig_preimage, read_entries_from, read_latest_checkpoint, read_window, AppendStep,
    PersistConfig, SegmentLine, Store, StoreError, DEFAULT_CHECKPOINT_EVERY,
    DEFAULT_RETAIN_CHECKPOINTS,
};
pub use reconcile::{
    reconcile, scope_report, CanonicalDb, Divergence, DivergenceKind, ReconcileReport, ScopeEntry,
    DECLARED_SCOPE,
};
pub use replicate::{
    AdoptSource, MergeError, Replica, ReplicaSnapshot, SyncReport, SNAPSHOT_DOMAIN,
};
pub use signing::{verify_ed25519, CheckpointSigner, KeyedSigner, KeyError};
