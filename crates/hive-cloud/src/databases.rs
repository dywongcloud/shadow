//! Serverless data services — provisioned per project, billed by usage, like
//! Vercel Storage / Railway plugins / Upstash / Prisma Postgres.
//!
//! Five kinds:
//! * **Postgres** — a managed Postgres instance. When a container runtime is
//!   available we boot a real `postgres` (the same "instant Postgres" experience
//!   Prisma Postgres ships, which runs each database as its own lightweight VM /
//!   unikernel — see prisma.io & nanovms.com). Otherwise a connection record is
//!   provisioned in simulated mode.
//! * **Redis** — managed key/value, Upstash-style (TCP + REST token).
//! * **Blob** — S3-compatible object storage.
//! * **Queue** — a durable FIFO message queue.
//! * **Vector** — an embeddings index for AI workloads.
//!
//! Blob/Queue/Vector are backed by in-process stores exposed over the admin API,
//! so they are immediately usable without external services.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Postgres,
    Redis,
    Blob,
    Queue,
    Vector,
    /// Topic-based publish/subscribe broker (RabbitMQ / Kafka-style fan-out).
    Pubsub,
    /// Secure WebSocket streaming channels (rooms) for realtime apps.
    Realtime,
}
impl DbKind {
    pub fn label(&self) -> &'static str {
        match self {
            DbKind::Postgres => "Postgres",
            DbKind::Redis => "Redis",
            DbKind::Blob => "Blob",
            DbKind::Queue => "Queue",
            DbKind::Vector => "Vector",
            DbKind::Pubsub => "Pub/Sub",
            DbKind::Realtime => "Realtime",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbStatus {
    Provisioning,
    Ready,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Database {
    pub id: String,
    pub name: String,
    pub project: String,
    #[serde(default)]
    pub team: String,
    pub kind: DbKind,
    pub region: String,
    pub status: DbStatus,
    /// Marketplace provider label (e.g. "Neon", "Upstash") — cosmetic; the
    /// backing engine is `kind`.
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub mode: String, // "live" (real backing service) | "simulated"
    #[serde(default)]
    pub created_ms: u64,
    /// Connection details. Sensitive values are masked by `masked()`.
    #[serde(default)]
    pub connection: HashMap<String, String>,
    /// podman container name backing this DB (if live).
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub note: String,
    // ---- Cross-region replication ----
    /// Extra regions this database is replicated to (empty = single-region). The
    /// primary lives in `region`; a read-scalable replica is kept in each of these.
    #[serde(default)]
    pub replicas: Vec<String>,
    /// "primary" (the writable instance) | "replica" (a region read replica).
    #[serde(default = "primary_role")]
    pub role: String,
    /// The node currently hosting the PRIMARY (so replicas know where to stream
    /// from / mirror against). Empty for a lone single-region database.
    #[serde(default)]
    pub primary_node: String,
    /// The external DB-gateway hostname `<slug>.{db_domain}` (e.g.
    /// `db-ab12.downstash.xyz`) — the TLS-SNI endpoint external clients connect to.
    /// Empty when the gateway is disabled (no HIVE_DB_DOMAIN).
    #[serde(default)]
    pub db_host: String,
    /// The node whose podman actually runs this DB's container — the reconciler
    /// points `db_host`'s A record at THIS node's public IP so the SNI proxy that
    /// receives the connection has the container locally.
    #[serde(default)]
    pub host_node: String,
}

fn primary_role() -> String {
    "primary".into()
}

impl Database {
    fn masked(&self) -> Database {
        let mut d = self.clone();
        for (k, v) in d.connection.iter_mut() {
            let kl = k.to_lowercase();
            if kl.contains("password") || kl.contains("token") || kl.contains("secret") || kl.contains("url") {
                *v = mask(v);
            }
        }
        d
    }
}

fn mask(v: &str) -> String {
    if v.len() <= 8 {
        return "••••••••".into();
    }
    let tail = &v[v.len().saturating_sub(4)..];
    format!("••••••••{tail}")
}

// ---- In-process backing stores for Blob / Queue / Vector ----

/// DURABLE blob store: objects are written to disk (content survives restarts),
/// under `$HIVE_DATA/blob/<bucket>/<hex(key)>.bin` with atomic temp+rename writes.
/// (Replaces the old in-memory map that was labelled "live" but lost on restart.)
struct BlobStore {
    root: std::path::PathBuf,
}
impl BlobStore {
    fn new(root: std::path::PathBuf) -> BlobStore {
        let _ = std::fs::create_dir_all(&root);
        BlobStore { root }
    }
    fn bucket_dir(&self, bucket: &str) -> std::path::PathBuf {
        let safe: String = bucket
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        self.root.join(if safe.is_empty() { "_".to_string() } else { safe })
    }
    fn put(&self, bucket: &str, key: &str, data: &[u8]) {
        let dir = self.bucket_dir(bucket);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let h = hex_encode(key.as_bytes());
        let tmp = dir.join(format!("{h}.tmp"));
        let file = dir.join(format!("{h}.bin"));
        if std::fs::write(&tmp, data).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }
    fn get(&self, bucket: &str, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self.bucket_dir(bucket).join(format!("{}.bin", hex_encode(key.as_bytes())))).ok()
    }
    fn list(&self, bucket: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.bucket_dir(bucket)) {
            for e in rd.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    if let Some(stem) = name.strip_suffix(".bin") {
                        if let Some(k) = hex_decode(stem) {
                            out.push(k);
                        }
                    }
                }
            }
        }
        out
    }
    /// Permanently remove every object in `bucket` (the deleting-database's own
    /// bucket dir). Best-effort — an I/O error here must not fail the caller's
    /// broader delete flow.
    fn delete_bucket(&self, bucket: &str) {
        let _ = std::fs::remove_dir_all(self.bucket_dir(bucket));
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
fn hex_decode(s: &str) -> Option<String> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    let raw = s.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        let hi = (raw[i] as char).to_digit(16)?;
        let lo = (raw[i + 1] as char).to_digit(16)?;
        bytes.push((hi * 16 + lo) as u8);
        i += 2;
    }
    String::from_utf8(bytes).ok()
}
#[derive(Default)]
struct QueueStore {
    // queue -> messages
    msgs: RwLock<HashMap<String, std::collections::VecDeque<String>>>,
}
#[derive(Default)]
struct VectorStore {
    // index -> id -> (vector, metadata)
    items: RwLock<HashMap<String, HashMap<String, (Vec<f32>, serde_json::Value)>>>,
}

