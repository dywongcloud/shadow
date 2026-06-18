//! Identity registry — orgs & users indexed in the platform store, referenced by
//! tenant id so they live in the right per-namespace collections/documents.
//!
//! These records mirror the identity provider (Clerk): the dashboard syncs the
//! signed-in user and their active organization, and we persist them scoped to
//! the org's tenant (its slug). This makes orgs/users first-class, queryable
//! collections in the data browser alongside projects/deployments/databases.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrgRecord {
    pub id: String,
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub image_url: String,
    /// Tenant id this org owns (its slug). All its projects/deployments use it.
    pub tenant: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRecord {
    pub id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub image_url: String,
    /// The user's currently-active tenant (org slug, or "personal").
    pub tenant: String,
    /// Org slugs this user has been seen as a member of.
    #[serde(default)]
    pub orgs: Vec<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Default)]
pub struct IdentityStore {
    orgs: RwLock<HashMap<String, OrgRecord>>,
    users: RwLock<HashMap<String, UserRecord>>,
}

impl IdentityStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert_org(&self, id: &str, slug: &str, name: &str, image_url: &str) {
        let now = now_ms();
        let mut m = self.orgs.write();
        let e = m.entry(id.to_string()).or_insert_with(|| OrgRecord {
            id: id.to_string(),
            slug: slug.to_string(),
            name: name.to_string(),
            image_url: image_url.to_string(),
            tenant: slug.to_string(),
            created_ms: now,
            updated_ms: now,
        });
        e.slug = slug.to_string();
        e.name = name.to_string();
        if !image_url.is_empty() {
            e.image_url = image_url.to_string();
        }
        e.tenant = slug.to_string();
        e.updated_ms = now;
    }

    /// Upsert a user, recording their active tenant and (optionally) an org
    /// membership.
    pub fn upsert_user(&self, id: &str, email: &str, name: &str, image_url: &str, tenant: &str, org_slug: Option<&str>) {
        let now = now_ms();
        let mut m = self.users.write();
        let e = m.entry(id.to_string()).or_insert_with(|| UserRecord {
            id: id.to_string(),
            email: email.to_string(),
            name: name.to_string(),
            image_url: image_url.to_string(),
            tenant: tenant.to_string(),
            orgs: Vec::new(),
            created_ms: now,
            updated_ms: now,
        });
        if !email.is_empty() {
            e.email = email.to_string();
        }
        if !name.is_empty() {
            e.name = name.to_string();
        }
        if !image_url.is_empty() {
            e.image_url = image_url.to_string();
        }
        e.tenant = tenant.to_string();
        if let Some(slug) = org_slug {
            if !slug.is_empty() && !e.orgs.contains(&slug.to_string()) {
                e.orgs.push(slug.to_string());
            }
        }
        e.updated_ms = now;
    }

    pub fn orgs(&self) -> Vec<OrgRecord> {
        self.orgs.read().values().cloned().collect()
    }
    pub fn users(&self) -> Vec<UserRecord> {
        self.users.read().values().cloned().collect()
    }
    pub fn load(&self, orgs: Vec<OrgRecord>, users: Vec<UserRecord>) {
        *self.orgs.write() = orgs.into_iter().map(|o| (o.id.clone(), o)).collect();
        *self.users.write() = users.into_iter().map(|u| (u.id.clone(), u)).collect();
    }
}
