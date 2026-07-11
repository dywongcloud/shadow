use crate::guardian::error::Result;
use crate::log::identity::Identity;
use async_trait::async_trait;
use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};
use iroh::{EndpointId as NodeId, SecretKey};
use std::sync::Arc;

/// Options for creating an identity.
pub struct CreateIdentityOptions {
    pub identity_keys_path: String,
    pub id_type: String,
    pub keystore: Arc<dyn Keystore>,
    pub id: String,
}

/// Trait for the Keystore.
#[async_trait]
pub trait Keystore: Send + Sync {
    async fn put(&self, key: &str, value: &[u8]) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    async fn has(&self, key: &str) -> Result<bool>;
    async fn delete(&self, key: &str) -> Result<()>;

    /// Enumerates the stored key identifiers (metadata only — never key material).
    /// Sync by design so it can be called under a `parking_lot` guard without
    /// holding the guard across an `.await`. Named distinctly from the concrete
    /// async `list_keys` methods to avoid shadowing them via trait resolution.
    fn enumerate_keys(&self) -> Result<Vec<String>>;

    /// Derives and returns the **public** key (hex/z-base32) of a stored keypair,
    /// or `None` if the key is missing or is not a 32-byte secret. **Never**
    /// returns private material. Sync (see `enumerate_keys`).
    fn public_key(&self, key_id: &str) -> Result<Option<String>>;

    /// Generates a fresh Ed25519 keypair, stores its secret under `key_id`
    /// (overwriting any existing value), and returns only the new **public** key.
    /// Sync (see `enumerate_keys`).
    fn generate_key(&self, key_id: &str) -> Result<String>;

    /// Lifecycle metadata for a stored key (creation time, kind, rotation count),
    /// or `None` when the key is unknown or untracked (D2). Default: untracked, so
    /// keystores that don't record metadata keep compiling. Sync (see
    /// `enumerate_keys`). **Never** exposes key material.
    fn key_meta(&self, _key_id: &str) -> Result<Option<KeyMeta>> {
        Ok(None)
    }
}

/// Lifecycle metadata recorded per key by keystores that track it (D2). Lets the
/// admin distinguish a freshly-generated key from one that has been **rotated**
/// (regenerated in place), and show its age/kind — none of which the raw
/// `key_id → secret` mapping captured before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMeta {
    /// Unix seconds when the key was first generated under this id.
    pub created_at: u64,
    /// Unix seconds of the most recent (re)generation.
    pub updated_at: u64,
    /// Key kind (currently always `"ed25519"`).
    pub kind: String,
    /// How many times this id has been regenerated in place (0 = never rotated).
    pub rotated_count: u32,
}

impl KeyMeta {
    /// Current wall-clock in unix seconds (0 if the clock is before the epoch).
    pub(crate) fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Serialize to a compact JSON line for storage.
    pub(crate) fn to_json(&self) -> Vec<u8> {
        format!(
            r#"{{"created_at":{},"updated_at":{},"kind":"{}","rotated_count":{}}}"#,
            self.created_at, self.updated_at, self.kind, self.rotated_count
        )
        .into_bytes()
    }

    /// Best-effort parse from stored JSON bytes (returns `None` on malformed data).
    pub(crate) fn from_json(bytes: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        Some(KeyMeta {
            created_at: v.get("created_at")?.as_u64()?,
            updated_at: v.get("updated_at").and_then(|x| x.as_u64()).unwrap_or(0),
            kind: v
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("ed25519")
                .to_string(),
            rotated_count: v.get("rotated_count").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        })
    }
}

/// Derive the public key (as a display string) from stored secret bytes, or
/// `None` if the bytes are not a valid 32-byte Ed25519 secret. Never exposes the
/// secret itself.
pub(crate) fn derive_public(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 32 {
        return None;
    }
    iroh::SecretKey::try_from(&bytes[..32])
        .ok()
        .map(|sk| sk.public().to_string())
}

