//! The storage plane: WHERE a managed database's bytes live, named independently of WHO serves it.
//!
//! **The coupling this module exists to remove.** Before it, "where are the bytes" and "who may run
//! the engine" were the same question, answered by one field: `Database.host_node` doubles as the
//! node whose `$HIVE_DATA/sqlite-dbs/` holds the file, the node whose podman holds the container or
//! loopback port, the node the `<slug>.{db_domain}` A record is published from, AND the node whose
//! `db_gateway` splices `127.0.0.1:<local_port>`. That is why a database could never be served by a
//! node that does not hold it, and why a host leaving `registry.nodes()` stranded it. Every answer
//! was `db.host_node == cloud.node_name` re-derived in eight files (22 lines).
//!
//! **The separation.** A [`StorageRef`] names the storage side only — `{node, kind, addr, epoch}` —
//! and `host_node` is left to mean what it now means operationally: where the DNS/edge currently
//! points. They are read separately, so they can differ: `connection["storage_node"]` overrides
//! `host_node` as the byte holder, and a compute tier that is not the storage node attaches over the
//! network instead of concluding the database does not exist. Nothing here moves bytes.
//!
//! **Single writer is unchanged and now stated.** Storage is still ONE node per database, and only
//! that node's supervised engine opens the file (`musql::supervise`, `sqlite_pool`); compute never
//! opens bytes it does not hold. `epoch` is the fence: when the lease moves, every capability minted
//! under the old epoch stops verifying, so stale compute is refused rather than served.
//!
//! **The attach capability.** Cross-node attach is a new attack surface, so it carries an
//! HMAC-SHA256 capability over `(database id, tenant, epoch)` under a subkey of the fleet-shared
//! `HIVE_SECRET_KEY` — the same primitive family as `studio_mac`/`artifact_sig`, never a new
//! algorithm. It is a MAC over identifiers: no credential is carried, echoed or logged, so nothing
//! here can leak one. Tenancy comes from the record's stored tag (`admin::record_tenant`, never a
//! request header), and an untagged record has no tenant to pin to, so it is unattachable rather
//! than attachable-by-anyone. Comparisons are constant-time (`admin::ct_eq`).
//!
//! **Not achieved here (stated, not implied).** Storage is addressed independently but not yet
//! *relocated* automatically: bytes still live on one node, are not replicated, and nothing here
//! re-hosts a stranded database (that would yield an EMPTY database under the same slug — the
//! documented `db:stranded-host` class). Postgres/Redis/Supabase are named by [`StorageKind`] but
//! still served through the loopback splice; only the SQLite lane attaches off-node today.

use crate::admin::{ct_eq, hmac_sha256_hex, record_tenant, UNTAGGED_TENANT};
use crate::databases::{Database, DbKind};
use crate::state::CloudState;
use serde_json::{json, Value};
use std::sync::Arc;

/// `connection` key naming the node that holds the bytes when it is NOT the node the DNS/edge points
/// at. Written only by an explicit storage move; absent means "still `host_node`", so every existing
/// record resolves exactly as it did before this module existed.
pub const STORAGE_NODE_KEY: &str = "storage_node";

/// `connection` key holding the fencing epoch (`0` when absent = never moved).
pub const STORAGE_EPOCH_KEY: &str = "storage_epoch";

/// Envelope field carrying the attach capability on the SQLite owner hop.
pub const ATTACH_FIELD: &str = "storage_key";

/// Domain separation for the subkey, and for the capability message itself.
const KEY_DOMAIN: &[u8] = b"hive-storage-plane-v1";
const CAP_DOMAIN: &str = "hive-storage-attach-v1";

/// How the storage node exposes one database's bytes to a compute tier. Musql and RusqliteFile are
/// two engines over the same file; LoopbackPort is a container or listener published on `127.0.0.1`
/// of the storage node only (Postgres, Supabase, Redis).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageKind {
    Musql,
    RusqliteFile,
    LoopbackPort,
}

impl StorageKind {
    pub fn label(&self) -> &'static str {
        match self {
            StorageKind::Musql => "musql",
            StorageKind::RusqliteFile => "rusqlite-file",
            StorageKind::LoopbackPort => "loopback-port",
        }
    }
}

