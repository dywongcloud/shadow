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
    mut req: Request,
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
    // Host resolution: HTTP/1.1 carries it in the `Host` header, but HTTP/2 (used
    // by our TLS listener after ALPN negotiates h2) carries it in the `:authority`
    // pseudo-header instead — exposed by hyper as the request URI's authority, with
    // no synthesized `Host` header. Fall back to that so h2 clients route correctly.
    let host = header(&headers_vec, "host")
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

    // EXPERIMENT (feature `zkauth`): anonymous preview-access bootstrap. Verifies
    // a membership proof and drops a short-lived access cookie, then redirects.
    #[cfg(feature = "zkauth")]
    if path.starts_with("/_shadw/zk") {
        return crate::zkauth::bootstrap(&cloud, &host, &query);
    }

    // ---- Cross-node mesh routing ------------------------------------------------
    // If this node doesn't host the requested deployment but a peer in the mesh
    // does, reverse-proxy the request to the best peer: same-region first, then
    // lowest latency (anycast), failing over to the next peer on a connection
    // error. `x-hive-proxied` breaks loops. This is what turns N nodes into one
    // cloud — hit any node, the request reaches wherever the deployment lives.
    let already_proxied = header(&headers_vec, "x-hive-proxied").is_some();
    let serve_local = cloud.gw.serves_host(&host);
    let sub = host
        .split(':').next().unwrap_or(&host)
        .split('.').next().unwrap_or(&host)
        .to_string();
    // CONTAINER single-owner enforcement: even if THIS node has the container
    // locally, only the lease owner may serve it — so route to the owner (prevents
    // split-brain double-running of a stateful container). Functions are unaffected.
    let container_owner: Option<String> = if serve_local && cloud.gw.is_container_host(&host) {
        match cloud.leases.owner_of(&sub) {
            Some(owner) if owner != cloud.node_name => Some(owner),
            _ => None,
        }
    } else {
        None
    };
    if !already_proxied && !host.is_empty() && (!serve_local || container_owner.is_some()) {
        let mut cands: Vec<crate::state::PeerRoute> = cloud
            .peer_routes
            .read()
            .get(&sub)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.healthy)
            .collect();
        // For container enforcement, route ONLY to the elected owner.
        if let Some(owner) = &container_owner {
            cands.retain(|c| &c.node_id == owner);
        }
        if !cands.is_empty() {
            cands.sort_by_key(|r| (if r.region == region { 0u8 } else { 1u8 }, r.latency_ms));
            let path_q = if query.is_empty() { path.clone() } else { format!("{path}?{query}") };
            let rmethod = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
            let (_parts, body) = req.into_parts();
            let body_bytes = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap_or_default();

            // For the iroh transport: the pooled mesh client (reuses one QUIC
            // connection per peer) + a map of peer node id -> its gossiped iroh dial
            // address. Headers forwarded over the tunnel.
            let mesh = cloud.mesh.read().clone();
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
                if let (Some(pool), Some(addr_json)) = (&mesh, node_iroh.get(&cand.node_id)) {
                    match pool.request(&cand.node_id, addr_json, &method, &path_q, &fwd_headers, &body_bytes).await {
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

    // No deployment is aliased for this host (exact match) and no peer in the mesh
    // serves it either → the deployment genuinely doesn't exist on the wildcard
    // domain. Render the Vercel-style DEPLOYMENT_NOT_FOUND page (region-aware id)
    // instead of silently falling back to the default deployment / preview gate.
    // (`_vercel/*` + `/_shadw/zk` were already handled above.)
    if !serve_local && !already_proxied && !host.is_empty() {
        let ev = cloud.event(&region, &method, &host, &path, 404, "deployment-not-found", &host);
        cloud.record(ev);
        return deployment_not_found(&region);
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

    let cacheable = method == "GET" && !is_rsc && status == 200 && is_cacheable(resp.headers());
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
    let slug: String = r.chars().filter(|c| c.is_ascii_alphanumeric()).take(3).collect();
    if slug.is_empty() { "dev1".into() } else { format!("{slug}1") }
}

/// Vercel-style `404: NOT_FOUND` / `DEPLOYMENT_NOT_FOUND` page. The id encodes the
/// serving region instance: `<region-code>::<rand>-<timestamp>-<hash>`.
fn deployment_not_found(region: &str) -> Response {
    let u = uuid::Uuid::new_v4().simple().to_string();
    let id = format!(
        "{}::{}-{}-{}",
        region_code(region),
        &u[0..5],
        hive_core::now_ms(),
        &u[8..20],
    );
    let docs = std::env::var("HIVE_DASHBOARD_URL")
        .ok()
        .map(|d| format!("{}/docs", d.trim_end_matches('/')))
        .unwrap_or_else(|| "/docs".into());
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
    // EXPERIMENT (feature `zkauth`): opt-in anonymous membership access. Honour a
    // valid ZK access cookie (dropped by the bootstrap after verifying a proof),
    // so a member can view a protected preview without revealing identity to this
    // (possibly peer) node. Additive — falls through to the 401 below if absent.
    #[cfg(feature = "zkauth")]
    if crate::zkauth::cookie_access(&project, headers) {
        return None;
    }
    let ev = cloud.event(region, method, host, path, 401, "preview-protected", &project);
    cloud.record(ev);

    // One-and-done unlock: for a BROWSER navigation (Accept: text/html) we bounce to
    // the dashboard's `/preview-unlock`, which (using the signed-in session) mints a
    // membership proof and redirects back through `/_shadw/zk` to drop the access
    // cookie ON THIS deployment domain — solving the cross-domain cookie problem.
    // Needs HIVE_DASHBOARD_URL (the dashboard origin); without it we fall back to the
    // plain 401 so nothing breaks.
    let wants_html = header(headers, "accept").map(|a| a.contains("text/html")).unwrap_or(false);
    if wants_html && method.eq_ignore_ascii_case("GET") {
        if let Ok(dash) = std::env::var("HIVE_DASHBOARD_URL") {
            let dash = dash.trim_end_matches('/');
            if !dash.is_empty() {
                let url = format!(
                    "{dash}/preview-unlock?host={}&project={}&team={}&next={}",
                    pct(host), pct(&project), pct(&team), pct(path),
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

/// Percent-encode a query-parameter value (RFC 3986 unreserved kept verbatim).
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
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

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}
