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
        "/v1/nodes" => match crate::admin::nodes(State(cloud.clone()), mesh_operator_claims()).await {
            Ok(j) => jb(j),
            Err(_) => Vec::new(),
        },
        "/v1/serve-hosts" => jb(crate::admin::serve_hosts(State(cloud.clone())).await),
        // TLS bundle distribution over the authenticated mesh (see acme.rs::
        // bundle_for_mesh — key decrypted in transit inside peer-authenticated
        // QUIC only; receiver re-encrypts with its own node key).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/tls/bundle") => {
            let name = p.split_once("name=").map(|(_, n)| n.split('&').next().unwrap_or(n)).unwrap_or("");
            crate::acme::bundle_for_mesh(name)
        }
        // NON-SECRET directory of gateway-addressable DBs hosted on this node
        // ({id, db_host, host_node, kind} — no credentials). The DNS leader fans
        // this out to publish per-DB `<slug>.{db_domain}` A records for DBs that
        // provisioned on other nodes (DB records themselves are not gossiped).
        // Prefix match: fetch_from_host appends `?team=`.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/db-directory") => {
            serde_json::to_vec(&cloud.databases.directory()).unwrap_or_default()
        }
        // Local per-function usage stats (each node meters its own compute) — the
        // billing meter loop on the coordinator fans this out to sum fleet usage.
        // Prefix match: `fetch_from_host` appends `?team=` so an exact arm misses.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/functions") => {
            match crate::admin::functions(State(cloud.clone()), mesh_operator_claims()).await {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Mesh project-delete cascade (single hop): the coordinator's cross-node
        // teardown for hosting nodes reachable only over iroh. Team must OWN the
        // project on THIS node (or the project must be absent — idempotent).
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/projects/") && p.contains("/delete") => {
            let project = p
                .trim_start_matches("/v1/projects/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            let team = p.split_once("?team=").map(|(_, t)| t.to_string()).unwrap_or_default();
            let owner = cloud.projects.team_of(&project);
            let owns_settings = crate::admin::norm(&owner) == crate::admin::norm(&team);
            let owns_deploys = cloud
                .gw
                .list()
                .iter()
                .any(|d| d.project == project && crate::admin::record_tenant(&d.tenant) == crate::admin::norm(&team));
            if project.is_empty() || !(owns_settings || owns_deploys) {
                return serde_json::to_vec(&serde_json::json!({ "error": "not owner" })).unwrap_or_default();
            }
            let removed = crate::admin::delete_project_local(&cloud, &project, &team).await;
            serde_json::to_vec(&serde_json::json!({ "project": project, "removed": removed })).unwrap_or_default()
        }
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
                Ok(req) => match crate::admin::git_deploy(State(cloud.clone()), headers, team_claims(p), axum::Json(req)).await {
                    Ok(j) => jb(j),
                    Err((_, msg)) => serde_json::to_vec(&serde_json::json!({ "error": msg })).unwrap_or_default(),
                },
                Err(_) => Vec::new(),
            }
        }
        // Instant rollback to a placed deployment: the coordinator proxies the
        // promote (a mutation) to the host node over the mesh. The host runs it
        // locally (it holds the deployment), so there's no re-proxy loop.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/deployments/") && p.contains("/promote") => {
            let id = p.trim_start_matches("/v1/deployments/").split('/').next().unwrap_or("").to_string();
            match crate::admin::dep_promote(State(cloud.clone()), team_headers(p), team_claims(p), axum::extract::Path(id)).await {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Build status/log polling for the fanout mirror (coordinator streams the
        // target's build into its own record so the dashboard UX is unchanged).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/builds/") => {
            let id = p.trim_start_matches("/v1/builds/").split('?').next().unwrap_or("").to_string();
            match crate::admin::build_get(State(cloud.clone()), team_headers(p), team_claims(p), axum::extract::Path(id)).await {
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
            jb(crate::admin::deployment_resources(State(cloud.clone()), team_headers(p), team_claims(p), axum::extract::Path(id)).await)
        }
        // Build record + logs for a deployment hosted on THIS node (proxied by the
        // coordinator when the deployment was placed here — build logs live where it built).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/deployments/") && p.contains("/build") => {
            let id = p.trim_start_matches("/v1/deployments/").split('/').next().unwrap_or("").to_string();
            match crate::admin::deployment_build(State(cloud.clone()), team_headers(p), team_claims(p), axum::extract::Path(id)).await {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Intelligent service graph for a deployment hosted on THIS node (scanned here).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/deployments/") && p.contains("/service-graph") => {
            let id = p.trim_start_matches("/v1/deployments/").split('/').next().unwrap_or("").to_string();
            match crate::admin::deployment_service_graph(State(cloud.clone()), team_headers(p), team_claims(p), axum::extract::Path(id)).await {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Project's latest service graph, served LOCALLY (the coordinator fans this
        // out to the node that built the project). Local-only → no re-proxy loop.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/projects/") && p.contains("/service-graph") => {
            let project = p.trim_start_matches("/v1/projects/").split('/').next().unwrap_or("").to_string();
            let team = qparam(p, "team").unwrap_or_default();
            // Only return it to the owning tenant (this node built it, so it has the
            // project→team mapping). Empty team = trusted/no-scope caller.
            let owner = cloud.projects.team_of(&project);
            let owner_ns = if owner.trim().is_empty() { "personal".to_string() } else { owner };
            let team_ns = if team.trim().is_empty() { String::new() } else { team };
            // On-demand scan if not yet stored (backfills existing/failed deployments).
            match crate::admin::local_project_graph(&cloud, &project).await {
                Some(g) if team_ns.is_empty() || team_ns == owner_ns => jb(axum::Json(serde_json::json!(g))),
                _ => Vec::new(),
            }
        }
        // A single run's DETAIL — must match before the runs LIST arm below.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows/runs/") => {
            let id = p.trim_start_matches("/v1/workflows/runs/").split('?').next().unwrap_or("").to_string();
            match crate::admin::wf_run_detail(State(cloud.clone()), team_headers(p), team_claims(p), axum::extract::Path(id), wf_query(p)).await {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows/runs") => {
            jb(crate::admin::wf_runs(State(cloud.clone()), team_headers(p), team_claims(p), wf_query(p)).await)
        }
        // Workflow summary rollup — must match before the generic `/v1/workflows` arm.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows/summary") => {
            jb(crate::admin::wf_summary(State(cloud.clone()), team_headers(p), team_claims(p), wf_query(p)).await)
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows") => {
            jb(crate::admin::wf_list(State(cloud.clone()), team_headers(p), team_claims(p), wf_query(p)).await)
        }
        // Request/routing event log: recorded on the SERVING node, so the coordinator
        // proxies here to read a placed project's logs. `local=true` rides in the query
        // so this node returns only its own events (no re-fan-out → no loop).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/logs") => {
            jb(crate::admin::logs(State(cloud.clone()), team_headers(p), team_claims(p), logs_query(p)).await)
        }
        // Sandbox READS: every sandbox MUTATION forwards through admin_ingress to the
        // single current control-plane owner (no independent placement), so that node
        // is the sole holder of sandbox state. sandboxes_api.rs's own `fetch_from_host`
        // fallback tries an HTTP admin URL first, then this iroh-mesh path — which,
        // before this arm existed, had NO dispatch entry for any `/sandboxes` path, so
        // the fallback silently returned nothing and every non-owner node 404'd on
        // every sandbox read (live-reproduced). One consolidated arm dispatches on
        // segment shape rather than N ordered prefix-checks, so there is no
        // most-specific-first ordering hazard to maintain as sandbox routes grow.
        p if method == hive_p2p::GOSSIP_GET && p.split('?').next().unwrap_or(p).contains("/sandboxes") => {
            let (project, segs) = sandbox_path_project_and_segs(p);
            let project = project.to_string();
            let state = State(cloud.clone());
            let hdrs = team_headers(p);
            let clm = team_claims(p);
            match segs.len() {
                0 => match crate::sandboxes_api::list_sandboxes(state, hdrs, clm, axum::extract::Path(project)).await {
                    Ok(j) => jb(j),
                    Err(_) => Vec::new(),
                },
                1 => {
                    let sandbox_id = segs[0].to_string();
                    match crate::sandboxes_api::get_sandbox(state, hdrs, clm, axum::extract::Path((project, sandbox_id))).await {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                2 if segs[1] == "commands" => {
                    let sandbox_id = segs[0].to_string();
                    match crate::sandboxes_api::list_commands(state, hdrs, clm, axum::extract::Path((project, sandbox_id))).await {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                2 if segs[1] == "snapshots" => {
                    let sandbox_id = segs[0].to_string();
                    match crate::sandboxes_api::list_snapshots(state, hdrs, clm, axum::extract::Path((project, sandbox_id))).await {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                2 if segs[1] == "mounts" => {
                    let sandbox_id = segs[0].to_string();
                    match crate::sandboxes_api::list_mounts(state, hdrs, clm, axum::extract::Path((project, sandbox_id))).await {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                2 if segs[1] == "domain" => {
                    let sandbox_id = segs[0].to_string();
                    let port: u16 = qparam(p, "port").and_then(|s| s.parse().ok()).unwrap_or(0);
                    match crate::sandboxes_api::get_domain(state, hdrs, clm, axum::extract::Path((project, sandbox_id)), axum::extract::Query(crate::sandboxes_api::DomainQuery { port })).await {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                3 if segs[1] == "commands" => {
                    let sandbox_id = segs[0].to_string();
                    let command_id = segs[2].to_string();
                    match crate::sandboxes_api::get_command(state, hdrs, clm, axum::extract::Path((project, sandbox_id, command_id))).await {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                3 if segs[1] == "files" && segs[2] == "read" => {
                    let sandbox_id = segs[0].to_string();
                    let q = crate::sandboxes_api::ReadFileQuery { path: qparam(p, "path").unwrap_or_default() };
                    match crate::sandboxes_api::read_file(state, hdrs, clm, axum::extract::Path((project, sandbox_id)), axum::extract::Query(q)).await {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                4 if segs[1] == "commands" && segs[3] == "logs" => {
                    let sandbox_id = segs[0].to_string();
                    let command_id = segs[2].to_string();
                    match crate::sandboxes_api::get_command_logs(state, hdrs, clm, axum::extract::Path((project, sandbox_id, command_id))).await {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                _ => Vec::new(),
            }
        }
        // Cross-region DB replica control (register/remove) over the mesh.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/databases/replica") => {
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(v) => match crate::admin::database_replica(State(cloud.clone()), axum::Json(v)).await {
                    Ok(j) => jb(j),
                    Err((_, e)) => jb(axum::Json(serde_json::json!({ "error": e }))),
                },
                Err(e) => jb(axum::Json(serde_json::json!({ "error": e.to_string() }))),
            }
        }
        // Mirrored storage writes over the mesh (replication data plane). The team
        // rides as `?team=` and is applied to the SAME tenant namespace on this
        // replica; these NEVER re-mirror (they're already replicated writes).
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/storage/") && (qparam(p, "mirror").as_deref() == Some("1")) => {
            crate::admin::apply_mirrored_write(&cloud, p, body).await;
            jb(axum::Json(serde_json::json!({ "ok": true })))
        }
        _ => Vec::new(),
    }
}

/// Reconstruct the logs `Query<LimitQ>` from a dispatched path's query string.
fn logs_query(path: &str) -> axum::extract::Query<crate::admin::LimitQ> {
    axum::extract::Query(crate::admin::LimitQ {
        limit: qparam(path, "limit").and_then(|v| v.parse().ok()),
        project: qparam(path, "project"),
        deployment: qparam(path, "deployment"),
        q: qparam(path, "q"),
        local: qparam(path, "local").map(|v| v == "true" || v == "1"),
    })
}

/// Pull a query-string value (`?k=v&...`) out of a dispatched path.
fn qparam(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// Splits a dispatched `/v1/projects/<project>/sandboxes[/<rest>]` path (query
/// string already ignored) into the project id and the remaining `/`-separated
/// segments after `sandboxes`, e.g. `.../sandboxes/sbx_1/commands/cmd_1/logs`
/// -> (`sbx_project`, [`sbx_1`, `commands`, `cmd_1`, `logs`]). Segment count +
/// shape drives which sandbox handler a dispatched request maps to.
fn sandbox_path_project_and_segs(path: &str) -> (&str, Vec<&str>) {
    let p = path.split('?').next().unwrap_or(path);
    let rest = p.trim_start_matches("/v1/projects/");
    let mut it = rest.splitn(2, "/sandboxes");
    let project = it.next().unwrap_or("");
    let tail = it.next().unwrap_or("");
    let segs = tail.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    (project, segs)
}

/// Build a verified-claims extension for a mesh-internal admin call. The iroh
/// transport carries no HTTP headers and the calling peer is already trusted
/// (peer-trust allowlist + mesh auth), so the team that rides as `?team=` is
/// injected as authoritative [`crate::auth::Claims`]. Handlers resolve the tenant
/// from these claims — under enforced JWT auth the synthesized `x-hive-team`
/// header is ignored, so without this the tenant would be lost on the host node.
fn team_claims(path: &str) -> Option<axum::Extension<crate::auth::Claims>> {
    // PREFERRED: a signed, short-lived delegation token (`?tok=`) minted by the
    // originating node from the verified user tenant. Verifying it here yields an
    // AUTHORITATIVE, integrity-protected, expiring tenant assertion — closing the
    // raw-`?team=` spoofing class: the value can no longer be an arbitrary
    // attacker-set query param, it must be a validly-signed unexpired JWT.
    // (Residual, inherent to a shared HIVE_JWT_SECRET mesh: a compromised
    // *trusted peer node* can still mint any tenant — that requires per-node
    // signing keys to close and is bounded by the peer-trust allowlist gate.)
    if let Some(tok) = qparam(path, "tok") {
        if let Ok(mut claims) = crate::auth::verify(&tok) {
            claims.platform_admin = false;
            return Some(axum::Extension(claims));
        }
        if crate::auth::enforced() {
            tracing::warn!(%path, "mesh delegation token present but INVALID; rejecting tenant assertion");
            return None;
        }
    }
    let team = qparam(path, "team")?;
    let team = team.trim();
    if team.is_empty() {
        return None;
    }
    // FALLBACK: raw `?team=`. Retained for dev/unenforced mode and rolling
    // upgrades (an origin node predating token-minting sends only `team=`).
    if crate::auth::enforced() {
        tracing::warn!(%path, "mesh call carried raw team= without a signed token (rolling-upgrade fallback)");
    }
    Some(axum::Extension(crate::auth::Claims {
        sub: "mesh-internal".into(),
        tenant: team.to_string(),
        role: "service".into(),
        iat: 0,
        exp: 0,
        platform_admin: false,
    }))
}

/// Synthetic operator identity for GLOBAL (non-tenant) admin reads proxied over the
/// mesh (`/v1/nodes`, `/v1/functions`) — these endpoints require `require_operator`
/// (platform_admin) on the public HTTP router, but a peer reaching THIS dispatch
/// function has already passed the P2P layer's own trust/admission gate
/// (STREAM_JOIN / trusted_peer_ids), a boundary this code never crosses on behalf
/// of an untrusted caller. Scoped to this mesh-dispatch surface only — never
/// constructed from request-supplied data.
fn mesh_operator_claims() -> Option<axum::Extension<crate::auth::Claims>> {
    Some(axum::Extension(crate::auth::Claims {
        sub: "mesh-internal".into(),
        tenant: "".into(),
        role: "service".into(),
        iat: 0,
        exp: 0,
        platform_admin: true,
    }))
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
        summary: qparam(path, "summary").map(|v| v == "true" || v == "1"),
    })
}

/// The iroh gossip handler `serve_tunnels` invokes for inbound `STREAM_GOSSIP`.
/// Pure core of the mesh-mutation authorization decision (no CloudState/env
/// deps, unit-testable directly). `signer_trusted`: `None` when no trust set
/// is configured at all (nothing to check against — permissionless, today's
/// default); `Some(bool)` when a trust set IS configured, carrying whether
/// the (possibly-absent, mapped to `false`) verified signer is a member.
fn mesh_mutation_authorized(method: u8, signer_trusted: Option<bool>) -> bool {
    method == hive_p2p::GOSSIP_GET || signer_trusted.unwrap_or(true)
}

pub fn handler(cloud: Arc<CloudState>) -> hive_p2p::GossipHandler {
    // `signer` is the message's VERIFIED ed25519 identity (Some only when the
    // request carried a valid signature bound to the QUIC peer).
    //
    // The iroh gossip transport carries no HTTP headers, so it never passes
    // through `auth::require_auth` (the JWT middleware) at all — an operator
    // who believes they've secured the API by setting HIVE_JWT_SECRET has
    // only secured the HTTP admin surface; this mesh path answered the exact
    // same privileged handlers (deploy, project delete, database_replica,
    // storage mirror writes, promote) with NO authorization check whatsoever,
    // regardless of that configuration. The only authorization signal this
    // transport can carry is a verified, trust-set-member signer, so: when a
    // trust set is actually configured (non-empty — matches the HTTP side's
    // "only enforce what's actually configured" pattern, e.g. `require_auth`/
    // `require_operator`), require it for every MUTATING call. Reads remain
    // open here (each handler still applies its own tenant/team scoping) —
    // this mirrors the HTTP side's "reads are always allowed" policy rather
    // than introducing an inconsistent new read-side gate. With no trust set
    // configured (today's default), this is unchanged/permissionless.
    Arc::new(move |method, path, body, signer: Option<String>| {
        let cloud = cloud.clone();
        Box::pin(async move {
            if let Some(s) = &signer {
                tracing::trace!(signer = %s, %path, "verified signed gossip");
            }
            let trust_configured = cloud.trusted_peer_ids.read().map(|s| !s.is_empty()).unwrap_or(false);
            let signer_trusted = trust_configured
                .then(|| signer.as_deref().map(|s| hive_p2p::peer_trusted(&cloud.trusted_peer_ids, s)).unwrap_or(false));
            if !mesh_mutation_authorized(method, signer_trusted) {
                tracing::warn!(%path, signer = ?signer, "REJECTED mutating gossip: no verified+trusted signer (trust set configured)");
                return serde_json::to_vec(&serde_json::json!({ "error": "untrusted or unsigned mesh caller" })).unwrap_or_default();
            }
            dispatch(&cloud, method, &path, &body).await
        })
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

#[cfg(test)]
mod mesh_auth_tests {
    use super::*;

    // REGRESSION TESTS for a real, confirmed vulnerability: the iroh gossip
    // transport carries no HTTP headers, so it never passed through
    // `auth::require_auth` at all — mutating handlers reachable via gossip
    // (deploy, project delete, database_replica, storage mirror writes,
    // promote) had NO authorization check whatsoever, regardless of
    // HIVE_JWT_SECRET/HIVE_PEER_TRUST configuration.

    #[test]
    fn reads_are_always_allowed_regardless_of_trust_config() {
        assert!(mesh_mutation_authorized(hive_p2p::GOSSIP_GET, None));
        assert!(mesh_mutation_authorized(hive_p2p::GOSSIP_GET, Some(false)));
        assert!(mesh_mutation_authorized(hive_p2p::GOSSIP_GET, Some(true)));
    }

    #[test]
    fn mutations_are_permissionless_when_no_trust_set_is_configured() {
        // Today's default — unchanged behavior when HIVE_PEER_TRUST is unset.
        assert!(mesh_mutation_authorized(hive_p2p::GOSSIP_POST, None));
    }

    #[test]
    fn mutations_require_a_trusted_signer_once_a_trust_set_is_configured() {
        assert!(mesh_mutation_authorized(hive_p2p::GOSSIP_POST, Some(true)), "a verified, trusted signer must be allowed");
        assert!(
            !mesh_mutation_authorized(hive_p2p::GOSSIP_POST, Some(false)),
            "an unsigned or untrusted-signer mutation must be REJECTED once trust is configured"
        );
    }

    #[test]
    fn mesh_team_qs_round_trips_a_signed_token_into_authoritative_claims() {
        // Env-independent: verify + issue are driven off an explicit secret via
        // the auth helpers, not global process state, so this can't race the
        // suite's other enforced()-sensitive tests. A signed token's tenant is
        // AUTHORITATIVE over a spoofed `?team=`; a garbled token yields no claims.
        let tok = crate::auth::issue_with_secret("mesh-internal", "acme", "service", false, 60, "sekret").unwrap();
        let path = format!("/v1/git/deploy?team=SPOOFED&tok={tok}");
        let (tok_in_path, _) = (super::qparam(&path, "tok").unwrap(), ());
        let claims = crate::auth::verify_with_secret(&tok_in_path, "sekret").unwrap();
        assert_eq!(claims.tenant, "acme", "signed token tenant is authoritative over the raw param");
        assert!(crate::auth::verify_with_secret("not-a-jwt", "sekret").is_err());

        // The raw `?team=` fallback (no token) still resolves a tenant for
        // dev/rolling-upgrade — this path needs no secret.
        let claims = team_claims("/v1/logs?team=personal").expect("raw team fallback").0;
        assert_eq!(claims.tenant, "personal");
        assert!(team_claims("/v1/logs").is_none(), "no team and no token yields no claims");
    }
}
