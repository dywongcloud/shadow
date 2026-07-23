/// Optimized cache layer for the Iroh backend.
///
/// Intelligent caching with compression, adaptive TTL and predictive eviction
/// to maximize the performance of the native Iroh backend.
use crate::guardian::error::{GuardianError, Result};
use blake3::Hasher;
use bytes::Bytes;
use iroh_blobs::BlobFormat;
use lru::LruCache;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, instrument, warn};

/// Optimized cache layer for Iroh operations.
pub struct OptimizedCache {
    /// LRU cache for recent data.
    data_cache: Arc<RwLock<LruCache<String, CacheEntry>>>,
    /// Metadata cache for CIDs.
    metadata_cache: Arc<RwLock<HashMap<String, MetadataEntry>>>,
    /// Compression cache for large data.
    compressed_cache: Arc<RwLock<LruCache<String, CompressedEntry>>>,
    /// Performance statistics.
    stats: Arc<RwLock<CacheStats>>,
    /// Cache configuration.
    cache_config: CacheConfig,
    /// Access predictor for intelligent eviction.
    access_predictor: Arc<Mutex<AccessPredictor>>,
    /// Live byte footprint of `data_cache` (uncompressed entry bytes).
    ///
    /// Guardian storage is content-addressed, so every mutated value is a NEW
    /// CID that is never read again — the old CIDs pile up. The upstream cache
    /// bounded ONLY by entry count (50k compressed / 10k data) with the byte
    /// budgets (`max_*_cache_size`) never enforced, so 50k multi-MB blobs could
    /// retain hundreds of GB of anon heap (the fleet's OOM/lockout root cause).
    /// These counters make the BYTE budget the real bound: the LRUs are given a
    /// huge entry capacity so their own count-eviction never fires, and every
    /// insert enforces the byte budget via `pop_lru`, keeping the counter exact.
    data_bytes: Arc<AtomicU64>,
    /// Live byte footprint of `compressed_cache` (compressed entry bytes).
    compressed_bytes: Arc<AtomicU64>,
}

/// Cache entry with performance metadata.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Blob data.
    pub data: Bytes,
    /// Creation timestamp.
    pub created_at: Instant,
    /// Last access.
    pub last_accessed: Instant,
    /// Number of accesses.
    pub access_count: u64,
    /// Priority (0-10, higher = more important).
    pub priority: u8,
    /// Original size (before compression, if applicable).
    pub original_size: usize,
    /// Integrity verification hash.
    pub integrity_hash: [u8; 32],
}

/// Compressed entry for large data.
#[derive(Debug, Clone)]
pub struct CompressedEntry {
    /// Data compressed with zstd.
    pub compressed_data: Bytes,
    /// Original size.
    pub original_size: usize,
    /// Compression level used.
    pub compression_level: i32,
    /// Compression timestamp.
    pub compressed_at: Instant,
    /// Compression ratio (0.0-1.0).
    pub compression_ratio: f64,
}

/// Metadata for CIDs.
#[derive(Debug, Clone)]
pub struct MetadataEntry {
    /// Blob size.
    pub size: u64,
    /// Blob format (Raw, DagCbor, etc.).
    pub format: BlobFormat,
    /// Peers that hold the content.
    pub providers: Vec<String>,
    /// Discovery timestamp.
    pub discovered_at: Instant,
    /// Average access latency (ms).
    pub avg_access_latency_ms: f64,
    /// Popularity (access frequency).
    pub popularity_score: f64,
}

/// Advanced cache statistics.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Hits in the data cache.
    pub data_cache_hits: u64,
    /// Misses in the data cache.
    pub data_cache_misses: u64,
    /// Hits in the compressed cache.
    pub compressed_cache_hits: u64,
    /// Misses in the compressed cache.
    pub compressed_cache_misses: u64,
    /// Total bytes stored.
    pub total_bytes_cached: u64,
    /// Bytes saved through compression.
    pub bytes_saved_compression: u64,
    /// Bytes saved by avoiding downloads.
    pub bytes_saved_network: u64,
    /// Average access time (microseconds).
    pub avg_access_time_us: f64,
    /// Global hit rate.
    pub hit_rate: f64,
    /// Number of evictions.
    pub evictions_count: u64,
    /// Number of compressions performed.
    pub compressions_count: u64,
}

