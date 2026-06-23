//! `fluid-core` — the deployment model for the serving layer.
//!
//! Where Hive builds code, Fluid *serves* it. A [`Deployment`] is the unit a
//! user ships: some static assets plus zero or more [`FunctionConfig`]s, with
//! [`Route`]s mapping request paths to either static files or a function.
//!
//! The function config carries the knobs that make compute "Fluid": an
//! in-function `max_concurrency` (many requests per instance), `min_instances`
//! (keep-warm), `max_instances` (autoscale ceiling), and an `idle_ttl` for
//! scale-to-zero.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeploymentId(pub String);

impl DeploymentId {
    pub fn new() -> Self {
        DeploymentId(format!("dpl-{}", &Uuid::new_v4().simple().to_string()[..10]))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Default for DeploymentId {
    fn default() -> Self {
        Self::new()
    }
}
impl From<String> for DeploymentId {
    fn from(s: String) -> Self {
        DeploymentId(s)
    }
}
impl std::fmt::Display for DeploymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::fmt::Debug for DeploymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A serverless function within a deployment. The server process must listen on
/// `$PORT` and speak HTTP/1.1.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionConfig {
    pub name: String,
    /// Informational: "node", "python", "go", "command", ...
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// argv to start the server, e.g. ["node", "server.js"].
    pub start_cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// vCPUs per instance (microVM). Standard tier = 1, Performance tier = 2.
    #[serde(default = "default_vcpus")]
    pub vcpus: u32,
    #[serde(default = "default_memory")]
    pub memory_mib: u32,
    /// Fluid in-function concurrency: max simultaneous requests per instance.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    /// Keep at least this many instances warm (0 = scale fully to zero).
    #[serde(default)]
    pub min_instances: u32,
    /// Autoscaling ceiling.
    #[serde(default = "default_max_instances")]
    pub max_instances: u32,
    /// Scale an idle instance down after this many seconds with no requests.
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl_secs: u64,
    /// Max wall-clock duration for a single invocation (Vercel default 300s).
    /// Exceeding it returns 504 — error isolation keeps other requests alive.
    #[serde(default = "default_max_duration")]
    pub max_duration_secs: u64,
    /// Per-function region preference (`vercel.json` `functions[].regions`).
    /// Overrides the project-level default for this function when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
    /// Glob of extra files to bundle (`functions[].includeFiles`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_files: Option<String>,
    /// Glob of files to exclude (`functions[].excludeFiles`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_files: Option<String>,
}

fn default_runtime() -> String {
    "command".into()
}
fn default_vcpus() -> u32 {
    1
}
fn default_memory() -> u32 {
    // Standard serverless tier: 2 GB.
    2048
}
fn default_max_concurrency() -> u32 {
    10
}
fn default_max_instances() -> u32 {
    10
}
fn default_idle_ttl() -> u64 {
    30
}
fn default_max_duration() -> u64 {
    300 // Vercel Fluid default max duration (5 minutes)
}

impl Default for FunctionConfig {
    fn default() -> Self {
        FunctionConfig {
            name: String::new(),
            runtime: default_runtime(),
            start_cmd: Vec::new(),
            env: BTreeMap::new(),
            vcpus: default_vcpus(),
            memory_mib: default_memory(),
            max_concurrency: default_max_concurrency(),
            min_instances: 0,
            max_instances: default_max_instances(),
            idle_ttl_secs: default_idle_ttl(),
            max_duration_secs: default_max_duration(),
            regions: Vec::new(),
            include_files: None,
            exclude_files: None,
        }
    }
}

/// What a route serves.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Static,
    Function(String),
}

/// A path-prefix route. Longest matching prefix wins (computed at match time).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub pattern: String,
    pub target: RouteTarget,
}

/// A redirect rule mapped from the framework build (Next.js `redirects()`,
/// Build Output API routes with a 3xx status) or from `vercel.json`. Evaluated
/// by the gateway before routing — first match wins. `status` is the resolved
/// HTTP code (308 permanent / 307 temporary / explicit `statusCode`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Redirect {
    pub source: String,
    pub destination: String,
    #[serde(default = "default_redirect_status")]
    pub status: u16,
    /// Conditional matching (`vercel.json` `has`) — all must be present/match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<RuleCondition>,
    /// Conditional matching (`vercel.json` `missing`) — all must be absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<RuleCondition>,
}
fn default_redirect_status() -> u16 {
    308
}