/// Where a database's bytes live. `addr` is meaningful ONLY on `node` — it is the storage node's own
/// attach address (loopback for every kind today), never an address a third node dials directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageRef {
    pub node: String,
    pub kind: StorageKind,
    pub addr: String,
    pub epoch: u64,
}

/// How this node relates to a database's storage: it holds the bytes, it can attach across the mesh,
/// the storage node is unknown to this node's registry (STRANDED: reported, never re-hosted), or the
/// record has no storage plane at all (the platform-native kinds, served in-process anywhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Local,
    Remote,
    Stranded,
    Unreachable,
}

impl Reach {
    pub fn label(&self) -> &'static str {
        match self {
            Reach::Local => "local",
            Reach::Remote => "remote",
            Reach::Stranded => "stranded",
            Reach::Unreachable => "unreachable",
        }
    }
}

/// Which kinds sit on the storage plane at all. Blob/Queue/Vector/Pubsub/Realtime are
/// platform-native: every node holds its own copy and serves it in-process, so "where the bytes
/// live" is not one node and there is nothing to attach to — `None` means "no single holder", NOT
/// "no bytes".
fn kind_of(db: &Database) -> Option<StorageKind> {
    match db.kind {
        DbKind::Sqlite => Some(match crate::musql::engine_for_db(&db.connection) {
            crate::musql::Engine::Musql => StorageKind::Musql,
            crate::musql::Engine::Rusqlite => StorageKind::RusqliteFile,
        }),
        DbKind::Postgres | DbKind::Supabase | DbKind::Redis => Some(StorageKind::LoopbackPort),
        _ => None,
    }
}

/// The node holding the bytes: an explicit `storage_node` when a move wrote one, else `host_node`.
/// This is the one place that question is answered.
pub fn storage_node(db: &Database) -> &str {
    match db.connection.get(STORAGE_NODE_KEY).map(|s| s.trim()) {
        Some(s) if !s.is_empty() => s,
        _ => db.host_node.as_str(),
    }
}

/// The fencing epoch: `0` until a storage move bumps it.
pub fn storage_epoch(db: &Database) -> u64 {
    db.connection
        .get(STORAGE_EPOCH_KEY)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// The storage node's OWN attach address for this database. Loopback for every kind today, which is
/// the point: a remote compute tier reaches it by asking that node, never by dialing it.
fn storage_addr(db: &Database, kind: StorageKind) -> String {
    match kind {
        StorageKind::Musql => crate::musql::local_attach_addr(),
        StorageKind::RusqliteFile => db
            .connection
            .get("file")
            .cloned()
            .unwrap_or_else(|| crate::databases::sqlite_store_dir().to_string_lossy().into_owned()),
        StorageKind::LoopbackPort => format!(
            "127.0.0.1:{}",
            db.connection
                .get("local_port")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("0")
        ),
    }
}

/// The storage reference, or `None` for a kind with no single holder (see [`on_storage_plane`]).
pub fn storage_ref(db: &Database) -> Option<StorageRef> {
    let kind = kind_of(db)?;
    let node = storage_node(db).trim();
    if node.is_empty() {
        return None;
    }
    Some(StorageRef {
        node: node.to_string(),
        kind,
        addr: storage_addr(db, kind),
        epoch: storage_epoch(db),
    })
}

/// Whether the bytes are on THIS node. The replacement for the smeared `host_node == node_name`
/// comparisons: an empty holder counts as 'not us', so a record with no storage node is never
/// answered from this node's own (possibly empty) copy.
pub fn is_storage_node(cloud: &Arc<CloudState>, db: &Database) -> bool {
    matches!(reach(cloud, db), Reach::Local)
}

/// How this node can serve `db`. `Stranded` is the reported-and-never-re-hosted class
/// (`db:stranded-host`): the record names a holder this node's registry no longer lists, so there is
/// no address to attach to. It is deliberately distinct from `Unreachable` (no storage plane, or no
/// holder named at all): the two have different fixes and must not collapse into one 421.
pub fn reach(cloud: &Arc<CloudState>, db: &Database) -> Reach {
    let Some(r) = storage_ref(db) else {
        return Reach::Unreachable;
    };
    if r.node == cloud.node_name {
        return Reach::Local;
    }
    if cloud.registry.nodes().iter().any(|n| n.name == r.node) {
        Reach::Remote
    } else {
        Reach::Stranded
    }
}

/// Operator view of one database's storage side, so the plane is visible on
/// `/v1/databases/sqlite-engines` rather than inferred from `host_node`.
pub fn storage_json(cloud: &Arc<CloudState>, db: &Database) -> Value {
    match storage_ref(db) {
        None => json!({ "on_storage_plane": false, "reach": Reach::Unreachable.label() }),
        Some(r) => json!({
            "on_storage_plane": true,
            "node": r.node,
            "kind": r.kind.label(),
            "addr": r.addr,
            "epoch": r.epoch,
            "reach": reach(cloud, db).label(),
            "separated_from_edge": r.node != db.host_node,
        }),
    }
}

/// The fleet-shared subkey, derived (never stored) from `HIVE_SECRET_KEY` with its own domain, or
/// `None` when this node has no fleet key. `None` is a REFUSAL to attach across nodes, never a
/// fallback to a per-node key: two nodes that disagree on the key would mint capabilities neither
/// exists to prevent. Resolved once and cached: this sits on every Hrana request (mint on the
/// compute side, verify on the storage side) and the process environment never changes under us.
fn plane_key() -> Option<&'static String> {
    static KEY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        // Operator lever only: with this set, cross-node attach presents no capability and the
        // storage node stops requiring one. It disables NOTHING else — the mesh gate, the tenant
        // bearer re-check and the storage-node check all still stand.
        if std::env::var("HIVE_STORAGE_ATTACH_AUTH")
            .ok()
            .map(|v| v.trim() == "0")
            .unwrap_or(false)
        {
            return None;
        }
        let hex = std::env::var("HIVE_SECRET_KEY").ok()?;
        let hex = hex.trim();
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(hmac_sha256_hex(hex.as_bytes(), KEY_DOMAIN))
    })
    .as_ref()
}

