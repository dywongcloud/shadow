//! Cross-region database replication.
//!
//! When a database is provisioned with `replicas: [<region>, …]` the PRIMARY
//! node (where the writable instance lives) fans the database out to one node in
//! each replica region so reads are served locally in every configured region:
//!
//!   * **Record + egress replication** — the [`Database`] record is registered on
//!     each replica node (role=`replica`) so it appears in that region's dashboard
//!     and its connection env is auto-injected into the owning project's
//!     deployments there too.
//!   * **Platform-native kinds** (blob / queue / vector / pub-sub / realtime) —
//!     the primary MIRRORS every write to each replica node over the authenticated
//!     mesh (see [`on_write`]); an `x-hive-mirror` header breaks the loop so a
//!     mirrored write is applied locally and never re-mirrored. Each region then
//!     serves reads from its own local copy.
//!   * **Postgres / Redis** — the replica node provisions a real in-region backing
//!     container. Native physical streaming (`replicaof` / hot-standby) is enabled
//!     when the primary is reachable from the replica (a public endpoint); across
//!     NAT without that, the replica is an independent in-region instance and the
//!     limitation is logged rather than silently pretended.
//!
//! Transport reuses the mesh: HTTP admin URL when known, else an iroh request to
//! the peer — the same dual path the deploy fanout uses.

use std::sync::Arc;

use hive_edge::NodeInfo;

use crate::databases::{Database, DbKind};
use crate::schedule::Target;
use crate::state::CloudState;

/// Build a dispatch target for a node: self (skipped by callers), HTTP admin URL,
/// or an iroh (id, addr) route. `None` when the node is unreachable.
fn target_for(cloud: &Arc<CloudState>, n: &NodeInfo) -> Option<Target> {
    if n.name == cloud.node_name {
        return Some(Target { node: n.name.clone(), admin: None, iroh: None });
    }
    if let Some(a) = cloud.node_admins.read().get(&n.name).cloned() {
        return Some(Target { node: n.name.clone(), admin: Some(a), iroh: None });
    }
    match (n.peer_id.clone(), n.iroh_addr.clone()) {
        (Some(id), Some(addr)) => Some(Target { node: n.name.clone(), admin: None, iroh: Some((id, addr)) }),
        _ => None,
    }
}

/// One reachable target node per replica region (healthy; prefers a node that
/// isn't the primary itself). Regions with no reachable node are skipped.
fn replica_targets(cloud: &Arc<CloudState>, db: &Database) -> Vec<Target> {
    let nodes = cloud.registry.nodes();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for region in &db.replicas {
        let region = region.trim().to_ascii_lowercase();
        if region.is_empty() || region == db.region {
            continue;
        }
        // Healthy nodes in the region, EXCLUDING this (primary) node and the node
        // hosting the primary — a replica must live on a different node. (Locally
        // two nodes can share a region; in production the replica region differs
        // from the primary's, so this simply picks that region's node.)
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
        crate::gossip::request_to(cloud, id, addr, hive_p2p::GOSSIP_POST, "/v1/databases/replica", &body, 15)
            .await
            .is_some()
    } else {
        false
    }
}

/// Provision/refresh replicas of `db` in each configured region (background).
pub fn ensure_replicas(cloud: Arc<CloudState>, mut db: Database) {
    if db.replicas.is_empty() {
        return;
    }
    // Stamp the primary node so replicas know where to stream from.
    db.primary_node = cloud.node_name.clone();
    db.role = "primary".into();
    tokio::spawn(async move {
        let targets = replica_targets(&cloud, &db);
        for t in targets {
            if t.admin.is_none() && t.iroh.is_none() {
                continue; // that's us — the primary
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
            if t.admin.is_none() && t.iroh.is_none() {
                continue;
            }
            let _ = send_replica_rpc(&cloud, &t, "remove", &db).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Data-plane write mirroring (platform-native kinds)
// ---------------------------------------------------------------------------

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
    // Find a local PRIMARY database of this team that owns `raw_name` and has replicas.
    let db = cloud.databases.snapshot().into_iter().find(|d| {
        d.role != "replica"
            && crate::admin::norm(&d.team) == crate::admin::norm(team)
            && !d.replicas.is_empty()
            && db_storage_name(d) == Some(raw_name)
    });
    match db {
        Some(d) => replica_targets(cloud, &d).into_iter().filter(|t| t.admin.is_some() || t.iroh.is_some()).collect(),
        None => Vec::new(),
    }
}

/// Mirror a storage write to every replica region for the owning database. No-op
/// when this call is itself a mirrored write (`is_mirror`) or the name isn't
/// replicated. `rel_path` is the storage API path (e.g. `/v1/storage/blob/<name>/<key>`);
/// `method` is "PUT"/"POST". Best-effort + async — never blocks the local write.
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
        return; // applied locally; do not re-mirror (loop breaker)
    }
    let targets = mirror_targets(cloud, team, raw_name);
    if targets.is_empty() {
        return;
    }
    let cloud = cloud.clone();
    let team = team.to_string();
    let method = method.to_string();
    let content_type = content_type.to_string();
    tokio::spawn(async move {
        for t in targets {
            let ok = if let Some(admin) = &t.admin {
                let url = format!("{admin}{rel_path}");
                let rb = if method == "PUT" { cloud.http.put(&url) } else { cloud.http.post(&url) };
                rb.header("x-hive-team", team.clone())
                    .header("x-hive-mirror", "1")
                    .header("content-type", content_type.clone())
                    .body(body.clone())
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            } else if let Some((id, addr)) = &t.iroh {
                // Over the mesh: encode team + mirror flag in the query; the gossip
                // dispatch reconstructs the storage write on the replica.
                let sep = if rel_path.contains('?') { '&' } else { '?' };
                let p = format!("{rel_path}{sep}mirror=1&{}", crate::admin::mesh_team_qs(&team));
                crate::gossip::request_to(&cloud, id, addr, hive_p2p::GOSSIP_POST, &p, &body, 10).await.is_some()
            } else {
                false
            };
            tracing::debug!(node = %t.node, path = %rel_path, ok, "db write mirror");
        }
    });
}
