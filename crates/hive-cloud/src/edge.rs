//! The edge request pipeline, mirroring Vercel's CDN layering:
//! **routing (redirects/rewrites) -> firewall (WAF + bots) -> concurrency
//! admission -> CDN cache (HIT/STALE/MISS w/ SWR) -> compute (route)**.
//! Adds `x-hive-region` and `x-hive-cache`, and records events for the dashboard.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
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

    // 2) Bot management.
    let policy = *cloud.bot_policy.read();
    if let Some(reason) = cloud.bot.should_block(&ua, policy) {
        let ev = cloud.event(&region, &method, &host, &path, 403, "bot-block", &reason);
        cloud.record(ev);
        let mut resp = (StatusCode::FORBIDDEN, format!("blocked: {reason}")).into_response();
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-bot", &reason);
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
        resp
    } else {
        let mut resp = resp;
        set(&mut resp, "x-hive-region", &region);
        set(&mut resp, "x-hive-cache", CacheState::Miss.header());
        resp
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

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}
