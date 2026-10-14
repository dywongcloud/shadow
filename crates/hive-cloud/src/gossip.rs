//! Control-plane gossip transport selection and mesh dispatch.
//!
//! Gossip (roster, serve-hosts, fleet view, container leases) can ride EITHER the legacy
//! HTTP-over-SSH-tunnel path OR the authenticated iroh QUIC mesh (the transport the data plane
//! uses). iroh is preferred when `HIVE_GOSSIP_IROH` is set AND a peer's iroh address is known
//! (learned from a prior roster exchange); HTTP is always the bootstrap and fallback, so a node
//! with no iroh address yet, or a failed QUIC attempt, still converges. The iroh side reuses the
//! connection-level peer trust gate, so gossip is authenticated by the peer's cryptographic
//! identity rather than by the SSH tunnel.
//!
//! Inbound requests are served by [`dispatch`]: an ordered table of arms that run this node's own
//! admin handlers, the SAME endpoints that answer HTTP gossip, so the two transports are
//! byte-equivalent. Rules every arm relies on: arms match in order and must stay
//! LONGEST-PREFIX-FIRST (the bare `/v1/billing` arm after `/v1/billing/ledger`, `/detach` before
//! its siblings); the transport carries no HTTP headers, so the tenant rides in the query as a
//! signed short-lived delegation token (`?tok=`, verified by [`team_claims`]) with a raw `?team=`
//! only when auth is not enforced; an unknown path answers an empty body, which a sender reads as
//! NO_HANDLER, and that is what makes a new arm safe against a pre-upgrade peer; and a forwarded
//! call is served LOCALLY with no further proxying (`local=true` or a dedicated single-hop arm), so
//! a proxy loop is impossible.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};

use crate::state::CloudState;

fn jb(j: axum::Json<serde_json::Value>) -> Vec<u8> {
    serde_json::to_vec(&j.0).unwrap_or_default()
}

/// Same as [`jb`] for a fallible handler: the error side is serialized as a
/// `{"error": {...}}` envelope carrying the HTTP status, so a peer's refusal
/// (plan gate, malformed filter) reaches the coordinator as a refusal instead
/// of an empty array that reads as "no logs".
fn jb_logs(r: Result<(HeaderMap, axum::Json<serde_json::Value>), (StatusCode, String)>) -> Vec<u8> {
    match r {
        Ok((_hdrs, j)) => jb(j),
        Err((st, msg)) => {
            let body = serde_json::from_str::<serde_json::Value>(&msg).unwrap_or_else(
                |_| serde_json::json!({ "error": { "code": "LOGS_ERROR", "message": msg } }),
            );
            let mut v = body;
            if let Some(o) = v.get_mut("error").and_then(|e| e.as_object_mut()) {
                o.insert("status".into(), serde_json::json!(st.as_u16()));
            }
            serde_json::to_vec(&v).unwrap_or_default()
        }
    }
}

/// Whether iroh is the preferred gossip transport (opt-in; HTTP-over-SSH otherwise).
pub fn iroh_enabled() -> bool {
    std::env::var("HIVE_GOSSIP_IROH")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

/// Active health probe to a specific node over the iroh mesh, addressed DIRECTLY by (node_id, addr)
/// from the registry, bypassing the URL-keyed `peer_iroh` map so the health loop can probe EVERY
/// known peer, not just configured `--peer` targets. Tight timeout and no HTTP fallback (health is
/// a mesh-transport signal: we want to know whether the QUIC path to the peer is alive). Returns
/// round-trip ms on success, `None` on failure or when no mesh transport is bound. It reuses
/// `/v1/nodes` (the path the gossip loop already treats as the liveness signal), so no new endpoint
/// is added. A failed or timed-out probe drops any cached trunk so the NEXT probe re-dials fresh:
/// critical for FAST RECOVERY, because after a peer restarts with new socket addrs the stale QUIC
/// trunk lingers until idle-timeout (~tens of seconds), and evicting it lets the next dial resolve
/// the peer's CURRENT addr via discovery (Seer pkarr) and reconnect within an interval or two.
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
            pool.close_peer(node_id).await;
            None
        }
    }
}

/// Relay-selection wiring: steers `addr_json`'s relay transport toward the best LIVE relay for the
/// peer identified by iroh `node_id`, instead of trusting whatever relay was serialized into the
/// cached hint at gossip time (for a peer only ever learned second- or third-hand it may be stale
/// or simply absent). Preference (see `hive_edge::select_relay_hint`): the target's OWN gossiped
/// `relay_url` > the geographically nearest OTHER healthy peer's `relay_url` > the central
/// [`hive_edge::CENTRAL_RELAY_URL`] backstop (a real `https://<node>.relay.shadw.app:3343` relay,
/// never a port-less name). Every caller of `request_to`/`fetch` inherits it (including
/// `admin::fetch_from_host`, `acme.rs`, `db_replicate.rs` and `git.rs`'s mesh fanout: all dial by
/// `(node_id, addr_json)` through these two functions). It falls back to `addr_json` unchanged when
/// `node_id` is not found in the registry (e.g. a bootstrap seed never gossiped as a full
/// `NodeInfo`).
///
/// The hint is applied ONLY to an addr that carries no relay at all (a peer learned second-hand
/// before it ever gossiped its home relay). A relay already in `addr_json` is the peer's OWN home
/// relay, the one it is actually attached to, and a relay only carries traffic to peers attached to
/// it: steering away from it to `relay_url` (the embedded relay the peer SERVES,
/// `http://<public-ip>:3341`) replaced a live transport with one that is TCP-unreachable between
/// Tencent hosts (the security group, not the listener), so a dial to a peer whose only other path
/// was direct UDP timed out whenever that path flapped. `node_id` is matched against the endpoint
/// id OR the node name: `request_to`'s callers pass a 64-hex `peer_id`, but `fetch` takes its tuple
/// from `peer_iroh`, whose entries for peers learned over the HTTP `/v1/nodes` path hold the node
/// NAME (`ps.id`), which can never equal a hex endpoint id.
fn relay_hinted_addr(cloud: &Arc<CloudState>, node_id: &str, addr_json: &str) -> String {
    if serde_json::from_str::<iroh::EndpointAddr>(addr_json)
        .map(|a| {
            a.addrs
                .iter()
                .any(|t| matches!(t, iroh::TransportAddr::Relay(_)))
        })
        .unwrap_or(false)
    {
        return addr_json.to_string();
    }
    let nodes = cloud.registry.nodes();
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
    request_to_with_response_cap(
        cloud,
        node_id,
        addr,
        method,
        path,
        body,
        timeout_secs,
        16 * 1024 * 1024,
    )
    .await
}

