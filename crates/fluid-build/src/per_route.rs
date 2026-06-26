//! Per-route build-artifact discovery for Next.js — the foundation of Vercel-style
//! per-route function splitting (one bundle per route, traced deps only) instead
//! of one `next start` server for the whole app.
//!
//! This module is PURE DISCOVERY + CLASSIFICATION: it reads the manifests Next.js
//! emits into `.next` and produces a [`PerRouteManifest`] mapping each route to a
//! kind, runtime, entrypoint, and the file-trace it would bundle. It never
//! executes anything and never touches the live serve path — the runtime
//! dispatcher consumes this manifest separately and falls back to `next start`.
//!
//! Manifests read (all optional; absent ones are skipped — version-tolerant):
//!   * `routes-manifest.json`         — Pages static/dynamic routes, headers/redirects
//!   * `pages-manifest.json`          — Pages Router route -> server file
//!   * `app-paths-manifest.json`      — App Router route -> server file
//!   * `middleware-manifest.json`     — middleware + edge functions (kept on fallback)
//!   * `prerender-manifest.json`      — SSG/ISR routes + revalidate
//!   * `required-server-files.json`   — base files every server bundle needs
//!   * `<server-file>.nft.json`       — per-route file traces (only these get bundled)

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// How a route executes — drives which bundle/runtime serves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    /// Fully prerendered HTML/asset — served from the CDN, no function.
    Static,
    /// Incremental Static Regeneration — cached + background revalidate.
    Isr,
    /// Pages Router `/api/*` (Node).
    ApiNode,
    /// App Router route handler (`route.ts` → GET/POST/…), Node runtime.
    RouteHandler,
    /// Server-rendered page (App or Pages Router), Node runtime.
    SsrPage,
    /// Next.js Middleware — Edge runtime. Kept on the `next start` fallback.
    Middleware,
    /// Route declared `runtime = "edge"` — Edge runtime. Kept on fallback.
    EdgeRoute,
}

impl RouteKind {
    /// Whether the per-route Node dispatcher can serve this kind. Edge/middleware
    /// stay on the fallback until a real V8 isolate runtime exists.
    pub fn per_route_eligible(self) -> bool {
        matches!(self, RouteKind::ApiNode | RouteKind::RouteHandler | RouteKind::SsrPage)
    }
    pub fn runtime(self) -> &'static str {
        match self {
            RouteKind::Middleware | RouteKind::EdgeRoute => "edge",
            RouteKind::Static | RouteKind::Isr => "static",
            _ => "nodejs",
        }
    }

    /// The runtime-facing class (#16). The canonical cache/retry/concurrency
    /// policy lives on [`fluid_core::RouteClass`] (the shared crate the serve path
    /// can reach); this maps build-time discovery onto it. `hive-cloud` does the
    /// actual mapping when it persists the per-route manifest into the deployment
    /// `Manifest` (it depends on both crates; `fluid-build` does not depend on
    /// `fluid-core`, so the string is the contract).
    pub fn class_name(self) -> &'static str {
        match self {
            RouteKind::Static => "static",
            RouteKind::Isr => "isr",
            RouteKind::ApiNode => "api_node",
            RouteKind::RouteHandler => "route_handler",
            RouteKind::SsrPage => "ssr_page",
            RouteKind::Middleware => "middleware",
            RouteKind::EdgeRoute => "edge",
        }
    }
}

/// One route's materialization plan.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteBundle {
    /// URL route pattern, e.g. `/api/claw` or `/blog/[slug]`.
    pub route: String,
    pub kind: RouteKind,
    pub runtime: String,
    /// Server entrypoint relative to `.next` (e.g. `server/app/api/claw/route.js`).
    /// None for static/ISR routes (served as assets).
    pub entrypoint: Option<String>,
    /// Files (relative to the traced file's dir) this route's bundle must include,
    /// from the route's `.nft.json` trace. Empty when no trace is available.
    pub traced_files: Vec<String>,
    /// ISR revalidate seconds, when applicable.
    pub revalidate: Option<i64>,
    /// True when this route must use the `next start` fallback (edge/middleware,
    /// or no usable entrypoint/trace).
    pub fallback: bool,
}