/// Generate a fresh keypair, returning `(secret_bytes, public_string)`.
pub(crate) fn new_secret_bytes() -> ([u8; 32], String) {
    let sk = iroh::SecretKey::generate();
    (sk.to_bytes(), sk.public().to_string())
}

/// Main IdentityProvider trait.
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    /// Returns the identity ID.
    async fn get_id(&self, opts: &CreateIdentityOptions) -> Result<String>;

    /// Signs an identity's data (GuardianDB public key signature).
    async fn sign_identity(&self, data: &[u8], id: &str) -> Result<Vec<u8>>;

    /// Returns the provider type (e.g. "GuardianDB", "ethereum", etc.).
    fn get_type(&self) -> String;

    /// Verifies the received identity.
    async fn verify_identity(&self, identity: &Identity) -> Result<()>;

    /// Signs a generic value with the identity.
    async fn sign(&self, identity: &Identity, bytes: &[u8]) -> Result<Vec<u8>>;

    /// Reconstructs a public key from bytes.
    fn unmarshal_public_key(&self, data: &[u8]) -> Result<VerifyingKey>;
}

/// Concrete IdentityProvider implementation for GuardianDB.
pub struct GuardianDBIdentityProvider {
    secret_key: SecretKey,
    provider_type: String,
}

impl Default for GuardianDBIdentityProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GuardianDBIdentityProvider {
    pub fn new() -> Self {
        Self {
            secret_key: SecretKey::generate(),
            provider_type: "GuardianDB".to_string(),
        }
    }

    pub fn new_with_secret_key(secret_key: SecretKey) -> Self {
        Self {
            secret_key,
            provider_type: "GuardianDB".to_string(),
        }
    }

    pub fn public_key(&self) -> NodeId {
        self.secret_key.public()
    }

    /// Creates an instance for use in tests.
    #[cfg(test)]
    pub fn new_for_testing() -> Self {
        Self::new()
    }

    fn get_signing_key(&self) -> ed25519_dalek::SigningKey {
        let bytes = self.secret_key.to_bytes();
        ed25519_dalek::SigningKey::from_bytes(&bytes)
    }
}

#[async_trait]
impl IdentityProvider for GuardianDBIdentityProvider {
    async fn get_id(&self, _opts: &CreateIdentityOptions) -> Result<String> {
        // Return the NodeId as a string.
        let node_id = self.secret_key.public();
        Ok(node_id.to_string())
    }

    async fn sign_identity(&self, data: &[u8], _id: &str) -> Result<Vec<u8>> {
        // Sign the data with the secret key using ed25519.
        let signing_key = self.get_signing_key();
        let signature = signing_key.sign(data);
        Ok(signature.to_bytes().to_vec())
    }

    fn get_type(&self) -> String {
        self.provider_type.clone()
    }

    async fn verify_identity(&self, identity: &Identity) -> Result<()> {
        // Check whether the identity has a valid signature.
        let public_key = identity.public_key().ok_or_else(|| {
            crate::guardian::error::GuardianError::Store("Identity missing public key".to_string())
        })?;

        // Use the signatures HashMap instead of accessing the Signatures struct directly.
        let signatures_map = identity.signatures_map();
        let signature_bytes = signatures_map.get("publicKey").ok_or_else(|| {
            crate::guardian::error::GuardianError::Store(
                "Identity missing publicKey signature".to_string(),
            )
        })?;

        // Reconstruct the signature.
        let signature = Signature::from_slice(signature_bytes).map_err(|e| {
            crate::guardian::error::GuardianError::Store(format!("Invalid signature format: {}", e))
        })?;

        // Reconstruct the data that was signed.
        let signed_data = format!("{}{}", identity.id(), identity.get_type());

        // Verify the signature using ed25519_dalek.
        public_key
            .verify(signed_data.as_bytes(), &signature)
            .map_err(|e| {
                crate::guardian::error::GuardianError::Store(format!(
                    "Invalid identity signature: {}",
                    e
                ))
            })
    }