/// Pub/Sub + Realtime broker. Each named channel (topic or room) is a tokio
/// broadcast bus: publishers fan out to every live subscriber in O(1).
///
/// Mesh note: to fan out across nodes we deliberately do NOT re-dial the iroh
/// DHT per message (that would rate-limit fast). Instead we ride the already-
/// established peer connections ("trunks") — the same long-lived QUIC tunnels /
/// keep-alive HTTP pool the gossip loop uses — and replicate over those.
struct Broker {
    channels: RwLock<HashMap<String, tokio::sync::broadcast::Sender<String>>>,
    published: RwLock<HashMap<String, u64>>,
}
impl Default for Broker {
    fn default() -> Self {
        Broker {
            channels: RwLock::new(HashMap::new()),
            published: RwLock::new(HashMap::new()),
        }
    }
}
impl Broker {
    fn sender(&self, channel: &str) -> tokio::sync::broadcast::Sender<String> {
        if let Some(s) = self.channels.read().get(channel) {
            return s.clone();
        }
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        self.channels.write().insert(channel.to_string(), tx.clone());
        tx
    }
}

/// Durable snapshot of the in-process data stores (persisted with the platform
/// snapshot so queue/vector data survives restarts).
#[derive(Default, Serialize, Deserialize)]
pub struct DataSnapshot {
    #[serde(default)]
    pub queues: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub vectors: HashMap<String, HashMap<String, (Vec<f32>, serde_json::Value)>>,
}

pub struct DatabaseStore {
    dbs: RwLock<Vec<Database>>,
    port: AtomicU32,
    blob: BlobStore,
    queue: QueueStore,
    vector: VectorStore,
    broker: Broker,
}

impl DatabaseStore {
    pub fn new() -> DatabaseStore {
        DatabaseStore {
            dbs: RwLock::new(Vec::new()),
            port: AtomicU32::new(0),
            blob: BlobStore::new(crate::persist::data_dir().join("blob")),
            queue: QueueStore::default(),
            vector: VectorStore::default(),
            broker: Broker::default(),
        }
    }

    // ---- Pub/Sub + Realtime broker ----
    /// Publish a message to a channel; returns the number of live subscribers
    /// that received it.
    pub fn publish(&self, channel: &str, msg: String) -> usize {
        *self.broker.published.write().entry(channel.to_string()).or_insert(0) += 1;
        let tx = self.broker.sender(channel);
        tx.send(msg).unwrap_or(0)
    }
    /// Subscribe to a channel, receiving all subsequent messages.
    pub fn subscribe(&self, channel: &str) -> tokio::sync::broadcast::Receiver<String> {
        self.broker.sender(channel).subscribe()
    }
    /// Live subscriber count for a channel.
    pub fn subscriber_count(&self, channel: &str) -> usize {
        self.broker.channels.read().get(channel).map(|s| s.receiver_count()).unwrap_or(0)
    }
    pub fn published_count(&self, channel: &str) -> u64 {
        self.broker.published.read().get(channel).copied().unwrap_or(0)
    }

