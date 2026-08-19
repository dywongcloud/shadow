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
    std::env::var("HIVE_GOSSIP_IROH")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

/// Active health probe to a specific node over the iroh mesh, addressed DIRECTLY by
/// (node_id, addr) from the registry — bypassing the URL-keyed `peer_iroh` map so the
/// health loop can probe EVERY known peer, not just configured `--peer` targets. Tight
/// timeout, no HTTP fallback (health is a mesh-transport signal; we want to know if the
/// QUIC path to the peer is alive). Returns round-trip ms on success, `None` on
/// failure / when no mesh transport is bound. Reuses `/v1/nodes` (the same path the
/// gossip loop already treats as the liveness signal) so no new endpoint is added.
pub async fn probe(
    cloud: &Arc<CloudState>,
    node_id: &str,
    addr: &str,
    timeout: Duration,
) -> Option<u64> {
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

/// Relay-selection-algorithm wiring: steer `addr_json`'s relay transport toward
/// the best LIVE relay for the peer identified by iroh `node_id`, instead of
/// trusting whatever relay was serialized into the cached hint at gossip time
/// (which — for a peer only ever learned second/third-hand — may be stale or
/// simply absent). Preference (see `hive_edge::select_relay_hint`): the target's
/// OWN gossiped `relay_url` > the geographically nearest OTHER healthy peer's
/// `relay_url` > the central `relay.shadw.cloud` backstop. Every caller of
/// `request_to`/`fetch` below inherits this automatically (including
/// `admin::fetch_from_host`, `acme.rs`, `db_replicate.rs`, `git.rs`'s mesh
/// fanout — every one of them dials by `(node_id, addr_json)` through these two
/// functions). Falls back to `addr_json` unchanged when `node_id` isn't found in
/// the registry (e.g. a bootstrap seed never gossiped as a full `NodeInfo`).
fn relay_hinted_addr(cloud: &Arc<CloudState>, node_id: &str, addr_json: &str) -> String {
    let nodes = cloud.registry.nodes();
    // Match on endpoint id OR node name. `node_id` here is whatever the caller had:
    // `request_to`'s callers pass a 64-hex `peer_id`, but `fetch` pulls its tuple
    // from `peer_iroh`, whose entries for peers learned over the HTTP `/v1/nodes`
    // path hold the node NAME (`ps.id`). A name can never equal a hex endpoint id,
    // so the old peer_id-only match ALWAYS missed on that path and returned
    // `addr_json` untouched — meaning relay-hint steering silently did nothing for
    // every peer learned that way, while appearing to work because the other
    // caller set passed ids. Accepting both makes the hint apply wherever the
    // caller's label actually identifies a real registry node.
    let Some(target) = nodes
        .iter()
        .find(|n| n.peer_id.as_deref() == Some(node_id) || n.id == node_id)
    else {
        return addr_json.to_string();
    };
    let me = cloud.registry.me();
    let hint = hive_edge::select_relay_hint(target, &nodes, &me);
    hive_p2p::with_relay_hint(addr_json, Some(&hint))
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
    let addr = relay_hinted_addr(cloud, node_id, addr);
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        pool.gossip_request(node_id, &addr, method, path, body),
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
                return jb(
                    crate::admin::node_announce(State(cloud.clone()), axum::Json(node)).await,
                );
            }
            Vec::new()
        }
        "/v1/nodes" => {
            match crate::admin::nodes(State(cloud.clone()), mesh_operator_claims()).await {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        "/v1/serve-hosts" => jb(crate::admin::serve_hosts(State(cloud.clone())).await),
        // Full TeamStore snapshot for the leader->follower teams sync (see
        // spawn_relational_mirror_loop's follower branch). Team mutations only
        // ever land on the control-plane leader (admin_ingress forward), so a
        // follower's local store otherwise diverges forever -- live-witnessed
        // as sj=5 / bkk=4 / va=2 teams across nodes, which let a failover
        // stand-in leader corrupt the relational teams mirror with its stale
        // list. Mesh-authenticated peers only (same trust surface as every
        // other arm here); team data carries no secrets beyond member emails,
        // which already ride /v1/nodes-adjacent surfaces mesh-wide.
        "/v1/teams/snapshot" if method == hive_p2p::GOSSIP_GET => {
            // Rolling-upgrade shim expects the old bare-map shape. Feed it only
            // authoritative rows; publishing the synthetic local fallback lets
            // an older peer strip its provenance and relay it back as real.
            serde_json::to_vec(&cloud.teams.snapshot_synced().rows).unwrap_or_default()
        }
        // Full IncidentStore snapshot for the same leader->follower sync
        // (identical shape and rationale as /v1/teams/snapshot above): incident
        // mutations only ever land on the control-plane leader via
        // admin_ingress, so a follower's local store stays empty forever --
        // live-witnessed as the admin incidents page showing nothing (and
        // creates "not working") whenever the dashboard's multi-A DNS landed
        // the read on any non-leader node while the 2 real incidents sat on
        // the leader alone.
        "/v1/incidents/snapshot" if method == hive_p2p::GOSSIP_GET => {
            serde_json::to_vec(&cloud.incidents.snapshot()).unwrap_or_default()
        }
        // Browser artifact BYTES (browser-function-artifact-delivery): the
        // content-addressed GET proxies here when round-robin ingress landed
        // it on a node that never ran the build. Prefix-disjoint with the
        // admissions/presence arms below; the owner re-runs the tenant gate
        // against ITS deployment state before serving (mesh_tenant carries the
        // signed delegation token). Returns raw bytes; empty = not servable.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/artifacts/") => {
            let digest = p
                .trim_start_matches("/v1/browser/artifacts/")
                .split('?')
                .next()
                .unwrap_or("");
            let tenant = mesh_tenant(p);
            if tenant.is_empty() {
                Vec::new()
            } else {
                crate::browser_artifacts::mesh_get(cloud, &tenant, digest).await
            }
        }
        // shadw drive (bn-shadw-drive-v1) file bytes: the tenant-gated
        // `GET /v1/drive/:project/file` proxy's mesh fallback for a NAT'd
        // owner node. The owner re-runs `project_owned_by` against its own
        // replicated state before serving (the `browser_artifacts::mesh_get`
        // precedent) — `mesh_tenant` carries the signed delegation token, so
        // an empty tenant (unsigned/expired) never resolves anything.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/drive/mesh-file/") => {
            let tenant = mesh_tenant(p);
            let project = qparam(p, "project").unwrap_or_default();
            let path = qparam(p, "path")
                .map(|v| {
                    percent_encoding::percent_decode_str(&v)
                        .decode_utf8_lossy()
                        .to_string()
                })
                .unwrap_or_default();
            if tenant.is_empty() || project.is_empty() {
                Vec::new()
            } else {
                crate::drive_api::mesh_get_file(cloud, &tenant, &project, &path).await
            }
        }
        // shadw drive share-link bytes: deliberately UNAUTHENTICATED (the
        // token hash itself is the capability, re-derived independently by
        // the owner from the SAME replicated `drive_shares` row — no tenant
        // to carry at all).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/drive/mesh-share/") => {
            let token = qparam(p, "token").unwrap_or_default();
            if token.is_empty() {
                Vec::new()
            } else {
                crate::drive_api::mesh_share_get(&token).await
            }
        }
        // Accept-side browser admission recheck is identity-only and must
        // precede the tenant-scoped endpoint arm below.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/admissions/accept/") => {
            let endpoint_id = p
                .trim_start_matches("/v1/browser/admissions/accept/")
                .split('?')
                .next()
                .unwrap_or("");
            crate::browser_admission::mesh_accept(cloud, endpoint_id)
        }
        // Browser admission first-read leader fallback. The endpoint-specific
        // arm must precede the list arm. Tenant comes from the verified,
        // short-lived mesh delegation token appended by fetch_from_host.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/admissions/") => {
            let endpoint_id = p
                .trim_start_matches("/v1/browser/admissions/")
                .split("?")
                .next()
                .unwrap_or("");
            let tenant = mesh_tenant(p);
            if tenant.is_empty() {
                Vec::new()
            } else {
                crate::browser_admission::mesh_get(cloud, &tenant, endpoint_id)
            }
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/admissions") => {
            let tenant = mesh_tenant(p);
            if tenant.is_empty() {
                Vec::new()
            } else {
                crate::browser_admission::mesh_list(cloud, &tenant)
            }
        }
        // Fast-path revoke echo (bn-p2p-revocation-latency): the leader fans
        // this out right after a real revoke so every follower applies the
        // relay-deny + routing-removal side effects within one mesh round
        // trip instead of waiting up to the ~60s periodic store_sync
        // snapshot pull. No tenant check here -- the leader already made and
        // validated the authoritative decision; a follower just echoes it
        // for this specific endpoint_id. Never touches versioned
        // active/tombstone state (see mark_denied's doc), so it cannot race
        // or corrupt this follower's own later adopt() ordering.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/browser/admissions/mesh-revoke/") =>
        {
            let endpoint_id = p
                .trim_start_matches("/v1/browser/admissions/mesh-revoke/")
                .split('?')
                .next()
                .unwrap_or("");
            if !endpoint_id.is_empty() {
                crate::browser_admission::mesh_revoke_echo(cloud, endpoint_id).await;
            }
            Vec::new()
        }
        // Fresh-admission deny-CLEAR echo (bn-relay-denylist-restart-friction):
        // the exact inverse of the mesh-revoke arm above — the leader fans
        // this out right after a validated admission supersedes a stale
        // stop()-created denylist entry, so follower relays let the
        // re-admitted endpoint reconnect within one mesh round trip instead
        // of waiting out the snapshot-pull interval. Denylist-only on the
        // receiving side; same "leader already decided" trust posture as the
        // revoke echo.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/browser/admissions/mesh-deny-clear/") =>
        {
            let endpoint_id = p
                .trim_start_matches("/v1/browser/admissions/mesh-deny-clear/")
                .split('?')
                .next()
                .unwrap_or("");
            if !endpoint_id.is_empty() {
                crate::browser_admission::mesh_deny_clear_echo(cloud, endpoint_id).await;
            }
            Vec::new()
        }
        // Fleet<->fleet browser-db anti-entropy (see `browser_db::mesh_crr_sync`).
        // Prefix-disjoint from every `/v1/browser/*` arm — note the hyphen — so it
        // neither shadows nor is shadowed by them. Body and reply are the raw
        // `Op::CrrSync` encodings, which is why this rides the binary gossip
        // dispatch rather than a JSON arm.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/browser-db/mesh-sync/") => {
            let project = p.trim_start_matches("/v1/browser-db/mesh-sync/");
            if project.is_empty() {
                Vec::new()
            } else {
                crate::browser_db::mesh_crr_sync(cloud, project, body).await
            }
        }
        // Presence write-through to the leader (see `echo_presence_to_leader`).
        // Ordered BEFORE the GET arm below only for readability — the two are
        // already disjoint by method — but it must stay ahead of any future
        // broader `/v1/browser/presence` arm, per the longest-prefix-first rule.
        p if method == hive_p2p::GOSSIP_POST && p == "/v1/browser/presence/mesh-echo" => {
            crate::browser_presence::mesh_echo(cloud, body)
        }
        // Coarse browser presence first-read leader fallback (constellation
        // satellites) — tenant-scoped the same way as the admissions list arm.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/presence") => {
            let tenant = mesh_tenant(p);
            if tenant.is_empty() {
                Vec::new()
            } else {
                crate::browser_presence::mesh_list(cloud, &tenant)
            }
        }
        // Per-tenant relay byte metering local slice (bn-impl-relay-byte-metering)
        // — the iroh-only-peer path for the fleet fanout; mirrors the
        // `/v1/functions` arm's "the outer request already enforced operator"
        // posture. Prefix-disjoint with the other /v1/browser/* arms.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/metering") => {
            crate::browser_metering::mesh_local(cloud)
        }
        // Generic leader->follower store snapshot: `GET /v1/store-snapshot/<name>`
        // serves any store registered in `store_sync::REGISTRY` (teams,
        // incidents, apikeys, webhooks, databases, domains, integrations,
        // gitops, docs, notifications, identity, enterprise). Supersedes the
        // two bespoke arms above (kept only as rolling-upgrade compat shims for
        // followers still on the pre-registry build; remove once the fleet is
        // uniform). See `crate::store_sync`.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/store-snapshot/") => {
            let name = p["/v1/store-snapshot/".len()..]
                .split(['?', '/'])
                .next()
                .unwrap_or("");
            crate::store_sync::serve(cloud, name)
        }
        // TLS bundle distribution over the authenticated mesh (see acme.rs::
        // bundle_for_mesh — key decrypted in transit inside peer-authenticated
        // QUIC only; receiver re-encrypts with its own node key).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/tls/bundle") => {
            let name = p
                .split_once("name=")
                .map(|(_, n)| n.split('&').next().unwrap_or(n))
                .unwrap_or("");
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
        // billing meter loop on the coordinator fans this out to sum fleet usage
        // ACROSS EVERY TENANT (main.rs's billing-meter loop builds its own
        // per-tenant totals from the unfiltered list via `FunctionStats.tenant`).
        // Calls `c.fluid.stats()` directly rather than the dashboard-facing
        // `admin::functions` handler, which is now tenant-scoped to a single
        // caller (matching `databases_list`) — this internal fan-out needs the
        // full, unfiltered per-node list instead. Prefix match: `fetch_from_host`
        // appends `?team=` so an exact arm misses.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/functions") => {
            serde_json::to_vec(&cloud.fluid.stats()).unwrap_or_default()
        }
        // Tenant's LOCAL custom domains for the fleet-aggregation fan-out (domains
        // live in node-local ProjectSettings; the coordinator merges each host's).
        // Team rides as `?team=`; always local-only (the caller already fanned out).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/domains") => {
            jb(crate::admin::domains_list(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Query(crate::admin::LocalQ { local: Some(true) }),
            )
            .await)
        }
        "/v1/project-delete/capabilities" if method == hive_p2p::GOSSIP_GET => {
            serde_json::to_vec(&serde_json::json!({ "delete_generation": "v1" }))
                .unwrap_or_default()
        }
        // Relocation reap (single hop): remove THIS node's superseded
        // PRODUCTION-lane records of the project — previews and settings stay.
        // A deliberately separate arm from the full delete below so a
        // pre-upgrade peer answers NO_HANDLER (safe no-op) instead of running
        // the full teardown the reaper used to trigger (which is how preview
        // records and node-local team tags were being destroyed fleet-wide).
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/projects/")
            && p.contains("/reap-deployments") =>
        {
            let project = p
                .trim_start_matches("/v1/projects/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            let team = team_claims(p)
                .map(|axum::Extension(cl)| crate::admin::norm(&cl.tenant).to_string())
                .or_else(|| {
                    if crate::auth::enforced() {
                        None
                    } else {
                        qparam(p, "team")
                    }
                })
                .unwrap_or_default();
            let owner = cloud.projects.team_of(&project);
            let owns_settings = !team.is_empty() && crate::admin::norm(&owner) == team;
            let owns_deploys = !team.is_empty()
                && cloud.gw.list().iter().any(|d| {
                    d.project == project && crate::admin::record_tenant(&d.tenant) == team
                });
            if project.is_empty() || !(owns_settings || owns_deploys) {
                return serde_json::to_vec(&serde_json::json!({ "error": "not owner" }))
                    .unwrap_or_default();
            }
            let reaped = crate::admin::reap_deployments_local(&cloud, &project).await;
            serde_json::to_vec(&serde_json::json!({ "project": project, "reaped": reaped }))
                .unwrap_or_default()
        }
        // Mesh domain activation / detach for owner nodes reachable only over
        // iroh (the HTTP-admin-less fleet reality). Ordered LONGEST-PREFIX-
        // FIRST so the /detach arm can't shadow the /activate one. Both run
        // the same local helpers as the HTTP arms; team must own the project
        // on this node's evidence (settings tag or a deployment record).
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/projects/")
            && p.contains("/domains/")
            && p.ends_with("/detach") =>
        {
            let rest = p
                .trim_start_matches("/v1/projects/")
                .trim_end_matches("/detach")
                .trim_end_matches('/');
            let mut parts = rest.splitn(2, "/domains/");
            let project = parts.next().unwrap_or("").to_string();
            let domain = parts
                .next()
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("")
                .to_string();
            let team = team_claims(p)
                .map(|axum::Extension(cl)| crate::admin::norm(&cl.tenant).to_string())
                .or_else(|| {
                    if crate::auth::enforced() {
                        None
                    } else {
                        qparam(p, "team")
                    }
                })
                .unwrap_or_default();
            let owner = cloud.projects.team_of(&project);
            let owns = !team.is_empty()
                && (crate::admin::norm(&owner) == team
                    || cloud.gw.list().iter().any(|d| {
                        d.project == project && crate::admin::record_tenant(&d.tenant) == team
                    }));
            if project.is_empty() || domain.is_empty() || !owns {
                return serde_json::to_vec(&serde_json::json!({ "error": "not owner" }))
                    .unwrap_or_default();
            }
            cloud.gw.remove_alias(&domain);
            crate::persist::persist(&cloud);
            serde_json::to_vec(&serde_json::json!({
                "domain": domain,
                "project": project,
                "detached": true,
            }))
            .unwrap_or_default()
        }
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/projects/")
            && p.contains("/domains/")
            && p.ends_with("/activate") =>
        {
            let rest = p
                .trim_start_matches("/v1/projects/")
                .trim_end_matches("/activate")
                .trim_end_matches('/');
            let mut parts = rest.splitn(2, "/domains/");
            let project = parts.next().unwrap_or("").to_string();
            let domain = parts
                .next()
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("")
                .to_string();
            let team = team_claims(p)
                .map(|axum::Extension(cl)| crate::admin::norm(&cl.tenant).to_string())
                .or_else(|| {
                    if crate::auth::enforced() {
                        None
                    } else {
                        qparam(p, "team")
                    }
                })
                .unwrap_or_default();
            let owner = cloud.projects.team_of(&project);
            let owns = !team.is_empty()
                && (crate::admin::norm(&owner) == team
                    || cloud.gw.list().iter().any(|d| {
                        d.project == project && crate::admin::record_tenant(&d.tenant) == team
                    }));
            if project.is_empty() || domain.is_empty() || !owns {
                return serde_json::to_vec(&serde_json::json!({ "error": "not owner" }))
                    .unwrap_or_default();
            }
            // The stored challenge must belong to THIS attach — the arm
            // applies routing for whatever challenge is stored, and a
            // mismatched one would pin another attach's proof onto this
            // project (the HTTP arm's binding, applied fleet-side too).
            if !cloud
                .domains
                .verify_of(&domain)
                .is_some_and(|v| v.project == project)
            {
                return serde_json::to_vec(&serde_json::json!({
                    "error": "no verification challenge links domain to project"
                }))
                .unwrap_or_default();
            }
            serde_json::to_vec(
                &crate::admin::domain_activate_local(&cloud, &project, &domain).await,
            )
            .unwrap_or_default()
        }
        // Mesh project-delete cascade (single hop): the coordinator's cross-node
        // teardown for hosting nodes reachable only over iroh. Team must OWN the
        // project on THIS node (or the project must be absent — idempotent).
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/projects/")
            && p.contains("/delete") =>
        {
            let project = p
                .trim_start_matches("/v1/projects/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            // Same verified-token parse as the settings arm: under enforcement
            // the query is `team=<t>&tok=<jwt>`, so the old raw
            // `split_once("?team=")` swallowed the token into the team string
            // and the ownership compares below always missed — silently
            // breaking the cross-node delete cascade on enforced fleets.
            let team = team_claims(p)
                .map(|axum::Extension(cl)| crate::admin::norm(&cl.tenant).to_string())
                .or_else(|| {
                    if crate::auth::enforced() {
                        None
                    } else {
                        qparam(p, "team")
                    }
                })
                .unwrap_or_default();
            let owner = cloud.projects.team_of(&project);
            let owns_settings = !team.is_empty() && crate::admin::norm(&owner) == team;
            let owns_deploys = !team.is_empty()
                && cloud.gw.list().iter().any(|d| {
                    d.project == project && crate::admin::record_tenant(&d.tenant) == team
                });
            let owns_relational = !team.is_empty()
                && crate::relational::projects_for_team(&team)
                    .await
                    .iter()
                    .any(|p| p == &project);
            if project.is_empty() || !(owns_settings || owns_deploys || owns_relational) {
                return serde_json::to_vec(&serde_json::json!({ "error": "not owner" }))
                    .unwrap_or_default();
            }
            let delete_ms = match qparam(p, "delete_ms") {
                Some(raw) => match raw
                    .parse::<u64>()
                    .ok()
                    .filter(|ms| crate::admin::valid_delete_generation(*ms))
                {
                    Some(ms) => ms,
                    None => {
                        return serde_json::to_vec(
                            &serde_json::json!({ "error": "invalid delete generation" }),
                        )
                        .unwrap_or_default()
                    }
                },
                None => {
                    return serde_json::to_vec(
                        &serde_json::json!({ "error": "delete generation required" }),
                    )
                    .unwrap_or_default()
                }
            };
            let outcome =
                crate::admin::delete_project_local(&cloud, &project, &team, delete_ms).await;
            // Echo the STORED generation, never the requested one: a receiver
            // that clamped an over-ceiling stamp must not let the coordinator
            // record acceptance of a value this node did not store — the
            // fleet would split on which records the generation dominates
            // (adversarial finding).
            let stored_ms = cloud.projects.tombstone_of(&project).unwrap_or(delete_ms);
            serde_json::to_vec(&serde_json::json!({
                "project": project,
                "delete_ms": stored_ms,
                "removed": outcome.removed.len(),
                "recreated": outcome.recreated,
            }))
            .unwrap_or_default()
        }
        "/v1/fleet-deployments" => jb(crate::admin::fleet_deployments(State(cloud.clone())).await),
        "/v1/leases" => jb(crate::admin::leases_get(State(cloud.clone())).await),
        // design-head-cid-exchange-rpc: per-namespace GuardianDB HEAD map (key,
        // content-hash, timestamp — never value bytes) for the anti-entropy
        // loop's peer-diff round. Same unauthenticated-read posture as
        // /v1/serve-hosts and /v1/leases above.
        "/v1/guardian/heads" => jb(crate::admin::guardian_heads(State(cloud.clone())).await),
        #[cfg(feature = "zkauth")]
        "/v1/zkauth/roster-export" => jb(crate::zkauth::roster_export().await),
        // Mint a preview-membership proof ON THIS NODE for a peer that received
        // the unlock request but doesn't serve the preview host. A ring proof
        // only verifies against the exact ring it was signed over, so the mint
        // has to happen where the verification will (see `zkauth::preview_proof`).
        // Modelled as a GET because minting is idempotent, which lets it reuse
        // the existing cross-node read helper rather than a new POST path.
        #[cfg(feature = "zkauth")]
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/zkauth/mint") => {
            let team = qparam(p, "team")
                .map(|v| crate::zkauth::urldecode(&v))
                .unwrap_or_default();
            let user = qparam(p, "user")
                .map(|v| crate::zkauth::urldecode(&v))
                .unwrap_or_default();
            let project = qparam(p, "project")
                .map(|v| crate::zkauth::urldecode(&v))
                .unwrap_or_default();
            jb(crate::zkauth::mint_rpc(&team, &user, &project).await)
        }
        // Deploy FANOUT over the mesh: a NAT'd coordinator (no HTTP path to FC nodes,
        // SSH tunnels cut) dispatches the per-target build here. Team rides as `?team=`
        // since the iroh transport carries no HTTP headers.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/git/deploy") => {
            let team = p
                .split_once("?team=")
                .map(|(_, t)| t.to_string())
                .unwrap_or_default();
            let mut headers = axum::http::HeaderMap::new();
            if let Ok(hv) = axum::http::HeaderValue::from_str(&team) {
                headers.insert("x-hive-team", hv);
            }
            match serde_json::from_slice::<fluid_core::GitDeployRequest>(body) {
                Ok(req) => match crate::admin::git_deploy(
                    State(cloud.clone()),
                    headers,
                    team_claims(p),
                    axum::Json(req),
                )
                .await
                {
                    Ok(j) => jb(j),
                    Err((_, msg)) => {
                        serde_json::to_vec(&serde_json::json!({ "error": msg })).unwrap_or_default()
                    }
                },
                Err(_) => Vec::new(),
            }
        }
        // Instant rollback to a placed deployment: the coordinator proxies the
        // promote (a mutation) to the host node over the mesh. The host runs it
        // locally (it holds the deployment), so there's no re-proxy loop.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/deployments/")
            && p.contains("/promote") =>
        {
            let id = p
                .trim_start_matches("/v1/deployments/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            match crate::admin::dep_promote(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Path(id),
            )
            .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Network-settings edit forward: the requesting node proxies a
        // body-carrying `PUT /v1/projects/:project/network` mutation to the
        // project's actual hosting node over the mesh (deployment records
        // are node-local — see `admin::project_network_put`'s doc comment
        // and `admin::put_to_host`). Body is the validated
        // `NetworkPutBody` JSON; team rides via `?team=`/`?tok=` since the
        // mesh transport carries no HTTP headers. The host runs it locally
        // (it holds the record), so there's no re-proxy loop.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/projects/")
            && p.contains("/network") =>
        {
            let project = p
                .trim_start_matches("/v1/projects/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            let parsed: crate::admin::NetworkPutBody = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            match crate::admin::project_network_put(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Path(project),
                axum::Json(parsed),
            )
            .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Build status/log polling for the fanout mirror (coordinator streams the
        // target's build into its own record so the dashboard UX is unchanged).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/builds/") => {
            let id = p
                .trim_start_matches("/v1/builds/")
                .split('?')
                .next()
                .unwrap_or("")
                .to_string();
            match crate::admin::build_get(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Path(id),
            )
            .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Per-deployment + per-project READ VIEWS the coordinator proxies to the host
        // node over the mesh (the NAT'd coordinator has no HTTP admin path to FC nodes).
        // Team rides as `?team=`. The host serves these LOCALLY (it hosts the project),
        // and the coordinator always requests `local=true` so there's no re-proxy loop.
        p if method == hive_p2p::GOSSIP_GET
            && p.starts_with("/v1/deployments/")
            && p.contains("/resources") =>
        {
            let id = p
                .trim_start_matches("/v1/deployments/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            jb(crate::admin::deployment_resources(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Path(id),
            )
            .await)
        }
        // Project settings for a project whose (node-local, never-gossiped) row
        // lives on THIS node — proxied by whichever node answered the dashboard
        // read without a local row. Ownership is checked against THIS node's row
        // or its own hosted deployments' tenant tags (the delete-cascade arm's
        // exact gate); serving is local-only so there is no re-proxy loop.
        // Preview card for a project hosted on THIS node, answered for a peer
        // that received the dashboard read but doesn't host the deployment.
        // Ownership is enforced by `project_preview` itself from the verified
        // delegation token, exactly as the `/settings` arm below does.
        p if method == hive_p2p::GOSSIP_GET
            && p.starts_with("/v1/projects/")
            && p.contains("/preview") =>
        {
            let project = p
                .trim_start_matches("/v1/projects/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            let hdrs = axum::http::HeaderMap::new();
            let claims = team_claims(p);
            jb(crate::admin::project_preview(
                State(cloud.clone()),
                hdrs,
                claims,
                axum::extract::Path(project),
            )
            .await)
        }
        p if method == hive_p2p::GOSSIP_GET
            && p.starts_with("/v1/projects/")
            && p.contains("/settings") =>
        {
            let project = p
                .trim_start_matches("/v1/projects/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            // Tenant from the VERIFIED mesh delegation token (`?tok=`), never a
            // raw `?team=` parse — under enforcement the query is
            // `team=<t>&tok=<jwt>`, so `split_once("?team=")` swallows the
            // token into the team string and every ownership compare misses.
            let team = team_claims(p)
                .map(|axum::Extension(cl)| crate::admin::norm(&cl.tenant).to_string())
                .or_else(|| {
                    if crate::auth::enforced() {
                        None
                    } else {
                        qparam(p, "team")
                    }
                })
                .unwrap_or_default();
            if project.is_empty()
                || team.is_empty()
                || !crate::admin::project_owned_by(&cloud, &project, &team)
            {
                return Vec::new();
            }
            serde_json::to_vec(&serde_json::json!(cloud.projects.get_masked(&project)))
                .unwrap_or_default()
        }
        // Build record + logs for a deployment hosted on THIS node (proxied by the
        // coordinator when the deployment was placed here — build logs live where it built).
        p if method == hive_p2p::GOSSIP_GET
            && p.starts_with("/v1/deployments/")
            && p.contains("/build") =>
        {
            let id = p
                .trim_start_matches("/v1/deployments/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            match crate::admin::deployment_build(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Path(id),
            )
            .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Intelligent service graph for a deployment hosted on THIS node (scanned here).
        p if method == hive_p2p::GOSSIP_GET
            && p.starts_with("/v1/deployments/")
            && p.contains("/service-graph") =>
        {
            let id = p
                .trim_start_matches("/v1/deployments/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            match crate::admin::deployment_service_graph(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Path(id),
            )
            .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Project's latest service graph, served LOCALLY (the coordinator fans this
        // out to the node that built the project). Local-only → no re-proxy loop.
        p if method == hive_p2p::GOSSIP_GET
            && p.starts_with("/v1/projects/")
            && p.contains("/service-graph") =>
        {
            let project = p
                .trim_start_matches("/v1/projects/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            let team = qparam(p, "team").unwrap_or_default();
            // Only return it to the owning tenant (this node built it, so it has the
            // project→team mapping). Empty team = trusted/no-scope caller.
            let owner = cloud.projects.team_of(&project);
            let owner_ns = if owner.trim().is_empty() {
                "personal".to_string()
            } else {
                owner
            };
            let team_ns = if team.trim().is_empty() {
                String::new()
            } else {
                team
            };
            // On-demand scan if not yet stored (backfills existing/failed deployments).
            match crate::admin::local_project_graph(&cloud, &project).await {
                Some(g) if team_ns.is_empty() || team_ns == owner_ns => {
                    jb(axum::Json(serde_json::json!(g)))
                }
                _ => Vec::new(),
            }
        }
        // A single run's DETAIL — must match before the runs LIST arm below.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows/runs/") => {
            let id = p
                .trim_start_matches("/v1/workflows/runs/")
                .split('?')
                .next()
                .unwrap_or("")
                .to_string();
            match crate::admin::wf_run_detail(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Path(id),
                wf_query(p),
            )
            .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows/runs") => {
            jb(crate::admin::wf_runs(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                wf_query(p),
            )
            .await)
        }
        // Workflow summary rollup — must match before the generic `/v1/workflows` arm.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows/summary") => {
            jb(crate::admin::wf_summary(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                wf_query(p),
            )
            .await)
        }
        // Workflow hooks list — must match before the generic `/v1/workflows` arm.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows/hooks") => {
            jb(crate::admin::wf_hooks(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                wf_query(p),
            )
            .await)
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/workflows") => {
            jb(crate::admin::wf_list(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                wf_query(p),
            )
            .await)
        }
        // Request/routing event log: recorded on the SERVING node, so the coordinator
        // proxies here to read a placed project's logs. `local=true` rides in the query
        // so this node returns only its own events (no re-fan-out → no loop).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/logs") => jb(crate::admin::logs(
            State(cloud.clone()),
            team_headers(p),
            team_claims(p),
            logs_query(p),
        )
        .await),
        // Cron fan-out: a peer answering `cron_list`'s fleet merge returns ONLY
        // its own jobs (local=true) for the requesting team. Same tenant-scoped
        // shape as /v1/logs above.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/cron") => {
            jb(crate::admin::cron_list(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Query(crate::admin::LocalQ { local: Some(true) }),
            )
            .await)
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
        p if method == hive_p2p::GOSSIP_GET
            && p.split('?').next().unwrap_or(p).contains("/sandboxes") =>
        {
            let (project, segs) = sandbox_path_project_and_segs(p);
            let project = project.to_string();
            let state = State(cloud.clone());
            let hdrs = team_headers(p);
            let clm = team_claims(p);
            match segs.len() {
                0 => match crate::sandboxes_api::list_sandboxes(
                    state,
                    hdrs,
                    clm,
                    axum::extract::Path(project),
                )
                .await
                {
                    Ok(j) => jb(j),
                    Err(_) => Vec::new(),
                },
                1 => {
                    let sandbox_id = segs[0].to_string();
                    match crate::sandboxes_api::get_sandbox(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, sandbox_id)),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                2 if segs[1] == "commands" => {
                    let sandbox_id = segs[0].to_string();
                    match crate::sandboxes_api::list_commands(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, sandbox_id)),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                2 if segs[1] == "snapshots" => {
                    let sandbox_id = segs[0].to_string();
                    match crate::sandboxes_api::list_snapshots(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, sandbox_id)),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                2 if segs[1] == "mounts" => {
                    let sandbox_id = segs[0].to_string();
                    match crate::sandboxes_api::list_mounts(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, sandbox_id)),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                2 if segs[1] == "domain" => {
                    let sandbox_id = segs[0].to_string();
                    let port: u16 = qparam(p, "port").and_then(|s| s.parse().ok()).unwrap_or(0);
                    match crate::sandboxes_api::get_domain(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, sandbox_id)),
                        axum::extract::Query(crate::sandboxes_api::DomainQuery { port }),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                3 if segs[1] == "commands" => {
                    let sandbox_id = segs[0].to_string();
                    let command_id = segs[2].to_string();
                    match crate::sandboxes_api::get_command(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, sandbox_id, command_id)),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                3 if segs[1] == "files" && segs[2] == "read" => {
                    let sandbox_id = segs[0].to_string();
                    let q = crate::sandboxes_api::ReadFileQuery {
                        path: qparam(p, "path").unwrap_or_default(),
                    };
                    match crate::sandboxes_api::read_file(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, sandbox_id)),
                        axum::extract::Query(q),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                4 if segs[1] == "commands" && segs[3] == "logs" => {
                    let sandbox_id = segs[0].to_string();
                    let command_id = segs[2].to_string();
                    match crate::sandboxes_api::get_command_logs(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, sandbox_id, command_id)),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                _ => Vec::new(),
            }
        }
        // Storage broker: snapshot LIST for a deployment hosted on THIS node
        // (proxied here by `storage_api::list_snapshots` when the requesting
        // node doesn't host the deployment itself — see storage_api.rs's
        // module doc on why this is per-node, not leader-forwarded).
        p if method == hive_p2p::GOSSIP_GET
            && p.split('?').next().unwrap_or(p).contains("/deployments/")
            && p.split('?').next().unwrap_or(p).contains("/snapshots") =>
        {
            let (project, deployment_id, _) = snapshot_path_parts(p);
            match crate::storage_api::list_snapshots(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Path((project, deployment_id)),
            )
            .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Storage broker: snapshot CREATE (no trailing id segment) and DELETE
        // (with one) both ride GOSSIP_POST — see `snapshot_path_parts`'s doc
        // comment for why. One consolidated arm, same style as the sandboxes
        // GET arm above, rather than N ordered prefix checks.
        p if method == hive_p2p::GOSSIP_POST
            && p.split('?').next().unwrap_or(p).contains("/deployments/")
            && p.split('?').next().unwrap_or(p).contains("/snapshots") =>
        {
            let (project, deployment_id, snapshot_id) = snapshot_path_parts(p);
            let state = State(cloud.clone());
            let hdrs = team_headers(p);
            let clm = team_claims(p);
            match snapshot_id {
                None => {
                    let req: crate::storage_api::CreateReq = match serde_json::from_slice(body) {
                        Ok(b) => b,
                        Err(_) => return Vec::new(),
                    };
                    match crate::storage_api::create_snapshot(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, deployment_id)),
                        axum::Json(req),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
                Some(snap_id) => {
                    match crate::storage_api::delete_snapshot(
                        state,
                        hdrs,
                        clm,
                        axum::extract::Path((project, deployment_id, snap_id)),
                    )
                    .await
                    {
                        Ok(j) => jb(j),
                        Err(_) => Vec::new(),
                    }
                }
            }
        }
        // Run operations (console 3-dots) forwarded to the project's HOST node
        // over the mesh: the host runs the world write locally (env decrypts
        // here). `local:true` in the body prevents a re-forward loop. Match
        // before any generic `/v1/workflows` arm.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/workflows/runs/")
            && (p.contains("/cancel")
                || p.contains("/replay")
                || p.contains("/reenqueue")
                || p.contains("/wakeup")) =>
        {
            let rest = p
                .trim_start_matches("/v1/workflows/runs/")
                .split('?')
                .next()
                .unwrap_or("");
            let mut it = rest.splitn(2, '/');
            let id = it.next().unwrap_or("").to_string();
            let op = it.next().unwrap_or("");
            let mut parsed: crate::admin::RunOpBody =
                serde_json::from_slice(body).unwrap_or_default();
            parsed.local = Some(true); // host executes without re-forwarding
            match crate::admin::wf_run_op_dispatch(
                cloud,
                &team_headers(p),
                team_claims(p).map(|axum::Extension(c)| c).as_ref(),
                &id,
                op,
                parsed,
            )
            .await
            {
                Ok(axum::Json(v)) => jb(axum::Json(v)),
                Err((_, msg)) => jb(axum::Json(serde_json::json!({ "error": msg }))),
            }
        }
        // Operator-only run event/attributes ops, forwarded to the project's
        // HOST node exactly like the run-op arm above. These are gated by
        // `require_operator_or_internal` (platform_admin), unlike cancel/
        // replay/reenqueue/wakeup — so the synthesized claims here mirror
        // `mesh_operator_claims`'s rationale (a peer reaching this dispatch
        // already passed the P2P trust/admission gate, which is what the
        // ORIGINATING node's `require_operator` check already gated on before
        // it ever forwarded here) while still carrying the real tenant from
        // `team_claims` (needed for the `wf_in_team` ownership check these
        // ops also enforce, unlike the fully-global `require_operator`
        // call sites `mesh_operator_claims` was built for).
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/workflows/runs/")
            && (p.contains("/events") || p.contains("/attributes")) =>
        {
            let rest = p
                .trim_start_matches("/v1/workflows/runs/")
                .split('?')
                .next()
                .unwrap_or("");
            let mut it = rest.splitn(2, '/');
            let id = it.next().unwrap_or("").to_string();
            let op = it.next().unwrap_or("");
            let mut claims =
                team_claims(p)
                    .map(|axum::Extension(c)| c)
                    .unwrap_or(crate::auth::Claims {
                        sub: "mesh-internal".into(),
                        tenant: String::new(),
                        role: "service".into(),
                        iat: 0,
                        exp: 0,
                        platform_admin: false,
                    });
            claims.platform_admin = true;
            match op {
                "events" => {
                    let mut parsed: crate::admin::RunEventBody =
                        serde_json::from_slice(body).unwrap_or_default();
                    parsed.local = Some(true);
                    match crate::admin::wf_run_event_dispatch(
                        cloud,
                        &team_headers(p),
                        Some(&claims),
                        &id,
                        parsed,
                    )
                    .await
                    {
                        Ok(axum::Json(v)) => jb(axum::Json(v)),
                        Err((_, msg)) => jb(axum::Json(serde_json::json!({ "error": msg }))),
                    }
                }
                "attributes" => {
                    let mut parsed: crate::admin::RunAttributesBody =
                        serde_json::from_slice(body).unwrap_or_default();
                    parsed.local = Some(true);
                    match crate::admin::wf_run_attributes_dispatch(
                        cloud,
                        &team_headers(p),
                        Some(&claims),
                        &id,
                        parsed,
                    )
                    .await
                    {
                        Ok(axum::Json(v)) => jb(axum::Json(v)),
                        Err((_, msg)) => jb(axum::Json(serde_json::json!({ "error": msg }))),
                    }
                }
                _ => Vec::new(),
            }
        }
        // SMS egress relay (see push::send_sms): a node whose Textbelt call was
        // rejected for egress geography forwards the send here; this node runs
        // the DIRECT send (never re-relays) and returns {success, error?}.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/push/sms-relay") => {
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(v) => jb(axum::Json(crate::push::sms_relay_exec(cloud, v).await)),
                Err(_) => Vec::new(),
            }
        }
        // Direct-MX carrier-gateway SMS relay: the leader (cloud node, :25
        // blocked) forwards a direct-MX send here to an NA-egress peer (LA Mac,
        // :25 open) whose IP a carrier gateway accepts. Runs the blast locally.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/push/sms-direct-mx") => {
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(v) => jb(axum::Json(crate::push::sms_direct_mx_exec(v).await)),
                Err(_) => Vec::new(),
            }
        }
        // libsql/Hrana OWNER PROXY: a managed SQLite database is a FILE on one
        // node, and every other node forwards here rather than opening a
        // second, empty copy of it. Longest-prefix-first against the
        // `/v1/databases/replica` arm below — the two are disjoint (this one
        // requires the `/hrana-mesh` suffix), but the ordering makes that
        // explicit rather than incidental. The envelope carries the CLIENT's
        // own database bearer, which `hrana::mesh_serve` re-checks against
        // THIS node's replicated record before touching the file (the
        // `proxy_to_owner` re-check precedent), and refuses to re-proxy.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/databases/")
            && p.split('?')
                .next()
                .unwrap_or_default()
                .ends_with("/hrana-mesh") =>
        {
            let id = p
                .split('?')
                .next()
                .unwrap_or_default()
                .trim_start_matches("/v1/databases/")
                .trim_end_matches("/hrana-mesh");
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(env) => jb(axum::Json(crate::hrana::mesh_serve(&cloud, id, &env).await)),
                Err(e) => jb(axum::Json(serde_json::json!({ "error": e.to_string() }))),
            }
        }
        // browser_db libsql/Hrana + Upstash REST OWNER PROXY
        // (bn-browser-db-rest): the exact `/v1/databases/*/hrana-mesh` shape
        // above, addressed by project instead of database id since there is
        // no `Database` record here — `browser_db_rest::mesh_serve`
        // re-derives the elected owner and the credential scope from THIS
        // node's own replicated state before touching the file.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/projects/")
            && p.split('?')
                .next()
                .unwrap_or_default()
                .ends_with("/browser-db/rest-mesh") =>
        {
            let project = p
                .split('?')
                .next()
                .unwrap_or_default()
                .trim_start_matches("/v1/projects/")
                .trim_end_matches("/browser-db/rest-mesh");
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(env) => jb(axum::Json(
                    crate::browser_db_rest::mesh_serve(&cloud, project, &env).await,
                )),
                Err(e) => jb(axum::Json(serde_json::json!({ "error": e.to_string() }))),
            }
        }
        // Cross-region DB replica control (register/remove) over the mesh.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/databases/replica") => {
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(v) => match crate::admin::database_replica(State(cloud.clone()), axum::Json(v))
                    .await
                {
                    Ok(j) => jb(j),
                    Err((_, e)) => jb(axum::Json(serde_json::json!({ "error": e }))),
                },
                Err(e) => jb(axum::Json(serde_json::json!({ "error": e.to_string() }))),
            }
        }
        // Mirrored storage writes over the mesh (replication data plane). The team
        // rides as `?team=` and is applied to the SAME tenant namespace on this
        // replica; these NEVER re-mirror (they're already replicated writes).
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/storage/")
            && (qparam(p, "mirror").as_deref() == Some("1")) =>
        {
            crate::admin::apply_mirrored_write(&cloud, p, body).await;
            jb(axum::Json(serde_json::json!({ "ok": true })))
        }
        // Billing authority reads, proxied here by `admin::fetch_from_host` /
        // `admin::proxy_billing_read` whenever the CALLING node isn't the billing
        // authority (`billing_authority_node`) — this is the OTHER end of that
        // proxy: without these three arms every `/v1/billing*` gossip call fell
        // through to the catch-all `_ => Vec::new()` below, so `fetch_from_host`
        // got back an empty body, failed to parse it as JSON, and the caller
        // silently fell back to ITS OWN local (non-authoritative) billing state —
        // on a perfectly healthy mesh connection, not just during a QUIC flap.
        // Longest-prefix arms first so `/v1/billing/invoices` and
        // `/v1/billing/ledger` don't get shadowed by the bare `/v1/billing` arm.
        // MUST precede the bare `/v1/billing` arm below, which would otherwise
        // shadow it (longest-prefix-first ordering, same as invoices/ledger).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/billing/checkout/") => {
            let id = p
                .trim_start_matches("/v1/billing/checkout/")
                .split('?')
                .next()
                .unwrap_or("")
                .to_string();
            match crate::admin::billing_checkout_get(State(cloud.clone()), axum::extract::Path(id))
                .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/billing/invoices") => jb(
            crate::admin::billing_invoices(State(cloud.clone()), team_headers(p), team_claims(p))
                .await,
        ),
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/billing/ledger") => jb(
            crate::admin::billing_ledger(State(cloud.clone()), team_headers(p), team_claims(p))
                .await,
        ),
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/billing") => jb(
            crate::admin::billing_get(State(cloud.clone()), team_headers(p), team_claims(p)).await,
        ),
        // Fleet-global raw-port allocation coordination (closes the F1
        // node-local-registry-vs-fleet-global-port-space race): the actual
        // claim decision runs ONLY on the control-plane leader — a non-leader
        // receiving this (stale routing, or a peer with an out-of-date view of
        // who the leader is) refuses rather than racing its own local
        // registry against the real leader's. See `raw_ports.rs` module doc
        // and `allocate_raw_ports_coordinated`.
        "/v1/raw-ports/allocate" if method == hive_p2p::GOSSIP_POST => {
            if !cloud.is_control_plane_leader() {
                return jb(axum::Json(
                    serde_json::json!({ "error": "not the control-plane leader" }),
                ));
            }
            #[derive(serde::Deserialize)]
            struct Req {
                project: String,
                manifest: fluid_core::Manifest,
            }
            match serde_json::from_slice::<Req>(body) {
                Ok(req) => {
                    match crate::raw_ports::allocate_raw_ports_records(&req.project, &req.manifest)
                    {
                        Ok(allocs) => jb(axum::Json(serde_json::json!({ "allocs": allocs }))),
                        Err(e) => jb(axum::Json(serde_json::json!({ "error": e.to_string() }))),
                    }
                }
                Err(e) => jb(axum::Json(
                    serde_json::json!({ "error": format!("malformed request: {e}") }),
                )),
            }
        }
        // Mirror of the allocate arm above, for permanent retirement — the
        // `/release` path and additive `released` response field are retained
        // for rolling compatibility, but the allocator never re-grants a
        // retired number. See `release_raw_ports_coordinated`.
        "/v1/raw-ports/release" if method == hive_p2p::GOSSIP_POST => {
            if !cloud.is_control_plane_leader() {
                return jb(axum::Json(
                    serde_json::json!({ "error": "not the control-plane leader" }),
                ));
            }
            #[derive(serde::Deserialize)]
            struct Req {
                project: String,
            }
            match serde_json::from_slice::<Req>(body) {
                Ok(req) => {
                    // Retire only when this leader holds NO live settings row
                    // for the project (tombstoned or absent): retirement is
                    // PERMANENT (numbers are never re-granted), so releasing
                    // a LIVE project's ports on any trusted peer's say-so
                    // would force a port swap nobody asked for (adversarial
                    // finding). The legitimate flow always reaches here after
                    // the project's delete, so a live row is exactly the
                    // attack/bug case.
                    if cloud.projects.get_if_set(&req.project).is_some() {
                        return jb(axum::Json(serde_json::json!({
                            "error": "project is live on this leader — refusing permanent port retirement"
                        })));
                    }
                    let retired = crate::raw_ports::release_raw_ports(&req.project);
                    jb(axum::Json(serde_json::json!({
                        "retired": &retired,
                        "released": &retired,
                    })))
                }
                Err(e) => jb(axum::Json(
                    serde_json::json!({ "error": format!("malformed request: {e}") }),
                )),
            }
        }
        _ => Vec::new(),
    }
}

