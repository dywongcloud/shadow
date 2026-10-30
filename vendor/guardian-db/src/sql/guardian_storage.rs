//! PostgreSQL-compatible SQL on top of GuardianDB's replicated document storage.
//!
//! This module (enabled by the `sql` feature) bridges the storage-agnostic
//! [`crate::relational::RelationalStorage`] boundary used by the
//! [`guardian_sql`] engine onto a GuardianDB [`DocumentStore`]. Each relational
//! table maps to a key-prefixed set of documents inside a single GuardianDB
//! document store; the catalog is one more document. Rows therefore replicate
//! exactly like any other GuardianDB document — preserving the local-first, P2P
//! model — while the relational engine reads a synchronous, locally-mirrored
//! view (the existing DocumentStore index).
//!
//! ```no_run
//! # async fn run(db: &guardian_db::guardian::GuardianDB) -> Result<(), Box<dyn std::error::Error>> {
//! use guardian_db::sql::open_sql;
//! use guardian_db::sql::engine::Session;
//!
//! let database = open_sql(db, "app").await?;
//! let mut session = Session::new(database, "guardian");
//! session.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").await?;
//! session.execute("INSERT INTO users VALUES (1, 'Alice')").await?;
//! # Ok(()) }
//! ```

use crate::guardian::GuardianDB;
use crate::guardian::error::{GuardianError, Result as GuardianResult};
use crate::relational::error::Result as RelResult;
use crate::relational::{RelError, RelationalStorage};
use crate::sql::engine::Database;
use crate::traits::{AsyncDocumentFilter, Document, DocumentStore};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Map, Value as Json};
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Separator between a collection prefix and a row id in the GuardianDB key.
/// `0x1f` (unit separator) does not occur in the engine's row ids.
const SEP: char = '\u{1f}';
/// Reserved collection used to persist the serialized catalog.
const CATALOG_COLLECTION: &str = "__gdb_sql_catalog";
/// How many lazily-fetched row blobs a single `scan` may have in flight at
/// once. Bounded deliberately: a large table's misses are one blob read each,
/// so this converts a serialized sum of fetch latencies into a bounded number
/// of batches without opening 21k concurrent store reads.
const SCAN_FETCH_CONCURRENCY: usize = 64;

/// Consistency mode for the GuardianDB-backed SQL layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Consistency {
    /// Local-first: statements are atomic on the local replica; replication is
    /// asynchronous and converges by GuardianDB/CRDT (LWW) semantics. This is
    /// the default and matches GuardianDB's model.
    #[default]
    LocalFirst,
    /// Strict SQL: route writes through a single-writer leader so that
    /// uniqueness and ordering are globally enforced. The routing flag and API
    /// exist; the cross-node leader/coordinator is an in-progress component
    /// (see `docs/postgres-compat.md`).
    Strict,
}

/// A [`RelationalStorage`] implementation backed by a GuardianDB document store.
pub struct GuardianRelationalStorage {
    store: Arc<dyn DocumentStore<Error = GuardianError>>,
    consistency: Consistency,
}

impl GuardianRelationalStorage {
    pub fn new(store: Arc<dyn DocumentStore<Error = GuardianError>>) -> Self {
        Self {
            store,
            consistency: Consistency::LocalFirst,
        }
    }

    pub fn with_consistency(mut self, consistency: Consistency) -> Self {
        self.consistency = consistency;
        self
    }

    pub fn consistency(&self) -> Consistency {
        self.consistency
    }

    /// Re-synchronize the local document-store index from replicated state.
    ///
    /// The relational engine reads the DocumentStore's synchronous local index;
    /// that index updates on local writes and on `load`/`sync`, but not
    /// automatically when documents arrive from peers in the background. A
    /// gateway serving a replicating node should call this (e.g. periodically or
    /// before a read) to observe remote writes. Returns the number of rows
    /// re-synced.
    pub async fn refresh(&self) -> GuardianResult<()> {
        self.store.load(0).await
    }