/// Send a gossip request whose response protocol has an explicit larger bound.
/// The default request path remains capped at 16 MiB.
pub async fn request_to_with_response_cap(
    cloud: &Arc<CloudState>,
    node_id: &str,
    addr: &str,
    method: u8,
    path: &str,
    body: &[u8],
    timeout_secs: u64,
    response_cap: usize,
) -> Option<Vec<u8>> {
    let pool = cloud.mesh.read().clone()?;
    let addr = relay_hinted_addr(cloud, node_id, addr);
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        pool.gossip_request_with_response_cap(node_id, &addr, method, path, body, response_cap),
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
    dispatch_verified(cloud, method, path, body, None).await
}

async fn dispatch_verified(
    cloud: &Arc<CloudState>,
    method: u8,
    path: &str,
    body: &[u8],
    signer: Option<&str>,
) -> Vec<u8> {
    match path {
        "/v1/nodes/announce" if method == hive_p2p::GOSSIP_POST => {
            if let Ok(node) = serde_json::from_slice::<hive_edge::NodeInfo>(body) {
                return jb(
                    crate::admin::node_announce(State(cloud.clone()), axum::Json(node)).await,
                );
            }
            Vec::new()
        }
        // Prewarm hint from an entry node that does not host the deployment (public hosts are
        // round-robin DNS): served here only, never re-forwarded, and the local gates in
        // `Gateway::warm_host`/`Fluid::warm` decide whether a start happens.
        "/v1/warm-hint" if method == hive_p2p::GOSSIP_POST => {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
                if let Some(host) = v.get("host").and_then(|h| h.as_str()) {
                    crate::prewarm::handle_remote(cloud.clone(), host.to_string());
                }
            }
            Vec::new()
        }
        "/v1/nodes" => {
            match crate::admin::nodes(State(cloud.clone()), mesh_operator_claims()).await {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // `?since=<gen>` is the incremental form (see `admin::serve_hosts_with`), accepted as on
        // the HTTP route so both transports stay byte-equivalent.
        p if p == "/v1/serve-hosts" || p.starts_with("/v1/serve-hosts?") => {
            let since = p
                .split_once("since=")
                .and_then(|(_, v)| v.split('&').next())
                .and_then(|v| v.parse::<u64>().ok());
            jb(axum::Json(crate::admin::serve_hosts_with(&cloud, since)))
        }
        // Compat shim for pre-registry followers: the full TeamStore as the old bare map,
        // authoritative rows only (publishing the synthetic local fallback would let an older peer
        // strip its provenance and relay it back as real). The generic `/v1/store-snapshot/` arm
        // below supersedes it; remove once the fleet is uniform.
        "/v1/teams/snapshot" if method == hive_p2p::GOSSIP_GET => {
            serde_json::to_vec(&cloud.teams.snapshot_synced().rows).unwrap_or_default()
        }
        // The same compat shim for the IncidentStore snapshot.
        "/v1/incidents/snapshot" if method == hive_p2p::GOSSIP_GET => {
            serde_json::to_vec(&cloud.incidents.snapshot()).unwrap_or_default()
        }
        // Browser artifact BYTES, proxied here when round-robin ingress landed the
        // content-addressed GET on a node that never ran the build. The owner re-runs the tenant
        // gate against ITS deployment state (`mesh_tenant` carries the signed delegation token); an
        // empty reply means not servable.
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
        // shadw drive file bytes: the mesh fallback of the tenant-gated
        // `GET /v1/drive/:project/file` proxy for a NAT'd owner node. The owner re-runs
        // `project_owned_by` against its own replicated state, and an empty tenant (unsigned or
        // expired token) never resolves anything.
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
        // shadw drive share-link bytes: deliberately UNAUTHENTICATED, the token hash itself is the
        // capability (re-derived by the owner from the SAME replicated `drive_shares` row), so
        // there is no tenant to carry.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/drive/mesh-share/") => {
            let token = qparam(p, "token").unwrap_or_default();
            if token.is_empty() {
                Vec::new()
            } else {
                crate::drive_api::mesh_share_get(&token).await
            }
        }
        // The accept-side browser admission recheck is identity-only and must precede the
        // tenant-scoped endpoint arm below.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/admissions/accept/") => {
            let endpoint_id = p
                .trim_start_matches("/v1/browser/admissions/accept/")
                .split('?')
                .next()
                .unwrap_or("");
            crate::browser_admission::mesh_accept(cloud, endpoint_id)
        }
        // Browser admission first-read leader fallback: the endpoint-specific arm must precede the
        // list arm, and the tenant comes from the verified mesh delegation token.
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
        // Fast-path revoke echo: the leader fans it out right after a real revoke so every follower
        // applies the relay-deny and routing-removal side effects within one mesh round trip
        // instead of waiting for the ~60 s store_sync pull. No tenant check (the leader already
        // made the authoritative decision), and it never touches versioned active/tombstone state
        // (see `mark_denied`), so it cannot race or corrupt this follower's own later adopt()
        // ordering.
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
        // The inverse of the revoke echo: fanned out right after a validated admission supersedes a
        // stale denylist entry, so follower relays let the re-admitted endpoint reconnect within
        // one mesh round trip. Denylist-only on the receiving side, with the same "leader already
        // decided" trust posture.
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
        // Fleet<->fleet browser-db anti-entropy (`browser_db::mesh_crr_sync`): body and reply are
        // the raw `Op::CrrSync` encodings, which is why it rides the binary dispatch.
        // Prefix-disjoint from every `/v1/browser/*` arm (note the hyphen).
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/browser-db/mesh-sync/") => {
            let project = p.trim_start_matches("/v1/browser-db/mesh-sync/");
            if project.is_empty() {
                Vec::new()
            } else {
                crate::browser_db::mesh_crr_sync(cloud, project, body).await
            }
        }
        // Presence write-through to the leader (`echo_presence_to_leader`); keep it ahead of any
        // broader `/v1/browser/presence` arm (longest-prefix-first).
        p if method == hive_p2p::GOSSIP_POST && p == "/v1/browser/presence/mesh-echo" => {
            crate::browser_presence::mesh_echo(cloud, body)
        }
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/presence") => {
            let tenant = mesh_tenant(p);
            if tenant.is_empty() {
                Vec::new()
            } else {
                crate::browser_presence::mesh_list(cloud, &tenant)
            }
        }
        // Per-tenant relay byte metering, local slice: the iroh-only-peer path of the fleet fanout.
        // Like `/v1/functions`, the outer request already enforced operator.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/browser/metering") => {
            crate::browser_metering::mesh_local(cloud)
        }
        // Generic leader->follower store snapshot, `GET /v1/store-snapshot/<name>`, for any store
        // in `store_sync::REGISTRY`; it supersedes the two bespoke compat arms above.
        p if method == hive_p2p::GOSSIP_GET
            && p.starts_with(crate::store_sync::SNAPSHOT_PATH_PREFIX) =>
        {
            let name = p[crate::store_sync::SNAPSHOT_PATH_PREFIX.len()..]
                .split(['?', '/'])
                .next()
                .unwrap_or("");
            crate::store_sync::serve(cloud, name)
        }
        // TLS bundle distribution over the authenticated mesh (`acme::bundle_for_mesh`): the key is
        // decrypted in transit inside peer-authenticated QUIC only, and the receiver re-encrypts
        // with its own node key.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/tls/bundle") => {
            let name = p
                .split_once("name=")
                .map(|(_, n)| n.split('&').next().unwrap_or(n))
                .unwrap_or("");
            crate::acme::bundle_for_mesh(&cloud, name)
        }
        // NON-SECRET directory of gateway-addressable DBs hosted on this node (`{id, db_host,
        // host_node, kind}`, no credentials): the DNS leader fans it out to publish per-DB
        // `<slug>.{db_domain}` A records for DBs provisioned on other nodes. Prefix match, because
        // `fetch_from_host` appends `?team=`.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/db-directory") => {
            serde_json::to_vec(&cloud.databases.directory()).unwrap_or_default()
        }
        // Local per-function usage stats (each node meters its own compute) for the coordinator's
        // billing fan-out, UNFILTERED across tenants (the billing meter builds per-tenant totals
        // through `FunctionStats.tenant`): it calls `c.fluid.stats()` directly, not the
        // tenant-scoped dashboard handler `admin::functions`. Prefix match, because
        // `fetch_from_host` appends `?team=`.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/functions") => {
            serde_json::to_vec(&cloud.fluid.stats()).unwrap_or_default()
        }
        // The tenant's LOCAL custom domains for the fleet-aggregation fan-out (domains live in
        // node-local ProjectSettings); always local-only, the caller already fanned out.
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
        // Relocation reap (single hop): removes THIS node's superseded PRODUCTION-lane records of
        // the project; previews and settings stay. It is a separate arm from the full delete below
        // so a pre-upgrade peer answers NO_HANDLER (a safe no-op) instead of running the full
        // teardown, which is how preview records and node-local team tags were once destroyed
        // fleet-wide.
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
            let team = mesh_tenant(p);
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
        // Mesh domain detach/activate for owner nodes reachable only over iroh: the same local
        // helpers as the HTTP arms, and the team must own the project on this node's evidence
        // (settings tag or a deployment record). /detach comes first (longest-prefix-first). Suffix
        // matches and trims run on the query-stripped path: the forwarded path always carries
        // `?team=…` or `?tok=…`.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/projects/")
            && p.contains("/domains/")
            && p.split('?').next().unwrap_or("").ends_with("/detach") =>
        {
            let rest = p
                .split('?')
                .next()
                .unwrap_or("")
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
            let team = mesh_tenant(p);
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
            && p.split('?').next().unwrap_or("").ends_with("/activate") =>
        {
            let rest = p
                .split('?')
                .next()
                .unwrap_or("")
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
            let team = mesh_tenant(p);
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
            // The stored challenge must belong to THIS attach: a mismatched one would pin another
            // attach's proof onto this project (the HTTP arm's binding, applied fleet-side too).
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
        // Mesh project RENAME (single hop): the coordinator's cross-node rewrite for iroh-only
        // nodes. The team must OWN the project on THIS node (settings tag, a local deployment or
        // the relational fallback: the same three tiers the delete arm checks, because a rename
        // hands the caller another project's URLs).
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/projects/")
            && p.split('?').next().unwrap_or("").ends_with("/rename") =>
        {
            let project = p
                .trim_start_matches("/v1/projects/")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            let team = mesh_tenant(p);
            let new = qparam(p, "name")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            // Same shape rules as the HTTP route: a name that sanitizes differently would not match
            // its own volume/checkout lookup prefix.
            if crate::admin::invalid_project_name(&new).is_some() {
                return serde_json::to_vec(&serde_json::json!({ "error": "invalid project name" }))
                    .unwrap_or_default();
            }
            let rename_ms = match qparam(p, "rename_ms") {
                Some(raw) => match raw
                    .parse::<u64>()
                    .ok()
                    .filter(|ms| crate::admin::valid_rename_generation(*ms))
                {
                    Some(ms) => ms,
                    None => {
                        return serde_json::to_vec(
                            &serde_json::json!({ "error": "invalid rename generation" }),
                        )
                        .unwrap_or_default()
                    }
                },
                None => {
                    return serde_json::to_vec(
                        &serde_json::json!({ "error": "rename generation required" }),
                    )
                    .unwrap_or_default()
                }
            };
            if project.is_empty() || new.is_empty() || team.is_empty() {
                return serde_json::to_vec(&serde_json::json!({ "error": "not owner" }))
                    .unwrap_or_default();
            }
            let owner = cloud.projects.team_of(&project);
            let owns_settings = crate::admin::norm(&owner) == team;
            let owns_deploys =
                cloud.gw.list().iter().any(|d| {
                    d.project == project && crate::admin::record_tenant(&d.tenant) == team
                });
            let owns_relational = crate::relational::projects_for_team(&team)
                .await
                .iter()
                .any(|p| p == &project);
            if !(owns_settings || owns_deploys || owns_relational) {
                return serde_json::to_vec(&serde_json::json!({ "error": "not owner" }))
                    .unwrap_or_default();
            }
            let outcome =
                crate::admin::rename_project_local(&cloud, &project, &new, &team, rename_ms).await;
            serde_json::to_vec(&serde_json::json!({
                "project": new,
                "previous": project,
                "rename_ms": outcome.rename_ms,
                "renamed": if outcome.local_renamed { outcome.renamed } else { 0 },
                "local_renamed": outcome.local_renamed,
            }))
            .unwrap_or_default()
        }
        // Mesh project-delete cascade (single hop): the coordinator's cross-node teardown for
        // iroh-only hosting nodes. The team must OWN the project on THIS node, or the project must
        // be absent (idempotent).
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
            let team = mesh_tenant(p);
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
            // Echo the STORED generation, never the requested one: a receiver that clamped an
            // over-ceiling stamp must not let the coordinator record acceptance of a value this
            // node did not store, or the fleet would split on which records the generation
            // dominates.
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
        // Per-namespace GuardianDB HEAD map (key, content hash, timestamp, never value bytes) for
        // the anti-entropy loop's peer diff; the same unauthenticated-read posture as
        // `/v1/serve-hosts` and `/v1/leases` above.
        "/v1/guardian/heads" => jb(crate::admin::guardian_heads(State(cloud.clone())).await),
        #[cfg(feature = "zkauth")]
        "/v1/zkauth/roster-export" => jb(crate::zkauth::roster_export().await),
        // Mints a preview-membership proof ON THIS NODE for a peer that received the unlock request
        // but does not serve the preview host: a ring proof only verifies against the exact ring it
        // was signed over (see `zkauth::preview_proof`). Modelled as a GET because minting is
        // idempotent, which reuses the existing cross-node read helper.
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
        // Bounded immutable runtime-artifact transfer. The exact versioned paths must stay ahead of
        // every broader runtime/deploy arm: an old peer answers an unknown path with an empty body,
        // which the sender treats as NO_HANDLER instead of letting a legacy deploy handler rebuild
        // source. Unlike `team_claims`, this arm accepts only a verified short-lived service token
        // and the transport's verified endpoint signer.
        p if method == hive_p2p::GOSSIP_POST
            && matches!(
                p.split('?').next(),
                Some("/v1/runtime-artifact-transfer/v1/begin")
                    | Some("/v1/runtime-artifact-transfer/v1/chunk")
                    | Some("/v1/runtime-artifact-transfer/v1/query")
                    | Some("/v1/runtime-artifact-transfer/v1/finalize")
                    | Some("/v1/runtime-artifact-transfer/v1/abort")
            ) =>
        {
            let operation = p
                .split('?')
                .next()
                .and_then(|path| path.rsplit('/').next())
                .unwrap_or_default();
            let token = qparam(p, "tok");
            crate::runtime_artifact_transfer::mesh_dispatch(
                cloud,
                operation,
                signer,
                token.as_deref(),
                body,
            )
            .await
        }
        // Deploy FANOUT over the mesh: a NAT'd coordinator (no HTTP path to FC nodes) dispatches
        // the per-target build here, with the team in `?team=`. The versioned path is deliberately
        // outside `/v1/git/deploy`, so a pre-protocol binary's broad legacy prefix arm cannot
        // consume a deployment whose runtime artifact/workdir semantics it does not understand.
        p if method == hive_p2p::GOSSIP_POST
            && matches!(
                p.split('?').next(),
                Some("/v1/runtime-artifact/v1/git/deploy") | Some("/v1/git/deploy")
            ) =>
        {
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
        // Region-aware database placement fanout: `admin::database_create` proxies the provisioning
        // here when it chose a DIFFERENT node than the one that received the request (podman
        // containers only run where the process that spawns them lives, so relabeling `host_node`
        // alone would claim a node that never got any containers). It never re-proxies, and it is
        // mesh-only in practice because admin ports are loopback-only fleet-wide. Gated by the
        // mesh's own peer-trust boundary; `x-hive-internal` is set anyway so
        // `require_operator_or_internal` passes without relying on synthesized claims.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/internal/databases/provision-local") =>
        {
            let headers = internal_hop_headers();
            match serde_json::from_slice::<crate::databases::ProvisionReq>(body) {
                Ok(req) => match crate::admin::database_provision_local(
                    State(cloud.clone()),
                    headers,
                    None,
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
        // Sandbox owner hops (leader -> owner), the mesh half of `sandboxes_api::internal_hop`: a
        // delegated CREATE on a node whose backend can exec (the leader's cannot), and the ONE
        // typed owner RPC every cell-touching mutation rides (stop/delete/run/kill). Both handlers
        // re-derive authority on the receiving node (fleet service credential and tenant binding
        // via `require_internal_hop`, owner == self via the record) and never re-proxy. A handler
        // refusal is wrapped in `refused_envelope` so the leader can tell a typed "no" (nothing
        // applied) from a dead transport (unknown). Same `x-hive-internal` posture as the
        // provision-local arm.
        p if method == hive_p2p::GOSSIP_POST
            && matches!(
                p.split('?').next(),
                Some(crate::sandboxes_api::DELEGATE_CREATE_PATH)
                    | Some(crate::sandboxes_api::OWNER_OP_PATH)
            ) =>
        {
            let headers = internal_hop_headers();
            let is_create = p.split('?').next() == Some(crate::sandboxes_api::DELEGATE_CREATE_PATH);
            let parsed = match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            let outcome = if is_create {
                match serde_json::from_value(parsed) {
                    Ok(req) => {
                        crate::sandboxes_api::delegate_create_sandbox(
                            State(cloud.clone()),
                            headers,
                            team_claims(p),
                            axum::Json(req),
                        )
                        .await
                    }
                    Err(e) => Err((
                        axum::http::StatusCode::BAD_REQUEST,
                        serde_json::json!({ "error": format!("bad delegate body: {e}") })
                            .to_string(),
                    )),
                }
            } else {
                match serde_json::from_value(parsed) {
                    Ok(req) => {
                        crate::sandboxes_api::owner_op(
                            State(cloud.clone()),
                            headers,
                            team_claims(p),
                            axum::Json(req),
                        )
                        .await
                    }
                    Err(e) => Err((
                        axum::http::StatusCode::BAD_REQUEST,
                        serde_json::json!({ "error": format!("bad owner-op body: {e}") })
                            .to_string(),
                    )),
                }
            };
            match outcome {
                Ok(j) => jb(j),
                Err((status, body)) => crate::sandboxes_api::refused_envelope(status, &body),
            }
        }
        // Companion of the provision-local arm: `admin::database_delete` proxies
        // backing-container/volume teardown here when the record's `host_node` is a peer (see
        // `teardown_db_backing`). It never re-proxies.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/internal/databases/teardown-local") =>
        {
            let headers = internal_hop_headers();
            match serde_json::from_slice::<String>(body) {
                Ok(id) => match crate::admin::database_teardown_local(
                    State(cloud.clone()),
                    headers,
                    None,
                    axum::Json(id),
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
        // Instant rollback to a placed deployment: the coordinator proxies the promote (a mutation)
        // to the host node, which runs it locally (it holds the deployment), so there is no
        // re-proxy loop.
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
        // Network-settings edit forward: the requesting node proxies the body-carrying
        // `PUT /v1/projects/:project/network` to the project's hosting node (deployment records are
        // node-local, see `admin::project_network_put` and `admin::put_to_host`). The body is the
        // validated `NetworkPutBody`, the team rides via `?team=`/`?tok=`, and the host runs it
        // locally.
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
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/gitops/projects") => {
            let query = p.split_once('?').map(|(_, q)| q).unwrap_or("");
            let view = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("view="))
                .unwrap_or("")
                .to_string();
            match crate::admin::gitops_projects(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Query(crate::admin::GitopsProjectsQuery {
                    view: Some(view),
                    local: Some(true),
                }),
            )
            .await
            {
                Ok(j) => jb(j),
                Err(_) => Vec::new(),
            }
        }
        // Build status/log polling for the fanout mirror (the coordinator streams the target's
        // build into its own record).
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
        // Per-deployment and per-project READ VIEWS the coordinator proxies to the host node (a
        // NAT'd coordinator has no HTTP admin path to FC nodes). The host serves them LOCALLY and
        // the coordinator always requests `local=true`, so there is no re-proxy loop.
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
        // Preview card for a project hosted on THIS node, answered for a peer that received the
        // dashboard read; `project_preview` enforces ownership itself from the verified delegation
        // token.
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
        // Project settings for a project whose (node-local, never-gossiped) row lives on THIS node,
        // proxied by whichever node answered the dashboard read without a local row. Ownership is
        // checked against this node's row or its own hosted deployments' tenant tags (the
        // delete-cascade arm's exact gate), and serving is local-only, so there is no re-proxy
        // loop.
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
            let team = mesh_tenant(p);
            if project.is_empty()
                || team.is_empty()
                || !crate::admin::project_owned_by(&cloud, &project, &team)
            {
                return Vec::new();
            }
            serde_json::to_vec(&serde_json::json!(cloud.projects.get_masked(&project)))
                .unwrap_or_default()
        }
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
            // Only returned to the owning tenant (this node built it, so it has the project->team
            // mapping); an empty team is a trusted/no-scope caller.
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
        // Request/routing event log, recorded on the SERVING node: the coordinator proxies here to
        // read a placed project's logs, with `local=true` in the query so this node returns only
        // its own events (no re-fan-out, no loop).
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/logs") => jb_logs(
            crate::admin::logs(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                logs_query(p),
            )
            .await,
        ),
        // Cron fan-out: a peer answering `cron_list`'s fleet merge returns ONLY its own jobs
        // (`local=true`) for the requesting team, the same tenant-scoped shape as /v1/logs.
        p if method == hive_p2p::GOSSIP_GET && p.starts_with("/v1/cron") => {
            jb(crate::admin::cron_list(
                State(cloud.clone()),
                team_headers(p),
                team_claims(p),
                axum::extract::Query(crate::admin::LocalQ { local: Some(true) }),
            )
            .await)
        }
        // Sandbox READS: every sandbox mutation forwards through admin_ingress to the single
        // current control-plane owner, so that node alone holds sandbox state, and
        // `sandboxes_api`'s `fetch_from_host` fallback reaches it over this arm. One consolidated
        // arm dispatches on segment shape rather than N ordered prefix checks, so there is no
        // most-specific-first ordering hazard as sandbox routes grow.
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
        // Storage broker: snapshot LIST for a deployment hosted on THIS node (per-node, not
        // leader-forwarded: see storage_api.rs's module doc).
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
        // Storage broker: snapshot CREATE (no trailing id) and DELETE (with one) both ride
        // GOSSIP_POST (see `snapshot_path_parts`); one consolidated arm, like the sandbox GET arm.
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
        // Run operations (console 3-dots) forwarded to the project's HOST node, which runs the
        // world write locally (env decrypts there); `local:true` in the body prevents a re-forward
        // loop. Match before any generic `/v1/workflows` arm.
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
            parsed.local = Some(true);
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
        // Operator-only run event/attributes ops, forwarded to the project's HOST node like the
        // run-op arm above. They are gated by `require_operator_or_internal` (platform_admin), so
        // the claims are synthesized as in `mesh_operator_claims` (a peer reaching this dispatch
        // already passed the P2P trust/admission gate that the ORIGINATING node's
        // `require_operator` relied on) while still carrying the real tenant from `team_claims`,
        // which the `wf_in_team` ownership check these ops also enforce needs.
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
        // SMS egress relay (see `push::send_sms`): a node whose Textbelt call was rejected for
        // egress geography forwards the send here; this node runs the DIRECT send (never re-relays)
        // and returns `{success, error?}`.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/push/sms-relay") => {
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(v) => jb(axum::Json(crate::push::sms_relay_exec(cloud, v).await)),
                Err(_) => Vec::new(),
            }
        }
        // Direct-MX carrier-gateway SMS relay: the leader (cloud node, :25 blocked) forwards a
        // direct-MX send here to an NA-egress peer (:25 open) whose IP a carrier gateway accepts;
        // this node runs the blast locally.
        p if method == hive_p2p::GOSSIP_POST && p.starts_with("/v1/push/sms-direct-mx") => {
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(v) => jb(axum::Json(crate::push::sms_direct_mx_exec(v).await)),
                Err(_) => Vec::new(),
            }
        }
        // libsql/Hrana OWNER PROXY: a managed SQLite database is a FILE on one node, and every
        // other node forwards here rather than opening a second, empty copy of it. The envelope
        // carries the CLIENT's own database bearer, which `hrana::mesh_serve` re-checks against
        // THIS node's replicated record before touching the file (the `proxy_to_owner` re-check
        // precedent), and it refuses to re-proxy. Disjoint from the `/v1/databases/replica` arm
        // below (this one requires the `/hrana-mesh` suffix).
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
        // Supabase Studio OWNER PROXY: the container lives on one node and per-DB DNS publishes the
        // full healthy node set, so any other edge forwards here rather than answering
        // MISDIRECTED_REQUEST. Same envelope and re-check shape as the `/hrana-mesh` arm above.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/databases/")
            && p.split('?')
                .next()
                .unwrap_or_default()
                .ends_with("/studio-mesh") =>
        {
            let id = p
                .split('?')
                .next()
                .unwrap_or_default()
                .trim_start_matches("/v1/databases/")
                .trim_end_matches("/studio-mesh");
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(env) => jb(axum::Json(
                    crate::db_rest::studio_mesh_serve(cloud, id, &env).await,
                )),
                Err(e) => jb(axum::Json(serde_json::json!({ "error": e.to_string() }))),
            }
        }
        // browser_db libsql/Hrana + Upstash REST OWNER PROXY: the `/hrana-mesh` shape addressed by
        // project instead of database id (there is no `Database` record), where
        // `browser_db_rest::mesh_serve` re-derives the elected owner and the credential scope from
        // THIS node's own replicated state before touching the file.
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
        // Mirrored storage writes (the replication data plane): the team rides as `?team=` and is
        // applied to the SAME tenant namespace on this replica; they NEVER re-mirror, being already
        // replicated writes.
        p if method == hive_p2p::GOSSIP_POST
            && p.starts_with("/v1/storage/")
            && (qparam(p, "mirror").as_deref() == Some("1")) =>
        {
            crate::admin::apply_mirrored_write(&cloud, p, body).await;
            jb(axum::Json(serde_json::json!({ "ok": true })))
        }
        // Billing authority reads, the other end of `admin::fetch_from_host` /
        // `admin::proxy_billing_read` whenever the CALLING node is not the billing authority
        // (`billing_authority_node`): without these arms every `/v1/billing*` gossip call fell
        // through to the catch-all, `fetch_from_host` got an empty body it could not parse, and the
        // caller silently served ITS OWN non-authoritative billing state on a perfectly healthy
        // mesh connection. Longest-prefix-first: `/v1/billing/checkout/`, `/invoices` and `/ledger`
        // must precede the bare `/v1/billing` arm, which would otherwise shadow them.
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
        // Fleet-global raw-port allocation: the claim decision runs ONLY on the control-plane
        // leader, and a non-leader receiving this (stale routing, or a peer with an out-of-date
        // view of who the leader is) refuses rather than racing its own local registry against the
        // real leader's (see `raw_ports.rs` and `allocate_raw_ports_coordinated`).
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
        // The mirror of the allocate arm, for permanent retirement: the `/release` path and the
        // additive `released` response field are kept for rolling compatibility, but the allocator
        // never re-grants a retired number (see `release_raw_ports_coordinated`).
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
                    // Retire only when this leader holds NO live settings row for the project
                    // (tombstoned or absent): retirement is PERMANENT (numbers are never
                    // re-granted), so releasing a LIVE project's ports on any trusted peer's say-so
                    // would force a port swap nobody asked for. The legitimate flow always arrives
                    // after the project's delete, so a live row is exactly the attack/bug case.
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

/// The tenant of a dispatched mesh call: the VERIFIED delegation token's tenant ([`team_claims`]),
/// else a raw `?team=` only when auth is not enforced; empty when neither resolves. Never a raw
/// `split_once("?team=")` parse: under enforcement the query is `team=<t>&tok=<jwt>`, so that
/// swallows the token into the team string and every ownership compare misses silently.
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

/// Headers for an in-process call into a handler gated by `require_operator_or_internal` or
/// `require_internal_hop`: the fleet's `HIVE_INTERNAL_TOKEN` as `x-hive-internal` (empty when the
/// token is unset), so the handler passes without relying on synthesized claims.
fn internal_hop_headers() -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    if let Ok(t) = std::env::var("HIVE_INTERNAL_TOKEN") {
        if let Ok(hv) = axum::http::HeaderValue::from_str(&t) {
            headers.insert("x-hive-internal", hv);
        }
    }
    headers
}