/// Reconstruct the logs `Query<LimitQ>` from a dispatched path's query string.
fn mesh_tenant(path: &str) -> String {
    team_claims(path)
        .map(|axum::Extension(claims)| crate::admin::norm(&claims.tenant).to_string())
        .or_else(|| {
            if crate::auth::enforced() {
                None
            } else {
                qparam(path, "team")
            }
        })
        .unwrap_or_default()
}

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
    let segs = tail
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    (project, segs)
}

/// Splits a dispatched
/// `/v1/projects/<project>/deployments/<deployment_id>/snapshots[/<snapshot_id>]`
/// path (query string already ignored) into (project, deployment_id, optional
/// snapshot_id). Mirrors `sandbox_path_project_and_segs`'s segment-shape
/// approach: `storage_api`'s create (no trailing id) and delete (with one)
/// both ride `GOSSIP_POST` — the mesh transport has no DELETE verb — so this
/// is how the single consolidated dispatch arm below tells them apart.
fn snapshot_path_parts(path: &str) -> (String, String, Option<String>) {
    let p = path.split('?').next().unwrap_or(path);
    let rest = p.trim_start_matches("/v1/projects/");
    let mut it = rest.splitn(2, "/deployments/");
    let project = it.next().unwrap_or("").to_string();
    let tail = it.next().unwrap_or("");
    let mut it2 = tail.splitn(2, "/snapshots");
    let deployment_id = it2.next().unwrap_or("").to_string();
    let snap = it2.next().unwrap_or("").trim_start_matches('/');
    let snapshot_id = if snap.is_empty() {
        None
    } else {
        Some(snap.to_string())
    };
    (project, deployment_id, snapshot_id)
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
        run_id: qparam(path, "runId"),
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
            let trust_configured = cloud
                .trusted_peer_ids
                .read()
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            let signer_trusted = trust_configured.then(|| {
                signer
                    .as_deref()
                    .map(|s| hive_p2p::peer_trusted(&cloud.trusted_peer_ids, s))
                    .unwrap_or(false)
            });
            if !mesh_mutation_authorized(method, signer_trusted) {
                tracing::warn!(%path, signer = ?signer, "REJECTED mutating gossip: no verified+trusted signer (trust set configured)");
                return serde_json::to_vec(
                    &serde_json::json!({ "error": "untrusted or unsigned mesh caller" }),
                )
                .unwrap_or_default();
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
            // Relay-selection-algorithm: steer toward the peer's own/nearest-peer's
            // live relay_url instead of whatever relay `addr` was cached with — see
            // `relay_hinted_addr`.
            let addr = relay_hinted_addr(cloud, &node_id, &addr);
            // BOUND the iroh attempt: dialing a peer's STALE identity (e.g. after it
            // restarted with a new key) can hang on connect/relay-retry. Without this
            // cap, a dead cached addr would stall the whole gossip loop and the HTTP
            // fallback below would never run — so the node could never re-learn the
            // peer's new address. On timeout/error we drop the stale mapping and fall
            // through to HTTP, which re-learns the fresh addr.
            //
            // ...but that reasoning only holds when the HTTP fallback CAN run. `peer`
            // is used verbatim as a URL prefix below, so a non-URL peer (notably the
            // `seed:<64hex>` form used before a seed has gossiped a full NodeInfo) has
            // no HTTP path at all. Capping those below `dial_fallback_ceiling` cancels
            // `acquire`'s fresh-discovery fallback mid-flight — the same cancellation
            // bug that made mesh join impossible for a new node — and leaves NO
            // recovery path, since the HTTP branch cannot work either. So: keep the
            // fast cap only where the fallback is real, and give the dial its full
            // budget where iroh is the only way through.
            let http_fallback_viable = peer.starts_with("http://") || peer.starts_with("https://");
            let budget = if http_fallback_viable {
                Duration::from_secs(3)
            } else {
                hive_p2p::dial_fallback_ceiling()
            };
            let attempt = tokio::time::timeout(
                budget,
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
    let mut req = if method == hive_p2p::GOSSIP_POST {
        cloud
            .http
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_vec())
    } else {
        cloud.http.get(&url)
    };
    // Live-witnessed real bug (bn-impl-mesh-admission investigation): this path
    // is a plain, unauthenticated reqwest::Client call, but the receiving
    // node's admin router (main.rs) wraps EVERY mutation in auth::require_auth
    // once HIVE_JWT_SECRET is set -- the DOCUMENTED normal production posture.
    // POST /v1/nodes/announce (and any other gossip mutation reached only via
    // this HTTP fallback, before an iroh mapping exists to use the auth-free
    // iroh gossip path instead) 401'd unconditionally, silently disabling the
    // "bootstrap + fallback" this comment promises on any JWT-enforced node.
    // Mint a short-lived mesh-internal token via the SAME mechanism
    // db_replicate.rs's send_mirrored already uses for exactly this class of
    // node-to-node call, and present it as a normal Bearer -- the receiving
    // handlers on this path (node_announce, nodes) don't read claims at all,
    // so any validly-signed token clears require_auth's gate without granting
    // any tenant-scoped privilege. A no-op when HIVE_JWT_SECRET is unset
    // (issue() returns Err, so no header is added -- matching dev/single-node
    // behavior exactly as before).
    if method == hive_p2p::GOSSIP_POST {
        if let Ok(tok) = crate::auth::issue("mesh-internal", "mesh", "service", false, 60) {
            req = req.header("authorization", format!("Bearer {tok}"));
        }
    }
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
        assert!(
            mesh_mutation_authorized(hive_p2p::GOSSIP_POST, Some(true)),
            "a verified, trusted signer must be allowed"
        );
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
        let tok =
            crate::auth::issue_with_secret("mesh-internal", "acme", "service", false, 60, "sekret")
                .unwrap();
        let path = format!("/v1/git/deploy?team=SPOOFED&tok={tok}");
        let (tok_in_path, _) = (super::qparam(&path, "tok").unwrap(), ());
        let claims = crate::auth::verify_with_secret(&tok_in_path, "sekret").unwrap();
        assert_eq!(
            claims.tenant, "acme",
            "signed token tenant is authoritative over the raw param"
        );
        assert!(crate::auth::verify_with_secret("not-a-jwt", "sekret").is_err());

        // The raw `?team=` fallback (no token) still resolves a tenant for
        // dev/rolling-upgrade — this path needs no secret.
        let claims = team_claims("/v1/logs?team=personal")
            .expect("raw team fallback")
            .0;
        assert_eq!(claims.tenant, "personal");
        assert!(
            team_claims("/v1/logs").is_none(),
            "no team and no token yields no claims"
        );
    }
}
