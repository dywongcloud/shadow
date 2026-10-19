//! Metrics scoped to ONE managed database INSTANCE (one `Database` record), for EVERY `DbKind`.
//!
//! Nothing here is per-project or per-tenant: `/v1/metrics` already counts HTTP requests by tenant,
//! and the SQLite pool/engine routes count files on one node. This module answers a different
//! question — "how big, how busy and how healthy is THIS database" — and answers it in each kind's
//! own units (SQLite files and streams, Postgres `pg_stat_database` counters, Redis `INFO` fields,
//! blob object counts and bytes, queue depth, vector counts, pub/sub subscriber and publish
//! counters). A single SQL-shaped box forced onto Redis or blob would be a lie, so the shape is
//! per-kind and a kind with no honest live gauge says so instead of reporting a healthy zero.
//!
//! # Where the measurement lives (the routing decision)
//!
//! The truth is not in one place for every kind, so there are exactly two placements and each kind
//! picks one by where its bytes physically are:
//!
//! * **Engine kinds — Sqlite, Postgres, Supabase, Redis.** The engine is a FILE or a loopback
//!   container port on `Database::host_node`, so there is exactly one node that can measure it and
//!   every other node would measure nothing (or, for SQLite, an empty second copy of the file).
//!   Counters and gauges therefore live in process memory ON THE OWNER, and
//!   `GET /v1/databases/:id/metrics` is OWNER-ROUTED: a non-owner proxies to `host_node` through
//!   `admin::fetch_from_host` (HTTP admin first, mesh gossip GET fallback, the `hrana-mesh` /
//!   `studio-mesh` shape) and the owner answers `?local=true` authoritatively. A record whose
//!   `host_node` is EMPTY is the `db:stranded-host` class: it reports `reachability: "stranded"` with
//!   `stats: null`, never a zero that reads as healthy.
//!
//! * **Platform-native kinds — Blob, Queue, Vector, Pubsub, Realtime.** Their payload is an
//!   in-process store with no owning node, so there is no owner to route to. The read is LOCAL plus a
//!   fan-out to the healthy peers with `?local=true`, merged the way `/v1/metrics` already merges
//!   (counters summed, gauges maxed), and the answer names every node that contributed in `sources`.
//!   Without the fan-out a blob bucket PUT through the round-robin public host would read zero on
//!   five of six nodes — the exact defect this placement exists to avoid.
//!
//! # Security
//!
//! The record's `connection` map holds `DATABASE_URL`, passwords and tokens. Nothing in this module
//! emits a connection field: the counters carry a fixed ERROR CLASS code, never an engine's error
//! text (which echoes credentials on a connection failure), and no stat is derived from a credential.
//! Authorization is the caller's job (see `admin::database_metrics`) and is per-database, never
//! per-tenant.

use crate::databases::{Database, DbKind};
use crate::state::CloudState;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// One observation at a request boundary. `err` is a FIXED CLASS, never an engine message: a
/// Postgres or Redis connection failure puts the whole DSN (password included) in its error string,
/// and a metrics surface must never be the thing that echoes it into a dashboard or a log.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    None,
    /// The backing engine could not be reached or refused the request.
    Engine,
    /// The caller's request was malformed or refused before reaching the engine.
    Client,
    /// The request reached the engine and came back an error reply.
    Query,
    /// The engine did not answer within the budget.
    Timeout,
    /// A denied command (the Redis gateway's deny list).
    Denied,
}

impl ErrorClass {
    pub fn label(self) -> &'static str {
        match self {
            ErrorClass::None => "",
            ErrorClass::Engine => "engine_unreachable",
            ErrorClass::Client => "bad_request",
            ErrorClass::Query => "query_error",
            ErrorClass::Timeout => "timeout",
            ErrorClass::Denied => "denied",
        }
    }
}

/// Per-instance request counters. Accumulated on the node that SERVED the request, which for the
/// engine kinds is the owner by construction (the DB gateway routes by SNI to `host_node`).
///
/// `Clone`, never `Copy`: `last_error` is a String, and a Copy counter read out of the map would be
/// a second, independently-mutable copy whose `last_error` could disagree with the map's.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Counters {
    pub requests: u64,
    pub errors: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Wall-clock ms of the most recent observation.
    pub last_ms: u64,
    /// Latency of the most recent observation.
    pub last_latency_ms: u64,
    /// Wall-clock ms of the most recent ERROR observation.
    pub last_error_ms: u64,
    #[serde(default)]
    pub last_error: String,
}