/// Reconstructs the logs `Query<LogsQ>` from a dispatched path's query string. EVERY field
/// `admin::LogsQ` declares must be mirrored here: a parameter the mesh arm does not read is a
/// parameter the peer never applies, so the filter works on the owner node and silently returns
/// unfiltered rows on every other node, the exact bug class this endpoint is being fixed for.
/// `range=` is not forwarded by the coordinator (it sends resolved absolute bounds) but is parsed
/// so a direct mesh dispatch that still carries one is honoured; `offset` is coordinator-only
/// (paging happens once, after the merge), so it is deliberately not read.
fn logs_query(path: &str) -> axum::extract::Query<crate::admin::LogsQ> {
    axum::extract::Query(crate::admin::LogsQ {
        limit: qparam(path, "limit").and_then(|v| v.parse().ok()),
        project: qparam(path, "project"),
        deployment: qparam(path, "deployment"),
        q: qparam(path, "q"),
        local: qparam(path, "local").map(|v| v == "true" || v == "1"),
        range: qparam(path, "range"),
        since_ms: qparam(path, "since_ms").and_then(|v| v.parse().ok()),
        until_ms: qparam(path, "until_ms").and_then(|v| v.parse().ok()),
        status: qparam(path, "status"),
        method: qparam(path, "method"),
        host: qparam(path, "host"),
        route: qparam(path, "route"),
        level: qparam(path, "level"),
        cache: qparam(path, "cache"),
        env: qparam(path, "env"),
        resource: qparam(path, "resource"),
        offset: None,
        cursor: None,
    })
}

