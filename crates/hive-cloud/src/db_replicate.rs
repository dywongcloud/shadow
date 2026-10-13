//! Cross-region database replication.
//!
//! A database provisioned with `replicas: [<region>, ...]` has its PRIMARY node (where the writable
//! instance lives) fan it out to one node in each replica region, so reads are served locally in
//! every configured region:
//!
//! * **Record and egress replication**: the [`Database`] record is registered on each replica node
//! (role `replica`), so it appears in that region's dashboard and its connection env is
//! auto-injected into the owning project's deployments there too.
//! * **Platform-native kinds** (blob, queue, vector, pub/sub, realtime): the primary MIRRORS every
//! write to each replica node over the authenticated mesh ([`on_write`]); an `x-hive-mirror` header
//! breaks the loop, so a mirrored write is applied locally and never re-mirrored. Each region
//! serves reads from its own local copy.
//! * **Postgres and Redis**: the replica node provisions a real in-region backing container. Native
//! physical streaming (`replicaof` / hot-standby) is enabled when the primary is reachable from the
//! replica (a public endpoint); across NAT without that, the replica is an independent in-region
//! instance and the limitation is logged rather than silently pretended.
//!
//! Transport reuses the mesh: the HTTP admin URL when known, else an iroh request to the peer, the
//! same dual path the deploy fanout uses.

use std::sync::Arc;

use hive_edge::NodeInfo;

use crate::databases::{Database, DbKind};
use crate::schedule::Target;
use crate::state::CloudState;

/// Build a dispatch target for a node: self (skipped by callers), HTTP admin URL,
/// or an iroh (id, addr) route. `None` when the node is unreachable.
fn target_for(cloud: &Arc<CloudState>, n: &NodeInfo) -> Option<Target> {
    if n.name == cloud.node_name {
        return Some(Target {
            node: n.name.clone(),
            admin: None,
            iroh: None,
        });
    }
    if let Some(a) = cloud.node_admins.read().get(&n.name).cloned() {
        return Some(Target {
            node: n.name.clone(),
            admin: Some(a),
            iroh: None,
        });
    }
    match (n.peer_id.clone(), n.iroh_addr.clone()) {
        (Some(id), Some(addr)) => Some(Target {
            node: n.name.clone(),
            admin: None,
            iroh: Some((id, addr)),
        }),
        _ => None,
    }
}

/// Whether `t` names a peer to send to: the target built for THIS node (see `target_for`) carries
/// neither an admin URL nor an iroh route.
fn is_remote(t: &Target) -> bool {
    t.admin.is_some() || t.iroh.is_some()
}

/// One reachable target node per replica region (healthy; prefers a node that
/// isn't the primary itself). Regions with no reachable node are skipped.
///
/// Candidates exclude this (primary) node and the node hosting the primary: a replica must live on
/// a different node.
fn replica_targets(cloud: &Arc<CloudState>, db: &Database) -> Vec<Target> {
    let nodes = cloud.registry.nodes();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for region in &db.replicas {
        let region = region.trim().to_ascii_lowercase();
        if region.is_empty() || region == db.region {
            continue;
        }
        let cands: Vec<&NodeInfo> = nodes
            .iter()
            .filter(|n| {
                n.healthy
                    && n.region.eq_ignore_ascii_case(&region)
                    && n.name != cloud.node_name
                    && n.name != db.primary_node
            })
            .collect();
        if let Some(t) = cands.iter().find_map(|n| target_for(cloud, n)) {
            if seen.insert(t.node.clone()) {
                out.push(t);
            }
        }
    }
    out
}

