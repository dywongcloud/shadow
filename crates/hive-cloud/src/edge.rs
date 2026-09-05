//! The edge request pipeline, mirroring Vercel's CDN layering:
//! **routing (redirects/rewrites) -> firewall (WAF + bots) -> concurrency
//! admission -> CDN cache (HIT/STALE/MISS w/ SWR) -> compute (route)**.
//! Adds `x-hive-region` and `x-hive-cache`, and records events for the dashboard.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};
use hive_edge::{
    cdn::{CacheState, CdnCache, Lookup},
    routing::RouteOutcome,
    waf::{RequestCtx, Verdict},
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::state::CloudState;

fn set(resp: &mut Response, name: &'static str, val: &str) {
    if let Ok(v) = HeaderValue::from_str(val) {
        resp.headers_mut().insert(HeaderName::from_static(name), v);
    }
}

/// Stamp the microfrontend debug headers on a served response (non-production
/// only — `dbg` is `None` for production requests, so nothing is added there).
fn set_mfe(resp: &mut Response, dbg: &Option<[String; 4]>) {
    if let Some([group, project, route, fallback]) = dbg {
        set(resp, "x-mfe-group", group);
        set(resp, "x-mfe-project", project);
        set(resp, "x-mfe-route", route);
        set(resp, "x-mfe-fallback", fallback);
    }
}

/// Build a structured failure response (#18): correct status + a stable public code
/// body + `x-hive-error` header. Internal detail stays in the recorded event/log.
fn fail_response(class: fluid_core::FailureClass, region: &str, rid: &str) -> Response {
    let status = StatusCode::from_u16(class.status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut resp = (status, class.code()).into_response();
    set(&mut resp, "x-hive-error", class.code());
    set(&mut resp, "x-hive-region", region);
    set(&mut resp, "x-hive-request-id", rid);
    resp
}

pub async fn edge_pipeline(
    State(cloud): State<Arc<CloudState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    // Stamp the SERVING node on every response (routing diagnosis — e.g. which iad
    // pool member returned a 503). A locally-served response gets THIS node's name;
    // a mesh-proxied one already carries the peer's stamp (copied through from the
    // peer's own pipeline), which we leave intact. Pairs with `x-hive-routed-to`.
    let node = cloud.node_name.clone();
    let mut resp = edge_pipeline_inner(State(cloud), peer, req, next).await;
    if !resp.headers().contains_key("x-hive-served-by") {
        set(&mut resp, "x-hive-served-by", &node);
    }
    resp
}

async fn edge_pipeline_inner(
    State(cloud): State<Arc<CloudState>>,
    peer: std::net::SocketAddr,
    mut req: Request,
    next: Next,
) -> Response {
    let method = req.method().as_str().to_string();
    let mut path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let headers_vec: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    // Host resolution: HTTP/1.1 carries it in the `Host` header, but HTTP/2 (used
    // by our TLS listener after ALPN negotiates h2) carries it in the `:authority`
    // pseudo-header instead — exposed by hyper as the request URI's authority, with
    // no synthesized `Host` header. Fall back to that so h2 clients route correctly.
    let mut host = hget(&req, "host")
        .filter(|s| !s.is_empty())
        .or_else(|| req.uri().authority().map(|a| a.host().to_string()))
        .unwrap_or_default();
    // Normalize: ensure a `Host` header is present for all downstream consumers
    // (the gateway router reads `header::HOST` directly). Under HTTP/2 it would
    // otherwise be absent. No-op for HTTP/1.1 where the header already exists.
    if !host.is_empty() && req.headers().get(axum::http::header::HOST).is_none() {
        if let Ok(v) = HeaderValue::from_str(&host) {
            req.headers_mut().insert(axum::http::header::HOST, v);
        }
    }
    let ua = hget(&req, "user-agent").unwrap_or_default();
    // Real TCP peer address, NOT the client-supplied `x-forwarded-for` header.
    // There is no reverse proxy (nginx/haproxy/ngrok) in front of this listener
    // on any fleet node — hive-cloud terminates TLS and binds 0.0.0.0:80/443
    // directly — so XFF has no legitimate source and is pure attacker-controlled
    // input; trusting it let any client spoof their IP past rate limiting and
    // the enterprise IP-block/allowlist firewall below.
    let ip = peer.ip().to_string();
    let region = cloud.region.clone();

    // ACME HTTP-01 challenges for custom tenant domains, answered from the
    // replicated `acme_http01` store — a validation fetch lands on ANY node
    // (round-robin/geo DNS is exactly what makes HTTP-01 work for a tenant
    // whose DNS already points at the fleet). Before EVERY other gate
    // (host_allowed would 404 an unattached host, which is precisely the
    // state a domain is in while being validated): the answer is a tiny
    // static string for a token only the order initiator knows, never
    // tenant-controlled content. Unknown token = plain 404, no existence leak.
    if let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
        if let Some(body) = cloud.acme_http01.lookup(token) {
            return (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                body,
            )
                .into_response();
        }
        // Miss on this node: the issuer is the leader and Let's Encrypt's
        // multi-perspective fetches land on RANDOM nodes within ~1s of
        // set_ready, long before the 60s store_sync pull can replicate the
        // token. Proxy the lookup to the leader rather than fail-validating
        // every issuance ~13/14 of the time (adversarial finding). Unknown
        // token everywhere = flat 404, no existence leak.
        let leader = cloud.control_plane_leader();
        if leader != cloud.node_name {
            if let Some(ip) = cloud
                .registry
                .nodes()
                .into_iter()
                .find(|n| n.name == leader && n.healthy)
                .and_then(|n| n.public_ip.clone().filter(|ip| !ip.is_empty()))
            {
                let url = format!(
                    "http://{ip}{}",
                    req.uri()
                        .path_and_query()
                        .map(|p| p.as_str().to_string())
                        .unwrap_or_else(|| "/".into())
                );
                if let Ok(r) = cloud
                    .http
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await
                {
                    if r.status().is_success() {
                        if let Ok(body) = r.text().await {
                            return (
                                StatusCode::OK,
                                [(
                                    axum::http::header::CONTENT_TYPE,
                                    "text/plain; charset=utf-8",
                                )],
                                body,
                            )
                                .into_response();
                        }
                    }
                }
            }
        }
        return (StatusCode::NOT_FOUND, "unknown challenge").into_response();
    }

    // DB-gateway HTTP REST surface: `<slug>.{db_domain}` hosts are the per-tenant
    // database endpoints (Upstash-style redis REST / SQL-over-HTTP), NOT app
    // deployments — branch before the deploy-suffix host validation below would
    // 404 them. Auth is per-DB bearer inside the handler (ZeroTrust: the edge
    // pipeline's WAF/preview gates are deployment concepts and don't apply), but
    // the generic per-IP flood shedder still applies — this is the only path on
    // the shared 443 listener that would otherwise have ZERO L7 rate limiting.
    if !cloud.db_domain.is_empty() {
        let hp = host.split(':').next().unwrap_or(&host).to_ascii_lowercase();
        if hp == cloud.db_domain || hp.ends_with(&format!(".{}", cloud.db_domain)) {
            let status_host = hp.clone();
            // DEDICATED db-tier limiter, NOT the shared browsing-tier
            // `cloud.ratelimit` (100/10s): a single WDK app's world-redis
            // dispatcher polls its database every 50ms (200 req/10s), so the
            // shared budget was permanently exhausted for that IP — 429ing
            // the app's own polls AND every workflow-world read the platform
            // makes from the same host (witnessed: hairpinned leader reads of
            // db-*.downstash.xyz got RATE_LIMITED while loopback PONGed,
            // silently emptying the workflows console). Keyed per (IP, DB
            // host), NOT per IP alone: every microVM cell on a node NATs out
            // through the node's own public IP, so one chatty app's queue
            // drain was eating the budget of every OTHER database reached
            // from that node (witnessed: a 22-job workflow-journal replay
            // burst RATE_LIMITED its own world mid-run). 5000/10s per
            // (IP, db) rides out a full journal-replay drain while still
            // shedding real floods; the body is JSON so Upstash-compatible
            // clients surface a clean error instead of a JSON-parse crash.
            fn db_rest_rate_limiter() -> &'static hive_edge::RateLimiter {
                static LIMITER: std::sync::OnceLock<hive_edge::RateLimiter> =
                    std::sync::OnceLock::new();
                LIMITER.get_or_init(|| hive_edge::RateLimiter::new(5000, 10_000))
            }
            let rl_key = format!("{ip}|{hp}");
            if !db_rest_rate_limiter().check(&rl_key, hive_core::now_ms()) {
                let ev = cloud.event(
                    &region,
                    &method,
                    &status_host,
                    &path,
                    429,
                    "rate-limited",
                    &ip,
                );
                cloud.record(ev);
                let mut resp = (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(serde_json::json!({ "error": "rate limited, retry shortly" })),
                )
                    .into_response();
                set(&mut resp, "x-hive-ratelimit", "exceeded");
                return resp;
            }
            let resp = crate::db_rest::handle(cloud.clone(), hp, req).await;
            let ev = cloud.event(
                &region,
                &method,
                &status_host,
                &path,
                resp.status().as_u16(),
                "db-rest",
                &status_host,
            );
            cloud.record(ev);
            return resp;
        }
    }

    // Vercel Web Analytics / Speed Insights beacons are infrastructure endpoints
    // served by the gateway itself — let them pass straight through, bypassing the
    // WAF / rate-limit / preview gates, so any deployed app using @vercel/analytics
    // or @vercel/speed-insights works regardless of the deployment's protection.
    if path.starts_with("/_vercel/") {
        return next.run(req).await;
    }

    // Microfrontends: compose child projects under the default (host) project's
    // domain. If this host is a group's default project and the path matches a
    // child's route, resolve the child's TARGET deployment (honoring the group's
    // fallback-environment policy for preview requests) and rewrite the host to
    // that deployment so the existing alias-resolution + dispatch serves it — one
    // domain, many projects, no second runtime path. (Fast-guarded: only runs when
    // some microfrontend group exists locally or via gossip.)
    let mut mfe_dbg: Option<[String; 4]> = None;
    if !host.is_empty() && cloud.enterprise.has_mfe() {
        if let Some(dep) = cloud.gw.deployment_for_host(&host) {
            let team = cloud.projects.team_of(&dep.project);
            if let Some(group) = cloud.enterprise.mfe_group_for_host(&team, &dep.project) {
                if let Some((child, matched)) = crate::microfrontends::resolve_child(&group, &path)
                {
                    // The request environment/commit come from the DEFAULT app's
                    // resolved deployment (production alias vs a preview commit URL).
                    let req_env = if dep.production || dep.target == "production" {
                        "production"
                    } else {
                        "preview"
                    };
                    let req_commit = dep
                        .git
                        .as_ref()
                        .map(|g| g.commit.clone())
                        .unwrap_or_default();
                    let cands: Vec<crate::microfrontends::DeployCand> = cloud
                        .gw
                        .list()
                        .into_iter()
                        .filter(|d| d.project == child)
                        .map(|d| crate::microfrontends::DeployCand {
                            id: d.id.to_string(),
                            env: if d.production || d.target == "production" {
                                "production".into()
                            } else {
                                "preview".into()
                            },
                            commit: d.git.as_ref().map(|g| g.commit.clone()).unwrap_or_default(),
                            production: d.production,
                        })
                        .collect();
                    if let Ok(target) = crate::microfrontends::resolve_child_target(
                        &group,
                        &child,
                        req_env,
                        &req_commit,
                        &cands,
                    ) {
                        // The target deployment id is a served-host alias label
                        // (published locally + gossiped), so rewriting the host's
                        // first label routes to that exact deployment via the same
                        // path the public router uses — no second HTTP hop.
                        let rest = host.split_once('.').map(|(_, r)| r.to_string());
                        host = match rest {
                            Some(r) => format!("{}.{}", target.deployment_id, r),
                            None => target.deployment_id.clone(),
                        };
                        if let Ok(v) = HeaderValue::from_str(&host) {
                            req.headers_mut().insert(axum::http::header::HOST, v);
                        }
                        // Debug/trace headers, non-production only (so prod responses
                        // never leak internal composition). Set on the REQUEST (seen
                        // by the child + across mesh hops) and stamped on the local
                        // response below.
                        if req_env != "production" {
                            let dbg = [
                                group.id.clone(),
                                child.clone(),
                                matched.clone(),
                                target.fallback.clone(),
                            ];
                            for (k, val) in [
                                ("x-mfe-group", &dbg[0]),
                                ("x-mfe-project", &dbg[1]),
                                ("x-mfe-route", &dbg[2]),
                                ("x-mfe-fallback", &dbg[3]),
                            ] {
                                if let Ok(v) = HeaderValue::from_str(val) {
                                    req.headers_mut().insert(HeaderName::from_static(k), v);
                                }
                            }
                            mfe_dbg = Some(dbg);
                        }
                        let ev =
                            cloud.event(&region, &method, &host, &path, 0, "mfe-route", &child);
                        cloud.record(ev);
                    }
                }
            }
        }
    }

    // EXPERIMENT (feature `zkauth`): anonymous preview-access bootstrap. Verifies
    // a membership proof and drops a short-lived access cookie, then redirects.
    // Only the node that OWNS the deployment can do this: it has the team roster
    // (to verify the ring-signature proof) and the secret key (to seal the access
    // cookie it later checks). When a pooled request for `/_shadw/zk` lands on a
    // node that doesn't serve this host, fall through to mesh routing so it's
    // proxied to the owner — which then handles it locally here.
    #[cfg(feature = "zkauth")]
    if path.starts_with("/_shadw/zk") && cloud.gw.serves_host(&host) {
        return crate::zkauth::bootstrap(&cloud, &host, &query);
    }

    // Request-ID (#4): accept an inbound `x-hive-request-id` or mint one; it's
    // forwarded over the mesh and stamped on every routing event below.
    let rid = hget(&req, "x-hive-request-id")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());

    // Allowed-suffix validation (#3): a multi-label Host must end with a configured
    // deploy suffix. A foreign root (e.g. `app.evil.com`) — even if its first label
    // collides with a real alias — is rejected here with a clean 404 that leaks
    // nothing. (`/_vercel/*` + `/_shadw/zk` already returned above; bare/empty hosts
    // and the default deployment are allowed.)
    if !host.is_empty() && !cloud.host_allowed(&host) {
        let mut ev = cloud.event(&region, &method, &host, &path, 404, "host-rejected", &host);
        ev.request_id = rid.clone();
        cloud.record(ev);
        return deployment_not_found(&region);
    }

    // WebSocket upgrade detection (#2): upgraded connections need a raw byte splice
    // across the mesh (HTTP framing bypassed), not the buffered/streamed tunnel.
    let is_ws_upgrade = hget(&req, "upgrade")
        .map(|u| u.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
        && hget(&req, "connection")
            .map(|c| {
                c.to_ascii_lowercase()
                    .split(',')
                    .any(|t| t.trim() == "upgrade")
            })
            .unwrap_or(false);

    // ---- Account-level IP blocking (enterprise) ----------------------------------
    // Enforced at the ENTRY node, BEFORE any mesh proxying/compute: if the target
    // deployment's owning team has denylisted this client IP, reject here. The
    // owning team resolves from the local gateway for locally-hosted deployments,
    // else from the gossiped fleet deployment list (`peer_deployments` carries
    // each deployment's tenant), else from the local project store. The block
    // list itself is mesh-replicated (see enterprise::EdgeExport), so this works
    // no matter which node authored the config or receives the request.
    if !host.is_empty() && cloud.enterprise.any_team_blocks(&ip) {
        let subl = host
            .split(':')
            .next()
            .unwrap_or(&host)
            .split('.')
            .next()
            .unwrap_or(&host)
            .to_string();
        let team: Option<String> = cloud
            .gw
            .deployment_for_host(&host)
            .map(|d| {
                if d.tenant.is_empty() {
                    cloud.projects.team_of(&d.project)
                } else {
                    d.tenant
                }
            })
            .or_else(|| {
                cloud
                    .peer_deployments
                    .read()
                    .values()
                    .flatten()
                    .find_map(|d| {
                        let m = [&d.alias, &d.commit_alias, &d.branch_alias, &d.id_alias]
                            .iter()
                            .any(|a| a.split('.').next().unwrap_or(a) == subl);
                        if !m {
                            return None;
                        }
                        Some(if d.tenant.is_empty() {
                            cloud.projects.team_of(&d.project)
                        } else {
                            d.tenant.clone()
                        })
                    })
            });
        if let Some(team) = team {
            if !cloud.enterprise.ip_allowed(&team, &ip) {
                let ev = cloud.event(&region, &method, &host, &path, 403, "ip-blocked", &ip);
                cloud.record(ev);
                let mut resp = (StatusCode::FORBIDDEN, "IP_BLOCKED").into_response();
                set(&mut resp, "x-hive-region", &region);
                set(&mut resp, "x-hive-blocked", "ip");
                return resp;
            }
        }
    }

    // ---- Cross-node mesh routing ------------------------------------------------
    // If this node doesn't host the requested deployment but a peer in the mesh
    // does, reverse-proxy to the best peer: ordered by the deployment's configured
    // regions/failover (#5), else same-region-first anycast, failing over to the
    // next candidate on error. `x-hive-proxied` breaks loops. Hit any node, the
    // request reaches wherever the deployment lives.
    let already_proxied = hget(&req, "x-hive-proxied").is_some();
    let serve_local = cloud.gw.serves_host(&host);
    let sub = host
        .split(':')
        .next()
        .unwrap_or(&host)
        .split('.')
        .next()
        .unwrap_or(&host)
        .to_string();
    // CONTAINER single-owner routing: a container deployment has a placement LEASE,
    // and ONLY its lease owner may serve it (prevents split-brain double-running of a
    // stateful container). The lease propagates mesh-wide via gossip, so ANY node can
    // determine the owner from `owner_of(sub)` — even a node that neither hosts the
    // container nor gossips serve-hosts directly with the owner. We route to that
    // owner using the gossip REGISTRY's address (below), NOT peer_routes (which needs
    // full-mesh serve-hosts gossip to be complete). `owner_of` returns `Some` only for
    // a live container lease, so functions are unaffected. `None` when WE are the
    // owner (serve locally) or there's no live lease.
    let container_owner: Option<String> = cloud
        .leases
        .owner_of(&sub)
        .filter(|owner| owner != &cloud.node_name);
    // A local record that CANNOT SERVE must not win the host over a peer that
    // can. `serves_host` only asks "is there an alias", and an alias outlives
    // the deployment's usefulness: an orphaned `Building…` placeholder (the
    // build task died before removing it, then `persist::restore` reconciled it
    // to `Error`) keeps the project label forever on the node that started the
    // build, while the real deployment lands on the node placement chose. That
    // node then served the dead record locally and NEVER consulted
    // `peer_routes` — witnessed on `archive-zip.shadw.app` (2026-08-05):
    // fc-sanjose answered the project host from its `Error` placeholder while
    // fc-sanjose-gpu-1 held the Ready production deployment one hop away, and
    // the contrast was visible in the same fleet — the branch alias, which
    // fc-sanjose did NOT hold, proxied correctly and returned 200.
    //
    // Deliberately gated on a peer actually having a healthy route: with no
    // peer to hand off to, serving the local `Building…`/build-failed page is
    // still the most honest answer this node has.
    //
    // `None` here — an alias whose deployment record is gone — counts as not
    // ready for the same reason.
    let local_state = serve_local
        .then(|| cloud.gw.host_deploy_state(&host))
        .flatten();
    // A local record that is Ready is not automatically the RIGHT answer: this
    // node's `select()` only reflects deployments THIS node has itself
    // registered (a build/publish that ran here), never a production promotion
    // that happened on a different node. A stale preview left over from an
    // earlier deploy stays Ready forever and wins the alias by default — this
    // node has no signal that a newer production deployment exists elsewhere.
    // Witnessed live (2026-08-26, `express.shadw.app`): fc-sanjose kept
    // serving its own leftover preview build (Ready, `production: false`)
    // while fc-frankfurt held the actual promoted production deployment for
    // the same alias — sanjose never even checked `peer_deployments` because
    // its local answer was Ready. Detect that gap from the gossiped fleet
    // deployment list (`peer_deployments`, populated by the SAME
    // `/v1/fleet-deployments` poll `raw_proxy.rs` already relies on): a peer
    // deployment for this exact alias is "newer" only if it out-ranks the
    // local candidate by the same `(production, created_at_ms)` ordering
    // `set_alias_if_newer` uses locally, so a preview can never pre-empt a
    // production deploy this node DOES hold correctly.
    let local_is_stale = serve_local
        && matches!(local_state, Some(fluid_core::DeployState::Ready))
        && {
            let local_rank = cloud
                .gw
                .deployment_for_host(&host)
                .map(|d| (d.production, d.created_at_ms));
            local_rank.is_some_and(|local_rank| {
                cloud
                    .peer_deployments
                    .read()
                    .values()
                    .flatten()
                    .filter(|d| d.state == fluid_core::DeployState::Ready)
                    .filter(|d| {
                        [&d.alias, &d.commit_alias, &d.branch_alias, &d.id_alias]
                            .iter()
                            .any(|a| a.eq_ignore_ascii_case(&host) || a.split('.').next() == Some(sub.as_str()))
                    })
                    .any(|d| (d.production, d.created_at_ms) > local_rank)
            })
        };
    let prefer_peer = serve_local
        && (!matches!(local_state, Some(fluid_core::DeployState::Ready)) || local_is_stale)
        && {
            // Full-host key first, same as the route lookup below (custom
            // tenant domains are keyed by full hostname in served_hosts).
            let routes = cloud.peer_routes.read();
            let host_key = host.split(':').next().unwrap_or(&host).to_ascii_lowercase();
            routes
                .get(&host_key)
                .or_else(|| routes.get(&sub))
                .is_some_and(|rs| rs.iter().any(|r| r.healthy))
        };
    if prefer_peer {
        tracing::warn!(
            host = %host,
            state = ?local_state,
            "local deployment for this host is not Ready; routing to a peer that serves it"
        );
    }
    if !already_proxied
        && !host.is_empty()
        && (!serve_local || container_owner.is_some() || prefer_peer)
    {
        // Only nodes healthy in BOTH the route table AND the gossip registry are
        // eligible (#6) — a dropped/unhealthy node is excluded as a candidate.
        let healthy_ids: std::collections::HashSet<String> = cloud
            .registry
            .nodes()
            .into_iter()
            .filter(|n| n.healthy)
            .map(|n| n.id)
            .collect();
        let raw: Vec<crate::state::PeerRoute> = {
            // A full-host alias (custom tenant domain) is keyed in
            // served_hosts — and therefore in this table — by its FULL
            // hostname, so the label-only lookup missed it entirely and every
            // custom domain 404'd on every non-owner edge (live witness:
            // domwit on the owner returned its alias answer while the leader
            // 404'd). Full-host key first, label second, deduped — the
            // `aliases_full`-before-label rule applied to the mesh route
            // table, so a custom host can never fall through to a same-named
            // project's label routes.
            let routes = cloud.peer_routes.read();
            let host_key = host.split(':').next().unwrap_or(&host).to_ascii_lowercase();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            routes
                .get(&host_key)
                .into_iter()
                .chain(routes.get(&sub))
                .flatten()
                .filter(|r| seen.insert(r.node_id.clone()))
                .filter(|r| r.healthy && healthy_ids.contains(&r.node_id))
                .cloned()
                .collect()
        };
        let had_routes = !raw.is_empty();

        // Candidate ordering: container → ONLY the elected lease owner (single-owner),
        // addressed from the gossip REGISTRY so it's reachable from any node even when
        // peer_routes lacks it (non-full-mesh serve-hosts gossip). Otherwise apply the
        // deployment's configured region/failover policy (#5) over peer_routes.
        let mut cands: Vec<crate::state::PeerRoute> = if let Some(owner) = &container_owner {
            cloud
                .registry
                .nodes()
                .into_iter()
                .find(|n| &n.id == owner && n.healthy)
                .map(|n| crate::state::PeerRoute {
                    node_id: n.id,
                    region: n.region,
                    gateway: n.public_url,
                    latency_ms: n.latency_ms,
                    healthy: true,
                    last_seen_ms: hive_core::now_ms(),
                })
                .into_iter()
                .collect()
        } else {
            let project = cloud
                .gw
                .project_for_host(&host)
                .unwrap_or_else(|| sub.clone());
            let fs = cloud.projects.get(&project).functions;
            order_candidates_with_cold(raw.clone(), &fs, &region, |node| {
                crate::health::is_cold(&cloud.registry, node)
            })
        };

        if cands.is_empty() {
            // The deployment EXISTS in the mesh but no healthy node can serve it: a
            // container whose lease owner is currently unhealthy, or a function with
            // failover:false + primary down / all configured regions unhealthy. Never
            // cross into a forbidden region — 503 rather than silently routing
            // elsewhere (#5/#6). `container_owner.is_some()` ⇒ the deployment exists
            // (we hold its lease) even if peer_routes is empty on this node.
            if container_owner.is_some() {
                let mut ev = cloud.event(
                    &region,
                    &method,
                    &host,
                    &path,
                    503,
                    "mesh-region-unavailable",
                    &sub,
                );
                ev.request_id = rid.clone();
                cloud.record(ev);
                return fail_response(fluid_core::FailureClass::NoHealthyRegion, &region, &rid);
            }
            // FUNCTION with healthy routes but none in the configured/preferred region
            // PER THIS NODE'S route table. That's almost always incomplete/mislabeled
            // per-node metadata (serve-hosts gossip is non-full-mesh; a peer learned the
            // host's route with a stale/missing region), NOT a genuine region outage —
            // the deployment is provably healthy somewhere (`raw` is non-empty). Falsely
            // 503ing a healthy deployment (the "no healthy region from some nodes" bug)
            // is worse than serving it: fall back to the healthy routes (latency-first),
            // which point at the real hosting node (itself in an allowed region, since
            // placement only hosts in allowed regions). Log it so the metadata gap is
            // observable.
            if had_routes {
                tracing::warn!(
                    sub = %sub,
                    routes = ?raw.iter().map(|r| (r.node_id.clone(), r.region.clone())).collect::<Vec<_>>(),
                    "no candidate in preferred region; falling back to healthy routes (incomplete per-node route metadata)"
                );
                let mut fb = raw;
                fb.sort_by_key(|r| {
                    (
                        u8::from(crate::health::is_cold(&cloud.registry, &r.node_id)),
                        if r.region == region { 0u8 } else { 1u8 },
                        hive_edge::region::peer_latency_sort_key(r.latency_ms),
                    )
                });
                cands = fb;
            }
            // else: no routes at all → fall through to DEPLOYMENT_NOT_FOUND below.
        }
        if !cands.is_empty() {
            let path_q = if query.is_empty() {
                path.clone()
            } else {
                format!("{path}?{query}")
            };

            // Region-affinity redirect (browser navigations only): a request that
            // entered the WRONG region for this deployment (e.g. `app.iad.ngrok.pizza`
            // when the app is hosted in sj) is 307'd to the app's HOME-region URL so the
            // next hop lands DIRECT instead of mesh-forwarding cross-region. Gated to:
            // region-coded hosts (`*.<code>.ngrok.pizza`, never the legacy zone), a
            // `text/html` Accept (top-level navigation — API/asset/fetch clients fall
            // through and forward transparently, so non-redirect clients never break),
            // non-WS, and only when the home code actually DIFFERS — so the redirected
            // request, now in the home region, serves local and never re-redirects (no
            // loop). `cands[0]` is the chosen serving node, so its region is the home.
            if !is_ws_upgrade {
                if let Some(host_code) = host_region_code(&host) {
                    let home_code = crate::admin::region_code(&cands[0].region);
                    let wants_html = header(&headers_vec, "accept")
                        .map(|a| a.contains("text/html"))
                        .unwrap_or(false);
                    if wants_html && !home_code.is_empty() && home_code != host_code {
                        let target = format!("https://{sub}.{home_code}.ngrok.pizza{path_q}");
                        let mut ev = cloud.event(
                            &region,
                            &method,
                            &host,
                            &path,
                            307,
                            "region-redirect",
                            home_code,
                        );
                        ev.request_id = rid.clone();
                        cloud.record(ev);
                        return axum::response::Redirect::temporary(&target).into_response();
                    }
                }
            }

            let mesh = cloud.mesh.read().clone();
            let node_iroh: std::collections::HashMap<String, String> = cloud
                .registry
                .nodes()
                .into_iter()
                .filter_map(|n| n.iroh_addr.map(|a| (n.id, a)))
                .collect();
            // Forwarded headers: preserve the original Host + everything else, add the
            // loop guard and the request-id (#4). (Hop-by-hop stripped.)
            let mut fwd_headers: Vec<(String, String)> = vec![
                ("host".into(), host.clone()),
                ("x-hive-proxied".into(), "1".into()),
                ("x-hive-request-id".into(), rid.clone()),
            ];
            for (k, v) in &headers_vec {
                let lk = k.to_lowercase();
                if matches!(
                    lk.as_str(),
                    "host"
                        | "connection"
                        | "content-length"
                        | "x-hive-proxied"
                        | "x-hive-request-id"
                ) {
                    continue;
                }
                fwd_headers.push((k.clone(), v.clone()));
            }

            // WebSocket (#2): prefer a genuinely LOCAL splice when the best
            // candidate IS this node — `local_ws_proxy` resolves + leases
            // directly, no mesh hop at all, which also sidesteps a self-dial
            // through our own trunk back into this same pipeline that a
            // uniform `ws_proxy`-for-every-candidate loop would otherwise
            // attempt. Any OTHER candidate still gets the raw cross-node byte
            // splice over the iroh trunk (`open_raw` is failover-safe — the
            // client upgrade is only consumed once an owner side is ready —
            // so trying each remaining candidate's trunk in turn is safe).
            if is_ws_upgrade {
                let host_opt = if host.is_empty() {
                    None
                } else {
                    Some(host.as_str())
                };
                if cands.iter().any(|c| c.node_id == cloud.node_name) {
                    if let Ok(resp) = local_ws_proxy(
                        &mut req,
                        &cloud,
                        host_opt,
                        &path,
                        &method,
                        &path_q,
                        &fwd_headers,
                    )
                    .await
                    {
                        let mut ev = cloud.event(
                            &region,
                            &method,
                            &host,
                            &path,
                            resp.status().as_u16(),
                            "local-ws",
                            &cloud.node_name,
                        );
                        ev.request_id = rid.clone();
                        cloud.record(ev);
                        return resp;
                    }
                }
                if let Some(pool) = &mesh {
                    for cand in cands.iter().filter(|c| c.node_id != cloud.node_name) {
                        if let Some(addr_json) = node_iroh.get(&cand.node_id) {
                            if let Ok(resp) = ws_proxy(
                                &mut req,
                                pool,
                                &cloud.registry,
                                &cand.node_id,
                                addr_json,
                                &method,
                                &path_q,
                                &fwd_headers,
                            )
                            .await
                            {
                                let mut ev = cloud.event(
                                    &region,
                                    &method,
                                    &host,
                                    &path,
                                    resp.status().as_u16(),
                                    "mesh-ws",
                                    &cand.node_id,
                                );
                                ev.request_id = rid.clone();
                                cloud.record(ev);
                                return resp;
                            }
                        }
                    }
                }
                let mut ev = cloud.event(&region, &method, &host, &path, 502, "mesh-ws-fail", &sub);
                ev.request_id = rid.clone();
                cloud.record(ev);
                let mut resp = (
                    StatusCode::BAD_GATEWAY,
                    "no node could serve this websocket",
                )
                    .into_response();
                set(&mut resp, "x-hive-region", &region);
                set(&mut resp, "x-hive-request-id", &rid);
                return resp;
            }

            // HTTP: buffer the request body once (reused across failover attempts).
            let rmethod =
                reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
            let (_parts, body) = req.into_parts();
            // Track replayability (#7): a body we couldn't fully buffer is NOT safe to
            // re-send on a retry. (>16 MiB streaming uploads are a known gap — they
            // still go out empty on the first attempt; here we just never RETRY them.)
            let (body_bytes, body_replay) = match axum::body::to_bytes(body, 16 * 1024 * 1024).await
            {
                Ok(b) => (b, crate::retry::BodyReplay::Buffered),
                Err(_) => (Bytes::new(), crate::retry::BodyReplay::NonReplayable),
            };
            // Retry-safety inputs (#6): idempotent method, or an explicit idempotency key.
            let has_idem_key = header(&headers_vec, "idempotency-key").is_some()
                || header(&headers_vec, "x-idempotency-key").is_some();
            // Once a request has been attempted on one transport/peer, any further
            // attempt is a RETRY — only allowed when safe (no double side-effect, no
            // started response, replayable body). The FIRST attempt always proceeds.
            let mut attempted = false;
            let mut refused_retry = false;

            'cands: for cand in &cands {
                // One coherent per-candidate deadline (#H4): iroh (bounded ~connect
                // + open + firstbyte) and the HTTP fallback SHARE this budget rather
                // than stacking (iroh worst-case + a full fresh 30s). The HTTP
                // fallback below gets whatever time is left.
                let cand_start = std::time::Instant::now();
                // TRANSPORT SELECTION BY COST, from live measurement rather than a
                // blanket preference: iroh is 2–3.6x FASTER cross-region (0.07–0.4s
                // vs 0.27–0.79s, its QUIC path beating a fresh cross-ocean TCP+TLS
                // handshake), but it carries a ~40ms per-request stream-setup cost
                // that DOMINATES when the network RTT is small — measured 0.052s
                // iroh vs 0.012s direct HTTP to a same-region peer. So: a
                // SAME-REGION candidate with a public gateway gets direct HTTP
                // FIRST (bounded short, see below, so a dead peer can't eat the
                // budget) with iroh as its fallback; everything else keeps
                // iroh-first with HTTP as the fallback. Same two attempts, same
                // retry gates, same shared budget — only the order changes.
                let same_region_http_first = !cand.region.is_empty()
                    && cand.region == region
                    && !cand.gateway.trim().is_empty();
                // Two passes per candidate — the SAME two attempts as before, in
                // the cost-chosen order. Pass 0 is the preferred transport.
                for pass in 0..2u8 {
                    let do_iroh = (pass == 0) != same_region_http_first;
                    if do_iroh {
                        // Prefer the real P2P (iroh QUIC) tunnel when both nodes have it —
                        // works across NATs. STREAM the response so SSE / chunked bodies
                        // arrive incrementally cross-node (#1). Fall through to HTTP on error.
                        if let (Some(pool), Some(addr_json)) = (&mesh, node_iroh.get(&cand.node_id))
                        {
                            // Gate a RE-attempt: refuse if retrying could double-execute / replay unsafely.
                            if attempted
                                && !crate::retry::can_retry(
                                    &method,
                                    has_idem_key,
                                    body_replay,
                                    crate::retry::ResponseState::NotStarted,
                                )
                                .allowed
                            {
                                refused_retry = true;
                                break 'cands;
                            }
                            attempted = true;
                            match pool
                                .request_stream(
                                    &cand.node_id,
                                    addr_json,
                                    &method,
                                    &path_q,
                                    &fwd_headers,
                                    &body_bytes,
                                )
                                .await
                            {
                                Ok(ts) => {
                                    let status = ts.status;
                                    let mut builder = Response::builder().status(ts.status);
                                    for (k, v) in &ts.headers {
                                        let lk = k.to_lowercase();
                                        if matches!(
                                            lk.as_str(),
                                            "transfer-encoding" | "connection" | "content-length"
                                        ) {
                                            continue;
                                        }
                                        if let (Ok(name), Ok(val)) = (
                                            HeaderName::from_bytes(k.as_bytes()),
                                            HeaderValue::from_bytes(v.as_bytes()),
                                        ) {
                                            builder = builder.header(name, val);
                                        }
                                    }
                                    // Body streams from the tunnel's chunk channel — no
                                    // buffering, no forced content-length on a streamed body.
                                    // The whole `TunnelStream` is moved into the unfold state
                                    // so the tunnel stays open until the body is fully sent.
                                    let stream = futures::stream::unfold(ts, |mut ts| async move {
                                        ts.recv()
                                            .await
                                            .map(|chunk| (Ok::<Bytes, std::io::Error>(chunk), ts))
                                    });
                                    let mut out =
                                        builder.body(Body::from_stream(stream)).unwrap_or_else(
                                            |_| StatusCode::BAD_GATEWAY.into_response(),
                                        );
                                    // Report the ACTUAL serving region (the owner node's),
                                    // not this intermediate node's — else the Network tab
                                    // shows the wrong region for a cross-node serve.
                                    let serve_region = if cand.region.is_empty() {
                                        region.as_str()
                                    } else {
                                        cand.region.as_str()
                                    };
                                    set(&mut out, "x-hive-routed-to", &cand.node_id);
                                    set(&mut out, "x-hive-transport", "iroh-p2p");
                                    set(&mut out, "x-hive-region", serve_region);
                                    set(&mut out, "x-hive-request-id", &rid);
                                    let mut ev = cloud.event(
                                        serve_region,
                                        &method,
                                        &host,
                                        &path,
                                        status,
                                        "mesh-route-p2p",
                                        &cand.node_id,
                                    );
                                    ev.request_id = rid.clone();
                                    cloud.record(ev);
                                    return out;
                                }
                                Err(e) => {
                                    // A pre-send (connect/open) timeout is the strongest
                                    // "peer dead" signal (#H4): mark it unhealthy so
                                    // order_candidates stops ranking it — otherwise every
                                    // request re-pays the connect budget against a dead node.
                                    if e.downcast_ref::<hive_p2p::DeadPeerTimeout>().is_some() {
                                        // Guarded chokepoint (health.rs): a peer that
                                        // is still gossiping keeps its fleet-visible
                                        // health and is only deprioritized locally —
                                        // one request hitting a stale trunk must not
                                        // pull a live node out of DNS/placement.
                                        crate::health::demote(
                                            &cloud.registry,
                                            &cand.node_id,
                                            "p2p forward: connect/open timeout",
                                            None,
                                        );
                                    }
                                    /* iroh failed → the other transport gets the next pass */
                                }
                            }
                        }
                        continue; // this pass was iroh; next pass (if any) is HTTP
                    }
                    // ---- HTTP attempt (preferred pass for a same-region public peer,
                    // fallback pass otherwise) ----
                    // Gate it like any re-attempt (after iroh for this candidate, or
                    // after a previous candidate). First attempt passes.
                    if attempted
                        && !crate::retry::can_retry(
                            &method,
                            has_idem_key,
                            body_replay,
                            crate::retry::ResponseState::NotStarted,
                        )
                        .allowed
                    {
                        refused_retry = true;
                        break 'cands;
                    }
                    attempted = true;
                    let url = format!("{}{}", cand.gateway.trim_end_matches('/'), path_q);
                    let mut rb = cloud
                        .http
                        .request(rmethod.clone(), &url)
                        .header("host", &host)
                        .header("x-hive-proxied", "1")
                        .header("x-hive-request-id", &rid)
                        .timeout(if same_region_http_first && pass == 0 {
                            // Preferred intra-region direct hop: bound it SHORT so a
                            // dead same-region peer costs ~2s before iroh gets the
                            // rest of the budget — measured healthy latency here is
                            // ~12ms, so 2s is two orders of magnitude of headroom.
                            std::time::Duration::from_millis(
                                std::env::var("HIVE_FORWARD_INTRA_HTTP_MS")
                                    .ok()
                                    .and_then(|s| s.parse().ok())
                                    .filter(|&v| v > 0)
                                    .unwrap_or(2000),
                            )
                            .min(http_fallback_budget(cand_start))
                        } else {
                            http_fallback_budget(cand_start)
                        })
                        .body(body_bytes.clone());
                    for (k, v) in &headers_vec {
                        let lk = k.to_lowercase();
                        if matches!(
                            lk.as_str(),
                            "host"
                                | "connection"
                                | "content-length"
                                | "x-hive-proxied"
                                | "x-hive-request-id"
                        ) {
                            continue;
                        }
                        rb = rb.header(k, v);
                    }
                    match rb.send().await {
                        Ok(r) => {
                            let status = r.status();
                            let rheaders = r.headers().clone();
                            let mut builder = Response::builder().status(status.as_u16());
                            for (k, v) in rheaders.iter() {
                                let lk = k.as_str().to_lowercase();
                                // Skip hop-by-hop headers — the body framing is re-derived.
                                if matches!(
                                    lk.as_str(),
                                    "transfer-encoding" | "connection" | "content-length"
                                ) {
                                    continue;
                                }
                                if let (Ok(name), Ok(val)) = (
                                    HeaderName::from_bytes(k.as_str().as_bytes()),
                                    HeaderValue::from_bytes(v.as_bytes()),
                                ) {
                                    builder = builder.header(name, val);
                                }
                            }
                            // Stream the upstream body through (SSE / chunked stay incremental).
                            let mut out = builder
                                .body(Body::from_stream(r.bytes_stream()))
                                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
                            let serve_region = if cand.region.is_empty() {
                                region.as_str()
                            } else {
                                cand.region.as_str()
                            };
                            set(&mut out, "x-hive-routed-to", &cand.node_id);
                            // Which transport actually served, so the cost-based
                            // choice above is observable instead of inferred (the
                            // iroh path has always set this; the HTTP path never did,
                            // which is why "everything uses iroh" had to be measured
                            // by elimination).
                            set(&mut out, "x-hive-transport", "http-direct");
                            set(&mut out, "x-hive-region", serve_region);
                            set(&mut out, "x-hive-request-id", &rid);
                            let mut ev = cloud.event(
                                serve_region,
                                &method,
                                &host,
                                &path,
                                status.as_u16(),
                                "mesh-route",
                                &cand.node_id,
                            );
                            ev.request_id = rid.clone();
                            cloud.record(ev);
                            return out;
                        }
                        Err(_) => {} // this pass failed — the other transport gets the next pass (if any)
                    }
                } // end per-transport pass loop
            }
            // Either every candidate failed, OR the first attempt failed and the
            // request was not safe to retry (non-idempotent / non-replayable). In the
            // latter case we deliberately do NOT re-execute it on another peer.
            let (action, class) = if refused_retry {
                ("mesh-route-noretry", fluid_core::FailureClass::NotRetryable)
            } else {
                ("mesh-route-fail", fluid_core::FailureClass::PeerUnreachable)
            };
            let mut ev = cloud.event(&region, &method, &host, &path, class.status(), action, &sub);
            ev.request_id = rid.clone();
            cloud.record(ev);
            let mut resp = fail_response(class, &region, &rid);
            if refused_retry {
                set(&mut resp, "x-hive-retry", "refused");
            }
            return resp;
        }
    }

    // No deployment is aliased for this host (exact match) and no peer in the mesh
    // serves it either → the deployment genuinely doesn't exist on the wildcard
    // domain. Render the Vercel-style DEPLOYMENT_NOT_FOUND page (region-aware id)
    // instead of silently falling back to the default deployment / preview gate.
    // (`_vercel/*` + `/_shadw/zk` were already handled above.)
    //
    // …UNLESS the fleet knows this label. "Never deployed" and "deployed, but
    // every build failed / nothing is Ready / no healthy route exists from here"
    // are different facts with different fixes, and answering both with the same
    // 404 sent owners hunting a DNS/typo problem for a broken build. Say which.
    if !serve_local && !already_proxied && !host.is_empty() {
        if let Some(known) = known_label_status(&cloud, &sub) {
            let ev = cloud.event(
                &region,
                &method,
                &host,
                &path,
                503,
                "deployment-not-ready",
                &known.detail,
            );
            cloud.record(ev);
            return deployment_not_ready(&region, &known);
        }
        let ev = cloud.event(
            &region,
            &method,
            &host,
            &path,
            404,
            "deployment-not-found",
            &host,
        );
        cloud.record(ev);
        return deployment_not_found(&region);
    }

    // -1) L7 DDoS mitigation: shed per-IP floods before any compute work.
    // (Account-level IP blocking runs earlier — before mesh routing.)
    if !cloud.ratelimit.check(&ip, hive_core::now_ms()) {
        let ev = cloud.event(&region, &method, &host, &path, 429, "rate-limited", &ip);
        cloud.record(ev);
        let mut resp = (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED").into_response();
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-ratelimit", "exceeded");
        return resp;
    }

    // Anycast: pick the optimal (lowest-latency healthy) node for this request.
    let anycast = cloud.registry.anycast(Some(&region));

    // 0) Routing layer: redirects (respond now) + rewrites (change path).
    match cloud.router.evaluate(&path) {
        RouteOutcome::Redirect { location, status } => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::PERMANENT_REDIRECT);
            let mut resp = (code, "").into_response();
            set(&mut resp, "location", &location);
            set(&mut resp, "x-hive-region", &region);
            let ev = cloud.event(
                &region, &method, &host, &path, status, "redirect", &location,
            );
            cloud.record(ev);
            return resp;
        }
        RouteOutcome::Continue(p) => {
            if p != path {
                let ev = cloud.event(&region, &method, &host, &path, 0, "rewrite", &p);
                cloud.record(ev);
                path = p;
            }
        }
    }
    let path_q = if query.is_empty() {
        path.clone()
    } else {
        format!("{path}?{query}")
    };

    // 1) WAF (evaluate on percent-decoded path+query).
    let dpath = percent_decode(&path);
    let dquery = percent_decode(&query);
    if let Verdict::Deny { rule_id, reason } = cloud.waf.evaluate(&RequestCtx {
        method: &method,
        path: &dpath,
        query: &dquery,
        ip: &ip,
        headers: &headers_vec,
    }) {
        let ev = cloud.event(
            &region,
            &method,
            &host,
            &path,
            403,
            "waf-deny",
            &format!("{rule_id}: {reason}"),
        );
        cloud.record(ev);
        let mut resp =
            (StatusCode::FORBIDDEN, format!("blocked by WAF ({rule_id})")).into_response();
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-waf-rule", &rule_id);
        return resp;
    }

    // 2) Bot management (incl. the three AI traffic types).
    let policy = *cloud.bot_policy.read();
    match cloud.bot.evaluate(&ua, policy) {
        hive_edge::bot::BotVerdict::Block(reason) => {
            let ev = cloud.event(&region, &method, &host, &path, 403, "bot-block", &reason);
            cloud.record(ev);
            let mut resp = (StatusCode::FORBIDDEN, format!("blocked: {reason}")).into_response();
            set(&mut resp, "x-hive-region", &region);
            set(&mut resp, "x-hive-bot", &reason);
            return resp;
        }
        hive_edge::bot::BotVerdict::Log(label) => {
            // Allowed, but recorded so it shows up in firewall analytics.
            let ev = cloud.event(&region, &method, &host, &path, 0, "bot-log", &label);
            cloud.record(ev);
        }
        hive_edge::bot::BotVerdict::Allow => {}
    }

    // 2.5) Preview protection: preview deployments are private to team members
    // by default. Anonymous requests to a protected preview host get a 401.
    if let Some(resp) = preview_gate(&cloud, &host, &headers_vec, &region, &method, &path, &query) {
        return resp;
    }

    // 3) CDN cache (GET) — serve HIT directly, or STALE while revalidating.
    //
    // RSC bypass: Next.js App Router fetches the SAME url with an `RSC` header to
    // get a React-Server-Components payload instead of the HTML document (its
    // responses set `Vary: rsc`). Our cache keys on host+path only, so without
    // this an RSC request and a document request for `/` collide — the browser
    // gets HTML where it expects an RSC payload (or vice-versa) and the app
    // renders blank/gray. Treat RSC requests as non-cacheable so each always
    // reaches the function; the HTML document still caches for normal requests.
    let is_rsc = header(&headers_vec, "rsc").is_some();
    let cache_key = CdnCache::key(&host, &path_q);
    if method == "GET" && !is_rsc {
        match cloud.cdn.lookup(&cache_key) {
            Lookup::Hit(c) => {
                let ev = cloud.event(&region, &method, &host, &path, c.status, "cache-hit", "");
                cloud.record(ev);
                return cached_response(c, &region, CacheState::Hit);
            }
            Lookup::Stale(c) => {
                // stale-while-revalidate: serve stale now, refresh in background —
                // unless a prior revalidation attempt failed and hasn't cleared
                // its 30s backoff yet (Vercel's documented ISR failed-revalidation
                // retry window): every request against a hot stale key would
                // otherwise fire its own background refresh and hammer a
                // possibly-still-failing origin.
                if !c.revalidation_backoff_active() {
                    spawn_revalidate(
                        cloud.clone(),
                        host.clone(),
                        path_q.clone(),
                        cache_key.clone(),
                    );
                }
                let ev = cloud.event(
                    &region,
                    &method,
                    &host,
                    &path,
                    c.status,
                    "cache-stale",
                    "swr",
                );
                cloud.record(ev);
                return cached_response(c, &region, CacheState::Stale);
            }
            Lookup::Miss => {}
        }
    }

    // 4a) Per-DEPLOYMENT concurrency admission (host-keyed burst budget). A
    // single busy deployment can only exhaust its OWN budget, so it cannot
    // starve every other tenant on this node — the node-wide backstop below no
    // longer conflates one noisy tenant with genuine total overload.
    if !cloud.admission.check(&host, hive_core::now_ms()) {
        let ev = cloud.event(
            &region,
            &method,
            &host,
            &path,
            503,
            "throttled",
            "FUNCTION_THROTTLED",
        );
        cloud.record(ev);
        let mut resp = (StatusCode::SERVICE_UNAVAILABLE, "FUNCTION_THROTTLED").into_response();
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-error", "FUNCTION_THROTTLED");
        set(&mut resp, "x-hive-throttle", "deployment");
        return resp;
    }
    // 4b) Node-wide concurrency backstop (total-overload self-protection).
    if !cloud.limiter.try_admit() {
        let ev = cloud.event(
            &region,
            &method,
            &host,
            &path,
            503,
            "throttled",
            "FUNCTION_THROTTLED",
        );
        cloud.record(ev);
        let mut resp = (StatusCode::SERVICE_UNAVAILABLE, "FUNCTION_THROTTLED").into_response();
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-error", "FUNCTION_THROTTLED");
        set(&mut resp, "x-hive-throttle", "node");
        return resp;
    }

    // 5) Compute: route through the gateway (rewrite path back onto the request).
    let req = with_path(req, &path, &query);
    let resp = next.run(req).await;
    let status = resp.status().as_u16();

    let cacheable = method == "GET" && !is_rsc && status == 200 && is_cacheable(resp.headers());
    let action = if cacheable { "cache-store" } else { "allow" };
    let ev = cloud.event(&region, &method, &host, &path, status, action, "");
    cloud.record(ev);

    if cacheable {
        let (parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024)
            .await
            .unwrap_or_else(|_| Bytes::new());
        let hdrs: Vec<(String, String)> = parts
            .headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|s| (k.as_str().to_string(), s.to_string()))
            })
            .collect();
        cloud.cdn.maybe_store(&cache_key, status, &hdrs, &bytes);
        let mut resp = Response::from_parts(parts, Body::from(bytes));
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-cache", CacheState::Miss.header());
        set_anycast(&mut resp, &anycast);
        set_mfe(&mut resp, &mfe_dbg);
        resp
    } else {
        let mut resp = resp;
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-cache", CacheState::Miss.header());
        set_anycast(&mut resp, &anycast);
        set_mfe(&mut resp, &mfe_dbg);
        resp
    }
}