/// A rewrite rule (path is rewritten server-side, client URL unchanged).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rewrite {
    pub source: String,
    pub destination: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<RuleCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<RuleCondition>,
}

/// A single response header (`vercel.json` `headers[].headers[]`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub key: String,
    pub value: String,
}

/// A response-header rule (`vercel.json` `headers`). When `source` (+ optional
/// `has`/`missing`) matches a request path, the gateway injects `headers` onto
/// the response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeaderRule {
    pub source: String,
    pub headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<RuleCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<RuleCondition>,
}

/// A scheduled job (`vercel.json` `crons`). Registered against the production
/// deployment; the scheduler invokes `path` on `schedule` (cron expression).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronSpec {
    pub path: String,
    pub schedule: String,
}

/// A `has`/`missing` condition matched against the request (`vercel.json`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleCondition {
    /// One of: `host`, `header`, `cookie`, `query`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<CondValue>,
}

/// The `value` of a condition — a literal string, or an expressive
/// prefix/suffix matcher (`{ "pre": "...", "suf": "..." }`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CondValue {
    Text(String),
    Expr {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suf: Option<String>,
    },
}

impl CondValue {
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            CondValue::Text(t) => candidate == t,
            CondValue::Expr { pre, suf } => {
                pre.as_deref().map(|p| candidate.starts_with(p)).unwrap_or(true)
                    && suf.as_deref().map(|s| candidate.ends_with(s)).unwrap_or(true)
            }
        }
    }
}

/// Image Optimization configuration (`vercel.json` `images`) — enforced by the
/// gateway's `/_vercel/image` endpoint.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImagesConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sizes: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qualities: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_cache_ttl: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_patterns: Vec<RemotePattern>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_patterns: Vec<LocalPattern>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangerously_allow_svg: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_security_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_disposition_type: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemotePattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalPattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

/// Minimal request context for evaluating `has`/`missing` conditions and
/// host-scoped matching. Built cheaply by the gateway per request.
#[derive(Clone, Debug, Default)]
pub struct ReqCtx {
    pub host: String,
    /// (lowercased key, value) pairs.
    pub headers: Vec<(String, String)>,
    /// Raw query string (without leading `?`).
    pub query: String,
}

impl ReqCtx {
    pub fn header(&self, key: &str) -> Option<String> {
        let k = key.to_ascii_lowercase();
        self.headers.iter().find(|(hk, _)| *hk == k).map(|(_, v)| v.clone())
    }
    pub fn cookie(&self, key: &str) -> Option<String> {
        let raw = self.header("cookie")?;
        for part in raw.split(';') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=') {
                if k.trim() == key {
                    return Some(v.trim().to_string());
                }
            }
        }
        None
    }
    pub fn query_param(&self, key: &str) -> Option<String> {
        for part in self.query.split('&') {
            if let Some((k, v)) = part.split_once('=') {
                if k == key {
                    return Some(v.to_string());
                }
            } else if part == key {
                return Some(String::new());
            }
        }
        None
    }
}

/// Evaluate one condition against the request.
fn cond_matches(c: &RuleCondition, ctx: &ReqCtx) -> bool {
    let actual: Option<String> = match c.kind.as_str() {
        "host" => Some(ctx.host.clone()),
        "header" => c.key.as_ref().and_then(|k| ctx.header(k)),
        "cookie" => c.key.as_ref().and_then(|k| ctx.cookie(k)),
        "query" => c.key.as_ref().and_then(|k| ctx.query_param(k)),
        _ => None,
    };
    match (&c.value, actual) {
        (None, Some(_)) => true,       // presence only
        (None, None) => false,
        (Some(v), Some(a)) => v.matches(&a),
        (Some(_), None) => false,
    }
}