/// Configuration of the optimized cache.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum data cache size (bytes).
    pub max_data_cache_size: usize,
    /// Maximum number of entries in the data cache.
    pub max_data_entries: usize,
    /// Maximum compressed cache size (bytes).
    pub max_compressed_cache_size: usize,
    /// Maximum number of entries in the compressed cache.
    pub max_compressed_entries: usize,
    /// Default TTL for entries (seconds).
    pub default_ttl_secs: u64,
    /// Threshold for enabling compression (bytes).
    pub compression_threshold: usize,
    /// zstd compression level (1-22).
    pub compression_level: i32,
    /// Threshold for eviction (0.0-1.0).
    pub eviction_threshold: f64,
    /// Enable the access predictor.
    pub enable_access_prediction: bool,
}

/// Access predictor using usage patterns.
#[derive(Debug)]
pub struct AccessPredictor {
    /// Access history per CID.
    access_history: HashMap<String, Vec<Instant>>,
    /// Identified patterns.
    #[allow(dead_code)]
    patterns: HashMap<String, AccessPattern>,
    /// Analysis window (seconds).
    analysis_window_secs: u64,
}

/// Identified access pattern.
#[derive(Debug, Clone)]
pub struct AccessPattern {
    /// Average access frequency (accesses per hour).
    pub avg_frequency: f64,
    /// Peak hours.
    pub peak_hours: Vec<u8>,
    /// Probability of re-access in the coming hours.
    pub reaccess_probability: f64,
    /// Identified pattern type.
    pub pattern_type: PatternType,
}

/// Access pattern types.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternType {
    /// One-time access (unlikely to be re-accessed).
    OneTime,
    /// Regular access (consistent pattern).
    Regular,
    /// Burst access (intense spikes).
    Burst,
    /// Seasonal access (by time/day).
    Seasonal,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_data_cache_size: 256 * 1024 * 1024, // 256MB
            max_data_entries: 10_000,
            max_compressed_cache_size: 1024 * 1024 * 1024, // 1GB
            max_compressed_entries: 50_000,
            default_ttl_secs: 3600,           // 1 hour
            compression_threshold: 64 * 1024, // 64 KB
            compression_level: 6,             // Balance between speed/compression
            eviction_threshold: 0.85,
            enable_access_prediction: true,
        }
    }
}