/// Map a node region (city slug) to a short region-instance code, Vercel-style
/// (`sfo1`, `iad1`, …). Falls back to a slug-derived code for unknown regions.
fn region_code(region: &str) -> String {
    let r = region.to_lowercase();
    let code = match r.replace(' ', "-").as_str() {
        "san-francisco" | "san-jose" | "norcal" => "sfo1",
        "los-angeles" | "socal" => "lax1",
        "seattle" => "sea1",
        "portland" => "pdx1",
        "chicago" => "ord1",
        "dallas" | "texas" => "dfw1",
        "new-york" | "newark" | "virginia" | "us-east" | "washington" => "iad1",
        "miami" => "mia1",
        "toronto" => "yyz1",
        "london" => "lhr1",
        "dublin" => "dub1",
        "frankfurt" => "fra1",
        "paris" => "cdg1",
        "amsterdam" => "ams1",
        "stockholm" => "arn1",
        "singapore" => "sin1",
        "hong-kong" | "hongkong" => "hkg1",
        "tokyo" => "hnd1",
        "osaka" => "kix1",
        "seoul" => "icn1",
        "sydney" => "syd1",
        "mumbai" => "bom1",
        "sao-paulo" | "sao paulo" => "gru1",
        _ => "",
    };
    if !code.is_empty() {
        return code.to_string();
    }
    let slug: String = r
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(3)
        .collect();
    if slug.is_empty() {
        "dev1".into()
    } else {
        format!("{slug}1")
    }
}