    /// Number of keys the local document index currently knows — `0` until
    /// the first full walk completes (see `refresh`), which is how a caller
    /// tells "empty database" from "index not built yet".
    pub fn index_len(&self) -> usize {
        self.store.index().len().unwrap_or(0)
    }

    /// Materialize every row value the index knows only by hash, in ONE
    /// pass (the store's async `query` walks the key set and lazily fetches
    /// each value, caching it). A cold index has every remotely-written row
    /// as hash-only, and each such row costs a peer fetch on first read —
    /// hundreds of milliseconds each — so a full-table scan on a cold index
    /// (measured: even `teams`) exceeds a 10 s statement budget. Run this
    /// once after the walk, off every request path. Returns the number of
    /// documents materialized.
    pub async fn warm_values(&self) -> GuardianResult<usize> {
        let all: AsyncDocumentFilter = Box::pin(|_document: &Document| {
            Box::pin(async { Ok(true) })
                as Pin<Box<dyn Future<Output = Result<bool, Box<dyn Error + Send + Sync>>> + Send>>
        });
        Ok(self.store.query(all).await?.len())
    }

    fn gkey(collection: &str, row_id: &str) -> String {
        format!("{collection}{SEP}{row_id}")
    }

    /// Persist a relational document wrapped with its GuardianDB key so that
    /// rows from different tables never collide on `_id`.
    async fn write_wrapped(&self, gkey: String, collection: &str, doc: &Json) -> RelResult<()> {
        let mut wrapped = Map::new();
        wrapped.insert("_id".to_string(), Json::String(gkey));
        wrapped.insert(
            "__collection".to_string(),
            Json::String(collection.to_string()),
        );
        wrapped.insert("doc".to_string(), doc.clone());
        let document: Document = Box::new(Json::Object(wrapped));
        self.store.put(document).await.map_err(map_err)?;
        Ok(())
    }

    fn unwrap_doc(bytes: &[u8]) -> RelResult<Option<Json>> {
        let mut wrapped: Json =
            serde_json::from_slice(bytes).map_err(|e| RelError::Storage(e.to_string()))?;
        // `remove`, not `get(...).cloned()`: the row payload is the bulk of the
        // document, and every scan of a large table deep-cloned it once per
        // row before returning it. Same semantics — `None` when the key is
        // absent, `Some(Null)` when it is JSON null.
        Ok(wrapped.as_object_mut().and_then(|m| m.remove("doc")))
    }

    /// One wrapped row document by its full key, through the store's ASYNC
    /// `get` — which lazily fetches a value the index only knows by hash.
    /// The synchronous `index().get_bytes` path answers `None` for every key
    /// a cold-start walk registered but nothing has read yet, and the SQL
    /// catalog is exactly such a key on every boot: reading it through the
    /// index made the engine load an empty catalog on a fully built index
    /// (every table "does not exist"), measured 2026-09-02 fleet-wide.
    async fn fetch_wrapped(&self, gkey: &str) -> RelResult<Option<Json>> {
        let documents = self.store.get(gkey, None).await.map_err(map_err)?;
        for document in documents {
            let Some(wrapped) = document.downcast_ref::<Json>() else {
                continue;
            };
            if wrapped.get("_id").and_then(Json::as_str) == Some(gkey) {
                return Ok(wrapped.get("doc").cloned());
            }
        }
        Ok(None)
    }
}

fn map_err(e: GuardianError) -> RelError {
    RelError::Storage(e.to_string())
}

