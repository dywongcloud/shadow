//! The edge request pipeline, mirroring Vercel's CDN layering: routing -> firewall (WAF + bots) ->
//! concurrency admission -> CDN cache (HIT/STALE/MISS with SWR) -> compute. Adds `x-hive-region`
//! and `x-hive-cache` and records dashboard events.

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

/// Stamps `x-hive-served-by` with THIS node on every response unless a mesh-proxied one already
/// carries the peer's stamp (routing diagnosis; pairs with `x-hive-routed-to`).
pub async fn edge_pipeline(
    State(cloud): State<Arc<CloudState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let node = cloud.node_name.clone();
    let mut resp = edge_pipeline_inner(State(cloud), peer, req, next).await;
    if !resp.headers().contains_key("x-hive-served-by") {
        set(&mut resp, "x-hive-served-by", &node);
    }
    resp
}

/// Platform liveness on the PUBLIC listener: a probe with a Host the platform does not admit (an IP
/// literal, a bare infrastructure name) would 404 once per probe as `host-rejected` and swamp the
/// log, and `/healthz` is no tenant route, so it is served here. The guard is the host gate's own
/// predicate, so an admitted Host keeps its pipeline, and nothing tenant-owned is reachable through
/// it.
fn is_unadmitted_host_liveness_probe(
    cloud: &Arc<CloudState>,
    method: &str,
    path: &str,
    host: &str,
) -> bool {
    path == "/healthz"
        && (method == "GET" || method == "HEAD")
        && !host.is_empty()
        && !cloud.host_allowed(host)
}

