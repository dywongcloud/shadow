//! # Administration RPC (feature = `sentinel`)
//!
//! A loopback socket server that fronts a **live** [`GuardianDB`], so tools such as
//! the TUI panel can inspect a running instance without opening its storage
//! directly — which the redb file lock forbids for a second process (see
//! `docs/ADMIN_RPC_PLAN.md`).
//!
//! The architecture mirrors the `pgwire` gateway: a single owner process runs
//! [`serve`], and any number of clients attach over a socket.
//!
//! ```text
//! owner process ── AdminContext ── EmbeddedSource ─┐
//!                                                  ├── serve() ── socket ── AdminClient
//!                                        (dispatch) ┘                        (RpcSource)
//! ```
//!
//! This module is the **R0 slice**: the [`AdminSource`] seam, the [`EmbeddedSource`]
//! backend, the request/response protocol, the [`serve`] loop, and the
//! [`AdminClient`] — proven end-to-end by `stores.list` and `node.info`. Streaming
//! (`events.subscribe`) and action ops are reserved for later phases.

mod client;
mod server;
mod store_registry;

pub use client::AdminClient;
pub use server::{serve, serve_on};
pub use store_registry::{StoreRegistry, StoreSpec};

use crate::guardian::GuardianDB;
use crate::p2p::network::client::IrohClient;
use crate::stores::event_log_store::GuardianDBEventLogStore;
use crate::stores::kv_store::GuardianDBKeyValue;
use crate::traits::{CreateDBOptions, StreamOptions};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

/// Default loopback bind address for the admin RPC (one past the pgwire gateway).
pub const DEFAULT_ADDR: &str = "127.0.0.1:15433";

/// Open the `GuardianDB` + `IrohClient` for a `data-dir` as the **owning process**
/// (the process that holds the redb lock). Shared by the `guardian-sentinel-server`
/// binary and the panel's embedded mode so their storage/discovery setup can't
/// drift. `iroh/` and `db/` subpaths are consistent across both.
pub async fn open_owned(
    data_dir: &std::path::Path,
) -> crate::guardian::error::Result<(Arc<GuardianDB>, IrohClient)> {
    use crate::guardian::core::NewGuardianDBOptions;
    use crate::p2p::network::config::ClientConfig;

    let config = ClientConfig {
        enable_pubsub: true,
        enable_discovery_mdns: true,
        enable_discovery_n0: true,
        data_store_path: Some(data_dir.join("iroh")),
        ..Default::default()
    };
    let client = IrohClient::new(config).await?;

    let options = NewGuardianDBOptions {
        directory: Some(data_dir.join("db")),
        backend: Some(client.backend().clone()),
        ..Default::default()
    };
    let db = Arc::new(GuardianDB::new(client.clone(), Some(options)).await?);
    Ok((db, client))
}

// ---------------------------------------------------------------------------
// Wire types (shared by server, client, and the panel)
// ---------------------------------------------------------------------------

/// One store as reported by `stores.list`. Mirrors what the TUI dashboard builds
/// from `db.list_stores()`, minus the render-only fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreSummary {
    pub address: String,
    pub store_type: String,
    pub db_name: String,
    pub entry_count: usize,
}

/// Options for `stores.create` (G1). All default to sensible values so the wizard
/// only has to override what the user changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreCreateOpts {
    /// Replicate over the network (default `true`).
    #[serde(default = "default_true")]
    pub replicate: bool,
    /// Keep strictly local (default `false`).
    #[serde(default)]
    pub local_only: bool,
    /// Open as a read-only replica (default `false`).
    #[serde(default)]
    pub read_only: bool,
    /// Address of an access controller to attach, if any.
    #[serde(default)]
    pub acl_address: Option<String>,
}

impl Default for StoreCreateOpts {
    fn default() -> Self {
        Self {
            replicate: true,
            local_only: false,
            read_only: false,
            acl_address: None,
        }
    }
}

/// Node/process overview as reported by `node.info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSummary {
    pub node_id: String,
    pub uptime_s: u64,
    pub stores: usize,
}

/// This node's shareable identity, as reported by `node.identity` (G3.1): the
/// `node_id` a peer needs to connect, plus its currently-bound addresses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: String,
    pub addresses: Vec<String>,
}

/// The `DocTicket`s that let a peer replicate a store, as reported by
/// `stores.share` (G3.2). The `read` ticket grants read-only access (no write
/// secret); the `write` ticket carries the namespace secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreTickets {
    pub read: String,
    pub write: String,
}

/// One key/value pair of a KeyValue store, as reported by `kv.entries`. The value
/// is rendered lossily as UTF-8 for display; `size` is the true byte length.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KvEntry {
    pub key: String,
    pub value_utf8: String,
    pub size: usize,
}

/// One document of a Document store, as reported by `docs.list` / `docs.get`.
/// `value_utf8` is the stored JSON (raw for `docs.list`, pretty-printed for
/// `docs.get`); `size` is the true byte length. Document stores had no inspector
/// op before (B4) — they only surfaced as `StoreDetail` metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocEntry {
    pub id: String,
    pub value_utf8: String,
    pub size: usize,
}

/// One entry of an EventLog store, as reported by `eventlog.entries`. Carries the
/// CRDT log-entry metadata (hash, clock, identity, next pointers) for the detail
/// view — populated from the operation's attached `Entry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub index: usize,
    pub op: String,
    pub key: Option<String>,
    pub value_utf8: String,
    pub size: usize,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub log_id: String,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub clock_id: String,
    #[serde(default)]
    pub clock_time: u64,
    #[serde(default)]
    pub next: Vec<String>,
}

/// One CRDT head (a current tip of the log DAG), as reported by `eventlog.heads`.
/// Multiple heads mean the log has diverged and a merge is pending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrdtHead {
    pub hash: String,
    pub clock_id: String,
    pub clock_time: u64,
    pub identity: Option<String>,
    pub next: Vec<String>,
}

/// One known peer, as reported by `peers.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerSummary {
    pub node_id: String,
    pub addresses: Vec<String>,
    pub connected: bool,
}

/// One active connection edge from this node, as reported by `net.topology`.
/// `link_kind` is inferred from the address (`direct` / `relay` / `unknown`);
/// `conn_type` is the **real** type from iroh's `remote_info` active address
/// (C1), when the endpoint knows the peer — otherwise `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopoLink {
    pub node_id: String,
    pub address: String,
    pub latency_ms: f64,
    pub ops: u64,
    pub connected_secs: u64,
    pub link_kind: String,
    #[serde(default)]
    pub conn_type: Option<String>,
    /// Per-peer p95 latency (ms) from this peer's sample history (C1), when enough
    /// samples exist — otherwise `None` (the global `node.latency` still applies).
    #[serde(default)]
    pub p95_ms: Option<f64>,
    /// Per-peer p99 latency (ms) (C1).
    #[serde(default)]
    pub p99_ms: Option<f64>,
}

/// One home-relay's connection status, as reported by `net.relay` (C2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayStatus {
    pub url: String,
    pub connected: bool,
    pub last_error: Option<String>,
}

/// Global latency percentiles (node-wide, over the perf history), as reported by
/// `node.latency` (C1). Not per-peer — that would need per-peer sampling (D-tier).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LatencyStats {
    pub p95_ms: f64,
    pub p99_ms: f64,
}

/// Aggregate (node-wide) throughput, as reported by `node.throughput` (D1).
/// Bytes/ops are aggregated, not per-sync/per-peer (per-peer accounting is D1-full).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThroughputStats {
    pub ops_per_second: f64,
    pub bytes_per_second: u64,
    pub peak_throughput: f64,
    pub avg_throughput: f64,
}

/// One stored blob, as reported by `blobs.list`. `size` is the real byte size and
/// `complete` distinguishes fully-stored blobs from partial downloads — both
/// resolved via `blobs().status(hash)` (C4/C5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobSummary {
    pub hash: String,
    pub size: u64,
    #[serde(default = "default_true")]
    pub complete: bool,
}

/// Serde default for `BlobSummary::complete` — a blob missing the field (older
/// peer) is assumed complete rather than silently shown as partial.
fn default_true() -> bool {
    true
}

/// Content/metadata of a blob, as reported by `blob.get` (preview is the first
/// bytes, lossily as UTF-8; `size` is the true byte length).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobContent {
    pub size: u64,
    pub is_text: bool,
    pub preview: String,
}

/// A live event pushed by an `events.subscribe` stream. `kind` is a stable slug
/// (`sync`, `peer_connected`, `store_updated`, …); `detail` is a short human line.
///
/// The structured fields below carry the typed data the core events already emit
/// (store address, peer/node id, sync duration, heads synced, timestamp) that the
/// old flat `{kind, detail}` shape discarded — this is the lever that unlocks
/// peers-per-store / stores-per-peer aggregation and rich sync history (B1). All
/// are `#[serde(default)]` so older/newer peers stay wire-compatible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AdminEvent {
    pub kind: String,
    pub detail: String,
    /// Store address this event pertains to, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// Peer/node id this event involves, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Heads synced (for `sync_completed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heads_synced: Option<usize>,
    /// Sync duration in milliseconds (for `sync_completed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// RFC-3339 wall-clock timestamp emitted by the core event, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
}

impl AdminEvent {
    /// Construct an event carrying only the slug + human line (structured fields
    /// left empty). Structured forwarders fill the rest via struct-update syntax.
    fn new(kind: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            detail: detail.into(),
            ..Default::default()
        }
    }
}

/// Metadata of a keystore entry, as reported by `keystore.detail`. Only the
/// **public** key is ever exposed — never the secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyInfo {
    pub key_id: String,
    pub public_key: Option<String>,
    /// Key kind, when the keystore records metadata (D2). `None` = untracked.
    #[serde(default)]
    pub kind: Option<String>,
    /// Unix seconds of first generation (D2).
    #[serde(default)]
    pub created_at: Option<u64>,
    /// How many times the key was regenerated in place; `> 0` = rotated (D2).
    #[serde(default)]
    pub rotated_count: Option<u32>,
}

/// The authorized keys for one role of an access controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AclRole {
    pub role: String,
    pub keys: Vec<String>,
}

/// One store's access controller, as reported by `acl.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AclSummary {
    pub store: String,
    pub controller_type: String,
    pub roles: Vec<AclRole>,
}