/// Pull a query-string value (`?k=v&...`) out of a dispatched path, percent-
/// decoded. Decoding matters: the coordinator URL-encodes every filter value
/// (`urlencode`), so a `route=/api/x` or `status=404,5xx` would otherwise
/// arrive here as `route=%2Fapi%2Fx` and silently match nothing.
fn qparam(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// Minimal percent-decoder (`+` is left literal: query strings here are built
/// by `urlencode`, which escapes spaces as `%20`). Invalid sequences decode to
/// `U+FFFD` rather than failing, so a malformed value can never make a whole
/// dispatch unparseable.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    if !b.contains(&b'%') {
        return s.to_string();
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
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

/// Builds a verified-claims extension for a mesh-internal admin call. The iroh transport carries no
/// HTTP headers and the calling peer is already trusted (peer-trust allowlist + mesh auth), so the
/// tenant that rides in the query is injected as authoritative [`crate::auth::Claims`]: handlers
/// resolve the tenant from claims, and under enforced JWT auth a synthesized `x-hive-team` header
/// is ignored, so without this the tenant would be lost on the host node.
///
/// The PREFERRED form is a signed, short-lived delegation token (`?tok=`) minted by the originating
/// node from the verified user tenant. Verifying it yields an AUTHORITATIVE, integrity-protected,
/// expiring tenant assertion (`platform_admin` is always stripped) and closes the raw-`?team=`
/// spoofing class: the value can no longer be an arbitrary attacker-set query parameter, it must be
/// a validly signed unexpired JWT, and an INVALID token under enforcement is rejected outright (no
/// claims). Residual, inherent to a shared `HIVE_JWT_SECRET` mesh: a compromised TRUSTED PEER node
/// can still mint any tenant; closing that needs per-node signing keys and is bounded by the
/// peer-trust allowlist gate. The FALLBACK is a raw `?team=`, retained for dev/unenforced mode and
/// rolling upgrades (an origin node predating token-minting sends only `team=`); under enforcement
/// it is accepted with a warning.
fn team_claims(path: &str) -> Option<axum::Extension<crate::auth::Claims>> {
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
        worlds: qparam(path, "worlds").map(|v| v == "true" || v == "1"),
    })
}

