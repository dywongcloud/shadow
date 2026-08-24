/// Wrapper client for iroh-blobs.
///
/// Provides a simplified interface for content-addressed blob storage
/// operations using BLAKE3 hashes.
///
/// This client uses the IrohBackend's shared store, ensuring consistency
/// and avoiding storage duplication.
use super::{BlobProtection, preflight_blob_size, read_blob_bounded};
use crate::guardian::error::{GuardianError, Result};
use bytes::Bytes;
use futures::StreamExt;
use iroh::EndpointId as NodeId;
use iroh::endpoint::Endpoint;
use iroh_blobs::{Hash as BlobHash, HashAndFormat, store::fs::FsStore};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, warn};

/// Detailed listing entry for a tagged blob (C4/C5): real byte size and whether
/// the blob is fully stored locally (vs. a partial download).
#[derive(Debug, Clone)]
pub struct BlobInfo {
    pub hash: BlobHash,
    /// Real byte size — the `Complete` size, a `Partial`'s known size, else 0.
    pub size: u64,
    /// True when the blob is fully stored; false for partial/missing.
    pub complete: bool,
}

/// Client for operations with iroh-blobs.
///
/// Supports local operations and P2P download of blobs from remote peers
/// when the Endpoint is configured.
#[derive(Clone)]
pub struct BlobStore {
    /// Shared iroh-blobs store (filesystem-based).
    store: Arc<RwLock<FsStore>>,
    /// Iroh Endpoint for P2P blob download (optional).
    endpoint: Option<Endpoint>,
    /// Shared with IrohBackend's collector when this client came from IrohClient.
    gc_gate: Option<Arc<RwLock<()>>>,
    /// Shared backend admission state. Standalone stores own their own lifecycle.
    accepting_work: Option<Arc<AtomicBool>>,
}

impl BlobStore {
    /// Creates a new iroh-blobs client instance using a shared store.
    ///
    /// # Arguments
    /// * `store` - The IrohBackend's shared store
    ///
    /// # Example
    /// ```no_run
    /// use std::sync::Arc;
    /// use tokio::sync::RwLock;
    /// use iroh_blobs::store::fs::FsStore;
    /// use guardian_db::p2p::network::core::BlobStore;
    ///
    /// # async fn example(fs_store: FsStore) {
    /// let store = Arc::new(RwLock::new(fs_store));
    /// let blobs_client = BlobStore::new(store);
    /// # }
    /// ```
    #[instrument(level = "debug", skip(store))]
    pub fn new(store: Arc<RwLock<FsStore>>) -> Self {
        debug!("Creating BlobStore with shared store (no P2P download)");
        Self {
            store,
            endpoint: None,
            gc_gate: None,
            accepting_work: None,
        }
    }

    /// Creates a new instance with P2P download support via an Endpoint.
    ///
    /// The Endpoint allows downloading blobs from remote peers using the native
    /// iroh-blobs protocol (QUIC + BLAKE3 verified streaming).
    #[instrument(level = "debug", skip(store, endpoint))]
    pub fn new_with_endpoint(store: Arc<RwLock<FsStore>>, endpoint: Endpoint) -> Self {
        debug!("Creating BlobStore with shared store + P2P download");
        Self {
            store,
            endpoint: Some(endpoint),
            gc_gate: None,
            accepting_work: None,
        }
    }

    pub(crate) fn new_guarded(
        store: Arc<RwLock<FsStore>>,
        endpoint: Option<Endpoint>,
        gc_gate: Arc<RwLock<()>>,
        accepting_work: Arc<AtomicBool>,
    ) -> Self {
        Self {
            store,
            endpoint,
            gc_gate: Some(gc_gate),
            accepting_work: Some(accepting_work),
        }
    }

    fn ensure_accepting_work(&self) -> Result<()> {
        if self
            .accepting_work
            .as_ref()
            .is_some_and(|accepting| !accepting.load(Ordering::Acquire))
        {
            return Err(GuardianError::Other(
                "Iroh backend is shutting down and no longer accepts blob work".to_string(),
            ));
        }
        Ok(())
    }