/// A structured, serializable error carried over the wire and surfaced by the
/// client. `code` is a stable machine-readable slug; `message` is human text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminError {
    pub code: String,
    pub message: String,
}

impl AdminError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AdminError {}

/// Result type used across the admin surface.
pub type AdminResult<T> = std::result::Result<T, AdminError>;

// ---------------------------------------------------------------------------
// Protocol envelope (JSON, one message per line)
// ---------------------------------------------------------------------------

/// A request from a client. `args` is op-specific; absent for no-arg ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminRequest {
    pub id: u64,
    pub op: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// A reply from the server. The variants are distinguished by which payload field
/// is present (`data` / `error` / `event` / `end`), so the untagged representation
/// stays unambiguous. `ok` is redundant but kept for wire clarity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AdminReply {
    Ok {
        id: u64,
        ok: bool,
        data: serde_json::Value,
    },
    Err {
        id: u64,
        ok: bool,
        error: AdminError,
    },
    /// One item of a streaming subscription (many share the request `id`).
    Event { id: u64, event: AdminEvent },
    /// Terminates a streaming subscription.
    End { id: u64, end: bool },
}

impl AdminReply {
    pub(crate) fn ok(id: u64, data: serde_json::Value) -> Self {
        AdminReply::Ok { id, ok: true, data }
    }

    pub(crate) fn err(id: u64, error: AdminError) -> Self {
        AdminReply::Err {
            id,
            ok: false,
            error,
        }
    }

    pub(crate) fn event(id: u64, event: AdminEvent) -> Self {
        AdminReply::Event { id, event }
    }

    pub(crate) fn end(id: u64) -> Self {
        AdminReply::End { id, end: true }
    }

    /// The request id this reply correlates to (used by the client to demux).
    pub(crate) fn id(&self) -> u64 {
        match self {
            AdminReply::Ok { id, .. }
            | AdminReply::Err { id, .. }
            | AdminReply::Event { id, .. }
            | AdminReply::End { id, .. } => *id,
        }
    }
}

// ---------------------------------------------------------------------------
// The seam: AdminSource
// ---------------------------------------------------------------------------

/// The data-access seam shared by both backends. The server dispatches ops to it;
/// the panel will consume it. [`EmbeddedSource`] talks to a local `GuardianDB`;
/// [`AdminClient`] implements the same trait over a socket, so features written
/// against `AdminSource` work in both modes unchanged.
#[async_trait]
pub trait AdminSource: Send + Sync {
    async fn stores_list(&self) -> AdminResult<Vec<StoreSummary>>;
    async fn node_info(&self) -> AdminResult<NodeSummary>;

    /// Create a store of `kind` (`eventlog`/`keyvalue`/`document`) named `name`
    /// with `opts`, persist it in the registry so it reopens on restart (G1), and
    /// return its address. Errors if a store with that name is already open.
    async fn stores_create(
        &self,
        kind: &str,
        name: &str,
        opts: StoreCreateOpts,
    ) -> AdminResult<String>;

    /// This node's shareable identity (node id + addresses) (G3.1).
    async fn node_identity(&self) -> AdminResult<NodeIdentity>;

    /// Generate read/write `DocTicket`s for an iroh-docs store (KeyValue/Document)
    /// so a peer can replicate it (G3.2). Errors for EventLog stores.
    async fn stores_share(&self, name: &str) -> AdminResult<StoreTickets>;

    /// Import a shared store from a `DocTicket` (G3.3): opens a `kind` store named
    /// `name` that joins the ticket's namespace, persists it in the registry, and
    /// returns its address. `read_only` opens it as a read replica.
    async fn stores_import(
        &self,
        kind: &str,
        name: &str,
        ticket: &str,
        read_only: bool,
    ) -> AdminResult<String>;

    /// Close a store: release its live handle without deleting data (G2.1). It
    /// stays in the registry, so it reopens on the next restart.
    async fn stores_close(&self, name: &str) -> AdminResult<()>;

    /// Drop a store: delete its local data **and** remove it from the registry
    /// (G2.2). Destructive and irreversible for local content.
    async fn stores_drop(&self, name: &str) -> AdminResult<()>;

    /// Append an entry to an EventLog store (G2.5), returning the new entry hash.
    async fn eventlog_append(&self, store: &str, data: &str) -> AdminResult<String>;

    /// Put a document into a Document store (G2.6). `json` is the document body;
    /// `id` is written as its `_id`. Returns the document id.
    async fn docs_put(&self, store: &str, id: &str, json: &str) -> AdminResult<String>;

    /// Delete a document by id from a Document store (G2.6).
    async fn docs_delete(&self, store: &str, id: &str) -> AdminResult<()>;

    /// All key/value pairs of the named KeyValue store, sorted by key.
    async fn kv_entries(&self, store: &str) -> AdminResult<Vec<KvEntry>>;

    /// Entries of the named EventLog store, newest-bounded by `limit` if given.
    /// When `before` is a valid entry hash, returns the block of entries that
    /// precede it (older history) — the cursor used by the panel to lazily page
    /// backwards through large logs (recurso 2.1). `None` returns the newest block.
    async fn eventlog_entries(
        &self,
        store: &str,
        limit: Option<usize>,
        before: Option<&str>,
    ) -> AdminResult<Vec<LogEntry>>;

    /// Current CRDT heads (DAG tips) of the named EventLog store. Length > 1 means
    /// the log has diverged (a merge is pending).
    async fn eventlog_heads(&self, store: &str) -> AdminResult<Vec<CrdtHead>>;

    /// All documents of the named Document store, sorted by id (B4).
    async fn docs_list(&self, store: &str) -> AdminResult<Vec<DocEntry>>;

    /// One document by id from the named Document store (value pretty-printed).
    async fn docs_get(&self, store: &str, id: &str) -> AdminResult<DocEntry>;

    /// Known peers (from the connection pool / endpoint).
    async fn peers_list(&self) -> AdminResult<Vec<PeerSummary>>;

    /// Active connection edges from this node (for the topology view), with
    /// per-link latency, inferred link kind, and real conn-type (C1).
    async fn net_topology(&self) -> AdminResult<Vec<TopoLink>>;

    /// Home-relay connection status (C2). Empty when no relay is configured.
    async fn net_relay(&self) -> AdminResult<Vec<RelayStatus>>;

    /// Global (node-wide) latency percentiles p95/p99 (C1).
    async fn node_latency(&self) -> AdminResult<LatencyStats>;

    /// Aggregate (node-wide) throughput metrics (D1).
    async fn node_throughput(&self) -> AdminResult<ThroughputStats>;

    /// Peers we have learned about but are not currently connected to (C3). Node
    /// ids as strings — an observed view, not the full discovery table.
    async fn net_discovered(&self) -> AdminResult<Vec<String>>;

    /// Stored blobs (tagged documents).
    async fn blobs_list(&self) -> AdminResult<Vec<BlobSummary>>;

    /// Content + metadata of one blob (real size + text preview).
    async fn blob_get(&self, hash: &str) -> AdminResult<BlobContent>;

    /// Add a blob from a file on the owner process, returning its hash.
    async fn blob_add(&self, path: &str) -> AdminResult<String>;

    /// Export a blob to a file on the owner process, returning bytes written.
    async fn blob_export(&self, hash: &str, path: &str) -> AdminResult<u64>;

    /// Delete a blob from local storage.
    async fn blob_delete(&self, hash: &str) -> AdminResult<()>;

    /// A live stream of normalized events. In embedded mode this wraps the local
    /// `EventBus`; over RPC it is fed by the client's demux task. The panel drives
    /// reactive refresh from this in both modes.
    async fn events_subscribe(&self) -> AdminResult<BoxStream<'static, AdminEvent>>;

    // --- action ops (R3): mutate state; gated by token auth on the RPC path ---

    /// Set a key in a KeyValue store (replicates through Iroh like any write).
    async fn kv_put(&self, store: &str, key: &str, value: Vec<u8>) -> AdminResult<()>;

    /// Delete a key from a KeyValue store.
    async fn kv_delete(&self, store: &str, key: &str) -> AdminResult<()>;

    /// Force a connection/sync with a peer given its NodeId (hex/z-base32 string).
    async fn peer_sync(&self, node_id: &str) -> AdminResult<()>;

    /// Stored key identifiers (metadata only — never key material).
    async fn keystore_list(&self) -> AdminResult<Vec<String>>;

    /// Metadata of one keystore entry (its derived public key; never the secret).
    async fn keystore_detail(&self, key_id: &str) -> AdminResult<KeyInfo>;

    /// Generate a fresh keypair stored under `key_id` (overwrites if it exists),
    /// returning only the new public key. Used for both "generate" and "rotate".
    async fn keystore_generate(&self, key_id: &str) -> AdminResult<String>;

    /// Access controllers of all open stores, with authorized keys per role.
    async fn acl_list(&self) -> AdminResult<Vec<AclSummary>>;

    /// Grant a role/capability to a key on a store's access controller.
    async fn acl_grant(&self, store: &str, role: &str, key_id: &str) -> AdminResult<()>;

    /// Revoke a role/capability from a key on a store's access controller.
    async fn acl_revoke(&self, store: &str, role: &str, key_id: &str) -> AdminResult<()>;

    /// Create and persist a new access-controller manifest, returning its shareable
    /// manifest hash (a BLAKE3 address).
    async fn acl_create(
        &self,
        controller_type: &str,
        name: &str,
        admin_keys: Vec<String>,
        write_keys: Vec<String>,
    ) -> AdminResult<String>;
}

// ---------------------------------------------------------------------------
// AdminContext + EmbeddedSource (the owner-process backend)
// ---------------------------------------------------------------------------

/// Shared handle to the live resources of the process that owns the storage.
/// Cloning is cheap (`Arc` inside); one is created per server.
#[derive(Clone)]
pub struct AdminContext {
    db: Arc<GuardianDB>,
    client: IrohClient,
    node_id: String,
    started_at: Instant,
    /// Persistent catalog of admin-created stores (G1). `None` for in-memory
    /// contexts (dev/tests) — creation still works, it just isn't persisted.
    store_registry: Option<Arc<StoreRegistry>>,
}

impl AdminContext {
    /// Build a context whose admin-managed keystore is **in-memory** (dev/tests):
    /// generated/rotated keys do not survive a restart, and created stores are not
    /// recorded for reopening.
    pub fn new(db: Arc<GuardianDB>, client: IrohClient) -> Self {
        Self::build(db, client, None, None)
    }

