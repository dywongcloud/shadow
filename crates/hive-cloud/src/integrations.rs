//! Connected third-party integrations as **consumable platform resources**.
//!
//! When a user links an integration on the Integrations tab (OAuth via Composio),
//! the dashboard registers it here as a team-scoped resource holding the
//! provider's credentials (OAuth token / connection params) plus the env vars they
//! map to. Deployments consume them two ways:
//!   * **auto-injected env vars** (written into the team's projects on link), and
//!   * the **Vercel-compatible SDK** (`packages/vercel-sdk`, repointed at this API
//!     and authenticated with a `hive_…` platform key) reading them back via
//!     `GET /v1/integrations` + `/v1/integrations/:id/credentials`.
//!
//! Secrets (`credentials`) are returned ONLY from the explicit credentials
//! endpoint; the list/get views are redacted (env var names + masked values).

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrationResource {
    pub id: String,
    pub team: String,
    /// Composio toolkit slug, e.g. "github", "stripe", "notion".
    pub provider: String,
    /// Display name (toolkit name, falls back to provider slug).
    pub name: String,
    /// Auth shape: "oauth" | "api_key" | "none" — generic across all toolkits.
    pub kind: String,
    /// Composio entity the connection belongs to (per-user). Lets the dashboard
    /// re-fetch a fresh token later without re-linking.
    #[serde(default)]
    pub entity: String,
    /// Raw credentials (OAuth access token / connection params). Secret — only
    /// ever returned from the credentials endpoint, never from list/get.
    #[serde(default)]
    pub credentials: BTreeMap<String, String>,
    /// Env vars this integration maps to (KEY -> value), auto-injected into the
    /// team's project deployments so apps consume the resource with no code.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub created_ms: u64,
    #[serde(default)]
    pub updated_ms: u64,
}

impl IntegrationResource {
    /// Redacted view safe for list/get (no secret values; env var NAMES only).
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "team": self.team,
            "provider": self.provider,
            "name": self.name,
            "kind": self.kind,
            // Env var names are safe to expose (values are not).
            "env_keys": self.env.keys().cloned().collect::<Vec<_>>(),
            "has_credentials": !self.credentials.is_empty(),
            "created_ms": self.created_ms,
            "updated_ms": self.updated_ms,
        })
    }

    /// Full view incl. secrets — only for the credentials endpoint (hive_ key auth).
    pub fn full(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "team": self.team,
            "provider": self.provider,
            "name": self.name,
            "kind": self.kind,
            "credentials": self.credentials,
            "env": self.env,
            "created_ms": self.created_ms,
            "updated_ms": self.updated_ms,
        })
    }
}

/// Upsert payload from the dashboard's link callback.
#[derive(Debug, Deserialize)]
pub struct UpsertReq {
    pub provider: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub entity: String,
    #[serde(default)]
    pub credentials: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

pub struct IntegrationStore {
    items: RwLock<Vec<IntegrationResource>>,
}

impl IntegrationStore {
    pub fn new() -> IntegrationStore {
        IntegrationStore {
            items: RwLock::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> Vec<IntegrationResource> {
        self.items.read().clone()
    }
    pub fn load(&self, data: Vec<IntegrationResource>) {
        *self.items.write() = data;
    }

    /// Link or update an integration for a team. Idempotent on (team, provider):
    /// re-linking the same provider refreshes its credentials/env in place.
    pub fn upsert(&self, team: &str, req: UpsertReq) -> IntegrationResource {
        let team = if team.is_empty() {
            "personal".to_string()
        } else {
            team.to_string()
        };
        let name = if req.name.trim().is_empty() {
            req.provider.clone()
        } else {
            req.name.clone()
        };
        let kind = if req.kind.trim().is_empty() {
            "oauth".to_string()
        } else {
            req.kind.clone()
        };
        let mut items = self.items.write();
        if let Some(existing) = items
            .iter_mut()
            .find(|i| i.team == team && i.provider == req.provider)
        {
            existing.name = name;
            existing.kind = kind;
            existing.entity = req.entity;
            existing.credentials = req.credentials;
            existing.env = req.env;
            existing.updated_ms = now_ms();
            return existing.clone();
        }
        let rec = IntegrationResource {
            id: format!("int_{}", uuid::Uuid::new_v4().simple()),
            team,
            provider: req.provider,
            name,
            kind,
            entity: req.entity,
            credentials: req.credentials,
            env: req.env,
            created_ms: now_ms(),
            updated_ms: now_ms(),
        };
        items.push(rec.clone());
        rec
    }

    pub fn list(&self, team: &str) -> Vec<IntegrationResource> {
        self.items
            .read()
            .iter()
            .filter(|i| i.team == team)
            .cloned()
            .collect()
    }

    pub fn get(&self, team: &str, id: &str) -> Option<IntegrationResource> {
        self.items
            .read()
            .iter()
            .find(|i| i.team == team && i.id == id)
            .cloned()
    }

    pub fn delete(&self, team: &str, id: &str) -> bool {
        let mut items = self.items.write();
        let before = items.len();
        items.retain(|i| !(i.team == team && i.id == id));
        items.len() != before
    }
}

impl Default for IntegrationStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(provider: &str) -> UpsertReq {
        let mut creds = BTreeMap::new();
        creds.insert("token".into(), "secret-xyz".into());
        let mut env = BTreeMap::new();
        env.insert("STRIPE_API_KEY".into(), "secret-xyz".into());
        UpsertReq {
            provider: provider.into(),
            name: "Stripe".into(),
            kind: "oauth".into(),
            entity: "gh_u1".into(),
            credentials: creds,
            env,
        }
    }

    #[test]
    fn upsert_is_idempotent_per_team_provider() {
        let s = IntegrationStore::new();
        let a = s.upsert("acme", req("stripe"));
        let b = s.upsert("acme", req("stripe"));
        assert_eq!(a.id, b.id, "re-linking same provider updates in place");
        assert_eq!(s.list("acme").len(), 1);
        s.upsert("acme", req("notion"));
        assert_eq!(s.list("acme").len(), 2);
    }

    #[test]
    fn list_and_delete_are_team_scoped() {
        let s = IntegrationStore::new();
        let a = s.upsert("t1", req("stripe"));
        s.upsert("t2", req("stripe"));
        assert_eq!(s.list("t1").len(), 1);
        assert!(
            !s.delete("t2", &a.id),
            "cannot delete another team's resource"
        );
        assert!(s.delete("t1", &a.id));
        assert!(s.list("t1").is_empty());
    }

    #[test]
    fn public_view_redacts_secrets_full_view_keeps_them() {
        let s = IntegrationStore::new();
        let r = s.upsert("t", req("stripe"));
        let pubv = r.public();
        assert!(
            pubv.get("credentials").is_none(),
            "list/get must not leak credentials"
        );
        assert_eq!(pubv["env_keys"][0], "STRIPE_API_KEY");
        assert_eq!(pubv["has_credentials"], true);
        let full = r.full();
        assert_eq!(full["credentials"]["token"], "secret-xyz");
    }
}