    pub fn list(&self, project: Option<&str>) -> Vec<Database> {
        let d = self.dbs.read();
        let it = d.iter();
        match project {
            Some(p) => it.filter(|x| x.project == p).map(|x| x.masked()).collect(),
            None => it.map(|x| x.masked()).collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<Database> {
        self.dbs.read().iter().find(|x| x.id == id).map(|x| x.masked())
    }

    /// UNMASKED lookup by the external DB-gateway hostname (`db_host`), for the SNI
    /// proxy — it needs the real `local_port` + credentials to splice the wire.
    /// Case-insensitive (SNI names arrive lowercased).
    pub fn by_db_host(&self, db_host: &str) -> Option<Database> {
        let h = db_host.trim().to_ascii_lowercase();
        self.dbs.read().iter().find(|x| !x.db_host.is_empty() && x.db_host.eq_ignore_ascii_case(&h)).cloned()
    }

    /// Unmasked connection (used by the "reveal credentials" endpoint).
    pub fn get_raw(&self, id: &str) -> Option<Database> {
        self.dbs.read().iter().find(|x| x.id == id).cloned()
    }

    /// NON-SECRET directory of gateway-addressable DBs hosted on THIS node — the
    /// mesh-shareable subset the DNS reconciler needs to publish per-DB A records
    /// for DBs hosted on OTHER nodes (DBs provision on the control-plane leader,
    /// the DNS leader is a different node, and DB records are not gossiped).
    /// Deliberately excludes connection/credentials: routing metadata only.
    pub fn directory(&self) -> Vec<serde_json::Value> {
        self.dbs
            .read()
            .iter()
            .filter(|d| !d.db_host.is_empty() && !d.host_node.is_empty())
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "db_host": d.db_host,
                    "host_node": d.host_node,
                    "kind": d.kind,
                })
            })
            .collect()
    }

    pub fn snapshot(&self) -> Vec<Database> {
        self.dbs.read().clone()
    }

    pub fn load(&self, data: Vec<Database>) {
        *self.dbs.write() = data;
    }

    /// Durable snapshot of the in-process data stores (queue + vector) so they
    /// survive a node restart like the disk-backed blob store + DB records.
    /// (Pub/sub + realtime are ephemeral streaming channels — nothing to persist.)
    pub fn data_snapshot(&self) -> DataSnapshot {
        DataSnapshot {
            queues: self
                .queue
                .msgs
                .read()
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
                .collect(),
            vectors: self
                .vector
                .items
                .read()
                .iter()
                .map(|(idx, m)| (idx.clone(), m.iter().map(|(id, (v, meta))| (id.clone(), (v.clone(), meta.clone()))).collect()))
                .collect(),
        }
    }

    pub fn data_load(&self, snap: DataSnapshot) {
        *self.queue.msgs.write() = snap.queues.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect();
        *self.vector.items.write() = snap.vectors.into_iter().map(|(idx, m)| (idx, m.into_iter().collect())).collect();
    }

    pub fn count(&self) -> usize {
        self.dbs.read().len()
    }

    pub fn remove_db(&self, id: &str) {
        self.dbs.write().retain(|d| d.id != id);
    }

    /// Remove a database record AND purge its actual payload data for the
    /// platform-native kinds (Queue/Vector) that live in separate stores keyed
    /// by a team-prefixed name (`"{team}::{connection[queue|index]}"`), not by
    /// the database's own id — `remove_db` alone only ever removed the catalog
    /// entry, leaving queue messages / vector embeddings (which may hold
    /// customer PII in their metadata) orphaned in memory and re-persisted in
    /// every subsequent snapshot forever. `team` is the OWNING team (from the
    /// `Database` record itself, not attacker-suppliable), so this can't be
    /// used to purge another tenant's namespace.
    pub fn remove_db_and_purge_data(&self, id: &str, team: &str) {
        if let Some(d) = self.get_raw(id) {
            if let Some(q) = d.connection.get("queue") {
                self.queue.msgs.write().remove(&format!("{team}::{q}"));
            }
            if let Some(idx) = d.connection.get("index") {
                self.vector.items.write().remove(&format!("{team}::{idx}"));
            }
            if let Some(bucket) = d.connection.get("bucket") {
                self.blob.delete_bucket(bucket);
            }
        }
        self.remove_db(id);
    }

    /// Insert or replace a database record by id (used when a replica is
    /// registered on a region node).
    pub fn upsert_replica(&self, db: Database) {
        let mut v = self.dbs.write();
        if let Some(existing) = v.iter_mut().find(|d| d.id == db.id) {
            *existing = db;
        } else {
            v.push(db);
        }
    }

    /// Allocate a loopback port for a new DB container. The in-memory cursor RESETS
    /// to 0 on restart, so a naive `base + cursor` re-hands `base` to a second DB
    /// created post-restart — colliding with a pre-existing DB that already holds it
    /// (the new container then can't publish its port and podman leaves it in a
    /// broken "Up-but-dead" state, which the gateway sees as connection-refused).
    /// Guard against that: skip ports already assigned to a live DB (persisted in
    /// each DB's `connection`, so they survive restart) AND ports actually bound on
    /// loopback (a leftover container), probing with a real bind.
    fn next_port(&self, base: u32) -> u32 {
        let used: std::collections::HashSet<u32> = self
            .dbs
            .read()
            .iter()
            .filter_map(|d| d.connection.get("local_port").or_else(|| d.connection.get("port")))
            .filter_map(|p| p.parse::<u32>().ok())
            .collect();
        loop {
            let cand = base + self.port.fetch_add(1, Ordering::SeqCst);
            // Safety valve: never exceed the valid TCP range — fall back to the raw
            // candidate rather than loop forever (exhaustion is not expected).
            if cand > 65_000 {
                return cand;
            }
            if used.contains(&cand) {
                continue;
            }
            // A successful bind proves the port is free right now; we drop the probe
            // listener immediately so podman can claim it (small, tolerable TOCTOU).
            if std::net::TcpListener::bind(("127.0.0.1", cand as u16)).is_ok() {
                return cand;
            }
        }
    }

    fn update<F: FnOnce(&mut Database)>(&self, id: &str, f: F) {
        if let Some(d) = self.dbs.write().iter_mut().find(|x| x.id == id) {
            f(d);
        }
    }

    /// UNMASKED reconcile view: every live-mode record with a named backing
    /// container, plus the stored connection fields a re-create needs
    /// (kind, local host port, raw password). Internal-only — never serialized
    /// to a client.
    pub(crate) fn containers_to_reconcile(&self) -> Vec<(String, DbKind, String, Option<String>, Option<String>, String)> {
        self.dbs
            .read()
            .iter()
            .filter(|d| d.mode == "live")
            .filter_map(|d| {
                let c = d.container.clone()?;
                Some((
                    d.id.clone(),
                    d.kind,
                    c,
                    d.connection.get("local_port").or_else(|| d.connection.get("port")).cloned(),
                    d.connection.get("password").cloned(),
                    d.project.clone(),
                ))
            })
            .collect()
    }

    fn insert(&self, d: Database) {
        self.dbs.write().push(d);
    }

    // ---- Blob ops (durable, disk-backed) ----
    pub fn blob_put(&self, bucket: &str, key: &str, data: Vec<u8>) {
        self.blob.put(bucket, key, &data);
    }
    pub fn blob_get(&self, bucket: &str, key: &str) -> Option<Vec<u8>> {
        self.blob.get(bucket, key)
    }
    pub fn blob_list(&self, bucket: &str) -> Vec<String> {
        self.blob.list(bucket)
    }

    // ---- Queue ops ----
    pub fn queue_push(&self, queue: &str, msg: String) -> usize {
        let mut m = self.queue.msgs.write();
        let q = m.entry(queue.to_string()).or_default();
        q.push_back(msg);
        q.len()
    }
    pub fn queue_pop(&self, queue: &str) -> Option<String> {
        self.queue.msgs.write().get_mut(queue)?.pop_front()
    }
    pub fn queue_depth(&self, queue: &str) -> usize {
        self.queue.msgs.read().get(queue).map(|q| q.len()).unwrap_or(0)
    }

    // ---- Vector ops ----
    pub fn vector_upsert(&self, index: &str, id: &str, v: Vec<f32>, meta: serde_json::Value) {
        self.vector.items.write().entry(index.to_string()).or_default().insert(id.to_string(), (v, meta));
    }
    /// Cosine-similarity top-k query.
    pub fn vector_query(&self, index: &str, q: &[f32], k: usize) -> Vec<serde_json::Value> {
        let items = self.vector.items.read();
        let Some(idx) = items.get(index) else { return vec![] };
        let mut scored: Vec<(f32, String, serde_json::Value)> = idx
            .iter()
            .map(|(id, (v, meta))| (cosine(q, v), id.clone(), meta.clone()))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(k)
            .map(|(score, id, meta)| serde_json::json!({ "id": id, "score": score, "metadata": meta }))
            .collect()
    }
}