    async fn gc_read_guard(&self) -> Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>> {
        self.ensure_accepting_work()?;
        Ok(match &self.gc_gate {
            Some(gate) => Some(gate.clone().read_owned().await),
            None => None,
        })
    }

    /// Adds a document (bytes) to the blob store.
    ///
    /// Returns the BLAKE3 Hash of the stored content.
    #[instrument(level = "debug", skip(self, data))]
    pub async fn add_document(&self, data: Bytes) -> Result<BlobHash> {
        preflight_blob_size(data.len() as u64, "document blob being added").map_err(|error| {
            GuardianError::Other(format!(
                "Document blob exceeds the in-memory limit: {error}"
            ))
        })?;
        let _gc_guard = self.gc_read_guard().await?;
        let store = self.store.read().await;

        // Keep the import temporarily protected while replacing that guard with
        // Guardian's one deliberate persistent tag. Awaiting AddProgress directly
        // calls `with_tag()` and creates an extra anonymous persistent tag; that
        // tag survives `delete_document` and makes the blob uncollectable forever.
        let import_guard = store
            .blobs()
            .add_bytes(data.clone())
            .temp_tag()
            .await
            .map_err(|e| {
                GuardianError::Other(format!("Error adding bytes to the blob store: {}", e))
            })?;

        let hash = import_guard.hash();

        // Create the sole permanent tag to protect against GC.
        // Format: doc_<hash_hex>
        let tag_name = format!("doc_{}", hex::encode(hash.as_bytes()));

        store
            .tags()
            .set(tag_name.as_bytes(), HashAndFormat::raw(hash))
            .await
            .map_err(|e| GuardianError::Other(format!("Error creating permanent tag: {}", e)))?;
        drop(import_guard);

        debug!(
            "Document added to the blob store: {} ({} bytes)",
            hex::encode(hash.as_bytes()),
            data.len()
        );

        Ok(hash)
    }

    /// Retrieves a document from the blob store by its hash.
    #[instrument(level = "debug", skip(self))]
    pub async fn get_document(&self, hash: &BlobHash) -> Result<Bytes> {
        let _gc_guard = self.gc_read_guard().await?;
        let store = self.store.read().await;

        let data = read_blob_bounded(&store, *hash, "document blob")
            .await
            .map_err(|e| GuardianError::Other(format!("Error fetching blob: {e}")))?;

        debug!(
            "Document retrieved from the blob store: {} ({} bytes)",
            hex::encode(hash.as_bytes()),
            data.len()
        );

        Ok(data)
    }

