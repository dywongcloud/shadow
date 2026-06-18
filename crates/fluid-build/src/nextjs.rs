//! Map a built Next.js app's manifests into platform features — redirects,
//! rewrites, headers, edge middleware (`middleware.ts` / `proxy.ts`), and the
//! serverless/edge function split.
//!
//! Plain `next build` writes:
//!   `.next/routes-manifest.json`            — redirects / rewrites / headers / dynamic routes
//!   `.next/server/middleware-manifest.json` — middleware + edge functions
//!   `.next/server/pages-manifest.json` / `app-paths-manifest.json` — pages
//!
//! When `vercel build` ran instead, `.vercel/output/config.json` carries the same
//! information in Build Output API form — [`from_build_output`] handles that.

use serde::Serialize;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
pub struct FeatureRedirect {
    pub source: String,
    pub destination: String,
    pub status: u16,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeatureRewrite {
    pub source: String,
    pub destination: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FeatureMiddleware {
    pub matcher: Vec<String>,
    pub runtime: String,
}

/// Everything we extracted from a framework build that the platform maps onto
/// the deployment (redirects/rewrites the gateway honors, plus surfaced counts).
#[derive(Clone, Debug, Default, Serialize)]
pub struct BuildFeatures {
    pub redirects: Vec<FeatureRedirect>,
    pub rewrites: Vec<FeatureRewrite>,
    pub middleware: Option<FeatureMiddleware>,
    /// Edge-runtime function routes (`export const runtime = "edge"`).
    pub edge_functions: Vec<String>,
    /// Node serverless function routes.
    pub serverless_functions: Vec<String>,
    /// App-Router parallel routes detected (`@slot` segments).
    pub parallel_routes: usize,
    pub pages: usize,
}

impl BuildFeatures {
    pub fn is_empty(&self) -> bool {
        self.redirects.is_empty()
            && self.rewrites.is_empty()
            && self.middleware.is_none()
            && self.edge_functions.is_empty()
            && self.serverless_functions.is_empty()
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let s = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

/// Detect and map Next.js build features from `<repo>/.next` (or, if present,
/// the Build Output API at `<repo>/.vercel/output`).
pub fn detect_features(repo: &Path) -> BuildFeatures {
    let mut f = BuildFeatures::default();

    // ---- routes-manifest.json: redirects / rewrites / headers ----
    if let Some(rm) = read_json(&repo.join(".next/routes-manifest.json")) {
        for r in rm.get("redirects").and_then(|v| v.as_array()).into_iter().flatten() {
            if let (Some(src), Some(dst)) = (r.get("source").and_then(|v| v.as_str()), r.get("destination").and_then(|v| v.as_str())) {
                let permanent = r.get("permanent").and_then(|v| v.as_bool()).unwrap_or(false);
                let status = r
                    .get("statusCode")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u16)
                    .unwrap_or(if permanent { 308 } else { 307 });
                f.redirects.push(FeatureRedirect { source: src.to_string(), destination: dst.to_string(), status });
            }
        }
        // rewrites is either an array or { beforeFiles, afterFiles, fallback }.
        let collect_rewrites = |arr: &serde_json::Value, out: &mut Vec<FeatureRewrite>| {
            for r in arr.as_array().into_iter().flatten() {
                if let (Some(src), Some(dst)) = (r.get("source").and_then(|v| v.as_str()), r.get("destination").and_then(|v| v.as_str())) {
                    out.push(FeatureRewrite { source: src.to_string(), destination: dst.to_string() });
                }
            }
        };
        if let Some(rw) = rm.get("rewrites") {
            if rw.is_array() {
                collect_rewrites(rw, &mut f.rewrites);
            } else {
                for k in ["beforeFiles", "afterFiles", "fallback"] {
                    if let Some(arr) = rw.get(k) {
                        collect_rewrites(arr, &mut f.rewrites);
                    }
                }
            }
        }
    }

    // ---- middleware-manifest.json: middleware + edge functions ----
    if let Some(mm) = read_json(&repo.join(".next/server/middleware-manifest.json")) {
        if let Some(mw) = mm.get("middleware").and_then(|v| v.as_object()) {
            // Usually keyed by "/"; collect matchers across entries.
            let mut matcher = Vec::new();
            for entry in mw.values() {
                for m in entry.get("matchers").and_then(|v| v.as_array()).into_iter().flatten() {
                    if let Some(rx) = m.get("originalSource").and_then(|v| v.as_str()).or_else(|| m.get("regexp").and_then(|v| v.as_str())) {
                        matcher.push(rx.to_string());
                    }
                }
            }
            if matcher.is_empty() {
                matcher.push("/:path*".into());
            }
            f.middleware = Some(FeatureMiddleware { matcher, runtime: "edge".into() });
        }
        if let Some(funcs) = mm.get("functions").and_then(|v| v.as_object()) {
            for name in funcs.keys() {
                f.edge_functions.push(name.clone());
            }
        }
    }

    // ---- app/pages function split + parallel routes ----
    if let Some(pm) = read_json(&repo.join(".next/server/pages-manifest.json")) {
        f.pages += pm.as_object().map(|o| o.len()).unwrap_or(0);
    }
    if let Some(am) = read_json(&repo.join(".next/server/app-paths-manifest.json")) {
        if let Some(o) = am.as_object() {
            f.pages += o.len();
            for route in o.keys() {
                if route.contains('@') {
                    f.parallel_routes += 1;
                }
                if !f.edge_functions.contains(route) {
                    f.serverless_functions.push(route.clone());
                }
            }
        }
    }

    // ---- Build Output API (vercel build) overrides/augments when present ----
    if let Some(extra) = from_build_output(repo) {
        if f.redirects.is_empty() {
            f.redirects = extra.redirects;
        }
        if f.rewrites.is_empty() {
            f.rewrites = extra.rewrites;
        }
        if f.middleware.is_none() {
            f.middleware = extra.middleware;
        }
        for e in extra.edge_functions {
            if !f.edge_functions.contains(&e) {
                f.edge_functions.push(e);
            }
        }
    }

    f
}

/// Map `.vercel/output/config.json` routes (Build Output API) into features.
pub fn from_build_output(repo: &Path) -> Option<BuildFeatures> {
    let cfg = read_json(&repo.join(".vercel/output/config.json"))?;
    let mut f = BuildFeatures::default();
    for route in cfg.get("routes").and_then(|v| v.as_array()).into_iter().flatten() {
        let src = route.get("src").and_then(|v| v.as_str());
        let dst = route.get("dest").and_then(|v| v.as_str());
        let status = route.get("status").and_then(|v| v.as_u64()).map(|n| n as u16);
        if let Some(mw) = route.get("middlewarePath").and_then(|v| v.as_str()) {
            let _ = mw;
            f.middleware.get_or_insert(FeatureMiddleware { matcher: vec!["/:path*".into()], runtime: "edge".into() });
        }
        match (src, dst, status) {
            (Some(s), Some(d), Some(code)) if (300..400).contains(&code) => {
                f.redirects.push(FeatureRedirect { source: s.to_string(), destination: d.to_string(), status: code });
            }
            (Some(s), Some(d), _) if s != d && route.get("handle").is_none() => {
                f.rewrites.push(FeatureRewrite { source: s.to_string(), destination: d.to_string() });
            }
            _ => {}
        }
    }
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_next_redirects_and_middleware() {
        let dir = std::env::temp_dir().join(format!("nx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".next/server")).unwrap();
        fs::write(
            dir.join(".next/routes-manifest.json"),
            r#"{"redirects":[{"source":"/old","destination":"/new","permanent":true}],
                "rewrites":{"beforeFiles":[{"source":"/a","destination":"/b"}],"afterFiles":[],"fallback":[]}}"#,
        )
        .unwrap();
        fs::write(
            dir.join(".next/server/middleware-manifest.json"),
            r#"{"middleware":{"/":{"matchers":[{"regexp":"^/dashboard"}]}},"functions":{}}"#,
        )
        .unwrap();
        let f = detect_features(&dir);
        assert_eq!(f.redirects.len(), 1);
        assert_eq!(f.redirects[0].status, 308);
        assert_eq!(f.rewrites.len(), 1);
        assert!(f.middleware.is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
