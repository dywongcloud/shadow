//! Cross-origin isolation (COOP + COEP) for the browser-node lane.
//!
//! ## Why this exists
//!
//! `SharedArrayBuffer` is only exposed to a document the browser considers
//! **cross-origin isolated**, which requires BOTH
//!
//! - `Cross-Origin-Opener-Policy: same-origin`
//! - `Cross-Origin-Embedder-Policy: require-corp` (or `credentialless`)
//!
//! node-worker's synchronous bridge (`receiveMessageOnPort`, a *synchronous*
//! drain of a `MessagePort`) cannot be implemented without it — see
//! `vendor/node-worker/src/worker/node/worker_threads.ts` ("## What is not
//! here"), which states the tradeoff outright: the runtime deliberately has no
//! `SharedArrayBuffer` today, and that is what lets it run without isolation.
//! Turning the bridge on therefore starts HERE, with headers.
//!
//! ## Which default, and why: `lane`, not `global`
//!
//! A global default is too risky, and the risk is not hypothetical — it is
//! readable straight out of this repo:
//!
//! - **The dashboard loads Clerk** (`ui/app/layout.tsx` wraps the whole tree in
//!   `<ClerkProvider>`; `ui/next.config.mjs` lists `CLERK_CSP_ORIGINS` and
//!   `https://challenges.cloudflare.com` in `script-src`/`frame-src`). Under
//!   `require-corp` every cross-origin `no-cors` subresource is BLOCKED unless
//!   it sends `Cross-Origin-Resource-Policy` or is fetched in CORS mode. A
//!   plain `<script src="https://<clerk-cdn>/...">` is `no-cors`, so
//!   `require-corp` on the dashboard apex takes out authentication.
//! - **Tenant deployments** are served by this same public listener under
//!   `*.{apps_domain}`. Stamping `require-corp` there would break every tenant
//!   app that hotlinks an image, a font or a script with no CORP header — a
//!   platform-wide regression caused by a header, blamed on the tenant.
//! - **`img-src https:`** (GitHub avatars) is in the enforcing CSP, so those
//!   loads are real and would break under `require-corp` too.
//!
//! So the default is `HIVE_COI=lane`: COOP/COEP are emitted **only** on the
//! surfaces that actually host the browser-node lane — the dashboard hosts
//! (apex + `www`) at the lane paths (`/run-node`, `/run-node-worker.js`,
//! `/browser-node/*`). Everything else on the public listener is untouched.
//!
//! ## `credentialless`, not `require-corp`
//!
//! Default `HIVE_COI_COEP=credentialless`: it keeps the document isolated while
//! still allowing cross-origin `no-cors` subresources (they are fetched without
//! credentials), which is what keeps Clerk's script bundle and third-party
//! images loading. `require-corp` is available for a lane that is known to be
//! free of cross-origin embeds. Honest caveat, not glossed over:
//! `credentialless` is Chromium/Firefox-only — **Safari does not implement
//! it**, so a Safari page never becomes isolated and the lane must degrade
//! (see "Degradation" below). That is a property of the browser, not of this
//! header.
//!
//! ## Inventory: cross-origin / embedded resources that break under COEP
//!
//! Swept with `grep -rn -oE "https?://..." ui/public crates/hive-browser/www`
//! plus a sweep for `<script|link|img|iframe|source>` and `import(`.
//!
//! **`ui/public` (the lane's own static root) — CLEAN.** Every load is
//! same-origin and relative to `import.meta.url`, so COEP changes nothing:
//! - `run-node-worker.js`: `import(WASM_MODULE_URL)` and
//!   `import(MESH_MODULE_URL)` are both `new URL("./browser-node/...",
//!   import.meta.url)` — same-origin module fetches.
//! - `browser-node/sqlite/sync-client.js`, `hcb1.js`, `wa-sqlite/**`: same.
//! - `browser-node/node-worker-agent.js` / `-host.js` / `-vfs.js` and
//!   `sync-fs-agent.js`: every URL is `new URL(..., import.meta.url)` or
//!   `new URL(..., location.href)`; its service-worker registration even
//!   REFUSES a worker script outside its own origin and scope.
//! - `offline.html`: one `<img src="/shadw-logo-dark.png">` — same-origin.
//! - The only absolute URLs found are XML namespace strings
//!   (`http://www.w3.org/2000/svg`) and comments/doc links (`sqlite.org`,
//!   `github.com` in `wa-sqlite` comments, `llms.txt`, `fonts/README.md`).
//!   Namespace URIs are never fetched; nothing here is a subresource.
//! - `browser-node/node-runtime.js` mentioned `http://browser.invalid` and
//!   `http://localhost` — placeholders inside strings, never fetched.
//!
//! **`crates/hive-browser/www` (the source synced into `ui/public/browser-node`
//! by `ui/scripts/sync-browser-node.mjs`) — CLEAN.** Same-origin by
//! construction:
//! - `function-runtime.js`: `new URL(configuredFrame || "./function-frame.html",
//!   location.href)` — the frame is same-origin with the host page.
//! - `function-runner.js`: `this.frame.src = runtime.frameUrl` — same URL.
//! - `index.html` / `function-frame.html`: only module scripts, no external
//!   `src`. `pkg/hive_browser.js` embeds its own wasm (`--loader:.wasm=binary`).
//! - `asset-store.js` / `asset-sw.js`: `new URL(..., location.origin)`.
//! - The one absolute URL is `index.html`'s dev-harness relay default
//!   (`http://127.0.0.1:3341`), a local bring-up string, not a subresource.
//!
//! The lane's own file set moves (the node-worker host/agent/vfs split
//! replaced `node-runtime.js` mid-flight), so the durable artifact is the
//! SWEEP, not this list: `grep -rn -oE "https?://[^[:space:]\"']+"
//! ui/public crates/hive-browser/www`, then discard XML namespaces
//! (`w3.org/2000/svg`) and comments. Re-run it before widening `lane_paths`
//! or switching to `require-corp`.
//!
//! **Not clean, and why lane-scoping is what contains it** — all of these live
//! OUTSIDE the lane paths and are therefore untouched by the default:
//! - Clerk (`ui/app/layout.tsx`, every dashboard page incl. `/run-node`).
//! - Stripe checkout (`ui/app/billing/page.tsx` redirects to a cross-origin
//!   Stripe URL).
//! - `https://challenges.cloudflare.com` (turnstile) in the CSP.
//! - GitHub avatar images (`img-src https:`).
//! - Every tenant deployment under `*.{apps_domain}`.
//!
//! One real consequence of lane-scoping worth naming: `/run-node` ALSO loads
//! Clerk (it is inside the root layout), which is exactly why the default COEP
//! is `credentialless` rather than `require-corp`. Setting `require-corp` here
//! is an operator decision that must be preceded by re-running the sweep
//! above.
//!
//! ## Degradation
//!
//! A header is a request, never a promise: the browser decides isolation, and
//! it withholds it on Safari (`credentialless` unimplemented), on a
//! non-secure origin, or when a Service Worker strips the header. So the lane
//! reports the truth instead of hanging: `public/run-node-worker.js` reads
//! `globalThis.crossOriginIsolated` / `typeof SharedArrayBuffer` in its own
//! global, ships it as the additive `isolation` status key, and answers the
//! `syncBridge` port message with a typed refusal when it is absent. Anything
//! that would synchronously drain a port calls that probe FIRST — a refusal is
//! an error, never a wait.