    /// Retrieves a document from the blob store, attempting a P2P download if not found locally.
    ///
    /// If the blob does not exist in the local store and a peer provider is given,
    /// it tries to download from the remote peer using the iroh-blobs protocol.
    #[instrument(level = "debug", skip(self))]
    pub async fn get_or_download(&self, hash: &BlobHash, providers: &[NodeId]) -> Result<Bytes> {
        use iroh_blobs::api::proto::BlobStatus;

        let gc_guard = self.gc_read_guard().await?;
        let store = self.store.read().await;
        match store
            .blobs()
            .status(*hash)
            .await
            .map_err(|e| GuardianError::Other(format!("Error checking local blob: {e}")))?
        {
            BlobStatus::Complete { .. } => {
                let data = read_blob_bounded(&store, *hash, "local document blob")
                    .await
                    .map_err(|e| GuardianError::Other(format!("Error fetching local blob: {e}")))?;
                debug!(
                    "Document found locally: {} ({} bytes)",
                    hex::encode(hash.as_bytes()),
                    data.len()
                );
                return Ok(data);
            }
            BlobStatus::Partial { .. } | BlobStatus::NotFound => {
                debug!(
                    "Document incomplete or absent locally: {}, attempting P2P download",
                    hex::encode(hash.as_bytes())
                );
            }
        }
        drop(store);

        // Move the already-acquired fair GC read guard into one owned protection
        // scope. Acquiring it a second time here could deadlock behind a queued GC
        // writer; this preserves the existing mutation-gate ordering.
        let protection = self.protect_download_with_guard(*hash, gc_guard).await?;
        self.download_from_peers_inner(hash, providers).await?;

        // The temporary root spans post-download materialisation and the durable
        // document tag commit. A failed read/tag write or request cancellation drops
        // it; success never exposes an unrooted acquisition window.
        let store = self.store.read().await;
        let data = read_blob_bounded(&store, *hash, "document blob fetched from peers")
            .await
            .map_err(|e| {
                GuardianError::Other(format!("Blob unavailable after P2P download: {e}"))
            })?;
        let tag_name = format!("doc_{}", hex::encode(hash.as_bytes()));
        store
            .tags()
            .set(tag_name.as_bytes(), HashAndFormat::raw(*hash))
            .await
            .map_err(|e| GuardianError::Other(format!("Error creating permanent tag: {e}")))?;
        drop(store);
        drop(protection);

        debug!(
            "Document downloaded via P2P: {} ({} bytes)",
            hex::encode(hash.as_bytes()),
            data.len()
        );
        Ok(data)
    }

    /// Downloads a blob from remote peers using the iroh-blobs Downloader.
    ///
    /// The returned owned protection is mandatory: callers must retain it until
    /// their persistent pin or document entry commits. Dropping it on failure or
    /// cancellation releases the temporary root automatically.
    #[instrument(level = "debug", skip(self))]
    pub async fn download_from_peers(
        &self,
        hash: &BlobHash,
        providers: &[NodeId],
    ) -> Result<BlobProtection> {
        let protection = self.protect_download(*hash).await?;
        self.download_from_peers_inner(hash, providers).await?;
        Ok(protection)
    }

    async fn protect_download(&self, hash: BlobHash) -> Result<BlobProtection> {
        let gc_guard = self.gc_read_guard().await?;
        self.protect_download_with_guard(hash, gc_guard).await
    }

