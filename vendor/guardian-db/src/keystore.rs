use crate::guardian::error::{GuardianError, Result};
use crate::log::identity_provider::{KeyMeta, Keystore as KeystoreInterface};
use async_trait::async_trait;
use iroh::SecretKey;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::sync::Arc;

const KEYSTORE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("keystore");
/// Parallel table holding per-key lifecycle metadata JSON (D2). Kept separate
/// from `KEYSTORE_TABLE` so `get`/`enumerate_keys` over secrets are untouched.
const KEYMETA_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("keystore_meta");

/// Keystore implementation that uses redb as the persistence backend
/// and is compatible with the internal 'log' interface.
#[derive(Debug)]
pub struct RedbKeystore {
    db: Database,
}

// Send + Sync is safe because redb::Database is thread-safe.
unsafe impl Send for RedbKeystore {}
unsafe impl Sync for RedbKeystore {}

impl RedbKeystore {
    /// Creates a new RedbKeystore.
    /// If path is None, creates a temporary in-memory database.
    pub fn new(path: Option<std::path::PathBuf>) -> Result<Self> {
        let db = match path {
            Some(p) => {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        GuardianError::Other(format!("Error creating directory: {}", e))
                    })?;
                }
                Database::create(&p)
                    .map_err(|e| GuardianError::Other(format!("Error opening redb: {}", e)))?
            }
            None => Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .map_err(|e| {
                    GuardianError::Other(format!("Error creating temporary redb: {}", e))
                })?,
        };

        // Ensure the table exists.
        {
            let write_txn = db
                .begin_write()
                .map_err(|e| GuardianError::Other(format!("Error starting transaction: {}", e)))?;
            {
                let _ = write_txn
                    .open_table(KEYSTORE_TABLE)
                    .map_err(|e| GuardianError::Other(format!("Error creating table: {}", e)))?;
                // Ensure the metadata table exists too (D2).
                let _ = write_txn.open_table(KEYMETA_TABLE).map_err(|e| {
                    GuardianError::Other(format!("Error creating meta table: {}", e))
                })?;
            }
            write_txn
                .commit()
                .map_err(|e| GuardianError::Other(format!("Error committing table: {}", e)))?;
        }

        Ok(Self { db })
    }

    /// Creates a temporary in-memory keystore for testing.
    pub fn temporary() -> Result<Self> {
        Self::new(None)
    }

    /// Stores an Iroh SecretKey as bytes.
    pub async fn put_keypair(&self, key: &str, secret_key: &SecretKey) -> Result<()> {
        let encoded = secret_key.to_bytes();
        self.put(key, &encoded).await
    }

    /// Retrieves an Iroh SecretKey from bytes.
    pub async fn get_keypair(&self, key: &str) -> Result<Option<SecretKey>> {
        match self.get(key).await? {
            Some(bytes) => {
                if bytes.len() != 32 {
                    return Err(GuardianError::Other("Invalid secret key size".to_string()));
                }
                let secret_key = SecretKey::try_from(&bytes[..32]).map_err(|e| {
                    GuardianError::Other(format!("Error decoding secret key: {}", e))
                })?;
                Ok(Some(secret_key))
            }
            None => Ok(None),
        }
    }

    /// Lists all stored keys.
    pub async fn list_keys(&self) -> Result<Vec<String>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| GuardianError::Other(format!("Error starting read: {}", e)))?;
        let table = read_txn
            .open_table(KEYSTORE_TABLE)
            .map_err(|e| GuardianError::Other(format!("Error opening table: {}", e)))?;

        let mut keys = Vec::new();
        let iter = table
            .iter()
            .map_err(|e| GuardianError::Other(format!("Error iterating: {}", e)))?;

        for entry_result in iter {
            let entry = entry_result
                .map_err(|e| GuardianError::Other(format!("Error listing keys: {}", e)))?;
            keys.push(entry.0.value().to_string());
        }

        Ok(keys)
    }

    /// Closes the database.
    pub async fn close(&self) -> Result<()> {
        // Data is already persisted via write transactions in redb.
        Ok(())
    }
}

#[async_trait]
impl KeystoreInterface for Arc<RedbKeystore> {
    async fn put(&self, key: &str, value: &[u8]) -> Result<()> {
        (**self).put(key, value).await
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        (**self).get(key).await
    }
    async fn has(&self, key: &str) -> Result<bool> {
        (**self).has(key).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        (**self).delete(key).await
    }
    fn enumerate_keys(&self) -> Result<Vec<String>> {
        (**self).enumerate_keys()
    }
    fn public_key(&self, key_id: &str) -> Result<Option<String>> {
        (**self).public_key(key_id)
    }
    fn generate_key(&self, key_id: &str) -> Result<String> {
        (**self).generate_key(key_id)
    }
    fn key_meta(&self, key_id: &str) -> Result<Option<KeyMeta>> {
        (**self).key_meta(key_id)
    }
}