impl Counters {
    /// Fleet merge for the platform-native kinds: counters SUM (every node saw part of the traffic)
    /// and last-* MAX (the newest observation anywhere is the newest one). Exactly the direction
    /// `/v1/metrics` merges its peer series.
    fn merge(&mut self, other: &Counters) {
        self.requests += other.requests;
        self.errors += other.errors;
        self.bytes_in += other.bytes_in;
        self.bytes_out += other.bytes_out;
        self.last_ms = self.last_ms.max(other.last_ms);
        self.last_latency_ms = self.last_latency_ms.max(other.last_latency_ms);
        if other.last_error_ms >= self.last_error_ms {
            self.last_error_ms = other.last_error_ms;
            self.last_error = other.last_error.clone();
        }
    }

    fn json(&self) -> Value {
        json!({
            "requests": self.requests,
            "errors": self.errors,
            "bytes_in": self.bytes_in,
            "bytes_out": self.bytes_out,
            "last_ms": self.last_ms,
            "last_latency_ms": self.last_latency_ms,
            "last_error_ms": self.last_error_ms,
            "last_error": self.last_error,
            "error_rate": if self.requests == 0 {
                0.0
            } else {
                self.errors as f64 / self.requests as f64
            },
        })
    }
}

/// Node-local, keyed by database id. Deliberately NOT persisted: these are liveness counters, and a
/// restarted node reporting zero real traffic beats one reporting a stale sum it cannot attribute.
fn counters() -> &'static Mutex<HashMap<String, Counters>> {
    static C: OnceLock<Mutex<HashMap<String, Counters>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Records one request against ONE database instance. `db_id` empty is a no-op: an unattributed
/// observation would land in a bucket no instance owns.
pub fn record(
    db_id: &str,
    ok: bool,
    bytes_in: u64,
    bytes_out: u64,
    latency_ms: u64,
    err: ErrorClass,
) {
    if db_id.is_empty() {
        return;
    }
    let now = hive_core::now_ms();
    let mut m = counters().lock();
    let c = m.entry(db_id.to_string()).or_default();
    c.requests += 1;
    c.bytes_in += bytes_in;
    c.bytes_out += bytes_out;
    c.last_ms = now;
    c.last_latency_ms = latency_ms;
    if !ok {
        c.errors += 1;
        c.last_error_ms = now;
        c.last_error = err.label().to_string();
    }
    // Bound the map: a deleted database's counters are never read again, and an unbounded
    // id-keyed map on a request path is a slow leak.
    if m.len() > 4096 {
        m.retain(|_, v| now.saturating_sub(v.last_ms) < 24 * 60 * 60 * 1000);
    }
}

/// [`record`] for an HTTP-level observation: the error class is derived from the STATUS, never from
/// the response body or the engine's error text, so a connection failure that echoes the DSN cannot
/// reach this surface.
pub fn record_http(db_id: &str, status: u16, latency_ms: u64) {
    let err = if status == 504 {
        ErrorClass::Timeout
    } else if status == 403 {
        ErrorClass::Denied
    } else if (400..500).contains(&status) {
        ErrorClass::Client
    } else if status >= 500 {
        ErrorClass::Engine
    } else {
        ErrorClass::None
    };
    record(db_id, status < 400, 0, 0, latency_ms, err);
}

/// This node's counters for one instance (used by the fan-out's `?local=true` inner hop).
pub fn counters_for(db_id: &str) -> Counters {
    counters().lock().get(db_id).cloned().unwrap_or_default()
}

/// How the numbers in this payload were obtained. A caller must be able to tell "measured" from
/// "not measurable here": a bare zero in `stats` is what let a stranded database read as healthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reach {
    /// Measured on this node, which hosts the instance.
    Local,
    /// Hosted on this node, but the engine did not answer the probe.
    LocalUnreachable,
    /// The record is `mode=simulated`: there is no backing engine to measure.
    Simulated,
    /// The record is still provisioning.
    Provisioning,
    /// No `host_node` at all: the `db:stranded-host` class. Never reported as a zero.
    Stranded,
    /// Read with `local=true` on a node that does not host this instance.
    NotHostedHere,
    /// Answered by the owner node after a proxy hop.
    RemoteOwner,
    /// Platform-native kind: local reading merged with the peer fan-out.
    FleetMerged,
}