    /// Build a context whose admin-managed keystore is a **persistent**
    /// `RedbKeystore` at `keystore_path` (falling back to in-memory if it can't be
    /// opened). Prefer [`AdminContext::with_data_dir`] for the full owner setup.
    pub fn with_keystore(
        db: Arc<GuardianDB>,
        client: IrohClient,
        keystore_path: std::path::PathBuf,
    ) -> Self {
        Self::build(db, client, Some(keystore_path), None)
    }

    /// Full owner setup rooted at `data_dir`: a persistent keystore at
    /// `<data_dir>/admin_keystore` **and** a persistent store registry at
    /// `<data_dir>/store_registry` (G1), so stores created via the TUI reopen on
    /// restart. Call [`AdminContext::reopen_stores`] once after this to reopen them.
    pub fn with_data_dir(
        db: Arc<GuardianDB>,
        client: IrohClient,
        data_dir: std::path::PathBuf,
    ) -> Self {
        Self::build(
            db,
            client,
            Some(data_dir.join("admin_keystore")),
            Some(data_dir.join("store_registry")),
        )
    }

    fn build(
        db: Arc<GuardianDB>,
        client: IrohClient,
        keystore_path: Option<std::path::PathBuf>,
        registry_path: Option<std::path::PathBuf>,
    ) -> Self {
        let node_id = db.base().node_id().to_string();
        // The facade `GuardianDB` path does not wire a keystore (the node identity
        // lives in identity.json). Ensure one exists so the admin key ops work —
        // persistent (redb) when a path is given, otherwise in-memory.
        {
            let ks = db.base().keystore();
            let mut guard = ks.write();
            if guard.is_none() {
                let store: Box<dyn crate::log::identity_provider::Keystore + Send + Sync> =
                    match keystore_path {
                        Some(path) => match crate::keystore::RedbKeystore::new(Some(path)) {
                            Ok(k) => Box::new(k),
                            Err(e) => {
                                tracing::warn!(
                                    "admin: could not open persistent keystore ({e}); \
                                 falling back to in-memory"
                                );
                                Box::new(crate::log::identity_provider::InMemoryKeystore::new())
                            }
                        },
                        None => Box::new(crate::log::identity_provider::InMemoryKeystore::new()),
                    };
                *guard = Some(store);
            }
        }
        // Open the persistent store registry (G1) when a path is given; a failure
        // is non-fatal (creation still works, just without reopen-on-restart).
        let store_registry = registry_path.and_then(|p| match StoreRegistry::open(p) {
            Ok(r) => Some(Arc::new(r)),
            Err(e) => {
                tracing::warn!("admin: could not open store registry ({e}); stores won't persist across restart");
                None
            }
        });
        Self {
            db,
            client,
            node_id,
            started_at: Instant::now(),
            store_registry,
        }
    }

    /// Specs of all stores recorded in the persistent registry (G1). Empty when no
    /// registry is configured. Reads the already-open registry handle (no new lock).
    pub fn registered_stores(&self) -> Vec<(String, StoreSpec)> {
        self.store_registry
            .as_ref()
            .and_then(|r| r.list().ok())
            .unwrap_or_default()
    }

    /// Reopen every store recorded in the registry (G1). Called once by the owner
    /// process at startup so admin-created stores repopulate `db.list_stores()`.
    /// Best-effort and per-store fault-tolerant: a store that fails to reopen is
    /// logged and skipped. Returns how many reopened successfully.
    pub async fn reopen_stores(&self) -> usize {
        let Some(reg) = &self.store_registry else {
            return 0;
        };
        let specs = match reg.list() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("admin: could not read store registry: {e}");
                return 0;
            }
        };
        let mut reopened = 0;
        for (name, spec) in specs {
            match self.open_store_from_spec(&name, &spec).await {
                Ok(()) => reopened += 1,
                Err(e) => tracing::warn!("admin: failed to reopen store '{name}': {e}"),
            }
        }
        if reopened > 0 {
            tracing::info!("admin: reopened {reopened} store(s) from the registry");
        }
        reopened
    }

    /// Open a store from its [`StoreSpec`] via the right `GuardianDB` constructor.
    /// Idempotent: reopening an existing store returns the open handle.
    async fn open_store_from_spec(&self, name: &str, spec: &StoreSpec) -> AdminResult<()> {
        let opts = CreateDBOptions {
            create: Some(true),
            replicate: Some(spec.replicate),
            local_only: Some(spec.local_only),
            read_only: Some(spec.read_only),
            access_controller_address: spec.acl_address.clone(),
            doc_ticket: spec.doc_ticket.clone(),
            ..Default::default()
        };
        match spec.kind.as_str() {
            "eventlog" => self.db.log(name, Some(opts)).await.map(|_| ()),
            "keyvalue" => self.db.key_value(name, Some(opts)).await.map(|_| ()),
            "document" => self.db.docs(name, Some(opts)).await.map(|_| ()),
            other => {
                return Err(AdminError::new(
                    "bad_kind",
                    format!("unknown store kind '{other}'"),
                ));
            }
        }
        .map_err(db_err)
    }
}

/// [`AdminSource`] backed by a local `GuardianDB` — used by the owner process
/// (the `guardian-sentinel-server` binary, or an embedded server in an app).
pub struct EmbeddedSource {
    ctx: AdminContext,
}