/// Pure core of the mesh-mutation authorization decision (no CloudState/env deps, unit-testable
/// directly). `signer_trusted` is `None` when no trust set is configured at all (nothing to check
/// against: permissionless, today's default) and `Some(bool)` when a trust set IS configured,
/// carrying whether the (possibly absent, mapped to `false`) verified signer is a member. Reads are
/// always allowed here (each handler still applies its own tenant/team scoping), mirroring the HTTP
/// side's "reads are always allowed" policy.
fn mesh_mutation_authorized(method: u8, signer_trusted: Option<bool>) -> bool {
    method == hive_p2p::GOSSIP_GET || signer_trusted.unwrap_or(true)
}

/// The iroh gossip handler `serve_tunnels` invokes for inbound `STREAM_GOSSIP`; `signer` is the
/// message's VERIFIED ed25519 identity (Some only when the request carried a valid signature bound
/// to the QUIC peer). The iroh gossip transport carries no HTTP headers, so it never passes through
/// `auth::require_auth`: an operator who secured the API by setting `HIVE_JWT_SECRET` had secured
/// only the HTTP admin surface, while this mesh path answered the same privileged handlers (deploy,
/// project delete, database_replica, storage mirror writes, promote) with NO authorization check.
/// The only authorization signal this transport can carry is a verified, trust-set-member signer,
/// so when a trust set is actually configured (non-empty, matching the HTTP side's "only enforce
/// what is actually configured" pattern, e.g. `require_auth`/`require_operator`) it is required for
/// every MUTATING call; reads stay open. With no trust set configured (today's default) this is
/// permissionless.
pub fn handler(cloud: Arc<CloudState>) -> hive_p2p::GossipHandler {
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
            dispatch_verified(&cloud, method, &path, &body, signer.as_deref()).await
        })
    })
}

