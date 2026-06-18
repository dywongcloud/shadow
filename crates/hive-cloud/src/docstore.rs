//! Generic editable document store for the ops Data Browser — free-form JSON
//! documents in named collections, with full CRUD. Persisted + audited so the
//! data browser is a real (ACID-durable) admin tool, separate from the typed
//! platform stores.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc {
    pub id: String,
    pub collection: String,
    pub tenant: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Arbitrary user fields.
    #[serde(flatten)]
    pub data: serde_json::Map<String, serde_json::Value>,
}

#[derive(Default)]
pub struct DocStore {
    docs: RwLock<Vec<Doc>>,
}

impl DocStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self, collection: &str) -> Vec<Doc> {
        self.docs.read().iter().filter(|d| d.collection == collection).cloned().collect()
    }

    pub fn all(&self) -> Vec<Doc> {
        self.docs.read().clone()
    }

    pub fn collections(&self) -> Vec<(String, usize)> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for d in self.docs.read().iter() {
            *counts.entry(d.collection.clone()).or_insert(0) += 1;
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    pub fn create(&self, collection: &str, tenant: &str, data: serde_json::Map<String, serde_json::Value>) -> Doc {
        let now = now_ms();
        let doc = Doc {
            id: format!("doc_{}", &Uuid::new_v4().simple().to_string()[..16]),
            collection: collection.to_string(),
            tenant: tenant.to_string(),
            created_ms: now,
            updated_ms: now,
            data,
        };
        self.docs.write().push(doc.clone());
        doc
    }

    /// Merge `patch` fields into the document with `id`. Returns the updated doc.
    pub fn patch(&self, id: &str, patch: serde_json::Map<String, serde_json::Value>) -> Option<Doc> {
        let mut docs = self.docs.write();
        let d = docs.iter_mut().find(|d| d.id == id)?;
        for (k, v) in patch {
            if k == "id" || k == "collection" || k == "created_ms" {
                continue;
            }
            d.data.insert(k, v);
        }
        d.updated_ms = now_ms();
        Some(d.clone())
    }

    pub fn delete(&self, id: &str) -> bool {
        let mut docs = self.docs.write();
        let before = docs.len();
        docs.retain(|d| d.id != id);
        docs.len() != before
    }

    pub fn snapshot(&self) -> Vec<Doc> {
        self.docs.read().clone()
    }
    pub fn load(&self, list: Vec<Doc>) {
        *self.docs.write() = list;
    }
}