    async fn sign(&self, _identity: &Identity, bytes: &[u8]) -> Result<Vec<u8>> {
        // Sign generic data with the secret key.
        let signing_key = self.get_signing_key();
        let signature = signing_key.sign(bytes);
        Ok(signature.to_bytes().to_vec())
    }

    fn unmarshal_public_key(&self, data: &[u8]) -> Result<VerifyingKey> {
        if data.len() != 32 {
            return Err(crate::guardian::error::GuardianError::Store(
                "Invalid public key length".to_string(),
            ));
        }
        VerifyingKey::from_bytes(data.try_into().map_err(|_| {
            crate::guardian::error::GuardianError::Store("Failed to convert bytes".to_string())
        })?)
        .map_err(|e| {
            crate::guardian::error::GuardianError::Store(format!(
                "Failed to unmarshal public key: {}",
                e
            ))
        })
    }
}

/// In-memory Keystore implementation for development and testing.
use std::collections::HashMap;
use tokio::sync::RwLock;

pub struct InMemoryKeystore {
    store: RwLock<HashMap<String, Vec<u8>>>,
    /// Lifecycle metadata per key (D2), kept separate from the secret material.
    meta: RwLock<HashMap<String, KeyMeta>>,
}

impl InMemoryKeystore {
    pub fn new() -> Self {
        Self {
            store: RwLock::new(HashMap::new()),
            meta: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryKeystore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Keystore for InMemoryKeystore {
    async fn put(&self, key: &str, value: &[u8]) -> Result<()> {
        let mut store = self.store.write().await;
        store.insert(key.to_string(), value.to_vec());
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let store = self.store.read().await;
        Ok(store.get(key).cloned())
    }

    async fn has(&self, key: &str) -> Result<bool> {
        let store = self.store.read().await;
        Ok(store.contains_key(key))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut store = self.store.write().await;
        store.remove(key);
        Ok(())
    }

    fn enumerate_keys(&self) -> Result<Vec<String>> {
        // Dev-only keystore; a best-effort non-blocking read is sufficient.
        Ok(self
            .store
            .try_read()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default())
    }

    fn public_key(&self, key_id: &str) -> Result<Option<String>> {
        Ok(self
            .store
            .try_read()
            .ok()
            .and_then(|s| s.get(key_id).and_then(|b| derive_public(b))))
    }

    fn generate_key(&self, key_id: &str) -> Result<String> {
        use zeroize::Zeroize;
        let (mut bytes, public) = new_secret_bytes();
        let result = match self.store.try_write() {
            Ok(mut s) => {
                s.insert(key_id.to_string(), bytes.to_vec());
                // Record/refresh lifecycle metadata (D2): preserve `created_at` and
                // bump `rotated_count` when regenerating an existing id.
                if let Ok(mut m) = self.meta.try_write() {
                    let now = KeyMeta::now_secs();
                    match m.get_mut(key_id) {
                        Some(existing) => {
                            existing.rotated_count = existing.rotated_count.saturating_add(1);
                            existing.updated_at = now;
                        }
                        None => {
                            m.insert(
                                key_id.to_string(),
                                KeyMeta {
                                    created_at: now,
                                    updated_at: now,
                                    kind: "ed25519".to_string(),
                                    rotated_count: 0,
                                },
                            );
                        }
                    }
                }
                Ok(public)
            }
            Err(_) => Err(crate::guardian::error::GuardianError::Other(
                "keystore busy".to_string(),
            )),
        };
        // Wipe the transient secret copy (the stored copy is the keystore's).
        bytes.zeroize();
        result
    }

    fn key_meta(&self, key_id: &str) -> Result<Option<KeyMeta>> {
        Ok(self
            .meta
            .try_read()
            .ok()
            .and_then(|m| m.get(key_id).cloned()))
    }
}