/// Cap on the iroh attempt when an HTTP fallback can actually run. Dialing a peer's STALE identity
/// (e.g. after it restarted with a new key) can hang on connect/relay-retry, and without a cap a
/// dead cached addr would stall the whole gossip loop and the HTTP fallback would never run, so the
/// node could never re-learn the peer's new address; on timeout or error the stale mapping is
/// dropped and the attempt falls through to HTTP, which re-learns the fresh addr. A non-URL peer
/// (notably the `seed:<64hex>` form used before a seed has gossiped a full NodeInfo) has no HTTP
/// path at all, so capping its dial below `dial_fallback_ceiling` would cancel `acquire`'s
/// fresh-discovery fallback mid-flight (the same cancellation that once made mesh join impossible
/// for a new node) and leave NO recovery path: those peers get the full ceiling instead.
const IROH_ATTEMPT_BUDGET_WITH_HTTP_FALLBACK: Duration = Duration::from_secs(3);

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
    fetch_outcome(cloud, peer, method, path, body).await.bytes()
}

/// What one gossip fetch proved about the TARGET, not just the payload.
pub enum FetchOutcome {
    Answered(Vec<u8>),
    /// The mesh dial itself failed — its budget elapsed, the connect phase
    /// timed out, or the peer's endpoint refused the connection — and no HTTP
    /// fallback answered. A second dial to this target in the same gossip
    /// round (the mesh join) would spend the same budget failing the same way.
    Unreachable,
    /// Anything else: connected but refused (e.g. not yet in the peer's
    /// trust set, which is what the mesh join exists for), HTTP failure, or
    /// no transport mapping at all.
    Failed,
}