/// The attach capability for one database: HMAC-SHA256 over `(id, tenant, epoch)`, bound to all
/// three. Length-delimited with a leading version so no two field splits collide. `None` when this
/// node cannot mint one (no fleet key, or an untagged record with no tenant to pin to): either way
/// the caller must refuse the attach rather than send an empty capability.
pub fn attach_capability(db: &Database) -> Option<String> {
    let tenant = record_tenant(&db.team);
    if tenant == UNTAGGED_TENANT {
        return None;
    }
    let key = plane_key()?;
    let msg = format!(
        "{}|{}|{}|{}",
        CAP_DOMAIN,
        db.id,
        tenant,
        storage_epoch(db)
    );
    Some(hmac_sha256_hex(key.as_bytes(), msg.as_bytes()))
}

/// Whether an attach presented to the STORAGE node is acceptable. The value is recomputed from THIS
/// node's own replicated record — the caller's claim is never trusted for id, tenant or epoch — and
/// compared in constant time.
///
/// `NotApplicable` (this node cannot derive the fleet key) is the honest "cannot verify"; callers
/// keep the existing mesh-gate and bearer checks and do not invent a weaker one. `Refused` is a
/// fixed sentence: nothing here echoes the presented value, the record, or a credential.
pub enum Verdict {
    Ok,
    Refused,
    NotApplicable,
}

pub fn verify_attach(db: &Database, presented: Option<&str>) -> Verdict {
    if plane_key().is_none() {
        return Verdict::NotApplicable;
    }
    let expected = match attach_capability(db) {
        Some(c) => c,
        None => return Verdict::Refused,
    };
    match presented {
        Some(p) if !p.is_empty() && ct_eq(p, &expected) => Verdict::Ok,
        _ => Verdict::Refused,
    }
}

/// The refusal sent back when an attach is refused. One fixed sentence for every cause: a distinct
/// message per cause (wrong tenant / stale epoch / forged) would turn this endpoint into an oracle
/// for exactly those facts.
pub const ATTACH_REFUSAL: &str = "this node does not serve that database's storage";