/// Send a replica control RPC (`register` | `remove`) to a target node.
async fn send_replica_rpc(cloud: &Arc<CloudState>, t: &Target, op: &str, db: &Database) -> bool {
    let payload = serde_json::json!({ "op": op, "db": db });
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    if let Some(admin) = &t.admin {
        cloud
            .http
            .post(format!("{admin}/v1/databases/replica"))
            .header("x-hive-team", db.team.clone())
            .header("content-type", "application/json")
            .body(body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    } else if let Some((id, addr)) = &t.iroh {
        crate::gossip::request_to(
            cloud,
            id,
            addr,
            hive_p2p::GOSSIP_POST,
            "/v1/databases/replica",
            &body,
            15,
        )
        .await
        .is_some()
    } else {
        false
    }
}

/// Provision/refresh replicas of `db` in each configured region (background).
///
/// The database is stamped with this node as `primary_node`, so replicas know where to stream from.
pub fn ensure_replicas(cloud: Arc<CloudState>, mut db: Database) {
    if db.replicas.is_empty() {
        return;
    }
    db.primary_node = cloud.node_name.clone();
    db.role = "primary".into();
    tokio::spawn(async move {
        let targets = replica_targets(&cloud, &db);
        for t in targets {
            if !is_remote(&t) {
                continue;
            }
            let ok = send_replica_rpc(&cloud, &t, "register", &db).await;
            tracing::info!(db = %db.id, region_node = %t.node, ok, "database replica register");
        }
    });
}

/// Tear down replicas of `db` in each configured region (background).
pub fn remove_replicas(cloud: Arc<CloudState>, db: Database) {
    if db.replicas.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for t in replica_targets(&cloud, &db) {
            if !is_remote(&t) {
                continue;
            }
            let _ = send_replica_rpc(&cloud, &t, "remove", &db).await;
        }
    });
}

/// The storage "name" a database owns for its kind (bucket/queue/index/topic/room),
/// used to map a storage write back to its database + replica regions.
fn db_storage_name(db: &Database) -> Option<&str> {
    let key = match db.kind {
        DbKind::Blob => "bucket",
        DbKind::Queue => "queue",
        DbKind::Vector => "index",
        DbKind::Pubsub => "topic",
        DbKind::Realtime => "room",
        _ => return None,
    };
    db.connection.get(key).map(|s| s.as_str())
}

/// Given a raw (un-namespaced) storage name + team, return the replica-region
/// targets to mirror a write to (empty when the name isn't a replicated DB).
fn mirror_targets(cloud: &Arc<CloudState>, team: &str, raw_name: &str) -> Vec<Target> {
    let db = cloud.databases.snapshot().into_iter().find(|d| {
        d.role != "replica"
            && crate::admin::norm(&d.team) == crate::admin::norm(team)
            && !d.replicas.is_empty()
            && db_storage_name(d) == Some(raw_name)
    });
    match db {
        Some(d) => replica_targets(cloud, &d)
            .into_iter()
            .filter(is_remote)
            .collect(),
        None => Vec::new(),
    }
}

/// Mirrors a storage write to every replica region of the owning database. A no-op when this call
/// is itself a mirrored write (`is_mirror`: applied locally, never re-mirrored, the loop breaker)
/// or the name is not replicated. `rel_path` is the storage API path
/// (`/v1/storage/blob/<name>/<key>`) and `method` is `PUT` or `POST`. Best-effort and async: it
/// never blocks the local write, and a failed delivery is only logged at debug (no retry).
pub fn on_write(
    cloud: &Arc<CloudState>,
    is_mirror: bool,
    team: &str,
    raw_name: &str,
    method: &str,
    rel_path: String,
    content_type: &str,
    body: Vec<u8>,
) {
    if is_mirror {
        return;
    }
    let targets = mirror_targets(cloud, team, raw_name);
    if targets.is_empty() {
        return;
    }
    send_mirrored(cloud, targets, team, method, rel_path, content_type, body);
}