use std::collections::HashSet;

use axum::http::header::{HeaderName, HeaderValue};

pub const COOP: HeaderName = HeaderName::from_static("cross-origin-opener-policy");
pub const COEP: HeaderName = HeaderName::from_static("cross-origin-embedder-policy");

const COOP_SAME_ORIGIN: &str = "same-origin";
const COEP_CREDENTIALLESS: &str = "credentialless";
const COEP_REQUIRE_CORP: &str = "require-corp";

/// The surfaces that host the browser-node lane. Everything under these
/// prefixes is served by the dashboard from its own static root and loads only
/// same-origin subresources (see the module doc's inventory), so isolating
/// them isolates the SharedWorker that owns node-worker's runtime without
/// touching Clerk, billing or any tenant deployment.
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
    /// Cross-origin `no-cors` subresources still load, without credentials.
    /// Keeps Clerk scripts and third-party images alive. Not implemented by
    /// Safari, so Safari never becomes isolated.
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

/// Host as the browser means it: `Host:` header first (that is what the
/// browser used to open the document), then the URI authority, lowercased and
/// stripped of `:port`.
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
    /// Read the policy from the environment. `lane_hosts` supplies the
    /// dashboard hosts (apex + `www`) this node serves; `HIVE_COI_LANE_HOSTS`
    /// overrides them outright (a dev node's tunnel host, or a lane served
    /// from its own name).
    ///
    /// Every unrecognised value falls back to the DEFAULT and says so — an
    /// env typo must never silently turn isolation off OR on.
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
    pub fn applies_to(&self, host: &str, path: &str) -> bool {
        match self.mode {
            Mode::Off => false,
            Mode::Global => true,
            Mode::Lane => {
                // Host AND path: a tenant deployment under `*.{apps_domain}`
                // may legitimately own a `/run-node` path of its own, and the
                // dashboard host serves plenty of non-lane pages (billing talks
                // to Stripe) that must not be isolated.
                self.lane_hosts.contains(host)
                    && self
                        .lane_paths
                        .iter()
                        .any(|p| path == p || path.starts_with(p))
            }
        }
    }
}

/// Response middleware: stamp COOP/COEP on the responses the policy selects.
///
/// `insert` (not `append`) on purpose — if the upstream already sent one of
/// these (a dashboard `next.config.mjs` header, or a proxy hop) this node's
/// policy wins, and there is never a duplicated COEP on the wire, which some
/// browsers treat as a policy violation rather than a join.
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