/// `has`: every condition must match. `missing`: every condition must NOT match.
fn conditions_pass(has: &[RuleCondition], missing: &[RuleCondition], ctx: &ReqCtx) -> bool {
    has.iter().all(|c| cond_matches(c, ctx)) && missing.iter().all(|c| !cond_matches(c, ctx))
}

/// Resolved redirect status for a redirect built from `vercel.json`:
/// explicit `statusCode` wins; else `permanent` => 308 / 307; else default 308.
pub fn redirect_status(permanent: Option<bool>, status_code: Option<u16>) -> u16 {
    if let Some(sc) = status_code {
        return sc;
    }
    match permanent {
        Some(false) => 307,
        _ => 308,
    }
}

// ---- path-to-regexp-lite matcher (Vercel `:param` / `:param*` + inline regex) ----

/// Compile a Vercel source pattern (`/blog/:slug`, `/post/:p(\\d+)`,
/// `/proxy/:m*`, `/(.*)`) into an anchored regex with named captures. Returns
/// `None` if the pattern isn't regex-like or fails to compile (caller falls back
/// to literal/prefix matching).
fn compile_source(source: &str) -> Option<regex::Regex> {
    if !(source.contains(':') || source.contains('(') || source.contains('*')) {
        return None;
    }
    let mut out = String::from("^");
    let mut lit = String::new();
    let flush = |out: &mut String, lit: &mut String| {
        if !lit.is_empty() {
            out.push_str(&regex::escape(lit));
            lit.clear();
        }
    };
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ':' => {
                flush(&mut out, &mut lit);
                i += 1;
                let mut name = String::new();
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    name.push(chars[i]);
                    i += 1;
                }
                if name.is_empty() {
                    lit.push(':');
                    continue;
                }
                // Optional modifier or inline regex.
                if i < chars.len() && chars[i] == '(' {
                    // Balanced custom pattern.
                    let mut depth = 0i32;
                    let mut body = String::new();
                    while i < chars.len() {
                        let ch = chars[i];
                        if ch == '(' {
                            depth += 1;
                            if depth == 1 {
                                i += 1;
                                continue;
                            }
                        } else if ch == ')' {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        body.push(ch);
                        i += 1;
                    }
                    out.push_str(&format!("(?P<{name}>{body})"));
                } else if i < chars.len() && chars[i] == '*' {
                    out.push_str(&format!("(?P<{name}>.*)"));
                    i += 1;
                } else if i < chars.len() && chars[i] == '+' {
                    out.push_str(&format!("(?P<{name}>.+)"));
                    i += 1;
                } else {
                    out.push_str(&format!("(?P<{name}>[^/]+)"));
                }
            }
            '(' => {
                // Raw regex group passes through verbatim (balanced copy).
                flush(&mut out, &mut lit);
                let mut depth = 0i32;
                while i < chars.len() {
                    let ch = chars[i];
                    out.push(ch);
                    if ch == '(' {
                        depth += 1;
                    } else if ch == ')' {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
            }
            '*' => {
                flush(&mut out, &mut lit);
                out.push_str(".*");
                i += 1;
            }
            _ => {
                lit.push(c);
                i += 1;
            }
        }
    }
    flush(&mut out, &mut lit);
    out.push('$');
    regex::Regex::new(&out).ok()
}