impl OptimizedCache {
    /// Creates a new instance of the optimized cache.
    pub fn new(cache_config: CacheConfig) -> Self {
        // UNBOUNDED entry count so the LRUs' OWN count-eviction never fires —
        // the BYTE budget (enforced on every insert via `pop_lru`) is the real
        // bound, and it must be the sole evictor for the byte counters to stay
        // exact. MUST be `LruCache::unbounded()`, NEVER
        // `LruCache::new(usize::MAX)`: lru's `new` pre-reserves its hashbrown
        // table for the requested capacity, and a usize::MAX reservation
        // panics at guardian init ("Hash table capacity overflow") — witnessed
        // live as a fleet-wide guardian init retry-loop that silently took
        // store replication down while the node otherwise ran fine.
        Self {
            data_cache: Arc::new(RwLock::new(LruCache::unbounded())),
            metadata_cache: Arc::new(RwLock::new(HashMap::new())),
            compressed_cache: Arc::new(RwLock::new(LruCache::unbounded())),
            stats: Arc::new(RwLock::new(CacheStats::default())),
            cache_config,
            access_predictor: Arc::new(Mutex::new(AccessPredictor {
                access_history: HashMap::new(),
                patterns: HashMap::new(),
                analysis_window_secs: 3600 * 24, // 24 hours
            })),
            data_bytes: Arc::new(AtomicU64::new(0)),
            compressed_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Evict LRU entries from `compressed_cache` until its live byte footprint
    /// is at or below `max_compressed_cache_size`. Called after every insert;
    /// `compressed_bytes` is kept exact because this is the ONLY evictor.
    async fn evict_compressed_to_budget(&self) {
        let budget = self.cache_config.max_compressed_cache_size as u64;
        if self.compressed_bytes.load(Ordering::Relaxed) <= budget {
            return;
        }
        let mut freed = 0u64;
        {
            let mut cache = self.compressed_cache.write().await;
            while self.compressed_bytes.load(Ordering::Relaxed).saturating_sub(freed) > budget {
                match cache.pop_lru() {
                    Some((_k, v)) => freed += v.compressed_data.len() as u64,
                    None => break,
                }
            }
        }
        if freed > 0 {
            self.compressed_bytes.fetch_sub(freed, Ordering::Relaxed);
            let mut stats = self.stats.write().await;
            stats.total_bytes_cached = stats.total_bytes_cached.saturating_sub(freed);
            stats.evictions_count += 1;
            debug!("compressed cache byte-eviction freed {} bytes", freed);
        }
    }

    /// Evict LRU entries from `data_cache` until its live byte footprint is at
    /// or below `max_data_cache_size`.
    async fn evict_data_to_budget(&self) {
        let budget = self.cache_config.max_data_cache_size as u64;
        if self.data_bytes.load(Ordering::Relaxed) <= budget {
            return;
        }
        let mut freed = 0u64;
        {
            let mut cache = self.data_cache.write().await;
            while self.data_bytes.load(Ordering::Relaxed).saturating_sub(freed) > budget {
                match cache.pop_lru() {
                    Some((_k, v)) => freed += v.data.len() as u64,
                    None => break,
                }
            }
        }
        if freed > 0 {
            self.data_bytes.fetch_sub(freed, Ordering::Relaxed);
            let mut stats = self.stats.write().await;
            stats.total_bytes_cached = stats.total_bytes_cached.saturating_sub(freed);
            stats.evictions_count += 1;
            debug!("data cache byte-eviction freed {} bytes", freed);
        }
    }

    /// Looks up data in the cache with intelligent optimizations.
    #[instrument(skip(self))]
    pub async fn get(&self, cid: &str) -> Option<Bytes> {
        let start_time = Instant::now();

        // Update the access history.
        if self.cache_config.enable_access_prediction {
            self.update_access_history(cid).await;
        }

        // Try the data cache first (fastest).
        {
            let mut cache = self.data_cache.write().await;
            if let Some(entry) = cache.get_mut(cid) {
                entry.last_accessed = Instant::now();
                entry.access_count += 1;

                // Update statistics.
                let mut stats = self.stats.write().await;
                stats.data_cache_hits += 1;
                stats.avg_access_time_us =
                    (stats.avg_access_time_us + start_time.elapsed().as_micros() as f64) / 2.0;

                debug!("Cache hit (data): {} ({} bytes)", cid, entry.data.len());
                return Some(entry.data.clone());
            }
        }

        // Try the compressed cache.
        {
            let mut compressed_cache = self.compressed_cache.write().await;
            if let Some(compressed_entry) = compressed_cache.get_mut(cid) {
                // Decompress the data.
                match self
                    .decompress_data(
                        &compressed_entry.compressed_data,
                        compressed_entry.original_size,
                    )
                    .await
                {
                    Ok(decompressed) => {
                        // Move it into the data cache for faster access.
                        let cache_entry = CacheEntry {
                            data: decompressed.clone(),
                            created_at: compressed_entry.compressed_at,
                            last_accessed: Instant::now(),
                            access_count: 1,
                            priority: 7, // High priority for decompressed data.
                            original_size: compressed_entry.original_size,
                            integrity_hash: self.calculate_hash(&decompressed),
                        };

                        let promoted_bytes = cache_entry.data.len() as u64;
                        {
                            let mut data_cache = self.data_cache.write().await;
                            let old = data_cache.put(cid.to_string(), cache_entry);
                            if let Some(old) = old {
                                self.data_bytes.fetch_sub(old.data.len() as u64, Ordering::Relaxed);
                            }
                        }
                        self.data_bytes.fetch_add(promoted_bytes, Ordering::Relaxed);
                        self.evict_data_to_budget().await;

                        // Update statistics.
                        let mut stats = self.stats.write().await;
                        stats.compressed_cache_hits += 1;
                        stats.avg_access_time_us = (stats.avg_access_time_us
                            + start_time.elapsed().as_micros() as f64)
                            / 2.0;

                        debug!(
                            "Cache hit (compressed): {} ({} bytes decompressed)",
                            cid,
                            decompressed.len()
                        );
                        return Some(decompressed);
                    }
                    Err(e) => {
                        warn!("Failed to decompress cached data for {}: {}", cid, e);
                        // Remove the corrupted entry (keep the byte counter exact).
                        if let Some(old) = compressed_cache.pop(cid) {
                            self.compressed_bytes
                                .fetch_sub(old.compressed_data.len() as u64, Ordering::Relaxed);
                        }
                    }
                }
            }
        }

        // Miss in both caches.
        let mut stats = self.stats.write().await;
        stats.data_cache_misses += 1;
        stats.compressed_cache_misses += 1;

        debug!("Cache miss: {}", cid);
        None
    }

    /// Stores data in the cache with automatic optimization.
    #[instrument(skip(self, data))]
    pub async fn put(&self, cid: &str, data: Bytes) -> Result<()> {
        let data_size = data.len();
        let integrity_hash = self.calculate_hash(&data);

        // Decide whether to compress based on the size.
        let should_compress = data_size >= self.cache_config.compression_threshold;

        if should_compress {
            // Try compression.
            match self.compress_data(&data).await {
                Ok((compressed_data, compression_ratio)) => {
                    let compressed_entry = CompressedEntry {
                        compressed_data,
                        original_size: data_size,
                        compression_level: self.cache_config.compression_level,
                        compressed_at: Instant::now(),
                        compression_ratio,
                    };

                    // Store it in the compressed cache, tracking the exact
                    // compressed byte footprint (subtracting any prior value for
                    // this same CID that `put` returns).
                    let new_bytes = compressed_entry.compressed_data.len() as u64;
                    {
                        let mut compressed_cache = self.compressed_cache.write().await;
                        let old = compressed_cache.put(cid.to_string(), compressed_entry);
                        if let Some(old) = old {
                            self.compressed_bytes
                                .fetch_sub(old.compressed_data.len() as u64, Ordering::Relaxed);
                        }
                    }
                    self.compressed_bytes.fetch_add(new_bytes, Ordering::Relaxed);

                    // Update statistics.
                    {
                        let mut stats = self.stats.write().await;
                        stats.compressions_count += 1;
                        stats.bytes_saved_compression +=
                            (data_size as f64 * (1.0 - compression_ratio)) as u64;
                        stats.total_bytes_cached += new_bytes;
                    }
                    // Enforce the compressed byte budget (the real bound).
                    self.evict_compressed_to_budget().await;

                    info!(
                        "Data compressed and stored: {} ({} bytes -> {} bytes, ratio: {:.2})",
                        cid,
                        data_size,
                        (data_size as f64 * compression_ratio) as usize,
                        compression_ratio
                    );
                }
                Err(e) => {
                    warn!(
                        "Compression failed for {}: {}. Storing without compression.",
                        cid, e
                    );
                    self.store_uncompressed(cid, data, integrity_hash).await?;
                }
            }
        } else {
            // Store without compression.
            self.store_uncompressed(cid, data, integrity_hash).await?;
        }

        // Byte-budget eviction already ran inside the insert paths above
        // (`evict_compressed_to_budget` / `evict_data_to_budget`), which is the
        // authoritative bound and keeps the byte counters exact. The old
        // access-pattern `check_and_evict` is intentionally NOT called here: it
        // decremented `total_bytes_cached` without updating the per-cache byte
        // counters, which would desync them and cause over-eviction.

        Ok(())
    }

    /// Stores data without compression.
    async fn store_uncompressed(
        &self,
        cid: &str,
        data: Bytes,
        integrity_hash: [u8; 32],
    ) -> Result<()> {
        let cache_entry = CacheEntry {
            data: data.clone(),
            created_at: Instant::now(),
            last_accessed: Instant::now(),
            access_count: 1,
            priority: 5, // Default priority.
            original_size: data.len(),
            integrity_hash,
        };

        let new_bytes = data.len() as u64;
        {
            let mut data_cache = self.data_cache.write().await;
            let old = data_cache.put(cid.to_string(), cache_entry);
            if let Some(old) = old {
                self.data_bytes.fetch_sub(old.data.len() as u64, Ordering::Relaxed);
            }
        }
        self.data_bytes.fetch_add(new_bytes, Ordering::Relaxed);

        // Update statistics.
        {
            let mut stats = self.stats.write().await;
            stats.total_bytes_cached += new_bytes;
        }
        self.evict_data_to_budget().await;

        debug!(
            "Data stored (without compression): {} ({} bytes)",
            cid,
            data.len()
        );
        Ok(())
    }

    /// Compresses data using zstd.
    async fn compress_data(&self, data: &Bytes) -> Result<(Bytes, f64)> {
        let original_size = data.len();

        let compressed = tokio::task::spawn_blocking({
            let data = data.clone();
            let compression_level = self.cache_config.compression_level;
            move || {
                zstd::bulk::compress(&data, compression_level)
                    .map_err(|e| GuardianError::Other(format!("Compression failed: {}", e)))
            }
        })
        .await
        .map_err(|e| GuardianError::Other(format!("Compression task failed: {}", e)))??;

        let compressed_size = compressed.len();
        let compression_ratio = compressed_size as f64 / original_size as f64;

        Ok((Bytes::from(compressed), compression_ratio))
    }

    /// Decompresses data using zstd.
    async fn decompress_data(
        &self,
        compressed_data: &Bytes,
        expected_size: usize,
    ) -> Result<Bytes> {
        let decompressed = tokio::task::spawn_blocking({
            let compressed_data = compressed_data.clone();
            move || {
                zstd::bulk::decompress(&compressed_data, expected_size)
                    .map_err(|e| GuardianError::Other(format!("Decompression failed: {}", e)))
            }
        })
        .await
        .map_err(|e| GuardianError::Other(format!("Decompression task failed: {}", e)))??;

        Ok(Bytes::from(decompressed))
    }

    /// Computes the integrity hash.
    fn calculate_hash(&self, data: &Bytes) -> [u8; 32] {
        let mut hasher = Hasher::new();
        hasher.update(data);
        hasher.finalize().into()
    }

    /// Updates the access history for prediction.
    async fn update_access_history(&self, cid: &str) {
        let mut predictor = self.access_predictor.lock().await;
        let now = Instant::now();

        predictor
            .access_history
            .entry(cid.to_string())
            .or_insert_with(Vec::new)
            .push(now);

        // Limit the history so it does not grow indefinitely.
        let analysis_window = predictor.analysis_window_secs; // Copy the value before the borrow.
        if let Some(history) = predictor.access_history.get_mut(cid) {
            // Use checked_sub to avoid overflow.
            if let Some(cutoff) = now.checked_sub(Duration::from_secs(analysis_window)) {
                history.retain(|&access_time| access_time > cutoff);
            }
        }
    }

    /// Checks whether eviction is needed and performs it if so.
    #[allow(dead_code)]
    async fn check_and_evict(&self) -> Result<()> {
        let stats = self.stats.read().await;
        let current_usage = stats.total_bytes_cached as f64;
        let max_usage = (self.cache_config.max_data_cache_size
            + self.cache_config.max_compressed_cache_size) as f64;

        if current_usage / max_usage > self.cache_config.eviction_threshold {
            drop(stats); // Release the lock.
            self.intelligent_eviction().await?;
        }

        Ok(())
    }

    /// Performs intelligent eviction based on access patterns.
    #[allow(dead_code)]
    async fn intelligent_eviction(&self) -> Result<()> {
        debug!("Starting intelligent cache eviction");

        // Collect eviction candidates from the data cache.
        let candidates = {
            let data_cache = self.data_cache.read().await;
            data_cache
                .iter()
                .map(|(cid, entry)| {
                    let age_score = Instant::now()
                        .saturating_duration_since(entry.last_accessed)
                        .as_secs() as f64;
                    let frequency_score = 1.0 / (entry.access_count as f64 + 1.0);
                    let priority_score = (10 - entry.priority) as f64;

                    // Higher score = better eviction candidate.
                    let eviction_score =
                        age_score * 0.4 + frequency_score * 0.3 + priority_score * 0.3;

                    (cid.clone(), eviction_score, entry.data.len())
                })
                .collect::<Vec<_>>()
        };

        // Sort by eviction score (highest first).
        let mut sorted_candidates = candidates;
        sorted_candidates
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Remove 20% of the candidates.
        let eviction_count = (sorted_candidates.len() as f64 * 0.2).ceil() as usize;
        let mut bytes_freed = 0u64;

        {
            let mut data_cache = self.data_cache.write().await;
            for (cid, _score, size) in sorted_candidates.iter().take(eviction_count) {
                if data_cache.pop(cid).is_some() {
                    bytes_freed += *size as u64;
                }
            }
        }

        // Update statistics.
        {
            let mut stats = self.stats.write().await;
            stats.evictions_count += eviction_count as u64;
            stats.total_bytes_cached = stats.total_bytes_cached.saturating_sub(bytes_freed);
        }

        info!(
            "Eviction complete: {} entries removed, {} bytes freed",
            eviction_count, bytes_freed
        );

        Ok(())
    }

    /// Returns the current cache statistics.
    pub async fn get_stats(&self) -> CacheStats {
        let stats = self.stats.read().await;
        let mut stats_copy = stats.clone();

        // Compute the hit rate.
        let total_requests = stats_copy.data_cache_hits
            + stats_copy.data_cache_misses
            + stats_copy.compressed_cache_hits
            + stats_copy.compressed_cache_misses;
        let total_hits = stats_copy.data_cache_hits + stats_copy.compressed_cache_hits;

        if total_requests > 0 {
            stats_copy.hit_rate = total_hits as f64 / total_requests as f64;
        }

        stats_copy
    }

    /// Clears the entire cache.
    pub async fn clear(&self) -> Result<()> {
        {
            let mut data_cache = self.data_cache.write().await;
            data_cache.clear();
        }

        {
            let mut compressed_cache = self.compressed_cache.write().await;
            compressed_cache.clear();
        }

        {
            let mut metadata_cache = self.metadata_cache.write().await;
            metadata_cache.clear();
        }

        {
            let mut stats = self.stats.write().await;
            *stats = CacheStats::default();
        }
        self.data_bytes.store(0, Ordering::Relaxed);
        self.compressed_bytes.store(0, Ordering::Relaxed);

        info!("Cache cleared completely");
        Ok(())
    }
}