#[async_trait]
impl RelationalStorage for GuardianRelationalStorage {
    async fn scan(&self, collection: &str) -> RelResult<Vec<(String, Json)>> {
        let prefix = format!("{collection}{SEP}");
        let index = self.store.index();
        let keys = index.keys().map_err(map_err)?;
        let total_keys = keys.len();
        // Progress log: the previous timer only fired on COMPLETION, so its
        // absence was consistent with "fast" and with "never finished" — an
        // inference error that mis-attributed the 10 s. Emitting while the walk
        // runs distinguishes them.
        let walk_started = std::time::Instant::now();
        tracing::info!(collection = %collection, keys = total_keys, "sql: scan start");
        // Two phases, because the per-row miss path is an async blob fetch
        // (`cat_bytes`) and awaiting it once per row serialized ~21.5k round
        // trips at 2.5 ms each, which is the whole 10 s exec budget on its own.
        // Phase 1 resolves every already-cached row synchronously (free) and
        // records the misses with the output slot they belong to; phase 2
        // fetches those concurrently under a fixed bound, so wall-clock becomes
        // the slowest fetch per batch instead of the sum of all of them. Slots
        // keep the row order identical to the key walk, so a scan with no
        // ORDER BY stays byte-for-byte as deterministic as it was.
        let mut out: Vec<Option<(String, Json)>> = Vec::new();
        let mut misses: Vec<(usize, String, String)> = Vec::new();
        let mut seen = 0usize;
        let mut hit_rows = 0u64;
        let mut miss_rows = 0u64;
        let mut miss_ms = 0u64;
        for key in keys {
            let Some(row_id) = key.strip_prefix(&prefix) else {
                continue;
            };
            seen += 1;
            let slot = out.len();
            out.push(None);
            match index.get_bytes(&key).map_err(map_err)? {
                Some(bytes) => {
                    hit_rows += 1;
                    out[slot] = Self::unwrap_doc(&bytes)?.map(|doc| (row_id.to_string(), doc));
                }
                None => {
                    miss_rows += 1;
                    misses.push((slot, row_id.to_string(), key));
                }
            }
            if seen % 5000 == 0 {
                tracing::info!(
                    collection = %collection,
                    matched = seen,
                    hit_rows,
                    miss_rows,
                    elapsed_ms = walk_started.elapsed().as_millis() as u64,
                    "sql: scan progress"
                );
            }
        }
        if !misses.is_empty() {
            let fetch_started = std::time::Instant::now();
            let fetched: Vec<(usize, String, RelResult<Option<Json>>)> =
                futures::stream::iter(misses.into_iter().map(|(slot, row_id, key)| async move {
                    (slot, row_id, self.fetch_wrapped(&key).await)
                }))
                .buffer_unordered(SCAN_FETCH_CONCURRENCY)
                .collect()
                .await;
            for (slot, row_id, doc) in fetched {
                if let Some(doc) = doc? {
                    out[slot] = Some((row_id, doc));
                }
            }
            miss_ms = fetch_started.elapsed().as_millis() as u64;
        }
        let out: Vec<(String, Json)> = out.into_iter().flatten().collect();
        tracing::info!(
            collection = %collection,
            rows = out.len(),
            keys = total_keys,
            hit_rows,
            miss_rows,
            miss_ms,
            elapsed_ms = walk_started.elapsed().as_millis() as u64,
            "sql: scan done"
        );
        Ok(out)
    }