/// Substitute `:name` / `:name*` references in a destination with values
/// captured from the source match.
fn subst_dest(dest: &str, caps: &regex::Captures) -> String {
    let chars: Vec<char> = dest.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ':' {
            i += 1;
            let mut name = String::new();
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                name.push(chars[i]);
                i += 1;
            }
            // Consume an optional trailing `*`/`+` modifier in the destination.
            if i < chars.len() && (chars[i] == '*' || chars[i] == '+') {
                i += 1;
            }
            if let Some(m) = caps.name(&name) {
                out.push_str(m.as_str());
            } else if name.is_empty() {
                out.push(':');
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Try to match `path` against `source` (param/regex aware), returning the
/// resolved destination if it matches. Falls back to literal/prefix matching.
pub fn rule_apply(source: &str, dest: &str, path: &str) -> Option<String> {
    if let Some(re) = compile_source(source) {
        if let Some(caps) = re.captures(path) {
            return Some(subst_dest(dest, &caps));
        }
        // A regex-like source that didn't match: also try the literal fallback,
        // since lookahead-bearing sources may have failed to compile elsewhere.
        if rule_match(source, path) {
            return Some(rule_target(source, dest, path));
        }
        return None;
    }
    if rule_match(source, path) {
        Some(rule_target(source, dest, path))
    } else {
        None
    }
}

/// Whether `source` matches `path` (used by header rules that have no dest).
pub fn rule_matches(source: &str, path: &str) -> bool {
    if let Some(re) = compile_source(source) {
        re.is_match(path) || rule_match(source, path)
    } else {
        rule_match(source, path)
    }
}

/// Middleware / proxy (`middleware.ts` / `proxy.ts`) detected in the build. Runs
/// in the edge runtime ahead of routing; `matcher` lists the path patterns it
/// applies to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Middleware {
    #[serde(default)]
    pub matcher: Vec<String>,
    #[serde(default = "default_edge_runtime")]
    pub runtime: String,
}
fn default_edge_runtime() -> String {
    "edge".into()
}

/// `fluid.json` — what a user writes to describe their deployment.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub project: String,
    /// Relative dir (within the deployment root) holding static assets.
    #[serde(default)]
    pub static_dir: Option<String>,
    /// Per-deployment cell image key. When set, the function pool provisions
    /// cells with this image instead of the node's default, so an isolated
    /// backend (Firecracker) can attach this deployment's delivered build
    /// artifact. `None` => use the node's default image (mock / same-host).
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub functions: Vec<FunctionConfig>,
    #[serde(default)]
    pub routes: Vec<Route>,
    /// Redirects mapped from the framework build (gateway honors these).
    #[serde(default)]
    pub redirects: Vec<Redirect>,
    /// Server-side rewrites mapped from the framework build.
    #[serde(default)]
    pub rewrites: Vec<Rewrite>,
    /// Edge middleware / proxy detected in the build, if any.
    #[serde(default)]
    pub middleware: Option<Middleware>,
    /// Response-header rules (`vercel.json` `headers`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderRule>,
    /// Scheduled jobs (`vercel.json` `crons`) — registered on production deploy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crons: Vec<CronSpec>,
    /// `vercel.json` `cleanUrls` — strip `.html` and redirect extension paths.
    #[serde(default)]
    pub clean_urls: bool,
    /// `vercel.json` `trailingSlash` — `Some(true)` enforce, `Some(false)` strip,
    /// `None` no normalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_slash: Option<bool>,
    /// `vercel.json` `images` — Image Optimization config (gateway enforces).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImagesConfig>,
}

impl Manifest {
    pub fn from_json(s: &str) -> Result<Manifest, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn function(&self, name: &str) -> Option<&FunctionConfig> {
        self.functions.iter().find(|f| f.name == name)
    }

    /// Resolve a request path to a route target using longest-prefix match.
    /// Falls back to Static if nothing matches.
    pub fn resolve(&self, path: &str) -> RouteTarget {
        let mut best: Option<&Route> = None;
        for r in &self.routes {
            if path_matches(&r.pattern, path) {
                match best {
                    Some(b) if b.pattern.len() >= r.pattern.len() => {}
                    _ => best = Some(r),
                }
            }
        }
        best.map(|r| r.target.clone()).unwrap_or(RouteTarget::Static)
    }

    /// The first matching redirect for `path`, as (location, status).
    /// Back-compat path-only entry point (no `has`/`missing` context).
    pub fn redirect_for(&self, path: &str) -> Option<(String, u16)> {
        self.redirect_for_ctx(path, &ReqCtx::default())
    }

    /// The first matching redirect for `path`, honoring `has`/`missing`
    /// conditions and `:param` / regex source patterns.
    pub fn redirect_for_ctx(&self, path: &str, ctx: &ReqCtx) -> Option<(String, u16)> {
        for r in &self.redirects {
            if !conditions_pass(&r.has, &r.missing, ctx) {
                continue;
            }
            if let Some(dest) = rule_apply(&r.source, &r.destination, path) {
                return Some((dest, r.status));
            }
        }
        None
    }