impl EmbeddedSource {
    pub fn new(ctx: AdminContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl AdminSource for EmbeddedSource {
    async fn stores_list(&self) -> AdminResult<Vec<StoreSummary>> {
        let summaries = self
            .ctx
            .db
            .list_stores()
            .into_iter()
            .map(|(address, store)| {
                let entry_count = store.index().len().unwrap_or(0);
                StoreSummary {
                    address,
                    store_type: store.store_type().to_string(),
                    db_name: store.db_name().to_string(),
                    entry_count,
                }
            })
            .collect();
        Ok(summaries)
    }

    async fn node_info(&self) -> AdminResult<NodeSummary> {
        Ok(NodeSummary {
            node_id: self.ctx.node_id.clone(),
            uptime_s: self.ctx.started_at.elapsed().as_secs(),
            stores: self.ctx.db.list_stores().len(),
        })
    }

    async fn stores_create(
        &self,
        kind: &str,
        name: &str,
        opts: StoreCreateOpts,
    ) -> AdminResult<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AdminError::new("bad_name", "store name must not be empty"));
        }
        if !matches!(kind, "eventlog" | "keyvalue" | "document") {
            return Err(AdminError::new(
                "bad_kind",
                format!("unknown store kind '{kind}' (use eventlog/keyvalue/document)"),
            ));
        }
        // Refuse if a store with this name is already open, so "create" never
        // silently returns an existing, differently-configured store.
        if self.find_store(name).is_ok() {
            return Err(AdminError::new(
                "already_exists",
                format!("a store named '{name}' is already open"),
            ));
        }
        let spec = StoreSpec {
            kind: kind.to_string(),
            replicate: opts.replicate,
            local_only: opts.local_only,
            read_only: opts.read_only,
            acl_address: opts.acl_address.clone(),
            doc_ticket: None,
        };
        self.open_and_register(name, spec).await
    }

    async fn node_identity(&self) -> AdminResult<NodeIdentity> {
        let info = self.ctx.client.id().await.map_err(db_err)?;
        Ok(NodeIdentity {
            node_id: info.id.to_string(),
            addresses: info.addresses,
        })
    }

    async fn stores_share(&self, name: &str) -> AdminResult<StoreTickets> {
        let handle = self.find_store(name)?;
        // Only iroh-docs-based stores (KeyValue/Document) can hand out tickets.
        let (read, write) = match handle.store_type() {
            "keyvalue" => as_kv(&handle, name)?
                .share_tickets()
                .await
                .map_err(db_err)?,
            "document" => as_document(&handle, name)?
                .share_tickets()
                .await
                .map_err(db_err)?,
            other => {
                return Err(AdminError::new(
                    "not_shareable",
                    format!(
                        "store '{name}' ({other}) is not iroh-docs-based; only keyvalue/document can be shared"
                    ),
                ));
            }
        };
        Ok(StoreTickets { read, write })
    }

    async fn stores_import(
        &self,
        kind: &str,
        name: &str,
        ticket: &str,
        read_only: bool,
    ) -> AdminResult<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AdminError::new("bad_name", "store name must not be empty"));
        }
        if ticket.trim().is_empty() {
            return Err(AdminError::new("bad_ticket", "ticket must not be empty"));
        }
        // Tickets are iroh-docs namespaces → only keyvalue/document can import them.
        if !matches!(kind, "keyvalue" | "document") {
            return Err(AdminError::new(
                "bad_kind",
                format!("only keyvalue/document stores can import a ticket, not '{kind}'"),
            ));
        }
        if self.find_store(name).is_ok() {
            return Err(AdminError::new(
                "already_exists",
                format!("a store named '{name}' is already open"),
            ));
        }
        let spec = StoreSpec {
            kind: kind.to_string(),
            replicate: true,
            local_only: false,
            read_only,
            acl_address: None,
            doc_ticket: Some(ticket.trim().to_string()),
        };
        self.open_and_register(name, spec).await
    }

    async fn stores_close(&self, name: &str) -> AdminResult<()> {
        let (address, handle) = self.find_store_entry(name)?;
        crate::traits::Store::close(handle.as_ref())
            .await
            .map_err(db_err)?;
        self.ctx.db.base().delete_store(&address);
        Ok(())
    }

    async fn stores_drop(&self, name: &str) -> AdminResult<()> {
        let (address, handle) = self.find_store_entry(name)?;
        let db_name = handle.db_name().to_string();
        // Fully-qualified: `handle.drop()` would resolve to the `Drop` destructor.
        crate::traits::Store::drop(handle.as_ref())
            .await
            .map_err(db_err)?;
        self.ctx.db.base().delete_store(&address);
        // Also forget it in the registry so it does not reopen on restart.
        if let Some(reg) = &self.ctx.store_registry
            && let Err(e) = reg.remove(&db_name)
        {
            tracing::warn!("admin: store '{db_name}' dropped but registry not updated: {e}");
        }
        Ok(())
    }

    async fn eventlog_append(&self, store: &str, data: &str) -> AdminResult<String> {
        let handle = self.find_store(store)?;
        let log = as_log(&handle, store)?;
        let op = log.add(data.as_bytes().to_vec()).await.map_err(db_err)?;
        Ok(op.entry().map(|e| e.hash().to_string()).unwrap_or_default())
    }

    async fn docs_put(&self, store: &str, id: &str, json: &str) -> AdminResult<String> {
        let handle = self.find_store(store)?;
        let doc = as_document(&handle, store)?;
        let mut value: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| AdminError::new("bad_json", format!("invalid JSON: {e}")))?;
        // Force `_id` so the default key extractor keys the document by `id`.
        match value.as_object_mut() {
            Some(obj) => {
                obj.insert("_id".to_string(), serde_json::Value::String(id.to_string()));
            }
            None => {
                return Err(AdminError::new(
                    "bad_json",
                    "document must be a JSON object",
                ));
            }
        }
        doc.put_impl(value).await.map_err(db_err)?;
        Ok(id.to_string())
    }

    async fn docs_delete(&self, store: &str, id: &str) -> AdminResult<()> {
        let handle = self.find_store(store)?;
        as_document(&handle, store)?
            .delete_impl(id)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn kv_entries(&self, store: &str) -> AdminResult<Vec<KvEntry>> {
        // Read the ALREADY-OPEN store from the managed map — never reopen, since
        // redb holds an exclusive lock and reopening an open store fails.
        let handle = self.find_store(store)?;
        let kv = as_kv(&handle, store)?;
        let mut entries: Vec<KvEntry> = kv
            .all()
            .into_iter()
            .map(|(key, value)| KvEntry {
                key,
                value_utf8: String::from_utf8_lossy(&value).into_owned(),
                size: value.len(),
            })
            .collect();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }

    async fn eventlog_entries(
        &self,
        store: &str,
        limit: Option<usize>,
        before: Option<&str>,
    ) -> AdminResult<Vec<LogEntry>> {
        let handle = self.find_store(store)?;
        let log = as_log(&handle, store)?;
        // A `before` cursor becomes an exclusive upper bound (`lt`) so the query
        // returns the block of entries just older than that hash. `parse_blob_hash`
        // guards the length so a malformed cursor can't panic (see its docs).
        let lt = match before {
            Some(h) => Some(parse_blob_hash(h)?),
            None => None,
        };
        let opts = (limit.is_some() || lt.is_some()).then(|| StreamOptions {
            // Clamp to i32::MAX so a large usize limit can't wrap to a negative
            // amount.
            amount: limit.map(|n| n.min(i32::MAX as usize) as i32),
            lt,
            ..Default::default()
        });
        let ops = log.list(opts).await.map_err(db_err)?;
        Ok(ops
            .into_iter()
            .enumerate()
            .map(|(index, op)| {
                let entry = op.entry();
                LogEntry {
                    index,
                    op: op.op().to_string(),
                    key: op.key().cloned(),
                    value_utf8: String::from_utf8_lossy(op.value()).into_owned(),
                    size: op.value().len(),
                    hash: entry.map(|e| e.hash().to_string()).unwrap_or_default(),
                    log_id: entry.map(|e| e.id().to_string()).unwrap_or_default(),
                    identity: entry.and_then(|e| e.identity.as_ref().map(|i| i.id().to_string())),
                    clock_id: entry
                        .map(|e| e.clock().id().to_string())
                        .unwrap_or_default(),
                    clock_time: entry.map(|e| e.clock().time()).unwrap_or(0),
                    next: entry
                        .map(|e| e.next().iter().map(|h| h.to_string()).collect())
                        .unwrap_or_default(),
                }
            })
            .collect())
    }

    async fn eventlog_heads(&self, store: &str) -> AdminResult<Vec<CrdtHead>> {
        let handle = self.find_store(store)?;
        // Extract the heads under the sync parking_lot guard, then map (no await
        // while the guard is held).
        let heads = {
            let oplog = handle.op_log();
            let guard = oplog.read();
            guard.heads()
        };
        Ok(heads
            .into_iter()
            .map(|e| CrdtHead {
                hash: e.hash().to_string(),
                clock_id: e.clock().id().to_string(),
                clock_time: e.clock().time(),
                identity: e.identity.as_ref().map(|i| i.id().to_string()),
                next: e.next().iter().map(|h| h.to_string()).collect(),
            })
            .collect())
    }

    async fn docs_list(&self, store: &str) -> AdminResult<Vec<DocEntry>> {
        let handle = self.find_store(store)?;
        ensure_document(&handle, store)?;
        // The generic store index exposes keys + raw JSON bytes for each document.
        let idx = handle.index();
        let keys = idx.keys().map_err(db_err)?;
        let mut out = Vec::with_capacity(keys.len());
        for id in keys {
            let bytes = idx.get_bytes(&id).map_err(db_err)?.unwrap_or_default();
            out.push(DocEntry {
                value_utf8: String::from_utf8_lossy(&bytes).into_owned(),
                size: bytes.len(),
                id,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn docs_get(&self, store: &str, id: &str) -> AdminResult<DocEntry> {
        let handle = self.find_store(store)?;
        ensure_document(&handle, store)?;
        let idx = handle.index();
        let bytes = idx
            .get_bytes(id)
            .map_err(db_err)?
            .ok_or_else(|| AdminError::new("not_found", format!("document '{id}' not found")))?;
        // Pretty-print when the stored value parses as JSON; else raw UTF-8.
        let value_utf8 = match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(v) => serde_json::to_string_pretty(&v)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned()),
            Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
        };
        Ok(DocEntry {
            id: id.to_string(),
            value_utf8,
            size: bytes.len(),
        })
    }

    async fn peers_list(&self) -> AdminResult<Vec<PeerSummary>> {
        let peers = self.ctx.client.backend().peers().await.map_err(db_err)?;
        Ok(peers
            .into_iter()
            .map(|p| PeerSummary {
                node_id: p.id.to_string(),
                addresses: p.addresses,
                connected: p.connected,
            })
            .collect())
    }

    async fn net_topology(&self) -> AdminResult<Vec<TopoLink>> {
        let backend = self.ctx.client.backend();
        let conns = backend.list_active_connections().await;
        let mut links = Vec::with_capacity(conns.len());
        for c in conns {
            // Real conn-type from iroh's active transport address (C1); the
            // address-inferred `link_kind` stays as a fallback/comparison.
            let conn_type = backend.conn_type(c.node_id).await;
            // Per-peer latency percentiles from the peer's sample history (C1).
            let (p95_ms, p99_ms) = match backend.peer_latency_percentiles(&c.node_id).await {
                Some((p95, p99)) => (Some(p95), Some(p99)),
                None => (None, None),
            };
            links.push(TopoLink {
                node_id: c.node_id.to_string(),
                link_kind: infer_link_kind(&c.address).to_string(),
                address: c.address,
                latency_ms: c.avg_latency_ms,
                ops: c.operations_count,
                connected_secs: c.connected_at.elapsed().as_secs(),
                conn_type,
                p95_ms,
                p99_ms,
            });
        }
        Ok(links)
    }

    async fn net_relay(&self) -> AdminResult<Vec<RelayStatus>> {
        let relays = self.ctx.client.backend().relay_status().await;
        Ok(relays
            .into_iter()
            .map(|r| RelayStatus {
                url: r.url,
                connected: r.connected,
                last_error: r.last_error,
            })
            .collect())
    }

    async fn node_latency(&self) -> AdminResult<LatencyStats> {
        let (p95_ms, p99_ms) = self
            .ctx
            .client
            .backend()
            .calculate_latency_percentiles()
            .await
            .map_err(db_err)?;
        Ok(LatencyStats { p95_ms, p99_ms })
    }

    async fn node_throughput(&self) -> AdminResult<ThroughputStats> {
        let t = self.ctx.client.backend().get_throughput_metrics().await;
        Ok(ThroughputStats {
            ops_per_second: t.ops_per_second,
            bytes_per_second: t.bytes_per_second,
            peak_throughput: t.peak_throughput,
            avg_throughput: t.avg_throughput,
        })
    }

    async fn net_discovered(&self) -> AdminResult<Vec<String>> {
        let peers = self.ctx.client.backend().discovered_not_connected().await;
        Ok(peers.into_iter().map(|p| p.to_string()).collect())
    }

    async fn blobs_list(&self) -> AdminResult<Vec<BlobSummary>> {
        // Blobs may not be initialized; that just means there is nothing to list.
        let Some(blobs) = self.ctx.client.blobs_client().await else {
            return Ok(Vec::new());
        };
        let docs = blobs.list_documents_status().await.map_err(db_err)?;
        Ok(docs
            .into_iter()
            .map(|b| BlobSummary {
                hash: b.hash.to_string(),
                size: b.size,
                complete: b.complete,
            })
            .collect())
    }

    async fn blob_get(&self, hash: &str) -> AdminResult<BlobContent> {
        let blobs = self.blobs_or_err().await?;
        let h = parse_blob_hash(hash)?;
        let bytes = blobs.get_document(&h).await.map_err(db_err)?;
        Ok(build_blob_content(&bytes))
    }

    async fn blob_add(&self, path: &str) -> AdminResult<String> {
        let data = tokio::fs::read(path)
            .await
            .map_err(|e| AdminError::new("io", e.to_string()))?;
        let blobs = self.blobs_or_err().await?;
        let hash = blobs
            .add_document(bytes::Bytes::from(data))
            .await
            .map_err(db_err)?;
        Ok(hash.to_string())
    }

    async fn blob_export(&self, hash: &str, path: &str) -> AdminResult<u64> {
        let blobs = self.blobs_or_err().await?;
        let h = parse_blob_hash(hash)?;
        let bytes = blobs.get_document(&h).await.map_err(db_err)?;
        let n = bytes.len() as u64;
        tokio::fs::write(path, &bytes)
            .await
            .map_err(|e| AdminError::new("io", e.to_string()))?;
        Ok(n)
    }

    async fn blob_delete(&self, hash: &str) -> AdminResult<()> {
        let blobs = self.blobs_or_err().await?;
        let h = parse_blob_hash(hash)?;
        blobs.delete_document(&h).await.map_err(db_err)?;
        Ok(())
    }

    async fn events_subscribe(&self) -> AdminResult<BoxStream<'static, AdminEvent>> {
        use crate::guardian::core::{
            EventDatabaseCreated, EventExchangeHeads, EventPeerConnected, EventPeerDisconnected,
            EventStoreUpdated, EventSyncCompleted, EventSyncError,
        };

        let bus = self.ctx.db.base().event_bus();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AdminEvent>();

        // One forwarder task per event type: subscribe to the typed broadcast
        // channel and map each event into a normalized `AdminEvent`, preserving the
        // structured fields (store/peer/duration/heads/ts) the core event carries
        // (B1). A task ends when the client drops the stream (send fails) or the
        // channel closes.
        macro_rules! forward {
            ($ty:ty, $build:expr) => {
                if let Ok(mut rx_ev) = bus.subscribe::<$ty>().await {
                    let tx = tx.clone();
                    let build_fn: fn(&$ty) -> AdminEvent = $build;
                    tokio::spawn(async move {
                        while let Ok(ev) = rx_ev.recv().await {
                            if tx.send(build_fn(&ev)).is_err() {
                                break;
                            }
                        }
                    });
                }
            };
        }

        forward!(EventExchangeHeads, |e| AdminEvent {
            peer: Some(e.peer.to_string()),
            ..AdminEvent::new("sync", e.peer.to_string())
        });
        forward!(EventPeerConnected, |e| AdminEvent {
            peer: Some(e.node_id.clone()),
            ..AdminEvent::new("peer_connected", e.node_id.clone())
        });
        forward!(EventPeerDisconnected, |e| AdminEvent {
            peer: Some(e.node_id.clone()),
            ..AdminEvent::new("peer_disconnected", e.node_id.clone())
        });
        forward!(EventStoreUpdated, |e| AdminEvent {
            store: Some(e.store_address.clone()),
            ts: Some(e.timestamp.to_rfc3339()),
            ..AdminEvent::new(
                "store_updated",
                format!("{} (+{})", e.store_type, e.entries_added),
            )
        });
        forward!(EventSyncCompleted, |e| AdminEvent {
            store: Some(e.store_address.clone()),
            peer: Some(e.node_id.clone()),
            heads_synced: Some(e.heads_synced),
            duration_ms: Some(e.duration_ms),
            ts: Some(e.timestamp.to_rfc3339()),
            ..AdminEvent::new(
                "sync_completed",
                format!("{} heads in {}ms", e.heads_synced, e.duration_ms),
            )
        });
        forward!(EventSyncError, |e| AdminEvent {
            store: Some(e.store_address.clone()),
            peer: Some(e.node_id.clone()),
            ts: Some(e.timestamp.to_rfc3339()),
            ..AdminEvent::new("sync_error", e.error_message.clone())
        });
        forward!(EventDatabaseCreated, |e| AdminEvent::new(
            "database_created",
            e.name.clone()
        ));

        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        ))
    }

    async fn kv_put(&self, store: &str, key: &str, value: Vec<u8>) -> AdminResult<()> {
        let handle = self.find_store(store)?;
        as_kv(&handle, store)?
            .put_impl(key, value)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn kv_delete(&self, store: &str, key: &str) -> AdminResult<()> {
        let handle = self.find_store(store)?;
        as_kv(&handle, store)?
            .delete_impl(key)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn peer_sync(&self, node_id: &str) -> AdminResult<()> {
        let id = node_id
            .parse::<iroh::EndpointId>()
            .map_err(|e| AdminError::new("bad_node_id", e.to_string()))?;
        self.ctx.db.connect_to_peer(id).await.map_err(db_err)?;
        Ok(())
    }

    async fn keystore_list(&self) -> AdminResult<Vec<String>> {
        let ks = self.ctx.db.base().keystore();
        // Sync read under the parking_lot guard — no `.await` while it's held.
        let keys = {
            let guard = ks.read();
            match guard.as_ref() {
                Some(k) => k.enumerate_keys().map_err(db_err)?,
                None => Vec::new(),
            }
        };
        Ok(keys)
    }

    async fn keystore_detail(&self, key_id: &str) -> AdminResult<KeyInfo> {
        let ks = self.ctx.db.base().keystore();
        // Read pubkey + lifecycle metadata (D2) under one guard (both sync).
        let (public_key, meta) = {
            let guard = ks.read();
            match guard.as_ref() {
                Some(k) => (
                    k.public_key(key_id).map_err(db_err)?,
                    k.key_meta(key_id).map_err(db_err)?,
                ),
                None => (None, None),
            }
        };
        Ok(KeyInfo {
            key_id: key_id.to_string(),
            public_key,
            kind: meta.as_ref().map(|m| m.kind.clone()),
            created_at: meta.as_ref().map(|m| m.created_at),
            rotated_count: meta.as_ref().map(|m| m.rotated_count),
        })
    }

    async fn keystore_generate(&self, key_id: &str) -> AdminResult<String> {
        let ks = self.ctx.db.base().keystore();
        let public = {
            let guard = ks.read();
            match guard.as_ref() {
                Some(k) => k.generate_key(key_id).map_err(db_err)?,
                None => return Err(AdminError::new("no_keystore", "keystore não disponível")),
            }
        };
        Ok(public)
    }

    async fn acl_list(&self) -> AdminResult<Vec<AclSummary>> {
        let mut out = Vec::new();
        for (store, handle) in self.ctx.db.list_stores() {
            let ac = handle.access_controller();
            let controller_type = ac.get_type().to_string();
            let mut roles = Vec::new();
            for role in ACL_ROLES {
                let keys = ac.get_authorized_by_role(role).await.unwrap_or_default();
                roles.push(AclRole {
                    role: role.to_string(),
                    keys,
                });
            }
            out.push(AclSummary {
                store,
                controller_type,
                roles,
            });
        }
        Ok(out)
    }

    async fn acl_grant(&self, store: &str, role: &str, key_id: &str) -> AdminResult<()> {
        let handle = self.find_store(store)?;
        handle
            .access_controller()
            .grant(role, key_id)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn acl_revoke(&self, store: &str, role: &str, key_id: &str) -> AdminResult<()> {
        let handle = self.find_store(store)?;
        handle
            .access_controller()
            .revoke(role, key_id)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn acl_create(
        &self,
        controller_type: &str,
        name: &str,
        admin_keys: Vec<String>,
        write_keys: Vec<String>,
    ) -> AdminResult<String> {
        use crate::access_control::manifest::{self, CreateAccessControllerOptions};
        use std::collections::HashMap;

        let mut access = HashMap::new();
        if !admin_keys.is_empty() {
            access.insert("admin".to_string(), admin_keys);
        }
        if !write_keys.is_empty() {
            access.insert("write".to_string(), write_keys);
        }
        // `new_simple` sets `skip_manifest = true` (in-memory only); flip it so the
        // manifest is persisted to Iroh and we get back a real shareable hash.
        let mut params =
            CreateAccessControllerOptions::new_simple(controller_type.to_string(), access);
        params.skip_manifest = false;
        params.name = name.to_string();

        let hash = manifest::create(
            Arc::new(self.ctx.client.clone()),
            controller_type.to_string(),
            &params,
        )
        .await
        .map_err(db_err)?;
        Ok(hash.to_string())
    }
}

/// Roles queried by `acl.list` (the Guardian ACL persists `write`/`admin`).
const ACL_ROLES: [&str; 2] = ["admin", "write"];

/// A managed store handle, as returned by `list_stores()`.
type StoreHandle =
    Arc<dyn crate::traits::Store<Error = crate::guardian::error::GuardianError> + Send + Sync>;

impl EmbeddedSource {
    /// The blob store, or a clear error if it is not initialized.
    async fn blobs_or_err(&self) -> AdminResult<crate::p2p::network::core::blobs::BlobStore> {
        self.ctx
            .client
            .blobs_client()
            .await
            .ok_or_else(|| AdminError::new("no_blobs", "blob store not initialized"))
    }

    /// Locate an already-open store by db-name or address, without reopening it.
    fn find_store(&self, store: &str) -> AdminResult<StoreHandle> {
        self.ctx
            .db
            .list_stores()
            .into_iter()
            // Exact match on db-name or full address only — a loose suffix test
            // would select the wrong store on a collision (and "" would match the
            // first store).
            .find(|(addr, s)| s.db_name() == store || addr.as_str() == store)
            .map(|(_, s)| s)
            .ok_or_else(|| AdminError::new("not_found", format!("store '{store}' is not open")))
    }

    /// Like [`find_store`], but also returns the store's **address** (the live-map
    /// key) so callers can `db.delete_store(addr)` on close/drop (G2).
    fn find_store_entry(&self, name: &str) -> AdminResult<(String, StoreHandle)> {
        self.ctx
            .db
            .list_stores()
            .into_iter()
            .find(|(addr, s)| s.db_name() == name || addr.as_str() == name)
            .ok_or_else(|| AdminError::new("not_found", format!("store '{name}' is not open")))
    }

    /// Open a store from a [`StoreSpec`], resolve its address, and persist it in
    /// the registry so it reopens on restart (G1/G3). Shared by create and import.
    async fn open_and_register(&self, name: &str, spec: StoreSpec) -> AdminResult<String> {
        self.ctx.open_store_from_spec(name, &spec).await?;
        let address = self
            .ctx
            .db
            .list_stores()
            .into_iter()
            .find(|(_, s)| s.db_name() == name)
            .map(|(addr, _)| addr)
            .unwrap_or_else(|| name.to_string());
        if let Some(reg) = &self.ctx.store_registry
            && let Err(e) = reg.put(name, &spec)
        {
            tracing::warn!("admin: store '{name}' opened but not persisted: {e}");
        }
        Ok(address)
    }
}

/// Downcast an open store handle to a concrete `GuardianDBKeyValue`, or a typed
/// `wrong_type` error naming the store.
fn as_kv<'a>(handle: &'a StoreHandle, store: &str) -> AdminResult<&'a GuardianDBKeyValue> {
    handle
        .as_any()
        .downcast_ref::<GuardianDBKeyValue>()
        .ok_or_else(|| AdminError::new("wrong_type", format!("'{store}' is not a KeyValue store")))
}

/// Downcast an open store handle to a concrete `GuardianDBEventLogStore`.
fn as_log<'a>(handle: &'a StoreHandle, store: &str) -> AdminResult<&'a GuardianDBEventLogStore> {
    handle
        .as_any()
        .downcast_ref::<GuardianDBEventLogStore>()
        .ok_or_else(|| AdminError::new("wrong_type", format!("'{store}' is not an EventLog store")))
}