/// The edge request pipeline. Invariants: the client IP is the real TCP peer, NEVER
/// `x-forwarded-for` (no proxy fronts a fleet node, so XFF is attacker-controlled); ACME HTTP-01
/// answers run before every other gate; account-level IP blocking happens at the ENTRY node before
/// any mesh proxying; a container deployment is served ONLY by its placement lease owner; a local
/// record that cannot serve or is stale never wins the host over a healthy peer route; a request is
/// retried on another candidate only when safe (idempotent method or key, replayable body, no
/// response started); a renamed-away project name is RESERVED forever, so its permanent redirect is
/// consulted only on the already-failing path; Next.js RSC requests bypass the CDN cache, which
/// keys on host+path only.
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
    let mut host = hget(&req, "host")
        .filter(|s| !s.is_empty())
        .or_else(|| req.uri().authority().map(|a| a.host().to_string()))
        .unwrap_or_default();
    if !host.is_empty() && req.headers().get(axum::http::header::HOST).is_none() {
        if let Ok(v) = HeaderValue::from_str(&host) {
            req.headers_mut().insert(axum::http::header::HOST, v);
        }
    }
    let ua = hget(&req, "user-agent").unwrap_or_default();
    let ip = peer.ip().to_string();
    let region = cloud.region.clone();

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
        if let Some(leader) = cloud.leader_forward_target() {
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

    if !cloud.db_domain.is_empty() {
        let hp = host.split(':').next().unwrap_or(&host).to_ascii_lowercase();
        if hp == cloud.db_domain || hp.ends_with(&format!(".{}", cloud.db_domain)) {
            let status_host = hp.clone();
            /// Dedicated db-tier limiter keyed per (IP, DB host), NOT the shared `cloud.ratelimit`:
            /// every microVM cell on a node NATs out through its public IP, so a shared or per-IP
            /// budget would let one chatty app starve the platform's own reads and every OTHER
            /// database reached from that node.
            fn db_rest_rate_limiter() -> &'static hive_edge::RateLimiter {
                const LIMIT_PER_WINDOW: u32 = 5000;
                const WINDOW_MS: u64 = 10_000;
                static LIMITER: std::sync::OnceLock<hive_edge::RateLimiter> =
                    std::sync::OnceLock::new();
                LIMITER.get_or_init(|| hive_edge::RateLimiter::new(LIMIT_PER_WINDOW, WINDOW_MS))
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

    if path.starts_with("/_vercel/") {
        return next.run(req).await;
    }

    let mut mfe_dbg: Option<[String; 4]> = None;
    if !host.is_empty() && cloud.enterprise.has_mfe() {
        if let Some(dep) = cloud.gw.deployment_for_host(&host) {
            let team = cloud.projects.team_of(&dep.project);
            if let Some(group) = cloud.enterprise.mfe_group_for_host(&team, &dep.project) {
                if let Some((child, matched)) = crate::microfrontends::resolve_child(&group, &path)
                {
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
                        let rest = host.split_once('.').map(|(_, r)| r.to_string());
                        host = match rest {
                            Some(r) => format!("{}.{}", target.deployment_id, r),
                            None => target.deployment_id.clone(),
                        };
                        if let Ok(v) = HeaderValue::from_str(&host) {
                            req.headers_mut().insert(axum::http::header::HOST, v);
                        }
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

    #[cfg(feature = "zkauth")]
    if path.starts_with("/_shadw/zk") && cloud.gw.serves_host(&host) {
        return crate::zkauth::bootstrap(&cloud, &host, &query);
    }

    if is_unadmitted_host_liveness_probe(&cloud, &method, &path, &host) {
        let ev = cloud.event(&region, &method, &host, &path, 200, "health", &host);
        cloud.record(ev);
        return (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            "ok",
        )
            .into_response();
    }

    let rid = hget(&req, "x-hive-request-id")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());

    if !host.is_empty() && !cloud.host_allowed(&host) {
        let mut ev = cloud.event(&region, &method, &host, &path, 404, "host-rejected", &host);
        ev.request_id = rid.clone();
        cloud.record(ev);
        return deployment_not_found(&region);
    }

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
    let container_owner: Option<String> = cloud
        .leases
        .owner_of(&sub)
        .filter(|owner| owner != &cloud.node_name);
    let local_state = serve_local
        .then(|| cloud.gw.host_deploy_state(&host))
        .flatten();
    let local_is_stale =
        serve_local && matches!(local_state, Some(fluid_core::DeployState::Ready)) && {
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
                            .any(|a| {
                                a.eq_ignore_ascii_case(&host)
                                    || a.split('.').next() == Some(sub.as_str())
                            })
                    })
                    .any(|d| (d.production, d.created_at_ms) > local_rank)
            })
        };
    let prefer_peer = serve_local
        && (!matches!(local_state, Some(fluid_core::DeployState::Ready)) || local_is_stale)
        && {
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
        let healthy_ids: std::collections::HashSet<String> = cloud
            .registry
            .nodes()
            .into_iter()
            .filter(|n| n.healthy)
            .map(|n| n.id)
            .collect();
        let raw: Vec<crate::state::PeerRoute> = {
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
        }
        if !cands.is_empty() {
            let path_q = if query.is_empty() {
                path.clone()
            } else {
                format!("{path}?{query}")
            };

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

            let rmethod =
                reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
            let (_parts, body) = req.into_parts();
            let (body_bytes, body_replay) = match axum::body::to_bytes(body, 16 * 1024 * 1024).await
            {
                Ok(b) => (b, crate::retry::BodyReplay::Buffered),
                Err(_) => (Bytes::new(), crate::retry::BodyReplay::NonReplayable),
            };
            let has_idem_key = header(&headers_vec, "idempotency-key").is_some()
                || header(&headers_vec, "x-idempotency-key").is_some();
            let mut attempted = false;
            let mut refused_retry = false;

            'cands: for cand in &cands {
                let cand_start = std::time::Instant::now();
                let same_region_http_first = !cand.region.is_empty()
                    && cand.region == region
                    && !cand.gateway.trim().is_empty();
                let iroh_cooled = !same_region_http_first
                    && mesh.is_some()
                    && node_iroh.contains_key(&cand.node_id)
                    && usable_gateway(&cand.gateway)
                    && iroh_cooling(&cand.node_id);
                let http_first = same_region_http_first || iroh_cooled;
                for pass in 0..2u8 {
                    let do_iroh = (pass == 0) != http_first;
                    if do_iroh {
                        if let (Some(pool), Some(addr_json)) = (&mesh, node_iroh.get(&cand.node_id))
                        {
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
                                    iroh_mark_ok(&cand.node_id);
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
                                    let stream = futures::stream::unfold(ts, |mut ts| async move {
                                        ts.recv()
                                            .await
                                            .map(|chunk| (Ok::<Bytes, std::io::Error>(chunk), ts))
                                    });
                                    let mut out =
                                        builder.body(Body::from_stream(stream)).unwrap_or_else(
                                            |_| StatusCode::BAD_GATEWAY.into_response(),
                                        );
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
                                    if e.downcast_ref::<hive_p2p::PostSendTimeout>().is_some()
                                        || e.downcast_ref::<hive_p2p::DeadPeerTimeout>().is_some()
                                    {
                                        iroh_mark_bad(&cand.node_id);
                                    }
                                    if e.downcast_ref::<hive_p2p::DeadPeerTimeout>().is_some() {
                                        crate::health::demote(
                                            &cloud.registry,
                                            &cand.node_id,
                                            "p2p forward: connect/open timeout",
                                            None,
                                        );
                                    }
                                }
                            }
                        }
                        continue;
                    }
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
                    // `path_q` is the VISITOR's own request path+query: a raw TCP client can put
                    // almost anything in it (the HTTP request-line grammar allows `@`/`:`/`//` in a
                    // path), so it is set as the PATH and QUERY of the gossiped `cand.gateway`
                    // base (`with_request_target`), never concatenated into a URL string and never
                    // resolved as a reference: the authority cannot change by construction, and a
                    // legitimate `//assets/x.js` request still reaches the backend as that path.
                    let Some(url) = reqwest::Url::parse(cand.gateway.trim_end_matches('/'))
                        .ok()
                        .map(|base| with_request_target(base, &path_q))
                    else {
                        continue;
                    };
                    // Never follows a redirect: a 3xx from the backend is relayed to the visitor
                    // verbatim below (their own client decides whether to follow it), and the node
                    // never dereferences a Location it does not control server-side.
                    let mut rb = hive_outbound::internal_client()
                        .request(rmethod.clone(), url)
                        .header("host", &host)
                        .header("x-hive-proxied", "1")
                        .header("x-hive-request-id", &rid)
                        .timeout(if same_region_http_first && pass == 0 {
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
                            let mut out = builder
                                .body(Body::from_stream(r.bytes_stream()))
                                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
                            let serve_region = if cand.region.is_empty() {
                                region.as_str()
                            } else {
                                cand.region.as_str()
                            };
                            set(&mut out, "x-hive-routed-to", &cand.node_id);
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
                        Err(_) => {}
                    }
                }
            }
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

    if !serve_local && !already_proxied && !host.is_empty() {
        if let Some(target) = cloud.projects.rename_target(&sub) {
            if !target.eq_ignore_ascii_case(&sub) && cloud.projects.find_key_ci(&sub).is_none() {
                let path_q = if query.is_empty() {
                    path.clone()
                } else {
                    format!("{path}?{query}")
                };
                let location = format!("{}{}", cloud.deploy_url(&target), path_q);
                let ev = cloud.event(
                    &region,
                    &method,
                    &host,
                    &path,
                    308,
                    "project-renamed",
                    &format!("{}->{}", sub, target),
                );
                cloud.record(ev);
                let mut resp = (StatusCode::PERMANENT_REDIRECT, "").into_response();
                set(&mut resp, "location", &location);
                set(&mut resp, "x-hive-region", &region);
                set(&mut resp, "x-hive-error", "PROJECT_RENAMED");
                return resp;
            }
        }
    }

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

    if !cloud.ratelimit.check(&ip, hive_core::now_ms()) {
        let ev = cloud.event(&region, &method, &host, &path, 429, "rate-limited", &ip);
        cloud.record(ev);
        let mut resp = (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED").into_response();
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-ratelimit", "exceeded");
        return resp;
    }

    let anycast = cloud.registry.anycast(Some(&region));

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
            let ev = cloud.event(&region, &method, &host, &path, 0, "bot-log", &label);
            cloud.record(ev);
        }
        hive_edge::bot::BotVerdict::Allow => {}
    }

    let cache_key = CdnCache::key(&host, &path_q);
    let protection = protection_for_host(&cloud, &host);
    if let Some(resp) = preview_gate(
        &cloud,
        &host,
        protection.as_ref(),
        &headers_vec,
        &region,
        &method,
        &path,
        &query,
    ) {
        cloud.cdn.invalidate(&cache_key);
        return resp;
    }

    let is_rsc = header(&headers_vec, "rsc").is_some();
    if method == "GET" && !is_rsc {
        match cloud.cdn.lookup(&cache_key) {
            Lookup::Hit(c) => {
                let ev = cloud.event(&region, &method, &host, &path, c.status, "cache-hit", "");
                cloud.record(ev);
                return cached_response(c, &region, CacheState::Hit);
            }
            Lookup::Stale(c) => {
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

    let req = with_path(req, &path, &query);
    let resp = next.run(req).await;
    let status = resp.status().as_u16();

    let protected = protection
        .as_ref()
        .is_some_and(|p| p.kind != Protection::None);
    let cacheable = method == "GET"
        && !is_rsc
        && status == 200
        && !protected
        && is_cacheable(resp.headers());
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

/// The ngrok ingress region code of a `*.<code>.ngrok.pizza` host (iad/sin/sfo/lax), `None` for the
/// legacy zone, localhost or any non-region host: detects a region-MISMATCHED request so a browser
/// navigation is redirected (307) to the app's home region. Not the vanity `region_code` (`iad1`).
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

/// Vercel-style `404: NOT_FOUND` / `DEPLOYMENT_NOT_FOUND` page; the id encodes the serving region
/// instance: `<region-code>::<rand>-<timestamp>-<hash>`.
fn deployment_not_found(region: &str) -> Response {
    let u = uuid::Uuid::new_v4().simple().to_string();
    let id = format!(
        "{}::{}-{}-{}",
        region_code(region),
        &u[0..5],
        hive_core::now_ms(),
        &u[8..20],
    );
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

/// Does ANY deployment record in the fleet (local or gossiped) or a project claim this host label?
/// `None` is unknown (the real DEPLOYMENT_NOT_FOUND); `Some` is known but unserved here (even a
/// Ready record we could not route to), a routing/gossip gap. The visitor-facing text never names
/// the hosting node; the event detail keeps it.
fn known_label_status(cloud: &Arc<CloudState>, sub: &str) -> Option<KnownLabel> {
    let label_hit = |d: &fluid_core::DeploymentInfo| {
        d.project == sub
            || [&d.alias, &d.commit_alias, &d.branch_alias, &d.id_alias]
                .iter()
                .any(|a| !a.is_empty() && a.split('.').next().unwrap_or(a) == sub)
    };
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

/// The honest counterpart to [`deployment_not_found`]: the platform KNOWS this project and has
/// nothing Ready to serve, so 503 rather than 404 (the name is real and the condition temporary).
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

/// `base` with a request target (`path[?query]`, the visitor's own) set as its PATH and QUERY.
///
/// The visitor controls this string, so it is never formatted into a URL (a leading `@`/`#`/`:`
/// would become userinfo, a fragment or a port) and never resolved as a URL REFERENCE (`Url::join`
/// treats a `//host/..` target as a network-path reference that replaces the authority, and an
/// ordinary `//assets/x.js` request, which sloppy asset-URL joins really do produce, would then
/// be refused or misrouted). `set_path` takes the whole string as a path on the unchanged
/// authority, so the destination cannot move; dot segments and percent-encoding are normalized
/// exactly as `Url::parse` does for the previous concatenated form. A path prefix on `base`
/// (a gateway served below a prefix) is kept.
fn with_request_target(mut base: reqwest::Url, path_q: &str) -> reqwest::Url {
    let (path, query) = match path_q.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_q, None),
    };
    let joined = format!("{}{}", base.path().trim_end_matches('/'), path);
    base.set_path(&joined);
    base.set_query(query);
    base
}

/// Background stale-while-revalidate refresh through our own gateway. Every early return is a
/// FAILED revalidation and must call `note_revalidation_failed`, keeping the stale entry and
/// engaging the 30 s retry backoff instead of letting the next request re-fire a refresh.
fn spawn_revalidate(cloud: Arc<CloudState>, host: String, path_q: String, key: String) {
    tokio::spawn(async move {
        // `path_q` is the original cached REQUEST's path+query (visitor-derived): set as the path
        // and query of `public_base` (`with_request_target`), the same construction as the
        // forward leg, never a raw `format!` a crafted path could redirect.
        let Some(target) = reqwest::Url::parse(&cloud.public_base)
            .ok()
            .map(|base| with_request_target(base, &path_q))
        else {
            cloud.cdn.note_revalidation_failed(&key);
            return;
        };
        let resp = match hive_outbound::same_origin_client()
            .get(target)
            .header("host", &host)
            .header("x-hive-revalidate", "1")
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(_) => {
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

/// The dashboard origin to bounce a protected-preview browser request to: over a PUBLIC wildcard
/// host `HIVE_PUBLIC_DASHBOARD_URL`, else the matched deploy suffix minus a leading `deployment.`
/// label (every node's `HIVE_DASHBOARD_URL` is the local-dev value); a loopback host uses
/// `HIVE_DASHBOARD_URL`.
fn dashboard_origin(cloud: &Arc<CloudState>, host: &str) -> Option<String> {
    derive_dashboard(
        host,
        &cloud.deploy_suffixes,
        &cloud.apps_domain,
        std::env::var("HIVE_PUBLIC_DASHBOARD_URL").ok(),
        std::env::var("HIVE_DASHBOARD_URL").ok(),
    )
}

/// Pure core of [`dashboard_origin`]: public wildcard host -> `public_env`, else the deploy suffix
/// (falling back to `apps_domain`, since `HIVE_DEPLOY_SUFFIXES` defaults to `localhost`) minus a
/// leading `deployment.` label as `https://...`; loopback -> `local_env`.
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

/// How the deployment serving a host is protected, decided ONCE per request by
/// `protection_for_host` and shared by the gate and the CDN cache. The cache key has no auth
/// dimension, so a protected body is never stored and a refusal evicts the key and answers
/// `no-store`; two paths disagreeing on "is this protected?" would put a protected body in a cache
/// any caller can read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Protection {
    /// Not protected — production by default, or protection explicitly off.
    None,
    /// Enterprise password mode: a shared password unlocks it, no team identity.
    Password,
    /// Membership — enterprise `standard` mode OR the legacy per-project preview
    /// toggle; a team credential unlocks it.
    Standard,
}

/// A host's `Protection` with the project that owns the deployment serving it.
struct HostProtection {
    project: String,
    kind: Protection,
}

/// `Protection` of the deployment serving `host`, or `None` when none resolves here. Its
/// environment is `DeploymentInfo::target`, which `fluid_gateway::view_of` normalizes so
/// `production` wins over a disagreeing stored `target`; an empty target is a legacy record and
/// falls back to the `production` flag.
fn protection_for_host(cloud: &Arc<CloudState>, host: &str) -> Option<HostProtection> {
    let dep = cloud.gw.deployment_for_host(host)?;
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
    let ent = cloud.enterprise.protection(&dep.project);
    let ent_protects = ent.protects(target);
    let kind = if ent_protects && ent.mode == "password" {
        Protection::Password
    } else if (ent_protects && ent.mode == "standard")
        || (is_preview && cloud.projects.preview_protected(&dep.project))
    {
        Protection::Standard
    } else {
        Protection::None
    };
    Some(HostProtection {
        project: dep.project.clone(),
        kind,
    })
}

/// Gates access to protected preview deployments: `Some(401)` for a team-private preview whose
/// request carries no valid access credential, `None` to proceed. Gated by the deployment's ACTUAL
/// environment, never the subdomain; enterprise protection (`password`/`standard`, scope
/// preview|all|production) is evaluated beside the legacy per-project toggle, and a password
/// unlocks via the sealed `hive_pw` cookie or the GET `__shadw_pw` form.
fn preview_gate(
    cloud: &Arc<CloudState>,
    host: &str,
    protection: Option<&HostProtection>,
    headers: &[(String, String)],
    region: &str,
    method: &str,
    path: &str,
    query: &str,
) -> Option<Response> {
    let protection = protection?;
    let project = protection.project.as_str();
    if protection.kind == Protection::None {
        return None;
    }
    if protection.kind == Protection::Password {
        if let Some(pw_cookie) = cookie_value(headers, "hive_pw") {
            if cloud
                .enterprise
                .verify_password_cookie(project, &pw_cookie)
            {
                return None;
            }
        }
        if let Some(submitted) = query_param(query, "__shadw_pw") {
            if cloud.enterprise.verify_password(project, &submitted) {
                let cookie = cloud.enterprise.seal_password_cookie(project, &submitted);
                // `path` is the visitor's own request path: a `//host/..` or `/\host` one would
                // make this Location protocol-relative to another host. Only a same-origin path
                // is a safe redirect target (anything else lands on the site root).
                let mut resp =
                    axum::response::Redirect::temporary(crate::auth::same_origin_path(path, "/"))
                        .into_response();
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
                    project,
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
                project,
            );
            cloud.record(ev);
            return Some(password_page(project, region, true));
        }
        let ev = cloud.event(region, method, host, path, 401, "deploy-password", project);
        cloud.record(ev);
        return Some(password_page(project, region, false));
    }

    let team = cloud.projects.team_of(project);
    if has_preview_access(headers, &team) {
        return None;
    }
    #[cfg(feature = "zkauth")]
    if crate::zkauth::cookie_access(project, headers) {
        return None;
    }
    let ev = cloud.event(
        region,
        method,
        host,
        path,
        401,
        "preview-protected",
        project,
    );
    cloud.record(ev);

    let wants_html = header(headers, "accept")
        .map(|a| a.contains("text/html"))
        .unwrap_or(false);
    if wants_html && method.eq_ignore_ascii_case("GET") {
        if let Some(dash) = dashboard_origin(cloud, host) {
            {
                let url = format!(
                    "{dash}/preview-unlock?host={}&project={}&team={}&next={}",
                    pct(host),
                    pct(project),
                    pct(&team),
                    pct(path),
                );
                let mut resp = axum::response::Redirect::temporary(&url).into_response();
                set(&mut resp, "x-hive-region", region);
                set(&mut resp, "x-hive-preview", "unlock-redirect");
                set(&mut resp, "cache-control", "no-store");
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
    set(&mut resp, "cache-control", "no-store");
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

/// Escapes text for an HTML text node or a quoted attribute value.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Vercel-style deployment-password prompt: a self-contained page that GET-submits `__shadw_pw`
/// back to the document's own URL (the gate seals a cookie and redirects); `bad` shows the
/// incorrect-password state.
///
/// The form deliberately has NO `action`: an absent action submits to the current URL, so the
/// visitor's own request path (attacker-controlled: a `//evil.example/..` path or one with a `"` in
/// it, which the request-line parser tolerates) is never interpolated into the page — as an
/// `action="{path}"` it sent the typed password to another host and broke out of the attribute.
fn password_page(project: &str, region: &str, bad: bool) -> Response {
    let err = if bad {
        "<p style=\"color:#f87171;font-size:13px;margin:0 0 12px\">Incorrect password. Try again.</p>"
    } else {
        ""
    };
    let project = html_escape(project);
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
<form method="GET">
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
    if let Some(auth) = header(headers, "authorization") {
        if let Some(tok) = auth.strip_prefix("Bearer ") {
            if let Ok(claims) = crate::auth::verify(tok) {
                if claims.tenant == team || claims.role == "owner" {
                    return true;
                }
            }
        }
    }
    if let Some(cookie) = header(headers, "cookie") {
        for part in cookie.split(';') {
            let kv = part.trim();
            if let Some(val) = kv.strip_prefix("hive_access=") {
                if crate::auth::enforced() {
                    if let Ok(claims) = crate::auth::verify(val) {
                        return claims.tenant == team || claims.role == "owner";
                    }
                } else {
                    return !val.is_empty();
                }
            }
        }
    }
    false
}

/// O(1) single-header read for the EARLY lookups that run before any proxy consumes `req`;
/// aggregate consumers use `headers_vec`.
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

/// Per-candidate iroh cooldown (`HIVE_EDGE_IROH_COOLDOWN_MS`, default 120 s, `0` disables): a
/// forward over a peer's iroh trunk with no first byte (`PostSendTimeout`) or no connection
/// (`DeadPeerTimeout`) that then succeeded over HTTP would pay again on the NEXT request, since the
/// peer still gossips healthy. While a candidate cools its HTTP gateway goes first; once the window
/// expires exactly ONE request claims the probe slot and tries iroh first, so a recovered trunk is
/// picked up without a pile-in.
fn iroh_cooldown() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>
{
    static COOLING: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    COOLING.get_or_init(Default::default)
}

fn iroh_cooldown_window() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("HIVE_EDGE_IROH_COOLDOWN_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(120_000),
    )
}

/// True while iroh should be tried SECOND for `node`. Expired entries hand the
/// caller the probe slot (see [`iroh_cooldown`]) and return false.
fn iroh_cooling(node: &str) -> bool {
    let window = iroh_cooldown_window();
    if window.is_zero() {
        return false;
    }
    let now = std::time::Instant::now();
    let mut map = iroh_cooldown().lock().unwrap_or_else(|e| e.into_inner());
    match map.get(node) {
        None => false,
        Some(until) if *until > now => true,
        Some(_) => {
            map.insert(node.to_string(), now + window);
            false
        }
    }
}

fn iroh_mark_bad(node: &str) {
    let window = iroh_cooldown_window();
    if window.is_zero() {
        return;
    }
    let mut map = iroh_cooldown().lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > 1024 {
        map.clear();
    }
    map.insert(node.to_string(), std::time::Instant::now() + window);
}

fn iroh_mark_ok(node: &str) {
    iroh_cooldown()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(node);
}

/// A gateway URL another node can reach: Macs and laptop dev nodes advertise their bind address
/// (`http://0.0.0.0:8787`, `http://127.0.0.1:...`), never the first choice.
fn usable_gateway(gateway: &str) -> bool {
    let g = gateway.trim();
    !g.is_empty()
        && !["://127.", "://0.0.0.0", "://localhost", "://[::"]
            .iter()
            .any(|bad| g.contains(bad))
}

/// HTTP-fallback timeout for one candidate: iroh and the fallback share ONE per-candidate deadline
/// (`HIVE_EDGE_CANDIDATE_MS`, default 30 s), the fallback getting what iroh left, floored at 1 s.
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

/// Orders mesh candidates by the deployment's region/failover policy: no `regions` is
/// same-region-first anycast with latency as tiebreak, `failover: true` keeps only configured
/// regions (primary first), `failover: false` restricts to the PRIMARY region and never crosses it
/// (an empty result makes the caller 503). Only `healthy` candidates are passed in.
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

/// Load-balances across the BEST region tier: rotates the equal-best prefix by a global round-robin
/// counter so same-region peers share traffic; failover tiers keep their order.
fn balance_same_region(
    mut raw: Vec<crate::state::PeerRoute>,
    is_cold: &impl Fn(&str) -> bool,
) -> Vec<crate::state::PeerRoute> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RR: AtomicUsize = AtomicUsize::new(0);
    /// Co-located peers are within this latency band of each other (30 ms covers intra-region
    /// jitter without spanning regions); a slower same-region node is not rotated into the front.
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

/// Cross-node WebSocket proxy for every candidate OTHER than this node: opens a RAW stream to the
/// owner over its iroh trunk, replays the upgrade request (the owner's local target performs the
/// handshake), relays the response head back, then splices raw bytes both ways. `open_raw` runs
/// BEFORE the client upgrade is consumed, so a failed candidate can be retried. Never dialed
/// against `cloud.node_name` ([`local_ws_proxy`] handles it): the owner would re-enter this
/// pipeline.
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
        if e.downcast_ref::<hive_p2p::DeadPeerTimeout>().is_some() {
            crate::health::demote(registry, node_id, "ws_proxy: connect/open timeout", None);
        }
    })?;
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

/// LOCAL counterpart to [`ws_proxy`] when the winning candidate is THIS node: leases the target
/// function (`Gateway::lease_for_path`), opens a fresh connection to the instance's tunnel listener
/// prefixed with [`fluid_tunnel::TunnelServer::RAW_UPGRADE_MAGIC`] (switching that ONE connection
/// to a raw splice), replays the upgrade and splices, avoiding a mesh self-dial. The lease rides as
/// the spliced task's guard, so inflight accounting covers the whole session.
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

/// Reads an HTTP/1.x response head from a raw stream; returns the status, header pairs and any
/// bytes already read past the head (the start of the spliced body or WS frames).
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
