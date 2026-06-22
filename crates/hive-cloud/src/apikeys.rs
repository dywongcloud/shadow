//! Platform API keys — tenant-scoped bearer tokens for programmatic access to
//! the control API (à la Vercel access tokens). A key is shown in full exactly
//! once at creation; only its SHA-256 hash is stored. Presenting a key on a
//! request (`Authorization: Bearer hive_…`) scopes that request to the key's
//! team/tenant.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn rand_token() -> String {
    // 256 bits of entropy, prefixed so it's recognizable + greppable in logs.
    format!("hive_{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    /// Display prefix, e.g. "hive_1a2b3c…" (the full token is never stored).
    pub prefix: String,
    /// SHA-256 of the token — persisted, but never returned to clients (use
    /// [`ApiKey::public`] for API responses).
    pub hash: String,
    pub team: String,
    pub role: String,
    pub created_ms: u64,
    #[serde(default)]
    pub last_used_ms: u64,
}

impl ApiKey {
    /// JSON view safe to return to clients (omits the secret hash).
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "prefix": self.prefix,
            "team": self.team,
            "role": self.role,
            "created_ms": self.created_ms,
            "last_used_ms": self.last_used_ms,
        })
    }
}

pub struct ApiKeyStore {
    keys: RwLock<Vec<ApiKey>>,
}

impl ApiKeyStore {
    pub fn new() -> ApiKeyStore {
        ApiKeyStore { keys: RwLock::new(Vec::new()) }
    }

    pub fn snapshot(&self) -> Vec<ApiKey> {
        self.keys.read().clone()
    }
    pub fn load(&self, data: Vec<ApiKey>) {
        *self.keys.write() = data;
    }

    /// Create a key. Returns the record and the **plaintext token** (shown once).
    pub fn create(&self, name: &str, team: &str, role: &str) -> (ApiKey, String) {
        let token = rand_token();
        let key = ApiKey {
            id: format!("key_{}", uuid::Uuid::new_v4().simple()),
            name: name.to_string(),
            prefix: format!("{}…", &token[..token.len().min(12)]),
            hash: sha256_hex(&token),
            team: if team.is_empty() { "personal".into() } else { team.to_string() },
            role: if role.is_empty() { "member".into() } else { role.to_string() },
            created_ms: now_ms(),
            last_used_ms: 0,
        };
        self.keys.write().push(key.clone());
        (key, token)
    }

    /// Resolve a presented token to its key (and stamp last-used).
    pub fn verify(&self, token: &str) -> Option<ApiKey> {
        let h = sha256_hex(token);
        let mut keys = self.keys.write();
        let k = keys.iter_mut().find(|k| k.hash == h)?;
        k.last_used_ms = now_ms();
        Some(k.clone())
    }

    pub fn list(&self, team: &str) -> Vec<ApiKey> {
        self.keys.read().iter().filter(|k| k.team == team).cloned().collect()
    }

    pub fn revoke(&self, id: &str, team: &str) -> bool {
        let mut keys = self.keys.write();
        let before = keys.len();
        keys.retain(|k| !(k.id == id && k.team == team));
        keys.len() != before
    }
}

impl Default for ApiKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_verify_revoke() {
        let s = ApiKeyStore::new();
        let (key, token) = s.create("ci", "acme", "member");
        assert!(token.starts_with("hive_"));
        assert_eq!(s.verify(&token).unwrap().team, "acme");
        assert!(s.verify("hive_wrong").is_none());
        assert_eq!(s.list("acme").len(), 1);
        assert!(s.revoke(&key.id, "acme"));
        assert!(s.verify(&token).is_none());
    }
}

#[cfg(test)]
mod tests_ext {
    use super::*;

    #[test]
    fn create_then_verify_roundtrips_and_scopes_team() {
        let s = ApiKeyStore::new();
        let (key, token) = s.create("ci", "acme", "owner");
        assert!(token.starts_with("hive_"), "token is prefixed");
        assert_eq!(key.team, "acme");
        assert_eq!(key.role, "owner");
        // The presented plaintext token resolves back to the key.
        let got = s.verify(&token).expect("token verifies");
        assert_eq!(got.id, key.id);
        assert_eq!(got.team, "acme");
        // A wrong token never verifies.
        assert!(s.verify("hive_wrongtoken").is_none());
    }

    #[test]
    fn defaults_empty_team_and_role() {
        let s = ApiKeyStore::new();
        let (key, _) = s.create("k", "", "");
        assert_eq!(key.team, "personal");
        assert_eq!(key.role, "member");
    }

    #[test]
    fn public_view_never_leaks_the_hash() {
        let s = ApiKeyStore::new();
        let (key, _) = s.create("k", "t", "owner");
        let v = key.public();
        assert!(v.get("hash").is_none(), "hash must not be in the public view");
        assert!(v.get("token").is_none(), "token is never stored/returned here");
        assert_eq!(v["team"], "t");
        assert!(v["prefix"].as_str().unwrap().starts_with("hive_"));
    }

    #[test]
    fn list_is_team_scoped_and_revoke_respects_team() {
        let s = ApiKeyStore::new();
        let (a, _) = s.create("a", "team1", "owner");
        s.create("b", "team1", "member");
        s.create("c", "team2", "owner");
        assert_eq!(s.list("team1").len(), 2);
        assert_eq!(s.list("team2").len(), 1);
        // Cannot revoke a key from the wrong team.
        assert!(!s.revoke(&a.id, "team2"));
        assert_eq!(s.list("team1").len(), 2);
        // Correct team revokes it.
        assert!(s.revoke(&a.id, "team1"));
        assert_eq!(s.list("team1").len(), 1);
        // Revoking a now-missing id is a no-op.
        assert!(!s.revoke(&a.id, "team1"));
    }

    #[test]
    fn distinct_tokens_have_distinct_hashes() {
        let s = ApiKeyStore::new();
        let (k1, _) = s.create("x", "t", "owner");
        let (k2, _) = s.create("y", "t", "owner");
        assert_ne!(k1.hash, k2.hash);
        assert_ne!(k1.id, k2.id);
    }

    #[test]
    fn snapshot_load_roundtrip_preserves_verification() {
        let s = ApiKeyStore::new();
        let (_k, token) = s.create("k", "t", "owner");
        let snap = s.snapshot();
        let s2 = ApiKeyStore::new();
        s2.load(snap);
        assert!(s2.verify(&token).is_some(), "restored key still verifies");
    }
}