/// Fans a write out to EVERY healthy peer rather than to configured replica regions. Pub/Sub and
/// Realtime are live delivery over a per-process broker, not durable stores with a replica policy:
/// `admin_ingress` sends a publish (a mutation) to the control-plane leader while WebSocket
/// subscribers (reads) stay on whichever node they connected to, so without this a publish reached
/// only the subscribers sharing the leader's node and still reported `delivered`. Delivery
/// therefore has to reach every node that might hold a subscriber. A no-op when `is_mirror`: that
/// call IS the fan-out arriving, and delivering again would loop.
pub fn fanout_all(
    cloud: &Arc<CloudState>,
    is_mirror: bool,
    team: &str,
    rel_path: String,
    content_type: &str,
    body: Vec<u8>,
) {
    if is_mirror {
        return;
    }
    let targets: Vec<Target> = cloud
        .registry
        .nodes()
        .iter()
        .filter(|n| n.healthy && n.name != cloud.node_name)
        .filter_map(|n| target_for(cloud, n))
        .collect();
    if targets.is_empty() {
        return;
    }
    send_mirrored(cloud, targets, team, "POST", rel_path, content_type, body);
}

/// How long a mesh mirror waits: above the 10 s HTTP timeout, for fresh-discovery fallback headroom
/// (see `admin::fetch_from_host`).
const MESH_MIRROR_TIMEOUT_SECS: u64 = 15;

/// Delivers one write to a fixed target set, over the HTTP admin URL or the iroh mesh, shared by
/// replica mirroring and pub/sub fan-out so both carry the identical signed mirror proof and
/// loop-breaker semantics.
///
/// The proof is a short-lived signed token of tenant identity that the receiving node's
/// `write_scope()` verifies (`x-hive-mirror-tok` over HTTP, `mesh_team_qs` over the mesh), so a
/// client-forged `x-hive-mirror`/`x-hive-team` pair can never redirect a write into another
/// tenant's namespace. Over HTTP the same token also rides as a Bearer: `auth::require_auth` gates
/// every mutation on `Authorization`/`hive_jwt` alone, so `x-hive-mirror-tok` by itself never
/// reached `write_scope` on a JWT-enforced node; reusing the one signed token keeps both parses
/// verifying byte-identical claims. Over the mesh the team and mirror flag ride the query string
/// and the gossip dispatch reconstructs the storage write on the replica.
fn send_mirrored(
    cloud: &Arc<CloudState>,
    targets: Vec<Target>,
    team: &str,
    method: &str,
    rel_path: String,
    content_type: &str,
    body: Vec<u8>,
) {
    let cloud = cloud.clone();
    let team = team.to_string();
    let method = method.to_string();
    let content_type = content_type.to_string();
    tokio::spawn(async move {
        for t in targets {
            let ok = if let Some(admin) = &t.admin {
                let url = format!("{admin}{rel_path}");
                let rb = if method == "PUT" {
                    cloud.http.put(&url)
                } else {
                    cloud.http.post(&url)
                };
                let mut rb = rb
                    .header("x-hive-team", team.clone())
                    .header("x-hive-mirror", "1")
                    .header("content-type", content_type.clone());
                if let Ok(tok) = crate::auth::issue(
                    "mesh-internal",
                    &team,
                    "service",
                    false,
                    crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
                ) {
                    rb = rb.header("x-hive-mirror-tok", tok.clone());
                    rb = rb.header("authorization", format!("Bearer {tok}"));
                }
                rb.body(body.clone())
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            } else if let Some((id, addr)) = &t.iroh {
                let sep = if rel_path.contains('?') { '&' } else { '?' };
                let p = format!(
                    "{rel_path}{sep}mirror=1&{}",
                    crate::admin::mesh_team_qs(&team)
                );
                crate::gossip::request_to(
                    &cloud,
                    id,
                    addr,
                    hive_p2p::GOSSIP_POST,
                    &p,
                    &body,
                    MESH_MIRROR_TIMEOUT_SECS,
                )
                .await
                .is_some()
            } else {
                false
            };
            tracing::debug!(node = %t.node, path = %rel_path, ok, "db write mirror");
        }
    });
}
