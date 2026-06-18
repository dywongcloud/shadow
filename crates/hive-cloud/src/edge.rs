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
    response::{IntoResponse, Response},
};
use hive_edge::{
    cdn::{CacheState, CdnCache, Lookup},
    routing::RouteOutcome,
    waf::{RequestCtx, Verdict},
};

use crate::state::CloudState;

fn set(resp: &mut Response, name: &'static str, val: &str) {
    if let Ok(v) = HeaderValue::from_str(val) {
        resp.headers_mut().insert(HeaderName::from_static(name), v);
    }
}

pub async fn edge_pipeline(
    State(cloud): State<Arc<CloudState>>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().as_str().to_string();
    let mut path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let headers_vec: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
        .collect();
    let host = header(&headers_vec, "host").unwrap_or_default();
    let ua = header(&headers_vec, "user-agent").unwrap_or_default();
    let ip = header(&headers_vec, "x-forwarded-for")
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let region = cloud.region.clone();

    // Vercel Web Analytics / Speed Insights beacons are infrastructure endpoints
    // served by the gateway itself — let them pass straight through, bypassing the
    // WAF / rate-limit / preview gates, so any deployed app using @vercel/analytics
    // or @vercel/speed-insights works regardless of the deployment's protection.
    if path.starts_with("/_vercel/") {
        return next.run(req).await;
    }

    // ---- Cross-node mesh routing ------------------------------------------------
    // If this node doesn't host the requested deployment but a peer in the mesh
    // does, reverse-proxy the request to the best peer: same-region first, then
    // lowest latency (anycast), failing over to the next peer on a connection
    // error. `x-hive-proxied` breaks loops. This is what turns N nodes into one
    // cloud — hit any node, the request reaches wherever the deployment lives.
    let already_proxied = header(&headers_vec, "x-hive-proxied").is_some();
    if !already_proxied && !host.is_empty() && !cloud.gw.serves_host(&host) {
        let sub = host
            .split(':').next().unwrap_or(&host)
            .split('.').next().unwrap_or(&host)
            .to_string();
        let mut cands: Vec<crate::state::PeerRoute> = cloud
            .peer_routes
            .read()
            .get(&sub)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.healthy)
            .collect();
        if !cands.is_empty() {
            cands.sort_by_key(|r| (if r.region == region { 0u8 } else { 1u8 }, r.latency_ms));
            let path_q = if query.is_empty() { path.clone() } else { format!("{path}?{query}") };
            let rmethod = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
            let (_parts, body) = req.into_parts();
            let body_bytes = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap_or_default();

            // For the iroh transport: this node's endpoint + a map of peer node id
            // -> its gossiped iroh dial address. Headers forwarded over the tunnel.
            let iroh_self = cloud.iroh.read().clone();
            let node_iroh: std::collections::HashMap<String, String> = cloud
                .registry
                .nodes()
                .into_iter()
                .filter_map(|n| n.iroh_addr.map(|a| (n.id, a)))
                .collect();
            let mut fwd_headers: Vec<(String, String)> =
                vec![("host".into(), host.clone()), ("x-hive-proxied".into(), "1".into())];
            for (k, v) in &headers_vec {
                let lk = k.to_lowercase();
                if matches!(lk.as_str(), "host" | "connection" | "content-length" | "x-hive-proxied") {
                    continue;
                }
                fwd_headers.push((k.clone(), v.clone()));
            }

            for cand in &cands {
                // Prefer the real P2P (iroh QUIC) tunnel when both nodes have it —
                // works across NATs. Fall through to HTTP on any failure.
                if let (Some(ep), Some(addr_json)) = (&iroh_self, node_iroh.get(&cand.node_id)) {
                    match hive_p2p::dial_request(ep, addr_json, &method, &path_q, fwd_headers.clone(), &body_bytes).await {
                        Ok(tr) => {
                            let mut builder = Response::builder().status(tr.status);
                            for (k, v) in &tr.headers {
                                let lk = k.to_lowercase();
                                if matches!(lk.as_str(), "transfer-encoding" | "connection" | "content-length") {
                                    continue;
                                }
                                if let (Ok(name), Ok(val)) =
                                    (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_bytes(v.as_bytes()))
                                {
                                    builder = builder.header(name, val);
                                }
                            }
                            let mut out = builder
                                .body(Body::from(tr.body))
                                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
                            set(&mut out, "x-hive-routed-to", &cand.node_id);
                            set(&mut out, "x-hive-transport", "iroh-p2p");
                            set(&mut out, "x-hive-region", &region);
                            let ev = cloud.event(&region, &method, &host, &path, tr.status, "mesh-route-p2p", &cand.node_id);
                            cloud.record(ev);
                            return out;
                        }
                        Err(_) => { /* iroh failed → try HTTP for this candidate */ }
                    }
                }
                let url = format!("{}{}", cand.gateway.trim_end_matches('/'), path_q);
                let mut rb = cloud
                    .http
                    .request(rmethod.clone(), &url)
                    .header("host", &host)
                    .header("x-hive-proxied", "1")
                    .timeout(std::time::Duration::from_secs(30))
                    .body(body_bytes.clone());
                for (k, v) in &headers_vec {
                    let lk = k.to_lowercase();
                    if matches!(lk.as_str(), "host" | "connection" | "content-length" | "x-hive-proxied") {
                        continue;
                    }
                    rb = rb.header(k, v);
                }
                match rb.send().await {
                    Ok(r) => {
                        let status = r.status();
                        let rheaders = r.headers().clone();
                        let bytes = r.bytes().await.unwrap_or_default();
                        let mut builder = Response::builder().status(status.as_u16());
                        for (k, v) in rheaders.iter() {
                            let lk = k.as_str().to_lowercase();
                            // Skip hop-by-hop headers — the body length is re-derived.
                            if matches!(lk.as_str(), "transfer-encoding" | "connection" | "content-length") {
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
                            .body(Body::from(bytes))
                            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
                        set(&mut out, "x-hive-routed-to", &cand.node_id);
                        set(&mut out, "x-hive-region", &region);
                        let ev = cloud.event(&region, &method, &host, &path, status.as_u16(), "mesh-route", &cand.node_id);
                        cloud.record(ev);
                        return out;
                    }
                    Err(_) => continue, // peer down → fail over to the next candidate
                }
            }
            // Every candidate peer failed.
            let ev = cloud.event(&region, &method, &host, &path, 502, "mesh-route-fail", &sub);
            cloud.record(ev);
            let mut resp = (StatusCode::BAD_GATEWAY, "no healthy node could serve this deployment").into_response();
            set(&mut resp, "x-hive-region", &region);
            return resp;
        }
    }

    // -1) L7 DDoS mitigation: shed per-IP floods before any compute work.
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
            let ev = cloud.event(&region, &method, &host, &path, status, "redirect", &location);
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
    let path_q = if query.is_empty() { path.clone() } else { format!("{path}?{query}") };

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
        let ev = cloud.event(&region, &method, &host, &path, 403, "waf-deny", &format!("{rule_id}: {reason}"));
        cloud.record(ev);
        let mut resp = (StatusCode::FORBIDDEN, format!("blocked by WAF ({rule_id})")).into_response();
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
    if let Some(resp) = preview_gate(&cloud, &host, &headers_vec, &region, &method, &path) {
        return resp;
    }

    // 3) CDN cache (GET) — serve HIT directly, or STALE while revalidating.
    let cache_key = CdnCache::key(&host, &path_q);
    if method == "GET" {
        match cloud.cdn.lookup(&cache_key) {
            Lookup::Hit(c) => {
                let ev = cloud.event(&region, &method, &host, &path, c.status, "cache-hit", "");
                cloud.record(ev);
                return cached_response(c, &region, CacheState::Hit);
            }
            Lookup::Stale(c) => {
                // stale-while-revalidate: serve stale now, refresh in background.
                spawn_revalidate(cloud.clone(), host.clone(), path_q.clone(), cache_key.clone());
                let ev = cloud.event(&region, &method, &host, &path, c.status, "cache-stale", "swr");
                cloud.record(ev);
                return cached_response(c, &region, CacheState::Stale);
            }
            Lookup::Miss => {}
        }
    }

    // 4) Concurrency admission (per-region burst limit) for compute requests.
    if !cloud.limiter.try_admit() {
        let ev = cloud.event(&region, &method, &host, &path, 503, "throttled", "FUNCTION_THROTTLED");
        cloud.record(ev);
        let mut resp = (StatusCode::SERVICE_UNAVAILABLE, "FUNCTION_THROTTLED").into_response();
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-error", "FUNCTION_THROTTLED");
        return resp;
    }

    // 5) Compute: route through the gateway (rewrite path back onto the request).
    let req = with_path(req, &path, &query);
    let resp = next.run(req).await;
    let status = resp.status().as_u16();

    let cacheable = method == "GET" && status == 200 && is_cacheable(resp.headers());
    let action = if cacheable { "cache-store" } else { "allow" };
    let ev = cloud.event(&region, &method, &host, &path, status, action, "");
    cloud.record(ev);

    if cacheable {
        let (parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap_or_else(|_| Bytes::new());
        let hdrs: Vec<(String, String)> = parts
            .headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
            .collect();
        cloud.cdn.maybe_store(&cache_key, status, &hdrs, &bytes);
        let mut resp = Response::from_parts(parts, Body::from(bytes));
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-cache", CacheState::Miss.header());
        set_anycast(&mut resp, &anycast);
        resp
    } else {
        let mut resp = resp;
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-cache", CacheState::Miss.header());
        set_anycast(&mut resp, &anycast);
        resp
    }
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
    let mut resp = Response::builder().status(c.status).body(Body::from(c.body)).unwrap();
    for (k, v) in &c.headers {
        if let (Ok(n), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            resp.headers_mut().insert(n, val);
        }
    }
    set(&mut resp, "x-hive-region", region);
    set(&mut resp, "x-hive-cache", state.header());
    resp
}

/// Background stale-while-revalidate refresh: re-fetch through our own gateway
/// and update the cache entry (next lookup becomes a fresh HIT / REVALIDATED).
fn spawn_revalidate(cloud: Arc<CloudState>, host: String, path_q: String, key: String) {
    tokio::spawn(async move {
        // Subdomain host -> gateway. Use the internal gateway via public_base.
        let url = format!("{}{}", cloud.public_base, path_q);
        if let Ok(resp) = cloud
            .http
            .get(url)
            .header("host", &host)
            .header("x-hive-revalidate", "1")
            .send()
            .await
        {
            let status = resp.status().as_u16();
            let hdrs: Vec<(String, String)> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
                .collect();
            if let Ok(body) = resp.bytes().await {
                if cloud.cdn.maybe_store(&key, status, &hdrs, &body) {
                    cloud.cdn.note_revalidated();
                    let ev = cloud.event(&cloud.region, "GET", &host, &path_q, status, "cache-revalidate", "");
                    cloud.record(ev);
                }
            }
        }
    });
}

fn is_cacheable(headers: &axum::http::HeaderMap) -> bool {
    let hdrs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
        .collect();
    hive_edge::cdn::cache_policy(&hdrs).is_some()
}

/// Rebuild the request URI with the (possibly rewritten) path, preserving query.
fn with_path(req: Request, path: &str, query: &str) -> Request {
    let (mut parts, body) = req.into_parts();
    let pq = if query.is_empty() { path.to_string() } else { format!("{path}?{query}") };
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
) -> Option<Response> {
    let subdomain = host.split(':').next().unwrap_or(host).split('.').next().unwrap_or("");
    let project = cloud.gw.project_for_host(host)?;
    // Production alias is `<project>.localhost`; anything else is a preview.
    let is_preview = !subdomain.eq_ignore_ascii_case(&project);
    if !is_preview || !cloud.projects.preview_protected(&project) {
        return None;
    }
    let team = cloud.projects.team_of(&project);
    if has_preview_access(headers, &team) {
        return None;
    }
    let ev = cloud.event(region, method, host, path, 401, "preview-protected", &project);
    cloud.record(ev);
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        format!("This preview deployment is private to the \"{team}\" team. Sign in to view it."),
    )
        .into_response();
    set(&mut resp, "x-hive-region", region);
    set(&mut resp, "x-hive-preview", "protected");
    Some(resp)
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

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}
