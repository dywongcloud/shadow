use crate::odm::schema::ModelSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Implemented by strongly typed GuardianDB ODM models.
///
/// The [`Model`](guardian_db_derive::Model) derive macro generates this
/// implementation from field attributes such as `#[primary_key]` and
/// `#[unique]`.
pub trait Model: Serialize + DeserializeOwned + Send + Sync + 'static {
    fn schema() -> ModelSchema;
}