    /// Row ids only — no value decoded. See the trait method: `COUNT(*)` and
    /// the row-id-level RLS / `FOR UPDATE` filters need no column value, and
    /// paying a blob fetch plus a JSON parse per row to then throw the values
    /// away was the largest single cost in the engine.
    ///
    /// Reachability is decided EXACTLY as `scan` decides it — a row counts
    /// when its value is cached or when the lazy fetch of it succeeds — so the
    /// two can never disagree. That matters: rows whose blob no peer can serve
    /// ("P2P blob fetch failed from every admitted peer", 433 of them in 20
    /// minutes on the leader) are legitimately dropped by `scan`, and a count
    /// taken from the bare key set would report them as present while
    /// `SELECT *` did not. Same two-phase shape as `scan` (cached pass, then a
    /// bounded concurrent fetch of the misses); only the decode is skipped.
    async fn row_ids(&self, collection: &str) -> RelResult<Vec<String>> {
        let prefix = format!("{collection}{SEP}");
        let index = self.store.index();
        let keys = index.keys().map_err(map_err)?;
        let started = std::time::Instant::now();
        let mut ids: Vec<Option<String>> = Vec::new();
        let mut misses: Vec<(usize, String, String)> = Vec::new();
        for key in keys {
            let Some(row_id) = key.strip_prefix(&prefix) else {
                continue;
            };
            let slot = ids.len();
            match index.get_bytes(&key).map_err(map_err)? {
                Some(bytes) => ids.push(if Self::unwrap_doc(&bytes)?.is_some() {
                    Some(row_id.to_string())
                } else {
                    None
                }),
                None => {
                    ids.push(None);
                    misses.push((slot, row_id.to_string(), key));
                }
            }
        }
        if !misses.is_empty() {
            let fetched: Vec<(usize, String, RelResult<Option<Json>>)> =
                futures::stream::iter(misses.into_iter().map(|(slot, row_id, key)| async move {
                    (slot, row_id, self.fetch_wrapped(&key).await)
                }))
                .buffer_unordered(SCAN_FETCH_CONCURRENCY)
                .collect()
                .await;
            for (slot, row_id, doc) in fetched {
                if doc?.is_some() {
                    ids[slot] = Some(row_id);
                }
            }
        }
        let ids: Vec<String> = ids.into_iter().flatten().collect();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if elapsed_ms >= 50 {
            tracing::info!(
                collection = %collection,
                rows = ids.len(),
                elapsed_ms,
                "sql: row_ids"
            );
        }
        Ok(ids)
    }

    async fn get(&self, collection: &str, row_id: &str) -> RelResult<Option<Json>> {
        let gkey = Self::gkey(collection, row_id);
        match self.store.index().get_bytes(&gkey).map_err(map_err)? {
            Some(bytes) => Self::unwrap_doc(&bytes),
            None => self.fetch_wrapped(&gkey).await,
        }
    }

    async fn put(&self, collection: &str, row_id: &str, doc: &Json) -> RelResult<()> {
        self.write_wrapped(Self::gkey(collection, row_id), collection, doc)
            .await
    }

    async fn delete(&self, collection: &str, row_id: &str) -> RelResult<()> {
        // Deleting a missing row is not an error.
        let _ = self.store.delete(&Self::gkey(collection, row_id)).await;
        Ok(())
    }

    async fn truncate(&self, collection: &str) -> RelResult<()> {
        let prefix = format!("{collection}{SEP}");
        let keys = self.store.index().keys().map_err(map_err)?;
        for key in keys {
            if key.starts_with(&prefix) {
                let _ = self.store.delete(&key).await;
            }
        }
        Ok(())
    }

    async fn load_catalog(&self) -> RelResult<Option<Json>> {
        self.get(CATALOG_COLLECTION, "catalog").await
    }

    async fn save_catalog(&self, catalog: &Json) -> RelResult<()> {
        self.put(CATALOG_COLLECTION, "catalog", catalog).await
    }
}

/// Open (or create) a relational SQL database backed by a GuardianDB document
/// store named `name`. The returned [`Database`] can be used to create
/// [`Session`](crate::sql::engine::Session)s or served over the wire with
/// `crate::pgwire::serve` (requires the `pgwire` feature).
pub async fn open_sql(
    db: &GuardianDB,
    name: &str,
) -> GuardianResult<Arc<Database<GuardianRelationalStorage>>> {
    open_sql_with(db, name, Consistency::LocalFirst).await
}