/// Downcast an open store handle to a concrete `GuardianDBDocumentStore` (for
/// share-ticket generation, which is inherent on the concrete type).
fn as_document<'a>(
    handle: &'a StoreHandle,
    store: &str,
) -> AdminResult<&'a crate::stores::document_store::GuardianDBDocumentStore> {
    handle
        .as_any()
        .downcast_ref::<crate::stores::document_store::GuardianDBDocumentStore>()
        .ok_or_else(|| AdminError::new("wrong_type", format!("'{store}' is not a Document store")))
}

/// Assert an open store handle is a Document store (the `docs.*` ops read its
/// index generically, so a type guard prevents listing KV/EventLog stores).
fn ensure_document(handle: &StoreHandle, store: &str) -> AdminResult<()> {
    if handle.store_type() == "document" {
        Ok(())
    } else {
        Err(AdminError::new(
            "wrong_type",
            format!("'{store}' is not a Document store"),
        ))
    }
}

/// Map any store/DB error into a wire [`AdminError`] with a stable code.
fn db_err(e: impl std::fmt::Display) -> AdminError {
    AdminError::new("store", e.to_string())
}

/// Parse a hex/base32 blob hash string into an iroh-blobs `Hash`.
///
/// `iroh_blobs 0.103`'s `Hash::from_str` decodes into a fixed 32-byte buffer and
/// **panics** (instead of erroring) on any input length that isn't 64 (hex) or 52
/// (base32-of-32-bytes). Guard the length up front so malformed input from a
/// client/cursor surfaces as a clean `AdminError` rather than aborting the task.
fn parse_blob_hash(s: &str) -> AdminResult<iroh_blobs::Hash> {
    if s.len() != 64 && s.len() != 52 {
        return Err(AdminError::new(
            "bad_hash",
            format!(
                "invalid hash length ({}): expected 64 hex or 52 base32",
                s.len()
            ),
        ));
    }
    s.parse::<iroh_blobs::Hash>()
        .map_err(|e| AdminError::new("bad_hash", e.to_string()))
}