impl Reach {
    pub fn label(self) -> &'static str {
        match self {
            Reach::Local => "local",
            Reach::LocalUnreachable => "local_unreachable",
            Reach::Simulated => "simulated",
            Reach::Provisioning => "provisioning",
            Reach::Stranded => "stranded",
            Reach::NotHostedHere => "not_hosted_here",
            Reach::RemoteOwner => "remote_owner",
            Reach::FleetMerged => "fleet_merged",
        }
    }

    /// Whether `stats` carries real measurements.
    pub fn measured(self) -> bool {
        matches!(self, Reach::Local | Reach::RemoteOwner | Reach::FleetMerged)
    }
}

/// True for the kinds whose bytes live on exactly one node (a file or a loopback container port), so
/// a metrics read must be owner-routed rather than merged.
pub fn owner_routed_kind(kind: DbKind) -> bool {
    matches!(
        kind,
        DbKind::Sqlite | DbKind::Postgres | DbKind::Supabase | DbKind::Redis
    )
}

/// The kind-specific gauges for one instance, computed on the node that can see them. `Ok(None)`
/// means "this kind has no live gauge to report here" (as distinct from a probe failure).
async fn kind_stats(cloud: &Arc<CloudState>, db: &Database) -> Result<Option<Value>, ErrorClass> {
    let g = |k: &str| db.connection.get(k).cloned().unwrap_or_default();
    match db.kind {
        DbKind::Sqlite => Ok(Some(sqlite_stats(db))),
        DbKind::Postgres | DbKind::Supabase => {
            let port: u16 = match g("local_port").parse() {
                Ok(p) => p,
                Err(_) => return Ok(None),
            };
            let dbname = g("database");
            let dbname = if dbname.is_empty() { "postgres" } else { &dbname };
            match crate::db_rest::postgres_engine_stats(port, &g("user"), &g("password"), dbname)
                .await
            {
                Ok(v) => Ok(Some(v)),
                Err(_) => Err(ErrorClass::Engine),
            }
        }
        DbKind::Redis => {
            let port: u16 = match g("local_port").parse() {
                Ok(p) => p,
                Err(_) => return Ok(None),
            };
            match crate::db_rest::redis_engine_stats(port, &g("password")).await {
                Ok(v) => Ok(Some(v)),
                Err(_) => Err(ErrorClass::Engine),
            }
        }
        DbKind::Blob => {
            let bucket = g("bucket");
            if bucket.is_empty() {
                return Ok(None);
            }
            let ns = format!("{}::{}", crate::admin::norm(&db.team), bucket);
            let (objects, bytes) = cloud.databases.blob_stats(&ns);
            Ok(Some(json!({
                "bucket": bucket,
                "objects": objects,
                "bytes": bytes,
            })))
        }
        DbKind::Queue => {
            let queue = g("queue");
            if queue.is_empty() {
                return Ok(None);
            }
            let ns = format!("{}::{}", crate::admin::norm(&db.team), queue);
            Ok(Some(json!({
                "queue": queue,
                "depth": cloud.databases.queue_depth(&ns),
            })))
        }
        DbKind::Vector => {
            let index = g("index");
            if index.is_empty() {
                return Ok(None);
            }
            let ns = format!("{}::{}", crate::admin::norm(&db.team), index);
            let (count, dim) = cloud.databases.vector_stats(&ns);
            Ok(Some(json!({
                "index": index,
                "vectors": count,
                "dimensions": dim,
            })))
        }
        DbKind::Pubsub => {
            let topic = g("topic");
            if topic.is_empty() {
                return Ok(None);
            }
            let ns = format!("{}::{}", crate::admin::norm(&db.team), topic);
            Ok(Some(json!({
                "topic": topic,
                "subscribers": cloud.databases.subscriber_count(&ns),
                "published": cloud.databases.published_count(&ns),
            })))
        }
        DbKind::Realtime => {
            let room = g("room");
            if room.is_empty() {
                return Ok(None);
            }
            let ns = format!("{}::{}", crate::admin::norm(&db.team), room);
            Ok(Some(json!({
                "room": room,
                "subscribers": cloud.databases.subscriber_count(&ns),
            })))
        }
    }
}