/// Like [`open_sql`] but selects a [`Consistency`] mode.
pub async fn open_sql_with(
    db: &GuardianDB,
    name: &str,
    consistency: Consistency,
) -> GuardianResult<Arc<Database<GuardianRelationalStorage>>> {
    let docs = db.docs(name, None).await?;
    let storage = Arc::new(GuardianRelationalStorage::new(docs).with_consistency(consistency));
    Ok(Arc::new(Database::new(storage, name.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::GuardianDB;
    use crate::guardian::core::NewGuardianDBOptions;
    use crate::p2p::network::client::IrohClient;
    use crate::p2p::network::config::ClientConfig;
    use crate::sql::ExecResult;
    use crate::sql::engine::Session;
    use tempfile::TempDir;

    async fn node() -> (GuardianDB, TempDir) {
        let temp = TempDir::new().unwrap();
        let mut cfg = ClientConfig::testing();
        cfg.data_store_path = Some(temp.path().join("iroh"));
        cfg.port = 0;
        let iroh = IrohClient::new(cfg).await.unwrap();
        let opts = NewGuardianDBOptions {
            directory: Some(temp.path().join("guardian")),
            backend: Some(iroh.backend().clone()),
            ..Default::default()
        };
        let db = GuardianDB::new(iroh.clone(), Some(opts)).await.unwrap();
        (db, temp)
    }

    fn rows(r: &ExecResult) -> &Vec<Vec<crate::sql::SqlValue>> {
        match r {
            ExecResult::Rows { rows, .. } => rows,
            ExecResult::Command { tag } => panic!("expected rows, got {tag}"),
        }
    }

    #[tokio::test]
    async fn sql_over_guardiandb_document_store() {
        let (db, _tmp) = node().await;
        let database = open_sql(&db, "app").await.unwrap();
        let mut s = Session::new(database, "guardian");

        s.execute(
            "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE NOT NULL, data JSONB)",
        )
        .await
        .unwrap();
        s.execute("INSERT INTO users (id, email, data) VALUES (1, 'a@x.com', '{\"plan\":\"pro\"}'), (2, 'b@x.com', '{}')")
            .await
            .unwrap();

        let mut r = s
            .execute("SELECT id, email FROM users ORDER BY id")
            .await
            .unwrap();
        let r = r.pop().unwrap();
        assert_eq!(rows(&r).len(), 2);
        assert_eq!(rows(&r)[0][1].to_text().unwrap(), "a@x.com");

        // Unique enforcement works over the document store.
        let err = s
            .execute("INSERT INTO users VALUES (3, 'a@x.com', '{}')")
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate(), "23505");

        // Update + delete persist to the document store.
        s.execute("UPDATE users SET email = 'a2@x.com' WHERE id = 1")
            .await
            .unwrap();
        s.execute("DELETE FROM users WHERE id = 2").await.unwrap();
        let mut r = s.execute("SELECT count(*) FROM users").await.unwrap();
        assert_eq!(rows(&r.pop().unwrap())[0][0].to_text().unwrap(), "1");
    }

    #[tokio::test]
    async fn catalog_and_data_persist_across_reopen() {
        let temp = TempDir::new().unwrap();
        let mut cfg = ClientConfig::testing();
        cfg.data_store_path = Some(temp.path().join("iroh"));
        cfg.port = 0;
        let iroh = IrohClient::new(cfg).await.unwrap();
        let guardian_dir = temp.path().join("guardian");
        let make_opts = || NewGuardianDBOptions {
            directory: Some(guardian_dir.clone()),
            backend: Some(iroh.backend().clone()),
            ..Default::default()
        };

        // First backend: create schema + data.
        {
            let db = GuardianDB::new(iroh.clone(), Some(make_opts()))
                .await
                .unwrap();
            let database = open_sql(&db, "app").await.unwrap();
            let mut s = Session::new(database, "guardian");
            s.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
                .await
                .unwrap();
            s.execute("INSERT INTO t VALUES (1, 'persisted')")
                .await
                .unwrap();
        }

        // Second backend opening the same document store sees the same data via
        // a fresh relational view (catalog + rows reload from storage).
        {
            let db = GuardianDB::new(iroh.clone(), Some(make_opts()))
                .await
                .unwrap();
            let database = open_sql(&db, "app").await.unwrap();
            let mut s = Session::new(database, "guardian");
            let mut r = s.execute("SELECT v FROM t WHERE id = 1").await.unwrap();
            assert_eq!(
                rows(&r.pop().unwrap())[0][0].to_text().unwrap(),
                "persisted"
            );
        }
    }
}
