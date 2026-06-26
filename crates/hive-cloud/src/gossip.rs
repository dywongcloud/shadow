//! Control-plane gossip transport selection.
//!
//! Gossip (roster, serve-hosts, fleet view, container leases) can ride EITHER the
//! legacy HTTP-over-SSH-tunnel path OR the authenticated iroh QUIC mesh (the same
//! transport the data plane uses). iroh is preferred when `HIVE_GOSSIP_IROH` is set
//! AND we know a peer's iroh address (learned from a prior roster exchange); HTTP is
//! always the bootstrap + fallback, so a node with no iroh address yet — or a failed
//! QUIC attempt — still converges. The iroh side reuses the connection-level peer
//! trust gate (#20), so gossip is authenticated by the peer's cryptographic identity
//! rather than by the SSH tunnel.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;

use crate::state::CloudState;

fn jb(j: axum::Json<serde_json::Value>) -> Vec<u8> {
    serde_json::to_vec(&j.0).unwrap_or_default()
}

/// Whether iroh is the preferred gossip transport (opt-in; HTTP-over-SSH otherwise).
pub fn iroh_enabled() -> bool {
    std::env::var("HIVE_GOSSIP_IROH").map(|v| v == "1" || v == "true").unwrap_or(false)
}

/// Active health probe to a specific node over the iroh mesh, addressed DIRECTLY by
/// (node_id, addr) from the registry — bypassing the URL-keyed `peer_iroh` map so the
/// health loop can probe EVERY known peer, not just configured `--peer` targets. Tight
/// timeout, no HTTP fallback (health is a mesh-transport signal; we want to know if the
/// QUIC path to the peer is alive). Returns round-trip ms on success, `None` on
/// failure / when no mesh transport is bound. Reuses `/v1/nodes` (the same path the
/// gossip loop already treats as the liveness signal) so no new endpoint is added.
pub async fn probe(cloud: &Arc<CloudState>, node_id: &str, addr: &str, timeout: Duration) -> Option<u64> {
    let pool = cloud.mesh.read().clone()?;
    let t0 = hive_core::now_ms();
    match tokio::time::timeout(
        timeout,
        pool.gossip_request(node_id, addr, hive_p2p::GOSSIP_GET, "/v1/nodes", &[]),
    )
    .await
    {
        Ok(Ok(_)) => Some(hive_core::now_ms().saturating_sub(t0)),
        _ => {
            // Probe failed/timed out → drop any cached trunk so the NEXT probe re-dials
            // fresh. Critical for FAST RECOVERY: after a peer restarts with new socket
            // addrs, the stale QUIC trunk lingers until idle-timeout (~tens of seconds);
            // evicting it lets the next dial resolve the peer's CURRENT addr via
            // discovery (Seer pkarr) and reconnect within an interval or two.
            pool.close_peer(node_id).await;
            None
        }
    }
}

/// Send an arbitrary gossip request to a node addressed DIRECTLY by (node_id, addr) —
/// the dispatch primitive for deploy fanout over the mesh (a NAT'd coordinator has no
/// HTTP admin path to FC nodes). Returns the response body, or None on timeout/error.
pub async fn request_to(
    cloud: &Arc<CloudState>,
    node_id: &str,
    addr: &str,
    method: u8,
    path: &str,
    body: &[u8],
    timeout_secs: u64,
) -> Option<Vec<u8>> {
    let pool = cloud.mesh.read().clone()?;
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        pool.gossip_request(node_id, addr, method, path, body),
    )
    .await
    {
        Ok(Ok(b)) => Some(b),
        _ => None,
    }
}

