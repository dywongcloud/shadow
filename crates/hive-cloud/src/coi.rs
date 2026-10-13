//! Cross-origin isolation (COOP + COEP) for the browser-node lane.
//!
//! ## Why this exists
//!
//! `SharedArrayBuffer` is only exposed to a document the browser considers **cross-origin
//! isolated**, which requires BOTH `Cross-Origin-Opener-Policy: same-origin` and
//! `Cross-Origin-Embedder-Policy: require-corp` (or `credentialless`). node-worker's synchronous
//! bridge (`receiveMessageOnPort`, a *synchronous* drain of a `MessagePort`) cannot be implemented
//! without it (`vendor/node-worker/src/worker/node/worker_threads.ts`, '## What is not here',
//! states the tradeoff: the runtime deliberately has no `SharedArrayBuffer` today, which is what
//! lets it run without isolation), so turning the bridge on starts HERE, with headers.
//!
//! ## Which default, and why: `lane`, not `global`
//!
//! A global default is too risky, and the risk is readable straight out of this repo. **The
//! dashboard loads Clerk** (`ui/app/layout.tsx` wraps the whole tree in `<ClerkProvider>`, and the
//! CSP lists the Clerk origins and `https://challenges.cloudflare.com`): under `require-corp` every
//! cross-origin `no-cors` subresource is BLOCKED unless it sends `Cross-Origin-Resource-Policy` or
//! is fetched in CORS mode, and a plain `<script src="https://<clerk-cdn>/...">` is `no-cors`, so
//! `require-corp` on the dashboard apex takes out authentication. **Tenant deployments** are served
//! by this same public listener under `*.{apps_domain}`, and stamping `require-corp` there would
//! break every tenant app that hotlinks an image, a font or a script with no CORP header: a
//! platform-wide regression caused by a header, blamed on the tenant. **`img-src https:`** (GitHub
//! avatars) is in the enforcing CSP, so those loads are real and would break too.
//!
//! So the default is `HIVE_COI=lane`: COOP/COEP are emitted **only** on the surfaces that host the
//! browser-node lane, the dashboard hosts (apex + `www`) at the lane paths (`/run-node`,
//! `/run-node-worker.js`, `/browser-node/*`). Everything else on the public listener is untouched.
//!
//! ## `credentialless`, not `require-corp`
//!
//! Default `HIVE_COI_COEP=credentialless` keeps the document isolated while still allowing
//! cross-origin `no-cors` subresources (fetched without credentials), which is what keeps Clerk's
//! script bundle and third-party images loading: `/run-node` ALSO loads Clerk, being inside the
//! root layout. `require-corp` is available for a lane known to be free of cross-origin embeds.
//! Caveat: `credentialless` is Chromium/Firefox-only, **Safari does not implement it**, so a Safari
//! page never becomes isolated and the lane must degrade (see Degradation); that is a property of
//! the browser, not of this header.
//!
//! ## What breaks under COEP
//!
//! When last swept, the lane's own static root (`ui/public`, synced from `crates/hive-browser/www`
//! by `ui/scripts/sync-browser-node.mjs`) loaded only same-origin subresources (module imports and
//! workers through `new URL(..., import.meta.url)` or `location`; the only absolute URLs were XML
//! namespaces, comments and local placeholder strings, never fetched), so COEP changes nothing
//! there. The lane's file set moves, so the durable artifact is the SWEEP, not a list: `grep -rn
//! -oE "https?://[^[:space:]\"']+" ui/public crates/hive-browser/www`, discarding XML namespaces
//! and comments. Re-run it before widening `lane_paths` or switching to `require-corp`. What is not
//! clean is contained by lane-scoping because it lives OUTSIDE the lane paths: Clerk (every
//! dashboard page), the Stripe checkout redirect (`ui/app/billing/page.tsx`),
//! `challenges.cloudflare.com` (turnstile), GitHub avatar images and every tenant deployment under
//! `*.{apps_domain}`.
//!
//! ## Degradation
//!
//! A header is a request, never a promise: the browser decides isolation, and withholds it on
//! Safari (`credentialless` unimplemented), on a non-secure origin, or when a Service Worker strips
//! the header. So the lane reports the truth instead of hanging: `public/run-node-worker.js` reads
//! `globalThis.crossOriginIsolated` / `typeof SharedArrayBuffer` in its own global, ships it as the
//! additive `isolation` status key, and answers the `syncBridge` port message with a typed refusal
//! when it is absent. Anything that would synchronously drain a port calls that probe FIRST: a
//! refusal is an error, never a wait.

use std::collections::HashSet;

use axum::http::header::{HeaderName, HeaderValue};

pub const COOP: HeaderName = HeaderName::from_static("cross-origin-opener-policy");
pub const COEP: HeaderName = HeaderName::from_static("cross-origin-embedder-policy");

const COOP_SAME_ORIGIN: &str = "same-origin";
const COEP_CREDENTIALLESS: &str = "credentialless";
const COEP_REQUIRE_CORP: &str = "require-corp";

/// The surfaces that host the browser-node lane. Everything under these prefixes is served by the
/// dashboard from its own static root and loads only same-origin subresources (see the module doc),
/// so isolating them isolates the SharedWorker that owns node-worker's runtime without touching
/// Clerk, billing or any tenant deployment.
pub const DEFAULT_LANE_PATHS: [&str; 3] = ["/run-node", "/run-node-worker.js", "/browser-node/"];