    /// Apply the first matching rewrite, returning the (possibly) rewritten path.
    /// Back-compat path-only entry point.
    pub fn rewrite_path(&self, path: &str) -> String {
        self.rewrite_path_ctx(path, &ReqCtx::default())
    }

    /// Apply the first matching rewrite, honoring `has`/`missing` + `:param`.
    pub fn rewrite_path_ctx(&self, path: &str, ctx: &ReqCtx) -> String {
        for r in &self.rewrites {
            if !conditions_pass(&r.has, &r.missing, ctx) {
                continue;
            }
            if let Some(dest) = rule_apply(&r.source, &r.destination, path) {
                return dest;
            }
        }
        path.to_string()
    }

    /// All response headers to inject for `path` (every matching rule, in order).
    pub fn headers_for(&self, path: &str, ctx: &ReqCtx) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for rule in &self.headers {
            if !conditions_pass(&rule.has, &rule.missing, ctx) {
                continue;
            }
            if rule_matches(&rule.source, path) {
                for h in &rule.headers {
                    out.push((h.key.clone(), h.value.clone()));
                }
            }
        }
        out
    }

    /// Trailing-slash normalization for `path`. Returns `Some(new_path)` when a
    /// 308 redirect should be issued, else `None`. Paths with a file extension in
    /// the last segment are never given a trailing slash (Vercel semantics).
    pub fn trailing_slash_redirect(&self, path: &str) -> Option<String> {
        let want = self.trailing_slash?;
        if path == "/" {
            return None;
        }
        let has_slash = path.ends_with('/');
        if want && !has_slash {
            let last = path.rsplit('/').next().unwrap_or("");
            if last.contains('.') {
                return None; // file with extension
            }
            Some(format!("{path}/"))
        } else if !want && has_slash {
            Some(path.trim_end_matches('/').to_string())
        } else {
            None
        }
    }

    /// Count of edge-runtime functions in this deployment.
    pub fn edge_function_count(&self) -> usize {
        self.functions.iter().filter(|f| f.runtime == "edge").count()
    }
}

/// Exact match, or prefix match when `source` ends with `/`.
fn rule_match(source: &str, path: &str) -> bool {
    if let Some(prefix) = source.strip_suffix('/') {
        path == prefix || path.starts_with(source)
    } else {
        path == source
    }
}

/// Build a redirect/rewrite target, preserving the remainder for prefix sources.
fn rule_target(source: &str, destination: &str, path: &str) -> String {
    if let Some(prefix) = source.strip_suffix('/') {
        if let Some(rest) = path.strip_prefix(prefix) {
            let dest = destination.trim_end_matches('/');
            return format!("{dest}{rest}");
        }
    }
    destination.to_string()
}

/// Prefix match with `/` boundary awareness. `"/api"` matches `/api` and
/// `/api/x` but not `/apixyz`. `"/"` matches everything.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "/" {
        return true;
    }
    let pattern = pattern.trim_end_matches('/');
    if let Some(rest) = path.strip_prefix(pattern) {
        rest.is_empty() || rest.starts_with('/')
    } else {
        false
    }
}

/// Git provenance for a deployment (shown Vercel-style in the dashboard).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitSource {
    pub repo_url: String,
    pub branch: String,
    pub commit: String,
    pub commit_message: String,
}

/// Lifecycle state of a deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployState {
    Queued,
    Building,
    Ready,
    Error,
}
impl Default for DeployState {
    fn default() -> Self { DeployState::Ready }
}