/// The whole-app per-route plan.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PerRouteManifest {
    /// Detected Next.js major version from a manifest `version` field (best-effort).
    pub next_version: Option<i64>,
    /// Base files every Node server bundle needs (from required-server-files.json).
    pub required_files: Vec<String>,
    pub routes: Vec<RouteBundle>,
}

impl PerRouteManifest {
    pub fn eligible_count(&self) -> usize {
        self.routes.iter().filter(|r| r.kind.per_route_eligible() && !r.fallback).count()
    }
    pub fn fallback_count(&self) -> usize {
        self.routes.iter().filter(|r| r.fallback || !r.kind.per_route_eligible()).count()
    }
}

/// Read a `.next` manifest by name, tolerant of version differences in WHERE Next
/// puts it: newer Next (13–16) writes `app-paths-manifest.json`,
/// `pages-manifest.json`, and `middleware-manifest.json` under `.next/server/`,
/// while `routes-manifest.json`, `prerender-manifest.json`, and
/// `required-server-files.json` sit at `.next/`. Try both so we work across
/// versions; a missing file is simply None.
fn read_json(dir: &Path, name: &str) -> Option<serde_json::Value> {
    for cand in [dir.join(name), dir.join("server").join(name)] {
        if let Ok(txt) = std::fs::read_to_string(&cand) {
            if let Ok(v) = serde_json::from_str(&txt) {
                return Some(v);
            }
        }
    }
    None
}