/// Where COOP/COEP are emitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Never emit. The lane degrades to async-only everywhere.
    Off,
    /// Emit on the lane hosts at the lane paths (the default).
    Lane,
    /// Emit on every response of the listener this is applied to. Requires the
    /// whole surface to be CORP-clean; use only after re-running the sweep.
    Global,
}

/// The `Cross-Origin-Embedder-Policy` value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Coep {
    /// Cross-origin `no-cors` subresources still load, without credentials. Keeps Clerk scripts and
    /// third-party images alive. Not implemented by Safari, so Safari never becomes isolated.
    Credentialless,
    /// Blocks every cross-origin subresource that is not CORP/CORS enabled.
    /// Strictest; breaks the dashboard's Clerk embed today.
    RequireCorp,
}

impl Coep {
    pub fn as_str(self) -> &'static str {
        match self {
            Coep::Credentialless => COEP_CREDENTIALLESS,
            Coep::RequireCorp => COEP_REQUIRE_CORP,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    pub coep: Coep,
    /// Lowercased hostnames (no port). `Lane` mode matches on these.
    pub lane_hosts: HashSet<String>,
    pub lane_paths: Vec<String>,
}

/// Host as the browser means it: the `Host:` header first (that is what the browser used to open
/// the document), then the URI authority, lowercased and stripped of `:port`.
fn request_host(req: &axum::http::Request<axum::body::Body>) -> String {
    req.headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| req.uri().host().map(|h| h.to_string()))
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

impl Config {
    /// Reads the policy from the environment. `lane_hosts` supplies the dashboard hosts (apex +
    /// `www`) this node serves; `HIVE_COI_LANE_HOSTS` overrides them outright (a dev node's tunnel
    /// host, or a lane served from its own name). Every unrecognised value falls back to the
    /// DEFAULT and says so: an env typo must never silently turn isolation off OR on.
    pub fn from_env(lane_hosts: &[String]) -> Self {
        let mode = match std::env::var("HIVE_COI")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "lane".to_string())
            .as_str()
        {
            "off" | "0" | "false" | "none" => Mode::Off,
            "global" | "on" | "all" | "1" | "true" => Mode::Global,
            "lane" => Mode::Lane,
            other => {
                tracing::warn!(value = other, "HIVE_COI unrecognised; defaulting to lane");
                Mode::Lane
            }
        };
        let coep = match std::env::var("HIVE_COI_COEP")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .unwrap_or_else(|| COEP_CREDENTIALLESS.to_string())
            .as_str()
        {
            COEP_REQUIRE_CORP | "require_corp" | "corp" => Coep::RequireCorp,
            COEP_CREDENTIALLESS => Coep::Credentialless,
            other => {
                tracing::warn!(
                    value = other,
                    "HIVE_COI_COEP unrecognised; defaulting to credentialless"
                );
                Coep::Credentialless
            }
        };
        let hosts = match std::env::var("HIVE_COI_LANE_HOSTS")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            Some(list) => list
                .split(',')
                .map(|s| {
                    s.trim()
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .split('/')
                        .next()
                        .unwrap_or("")
                        .split(':')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                })
                .filter(|s| !s.is_empty())
                .collect(),
            None => lane_hosts
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        };
        let lane_paths = match std::env::var("HIVE_COI_LANE_PATHS")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            Some(list) => list
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => DEFAULT_LANE_PATHS.iter().map(|s| s.to_string()).collect(),
        };
        let cfg = Self {
            mode,
            coep,
            lane_hosts: hosts,
            lane_paths,
        };
        tracing::info!(
            mode = ?cfg.mode,
            coep = cfg.coep.as_str(),
            hosts = ?cfg.lane_hosts,
            paths = ?cfg.lane_paths,
            "cross-origin isolation policy (SharedArrayBuffer for the browser-node lane)"
        );
        cfg
    }

    /// Does this request's document get the isolation headers?
    /// `Lane` mode needs host AND path: a tenant deployment under `*.{apps_domain}` may
    /// legitimately own a `/run-node` path of its own, and the dashboard host serves plenty of
    /// non-lane pages (billing talks to Stripe) that must not be isolated.
    pub fn applies_to(&self, host: &str, path: &str) -> bool {
        match self.mode {
            Mode::Off => false,
            Mode::Global => true,
            Mode::Lane => {
                self.lane_hosts.contains(host)
                    && self
                        .lane_paths
                        .iter()
                        .any(|p| path == p || path.starts_with(p))
            }
        }
    }
}

/// Response middleware: stamps COOP/COEP on the responses the policy selects. `insert` (not
/// `append`) on purpose: if the upstream already sent one of these (a dashboard `next.config.mjs`
/// header, or a proxy hop) this node's policy wins, and there is never a duplicated COEP on the
/// wire, which some browsers treat as a policy violation rather than a join.
pub async fn headers(
    axum::extract::State(cfg): axum::extract::State<std::sync::Arc<Config>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let host = request_host(&req);
    let path = req.uri().path().to_string();
    let mut resp = next.run(req).await;
    if cfg.applies_to(&host, &path) {
        let headers = resp.headers_mut();
        headers.insert(COOP, HeaderValue::from_static(COOP_SAME_ORIGIN));
        headers.insert(COEP, HeaderValue::from_static(cfg.coep.as_str()));
    }
    resp
}

/// Wrap a router with the isolation layer.
pub fn layer(router: axum::Router, cfg: Config) -> axum::Router {
    router.layer(axum::middleware::from_fn_with_state(
        std::sync::Arc::new(cfg),
        headers,
    ))
}
