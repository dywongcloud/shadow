#[cfg(feature = "odm")]
extern crate self as guardian_db;

pub mod access_control;
pub mod address;
pub mod cache;
pub mod data_store;
pub mod db_manifest;
pub mod events;
pub mod guardian;
pub mod keystore;
pub mod log;
pub mod message_marshaler;
#[cfg(feature = "odm")]
pub mod odm;
pub mod p2p;
/// PostgreSQL wire-protocol server fronting the [`sql`] engine. Enabled by the
/// `pgwire` feature (which implies `sql`).
#[cfg(feature = "pgwire")]
pub mod pgwire;
pub mod reactive_synchronizer;
/// Storage-agnostic relational core (catalog, types, values, indexes) underlying
/// the PostgreSQL compatibility layer. Enabled by the `sql` feature.
#[cfg(feature = "sql")]
pub mod relational;
pub mod rotation;
/// Administration RPC: a loopback socket server fronting a live [`guardian::GuardianDB`]
/// so tools (e.g. the TUI panel) attach over a socket instead of opening the storage
/// directly, which the redb file lock forbids for a second process. Enabled by the
/// `sentinel` feature. See `docs/ADMIN_RPC_PLAN.md`.
#[cfg(feature = "sentinel")]
pub mod sentinel;
#[cfg(feature = "sql")]
pub mod sql;
pub mod stores;
/// Supabase-compatible HTTP gateway (Kong-shaped REST + Auth) fronting the
/// [`sql`] engine. Enabled by the `supabase` feature (which implies `sql`).
#[cfg(feature = "supabase")]
pub mod supabase;
pub mod traits;

#[cfg(test)]
pub mod tests;