/// Serializable snapshot of a deployment (for persistence + restore).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployRecord {
    pub id: String,
    pub project: String,
    pub root: String,
    pub manifest: Manifest,
    pub created_at_ms: u64,
    pub creator: String,
    pub git: Option<GitSource>,
    pub production: bool,
    /// Environment the deployment was built for: "production" | "preview".
    /// Immutable; `production` only reflects whether it currently holds the prod
    /// alias. Defaults to empty (derive from `production`) for old snapshots.
    #[serde(default)]
    pub target: String,
    /// Final lifecycle state (so a failed build stays "error" across restarts).
    /// Defaults to `ready` for back-compat with snapshots written before this field.
    #[serde(default)]
    pub state: DeployState,
    /// Owning team/tenant. `#[serde(default)]` keeps pre-tenancy snapshots
    /// loadable (they normalize to "personal"); on restore this re-registers the
    /// deployment's function pools under the correct tenant.
    #[serde(default)]
    pub tenant: String,
}

/// A registered deployment (manifest + where its files live).
#[derive(Clone, Debug)]
pub struct Deployment {
    pub id: DeploymentId,
    pub project: String,
    /// Host path to deployment files (mock backend serves from here).
    pub root: std::path::PathBuf,
    pub manifest: Manifest,
    pub created_at_ms: u64,
    pub state: DeployState,
    pub creator: String,
    pub git: Option<GitSource>,
    /// Whether this deployment currently holds the project's PRODUCTION alias
    /// (Vercel's "promoted" flag). Flips on promote/rollback — it does NOT change
    /// `target`.
    pub production: bool,
    /// The environment the deployment was BUILT for: "production" | "preview".
    /// Immutable for the life of the deployment (a superseded production build
    /// keeps target=production even after a newer one is promoted). Empty string
    /// means "derive from `production`" (back-compat for old in-memory values).
    pub target: String,
    /// Owning team/tenant slug (empty = "personal"). Set at deploy time from the
    /// project's team; flows into each cell's `CellSpec` and the Fluid pool so
    /// compute is partitioned and quota'd per tenant.
    pub tenant: String,
}

/// Admin API: request to create a deployment. For the mock backend the gateway
/// reads files directly from `root` (same host); a real deploy would upload a
/// tarball / build artifact instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployRequest {
    pub root: String,
    pub manifest: Manifest,
    #[serde(default)]
    pub creator: Option<String>,
    #[serde(default)]
    pub git: Option<GitSource>,
    #[serde(default)]
    pub production: bool,
}

/// Admin API: deploy directly from a git repository URL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitDeployRequest {
    pub repo_url: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub creator: Option<String>,
    #[serde(default = "default_prod")]
    pub production: bool,
    /// Explicit deploy target: "production" | "preview". When None (the default),
    /// the target is CLASSIFIED from the branch — a push to the project's
    /// production branch is production, every other branch / PR is a preview
    /// (Vercel's model). Webhooks set this to "preview" for PR events; the import
    /// + redeploy flows leave it None so the branch decides.
    #[serde(default)]
    pub target: Option<String>,
    /// Whether to reuse the existing dependency build cache. Defaults to true.
    /// A redeploy can set this false ("Use existing Build Cache" unchecked) to
    /// force a clean install — when a package-lock.json is present that means
    /// `npm ci` instead of `npm install`, and the cached node_modules is skipped.
    #[serde(default = "default_prod")]
    pub use_cache: bool,
    /// Subdirectory within the repo to build (for monorepo templates, e.g.
    /// `examples/nextjs`). Empty/None = repo root.
    #[serde(default)]
    pub root_dir: Option<String>,
    /// Environment variables to set on the project at creation, injected into
    /// BOTH the build (install/build commands) and the runtime (functions /
    /// containers). Set from the "New Project" screen.
    #[serde(default)]
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// When true, this node deploys LOCALLY only (build + host) and does NOT run
    /// the placement scheduler / fanout. The coordinator sets this on the
    /// per-target deploys it dispatches, so a target node "just hosts this" and
    /// placement never recurses.
    #[serde(default)]
    pub no_fanout: bool,
    /// Project BuildConfig (framework/install/build/output/root), forwarded by the
    /// coordinator on a fanout deploy so the target builds with the SAME settings
    /// the user configured — not just whatever it auto-detects. Opaque JSON to
    /// avoid a fluid-core → hive-cloud dependency. None on direct user deploys.
    #[serde(default)]
    pub build_config: Option<serde_json::Value>,
    /// Project FunctionSettings (vcpus/memory/regions/…), forwarded on fanout so a
    /// remotely-placed deployment honors the user's compute tier. Opaque JSON.
    #[serde(default)]
    pub function_settings: Option<serde_json::Value>,
}
fn default_prod() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeploymentInfo {
    pub id: DeploymentId,
    pub project: String,
    pub functions: Vec<String>,
    pub created_at_ms: u64,
    /// Convenience: the project (production-domain) Host alias `<project>.localhost`.
    pub alias: String,
    /// Immutable per-commit Host alias `<project>-<shortsha>.localhost` (Vercel's
    /// commit URL). Empty when the deployment has no git commit. Always resolves
    /// to THIS exact deployment.
    #[serde(default)]
    pub commit_alias: String,
    /// Per-branch Host alias `<project>-git-<branch>.localhost` (Vercel's branch
    /// URL) — resolves to the latest deployment on that branch. Empty without git.
    #[serde(default)]
    pub branch_alias: String,
    /// Immutable per-deployment Host alias `<id>.localhost`, always this deployment.
    #[serde(default)]
    pub id_alias: String,
    /// Build environment: "production" | "preview" — IMMUTABLE (unlike
    /// `production`, which is the live "is currently promoted" flag).
    #[serde(default)]
    pub target: String,
    #[serde(default = "default_ready")]
    pub state: DeployState,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub git: Option<GitSource>,
    #[serde(default)]
    pub production: bool,
    /// Type label for the UI: "static" | "function" | "fullstack".
    #[serde(default)]
    pub kind: String,
    /// Framework features mapped onto this deployment (redirects, middleware…).
    #[serde(default)]
    pub features: DeploymentFeatures,
    /// Owning team/tenant slug (empty = "personal").
    #[serde(default)]
    pub tenant: String,
}
fn default_ready() -> DeployState {
    DeployState::Ready
}