/// SQLite gauges, read from the FILESYSTEM rather than by opening the database: the engine is either
/// a rusqlite file or a musql `.musq`, an open would contend with the live pool (and a `musql`
/// file is not a SQLite file at all), and everything an operator actually wants — how big is it,
/// which engine owns it, how many live streams — is a fact about the directory and the process.
fn sqlite_stats(db: &Database) -> Value {
    let engine = crate::musql::engine_for_db(&db.connection);
    let primary = match engine {
        crate::musql::Engine::Musql => crate::databases::sqlite_musql_path(&db.id),
        crate::musql::Engine::Rusqlite => crate::databases::sqlite_file_path(&db.id),
    };
    let dir = crate::databases::sqlite_store_dir();
    let stem = crate::databases::sqlite_musql_stem(&db.id);
    let mut bytes = 0u64;
    let mut files: Vec<Value> = Vec::new();
    for suffix in [".db", ".db-wal", ".db-shm", ".musq", ".musq.delta"] {
        let p = dir.join(format!("{stem}{suffix}"));
        if let Ok(meta) = std::fs::metadata(&p) {
            let len = meta.len();
            bytes += len;
            files.push(json!({ "name": format!("{stem}{suffix}"), "bytes": len }));
        }
    }
    json!({
        "engine": engine.label(),
        "engine_file": primary.map(|p| p.display().to_string()).unwrap_or_default(),
        "file_bytes": bytes,
        "files": files,
        "open_streams": crate::hrana::open_streams_for(&db.id),
        "engine_ready": match engine {
            crate::musql::Engine::Musql => crate::musql::ready(),
            crate::musql::Engine::Rusqlite => true,
        },
    })
}

/// The metrics payload for ONE instance as seen from THIS NODE, with no proxying. `reach` says
/// whether `stats` is a measurement or an honest absence.
pub async fn local_payload(cloud: &Arc<CloudState>, db: &Database) -> Value {
    let hosted_here = !db.host_node.is_empty() && db.host_node == cloud.node_name;
    let owner_kind = owner_routed_kind(db.kind);

    let reach = if !matches!(db.status, crate::databases::DbStatus::Ready) {
        Reach::Provisioning
    } else if owner_kind && db.host_node.is_empty() {
        Reach::Stranded
    } else if owner_kind && !hosted_here {
        Reach::NotHostedHere
    } else if db.mode == "simulated" && owner_kind {
        Reach::Simulated
    } else {
        Reach::Local
    };

    let mut stats: Option<Value> = None;
    let mut note: Option<String> = None;
    if reach == Reach::Local {
        match kind_stats(cloud, db).await {
            Ok(v) => {
                stats = v;
                if stats.is_none() {
                    note = Some(format!(
                        "no live gauge for this {} instance on its host node",
                        db.kind.label()
                    ));
                }
            }
            Err(class) => {
                // An unreachable engine is a MEASUREMENT, not a zero: say which class of failure it
                // was and never invent a value.
                note = Some(class.label().to_string());
                stats = None;
                return payload(db, cloud, Reach::LocalUnreachable, stats, note);
            }
        }
    } else {
        note = Some(match reach {
            Reach::Provisioning => "database is still provisioning".to_string(),
            Reach::Stranded => {
                "no host_node: this database is stranded (db:stranded-host) and cannot be measured"
                    .to_string()
            }
            Reach::NotHostedHere => format!(
                "hosted on {}; read it there or drop ?local=true to proxy",
                db.host_node
            ),
            Reach::Simulated => {
                "simulated mode: no backing engine exists to measure".to_string()
            }
            _ => String::new(),
        });
    }
    payload(db, cloud, reach, stats, note)
}

fn payload(
    db: &Database,
    cloud: &Arc<CloudState>,
    reach: Reach,
    stats: Option<Value>,
    note: Option<String>,
) -> Value {
    json!({
        "db_id": db.id,
        "name": db.name,
        "kind": db.kind,
        "kind_label": db.kind.label(),
        "status": db.status,
        "mode": db.mode,
        "host_node": db.host_node,
        "node": cloud.node_name,
        "reachability": reach.label(),
        "measured": reach.measured(),
        "observed_at_ms": hive_core::now_ms(),
        "stats": stats,
        "note": note.unwrap_or_default(),
        "counters": counters_for(&db.id).json(),
    })
}

