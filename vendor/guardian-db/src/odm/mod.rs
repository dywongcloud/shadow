//! Optional object-document mapper for GuardianDB.
//!
//! The ODM deliberately sits above [`crate::traits::DocumentStore`]. It adds
//! declarative schemas, validation, local indexes, uniqueness checks, and a
//! Mongoose-style CRUD surface without changing GuardianDB's decentralized
//! replication engine.

mod collection;
mod error;
mod index;
mod model;
mod query;
mod schema;
mod storage;
mod transaction;
mod update;

pub use collection::{Collection, DocumentId, TypedCollection};
pub use error::{OdmError, Result};
pub use guardian_db_derive::Model;
pub use index::IndexMetadata;
pub use model::Model;
pub use schema::{FieldDefinition, FieldType, ModelSchema, TimestampDefinition};
pub use storage::{CollectionStorage, DocumentStoreStorage, MemoryStorage};
pub use transaction::{ConsistencyLevel, TransactionContext, WriteOptions};