impl FetchOutcome {
    pub fn bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Answered(b) => Some(b),
            Self::Unreachable | Self::Failed => None,
        }
    }
}

/// A pool error that means the connection was never established.
fn is_dial_failure(e: &anyhow::Error) -> bool {
    e.downcast_ref::<hive_p2p::DeadPeerTimeout>()
        .is_some_and(|d| d.phase == "connect")
        || e.downcast_ref::<hive_p2p::PeerRefused>().is_some()
}

/// [`fetch`], classified (see [`FetchOutcome`]). The HTTP-over-SSH fallback presents a short-lived
/// mesh-internal Bearer (the SAME mechanism `db_replicate.rs`'s `send_mirrored` uses for this class
/// of node-to-node call) on EVERY gossip method. The receiving node's admin router wraps every
/// mutation in `auth::require_auth` once `HIVE_JWT_SECRET` is set (the documented production
/// posture), and guards the fleet-wide READ handlers this loop pulls (`/v1/serve-hosts`,
/// `/v1/fleet-deployments`, `/v1/leases`, `/v1/guardian/heads`) with `admin::require_fleet_read`,
/// which accepts exactly this signed `service` delegation: without it, a round that fell back to
/// HTTP (bootstrap, or a stale or dead cached iroh addr) was answered 401/403 and the node silently
/// stopped learning its peers' routes, deployments and leases. The receiving handlers on this path
/// do not read claims, so any validly signed token clears the gate without granting a tenant-scoped
/// privilege; with `HIVE_JWT_SECRET` unset `issue()` errors and no header is added (dev/single-node
/// behaviour).
pub async fn fetch_outcome(
    cloud: &Arc<CloudState>,
    peer: &str,
    method: u8,
    path: &str,
    body: &[u8],
) -> FetchOutcome {
    let mut dial_failed = false;
    if iroh_enabled() {
        let target = cloud.peer_iroh.read().get(peer).cloned();
        let pool = cloud.mesh.read().clone();
        if let (Some((node_id, addr)), Some(pool)) = (target, pool) {
            let addr = relay_hinted_addr(cloud, &node_id, &addr);
            let http_fallback_viable = peer.starts_with("http://") || peer.starts_with("https://");
            let budget = if http_fallback_viable {
                IROH_ATTEMPT_BUDGET_WITH_HTTP_FALLBACK
            } else {
                hive_p2p::dial_fallback_ceiling()
            };
            let attempt = tokio::time::timeout(
                budget,
                pool.gossip_request(&node_id, &addr, method, path, body),
            )
            .await;
            match attempt {
                Ok(Ok(bytes)) => return FetchOutcome::Answered(bytes),
                Ok(Err(e)) => {
                    tracing::debug!(peer, path, error = %e, "iroh gossip failed; falling back to HTTP");
                    dial_failed = is_dial_failure(&e);
                    cloud.peer_iroh.write().remove(peer);
                }
                Err(_) => {
                    tracing::debug!(peer, path, "iroh gossip timed out; falling back to HTTP");
                    dial_failed = true;
                    cloud.peer_iroh.write().remove(peer);
                }
            }
        }
    }
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
    if let Ok(tok) = crate::auth::issue(
        "mesh-internal",
        "mesh",
        "service",
        false,
        crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
    ) {
        req = req.header("authorization", format!("Bearer {tok}"));
    }
    let answered = match req.timeout(Duration::from_secs(4)).send().await {
        Ok(r) if r.status().is_success() => r.bytes().await.ok().map(|b| b.to_vec()),
        _ => None,
    };
    match answered {
        Some(bytes) => FetchOutcome::Answered(bytes),
        None if dial_failed => FetchOutcome::Unreachable,
        None => FetchOutcome::Failed,
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