/// Read a `<server_file>.nft.json` trace next to a server file. Returns the file
/// list (relative to the trace file's directory, as Next emits them).
fn read_trace(next_dir: &Path, server_file_rel: &str) -> Vec<String> {
    let nft = next_dir.join(format!("{server_file_rel}.nft.json"));
    let Some(v) = std::fs::read_to_string(&nft).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else {
        return Vec::new();
    };
    v.get("files")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Discover + classify every route in a built `.next` directory.
pub fn discover(next_dir: &Path) -> PerRouteManifest {
    let mut m = PerRouteManifest::default();
    if !next_dir.exists() {
        return m;
    }

    // ISR/SSG routes (route -> revalidate) from prerender-manifest.
    let mut prerender: BTreeMap<String, Option<i64>> = BTreeMap::new();
    if let Some(pr) = read_json(next_dir, "prerender-manifest.json") {
        m.next_version = pr.get("version").and_then(|v| v.as_i64()).or(m.next_version);
        if let Some(routes) = pr.get("routes").and_then(|r| r.as_object()) {
            for (route, meta) in routes {
                let rev = meta.get("initialRevalidateSeconds").and_then(|v| v.as_i64());
                prerender.insert(route.clone(), rev);
            }
        }
    }

    // Edge functions + middleware (kept on fallback).
    let mut edge_routes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut has_middleware = false;
    if let Some(mw) = read_json(next_dir, "middleware-manifest.json") {
        if let Some(fns) = mw.get("functions").and_then(|f| f.as_object()) {
            for (route, _) in fns {
                // Normalize manifest keys ("/api/geo/route", "/x/page") to URL form.
                let u = route.trim_end_matches("/page").trim_end_matches("/route");
                edge_routes.insert(if u.is_empty() { "/".into() } else { u.to_string() });
            }
        }
        if mw.get("middleware").and_then(|x| x.as_object()).map(|o| !o.is_empty()).unwrap_or(false) {
            has_middleware = true;
        }
    }

    // required-server-files: base files for any Node bundle.
    if let Some(rsf) = read_json(next_dir, "required-server-files.json") {
        m.next_version = rsf.get("version").and_then(|v| v.as_i64()).or(m.next_version);
        if let Some(files) = rsf.get("files").and_then(|f| f.as_array()) {
            m.required_files = files.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
    }

    let classify = |route: &str, server_file: &str| -> RouteBundle {
        let is_edge = edge_routes.contains(route);
        let rev = prerender.get(route).copied().flatten();
        let is_prerendered = prerender.contains_key(route);
        let kind = if is_edge {
            RouteKind::EdgeRoute
        } else if route.starts_with("/api/") || route == "/api" {
            RouteKind::ApiNode
        } else if server_file.ends_with("route.js") {
            RouteKind::RouteHandler
        } else if rev.is_some() {
            RouteKind::Isr
        } else if is_prerendered {
            RouteKind::Static
        } else {
            RouteKind::SsrPage
        };
        let traced = if kind.per_route_eligible() { read_trace(next_dir, server_file) } else { Vec::new() };
        let fallback = !kind.per_route_eligible() || (kind != RouteKind::Static && server_file.is_empty());
        RouteBundle {
            route: route.to_string(),
            kind,
            runtime: kind.runtime().to_string(),
            entrypoint: (!server_file.is_empty()).then(|| server_file.to_string()),
            traced_files: traced,
            revalidate: rev,
            fallback,
        }
    };

    // App Router: app-paths-manifest maps route -> server file (relative to .next).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(app) = read_json(next_dir, "app-paths-manifest.json").and_then(|v| v.as_object().cloned()) {
        for (route, file) in app {
            let sf = file.as_str().unwrap_or("");
            // App Router server files live under server/. Normalize route key
            // ("/page" suffix → its URL path).
            let url = route.trim_end_matches("/page").trim_end_matches("/route");
            let url = if url.is_empty() { "/" } else { url };
            if seen.insert(url.to_string()) {
                m.routes.push(classify(url, &format!("server/{}", sf.trim_start_matches('/'))));
            }
        }
    }

    // Pages Router: pages-manifest maps route -> server file.
    if let Some(pages) = read_json(next_dir, "pages-manifest.json").and_then(|v| v.as_object().cloned()) {
        for (route, file) in pages {
            // Skip Next internals.
            if route.starts_with("/_") && route != "/_error" {
                continue;
            }
            if seen.insert(route.clone()) {
                let sf = file.as_str().unwrap_or("");
                m.routes.push(classify(&route, &format!("server/{}", sf.trim_start_matches('/'))));
            }
        }
    }

    // Record middleware as a (fallback) route so the dispatcher knows to defer it.
    if has_middleware && seen.insert("__middleware__".into()) {
        m.routes.push(RouteBundle {
            route: "/_middleware".into(),
            kind: RouteKind::Middleware,
            runtime: "edge".into(),
            entrypoint: None,
            traced_files: Vec::new(),
            revalidate: None,
            fallback: true,
        });
    }

    m.routes.sort_by(|a, b| a.route.cmp(&b.route));
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, json: &str) {
        if let Some(p) = dir.join(name).parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(dir.join(name), json).unwrap();
    }

    fn fixture() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("nft-fixture-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        // App Router: an SSR page, a route handler, plus an API + an edge route.
        write(&base, "app-paths-manifest.json", r#"{
            "/page": "app/page.js",
            "/ui/page": "app/ui/page.js",
            "/api/claw/route": "app/api/claw/route.js",
            "/api/geo/route": "app/api/geo/route.js"
        }"#);
        write(&base, "prerender-manifest.json", r#"{
            "version": 4,
            "routes": { "/": { "initialRevalidateSeconds": false }, "/blog": { "initialRevalidateSeconds": 60 } }
        }"#);
        write(&base, "middleware-manifest.json", r#"{
            "middleware": { "/": { "name": "middleware" } },
            "functions": { "/api/geo/route": { "name": "app/api/geo/route" } }
        }"#);
        write(&base, "required-server-files.json", r#"{ "version": 1, "files": ["server/next.config.js", "server/required.js"] }"#);
        // nft trace for the SSR page + the route handler.
        write(&base, "server/app/ui/page.js.nft.json", r#"{ "version": 1, "files": ["../../chunks/x.js", "../../../node_modules/foo/index.js"] }"#);
        write(&base, "server/app/api/claw/route.js.nft.json", r#"{ "version": 1, "files": ["../../../chunks/y.js"] }"#);
        base
    }

    #[test]
    fn classifies_app_router_routes() {
        let m = discover(&fixture());
        let by = |r: &str| m.routes.iter().find(|x| x.route == r).cloned();
        assert_eq!(by("/ui").unwrap().kind, RouteKind::SsrPage, "/ui is an SSR page");
        assert_eq!(by("/api/claw").unwrap().kind, RouteKind::ApiNode, "/api/claw is a Node API");
        assert_eq!(by("/api/geo").unwrap().kind, RouteKind::EdgeRoute, "/api/geo is edge (in middleware-manifest functions)");
        // "/" is prerendered (revalidate false) → Static.
        assert_eq!(by("/").unwrap().kind, RouteKind::Static);
    }

    #[test]
    fn edge_and_middleware_fall_back() {
        let m = discover(&fixture());
        let geo = m.routes.iter().find(|x| x.route == "/api/geo").unwrap();
        assert!(geo.fallback, "edge route must fall back to next start");
        assert!(m.routes.iter().any(|r| r.kind == RouteKind::Middleware && r.fallback), "middleware present + fallback");
    }

    #[test]
    fn node_routes_carry_traces_and_entrypoint() {
        let m = discover(&fixture());
        let ui = m.routes.iter().find(|x| x.route == "/ui").unwrap();
        assert!(ui.kind.per_route_eligible() && !ui.fallback);
        assert_eq!(ui.entrypoint.as_deref(), Some("server/app/ui/page.js"));
        assert_eq!(ui.traced_files.len(), 2, "trace files loaded from .nft.json");
        let claw = m.routes.iter().find(|x| x.route == "/api/claw").unwrap();
        assert_eq!(claw.traced_files.len(), 1);
        assert_eq!(m.required_files.len(), 2, "required-server-files loaded");
        assert!(m.eligible_count() >= 2 && m.fallback_count() >= 2);
    }

    #[test]
    fn empty_when_no_next_dir() {
        let m = discover(Path::new("/nonexistent/.next"));
        assert!(m.routes.is_empty());
    }

    #[test]
    fn route_kind_class_names_are_stable() {
        // The class_name string is the cross-crate contract with fluid_core::RouteClass.
        assert_eq!(RouteKind::Static.class_name(), "static");
        assert_eq!(RouteKind::Isr.class_name(), "isr");
        assert_eq!(RouteKind::ApiNode.class_name(), "api_node");
        assert_eq!(RouteKind::RouteHandler.class_name(), "route_handler");
        assert_eq!(RouteKind::SsrPage.class_name(), "ssr_page");
        assert_eq!(RouteKind::Middleware.class_name(), "middleware");
        assert_eq!(RouteKind::EdgeRoute.class_name(), "edge");
    }

    #[test]
    fn finds_manifests_under_server_dir_next16_layout() {
        // Next 13–16 put app-paths/pages/middleware manifests under .next/server/.
        let base = std::env::temp_dir().join(format!("nft-srv-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        write(&base, "server/app-paths-manifest.json", r#"{ "/ui/page": "app/ui/page.js", "/api/x/route": "app/api/x/route.js" }"#);
        write(&base, "prerender-manifest.json", r#"{ "version": 4, "routes": {} }"#);
        write(&base, "server/app/ui/page.js.nft.json", r#"{ "files": ["../../chunks/a.js"] }"#);
        let m = discover(&base);
        assert!(m.routes.iter().any(|r| r.route == "/ui" && r.kind == RouteKind::SsrPage), "must find app route under server/");
        assert!(m.routes.iter().any(|r| r.route == "/api/x" && r.kind == RouteKind::ApiNode));
    }
}