/// Build a [`BlobContent`] preview from raw bytes: first 512 bytes, with a
/// best-effort text/binary classification.
fn build_blob_content(bytes: &[u8]) -> BlobContent {
    let head = &bytes[..bytes.len().min(512)];
    BlobContent {
        size: bytes.len() as u64,
        is_text: looks_like_text(head),
        preview: String::from_utf8_lossy(head).into_owned(),
    }
}

/// Classify a byte head as text: valid UTF-8 (tolerating a truncated final
/// multi-byte char) with no control chars other than tab/newline/CR. Rejects
/// binary, unlike a naive "any high byte is text" check.
fn looks_like_text(head: &[u8]) -> bool {
    if head.is_empty() {
        return false;
    }
    let valid = match std::str::from_utf8(head) {
        Ok(_) => head,
        // Accept if only the last (possibly cut) multi-byte sequence is invalid.
        Err(e) if head.len() - e.valid_up_to() <= 3 => &head[..e.valid_up_to()],
        Err(_) => return false,
    };
    std::str::from_utf8(valid)
        .map(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| matches!(c, '\n' | '\r' | '\t') || !c.is_control())
        })
        .unwrap_or(false)
}

/// Best-effort classification of a connection's link kind from its address:
/// relay/n0 (URL-ish), direct (socket address), or unknown.
fn infer_link_kind(address: &str) -> &'static str {
    let a = address.to_lowercase();
    if a.starts_with("http") || a.contains("relay") {
        "relay"
    } else if address.parse::<std::net::SocketAddr>().is_ok() {
        "direct"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend with no GuardianDB, so the roundtrip test exercises exactly the
    /// seam + protocol + server + client, deterministically and without networking
    /// setup. `EmbeddedSource` (the real backend) is covered by the binary.
    struct MockSource;

    #[async_trait]
    impl AdminSource for MockSource {
        async fn stores_list(&self) -> AdminResult<Vec<StoreSummary>> {
            Ok(vec![StoreSummary {
                address: "guardian/abc/log1".into(),
                store_type: "eventlog".into(),
                db_name: "log1".into(),
                entry_count: 3,
            }])
        }

        async fn node_info(&self) -> AdminResult<NodeSummary> {
            Ok(NodeSummary {
                node_id: "node-abc".into(),
                uptime_s: 0,
                stores: 1,
            })
        }

        async fn stores_create(
            &self,
            kind: &str,
            name: &str,
            _opts: StoreCreateOpts,
        ) -> AdminResult<String> {
            Ok(format!("guardian/mock/{kind}/{name}"))
        }

        async fn node_identity(&self) -> AdminResult<NodeIdentity> {
            Ok(NodeIdentity {
                node_id: "node-abc".into(),
                addresses: vec!["127.0.0.1:11204".into()],
            })
        }

        async fn stores_share(&self, _name: &str) -> AdminResult<StoreTickets> {
            Ok(StoreTickets {
                read: "docticket-read-abc".into(),
                write: "docticket-write-abc".into(),
            })
        }

        async fn stores_import(
            &self,
            kind: &str,
            name: &str,
            _ticket: &str,
            _read_only: bool,
        ) -> AdminResult<String> {
            Ok(format!("guardian/mock/{kind}/{name}"))
        }

        async fn stores_close(&self, _name: &str) -> AdminResult<()> {
            Ok(())
        }

        async fn stores_drop(&self, _name: &str) -> AdminResult<()> {
            Ok(())
        }

        async fn eventlog_append(&self, _store: &str, _data: &str) -> AdminResult<String> {
            Ok("blake3-appended".into())
        }

        async fn docs_put(&self, _store: &str, id: &str, _json: &str) -> AdminResult<String> {
            Ok(id.into())
        }

        async fn docs_delete(&self, _store: &str, _id: &str) -> AdminResult<()> {
            Ok(())
        }

        async fn kv_entries(&self, _store: &str) -> AdminResult<Vec<KvEntry>> {
            Ok(vec![KvEntry {
                key: "theme".into(),
                value_utf8: "dark".into(),
                size: 4,
            }])
        }

        async fn eventlog_entries(
            &self,
            _store: &str,
            _limit: Option<usize>,
            _before: Option<&str>,
        ) -> AdminResult<Vec<LogEntry>> {
            Ok(vec![LogEntry {
                index: 0,
                op: "ADD".into(),
                key: None,
                value_utf8: "hello".into(),
                size: 5,
                hash: "blake3-entry".into(),
                log_id: "log1".into(),
                identity: Some("id-1".into()),
                clock_id: "id-1".into(),
                clock_time: 1,
                next: vec![],
            }])
        }

        async fn eventlog_heads(&self, _store: &str) -> AdminResult<Vec<CrdtHead>> {
            Ok(vec![CrdtHead {
                hash: "blake3-head".into(),
                clock_id: "id-1".into(),
                clock_time: 1,
                identity: Some("id-1".into()),
                next: vec![],
            }])
        }

        async fn docs_list(&self, _store: &str) -> AdminResult<Vec<DocEntry>> {
            Ok(vec![DocEntry {
                id: "doc-1".into(),
                value_utf8: "{\"_id\":\"doc-1\"}".into(),
                size: 15,
            }])
        }

        async fn docs_get(&self, _store: &str, id: &str) -> AdminResult<DocEntry> {
            Ok(DocEntry {
                id: id.into(),
                value_utf8: format!("{{\n  \"_id\": \"{id}\"\n}}"),
                size: 15,
            })
        }

        async fn peers_list(&self) -> AdminResult<Vec<PeerSummary>> {
            Ok(vec![PeerSummary {
                node_id: "peer-xyz".into(),
                addresses: vec!["127.0.0.1:5000".into()],
                connected: true,
            }])
        }

        async fn net_topology(&self) -> AdminResult<Vec<TopoLink>> {
            Ok(vec![TopoLink {
                node_id: "peer-xyz".into(),
                address: "127.0.0.1:5000".into(),
                latency_ms: 12.0,
                ops: 3,
                connected_secs: 60,
                link_kind: "direct".into(),
                conn_type: Some("direct".into()),
                p95_ms: Some(30.0),
                p99_ms: Some(55.0),
            }])
        }

        async fn net_relay(&self) -> AdminResult<Vec<RelayStatus>> {
            Ok(vec![RelayStatus {
                url: "https://relay.example".into(),
                connected: true,
                last_error: None,
            }])
        }

        async fn node_latency(&self) -> AdminResult<LatencyStats> {
            Ok(LatencyStats {
                p95_ms: 40.0,
                p99_ms: 90.0,
            })
        }

        async fn node_throughput(&self) -> AdminResult<ThroughputStats> {
            Ok(ThroughputStats {
                ops_per_second: 12.5,
                bytes_per_second: 4096,
                peak_throughput: 20.0,
                avg_throughput: 10.0,
            })
        }

        async fn net_discovered(&self) -> AdminResult<Vec<String>> {
            Ok(vec!["peer-known-offline".into()])
        }

        async fn blobs_list(&self) -> AdminResult<Vec<BlobSummary>> {
            Ok(vec![BlobSummary {
                hash: "blake3-abc".into(),
                size: 42,
                complete: true,
            }])
        }

        async fn blob_get(&self, _hash: &str) -> AdminResult<BlobContent> {
            Ok(BlobContent {
                size: 5,
                is_text: true,
                preview: "hello".into(),
            })
        }

        async fn blob_add(&self, _path: &str) -> AdminResult<String> {
            Ok("blake3-new".into())
        }

        async fn blob_export(&self, _hash: &str, _path: &str) -> AdminResult<u64> {
            Ok(5)
        }

        async fn blob_delete(&self, _hash: &str) -> AdminResult<()> {
            Ok(())
        }

        async fn events_subscribe(&self) -> AdminResult<BoxStream<'static, AdminEvent>> {
            // A finite stream so the roundtrip test terminates deterministically.
            let events = vec![
                AdminEvent {
                    peer: Some("peer-1".into()),
                    store: Some("store-1".into()),
                    ..AdminEvent::new("sync", "peer-1")
                },
                AdminEvent {
                    store: Some("kv".into()),
                    ..AdminEvent::new("store_updated", "kv (+1)")
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }

        async fn kv_put(&self, _store: &str, _key: &str, _value: Vec<u8>) -> AdminResult<()> {
            Ok(())
        }

        async fn kv_delete(&self, _store: &str, _key: &str) -> AdminResult<()> {
            Ok(())
        }

        async fn peer_sync(&self, _node_id: &str) -> AdminResult<()> {
            Ok(())
        }

        async fn keystore_list(&self) -> AdminResult<Vec<String>> {
            Ok(vec!["identity-abc".into()])
        }

        async fn keystore_detail(&self, key_id: &str) -> AdminResult<KeyInfo> {
            Ok(KeyInfo {
                key_id: key_id.into(),
                public_key: Some("pub-abc".into()),
                kind: Some("ed25519".into()),
                created_at: Some(1_700_000_000),
                rotated_count: Some(2),
            })
        }

        async fn keystore_generate(&self, _key_id: &str) -> AdminResult<String> {
            Ok("pub-new".into())
        }

        async fn acl_list(&self) -> AdminResult<Vec<AclSummary>> {
            Ok(vec![AclSummary {
                store: "log1".into(),
                controller_type: "guardian".into(),
                roles: vec![AclRole {
                    role: "write".into(),
                    keys: vec!["key-1".into()],
                }],
            }])
        }

        async fn acl_grant(&self, _store: &str, _role: &str, _key_id: &str) -> AdminResult<()> {
            Ok(())
        }

        async fn acl_revoke(&self, _store: &str, _role: &str, _key_id: &str) -> AdminResult<()> {
            Ok(())
        }

        async fn acl_create(
            &self,
            _t: &str,
            _name: &str,
            _admin: Vec<String>,
            _write: Vec<String>,
        ) -> AdminResult<String> {
            Ok("blake3-manifest-hash".into())
        }
    }

    async fn spawn_mock_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let source: Arc<dyn AdminSource> = Arc::new(MockSource);
        tokio::spawn(async move {
            let _ = serve_on(listener, source, None).await;
        });
        addr
    }

    #[tokio::test]
    async fn roundtrip_stores_list_and_node_info() {
        let addr = spawn_mock_server().await;
        let client = AdminClient::connect(&addr).await.unwrap();

        let stores = client.stores_list().await.unwrap();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].db_name, "log1");
        assert_eq!(stores[0].store_type, "eventlog");
        assert_eq!(stores[0].entry_count, 3);

        let node = client.node_info().await.unwrap();
        assert_eq!(node.node_id, "node-abc");
        assert_eq!(node.stores, 1);

        // Document inspector ops (B4) exercised over the wire via the mock source.
        let docs = client.docs_list("mydocs").await.unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].id, "doc-1");
        let doc = client.docs_get("mydocs", "doc-1").await.unwrap();
        assert_eq!(doc.id, "doc-1");
        assert!(doc.value_utf8.contains("doc-1"));

        // Onda 3 ops over the wire: relay status (C2), global latency (C1), and the
        // blob size/completeness fields (C4/C5).
        let relays = client.net_relay().await.unwrap();
        assert_eq!(relays.len(), 1);
        assert!(relays[0].connected);
        let lat = client.node_latency().await.unwrap();
        assert_eq!((lat.p95_ms, lat.p99_ms), (40.0, 90.0));
        let blobs = client.blobs_list().await.unwrap();
        assert_eq!(blobs[0].size, 42);
        assert!(blobs[0].complete);
        let topo = client.net_topology().await.unwrap();
        assert_eq!(topo[0].conn_type.as_deref(), Some("direct"));
        // Per-peer p95/p99 (C1) ride along on the topology link.
        assert_eq!(topo[0].p95_ms, Some(30.0));
        assert_eq!(topo[0].p99_ms, Some(55.0));

        // Onda 4 ops over the wire: throughput (D1), discovered-not-connected (C3),
        // and keystore lifecycle metadata (D2).
        let thr = client.node_throughput().await.unwrap();
        assert_eq!(thr.bytes_per_second, 4096);
        let disc = client.net_discovered().await.unwrap();
        assert_eq!(disc, vec!["peer-known-offline".to_string()]);
        let key = client.keystore_detail("identity-abc").await.unwrap();
        assert_eq!(key.kind.as_deref(), Some("ed25519"));
        assert_eq!(key.rotated_count, Some(2));

        // G3 ops over the wire: identity, share tickets, import.
        let ident = client.node_identity().await.unwrap();
        assert_eq!(ident.node_id, "node-abc");
        assert!(!ident.addresses.is_empty());
        let tickets = client.stores_share("settings").await.unwrap();
        assert!(tickets.read.contains("read") && tickets.write.contains("write"));
        let imported = client
            .stores_import("keyvalue", "shared", "docticket-xyz", true)
            .await
            .unwrap();
        assert!(imported.contains("shared"));

        // G2 write/lifecycle ops over the wire.
        client.stores_close("settings").await.unwrap();
        client.stores_drop("shared").await.unwrap();
        let h = client.eventlog_append("events", "hello").await.unwrap();
        assert!(!h.is_empty());
        let did = client
            .docs_put("mydocs", "d2", r#"{"name":"bob"}"#)
            .await
            .unwrap();
        assert_eq!(did, "d2");
        client.docs_delete("mydocs", "d2").await.unwrap();
    }

    #[tokio::test]
    async fn unknown_op_returns_structured_error() {
        let addr = spawn_mock_server().await;
        let client = AdminClient::connect(&addr).await.unwrap();

        let err = client
            .request("does.not.exist", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code, "unknown_op");
    }

    #[tokio::test]
    async fn events_stream_over_rpc() {
        use futures::StreamExt;

        let addr = spawn_mock_server().await;
        let client = AdminClient::connect(&addr).await.unwrap();

        let mut stream = client.events_subscribe().await.unwrap();
        let mut kinds = Vec::new();
        while let Some(ev) = stream.next().await {
            kinds.push(ev.kind);
        }
        // The finite MockSource stream yields two events, then the server sends
        // `End`, which terminates the client-side stream.
        assert_eq!(kinds, vec!["sync".to_string(), "store_updated".to_string()]);
    }

    #[tokio::test]
    async fn subscription_drop_leaves_connection_usable() {
        use futures::StreamExt;

        let addr = spawn_mock_server().await;
        let client = AdminClient::connect(&addr).await.unwrap();

        // Subscribe, take one event, then drop the stream early (before End) —
        // Drop deregisters the demux entry and fires `events.unsubscribe`.
        {
            let mut sub = client.events_subscribe().await.unwrap();
            let _ = sub.next().await;
        }

        // The unsubscribe write must not corrupt the protocol: a normal request
        // on the same connection still succeeds.
        assert!(client.stores_list().await.is_ok());
        assert!(client.node_info().await.is_ok());
    }

    #[tokio::test]
    async fn token_auth_gates_ops() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let source: Arc<dyn AdminSource> = Arc::new(MockSource);
        tokio::spawn(async move {
            let _ = serve_on(listener, source, Some("s3cret".to_string())).await;
        });

        let client = AdminClient::connect(&addr).await.unwrap();

        // Unauthenticated ops are refused — both reads and **writes** (G5.1): the
        // gate is connection-level, so every mutating op is covered too.
        let err = client.stores_list().await.unwrap_err();
        assert_eq!(err.code, "unauthorized");
        assert_eq!(
            client
                .stores_create("keyvalue", "x", StoreCreateOpts::default())
                .await
                .unwrap_err()
                .code,
            "unauthorized"
        );
        assert_eq!(
            client.stores_drop("x").await.unwrap_err().code,
            "unauthorized"
        );

        // Wrong token is refused; correct token unlocks the connection.
        assert_eq!(
            client.authenticate("nope").await.unwrap_err().code,
            "unauthorized"
        );
        client.authenticate("s3cret").await.unwrap();
        assert!(client.stores_list().await.is_ok());
    }

    /// End-to-end over a real `GuardianDB`: `EmbeddedSource` → `serve` →
    /// `AdminClient`. This is exactly the path the panel's `--connect` mode uses.
    #[tokio::test]
    async fn embedded_source_over_real_db_via_rpc() {
        use crate::guardian::GuardianDB;
        use crate::guardian::core::NewGuardianDBOptions;
        use crate::p2p::network::{client::IrohClient, config::ClientConfig};

        let tmp = tempfile::TempDir::new().unwrap();
        let iroh = IrohClient::new(ClientConfig {
            data_store_path: Some(tmp.path().join("iroh")),
            ..Default::default()
        })
        .await
        .unwrap();
        let backend = iroh.backend().clone();
        let db = GuardianDB::new(
            iroh.clone(),
            Some(NewGuardianDBOptions {
                directory: Some(tmp.path().join("db")),
                backend: Some(backend),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        // Open a store so `list_stores()` has content to report.
        let _kv = db.key_value("settings", None).await.unwrap();

        let ctx = AdminContext::new(Arc::new(db), iroh);
        let source: Arc<dyn AdminSource> = Arc::new(EmbeddedSource::new(ctx));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let _ = serve_on(listener, source, None).await;
        });

        let client = AdminClient::connect(&addr).await.unwrap();
        let stores = client.stores_list().await.unwrap();
        assert!(
            stores.iter().any(|s| s.db_name == "settings"),
            "expected 'settings' store, got {stores:?}"
        );

        let node = client.node_info().await.unwrap();
        assert!(!node.node_id.is_empty());
        assert!(node.stores >= 1);
    }

    /// G1: create a store through the admin op and verify (a) it shows up in
    /// `stores.list`, and (b) it was recorded in the persistent registry so it
    /// would reopen on restart. Also exercises `reopen_stores` (idempotent here).
    #[tokio::test]
    async fn stores_create_persists_in_registry() {
        use crate::guardian::GuardianDB;
        use crate::guardian::core::NewGuardianDBOptions;
        use crate::p2p::network::{client::IrohClient, config::ClientConfig};

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let iroh = IrohClient::new(ClientConfig {
            data_store_path: Some(data_dir.join("iroh")),
            ..Default::default()
        })
        .await
        .unwrap();
        let backend = iroh.backend().clone();
        let db = GuardianDB::new(
            iroh.clone(),
            Some(NewGuardianDBOptions {
                directory: Some(data_dir.join("db")),
                backend: Some(backend),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let ctx = AdminContext::with_data_dir(Arc::new(db), iroh, data_dir.clone());
        let source = EmbeddedSource::new(ctx.clone());

        // Create an EventLog store with local_only (avoids the replication path in
        // this isolated test) via the admin op.
        let opts = StoreCreateOpts {
            replicate: false,
            local_only: true,
            read_only: false,
            acl_address: None,
        };
        let addr = source
            .stores_create("eventlog", "audit", opts)
            .await
            .unwrap();
        assert!(!addr.is_empty());

        // It must show up in the live store list.
        let stores = source.stores_list().await.unwrap();
        assert!(
            stores.iter().any(|s| s.db_name == "audit"),
            "created store should be listed, got {stores:?}"
        );

        // Creating the same name again is rejected (not silently reopened).
        assert!(
            source
                .stores_create("eventlog", "audit", StoreCreateOpts::default())
                .await
                .is_err(),
            "duplicate create should error"
        );

        // The spec was persisted to the registry, so a fresh process would reopen
        // it on boot (`reopen_stores` reads exactly this). We assert the persisted
        // spec directly via the already-open registry handle — reopening in the
        // same process would hit the store's own lock. The registry's
        // survives-restart behavior is covered by `store_registry::tests`.
        let recorded = ctx.registered_stores();
        let spec = recorded
            .iter()
            .find(|(n, _)| n == "audit")
            .map(|(_, s)| s)
            .expect("spec persisted in the registry");
        assert_eq!(spec.kind, "eventlog");
        assert!(spec.local_only && !spec.replicate);
    }

    /// End-to-end for the R1 data inspectors: write a KV pair and a log entry into
    /// a real `GuardianDB`, then read them back through the RPC seam.
    #[tokio::test]
    async fn kv_and_eventlog_entries_over_rpc() {
        use crate::guardian::GuardianDB;
        use crate::guardian::core::NewGuardianDBOptions;
        use crate::p2p::network::{client::IrohClient, config::ClientConfig};
        use crate::traits::CreateDBOptions;

        let tmp = tempfile::TempDir::new().unwrap();
        let iroh = IrohClient::new(ClientConfig {
            data_store_path: Some(tmp.path().join("iroh")),
            ..Default::default()
        })
        .await
        .unwrap();
        let backend = iroh.backend().clone();
        let db = GuardianDB::new(
            iroh.clone(),
            Some(NewGuardianDBOptions {
                directory: Some(tmp.path().join("db")),
                backend: Some(backend),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        // Seed a KV store and an EventLog store. `replicate: false` avoids the redb
        // lock that opening multiple replicating stores in one db would hit; the
        // handles are kept alive so they stay in `list_stores()`.
        let store_opts = || {
            Some(CreateDBOptions {
                replicate: Some(false),
                local_only: Some(false),
                ..Default::default()
            })
        };
        let kv = db.key_value("settings", store_opts()).await.unwrap();
        kv.put("theme", b"dark".to_vec()).await.unwrap();
        let log = db.log("events", store_opts()).await.unwrap();
        log.add(b"hello".to_vec()).await.unwrap();
        // A few more entries so cursor pagination has something to page over.
        for i in 0..4 {
            log.add(format!("entry-{i}").into_bytes()).await.unwrap();
        }

        let ctx = AdminContext::new(Arc::new(db), iroh);
        let source: Arc<dyn AdminSource> = Arc::new(EmbeddedSource::new(ctx));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let _ = serve_on(listener, source, None).await;
        });

        let client = AdminClient::connect(&addr).await.unwrap();

        let kv_entries = client.kv_entries("settings").await.unwrap();
        assert!(
            kv_entries
                .iter()
                .any(|e| e.key == "theme" && e.value_utf8 == "dark"),
            "expected theme=dark, got {kv_entries:?}"
        );

        let log_entries = client.eventlog_entries("events", None, None).await.unwrap();
        assert!(
            log_entries.iter().any(|e| e.value_utf8 == "hello"),
            "expected an entry 'hello', got {log_entries:?}"
        );

        // Cursor pagination (recurso 2.1): fetch the newest 2, then use the oldest
        // of that block as a `before` cursor to page backwards into older history.
        let newest = client
            .eventlog_entries("events", Some(2), None)
            .await
            .unwrap();
        assert_eq!(newest.len(), 2, "expected a 2-entry page, got {newest:?}");
        let cursor = newest[0].hash.clone();
        assert!(!cursor.is_empty(), "cursor hash should be populated");
        let older = client
            .eventlog_entries("events", Some(2), Some(&cursor))
            .await
            .unwrap();
        assert!(
            !older.is_empty() && older.iter().all(|e| e.hash != cursor),
            "older page must exclude the cursor entry, got {older:?}"
        );
        // An unparseable cursor is a structured error, not a panic.
        assert!(
            client
                .eventlog_entries("events", Some(2), Some("not-a-hash"))
                .await
                .is_err(),
            "a malformed cursor should surface an error"
        );

        // Document inspector ops (B4): the `docs.*` type guard rejects a
        // non-document store. (Creating a live iroh-docs store needs relay/network
        // this isolated test lacks; the docs wire path is covered by MockSource.)
        assert!(
            client.docs_list("events").await.is_err(),
            "docs.list on an EventLog store must be a wrong_type error"
        );

        // A single-writer log has exactly one head (no divergence).
        let heads = client.eventlog_heads("events").await.unwrap();
        assert_eq!(heads.len(), 1, "expected one head, got {heads:?}");
        assert!(!heads[0].hash.is_empty());

        // No peers connected and no blobs added in this isolated test, but both ops
        // must succeed over the wire (exercising the client/backend path).
        let peers = client.peers_list().await.unwrap();
        assert!(peers.is_empty(), "expected no peers, got {peers:?}");
        let blobs = client.blobs_list().await.unwrap();
        assert!(blobs.is_empty(), "expected no blobs, got {blobs:?}");
        // No active connections in this isolated node, but the op must succeed.
        let topo = client.net_topology().await.unwrap();
        assert!(topo.is_empty(), "expected no links, got {topo:?}");

        // Blob cycle (recurso 9): add from a file, read back, list, export, delete.
        let blob_src = tmp.path().join("blob-src.txt");
        tokio::fs::write(&blob_src, b"hello blob").await.unwrap();
        let bhash = client.blob_add(blob_src.to_str().unwrap()).await.unwrap();
        assert!(!bhash.is_empty());
        let content = client.blob_get(&bhash).await.unwrap();
        assert_eq!(content.size, 10);
        assert!(content.is_text);
        assert!(content.preview.contains("hello blob"));
        assert!(
            client
                .blobs_list()
                .await
                .unwrap()
                .iter()
                .any(|b| b.hash == bhash),
            "added blob should appear in blobs.list"
        );
        let out = tmp.path().join("blob-out.txt");
        let n = client
            .blob_export(&bhash, out.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(n, 10);
        assert_eq!(tokio::fs::read(&out).await.unwrap(), b"hello blob");
        client.blob_delete(&bhash).await.unwrap();

        // R3 actions: put a new key, confirm it lands, delete it, confirm it's gone.
        client
            .kv_put("settings", "lang", b"pt".to_vec())
            .await
            .unwrap();
        let after_put = client.kv_entries("settings").await.unwrap();
        assert!(
            after_put
                .iter()
                .any(|e| e.key == "lang" && e.value_utf8 == "pt"),
            "expected lang=pt after put, got {after_put:?}"
        );

        client.kv_delete("settings", "lang").await.unwrap();
        let after_del = client.kv_entries("settings").await.unwrap();
        assert!(
            !after_del.iter().any(|e| e.key == "lang"),
            "expected 'lang' gone after delete, got {after_del:?}"
        );
    }

    /// End-to-end for R4/recurso-4: list keystore + ACL, grant/revoke a key.
    #[tokio::test]
    async fn keystore_and_acl_over_rpc() {
        use crate::guardian::GuardianDB;
        use crate::guardian::core::NewGuardianDBOptions;
        use crate::p2p::network::{client::IrohClient, config::ClientConfig};
        use crate::traits::CreateDBOptions;

        let tmp = tempfile::TempDir::new().unwrap();
        let iroh = IrohClient::new(ClientConfig {
            data_store_path: Some(tmp.path().join("iroh")),
            ..Default::default()
        })
        .await
        .unwrap();
        let backend = iroh.backend().clone();
        let db = GuardianDB::new(
            iroh.clone(),
            Some(NewGuardianDBOptions {
                directory: Some(tmp.path().join("db")),
                backend: Some(backend),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let _kv = db
            .key_value(
                "settings",
                Some(CreateDBOptions {
                    replicate: Some(false),
                    local_only: Some(false),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        let ctx = AdminContext::new(Arc::new(db), iroh);
        let source: Arc<dyn AdminSource> = Arc::new(EmbeddedSource::new(ctx));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let _ = serve_on(listener, source, None).await;
        });

        let client = AdminClient::connect(&addr).await.unwrap();

        // keystore.list must succeed (metadata only).
        client.keystore_list().await.unwrap();

        // keystore generate → returns a public key; detail derives the same public
        // (never the secret); the key appears in the list.
        let pubk = client.keystore_generate("admin-test-key").await.unwrap();
        assert!(!pubk.is_empty());
        let info = client.keystore_detail("admin-test-key").await.unwrap();
        assert_eq!(info.public_key.as_deref(), Some(pubk.as_str()));
        assert!(
            client
                .keystore_list()
                .await
                .unwrap()
                .iter()
                .any(|k| k == "admin-test-key")
        );

        // acl.list reports the store's (Simple) controller.
        let acls = client.acl_list().await.unwrap();
        assert!(!acls.is_empty(), "expected a controller, got {acls:?}");
        let store_addr = acls[0].store.clone();
        assert_eq!(acls[0].controller_type, "simple");

        // grant → appears under "write".
        client
            .acl_grant(&store_addr, "write", "test-key")
            .await
            .unwrap();
        let write_keys = |acls: &[AclSummary]| -> Vec<String> {
            acls.iter()
                .find(|a| a.store == store_addr)
                .and_then(|a| a.roles.iter().find(|r| r.role == "write"))
                .map(|r| r.keys.clone())
                .unwrap_or_default()
        };
        assert!(
            write_keys(&client.acl_list().await.unwrap()).contains(&"test-key".to_string()),
            "expected test-key granted"
        );

        // revoke → gone.
        client
            .acl_revoke(&store_addr, "write", "test-key")
            .await
            .unwrap();
        assert!(
            !write_keys(&client.acl_list().await.unwrap()).contains(&"test-key".to_string()),
            "expected test-key revoked"
        );

        // create a new controller manifest → a non-empty shareable hash.
        let manifest = client
            .acl_create("simple", "test-ctrl", vec![], vec!["writer-key".into()])
            .await
            .unwrap();
        assert!(!manifest.is_empty(), "expected a manifest hash");
    }
}
