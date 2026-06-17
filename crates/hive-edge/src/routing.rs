//! Routing layer — redirects and rewrites evaluated before caching, per Vercel's
//! CDN routing layer (<https://vercel.com/docs/how-vercel-cdn-works>):
//!
//! 1. **Redirects** — respond immediately with a new Location (e.g. enforce
//!    HTTPS, move pages).
//! 2. **Rewrites** — map a public path to a different internal path without the
//!    client seeing it; processing continues with the rewritten path.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Redirect {
    /// Exact path or prefix (ending in `/`) to match.
    pub source: String,
    pub destination: String,
    #[serde(default = "default_308")]
    pub status: u16,
}
fn default_308() -> u16 {
    308
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rewrite {
    pub source: String,
    pub destination: String,
}

#[derive(Clone, Debug)]
pub enum RouteOutcome {
    /// Respond now with a redirect.
    Redirect { location: String, status: u16 },
    /// Continue with this (possibly rewritten) path.
    Continue(String),
}

pub struct Router {
    redirects: RwLock<Vec<Redirect>>,
    rewrites: RwLock<Vec<Rewrite>>,
}

impl Router {
    pub fn new() -> Arc<Router> {
        Arc::new(Router {
            redirects: RwLock::new(Vec::new()),
            rewrites: RwLock::new(Vec::new()),
        })
    }

    pub fn add_redirect(&self, r: Redirect) {
        self.redirects.write().push(r);
    }
    pub fn add_rewrite(&self, r: Rewrite) {
        self.rewrites.write().push(r);
    }
    pub fn redirects(&self) -> Vec<Redirect> {
        self.redirects.read().clone()
    }
    pub fn rewrites(&self) -> Vec<Rewrite> {
        self.rewrites.read().clone()
    }
    pub fn set_redirects(&self, v: Vec<Redirect>) {
        *self.redirects.write() = v;
    }
    pub fn set_rewrites(&self, v: Vec<Rewrite>) {
        *self.rewrites.write() = v;
    }

    /// Evaluate redirects (first match wins, immediate), then rewrites
    /// (first match wins, path rewritten), returning the outcome.
    pub fn evaluate(&self, path: &str) -> RouteOutcome {
        for r in self.redirects.read().iter() {
            if path_match(&r.source, path) {
                return RouteOutcome::Redirect {
                    location: rewrite_target(&r.source, &r.destination, path),
                    status: r.status,
                };
            }
        }
        for r in self.rewrites.read().iter() {
            if path_match(&r.source, path) {
                return RouteOutcome::Continue(rewrite_target(&r.source, &r.destination, path));
            }
        }
        RouteOutcome::Continue(path.to_string())
    }
}

impl Default for Router {
    fn default() -> Self {
        // Note: returns inner value, not Arc; prefer Router::new().
        Router {
            redirects: RwLock::new(Vec::new()),
            rewrites: RwLock::new(Vec::new()),
        }
    }
}

/// Exact match, or prefix match when `source` ends with `/`.
fn path_match(source: &str, path: &str) -> bool {
    if let Some(prefix) = source.strip_suffix('/') {
        path == prefix || path.starts_with(source)
    } else {
        path == source
    }
}

/// Build the target: for a prefix source, preserve the trailing remainder.
fn rewrite_target(source: &str, destination: &str, path: &str) -> String {
    if let Some(prefix) = source.strip_suffix('/') {
        if let Some(rest) = path.strip_prefix(prefix) {
            let dest = destination.trim_end_matches('/');
            return format!("{dest}{rest}");
        }
    }
    destination.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_then_rewrite() {
        let r = Router::new();
        r.add_redirect(Redirect { source: "/old".into(), destination: "/new".into(), status: 308 });
        r.add_rewrite(Rewrite { source: "/blog/".into(), destination: "/posts/".into() });

        match r.evaluate("/old") {
            RouteOutcome::Redirect { location, status } => {
                assert_eq!(location, "/new");
                assert_eq!(status, 308);
            }
            _ => panic!("expected redirect"),
        }
        match r.evaluate("/blog/hello") {
            RouteOutcome::Continue(p) => assert_eq!(p, "/posts/hello"),
            _ => panic!("expected rewrite"),
        }
        match r.evaluate("/keep") {
            RouteOutcome::Continue(p) => assert_eq!(p, "/keep"),
            _ => panic!("expected passthrough"),
        }
    }
}