#[async_trait]
impl KeystoreInterface for RedbKeystore {
    async fn put(&self, key: &str, value: &[u8]) -> Result<()> {
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| GuardianError::Other(format!("Error inserting into keystore: {}", e)))?;
        {
            let mut table = write_txn
                .open_table(KEYSTORE_TABLE)
                .map_err(|e| GuardianError::Other(format!("Error opening table: {}", e)))?;
            table.insert(key, value).map_err(|e| {
                GuardianError::Other(format!("Error inserting into keystore: {}", e))
            })?;
        }
        write_txn
            .commit()
            .map_err(|e| GuardianError::Other(format!("Error committing insertion: {}", e)))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| GuardianError::Other(format!("Error retrieving from keystore: {}", e)))?;
        let table = read_txn
            .open_table(KEYSTORE_TABLE)
            .map_err(|e| GuardianError::Other(format!("Error opening table: {}", e)))?;
        match table.get(key) {
            Ok(Some(value)) => Ok(Some(value.value().to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(GuardianError::Other(format!(
                "Error retrieving from keystore: {}",
                e
            ))),
        }
    }

    async fn has(&self, key: &str) -> Result<bool> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| GuardianError::Other(format!("Error checking key in keystore: {}", e)))?;
        let table = read_txn
            .open_table(KEYSTORE_TABLE)
            .map_err(|e| GuardianError::Other(format!("Error opening table: {}", e)))?;
        match table.get(key) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(GuardianError::Other(format!(
                "Error checking key in keystore: {}",
                e
            ))),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| GuardianError::Other(format!("Error removing from keystore: {}", e)))?;
        {
            let mut table = write_txn
                .open_table(KEYSTORE_TABLE)
                .map_err(|e| GuardianError::Other(format!("Error opening table: {}", e)))?;
            table.remove(key).map_err(|e| {
                GuardianError::Other(format!("Error removing from keystore: {}", e))
            })?;
        }
        write_txn
            .commit()
            .map_err(|e| GuardianError::Other(format!("Error committing removal: {}", e)))?;
        Ok(())
    }

    fn enumerate_keys(&self) -> Result<Vec<String>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| GuardianError::Other(format!("Error opening keystore: {}", e)))?;
        let table = match read_txn.open_table(KEYSTORE_TABLE) {
            Ok(t) => t,
            Err(_) => return Ok(Vec::new()), // table not created yet
        };
        let mut keys = Vec::new();
        for item in table
            .iter()
            .map_err(|e| GuardianError::Other(format!("Error iterating keystore: {}", e)))?
        {
            let (k, _v) = item.map_err(|e| {
                GuardianError::Other(format!("Error reading keystore entry: {}", e))
            })?;
            keys.push(k.value().to_string());
        }
        Ok(keys)
    }

    fn public_key(&self, key_id: &str) -> Result<Option<String>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| GuardianError::Other(format!("Error opening keystore: {}", e)))?;
        let table = match read_txn.open_table(KEYSTORE_TABLE) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        match table.get(key_id) {
            Ok(Some(v)) => Ok(crate::log::identity_provider::derive_public(v.value())),
            Ok(None) => Ok(None),
            Err(e) => Err(GuardianError::Other(format!(
                "Error reading keystore: {}",
                e
            ))),
        }
    }

    fn generate_key(&self, key_id: &str) -> Result<String> {
        use zeroize::Zeroize;
        let (mut bytes, public) = crate::log::identity_provider::new_secret_bytes();
        // Do the fallible write in a closure so the transient secret is always
        // wiped afterwards, on both the success and error paths.
        let stored: Result<()> = (|| {
            let write_txn = self
                .db
                .begin_write()
                .map_err(|e| GuardianError::Other(format!("Error opening keystore: {}", e)))?;
            {
                let mut table = write_txn
                    .open_table(KEYSTORE_TABLE)
                    .map_err(|e| GuardianError::Other(format!("Error opening table: {}", e)))?;
                table
                    .insert(key_id, &bytes[..])
                    .map_err(|e| GuardianError::Other(format!("Error inserting key: {}", e)))?;
            }
            {
                // Record/refresh lifecycle metadata in the parallel table (D2),
                // in the same transaction so secret + metadata stay consistent.
                let mut meta_table = write_txn.open_table(KEYMETA_TABLE).map_err(|e| {
                    GuardianError::Other(format!("Error opening meta table: {}", e))
                })?;
                let now = KeyMeta::now_secs();
                let prior = meta_table
                    .get(key_id)
                    .ok()
                    .flatten()
                    .and_then(|v| KeyMeta::from_json(v.value()));
                let meta = match prior {
                    Some(mut m) => {
                        m.rotated_count = m.rotated_count.saturating_add(1);
                        m.updated_at = now;
                        m
                    }
                    None => KeyMeta {
                        created_at: now,
                        updated_at: now,
                        kind: "ed25519".to_string(),
                        rotated_count: 0,
                    },
                };
                meta_table
                    .insert(key_id, &meta.to_json()[..])
                    .map_err(|e| GuardianError::Other(format!("Error inserting meta: {}", e)))?;
            }
            write_txn
                .commit()
                .map_err(|e| GuardianError::Other(format!("Error committing key: {}", e)))?;
            Ok(())
        })();
        bytes.zeroize();
        stored.map(|()| public)
    }

    fn key_meta(&self, key_id: &str) -> Result<Option<KeyMeta>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| GuardianError::Other(format!("Error opening keystore: {}", e)))?;
        let table = match read_txn.open_table(KEYMETA_TABLE) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        match table.get(key_id) {
            Ok(Some(v)) => Ok(KeyMeta::from_json(v.value())),
            Ok(None) => Ok(None),
            Err(e) => Err(GuardianError::Other(format!("Error reading meta: {}", e))),
        }
    }
}