/// Merges a peer's `?local=true` payload into this node's own (the platform-native fan-out).
/// Gauges are MAXed for cardinalities and SUMmed for byte/rate-like counters, and the contributing
/// node names are unioned so the answer says where it came from.
pub fn merge_peer(db_id: &str, base: &mut Value, peer: &Value, sources: &mut Vec<String>) {
    let Some(node) = peer.get("node").and_then(|n| n.as_str()) else {
        return;
    };
    if !sources.iter().any(|s| s == node) {
        sources.push(node.to_string());
    }
    // A peer that could not measure anything contributes nothing to the gauges; its counters still
    // count, because a request served there really happened.
    if let (Some(base_stats), Some(peer_stats)) = (
        base.get_mut("stats").and_then(|s| s.as_object_mut()),
        peer.get("stats").and_then(|s| s.as_object()),
    ) {
        for (k, v) in peer_stats {
            let Some(pn) = v.as_u64() else { continue };
            match base_stats.get_mut(k).and_then(|b| b.as_u64()) {
                Some(bn) => {
                    if CARDINALITY_GAUGES.contains(&k.as_str()) {
                        // Two nodes holding parts of one bucket is a partition, not a sum: the same
                        // object PUT through two nodes is one object.
                        if pn > bn {
                            base_stats.insert(k.clone(), json!(pn));
                        }
                    } else {
                        base_stats.insert(k.clone(), json!(bn.saturating_add(pn)));
                    }
                }
                None => {
                    base_stats.insert(k.clone(), json!(pn));
                }
            }
        }
    }
    if let Some(pc) = peer.get("counters").and_then(|c| c.as_object()) {
        let mut mine = counters_for(db_id);
        mine.merge(&Counters {
            requests: pc.get("requests").and_then(|v| v.as_u64()).unwrap_or(0),
            errors: pc.get("errors").and_then(|v| v.as_u64()).unwrap_or(0),
            bytes_in: pc.get("bytes_in").and_then(|v| v.as_u64()).unwrap_or(0),
            bytes_out: pc.get("bytes_out").and_then(|v| v.as_u64()).unwrap_or(0),
            last_ms: pc.get("last_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            last_latency_ms: pc
                .get("last_latency_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            last_error_ms: pc.get("last_error_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            last_error: pc
                .get("last_error")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        });
        if let Some(bc) = base.get_mut("counters").and_then(|c| c.as_object_mut()) {
            for (k, v) in mine.json().as_object().cloned().unwrap_or_default() {
                bc.insert(k, v);
            }
        }
    }
}

/// Gauges that name HOW MANY distinct things exist, so a fleet merge takes the largest shard rather
/// than summing partitions into a number no node ever held.
const CARDINALITY_GAUGES: &[&str] = &["objects", "vectors", "depth", "dimensions", "subscribers"];

/// The mesh (gossip GET) half of the owner proxy: re-resolves the record, re-runs the tenant check
/// against THIS node's own replicated state, and answers the LOCAL reading only (never re-proxies,
/// so a stale router cannot build a forwarding loop). An empty tenant — an unsigned or expired
/// delegation token — resolves nothing, the same posture as every other mesh arm.
///
/// There is deliberately no operator override here: the mesh envelope carries only a tenant string,
/// so an "operator" flag on it would be forgeable by any peer. An operator reading a database hosted
/// elsewhere proxies with the DATABASE'S OWN tenant as the carried team (see
/// `admin::database_metrics`), which the owner re-verifies — no new privilege channel.
pub async fn mesh_serve(cloud: &Arc<CloudState>, id: &str, tenant: &str) -> Value {
    if tenant.is_empty() {
        return json!({ "error": "unauthorized" });
    }
    let Some(db) = cloud.databases.get_raw(id) else {
        return json!({ "error": "not_found" });
    };
    if crate::admin::norm(crate::admin::record_tenant(&db.team)) != crate::admin::norm(tenant) {
        return json!({ "error": "not_found" });
    }
    let mut v = local_payload(cloud, &db).await;
    if let Some(o) = v.as_object_mut() {
        o.insert("local".into(), json!(true));
    }
    v
}