/// Serve one gossip request locally by dispatching onto this node's admin handlers —
/// the SAME endpoints that answer HTTP gossip, so the two transports are
/// byte-equivalent. Returns the response body bytes.
pub async fn dispatch(cloud: &Arc<CloudState>, method: u8, path: &str, body: &[u8]) -> Vec<u8> {
    match path {
        "/v1/nodes/announce" if method == hive_p2p::GOSSIP_POST => {
            if let Ok(node) = serde_json::from_slice::<hive_edge::NodeInfo>(body) {
                return jb(crate::admin::node_announce(State(cloud.clone()), axum::Json(node)).await);
            }
            Vec::new()
        }
        "/v1/nodes" => jb(crate::admin::nodes(State(cloud.clone())).await),
        "/v1/serve-hosts" => jb(crate::admin::serve_hosts(State(cloud.clone())).await),
        "/v1/fleet-deployments" => jb(crate::admin::fleet_deployments(State(cloud.clone())).await),
        "/v1/leases" => jb(crate::admin::leases_get(State(cloud.clone())).await),
        #[cfg(feature = "zkauth")]
        "/v1/zkauth/roster-export" => jb(crate::zkauth::roster_export().await),
        // Deploy FANOUT over the mesh: a NAT'd coordinator (no HTTP path to FC nodes,
        // SSH tunnels cut) dispatches the per-target build here. Team rides as `?team=`
        // since the iroh transport carries no HTTP headers.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/git/deploy") => {
            let team = p.split_once("?team=").map(|(_, t)| t.to_string()).unwrap_or_default();
            let mut headers = axum::http::HeaderMap::new();
            if let Ok(hv) = axum::http::HeaderValue::from_str(&team) {
                headers.insert("x-hive-team", hv);
            }
            match serde_json::from_slice::<fluid_core::GitDeployRequest>(body) {
                Ok(req) => match crate::admin::git_deploy(State(cloud.clone()), headers, axum::Json(req)).await {
                    Ok(j) => jb(j),
                    Err((_, msg)) => serde_json::to_vec(&serde_json::json!({ "error": msg })).unwrap_or_default(),
                },
                Err(_) => Vec::new(),
            }
        }
        // Build status/log polling for the fanout mirror (coordinator streams the
        // target's build into its own record so the dashboard UX is unchanged).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/builds/") => {
            let id = p.trim_start_matches("/v1/builds/").split('?').next().unwrap_or("").to_string();
            match crate::admin::build_get(State(cloud.clone()), axum::extract::Path(id)).await {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Per-deployment + per-project READ VIEWS the coordinator proxies to the host
        // node over the mesh (the NAT'd coordinator has no HTTP admin path to FC nodes).
        // Team rides as `?team=`. The host serves these LOCALLY (it hosts the project),
        // and the coordinator always requests `local=true` so there's no re-proxy loop.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/deployments/") && p.contains("/resources") => {
            let id = p.trim_start_matches("/v1/deployments/").split('/').next().unwrap_or("").to_string();
            jb(crate::admin::deployment_resources(State(cloud.clone()), team_headers(p), axum::extract::Path(id)).await)
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows/runs") => {
            jb(crate::admin::wf_runs(State(cloud.clone()), team_headers(p), wf_query(p)).await)
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows") => {
            jb(crate::admin::wf_list(State(cloud.clone()), team_headers(p), wf_query(p)).await)
        }
        _ => Vec::new(),
    }
}

/// Pull a query-string value (`?k=v&...`) out of a dispatched path.
fn qparam(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// Build a HeaderMap carrying the `?team=` param as `x-hive-team` (the mesh transport
/// has no HTTP headers, so read-view proxies pass the tenant in the query).
fn team_headers(path: &str) -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    if let Some(t) = qparam(path, "team") {
        if let Ok(hv) = axum::http::HeaderValue::from_str(&t) {
            h.insert("x-hive-team", hv);
        }
    }
    h
}

/// Reconstruct the workflows `Query<WfQuery>` from a dispatched path's query string.
fn wf_query(path: &str) -> axum::extract::Query<crate::admin::WfQuery> {
    axum::extract::Query(crate::admin::WfQuery {
        project: qparam(path, "project"),
        local: qparam(path, "local").map(|v| v == "true" || v == "1"),
    })
}

/// The iroh gossip handler `serve_tunnels` invokes for inbound `STREAM_GOSSIP`.
pub fn handler(cloud: Arc<CloudState>) -> hive_p2p::GossipHandler {
    Arc::new(move |method, path, body| {
        let cloud = cloud.clone();
        Box::pin(async move { dispatch(&cloud, method, &path, &body).await })
    })
}

/// Fetch a gossip endpoint from `peer`, preferring iroh when enabled + the peer's
/// iroh address is known, else HTTP-over-SSH (also the bootstrap/fallback path).
/// Returns the response body bytes, or `None` on failure.
pub async fn fetch(
    cloud: &Arc<CloudState>,
    peer: &str,
    method: u8,
    path: &str,
    body: &[u8],
) -> Option<Vec<u8>> {
    if iroh_enabled() {
        // Clone out of the locks and DROP the guards before awaiting (parking_lot
        // guards aren't Send and can't be held across `.await`).
        let target = cloud.peer_iroh.read().get(peer).cloned();
        let pool = cloud.mesh.read().clone();
        if let (Some((node_id, addr)), Some(pool)) = (target, pool) {
            // BOUND the iroh attempt: dialing a peer's STALE identity (e.g. after it
            // restarted with a new key) can hang on connect/relay-retry. Without this
            // cap, a dead cached addr would stall the whole gossip loop and the HTTP
            // fallback below would never run — so the node could never re-learn the
            // peer's new address. On timeout/error we drop the stale mapping and fall
            // through to HTTP, which re-learns the fresh addr.
            let attempt = tokio::time::timeout(
                Duration::from_secs(3),
                pool.gossip_request(&node_id, &addr, method, path, body),
            )
            .await;
            match attempt {
                Ok(Ok(bytes)) => return Some(bytes),
                Ok(Err(e)) => {
                    tracing::debug!(peer, path, error = %e, "iroh gossip failed; falling back to HTTP");
                    cloud.peer_iroh.write().remove(peer);
                }
                Err(_) => {
                    tracing::debug!(peer, path, "iroh gossip timed out; falling back to HTTP");
                    cloud.peer_iroh.write().remove(peer);
                }
            }
        }
    }
    // HTTP-over-SSH (bootstrap + fallback).
    let url = format!("{peer}{path}");
    let req = if method == hive_p2p::GOSSIP_POST {
        cloud.http.post(&url).header("content-type", "application/json").body(body.to_vec())
    } else {
        cloud.http.get(&url)
    };
    match req.timeout(Duration::from_secs(4)).send().await {
        Ok(r) if r.status().is_success() => r.bytes().await.ok().map(|b| b.to_vec()),
        _ => None,
    }
}