/// Summary of framework build features the platform mapped onto a deployment —
/// surfaced in the dashboard (service graph, overview).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeploymentFeatures {
    pub redirects: usize,
    pub rewrites: usize,
    pub middleware: bool,
    pub edge_functions: usize,
    pub serverless_functions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins() {
        let m = Manifest {
            project: "p".into(),
            static_dir: Some("public".into()),
            functions: vec![],
            routes: vec![
                Route { pattern: "/".into(), target: RouteTarget::Static },
                Route { pattern: "/api".into(), target: RouteTarget::Function("api".into()) },
                Route { pattern: "/api/admin".into(), target: RouteTarget::Function("admin".into()) },
            ],
            ..Default::default()
        };
        assert_eq!(m.resolve("/index.html"), RouteTarget::Static);
        assert_eq!(m.resolve("/api/users"), RouteTarget::Function("api".into()));
        assert_eq!(m.resolve("/api/admin/x"), RouteTarget::Function("admin".into()));
        assert!(!path_matches("/api", "/apixyz"));
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    #[test]
    fn deploy_state_defaults_to_ready() {
        assert_eq!(DeployState::default(), DeployState::Ready);
    }

    #[test]
    fn manifest_resolve_longest_prefix_wins() {
        let m = Manifest {
            routes: vec![
                Route { pattern: "/".into(), target: RouteTarget::Static },
                Route { pattern: "/api".into(), target: RouteTarget::Function("api".into()) },
            ],
            ..Default::default()
        };
        assert_eq!(m.resolve("/api/users"), RouteTarget::Function("api".into()));
        assert_eq!(m.resolve("/index.html"), RouteTarget::Static);
    }

    #[test]
    fn manifest_redirect_and_rewrite() {
        let m = Manifest {
            redirects: vec![Redirect { source: "/old".into(), destination: "/new".into(), status: 308, has: vec![], missing: vec![] }],
            rewrites: vec![Rewrite { source: "/proxy".into(), destination: "/internal".into(), has: vec![], missing: vec![] }],
            ..Default::default()
        };
        assert_eq!(m.redirect_for("/old"), Some(("/new".to_string(), 308)));
        assert_eq!(m.redirect_for("/nope"), None);
        assert_eq!(m.rewrite_path("/proxy"), "/internal");
        assert_eq!(m.rewrite_path("/untouched"), "/untouched");
    }

    fn red(source: &str, dest: &str, status: u16, has: Vec<RuleCondition>, missing: Vec<RuleCondition>) -> Redirect {
        Redirect { source: source.into(), destination: dest.into(), status, has, missing }
    }

    #[test]
    fn param_matching_and_substitution() {
        let m = Manifest {
            redirects: vec![
                red("/blog/:slug", "/news/:slug", 308, vec![], vec![]),
                red("/proxy/:path*", "/internal/:path*", 307, vec![], vec![]),
                red("/post/:p(\\d+)", "/n/:p", 308, vec![], vec![]),
            ],
            ..Default::default()
        };
        assert_eq!(m.redirect_for("/blog/hello"), Some(("/news/hello".into(), 308)));
        assert_eq!(m.redirect_for("/proxy/a/b/c"), Some(("/internal/a/b/c".into(), 307)));
        assert_eq!(m.redirect_for("/post/42"), Some(("/n/42".into(), 308)));
        assert_eq!(m.redirect_for("/post/abc"), None); // non-numeric fails the inline regex
    }

    #[test]
    fn has_missing_conditions() {
        let m = Manifest {
            rewrites: vec![Rewrite {
                source: "/dashboard".into(),
                destination: "/login".into(),
                has: vec![],
                missing: vec![RuleCondition { kind: "cookie".into(), key: Some("auth_token".into()), value: None }],
            }],
            ..Default::default()
        };
        // No auth cookie -> rewrite to /login.
        let ctx_no = ReqCtx::default();
        assert_eq!(m.rewrite_path_ctx("/dashboard", &ctx_no), "/login");
        // With auth cookie present -> NOT rewritten.
        let ctx_yes = ReqCtx { headers: vec![("cookie".into(), "auth_token=abc".into())], ..Default::default() };
        assert_eq!(m.rewrite_path_ctx("/dashboard", &ctx_yes), "/dashboard");
    }

    #[test]
    fn header_rules_inject() {
        let m = Manifest {
            headers: vec![HeaderRule {
                source: "/(.*)".into(),
                headers: vec![Header { key: "X-Frame-Options".into(), value: "DENY".into() }],
                has: vec![],
                missing: vec![],
            }],
            ..Default::default()
        };
        let got = m.headers_for("/anything", &ReqCtx::default());
        assert_eq!(got, vec![("X-Frame-Options".to_string(), "DENY".to_string())]);
    }

    #[test]
    fn trailing_slash_normalization() {
        let strip = Manifest { trailing_slash: Some(false), ..Default::default() };
        assert_eq!(strip.trailing_slash_redirect("/about/"), Some("/about".into()));
        assert_eq!(strip.trailing_slash_redirect("/about"), None);
        let add = Manifest { trailing_slash: Some(true), ..Default::default() };
        assert_eq!(add.trailing_slash_redirect("/about"), Some("/about/".into()));
        assert_eq!(add.trailing_slash_redirect("/styles.css"), None); // file ext untouched
        let none = Manifest { trailing_slash: None, ..Default::default() };
        assert_eq!(none.trailing_slash_redirect("/about/"), None);
    }

    #[test]
    fn redirect_status_resolution() {
        assert_eq!(redirect_status(None, None), 308);
        assert_eq!(redirect_status(Some(false), None), 307);
        assert_eq!(redirect_status(Some(true), None), 308);
        assert_eq!(redirect_status(Some(true), Some(301)), 301);
    }

    #[test]
    fn deploy_record_state_defaults_when_absent() {
        // Snapshots written before `state` existed deserialize to Ready.
        let json = r#"{"id":"d1","project":"p","root":"/tmp","manifest":{"project":"p"},"created_at_ms":0,"creator":"you","git":null,"production":true}"#;
        let rec: DeployRecord = serde_json::from_str(json).expect("deserializes without state");
        assert_eq!(rec.state, DeployState::Ready);
    }
}