/// Factory function to create keystores based on configuration.
pub fn create_keystore(
    directory: Option<std::path::PathBuf>,
) -> Result<Arc<dyn KeystoreInterface + Send + Sync>> {
    let keystore = RedbKeystore::new(directory)?;
    Ok(Arc::new(keystore))
}

/// Creates a temporary in-memory keystore.
pub fn create_temp_keystore() -> Result<Arc<dyn KeystoreInterface + Send + Sync>> {
    let keystore = RedbKeystore::temporary()?;
    Ok(Arc::new(keystore))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_redb_keystore_basic_operations() {
        let keystore = RedbKeystore::temporary().unwrap();

        // Test put/get/has
        let key = "test_key";
        let value = b"test_value";

        assert!(!keystore.has(key).await.unwrap());

        keystore.put(key, value).await.unwrap();
        assert!(keystore.has(key).await.unwrap());

        let retrieved = keystore.get(key).await.unwrap().unwrap();
        assert_eq!(retrieved, value);

        // Test delete
        keystore.delete(key).await.unwrap();
        assert!(!keystore.has(key).await.unwrap());
    }

    #[test]
    fn test_generate_key_persists_across_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("admin_keystore");

        // Generate a key, then drop the store (releases the redb lock).
        let public = {
            let ks = RedbKeystore::new(Some(path.clone())).unwrap();
            let p = ks.generate_key("k1").unwrap();
            assert!(!p.is_empty());
            // The derived public matches what public_key reports.
            assert_eq!(ks.public_key("k1").unwrap().as_deref(), Some(p.as_str()));
            p
        };

        // Reopen the same path: the generated key must still be there.
        let ks2 = RedbKeystore::new(Some(path)).unwrap();
        assert_eq!(
            ks2.public_key("k1").unwrap().as_deref(),
            Some(public.as_str())
        );
        assert!(ks2.enumerate_keys().unwrap().iter().any(|k| k == "k1"));
    }

    #[tokio::test]
    async fn test_keypair_storage() {
        let keystore = RedbKeystore::temporary().unwrap();
        let key_name = "test_keypair";

        // Generate a secret key
        let original_secret = SecretKey::generate();

        // Store it
        keystore
            .put_keypair(key_name, &original_secret)
            .await
            .unwrap();

        // Retrieve it
        let retrieved_secret = keystore.get_keypair(key_name).await.unwrap().unwrap();

        // Compare public keys
        assert_eq!(original_secret.public(), retrieved_secret.public());
    }

    #[test]
    fn test_key_meta_tracks_creation_and_rotation() {
        let ks = RedbKeystore::temporary().unwrap();
        // No metadata before the key exists.
        assert!(ks.key_meta("k1").unwrap().is_none());

        // First generation: rotated_count == 0, kind ed25519, timestamps set.
        ks.generate_key("k1").unwrap();
        let m1 = ks.key_meta("k1").unwrap().expect("meta after generate");
        assert_eq!(m1.kind, "ed25519");
        assert_eq!(m1.rotated_count, 0);
        assert!(m1.created_at > 0);

        // Regenerating the same id counts as a rotation and preserves created_at.
        ks.generate_key("k1").unwrap();
        let m2 = ks.key_meta("k1").unwrap().unwrap();
        assert_eq!(m2.rotated_count, 1);
        assert_eq!(m2.created_at, m1.created_at);

        // The secret table stays a plain 32-byte secret (metadata is a sidecar).
        assert!(ks.public_key("k1").unwrap().is_some());
        assert!(
            !ks.enumerate_keys()
                .unwrap()
                .iter()
                .any(|k| k == "keystore_meta")
        );
    }

    #[tokio::test]
    async fn test_list_keys() {
        let keystore = RedbKeystore::temporary().unwrap();

        // Add some keys
        keystore.put("key1", b"value1").await.unwrap();
        keystore.put("key2", b"value2").await.unwrap();
        keystore.put("key3", b"value3").await.unwrap();

        // List keys
        let mut keys = keystore.list_keys().await.unwrap();
        keys.sort();

        assert_eq!(keys, vec!["key1", "key2", "key3"]);
    }
}
