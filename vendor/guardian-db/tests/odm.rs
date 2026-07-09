#![cfg(feature = "odm")]

use guardian_db::odm::{
    Collection, ConsistencyLevel, FieldDefinition, FieldType, MemoryStorage, Model, ModelSchema,
    OdmError, TransactionContext, TypedCollection, WriteOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

fn employee_schema() -> ModelSchema {
    ModelSchema::new("Employee", "employees")
        .field(
            FieldDefinition::new("ssn", FieldType::String)
                .primary_key()
                .required(),
        )
        .field(
            FieldDefinition::new("name", FieldType::String)
                .required()
                .indexed(),
        )
        .field(
            FieldDefinition::new("email", FieldType::String)
                .unique()
                .nullable(),
        )
        .field(
            FieldDefinition::new("skills", FieldType::Array)
                .indexed()
                .nullable(),
        )
        .field(FieldDefinition::new("hourly_pay", FieldType::String).required())
        .timestamps("created_at", "updated_at")
}

#[tokio::test]
async fn mongoose_style_crud_and_constraints_work() {
    let collection = Collection::new(
        "employees",
        employee_schema(),
        Arc::new(MemoryStorage::new()),
    )
    .await
    .unwrap();

    let inserted = collection
        .insert_one(json!({
            "name": "Elon",
            "ssn": "562-48-5384",
            "email": "elon@example.test",
            "skills": ["engineering", "space"],
            "hourly_pay": "$15"
        }))
        .await
        .unwrap();
    assert_eq!(inserted["ssn"], "562-48-5384");
    assert!(inserted.get("created_at").is_some());
    assert!(inserted.get("_id").is_none());

    let found = collection
        .find_one(json!({ "ssn": "562-48-5384" }))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found["name"], "Elon");
    assert_eq!(
        collection
            .find(json!({ "skills": "space" }))
            .await
            .unwrap()
            .len(),
        1
    );

    collection
        .insert_one(json!({
            "name": "Null Email",
            "ssn": "null-email",
            "email": null,
            "hourly_pay": "$20"
        }))
        .await
        .unwrap();
    assert_eq!(
        collection
            .find(json!({ "email": null }))
            .await
            .unwrap()
            .len(),
        1
    );

    let updated = collection
        .update(
            json!({ "ssn": "562-48-5384" }),
            json!({ "$set": { "hourly_pay": "$100" } }),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated["hourly_pay"], "$100");
    assert!(
        collection
            .find_by_id("562-48-5384")
            .await
            .unwrap()
            .is_some()
    );

    let duplicate = collection
        .insert_one(json!({
            "name": "Duplicate",
            "ssn": "new-ssn",
            "email": "elon@example.test",
            "hourly_pay": "$1"
        }))
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate,
        OdmError::DuplicateKey { ref field, .. } if field == "email"
    ));
}

#[tokio::test]
async fn validation_and_batch_insert_are_atomic_before_storage() {
    let storage = Arc::new(MemoryStorage::new());
    let collection = Collection::new("employees", employee_schema(), storage.clone())
        .await
        .unwrap();

    let error = collection
        .insert(vec![
            json!({
                "name": "One",
                "ssn": "one",
                "email": "same@example.test",
                "hourly_pay": "$1"
            }),
            json!({
                "name": "Two",
                "ssn": "two",
                "email": "same@example.test",
                "hourly_pay": "$2"
            }),
        ])
        .await
        .unwrap_err();
    assert!(matches!(error, OdmError::DuplicateKey { .. }));
    assert!(storage.snapshot().is_empty());

    let validation = collection
        .insert_one(json!({ "name": "Missing fields" }))
        .await
        .unwrap_err();
    assert!(matches!(validation, OdmError::Validation { .. }));
}

#[tokio::test]
async fn primary_keys_are_immutable_and_update_operators_validate() {
    let collection = Collection::new(
        "employees",
        employee_schema(),
        Arc::new(MemoryStorage::new()),
    )
    .await
    .unwrap();
    collection
        .insert_one(json!({
            "name": "Ada",
            "ssn": "ada",
            "hourly_pay": "$50"
        }))
        .await
        .unwrap();

    let error = collection
        .update(
            json!({ "ssn": "ada" }),
            json!({ "$set": { "ssn": "changed" } }),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        OdmError::ImmutableField(ref field) if field == "ssn"
    ));

    let error = collection
        .update(
            json!({ "ssn": "ada" }),
            json!({ "$inc": { "hourly_pay": 1 } }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, OdmError::InvalidUpdate(_)));
}

#[tokio::test]
async fn replicated_consistency_is_explicitly_reserved() {
    let collection = Collection::new(
        "employees",
        employee_schema(),
        Arc::new(MemoryStorage::new()),
    )
    .await
    .unwrap();
    let options = WriteOptions {
        transaction: Some(TransactionContext::with_consistency(
            ConsistencyLevel::Replicated,
        )),
    };

    let error = collection
        .insert_one_with_options(
            json!({
                "name": "Reserved",
                "ssn": "reserved",
                "hourly_pay": "$1"
            }),
            options,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, OdmError::UnsupportedConsistency(_)));
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Model)]
#[model(collection = "typed_employees", timestamps)]
struct TypedEmployee {
    #[primary_key]
    employee_id: String,
    #[unique]
    email: String,
    #[index]
    department: String,
    name: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Model)]
#[model(collection = "serde_employees")]
#[serde(rename_all = "camelCase")]
struct SerdeEmployee {
    #[primary_key]
    employee_id: String,
    #[serde(rename = "legalName")]
    name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    label: String,
    #[serde(skip)]
    transient: String,
}

#[tokio::test]
async fn derive_model_builds_a_typed_collection() {
    let schema = TypedEmployee::schema();
    assert_eq!(schema.collection(), "typed_employees");
    assert_eq!(schema.primary_key(), "employee_id");
    assert!(schema.unique_fields().contains("email"));
    assert!(schema.indexed_fields().contains("department"));

    let collection = TypedCollection::<TypedEmployee>::new(Arc::new(MemoryStorage::new()))
        .await
        .unwrap();
    let employee = TypedEmployee {
        employee_id: "e-1".to_string(),
        email: "ada@example.test".to_string(),
        department: "research".to_string(),
        name: "Ada".to_string(),
        created_at: None,
        updated_at: None,
    };
    let inserted = collection.insert_one(employee.clone()).await.unwrap();
    assert_eq!(inserted.employee_id, employee.employee_id);
    assert!(inserted.created_at.is_some());

    let found = collection
        .find_one(json!({ "department": "research" }))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.name, "Ada");
}

#[tokio::test]
async fn derive_model_honors_common_serde_field_names() {
    let schema = SerdeEmployee::schema();
    assert_eq!(schema.primary_key(), "employeeId");
    assert!(schema.fields().contains_key("legalName"));
    assert!(schema.fields().contains_key("label"));
    assert!(!schema.fields()["label"].required);
    assert!(!schema.fields().contains_key("transient"));

    let collection = TypedCollection::<SerdeEmployee>::new(Arc::new(MemoryStorage::new()))
        .await
        .unwrap();
    let inserted = collection
        .insert_one(SerdeEmployee {
            employee_id: "serde-1".to_string(),
            name: "Ada".to_string(),
            label: String::new(),
            transient: "not persisted".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(inserted.employee_id, "serde-1");
    assert!(inserted.label.is_empty());
    assert!(inserted.transient.is_empty());
}