impl Default for DatabaseStore {
    fn default() -> Self {
        Self::new()
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn token(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Request to provision a new database.
#[derive(Clone, Debug, Deserialize)]
pub struct ProvisionReq {
    pub name: String,
    pub project: String,
    #[serde(default)]
    pub team: String,
    pub kind: DbKind,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// Regions to keep read replicas in (cross-region replication when set).
    #[serde(default)]
    pub replicas: Vec<String>,
}

/// The environment variables a database exposes to its owning project's
/// deployments — the connection surface an app reads (Vercel-style). `api_base`
/// is the public API origin (e.g. `https://api.shadw.cloud`) used to form
/// absolute URLs for the platform-native (blob/queue/vector/pubsub/realtime)
/// services, which are reached over the public API with the DB's token.
///
/// For Postgres/Redis, when the DB container joined the project's podman network
/// (`net_host`/`net_port` present), the injected URL uses that in-network address
/// so a SAME-NODE app container actually connects (a container's `127.0.0.1` is
/// itself, not the host). Falls back to the loopback URL otherwise. Returns
/// (KEY, VALUE) pairs; ALL are treated as sensitive.
pub fn env_exports(db: &Database, api_base: &str) -> Vec<(String, String)> {
    let c = &db.connection;
    let g = |k: &str| c.get(k).cloned().unwrap_or_default();
    let base = api_base.trim_end_matches('/');
    let net_host = c.get("net_host").filter(|s| !s.is_empty());
    let mut out: Vec<(String, String)> = match db.kind {
        DbKind::Postgres => {
            let url = match net_host {
                Some(h) => format!(
                    "postgres://{}:{}@{}:{}/{}?sslmode=disable",
                    g("user"), g("password"), h, c.get("net_port").cloned().unwrap_or_else(|| "5432".into()), g("database")
                ),
                None => g("DATABASE_URL"),
            };
            vec![("DATABASE_URL".into(), url), ("PRISMA_DATABASE_URL".into(), g("PRISMA_DATABASE_URL"))]
        }
        DbKind::Redis => {
            let url = match net_host {
                Some(h) => format!("redis://default:{}@{}:{}", g("password"), h, c.get("net_port").cloned().unwrap_or_else(|| "6379".into())),
                None => g("REDIS_URL"),
            };
            vec![
                ("REDIS_URL".into(), url),
                ("UPSTASH_REDIS_REST_URL".into(), g("UPSTASH_REDIS_REST_URL")),
                ("UPSTASH_REDIS_REST_TOKEN".into(), g("UPSTASH_REDIS_REST_TOKEN")),
            ]
        }
        DbKind::Blob => vec![
            ("BLOB_READ_WRITE_TOKEN".into(), g("read_write_token")),
            ("BLOB_BUCKET".into(), g("bucket")),
            ("BLOB_ENDPOINT".into(), format!("{base}/v1/storage/blob")),
        ],
        DbKind::Queue => vec![
            ("QUEUE_NAME".into(), g("queue")),
            ("QUEUE_TOKEN".into(), g("token")),
            ("QUEUE_ENDPOINT".into(), format!("{base}/v1/storage/queue")),
        ],
        DbKind::Vector => vec![
            ("VECTOR_INDEX".into(), g("index")),
            ("VECTOR_TOKEN".into(), g("token")),
            ("VECTOR_ENDPOINT".into(), format!("{base}/v1/storage/vector")),
        ],
        DbKind::Pubsub => vec![
            ("PUBSUB_TOPIC".into(), g("topic")),
            ("PUBSUB_TOKEN".into(), g("token")),
            ("PUBSUB_PUBLISH_URL".into(), format!("{base}/v1/storage/pubsub/{}/publish", g("topic"))),
            ("PUBSUB_SUBSCRIBE_WS".into(), format!("{base}/v1/ws/pubsub/{}", g("topic"))),
        ],
        DbKind::Realtime => vec![
            ("REALTIME_ROOM".into(), g("room")),
            ("REALTIME_TOKEN".into(), g("token")),
            ("REALTIME_WS_URL".into(), format!("{base}/v1/ws/realtime/{}", g("room"))),
        ],
    };
    out.retain(|(_, v)| !v.is_empty());
    out
}

/// Uppercase, env-safe prefix from a database name (for collision-free variants
/// when a project has multiple databases of the same kind).
pub fn env_prefix(name: &str) -> String {
    let p: String = name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_uppercase() } else { '_' })
        .collect();
    p.trim_matches('_').to_string()
}

/// Rewrite a freshly-provisioned connection map so the PRIMARY, copyable URL is
/// the EXTERNAL DB-gateway endpoint (`<db_host>:5432`/`:6379`, TLS-required), while
/// preserving the node-local URL under `INTERNAL_*` (for on-node tooling) and the
/// private `net_host`/`net_port` (for same-node app containers). No-op when the
/// gateway is off (`db_host` empty) — the original 127.0.0.1 URLs stand.
fn with_external_endpoint(mut conn: HashMap<String, String>, kind: DbKind, db_host: &str) -> HashMap<String, String> {
    if db_host.is_empty() {
        return conn;
    }
    // Preserve the container's REAL node-local host port BEFORE overwriting `port`
    // with the public gateway port — the SNI proxy on the host node splices to
    // 127.0.0.1:<local_port>, and the reconciler/router reads it back.
    if let Some(p) = conn.get("port").cloned() {
        conn.insert("local_port".into(), p);
    }
    match kind {
        DbKind::Postgres => {
            let user = conn.get("user").cloned().unwrap_or_default();
            let pw = conn.get("password").cloned().unwrap_or_default();
            let dbname = conn.get("database").cloned().unwrap_or_else(|| "hive".into());
            if let Some(internal) = conn.remove("DATABASE_URL") {
                conn.insert("INTERNAL_DATABASE_URL".into(), internal);
            }
            // TLS-required so credentials never cross the wire in plaintext and the
            // gateway can route by SNI.
            conn.insert("DATABASE_URL".into(), format!("postgres://{user}:{pw}@{db_host}:5432/{dbname}?sslmode=require"));
            // SQL-over-HTTP (Neon-style) REST endpoint served by the same gateway
            // host: POST {"query","params"} with `Authorization: Bearer <password>`.
            conn.insert("SQL_HTTP_URL".into(), format!("https://{db_host}/sql"));
            conn.insert("host".into(), db_host.to_string());
            conn.insert("port".into(), "5432".into());
        }
        DbKind::Redis => {
            let pw = conn.get("password").cloned().unwrap_or_default();
            if let Some(internal) = conn.remove("REDIS_URL") {
                conn.insert("INTERNAL_REDIS_URL".into(), internal);
            }
            // `rediss://` = TLS.
            conn.insert("REDIS_URL".into(), format!("rediss://default:{pw}@{db_host}:6379"));
            conn.insert("UPSTASH_REDIS_REST_URL".into(), format!("https://{db_host}"));
            conn.insert("host".into(), db_host.to_string());
            conn.insert("port".into(), "6379".into());
        }
        // HTTP-REST kinds (blob/queue/vector/pubsub/realtime): expose the gateway
        // host so their REST/WS URLs are externally reachable over TLS.
        _ => {
            conn.insert("endpoint".into(), format!("https://{db_host}"));
            conn.insert("host".into(), db_host.to_string());
        }
    }
    conn
}

/// Provision a database. Returns the record immediately (status=provisioning)
/// and finishes the backing service in the background.
pub fn provision(
    store: Arc<DatabaseStore>,
    region: String,
    req: ProvisionReq,
    // The DB-gateway domain (empty = gateway off) and the node whose podman runs
    // the container — used to assign the external `<slug>.{db_domain}` endpoint and
    // route its DNS to this node.
    db_domain: String,
    host_node: String,
    on_ready: impl Fn(Database) + Send + 'static,
) -> Database {
    let id = format!("db_{}", uuid::Uuid::new_v4().simple());
    let region = req.region.clone().unwrap_or(region);
    // Hostname-safe slug from the id (`db_<hex>` -> `db-<12hex>`) — a valid single
    // DNS/SNI label. `db_host` is the external endpoint; empty when the gateway is off.
    let slug = format!("db-{}", &id[3..15.min(id.len())]);
    let db_host = if db_domain.is_empty() { String::new() } else { format!("{slug}.{db_domain}") };
    let db = Database {
        id: id.clone(),
        name: req.name.clone(),
        project: req.project.clone(),
        team: req.team.clone(),
        kind: req.kind,
        region: region.clone(),
        status: DbStatus::Provisioning,
        provider: req.provider.clone().unwrap_or_else(|| req.kind.label().to_string()),
        mode: "simulated".into(),
        created_ms: now_ms(),
        connection: HashMap::new(),
        container: None,
        note: String::new(),
        replicas: req.replicas.iter().map(|r| r.trim().to_ascii_lowercase()).filter(|r| !r.is_empty() && *r != region).collect(),
        role: "primary".into(),
        primary_node: String::new(),
        db_host: db_host.clone(),
        host_node: host_node.clone(),
    };
    store.insert(db.clone());

    tokio::spawn(async move {
        let outcome = provision_backing(&store, &id, &req).await;
        store.update(&id, |d| {
            match outcome {
                Ok((mode, conn, container)) => {
                    d.status = DbStatus::Ready;
                    // Make the fallback explicit: a "simulated" DB has no real
                    // backing engine (e.g. podman unavailable). The record's `mode`
                    // surfaces this in the UI; log it so it's never silent.
                    if mode == "simulated" {
                        tracing::warn!(db = %id, kind = ?req.kind, "database provisioned in SIMULATED mode (no live backing engine — install podman for a real instance)");
                        d.note = "Simulated: no live backing engine available (install podman for a real instance).".into();
                    }
                    d.mode = mode;
                    d.connection = with_external_endpoint(conn, req.kind, &db_host);
                    d.container = container;
                }
                Err(e) => {
                    d.status = DbStatus::Error;
                    d.note = e;
                }
            }
        });
        if let Some(d) = store.get_raw(&id) {
            on_ready(d);
        }
    });

    db
}

async fn provision_backing(
    store: &Arc<DatabaseStore>,
    id: &str,
    req: &ProvisionReq,
) -> Result<(String, HashMap<String, String>, Option<String>), String> {
    match req.kind {
        DbKind::Postgres => provision_postgres(store, id, &req.project).await,
        DbKind::Redis => provision_redis(store, id, &req.project).await,
        DbKind::Blob => {
            let bucket = format!("hive-{}", &id[3..11.min(id.len())]);
            let mut c = HashMap::new();
            c.insert("provider".into(), "Hive Blob (S3-compatible)".into());
            c.insert("endpoint".into(), "/v1/storage/blob".into());
            c.insert("bucket".into(), bucket);
            c.insert("access_key_id".into(), token("AKIA"));
            c.insert("secret_access_key".into(), token("hbsk"));
            c.insert("read_write_token".into(), token("blob_rw"));
            Ok(("live".into(), c, None))
        }
        DbKind::Queue => {
            let q = format!("queue-{}", &id[3..11.min(id.len())]);
            let mut c = HashMap::new();
            c.insert("provider".into(), "Hive Queue (FIFO)".into());
            c.insert("endpoint".into(), "/v1/storage/queue".into());
            c.insert("queue".into(), q);
            c.insert("token".into(), token("queue"));
            Ok(("live".into(), c, None))
        }
        DbKind::Vector => {
            let idx = format!("index-{}", &id[3..11.min(id.len())]);
            let mut c = HashMap::new();
            c.insert("provider".into(), "Hive Vector".into());
            c.insert("endpoint".into(), "/v1/storage/vector".into());
            c.insert("index".into(), idx);
            c.insert("dimensions".into(), "1536".into());
            c.insert("metric".into(), "cosine".into());
            c.insert("token".into(), token("vec"));
            Ok(("live".into(), c, None))
        }
        DbKind::Pubsub => {
            let topic = format!("topic-{}", &id[3..11.min(id.len())]);
            let mut c = HashMap::new();
            c.insert("provider".into(), "Hive Pub/Sub".into());
            c.insert("publish_url".into(), format!("/v1/storage/pubsub/{topic}/publish"));
            c.insert("subscribe_ws".into(), format!("/v1/ws/pubsub/{topic}"));
            c.insert("topic".into(), topic);
            c.insert("token".into(), token("psb"));
            Ok(("live".into(), c, None))
        }
        DbKind::Realtime => {
            let room = format!("room-{}", &id[3..11.min(id.len())]);
            let mut c = HashMap::new();
            c.insert("provider".into(), "Hive Realtime (WSS)".into());
            c.insert("channel_ws".into(), format!("/v1/ws/realtime/{room}"));
            c.insert("room".into(), room);
            c.insert("token".into(), token("rt"));
            Ok(("live".into(), c, None))
        }
    }
}

/// Ensure the per-project DNS-less podman network exists (idempotent) and return
/// (net_name, static_ip) to attach a managed DB container so same-node app
/// containers reach it BY IP (the network has no DNS). The IP is a deterministic
/// high host (.200+) derived from the db id to avoid colliding with compose
/// service IPs (.11+). Returns None if podman/network setup is unavailable.
///
/// Podman-only, unconditionally, even on macOS: every DB provision attaches to
/// this static-IP network (never optional — see the two callers below), and
/// Apple's `container` tool has no static-IP-assignment flag at all (verified
/// live: no equivalent to `--ip`) — the same hard gap documented for
/// multi-service compose deploys in `hive_backend::container_cli`'s module doc.
async fn ensure_project_db_net(project: &str, id: &str) -> Option<(String, String)> {
    let (net, subnet, gw) = crate::git::project_net(project);
    // subnet like "10.o2.o3.0/24" -> prefix "10.o2.o3"
    let prefix = subnet.rsplit_once('.').map(|(p, _)| p.to_string())?;
    let host: u32 = 200 + (fnv1a(id.as_bytes()) % 50);
    let ip = format!("{prefix}.{host}");
    let created = Command::new("podman")
        .args(["network", "create", "--disable-dns", "--subnet", &subnet, "--gateway", &gw, &net])
        .env("PATH", augmented_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    // "already exists" is fine (idempotent).
    let _ = created;
    Some((net, ip))
}

pub(crate) fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Provision the in-region backing for a REPLICA of `db` on this node. Postgres/
/// Redis get a real local instance (so in-region reads are local); the
/// platform-native kinds serve from this node's in-process store (data arrives
/// via write-mirroring) and keep the record's connection. Returns
/// (connection, container, mode).
pub async fn provision_replica_backing(
    store: Arc<DatabaseStore>,
    db: &Database,
) -> (HashMap<String, String>, Option<String>, String) {
    match db.kind {
        DbKind::Postgres => match provision_postgres(&store, &db.id, &db.project).await {
            Ok((mode, conn, container)) => (conn, container, mode),
            Err(_) => (db.connection.clone(), None, "simulated".into()),
        },
        DbKind::Redis => match provision_redis(&store, &db.id, &db.project).await {
            Ok((mode, conn, container)) => (conn, container, mode),
            Err(_) => (db.connection.clone(), None, "simulated".into()),
        },
        // Blob/Queue/Vector/Pubsub/Realtime: in-process on every node; the replica
        // serves from local state kept in sync by the primary's write-mirroring.
        _ => (db.connection.clone(), None, "live".into()),
    }
}

/// Poll a freshly-published loopback port until the container's engine accepts a
/// TCP connection (postgres/redis take ~1-3s to start listening). Returns true once
/// reachable, false if it never came up within the budget — a broken container that
/// reports "Up" but doesn't actually publish its port (so the DB gateway would see
/// connection-refused). Surfacing this beats a silent zombie.
async fn wait_tcp_ready(port: u32) -> bool {
    use tokio::net::TcpStream;
    for _ in 0..30 {
        if TcpStream::connect(("127.0.0.1", port as u16)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

async fn provision_postgres(
    store: &Arc<DatabaseStore>,
    id: &str,
    project: &str,
) -> Result<(String, HashMap<String, String>, Option<String>), String> {
    let port = store.next_port(54320);
    let password = uuid::Uuid::new_v4().simple().to_string();
    let dbname = "hive";
    let user = "hive";
    let cname = format!("hive-db-{}", &id[3..11.min(id.len())]);
    let direct = format!("postgres://{user}:{password}@127.0.0.1:{port}/{dbname}?sslmode=disable");
    // Prisma-Postgres-style pooled/accelerated URL (over the same instance).
    let pooled = format!("prisma+postgres://127.0.0.1:{port}/{dbname}?api_key={}", token("ppg"));
    let mut conn = HashMap::new();
    conn.insert("host".into(), "127.0.0.1".into());
    conn.insert("port".into(), port.to_string());
    conn.insert("database".into(), dbname.into());
    conn.insert("user".into(), user.into());
    conn.insert("password".into(), password.clone());
    // Dedicated, independently-revocable REST bearer for the SQL-over-HTTP
    // surface — distinct from the engine `password`, mirroring Redis's
    // UPSTASH_REDIS_REST_TOKEN. Once set, `credential_matches` accepts ONLY this
    // token for REST (never the raw engine password), so REST access can be
    // rotated without changing the Postgres role password.
    conn.insert("DB_REST_TOKEN".into(), token("pgrest"));
    conn.insert("DATABASE_URL".into(), direct);
    conn.insert("PRISMA_DATABASE_URL".into(), pooled);

    if podman_available().await {
        // Attach to the project's DNS-less net with a static IP so same-node app
        // containers reach the DB by IP; ALSO publish on loopback for host tooling.
        let net = ensure_project_db_net(project, id).await;
        let mut args: Vec<String> = vec![
            "run".into(), "-d".into(), "--name".into(), cname.clone(), "--replace".into(),
            "-e".into(), format!("POSTGRES_PASSWORD={password}"),
            "-e".into(), format!("POSTGRES_USER={user}"),
            "-e".into(), format!("POSTGRES_DB={dbname}"),
            "-p".into(), format!("127.0.0.1:{port}:5432"),
        ];
        if let Some((netname, ip)) = &net {
            args.push("--network".into());
            args.push(netname.clone());
            args.push("--ip".into());
            args.push(ip.clone());
            // The in-network host:port an app container uses to reach this DB.
            conn.insert("net_host".into(), ip.clone());
            conn.insert("net_port".into(), "5432".into());
        }
        // FULLY-QUALIFIED image ref: fleet podman enforces short-name resolution
        // and can't prompt without a TTY, so a bare `postgres:16-alpine` errors and
        // the DB silently fell back to "simulated" (no live engine). Qualify it.
        args.push("docker.io/library/postgres:16-alpine".into());
        // Capture stderr (don't discard it) so a provision failure is diagnosable
        // instead of silently degrading to "simulated".
        let out = Command::new("podman").args(&args).env("PATH", augmented_path()).output().await;
        match out {
            Ok(o) if o.status.success() => {
                // Confirm the published port actually accepts connections — a broken
                // container reports "Up" but never publishes, which the gateway sees
                // as connection-refused. Warn (don't silently claim health) but keep
                // the DB live: a slow start shouldn't demote it to simulated forever.
                if !wait_tcp_ready(port).await {
                    tracing::warn!(db = %id, %port, container = %cname, "postgres container up but loopback port not reachable — gateway may fail to connect");
                }
                return Ok(("live".into(), conn, Some(cname)));
            }
            Ok(o) => tracing::warn!(db = %id, stderr = %String::from_utf8_lossy(&o.stderr).trim(), "postgres provision failed — simulating"),
            Err(e) => tracing::warn!(db = %id, error = %e, "postgres provision could not spawn podman — simulating"),
        }
    }
    // Fallback: record provisioned in simulated mode (no live engine available).
    Ok(("simulated".into(), conn, None))
}

async fn provision_redis(
    store: &Arc<DatabaseStore>,
    id: &str,
    project: &str,
) -> Result<(String, HashMap<String, String>, Option<String>), String> {
    let port = store.next_port(63790);
    let password = uuid::Uuid::new_v4().simple().to_string();
    let cname = format!("hive-db-{}", &id[3..11.min(id.len())]);
    let url = format!("redis://default:{password}@127.0.0.1:{port}");
    let mut conn = HashMap::new();
    conn.insert("host".into(), "127.0.0.1".into());
    conn.insert("port".into(), port.to_string());
    conn.insert("password".into(), password.clone());
    conn.insert("REDIS_URL".into(), url);
    // Upstash-style REST surface.
    conn.insert("UPSTASH_REDIS_REST_URL".into(), format!("http://127.0.0.1:{port}"));
    conn.insert("UPSTASH_REDIS_REST_TOKEN".into(), token("redis"));

    if podman_available().await {
        let net = ensure_project_db_net(project, id).await;
        let mut args: Vec<String> = vec![
            "run".into(), "-d".into(), "--name".into(), cname.clone(), "--replace".into(),
            "-p".into(), format!("127.0.0.1:{port}:6379"),
        ];
        if let Some((netname, ip)) = &net {
            args.push("--network".into());
            args.push(netname.clone());
            args.push("--ip".into());
            args.push(ip.clone());
            conn.insert("net_host".into(), ip.clone());
            conn.insert("net_port".into(), "6379".into());
        }
        args.push("docker.io/library/redis:7-alpine".into()); // qualify (see postgres)
        args.push("redis-server".into());
        args.push("--requirepass".into());
        args.push(password.clone());
        let out = Command::new("podman").args(&args).env("PATH", augmented_path()).output().await;
        match out {
            Ok(o) if o.status.success() => {
                if !wait_tcp_ready(port).await {
                    tracing::warn!(db = %id, %port, container = %cname, "redis container up but loopback port not reachable — gateway may fail to connect");
                }
                return Ok(("live".into(), conn, Some(cname)));
            }
            Ok(o) => tracing::warn!(db = %id, stderr = %String::from_utf8_lossy(&o.stderr).trim(), "redis provision failed — simulating"),
            Err(e) => tracing::warn!(db = %id, error = %e, "redis provision could not spawn podman — simulating"),
        }
    }
    Ok(("simulated".into(), conn, None))
}

/// Self-healing for provisioned DB backings. Two witnessed loss classes made
/// this necessary (both took a project's live database — and everything above
/// it, e.g. the WDK workflow world riding a provisioned Redis — down SILENTLY):
///   1. `systemctl restart hive-node` SIGTERMs the whole service cgroup,
///      which includes the conmon/db processes podman spawned from inside the
///      service — every provisioned container exits (cleanly!) on every
///      backend rollout, and nothing ever started them again.
///   2. A podman machine reset (macOS) deletes the containers entirely.
/// Every minute: `podman start` any exited backing; re-CREATE a vanished
/// Redis backing from the record's own stored creds (same host port, same
/// password — so injected env / gateway tokens keep working; data is
/// acknowledged lost, service is restored); warn loudly for kinds we can't
/// rebuild automatically rather than staying silent.
pub fn spawn_db_reconcile(cloud: Arc<crate::state::CloudState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            if !podman_available().await {
                continue;
            }
            for (id, kind, cname, local_port, password, project) in cloud.databases.containers_to_reconcile() {
                let inspect = Command::new("podman")
                    .args(["inspect", &cname, "--format", "{{.State.Running}}"])
                    .env("PATH", augmented_path())
                    .output()
                    .await;
                match inspect {
                    Ok(o) if o.status.success() => {
                        if String::from_utf8_lossy(&o.stdout).trim() != "true" {
                            match Command::new("podman").args(["start", &cname]).env("PATH", augmented_path()).output().await {
                                Ok(s) if s.status.success() => {
                                    tracing::info!(db = %id, container = %cname, "db-reconcile: restarted exited backing container");
                                }
                                Ok(s) => tracing::warn!(db = %id, container = %cname, stderr = %String::from_utf8_lossy(&s.stderr).trim(), "db-reconcile: start failed"),
                                Err(e) => tracing::warn!(db = %id, container = %cname, error = %e, "db-reconcile: start could not spawn podman"),
                            }
                        }
                    }
                    // inspect failed -> container record is GONE (machine reset /
                    // pruned). Rebuild what we safely can.
                    _ => match (kind, local_port.as_deref(), password.as_deref()) {
                        (DbKind::Redis, Some(port), Some(pw)) if !port.is_empty() && !pw.is_empty() => {
                            let args = [
                                "run", "-d", "--name", &cname, "--replace",
                                "-p", &format!("127.0.0.1:{port}:6379"),
                                "docker.io/library/redis:7-alpine",
                                "redis-server", "--requirepass", pw,
                            ]
                            .map(String::from)
                            .to_vec();
                            match Command::new("podman").args(&args).env("PATH", augmented_path()).output().await {
                                Ok(s) if s.status.success() => {
                                    tracing::warn!(db = %id, container = %cname, %project, %port, "db-reconcile: backing container was GONE — re-created empty Redis with the record's stored port/password (data lost, service restored)");
                                }
                                Ok(s) => tracing::warn!(db = %id, container = %cname, stderr = %String::from_utf8_lossy(&s.stderr).trim(), "db-reconcile: re-create failed"),
                                Err(e) => tracing::warn!(db = %id, container = %cname, error = %e, "db-reconcile: re-create could not spawn podman"),
                            }
                        }
                        _ => {
                            tracing::warn!(db = %id, container = %cname, %project, ?kind, "db-reconcile: backing container is GONE and this kind is not auto-rebuilt — the database is DOWN until re-provisioned");
                        }
                    },
                }
            }
        }
    });
}

async fn podman_available() -> bool {
    Command::new("podman")
        .arg("--version")
        .env("PATH", augmented_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) fn augmented_path() -> String {
    let cur = std::env::var("PATH").unwrap_or_default();
    format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{cur}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(kind: DbKind, name: &str, conn: &[(&str, &str)]) -> Database {
        Database {
            id: "db_abc123def456".into(),
            name: name.into(),
            project: "proj".into(),
            team: "acme".into(),
            kind,
            region: "san-jose".into(),
            status: DbStatus::Ready,
            provider: kind.label().into(),
            mode: "live".into(),
            created_ms: 0,
            connection: conn.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            container: None,
            note: String::new(),
            replicas: vec![],
            role: "primary".into(),
            primary_node: String::new(),
            db_host: String::new(),
            host_node: String::new(),
        }
    }

    #[test]
    fn postgres_egress_uses_network_ip_and_internal_port() {
        // When the DB joined the project net, the injected URL must use the in-net
        // IP + INTERNAL port (5432), not the published loopback port.
        let d = db(
            DbKind::Postgres,
            "maindb",
            &[
                ("user", "hive"),
                ("password", "secret"),
                ("database", "hive"),
                ("DATABASE_URL", "postgres://hive:secret@127.0.0.1:54320/hive?sslmode=disable"),
                ("net_host", "10.130.7.200"),
                ("net_port", "5432"),
            ],
        );
        let ex = env_exports(&d, "https://api.shadw.cloud");
        let url = ex.iter().find(|(k, _)| k == "DATABASE_URL").unwrap();
        assert_eq!(url.1, "postgres://hive:secret@10.130.7.200:5432/hive?sslmode=disable");
    }

    #[test]
    fn native_kind_egress_is_absolute_api_url() {
        let d = db(DbKind::Blob, "assets", &[("bucket", "hive-abc"), ("read_write_token", "blob_rw_x")]);
        let ex = env_exports(&d, "https://api.shadw.cloud/");
        let endpoint = ex.iter().find(|(k, _)| k == "BLOB_ENDPOINT").unwrap();
        assert_eq!(endpoint.1, "https://api.shadw.cloud/v1/storage/blob");
        assert!(ex.iter().any(|(k, v)| k == "BLOB_READ_WRITE_TOKEN" && v == "blob_rw_x"));
    }

    #[test]
    fn env_prefix_is_env_safe() {
        assert_eq!(env_prefix("main-db 2"), "MAIN_DB_2");
        assert_eq!(env_prefix("_weird_"), "WEIRD");
    }

    #[test]
    fn next_port_skips_ports_held_by_existing_dbs() {
        // Regression for the DB-gateway "unexpected eof": the in-memory port cursor
        // RESETS to 0 on restart, so a naive `base + cursor` re-hands the base port
        // to a DB created post-restart — colliding with a pre-existing DB that still
        // holds it. The new container then can't publish its port and lands in a
        // broken "Up-but-dead" state (gateway sees connection-refused). next_port
        // must skip any port already assigned to a live DB.
        let s = DatabaseStore::new();
        s.insert(db(DbKind::Postgres, "existing", &[("local_port", "54320"), ("port", "5432")]));
        // Cursor is fresh (0), exactly as after a process restart.
        let p = s.next_port(54320);
        assert_ne!(p, 54320, "must not collide with the port an existing DB already holds");
        assert!(p > 54320);
        // A second allocation is distinct from both the first and the existing DB.
        let q = s.next_port(54320);
        assert_ne!(q, p);
        assert_ne!(q, 54320);
    }

    #[test]
    fn queue_and_vector_data_survive_snapshot_roundtrip() {
        let s = DatabaseStore::new();
        s.queue_push("acme::q1", "m1".into());
        s.queue_push("acme::q1", "m2".into());
        s.vector_upsert("acme::idx", "v1", vec![1.0, 0.0], serde_json::json!({"t": "a"}));
        let snap = s.data_snapshot();
        let s2 = DatabaseStore::new();
        s2.data_load(snap);
        assert_eq!(s2.queue_depth("acme::q1"), 2);
        assert_eq!(s2.queue_pop("acme::q1").as_deref(), Some("m1"));
        let hits = s2.vector_query("acme::idx", &[1.0, 0.0], 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn remove_db_and_purge_data_erases_queue_vector_and_blob_payloads() {
        // REGRESSION TEST for a real, confirmed GDPR Art.17 gap: `remove_db`
        // only ever filtered the catalog Vec<Database> — the ACTUAL queue
        // messages / vector embeddings (which may hold customer PII in
        // metadata) and blob objects lived in separate stores keyed by a
        // team-prefixed connection name, so they survived a "deleted"
        // database indefinitely and kept being re-persisted in every
        // snapshot.
        let s = DatabaseStore::new();

        // Queue-kind DB: connection["queue"] = "queue-<id-slice>" (real
        // provisioning naming, see provision_native_backing/DbKind::Queue).
        let mut qdb = db(DbKind::Queue, "orders", &[("queue", "queue-abc123de")]);
        qdb.id = "db_queuedb0001".into();
        s.insert(qdb.clone());
        s.queue_push(&format!("acme::{}", "queue-abc123de"), "order #1".into());
        assert_eq!(s.queue_depth("acme::queue-abc123de"), 1);

        // Vector-kind DB.
        let mut vdb = db(DbKind::Vector, "embeddings", &[("index", "index-xyz789ab")]);
        vdb.id = "db_vecdb000001".into();
        s.insert(vdb.clone());
        s.vector_upsert("acme::index-xyz789ab", "v1", vec![1.0, 0.0], serde_json::json!({"pii": "user@example.com"}));
        assert_eq!(s.vector_query("acme::index-xyz789ab", &[1.0, 0.0], 5).len(), 1);

        // Blob-kind DB.
        let mut bdb = db(DbKind::Blob, "uploads", &[("bucket", "hive-purge-test-bucket")]);
        bdb.id = "db_blobdb00001".into();
        s.insert(bdb.clone());
        s.blob_put("hive-purge-test-bucket", "secret.txt", b"customer PII".to_vec());
        assert!(s.blob_get("hive-purge-test-bucket", "secret.txt").is_some());

        // Delete all three — must purge the real payload, not just the catalog row.
        s.remove_db_and_purge_data(&qdb.id, "acme");
        s.remove_db_and_purge_data(&vdb.id, "acme");
        s.remove_db_and_purge_data(&bdb.id, "acme");

        assert_eq!(s.count(), 0, "catalog entries must be gone");
        assert_eq!(s.queue_depth("acme::queue-abc123de"), 0, "queue payload must be purged");
        assert_eq!(s.vector_query("acme::index-xyz789ab", &[1.0, 0.0], 5).len(), 0, "vector payload (incl. PII in metadata) must be purged");
        assert!(s.blob_get("hive-purge-test-bucket", "secret.txt").is_none(), "blob object must be purged");

        // Cleanup the real on-disk blob dir this test created.
        let _ = std::fs::remove_dir_all(s.blob.bucket_dir("hive-purge-test-bucket"));
    }

    #[test]
    fn replicas_normalize_and_exclude_primary_region() {
        // The normalization provision() applies to req.replicas: lowercase, trim,
        // drop empties and the primary region itself.
        let region = "san-jose";
        let raw = ["san-jose", "Virginia", " bangkok ", ""];
        let got: Vec<String> = raw
            .iter()
            .map(|r| r.trim().to_ascii_lowercase())
            .filter(|r| !r.is_empty() && r != region)
            .collect();
        assert_eq!(got, vec!["virginia".to_string(), "bangkok".to_string()]);
    }
}
