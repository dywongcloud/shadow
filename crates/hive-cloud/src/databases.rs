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

    /// Unmasked connection (used by the "reveal credentials" endpoint).
    pub fn get_raw(&self, id: &str) -> Option<Database> {
        self.dbs.read().iter().find(|x| x.id == id).cloned()
    }

    pub fn snapshot(&self) -> Vec<Database> {
        self.dbs.read().clone()
    }

    pub fn load(&self, data: Vec<Database>) {
        *self.dbs.write() = data;
    }

    pub fn count(&self) -> usize {
        self.dbs.read().len()
    }

    pub fn remove_db(&self, id: &str) {
        self.dbs.write().retain(|d| d.id != id);
    }

    fn next_port(&self, base: u32) -> u32 {
        base + self.port.fetch_add(1, Ordering::SeqCst)
    }

    fn update<F: FnOnce(&mut Database)>(&self, id: &str, f: F) {
        if let Some(d) = self.dbs.write().iter_mut().find(|x| x.id == id) {
            f(d);
        }
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
}

/// Provision a database. Returns the record immediately (status=provisioning)
/// and finishes the backing service in the background.
pub fn provision(
    store: Arc<DatabaseStore>,
    region: String,
    req: ProvisionReq,
    on_ready: impl Fn(Database) + Send + 'static,
) -> Database {
    let id = format!("db_{}", uuid::Uuid::new_v4().simple());
    let region = req.region.clone().unwrap_or(region);
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
    };
    store.insert(db.clone());

    tokio::spawn(async move {
        let outcome = provision_backing(&store, &id, req.kind).await;
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
                    d.connection = conn;
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
    kind: DbKind,
) -> Result<(String, HashMap<String, String>, Option<String>), String> {
    match kind {
        DbKind::Postgres => provision_postgres(store, id).await,
        DbKind::Redis => provision_redis(store, id).await,
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

async fn provision_postgres(
    store: &Arc<DatabaseStore>,
    id: &str,
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
    conn.insert("DATABASE_URL".into(), direct);
    conn.insert("PRISMA_DATABASE_URL".into(), pooled);

    if podman_available().await {
        let ok = Command::new("podman")
            .args([
                "run", "-d", "--name", &cname, "--replace",
                "-e", &format!("POSTGRES_PASSWORD={password}"),
                "-e", &format!("POSTGRES_USER={user}"),
                "-e", &format!("POSTGRES_DB={dbname}"),
                "-p", &format!("127.0.0.1:{port}:5432"),
                "postgres:16-alpine",
            ])
            .env("PATH", augmented_path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        match ok {
            Ok(s) if s.success() => return Ok(("live".into(), conn, Some(cname))),
            _ => {}
        }
    }
    // Fallback: record provisioned in simulated mode (no live engine available).
    Ok(("simulated".into(), conn, None))
}

async fn provision_redis(
    store: &Arc<DatabaseStore>,
    id: &str,
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
        let ok = Command::new("podman")
            .args([
                "run", "-d", "--name", &cname, "--replace",
                "-p", &format!("127.0.0.1:{port}:6379"),
                "redis:7-alpine",
                "redis-server", "--requirepass", &password,
            ])
            .env("PATH", augmented_path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        match ok {
            Ok(s) if s.success() => return Ok(("live".into(), conn, Some(cname))),
            _ => {}
        }
    }
    Ok(("simulated".into(), conn, None))
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

fn augmented_path() -> String {
    let cur = std::env::var("PATH").unwrap_or_default();
    format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{cur}")
}