/// Vercel-style `404: NOT_FOUND` / `DEPLOYMENT_NOT_FOUND` page. The id encodes the
/// serving region instance: `<region-code>::<rand>-<timestamp>-<hash>`.
/// The ngrok ingress region code of a `*.<code>.ngrok.pizza` host (iad/sin/sfo/lax),
/// or None for the legacy zone, localhost, or any non-region host. Used to detect a
/// region-MISMATCHED request so a browser can be redirected to the app's home region.
/// NOTE: distinct from the vanity `region_code` above (which returns `iad1`/`sfo1` for
/// error-IDs); these no-digit codes are the actual reserved ngrok wildcards.
fn host_region_code(host: &str) -> Option<&'static str> {
    let h = host.split(':').next().unwrap_or(host);
    let stem = h.strip_suffix(".ngrok.pizza")?;
    match stem.rsplit('.').next()? {
        "iad" => Some("iad"),
        "sin" => Some("sin"),
        "sfo" => Some("sfo"),
        "lax" => Some("lax"),
        _ => None,
    }
}

fn deployment_not_found(region: &str) -> Response {
    let u = uuid::Uuid::new_v4().simple().to_string();
    let id = format!(
        "{}::{}-{}-{}",
        region_code(region),
        &u[0..5],
        hive_core::now_ms(),
        &u[8..20],
    );
    // Link straight to the error's own doc page, not the docs root — the visitor
    // is here because of this specific error, so the "learn more" target should be
    // the page that explains it (and its Related section fans out from there).
    //
    // PUBLIC origin first: this screen is only ever served to an external visitor,
    // and every node's HIVE_DASHBOARD_URL is the local-dev http://localhost:3002
    // (the same trap dashboard_origin/derive_dashboard exist for) — witnessed live
    // on the deployed fleet handing out a localhost Learn-more link.
    let docs = std::env::var("HIVE_PUBLIC_DASHBOARD_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("HIVE_DASHBOARD_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .map(|d| {
            format!(
                "{}/docs/errors/deployment-not-found",
                d.trim_end_matches('/')
            )
        })
        .unwrap_or_else(|| "/docs/errors/deployment-not-found".into());
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>404: NOT_FOUND</title>
<style>
  html,body{{height:100%}}
  body{{margin:0;background:#fff;color:#000;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased}}
  .wrap{{max-width:780px;margin:0 auto;padding:34vh 24px 0}}
  .card{{border:1px solid #eaeaea;border-radius:8px;padding:26px 30px}}
  .card h1{{font-size:15px;font-weight:400;margin:0 0 20px}}
  .card h1 b{{font-weight:700}}
  .row{{font-size:15px;margin:13px 0;line-height:1.5}}
  code{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace;font-size:14px}}
  .info{{margin-top:22px;border:1px solid #0070f3;border-radius:8px;padding:18px 22px;text-align:center;font-size:15px;color:#0070f3}}
  .info a{{color:#0070f3;text-decoration:none}}
  .info a:hover{{text-decoration:underline}}
</style></head>
<body><div class="wrap">
  <div class="card">
    <h1><b>404</b>: NOT_FOUND</h1>
    <div class="row">Code: <code>`DEPLOYMENT_NOT_FOUND`</code></div>
    <div class="row">ID: <code>`{id}`</code></div>
  </div>
  <div class="info">This deployment cannot be found. For more information and troubleshooting, see <a href="{docs}">our documentation</a>.</div>
</div></body></html>"#,
        id = id,
        docs = docs,
    );
    let mut resp = Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response());
    set(&mut resp, "x-hive-region", region);
    set(&mut resp, "x-hive-error", "DEPLOYMENT_NOT_FOUND");
    resp
}

/// What the fleet knows about a host label that nothing here can serve.
struct KnownLabel {
    project: String,
    /// One sentence naming the actual condition (build state, or "hosted on X
    /// with no healthy route from here").
    reason: String,
    /// Short, log/event-sized form of the same fact.
    detail: String,
}

/// Does ANY deployment record in the fleet — local or gossiped — claim this host
/// label, or does a project by that name exist at all?
///
/// Returns `None` for a label nothing has ever heard of (the real
/// DEPLOYMENT_NOT_FOUND) and `Some` when the label is known but unserved here,
/// which is a different problem with a different fix. Ready records are reported
/// too: a Ready deployment we could not route to is a routing/gossip gap, and
/// saying "not found" for it has actively misdirected debugging.
fn known_label_status(cloud: &Arc<CloudState>, sub: &str) -> Option<KnownLabel> {
    let label_hit = |d: &fluid_core::DeploymentInfo| {
        d.project == sub
            || [&d.alias, &d.commit_alias, &d.branch_alias, &d.id_alias]
                .iter()
                .any(|a| !a.is_empty() && a.split('.').next().unwrap_or(a) == sub)
    };
    // Local records first (authoritative for this node), then the gossiped
    // fleet view; newest wins so the answer describes the latest attempt.
    let mut best: Option<(u64, String, fluid_core::DeployState, Option<String>)> = None;
    for d in cloud.gw.list().into_iter().filter(label_hit) {
        let cand = (d.created_at_ms, d.project.clone(), d.state, None);
        if best.as_ref().is_none_or(|b| b.0 < cand.0) {
            best = Some(cand);
        }
    }
    for (node, deps) in cloud.peer_deployments.read().iter() {
        for d in deps.iter().filter(|d| label_hit(d)) {
            let cand = (
                d.created_at_ms,
                d.project.clone(),
                d.state,
                Some(node.clone()),
            );
            if best.as_ref().is_none_or(|b| b.0 < cand.0) {
                best = Some(cand);
            }
        }
    }
    if let Some((_, project, state, node)) = best {
        let (reason, detail) = match (state, node.as_deref()) {
            // The visitor-facing sentence never names the hosting NODE — that is
            // internal topology on a page served to the public internet. The
            // event detail keeps it, which is where an operator looks anyway.
            (fluid_core::DeployState::Ready, Some(n)) => (
                format!(
                    "The latest deployment of \"{project}\" is ready, but this region has no \
                     healthy route to it right now. Retry in a moment."
                ),
                format!("{project}:ready-unroutable:{n}"),
            ),
            (fluid_core::DeployState::Ready, None) => (
                format!(
                    "The latest deployment of \"{project}\" is ready, but it is not aliased to \
                     this host. Retry in a moment."
                ),
                format!("{project}:ready-unaliased"),
            ),
            (fluid_core::DeployState::Error, _) => (
                format!(
                    "The latest build of \"{project}\" FAILED, so there is no deployment to \
                     serve. Open the project's build logs, fix the error and redeploy."
                ),
                format!("{project}:build-error"),
            ),
            (fluid_core::DeployState::Cancelled, _) => (
                format!(
                    "The latest build of \"{project}\" was cancelled, so there is no deployment \
                     to serve. Redeploy to bring it back."
                ),
                format!("{project}:build-cancelled"),
            ),
            (st, _) => (
                format!(
                    "\"{project}\" is still building ({st:?}). This URL starts serving as soon \
                     as the build finishes."
                ),
                format!("{project}:building"),
            ),
        };
        return Some(KnownLabel {
            project,
            reason,
            detail,
        });
    }
    // No deployment record anywhere, but the project itself exists (every build
    // failed before a record was ever registered — the dropped-browser-opt-in
    // guard and every other pre-registration rejection land here).
    let project = cloud.projects.find_key_ci(sub)?;
    Some(KnownLabel {
        reason: format!(
            "The project \"{project}\" exists but has no deployment: no build has ever completed \
             successfully. Open its build logs, fix the error and redeploy."
        ),
        detail: format!("{project}:no-deployment"),
        project,
    })
}

/// The honest counterpart to [`deployment_not_found`]: the platform KNOWS this
/// project, it just has nothing Ready to serve. 503 (not 404) because the name
/// is real and the condition is temporary by construction — a fixed build makes
/// it serve with no other action.
fn deployment_not_ready(region: &str, known: &KnownLabel) -> Response {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>503: DEPLOYMENT_NOT_READY</title>
<style>
  html,body{{height:100%}}
  body{{margin:0;background:#fff;color:#000;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased}}
  .wrap{{max-width:780px;margin:0 auto;padding:34vh 24px 0}}
  .card{{border:1px solid #eaeaea;border-radius:8px;padding:26px 30px}}
  .card h1{{font-size:15px;font-weight:400;margin:0 0 20px}}
  .card h1 b{{font-weight:700}}
  .row{{font-size:15px;margin:13px 0;line-height:1.5}}
  code{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace;font-size:14px}}
</style></head>
<body><div class="wrap">
  <div class="card">
    <h1><b>503</b>: DEPLOYMENT_NOT_READY</h1>
    <div class="row">Project: <code>{project}</code></div>
    <div class="row">{reason}</div>
  </div>
</div></body></html>"#,
        project = esc(&known.project),
        reason = esc(&known.reason),
    );
    let mut resp = Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response());
    set(&mut resp, "x-hive-region", region);
    set(&mut resp, "x-hive-error", "DEPLOYMENT_NOT_READY");
    set(&mut resp, "x-hive-project", &known.project);
    set(&mut resp, "retry-after", "30");
    resp
}

/// Stamp the anycast routing decision onto a response for observability.
fn set_anycast(resp: &mut Response, node: &Option<hive_edge::NodeInfo>) {
    if let Some(n) = node {
        set(resp, "x-hive-anycast-node", &n.name);
        set(resp, "x-hive-anycast-region", &n.region);
        set(resp, "x-hive-anycast-latency", &n.latency_ms.to_string());
    }
}

fn cached_response(c: hive_edge::cdn::CachedResponse, region: &str, state: CacheState) -> Response {
    let mut resp = Response::builder()
        .status(c.status)
        .body(Body::from(c.body))
        .unwrap();
    for (k, v) in &c.headers {
        if let (Ok(n), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            resp.headers_mut().insert(n, val);
        }
    }
    set(&mut resp, "x-hive-region", region);
    set(&mut resp, "x-hive-cache", state.header());
    resp
}

/// Background stale-while-revalidate refresh: re-fetch through our own gateway
/// and update the cache entry (next lookup becomes a fresh HIT / REVALIDATED).
/// Every early-return path below is a FAILED revalidation in Vercel's documented
/// ISR sense (network error/timeout, or a status outside the success allowlist —
/// `maybe_store` itself classifies the latter) and must call
/// `note_revalidation_failed` so the existing stale entry is preserved and the
/// 30s retry backoff engages, instead of silently doing nothing and letting the
/// very next request re-fire another background refresh immediately.
fn spawn_revalidate(cloud: Arc<CloudState>, host: String, path_q: String, key: String) {
    tokio::spawn(async move {
        // Subdomain host -> gateway. Use the internal gateway via public_base.
        let url = format!("{}{}", cloud.public_base, path_q);
        let resp = match cloud
            .http
            .get(url)
            .header("host", &host)
            .header("x-hive-revalidate", "1")
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(_) => {
                // Connection refused/timeout/DNS failure reaching the origin —
                // no status code at all, so this can't route through
                // maybe_store's own allowlist check.
                cloud.cdn.note_revalidation_failed(&key);
                return;
            }
        };
        let status = resp.status().as_u16();
        let hdrs: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|s| (k.as_str().to_string(), s.to_string()))
            })
            .collect();
        let body = match resp.bytes().await {
            Ok(body) => body,
            Err(_) => {
                // Status line arrived but the body stream broke mid-read
                // (origin died mid-response) — same failure class as a
                // connection error.
                cloud.cdn.note_revalidation_failed(&key);
                return;
            }
        };
        if cloud.cdn.maybe_store(&key, status, &hdrs, &body) {
            cloud.cdn.note_revalidated();
            let ev = cloud.event(
                &cloud.region,
                "GET",
                &host,
                &path_q,
                status,
                "cache-revalidate",
                "",
            );
            cloud.record(ev);
        }
    });
}

fn is_cacheable(headers: &axum::http::HeaderMap) -> bool {
    let hdrs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    hive_edge::cdn::cache_policy(&hdrs).is_some()
}

/// Rebuild the request URI with the (possibly rewritten) path, preserving query.
fn with_path(req: Request, path: &str, query: &str) -> Request {
    let (mut parts, body) = req.into_parts();
    let pq = if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    };
    if let Ok(uri) = pq.parse::<axum::http::Uri>() {
        parts.uri = uri;
    }
    Request::from_parts(parts, body)
}

/// Minimal percent-decode (`%20` -> space, `+` -> space) for WAF inspection.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve the dashboard origin to bounce a protected-preview browser request to.
///
/// When the request arrived over a PUBLIC wildcard host (e.g.
/// `app.deployment.shadow.ngrok.pizza`), the unlock screen must stay on the public
/// tunnel — NOT redirect to `localhost:3002` (which is what every node's
/// `HIVE_DASHBOARD_URL` points at for local dev). Prefer an explicit
/// `HIVE_PUBLIC_DASHBOARD_URL`, else derive it from the matched deploy suffix by
/// dropping the leading `deployment.` label (→ `https://shadow.ngrok.pizza`). For
/// a local/loopback host, use `HIVE_DASHBOARD_URL` as before.
fn dashboard_origin(cloud: &Arc<CloudState>, host: &str) -> Option<String> {
    derive_dashboard(
        host,
        &cloud.deploy_suffixes,
        &cloud.apps_domain,
        std::env::var("HIVE_PUBLIC_DASHBOARD_URL").ok(),
        std::env::var("HIVE_DASHBOARD_URL").ok(),
    )
}

/// Pure core of [`dashboard_origin`] (unit-testable without a node). A public
/// wildcard host resolves to `public_env` if set, else the deploy suffix minus a
/// leading `deployment.` label as `https://…`; a local/loopback host resolves to
/// `local_env`.
fn derive_dashboard(
    host: &str,
    suffixes: &[String],
    apps_domain: &str,
    public_env: Option<String>,
    local_env: Option<String>,
) -> Option<String> {
    let h = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let public_suffix = if h.contains('.') {
        suffixes
            .iter()
            .find(|s| s.as_str() != "localhost" && (h == **s || h.ends_with(&format!(".{s}"))))
            .cloned()
            // `HIVE_DEPLOY_SUFFIXES` defaults to just `["localhost"]` (excluded
            // above) and was never separately set fleet-wide for the real apps
            // domain — silently making this whole redirect a permanent no-op for
            // every *.{apps_domain} preview host, since `apps_domain` (correctly
            // configured via HIVE_APPS_DOMAIN and already used by
            // `state::host_matches_apps_domain` for ingress) was never consulted
            // here. Fall back to it directly instead of depending on a second,
            // redundant env var to duplicate the same value.
            .or_else(|| {
                crate::state::host_matches_apps_domain(&h, apps_domain)
                    .then(|| apps_domain.to_string())
            })
    } else {
        None
    };
    if let Some(suffix) = public_suffix {
        if let Some(u) = public_env {
            let u = u.trim_end_matches('/');
            if !u.is_empty() {
                return Some(u.to_string());
            }
        }
        let base = suffix.strip_prefix("deployment.").unwrap_or(&suffix);
        return Some(format!("https://{base}"));
    }
    local_env
        .map(|u| u.trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

/// Gate access to protected preview deployments. Returns `Some(401)` when the
/// host is a team-private preview and the request carries no valid access
/// credential; `None` to let the request proceed.
fn preview_gate(
    cloud: &Arc<CloudState>,
    host: &str,
    headers: &[(String, String)],
    region: &str,
    method: &str,
    path: &str,
    query: &str,
) -> Option<Response> {
    // Gate by the deployment's ACTUAL environment, not the subdomain. A production
    // deployment is never preview-protected — via its production domain OR its
    // commit/id URLs. Only PREVIEW deployments (target=preview) are gated. (The
    // old `subdomain != project` heuristic wrongly gated production deployments
    // reached through their commit/id aliases.)
    let dep = cloud.gw.deployment_for_host(host)?;
    let project = dep.project.clone();
    let target = if dep.target.is_empty() {
        if dep.production {
            "production"
        } else {
            "preview"
        }
    } else {
        dep.target.as_str()
    };
    let is_preview = !target.eq_ignore_ascii_case("production");

    // Enterprise deployment protection (Vercel-parity): password / standard modes
    // with a configurable scope ("preview" | "all" | "production" only). Evaluated
    // alongside the legacy per-project preview toggle.
    let ent = cloud.enterprise.protection(&project);
    let ent_protects = ent.protects(target);

    // Password mode — a shared password unlocks the deployment (no team identity).
    if ent_protects && ent.mode == "password" {
        // Already unlocked via the sealed `hive_pw` cookie?
        if let Some(pw_cookie) = cookie_value(headers, "hive_pw") {
            if cloud
                .enterprise
                .verify_password_cookie(&project, &pw_cookie)
            {
                return None;
            }
        }
        // Password submission via the unlock form (GET ?__shadw_pw=...). On success
        // set the sealed cookie and redirect to the clean path; on failure re-prompt.
        if let Some(submitted) = query_param(query, "__shadw_pw") {
            if cloud.enterprise.verify_password(&project, &submitted) {
                let cookie = cloud.enterprise.seal_password_cookie(&project, &submitted);
                let mut resp = axum::response::Redirect::temporary(path).into_response();
                set(
                    &mut resp,
                    "set-cookie",
                    &format!("hive_pw={cookie}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400"),
                );
                set(&mut resp, "x-hive-region", region);
                set(&mut resp, "x-hive-preview", "password-unlocked");
                let ev = cloud.event(
                    region,
                    method,
                    host,
                    path,
                    302,
                    "deploy-password-ok",
                    &project,
                );
                cloud.record(ev);
                return Some(resp);
            }
            let ev = cloud.event(
                region,
                method,
                host,
                path,
                401,
                "deploy-password-bad",
                &project,
            );
            cloud.record(ev);
            return Some(password_page(&project, path, region, true));
        }
        let ev = cloud.event(region, method, host, path, 401, "deploy-password", &project);
        cloud.record(ev);
        return Some(password_page(&project, path, region, false));
    }

    // Standard/membership protection: enterprise "standard" scope OR the legacy
    // per-project preview toggle. Anything else passes.
    let legacy_preview = is_preview && cloud.projects.preview_protected(&project);
    if !((ent_protects && ent.mode == "standard") || legacy_preview) {
        return None;
    }
    let team = cloud.projects.team_of(&project);
    if has_preview_access(headers, &team) {
        return None;
    }
    // EXPERIMENT (feature `zkauth`): opt-in anonymous membership access. Honour a
    // valid ZK access cookie (dropped by the bootstrap after verifying a proof),
    // so a member can view a protected preview without revealing identity to this
    // (possibly peer) node. Additive — falls through to the 401 below if absent.
    #[cfg(feature = "zkauth")]
    if crate::zkauth::cookie_access(&project, headers) {
        return None;
    }
    let ev = cloud.event(
        region,
        method,
        host,
        path,
        401,
        "preview-protected",
        &project,
    );
    cloud.record(ev);

    // One-and-done unlock: for a BROWSER navigation (Accept: text/html) we bounce to
    // the dashboard's `/preview-unlock`, which (using the signed-in session) mints a
    // membership proof and redirects back through `/_shadw/zk` to drop the access
    // cookie ON THIS deployment domain — solving the cross-domain cookie problem.
    // Needs HIVE_DASHBOARD_URL (the dashboard origin); without it we fall back to the
    // plain 401 so nothing breaks.
    let wants_html = header(headers, "accept")
        .map(|a| a.contains("text/html"))
        .unwrap_or(false);
    if wants_html && method.eq_ignore_ascii_case("GET") {
        if let Some(dash) = dashboard_origin(cloud, host) {
            {
                let url = format!(
                    "{dash}/preview-unlock?host={}&project={}&team={}&next={}",
                    pct(host),
                    pct(&project),
                    pct(&team),
                    pct(path),
                );
                let mut resp = axum::response::Redirect::temporary(&url).into_response();
                set(&mut resp, "x-hive-region", region);
                set(&mut resp, "x-hive-preview", "unlock-redirect");
                return Some(resp);
            }
        }
    }

    let mut resp = (
        StatusCode::UNAUTHORIZED,
        format!("This preview deployment is private to the \"{team}\" team. Sign in to view it."),
    )
        .into_response();
    set(&mut resp, "x-hive-region", region);
    set(&mut resp, "x-hive-preview", "protected");
    Some(resp)
}

/// Extract one cookie value by name from the request headers.
fn cookie_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let cookie = header(headers, "cookie")?;
    let prefix = format!("{name}=");
    cookie
        .split(';')
        .map(|p| p.trim())
        .find_map(|kv| kv.strip_prefix(&prefix).map(|v| v.to_string()))
}

/// Extract one raw (percent-decoded) query-parameter value from a query string.
fn query_param(query: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&prefix).map(|v| percent_decode(v)))
}

/// Vercel-style deployment-password prompt. A minimal self-contained HTML page
/// that GET-submits `__shadw_pw` back to the same path (the gate then seals a
/// cookie + redirects). `bad` renders the "incorrect password" state.
fn password_page(project: &str, path: &str, region: &str, bad: bool) -> Response {
    let err = if bad {
        "<p style=\"color:#f87171;font-size:13px;margin:0 0 12px\">Incorrect password. Try again.</p>"
    } else {
        ""
    };
    // Preserve any existing path so the redirect lands where the visitor asked.
    let action = path;
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Protected Deployment</title>
<style>
  html,body{{height:100%;margin:0;background:#0c0d10;color:#e5e7eb;font-family:ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif}}
  .wrap{{min-height:100%;display:flex;align-items:center;justify-content:center;padding:24px}}
  .card{{width:100%;max-width:360px;background:#131316;border:1px solid rgba(255,255,255,.08);border-radius:14px;padding:28px}}
  h1{{font-size:18px;margin:0 0 6px}}
  p.sub{{color:#9ca3af;font-size:13px;margin:0 0 20px}}
  input{{width:100%;box-sizing:border-box;background:#0c0d10;border:1px solid rgba(255,255,255,.12);border-radius:8px;color:#fff;padding:10px 12px;font-size:14px;margin-bottom:12px}}
  button{{width:100%;background:#fff;color:#000;border:0;border-radius:8px;padding:10px 12px;font-size:14px;font-weight:600;cursor:pointer}}
  .foot{{margin-top:16px;color:#6b7280;font-size:11px;text-align:center}}
</style></head><body><div class="wrap"><div class="card">
<h1>This deployment is password protected</h1>
<p class="sub">Enter the password for <strong>{project}</strong> to continue.</p>
{err}
<form method="GET" action="{action}">
  <input type="password" name="__shadw_pw" placeholder="Password" autofocus autocomplete="current-password" required>
  <button type="submit">Continue</button>
</form>
<div class="foot">Protected by shadw</div>
</div></div></body></html>"#
    );
    let mut resp = (StatusCode::UNAUTHORIZED, Html(html)).into_response();
    set(&mut resp, "x-hive-region", region);
    set(&mut resp, "x-hive-preview", "password");
    set(&mut resp, "cache-control", "no-store");
    resp
}

/// Percent-encode a query-parameter value (RFC 3986 unreserved kept verbatim).
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn has_preview_access(headers: &[(String, String)], team: &str) -> bool {
    // 1) Bearer token (enforced mode): must be valid and scoped to the team.
    if let Some(auth) = header(headers, "authorization") {
        if let Some(tok) = auth.strip_prefix("Bearer ") {
            if let Ok(claims) = crate::auth::verify(tok) {
                if claims.tenant == team || claims.role == "owner" {
                    return true;
                }
            }
        }
    }
    // 2) Access cookie set by the dashboard after a member signs in.
    if let Some(cookie) = header(headers, "cookie") {
        for part in cookie.split(';') {
            let kv = part.trim();
            if let Some(val) = kv.strip_prefix("hive_access=") {
                if crate::auth::enforced() {
                    if let Ok(claims) = crate::auth::verify(val) {
                        return claims.tenant == team || claims.role == "owner";
                    }
                } else {
                    // Dev mode (no JWT secret): presence of the cookie grants access.
                    return !val.is_empty();
                }
            }
        }
    }
    false
}

/// O(1) single-header read straight from the request's HeaderMap, avoiding the
/// O(n) linear scan over `headers_vec`. Only valid while `req` is owned/borrowable
/// — used for the always-executed EARLY lookups (before any proxy consumes `req`);
/// aggregate consumers (WAF ctx, preview gate, forward loops) keep using
/// `headers_vec` because they need the full header list.
#[inline]
fn hget(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// HTTP-fallback timeout for one candidate (#H4). iroh and the HTTP fallback share
/// a single per-candidate deadline (`HIVE_EDGE_CANDIDATE_MS`, default 30s) instead
/// of stacking — the fallback gets whatever time the iroh attempt left, floored at
/// 1s so a candidate that already burned the budget still gets one quick HTTP shot.
fn http_fallback_budget(cand_start: std::time::Instant) -> std::time::Duration {
    let total_ms = std::env::var("HIVE_EDGE_CANDIDATE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(30_000);
    let total = std::time::Duration::from_millis(total_ms);
    total
        .saturating_sub(cand_start.elapsed())
        .max(std::time::Duration::from_secs(1))
}

/// Order mesh candidates by the deployment's configured region/failover policy (#5).
///
/// * `regions` empty → same-region-first anycast (the serving node's region),
///   latency as tiebreak (unchanged default).
/// * `failover: true` → keep only configured regions, ordered primary-first then
///   latency, so traffic falls through to the next configured region's healthy node.
/// * `failover: false` → restrict to the PRIMARY region only; never cross regions
///   (an empty result makes the caller return 503 instead of routing elsewhere).
///
/// Only `healthy` candidates are passed in; latency is always the within-region
/// tiebreak.
#[allow(dead_code)]
fn order_candidates(
    raw: Vec<crate::state::PeerRoute>,
    fs: &crate::project_settings::FunctionSettings,
    serving_region: &str,
) -> Vec<crate::state::PeerRoute> {
    order_candidates_with_cold(raw, fs, serving_region, |_| false)
}

fn order_candidates_with_cold(
    mut raw: Vec<crate::state::PeerRoute>,
    fs: &crate::project_settings::FunctionSettings,
    serving_region: &str,
    is_cold: impl Fn(&str) -> bool,
) -> Vec<crate::state::PeerRoute> {
    let regions: Vec<&String> = fs.regions.iter().filter(|r| !r.is_empty()).collect();
    let cold = |r: &crate::state::PeerRoute| u8::from(is_cold(&r.node_id));
    let latency =
        |r: &crate::state::PeerRoute| hive_edge::region::peer_latency_sort_key(r.latency_ms);
    if regions.is_empty() {
        raw.sort_by_key(|r| {
            (
                cold(r),
                if r.region == serving_region { 0u8 } else { 1u8 },
                latency(r),
            )
        });
        return balance_same_region(raw, &is_cold);
    }
    let rank = |r: &crate::state::PeerRoute| {
        regions
            .iter()
            .position(|cr| cr.eq_ignore_ascii_case(&r.region))
    };
    if fs.failover {
        raw.retain(|r| rank(r).is_some());
        raw.sort_by_key(|r| (cold(r), rank(r).unwrap_or(usize::MAX) as u16, latency(r)));
    } else {
        let primary = regions[0].clone();
        raw.retain(|r| r.region.eq_ignore_ascii_case(&primary));
        raw.sort_by_key(|r| (cold(r), latency(r)));
    }
    balance_same_region(raw, &is_cold)
}

/// Load-balance across nodes in the BEST (front) region tier so same-region peers
/// — e.g. virginia + virginia-2 — share traffic instead of every request hitting
/// the single lowest-latency node. Rotates the equal-best-region prefix by a
/// global round-robin counter; the rest (failover tiers) keep their order. Still
/// adheres to Fluid Compute: each chosen node runs its own in-instance-concurrent
/// pool, this just spreads the *first-tried* node across same-region peers.
fn balance_same_region(
    mut raw: Vec<crate::state::PeerRoute>,
    is_cold: &impl Fn(&str) -> bool,
) -> Vec<crate::state::PeerRoute> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RR: AtomicUsize = AtomicUsize::new(0);
    // Co-located peers (e.g. virginia + virginia-2) are within this latency band of
    // each other; a much-slower same-region node falls outside it and is NOT rotated
    // into the front (latency still wins there). 30ms comfortably covers intra-region
    // jitter without spanning distinct regions.
    const BAND_MS: u64 = 30;
    if raw.len() > 1 {
        let best = raw[0].region.clone();
        let best_lat = raw[0].latency_ms;
        let best_cold = is_cold(&raw[0].node_id);
        let best_latency_class = hive_edge::region::peer_latency_sort_key(best_lat).0;
        let n = raw
            .iter()
            .take_while(|r| {
                r.region.eq_ignore_ascii_case(&best)
                    && is_cold(&r.node_id) == best_cold
                    && hive_edge::region::peer_latency_sort_key(r.latency_ms).0
                        == best_latency_class
                    && r.latency_ms <= best_lat.saturating_add(BAND_MS)
            })
            .count();
        if n > 1 {
            let k = RR.fetch_add(1, Ordering::Relaxed) % n;
            raw[0..n].rotate_left(k);
        }
    }
    raw
}

/// Cross-node WebSocket proxy (#2), used for every candidate OTHER than this
/// node itself (see [`local_ws_proxy`] for the local case): open a RAW byte
/// stream to the owner over its iroh trunk, replay the upgrade request (so
/// the owner's local target performs the handshake), relay the owner's
/// response head (e.g. `101`) back to the client, then splice raw bytes both
/// ways (client ↔ iroh ↔ owner) — HTTP framing bypassed. `open_raw` runs
/// BEFORE the client upgrade is consumed, so a failed candidate can be
/// retried on the next one.
///
/// The owner splices to its own gateway address (re-entering THIS SAME
/// pipeline as a fresh HTTP connection) — which is exactly why the caller
/// (`edge_pipeline`) never dials this function against `cloud.node_name`
/// itself: that would re-evaluate `is_ws_upgrade` a second time on the same
/// request. `local_ws_proxy` closes that gap end-to-end for the local case:
/// a per-CONNECTION raw escape on the instance's own tunnel listener
/// (`fluid_tunnel::TunnelServer::serve_maybe_raw`, magic-byte-gated) plus a
/// direct lease + splice from THIS layer, no gateway re-entry at all.
///
/// Generic raw TCP/UDP (no HTTP upgrade to replay) does NOT go through this
/// function: the raw-port proxy / UDP relay use `PeerPool::open_raw_to_port`,
/// whose handshake names the target deployment/port explicitly (owner side:
/// `mesh_raw::resolver`).
async fn ws_proxy(
    req: &mut Request,
    pool: &hive_p2p::PeerPool,
    registry: &hive_edge::region::NodeRegistry,
    node_id: &str,
    addr_json: &str,
    method: &str,
    path_q: &str,
    fwd_headers: &[(String, String)],
) -> Result<Response, ()> {
    let mut raw = pool.open_raw(node_id, addr_json).await.map_err(|e| {
        // Pre-send (connect/open) timeout on the raw trunk → stop re-paying the
        // budget against it. Via the guarded chokepoint (health.rs), which marks
        // the peer locally cold and only withdraws it fleet-wide when gossip
        // agrees it is gone.
        if e.downcast_ref::<hive_p2p::DeadPeerTimeout>().is_some() {
            crate::health::demote(registry, node_id, "ws_proxy: connect/open timeout", None);
        }
    })?;
    // Replay the upgrade request to the owner so its target does the WS handshake.
    let mut head = format!("{method} {path_q} HTTP/1.1\r\n");
    for (k, v) in fwd_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    if raw.write_all(head.as_bytes()).await.is_err() {
        return Err(());
    }
    let _ = raw.flush().await;
    // Read the owner's response head (its target's 101 carries the correct
    // Sec-WebSocket-Accept for the client's key, which we forwarded verbatim).
    let (status, resp_headers, leftover) = read_http_head(&mut raw).await?;
    let mut builder = Response::builder().status(status);
    for (k, v) in &resp_headers {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            builder = builder.header(name, val);
        }
    }
    let resp = builder.body(Body::empty()).map_err(|_| ())?;
    // Commit: take over the client connection and splice it to the iroh stream.
    let on_upg = hyper::upgrade::on(&mut *req);
    tokio::spawn(async move {
        if let Ok(upgraded) = on_upg.await {
            let mut client = hyper_util::rt::TokioIo::new(upgraded);
            if !leftover.is_empty() {
                let _ = client.write_all(&leftover).await;
            }
            let _ = tokio::io::copy_bidirectional(&mut client, &mut raw).await;
        }
    });
    Ok(resp)
}

/// LOCAL counterpart to [`ws_proxy`]: when the winning candidate for a
/// WebSocket upgrade is THIS node, splice directly into the instance's
/// tunnel listener instead of dialing out over the iroh mesh to ourselves —
/// avoiding a pointless (and, depending on whether a self-dial resolves at
/// all, potentially wedged) mesh round-trip back through this same node's
/// own public router. Resolves the target function the SAME way an ordinary
/// request would (`Gateway::lease_for_path`), opens a fresh connection to
/// the leased instance's tunnel listener prefixed with
/// [`fluid_tunnel::TunnelServer::RAW_UPGRADE_MAGIC`] (switches that ONE
/// connection on the instance's listener to a raw splice — see
/// `TunnelServer::serve_maybe_raw`), replays the upgrade request exactly as
/// `ws_proxy` does for the mesh case, then splices. The lease rides as the
/// spliced task's guard so inflight accounting covers the whole WS session.
async fn local_ws_proxy(
    req: &mut Request,
    cloud: &Arc<CloudState>,
    host: Option<&str>,
    path: &str,
    method: &str,
    path_q: &str,
    fwd_headers: &[(String, String)],
) -> Result<Response, ()> {
    let lease = cloud.gw.lease_for_path(host, path).await.ok_or(())?;
    let addr = match &lease.endpoint {
        hive_backend::CellEndpoint::Tcp(a) => a.clone(),
        // A vsock (microVM) endpoint has no TCP address to dial directly —
        // not a served shape for this local fast path; the caller falls back
        // to the mesh path (which itself would refuse the same way raw
        // targets do — see `mesh_raw::resolve`).
        hive_backend::CellEndpoint::Vsock { .. } => return Err(()),
    };
    let mut raw = tokio::net::TcpStream::connect(&addr)
        .await
        .map_err(|_| ())?;
    raw.write_all(&[fluid_tunnel::TunnelServer::RAW_UPGRADE_MAGIC])
        .await
        .map_err(|_| ())?;
    let mut head = format!("{method} {path_q} HTTP/1.1\r\n");
    for (k, v) in fwd_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    if raw.write_all(head.as_bytes()).await.is_err() {
        return Err(());
    }
    let _ = raw.flush().await;
    let (status, resp_headers, leftover) = read_http_head(&mut raw).await?;
    let mut builder = Response::builder().status(status);
    for (k, v) in &resp_headers {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            builder = builder.header(name, val);
        }
    }
    let resp = builder.body(Body::empty()).map_err(|_| ())?;
    let on_upg = hyper::upgrade::on(&mut *req);
    tokio::spawn(async move {
        // Held for the whole spliced session — the lease's inflight
        // accounting must cover a WS connection's full lifetime, not just
        // the initial handshake.
        let _guard = lease;
        if let Ok(upgraded) = on_upg.await {
            let mut client = hyper_util::rt::TokioIo::new(upgraded);
            if !leftover.is_empty() {
                let _ = client.write_all(&leftover).await;
            }
            let _ = tokio::io::copy_bidirectional(&mut client, &mut raw).await;
        }
    });
    Ok(resp)
}

/// Read an HTTP/1.x response head (status line + headers up to the blank line)
/// from a raw stream; returns the status, header pairs, and any bytes already read
/// past the head (the start of the spliced body / WS frames).
async fn read_http_head<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
) -> Result<(StatusCode, Vec<(String, String)>, Vec<u8>), ()> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let leftover = buf[pos + 4..].to_vec();
            let text = String::from_utf8_lossy(&buf[..pos]).into_owned();
            let mut lines = text.split("\r\n");
            let code = lines
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|c| c.parse::<u16>().ok())
                .ok_or(())?;
            let status = StatusCode::from_u16(code).map_err(|_| ())?;
            let headers = lines
                .filter_map(|l| {
                    l.split_once(':')
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                })
                .collect();
            return Ok((status, headers, leftover));
        }
        let n = r.read(&mut tmp).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            return Err(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{derive_dashboard, order_candidates};
    use crate::project_settings::FunctionSettings;
    use crate::state::PeerRoute;

    fn suffixes() -> Vec<String> {
        vec!["deployment.shadow.ngrok.pizza".into(), "localhost".into()]
    }

    #[test]
    fn public_wildcard_preview_unlock_uses_public_dashboard_not_localhost() {
        let s = suffixes();
        let local = Some("http://localhost:3002".to_string());
        // Public wildcard host → derived public dashboard (NOT localhost:3002).
        assert_eq!(
            derive_dashboard(
                "app.deployment.shadow.ngrok.pizza",
                &s,
                "",
                None,
                local.clone()
            ),
            Some("https://shadow.ngrok.pizza".to_string())
        );
        // Explicit override wins.
        assert_eq!(
            derive_dashboard(
                "app.deployment.shadow.ngrok.pizza:443",
                &s,
                "",
                Some("https://dash.example.com/".to_string()),
                local.clone()
            ),
            Some("https://dash.example.com".to_string())
        );
        // Local/loopback host → the local dashboard env (dev unchanged).
        assert_eq!(
            derive_dashboard("app.localhost", &s, "", None, local.clone()),
            local
        );
    }

    #[test]
    fn apps_domain_falls_back_when_deploy_suffixes_dont_match() {
        // Live regression: HIVE_DEPLOY_SUFFIXES is unset fleet-wide (defaults to
        // just ["localhost"], which is explicitly excluded from matching), so a
        // real *.{apps_domain} preview host never matched `suffixes` at all —
        // the preview-unlock redirect silently never fired for ANY team/project.
        // `apps_domain` (HIVE_APPS_DOMAIN, already correctly configured and used
        // for ingress) must be consulted as a fallback.
        let s = vec!["localhost".to_string()]; // the real fleet default
        let public = Some("https://shadw.cloud".to_string());
        assert_eq!(
            derive_dashboard(
                "gm-verify-final-fe62ab2.shadw.app",
                &s,
                "shadw.app",
                public.clone(),
                None
            ),
            Some("https://shadw.cloud".to_string())
        );
        // Apex of the apps domain also matches.
        assert_eq!(
            derive_dashboard("shadw.app", &s, "shadw.app", public.clone(), None),
            Some("https://shadw.cloud".to_string())
        );
        // A totally unrelated host matches neither suffixes nor apps_domain.
        assert_eq!(
            derive_dashboard("evil.example.com", &s, "shadw.app", public, None),
            None
        );
    }

    fn route(node: &str, region: &str, lat: u64) -> PeerRoute {
        PeerRoute {
            node_id: node.into(),
            region: region.into(),
            gateway: format!("http://{node}:8787"),
            latency_ms: lat,
            healthy: true,
            last_seen_ms: 0,
        }
    }
    fn fs(regions: &[&str], failover: bool) -> FunctionSettings {
        FunctionSettings {
            regions: regions.iter().map(|s| s.to_string()).collect(),
            failover,
            ..Default::default()
        }
    }

    #[test]
    fn failover_true_orders_by_configured_region_then_falls_through() {
        let raw = vec![
            route("la1", "los-angeles", 5),
            route("bkk1", "bangkok", 50),
            route("sf1", "san-francisco", 1),
        ];
        let out = order_candidates(raw, &fs(&["bangkok", "los-angeles"], true), "san-francisco");
        let ids: Vec<_> = out.iter().map(|r| r.node_id.as_str()).collect();
        // Primary (bangkok) first, then the next configured region (los-angeles);
        // san-francisco is excluded entirely (not a configured region).
        assert_eq!(ids, vec!["bkk1", "la1"]);
    }

    #[test]
    fn failover_false_restricts_to_primary_region() {
        let raw = vec![route("la1", "los-angeles", 5), route("bkk1", "bangkok", 50)];
        let out = order_candidates(raw, &fs(&["bangkok"], false), "los-angeles");
        let ids: Vec<_> = out.iter().map(|r| r.node_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["bkk1"],
            "no cross-region: only the primary region's node"
        );
    }

    #[test]
    fn failover_false_primary_down_yields_empty() {
        let raw = vec![
            route("la1", "los-angeles", 5),
            route("sf1", "san-francisco", 1),
        ];
        let out = order_candidates(raw, &fs(&["bangkok"], false), "los-angeles");
        assert!(
            out.is_empty(),
            "primary down + no failover → empty (caller returns 503, never crosses)"
        );
    }

    #[test]
    fn no_regions_is_anycast_serving_region_first() {
        let raw = vec![
            route("la1", "los-angeles", 5),
            route("sf1", "san-francisco", 50),
        ];
        let out = order_candidates(raw, &fs(&[], false), "san-francisco");
        assert_eq!(
            out[0].node_id, "sf1",
            "serving region first even at higher latency"
        );
    }

    #[test]
    fn within_region_latency_is_the_tiebreak() {
        let raw = vec![
            route("bkk-slow", "bangkok", 90),
            route("bkk-fast", "bangkok", 3),
        ];
        let out = order_candidates(raw, &fs(&["bangkok"], true), "x");
        // 90ms is outside the co-location band of 3ms → not rotated in; latency wins.
        assert_eq!(
            out[0].node_id, "bkk-fast",
            "lowest latency within the region wins"
        );
    }

    #[test]
    fn colocated_same_region_peers_load_balance_across_requests() {
        // virginia + virginia-2 are co-located (near-equal latency). Over several
        // requests the FIRST-tried node must rotate between them (issue #2), instead
        // of every request hammering the single lowest-latency node.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..6 {
            let raw = vec![route("va1", "virginia", 4), route("va2", "virginia", 6)];
            let out = order_candidates(raw, &fs(&["virginia"], true), "virginia");
            seen.insert(out[0].node_id.clone());
            // Both peers are always present (balancing reorders, never drops).
            assert_eq!(out.len(), 2);
        }
        assert!(
            seen.contains("va1") && seen.contains("va2"),
            "both same-region peers must take first-tried turns, got {seen:?}"
        );
    }
}