    async fn protect_download_with_guard(
        &self,
        hash: BlobHash,
        gc_guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<BlobProtection> {
        let store = self.store.read().await;
        let batch =
            store.blobs().batch().await.map_err(|e| {
                GuardianError::Other(format!("Error creating P2P download batch: {e}"))
            })?;
        let tag = batch
            .temp_tag(hash)
            .await
            .map_err(|e| GuardianError::Other(format!("Error protecting P2P download: {e}")))?;
        BlobProtection::new(gc_guard, vec![tag], batch)
    }

    async fn download_from_peers_inner(&self, hash: &BlobHash, providers: &[NodeId]) -> Result<()> {
        let endpoint = self.endpoint.as_ref().ok_or_else(|| {
            GuardianError::Other("Endpoint not available for P2P blob download".to_string())
        })?;

        if providers.is_empty() {
            return Err(GuardianError::Other(
                "No provider given for P2P download".to_string(),
            ));
        }

        let downloader = {
            let store = self.store.read().await;
            store.downloader(endpoint)
        };
        let providers_vec: Vec<NodeId> = providers.to_vec();
        info!(
            "Starting P2P download of blob {} from {} provider(s)",
            hex::encode(hash.as_bytes()),
            providers_vec.len()
        );

        let progress = downloader.download(*hash, providers_vec);
        let mut stream = progress
            .stream()
            .await
            .map_err(|e| GuardianError::Other(format!("Error starting P2P download: {e}")))?;

        while let Some(item) = stream.next().await {
            match &item {
                iroh_blobs::api::downloader::DownloadProgressItem::Error(e) => {
                    return Err(GuardianError::Other(format!("Error in P2P download: {e}")));
                }
                iroh_blobs::api::downloader::DownloadProgressItem::DownloadError => {
                    return Err(GuardianError::Other("P2P download failed".to_string()));
                }
                iroh_blobs::api::downloader::DownloadProgressItem::PartComplete { .. } => {
                    debug!("P2P download: part complete");
                }
                iroh_blobs::api::downloader::DownloadProgressItem::Progress(bytes) => {
                    debug!("P2P download: {} bytes received", bytes);
                }
                _ => {}
            }
        }

        info!("P2P download complete: {}", hex::encode(hash.as_bytes()));
        Ok(())
    }

    /// Checks whether a document exists in the blob store.
    #[instrument(level = "debug", skip(self))]
    pub async fn has_document(&self, hash: &BlobHash) -> Result<bool> {
        self.ensure_accepting_work()?;
        let store = self.store.read().await;

        // Use the new API: blobs().has() - requires an owned Hash.
        let has_blob = store.blobs().has(*hash).await.unwrap_or(false);

        Ok(has_blob)
    }

    /// Deletes a document from the blob store.
    ///
    /// Removes the protection tag and optionally deletes the physical blob.
    #[instrument(level = "debug", skip(self))]
    pub async fn delete_document(&self, hash: &BlobHash) -> Result<()> {
        let _gc_guard = self.gc_read_guard().await?;
        let store = self.store.read().await;

        // Remove the protection tag.
        let tag_name = format!("doc_{}", hex::encode(hash.as_bytes()));

        store
            .tags()
            .delete(tag_name.as_bytes())
            .await
            .map_err(|e| {
                warn!("Error deleting document tag: {}", e);
                GuardianError::Other(format!("Error deleting tag: {}", e))
            })?;

        // With periodic GC enabled (GUARDIAN_GC_SECS > 0), the physical blob
        // becomes collectible once no document head, temporary guard, or other
        // persistent tag references it. Shared content is never deleted eagerly.

        debug!("Document tag removed: {}", hex::encode(hash.as_bytes()));

        Ok(())
    }

    /// Lists all tagged documents in the blob store.
    ///
    /// Returns (hash, size) pairs for all documents. `size` is the real byte size,
    /// resolved via `blobs().status()` (see [`BlobStore::list_documents_status`]).
    #[instrument(level = "debug", skip(self))]
    pub async fn list_documents(&self) -> Result<Vec<(BlobHash, u64)>> {
        Ok(self
            .list_documents_status()
            .await?
            .into_iter()
            .map(|b| (b.hash, b.size))
            .collect())
    }

    /// Lists all tagged documents with real size + completeness (C4/C5).
    ///
    /// The tag stream only yields hashes; the real byte size and whether the blob
    /// is fully stored (vs. a partial download) come from `blobs().status(hash)`
    /// (`iroh-blobs 0.103` `BlobStatus`). Costs one `status()` call per document.
    #[instrument(level = "debug", skip(self))]
    pub async fn list_documents_status(&self) -> Result<Vec<BlobInfo>> {
        use futures::stream::StreamExt;
        use iroh_blobs::api::proto::BlobStatus;

        self.ensure_accepting_work()?;
        let store = self.store.read().await;
        let mut documents = Vec::new();

        // Use the new API: tags().list_prefix() to list tags with the "doc_" prefix.
        let mut tags_stream = store
            .tags()
            .list_prefix(b"doc_")
            .await
            .map_err(|e| GuardianError::Other(format!("Error getting tags: {}", e)))?;

        while let Some(tag_result) = tags_stream.next().await {
            match tag_result {
                Ok(tag_info) => {
                    let hash = tag_info.hash;
                    // Resolve real size + completeness from the store's blob status.
                    let (size, complete) = match store.blobs().status(hash).await {
                        Ok(BlobStatus::Complete { size }) => (size, true),
                        Ok(BlobStatus::Partial { size }) => (size.unwrap_or(0), false),
                        Ok(BlobStatus::NotFound) => (0, false),
                        Err(e) => {
                            warn!("status({}) failed: {}", hash, e);
                            (0, false)
                        }
                    };
                    documents.push(BlobInfo {
                        hash,
                        size,
                        complete,
                    });
                }
                Err(e) => {
                    warn!("Error processing tag during listing: {}", e);
                }
            }
        }

        debug!("Listed {} documents in the blob store", documents.len());

        Ok(documents)
    }

    /// Reports the number of hashes currently protected by persistent tags.
    ///
    /// This does not trigger collection. `IrohBackend::initialize_node` wires
    /// FsStore's periodic collector and the iroh-docs protect handler together;
    /// `GUARDIAN_GC_SECS=0` disables both halves. Current document heads are
    /// protected by that callback, while this count includes standalone/sentinel
    /// `doc_*` tags and every other persistent tag.
    #[instrument(level = "debug", skip(self))]
    pub async fn gc(&self) -> Result<u64> {
        use futures::stream::StreamExt;

        self.ensure_accepting_work()?;
        let store = self.store.read().await;

        // Collect all hashes protected by tags.
        let mut protected_hashes = std::collections::BTreeSet::new();
        let mut tags_stream = store
            .tags()
            .list()
            .await
            .map_err(|e| GuardianError::Other(format!("Error getting tags for GC: {}", e)))?;

        while let Some(tag_result) = tags_stream.next().await {
            if let Ok(tag_info) = tag_result {
                protected_hashes.insert(tag_info.hash);
            }
        }

        // Collection itself is periodic and owned by FsStore. This diagnostic
        // intentionally performs no sweep; it reports only the persistent-tag
        // side of the protection set (iroh-docs heads and temporary in-flight
        // guards are supplied independently to each GC pass).
        let protected = protected_hashes.len() as u64;
        debug!(
            "GC status: {} hashes protected by persistent tags",
            protected
        );
        Ok(protected)
    }

    /// Returns true if the BlobStore supports P2P download.
    pub fn has_p2p_support(&self) -> bool {
        self.endpoint.is_some()
    }

    /// Creates a test instance with a temporary store.
    #[cfg(test)]
    pub async fn memory() -> Result<Self> {
        // Create a temporary directory.
        let temp_dir =
            std::env::temp_dir().join(format!("iroh-blobs-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| {
            GuardianError::Other(format!("Error creating temporary directory: {}", e))
        })?;

        // Load FsStore in the temporary directory.
        let store = FsStore::load(&temp_dir)
            .await
            .map_err(|e| GuardianError::Other(format!("Error creating temporary store: {}", e)))?;

        Ok(Self::new(Arc::new(RwLock::new(store))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_add_and_get_document() {
        let blobs_client = BlobStore::memory().await.unwrap();

        let data = Bytes::from("Hello, iroh-blobs!");
        let hash = blobs_client.add_document(data.clone()).await.unwrap();

        let retrieved = blobs_client.get_document(&hash).await.unwrap();
        assert_eq!(data, retrieved);
    }

    #[tokio::test]
    async fn test_has_document() {
        let blobs_client = BlobStore::memory().await.unwrap();

        let data = Bytes::from("Test data");
        let hash = blobs_client.add_document(data).await.unwrap();

        assert!(blobs_client.has_document(&hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_delete_document() {
        let blobs_client = BlobStore::memory().await.unwrap();

        let data = Bytes::from("To be deleted");
        let hash = blobs_client.add_document(data).await.unwrap();

        blobs_client.delete_document(&hash).await.unwrap();

        // After deleting the tag, GC may remove the blob.
        // But immediately after delete_document it may still exist
        // until GC runs.
    }

    #[tokio::test]
    async fn test_list_documents() {
        let blobs_client = BlobStore::memory().await.unwrap();

        let data1 = Bytes::from("Document 1");
        let data2 = Bytes::from("Document 2");

        blobs_client.add_document(data1).await.unwrap();
        blobs_client.add_document(data2).await.unwrap();

        let docs = blobs_client.list_documents().await.unwrap();
        assert_eq!(docs.len(), 2);
    }
}
