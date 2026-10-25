//! The contract INSTANCES of this crate, one module each: the actual declared contracts.
//!
//! [`crate::contract`] is the machinery -- the type system, the gates, validation. The contract
//! itself lives here, as a declarative table a reviewer can read in one screen and diff in review.

pub mod managed_database;

pub use managed_database::managed_db_contract;
