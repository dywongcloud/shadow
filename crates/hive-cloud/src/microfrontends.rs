//! Platform-native Microfrontends — the model, path matcher, conflict detector,
//! config generation, validation, and fallback-environment resolution.
//!
//! One logical application is composed of multiple independently deployed
//! projects. Exactly one project is the DEFAULT application (serves `/` and any
//! unmatched path); every other member is a CHILD that owns one or more path
//! routes. The ingress rewrites the request host to the child project so the
//! existing alias-resolution + dispatch serves the child's deployment for that
//! path — one domain, many projects, no second runtime path.
//!
//! Storage lives in [`crate::enterprise::EnterpriseStore`] (`mfe` field, team ->
//! groups), persisted with the rest of the enterprise config and gossiped via
//! `EdgeExport`. This module owns only PURE logic (types + matcher + validation +
//! config), so it is exhaustively unit-testable with no I/O.

use serde::{Deserialize, Serialize};

fn yes() -> bool {
    true
}
fn default_fallback_env() -> String {
    "production".into()
}
fn default_observability() -> String {
    "default_application".into()
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// Every failure mode carries a stable machine code (the `code()` string) plus a
/// human message. The API layer maps these to HTTP 4xx with the code in the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MfeError {
    GroupNotFound(String),
    ProjectAlreadyInGroup {
        project: String,
        group: String,
    },
    MissingDefaultApp,
    MultipleDefaultApps,
    InvalidRoute {
        path: String,
        reason: String,
    },
    RouteConflict {
        a: String,
        b: String,
    },
    MissingFallback,
    Unauthorized(String),
    TargetDeploymentNotFound {
        project: String,
    },
    /// A child declared no routes (only the default app may omit routing).
    ChildMissingRoute {
        project: String,
    },
    /// `asset_prefix` set without a matching `/{prefix}/:path*` route.
    AssetPrefixWithoutRoute {
        project: String,
        asset_prefix: String,
    },
    /// `default_route` did not start with `/`.
    InvalidDefaultRoute {
        project: String,
        value: String,
    },
    /// A referenced project is not a member of the group.
    UnknownMember(String),
}

impl MfeError {
    pub fn code(&self) -> &'static str {
        match self {
            MfeError::GroupNotFound(_) => "MICROFRONTENDS_GROUP_NOT_FOUND",
            MfeError::ProjectAlreadyInGroup { .. } => "MICROFRONTENDS_PROJECT_ALREADY_IN_GROUP",
            MfeError::MissingDefaultApp => "MICROFRONTENDS_MISSING_DEFAULT_APP",
            MfeError::MultipleDefaultApps => "MICROFRONTENDS_MISSING_DEFAULT_APP",
            MfeError::InvalidRoute { .. } => "MICROFRONTENDS_INVALID_ROUTE",
            MfeError::RouteConflict { .. } => "MICROFRONTENDS_ROUTE_CONFLICT",
            MfeError::MissingFallback => "MICROFRONTENDS_MISSING_FALLBACK",
            MfeError::Unauthorized(_) => "MICROFRONTENDS_UNAUTHORIZED",
            MfeError::TargetDeploymentNotFound { .. } => {
                "MICROFRONTENDS_TARGET_DEPLOYMENT_NOT_FOUND"
            }
            MfeError::ChildMissingRoute { .. } => "MICROFRONTENDS_INVALID_ROUTE",
            MfeError::AssetPrefixWithoutRoute { .. } => "MICROFRONTENDS_INVALID_ROUTE",
            MfeError::InvalidDefaultRoute { .. } => "MICROFRONTENDS_INVALID_ROUTE",
            MfeError::UnknownMember(_) => "MICROFRONTENDS_GROUP_NOT_FOUND",
        }
    }
    pub fn message(&self) -> String {
        match self {
            MfeError::GroupNotFound(id) => format!("microfrontend group '{id}' not found"),
            MfeError::ProjectAlreadyInGroup { project, group } => {
                format!("project '{project}' already belongs to microfrontend group '{group}'")
            }
            MfeError::MissingDefaultApp => {
                "a microfrontend group must have exactly one default application".into()
            }
            MfeError::MultipleDefaultApps => {
                "a microfrontend group must have exactly one default application".into()
            }
            MfeError::InvalidRoute { path, reason } => format!("invalid route '{path}': {reason}"),
            MfeError::RouteConflict { a, b } => {
                format!("route '{a}' conflicts with '{b}' (cannot uniquely resolve priority)")
            }
            MfeError::MissingFallback => {
                "custom fallback environment requires customFallbackEnvironmentName".into()
            }
            MfeError::Unauthorized(m) => m.clone(),
            MfeError::TargetDeploymentNotFound { project } => {
                format!("no deployment found for project '{project}'")
            }
            MfeError::ChildMissingRoute { project } => {
                format!("child project '{project}' must declare at least one route")
            }
            MfeError::AssetPrefixWithoutRoute {
                project,
                asset_prefix,
            } => {
                format!("project '{project}' sets assetPrefix '{asset_prefix}' but has no matching route '/{asset_prefix}/:path*'")
            }
            MfeError::InvalidDefaultRoute { project, value } => {
                format!("defaultRoute '{value}' for project '{project}' must start with '/'")
            }
            MfeError::UnknownMember(p) => format!("project '{p}' is not a member of this group"),
        }
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One route rule: a set of path patterns, optionally scoped to a routing group
/// label and/or gated behind a feature flag (both are metadata carried through
/// to the generated config; the platform router matches on `paths`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MfeRoute {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Local-development settings for a member (Vercel `development` block).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MfeDevelopment {
    /// Local port or host the member runs on during `mfe dev` (string OR number in
    /// Vercel's schema; stored as a string, emitted as a number when numeric).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// A fallback origin (e.g. `example.com`) used when the member is not running
    /// locally. Vercel places this on the DEFAULT application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// One project's membership in a group (both the default app and each child).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MfeMembership {
    pub project: String,
    /// "default" | "child". Derived from the group's `host_project`, but stored
    /// explicitly so the API/UI can render it without cross-referencing.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub routing: Vec<MfeRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_prefix: Option<String>,
    #[serde(default = "default_observability")]
    pub observability_routing: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub development: Option<MfeDevelopment>,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub updated_ms: u64,
}

impl MfeMembership {
    pub fn is_default(&self) -> bool {
        self.role == "default"
    }
    /// Every path across every route rule, flattened.
    pub fn all_paths(&self) -> Vec<String> {
        self.routing
            .iter()
            .flat_map(|r| r.paths.iter().cloned())
            .collect()
    }
}

/// Legacy child shape (`{project, path_prefix}`) kept ONLY so previously-persisted
/// groups deserialize; migrated to `members` by [`MfeGroup::normalized`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MfeChild {
    pub project: String,
    pub path_prefix: String,
}

/// A microfrontend group: one logical app composed of a default project plus
/// child projects, each owning path routes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MfeGroup {
    pub id: String,
    pub name: String,
    /// The default/host project id (serves `/` and anything unmatched). Equals the
    /// `project` of the member whose `role == "default"` — Vercel's `defaultApp`.
    pub host_project: String,
    /// All members (default + children). New source of truth.
    #[serde(default)]
    pub members: Vec<MfeMembership>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// "same_environment" | "production" | "custom".
    #[serde(default = "default_fallback_env")]
    pub fallback_environment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_fallback_environment_name: Option<String>,
    #[serde(default)]
    pub disable_overrides: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_proxy_port: Option<u32>,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub updated_ms: u64,
    /// Legacy children, migrated into `members` on read. Never re-serialized.
    #[serde(default, skip_serializing)]
    pub children: Vec<MfeChild>,
}

impl MfeGroup {
    /// A fresh group with only the default (host) member, sensible defaults, and
    /// caller-supplied timestamps (the pure model never reads the clock).
    pub fn new(id: String, name: String, host_project: String, now_ms: u64) -> MfeGroup {
        MfeGroup {
            id,
            name,
            host_project: host_project.clone(),
            members: vec![MfeMembership {
                project: host_project,
                role: "default".into(),
                routing: vec![],
                default_route: None,
                package_name: None,
                asset_prefix: None,
                observability_routing: default_observability(),
                development: None,
                created_ms: now_ms,
                updated_ms: now_ms,
            }],
            enabled: true,
            fallback_environment: default_fallback_env(),
            custom_fallback_environment_name: None,
            disable_overrides: false,
            local_proxy_port: None,
            created_ms: now_ms,
            updated_ms: now_ms,
            children: vec![],
        }
    }

    /// Migrate any legacy `children` into `members`, ensure the default member
    /// exists, and stamp `role` on every member. Idempotent.
    pub fn normalized(mut self) -> MfeGroup {
        if self.members.is_empty() && !self.children.is_empty() {
            let mut members = Vec::new();
            members.push(MfeMembership {
                project: self.host_project.clone(),
                role: "default".into(),
                routing: vec![],
                default_route: None,
                package_name: None,
                asset_prefix: None,
                observability_routing: default_observability(),
                development: None,
                created_ms: self.created_ms,
                updated_ms: self.created_ms,
            });
            for ch in &self.children {
                let p = ch.path_prefix.trim_end_matches('/').to_string();
                let paths = if p.is_empty() {
                    vec![]
                } else {
                    vec![p.clone(), format!("{p}/:path*")]
                };
                members.push(MfeMembership {
                    project: ch.project.clone(),
                    role: "child".into(),
                    routing: vec![MfeRoute {
                        group: None,
                        flag: None,
                        paths,
                    }],
                    default_route: None,
                    package_name: None,
                    asset_prefix: None,
                    observability_routing: default_observability(),
                    development: None,
                    created_ms: self.created_ms,
                    updated_ms: self.created_ms,
                });
            }
            self.members = members;
        }
        self.children.clear();
        // Stamp roles from host_project so `role` is always authoritative.
        for m in &mut self.members {
            m.role = if m.project == self.host_project {
                "default".into()
            } else {
                "child".into()
            };
        }
        self
    }

    pub fn default_member(&self) -> Option<&MfeMembership> {
        self.members.iter().find(|m| m.project == self.host_project)
    }
    pub fn child_members(&self) -> impl Iterator<Item = &MfeMembership> {
        self.members
            .iter()
            .filter(move |m| m.project != self.host_project)
    }
    pub fn member(&self, project: &str) -> Option<&MfeMembership> {
        self.members.iter().find(|m| m.project == project)
    }
}

// ---------------------------------------------------------------------------
// Path matcher (Vercel-style patterns)
// ---------------------------------------------------------------------------

/// One compiled path-pattern segment.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    /// Exact segment text.
    Literal(String),
    /// `:name` — exactly one non-empty segment.
    Param,
    /// `:name*` — zero or more trailing segments (must be the last segment).
    CatchAllStar,
    /// `:name+` — one or more trailing segments (must be the last segment).
    CatchAllPlus,
    /// `:name(a|b|c)` — one segment equal to one of the alternates.
    Alt(Vec<String>),
    /// `prefix-:name-suffix` — one segment matching literal `prefix` + non-empty
    /// middle + literal `suffix` (either affix may be empty but not both).
    Affix { prefix: String, suffix: String },
}

/// A compiled route pattern with a precomputed specificity score (higher = more
/// specific), used to deterministically resolve which child owns an overlapping
/// path (e.g. `/docs/api/:path*` beats `/docs/:path*`).
#[derive(Clone, Debug)]
pub struct RoutePattern {
    raw: String,
    segs: Vec<Seg>,
    specificity: i64,
}

/// Parse a single pattern segment string.
fn parse_seg(s: &str) -> Result<Seg, String> {
    // Catch-alls: `:name*` / `:name+`.
    if let Some(rest) = s.strip_prefix(':') {
        if let Some(name) = rest.strip_suffix('*') {
            if is_ident(name) {
                return Ok(Seg::CatchAllStar);
            }
        }
        if let Some(name) = rest.strip_suffix('+') {
            if is_ident(name) {
                return Ok(Seg::CatchAllPlus);
            }
        }
        // Alternates: `:name(a|b)`.
        if let Some(open) = rest.find('(') {
            if rest.ends_with(')') {
                let name = &rest[..open];
                let inner = &rest[open + 1..rest.len() - 1];
                if is_ident(name) && !inner.is_empty() {
                    let alts: Vec<String> = inner.split('|').map(|x| x.to_string()).collect();
                    if alts.iter().all(|a| !a.is_empty()) {
                        return Ok(Seg::Alt(alts));
                    }
                }
            }
        }
        // Plain `:name`.
        if is_ident(rest) {
            return Ok(Seg::Param);
        }
        return Err(format!("malformed parameter segment ':{rest}'"));
    }
    // Affix: literal text containing a single `:name` somewhere inside a segment,
    // e.g. `prefix-:path-suffix` or `v-:ver`.
    if let Some(colon) = s.find(':') {
        let prefix = s[..colon].to_string();
        let after = &s[colon + 1..];
        // Read the identifier, then the suffix is the literal remainder.
        let id_end = after
            .find(|c: char| !is_ident_char(c))
            .unwrap_or(after.len());
        let name = &after[..id_end];
        let suffix = after[id_end..].to_string();
        if is_ident(name) && (!prefix.is_empty() || !suffix.is_empty()) && !suffix.contains(':') {
            return Ok(Seg::Affix { prefix, suffix });
        }
        return Err(format!("malformed segment '{s}'"));
    }
    Ok(Seg::Literal(s.to_string()))
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_ident_char)
}
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl RoutePattern {
    /// Compile a Vercel-style pattern. Errors on syntax the matcher can't honor.
    pub fn compile(pattern: &str) -> Result<RoutePattern, MfeError> {
        if !pattern.starts_with('/') {
            return Err(MfeError::InvalidRoute {
                path: pattern.to_string(),
                reason: "path must start with '/'".into(),
            });
        }
        let body = pattern.trim_start_matches('/');
        let raw_segs: Vec<&str> = if body.is_empty() {
            vec![]
        } else {
            body.split('/').collect()
        };
        let mut segs = Vec::with_capacity(raw_segs.len());
        for (i, rs) in raw_segs.iter().enumerate() {
            if rs.is_empty() {
                return Err(MfeError::InvalidRoute {
                    path: pattern.to_string(),
                    reason: "empty path segment ('//')".into(),
                });
            }
            let seg = parse_seg(rs).map_err(|reason| MfeError::InvalidRoute {
                path: pattern.to_string(),
                reason,
            })?;
            if matches!(seg, Seg::CatchAllStar | Seg::CatchAllPlus) && i != raw_segs.len() - 1 {
                return Err(MfeError::InvalidRoute {
                    path: pattern.to_string(),
                    reason: "catch-all (:path* / :path+) must be the final segment".into(),
                });
            }
            segs.push(seg);
        }
        let specificity = segs
            .iter()
            .map(|s| match s {
                Seg::Literal(t) => 100 + t.len() as i64,
                Seg::Affix { prefix, suffix } => 30 + (prefix.len() + suffix.len()) as i64,
                Seg::Alt(_) => 50,
                Seg::Param => 10,
                Seg::CatchAllPlus => 5,
                Seg::CatchAllStar => 1,
            })
            .sum();
        Ok(RoutePattern {
            raw: pattern.to_string(),
            segs,
            specificity,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
    pub fn specificity(&self) -> i64 {
        self.specificity
    }

    /// Does this pattern match `path`? `path` must begin with `/`.
    pub fn matches(&self, path: &str) -> bool {
        let path = path.split(['?', '#']).next().unwrap_or(path);
        let body = path.trim_start_matches('/');
        let segs: Vec<&str> = if body.is_empty() {
            vec![]
        } else {
            body.split('/').collect()
        };
        match_segs(&self.segs, &segs)
    }
}

/// Match compiled pattern segments against concrete path segments.
fn match_segs(pat: &[Seg], path: &[&str]) -> bool {
    let mut pi = 0;
    let mut si = 0;
    while pi < pat.len() {
        match &pat[pi] {
            Seg::CatchAllStar => return true, // last seg by construction; matches remaining (incl. none)
            Seg::CatchAllPlus => return si < path.len(), // needs >= 1 remaining
            _ => {}
        }
        let Some(seg) = path.get(si) else {
            return false;
        };
        let ok = match &pat[pi] {
            Seg::Literal(t) => seg == t,
            Seg::Param => !seg.is_empty(),
            Seg::Alt(alts) => alts.iter().any(|a| a == seg),
            Seg::Affix { prefix, suffix } => {
                seg.len() > prefix.len() + suffix.len()
                    && seg.starts_with(prefix.as_str())
                    && seg.ends_with(suffix.as_str())
            }
            Seg::CatchAllStar | Seg::CatchAllPlus => unreachable!(),
        };
        if !ok {
            return false;
        }
        pi += 1;
        si += 1;
    }
    si == path.len()
}

// ---------------------------------------------------------------------------
// Routing resolution
// ---------------------------------------------------------------------------

/// The child project (if any) that owns `path` in this group, resolving overlap
/// by specificity (most specific wins; ties broken by project name for
/// determinism — validation rejects true equal-specificity conflicts up front).
/// Returns `(project, matched_pattern)`. `None` => the default app serves it.
pub fn resolve_child(group: &MfeGroup, path: &str) -> Option<(String, String)> {
    let mut best: Option<(i64, String, String)> = None;
    for m in group.child_members() {
        for route in &m.routing {
            for pat in &route.paths {
                if let Ok(rp) = RoutePattern::compile(pat) {
                    if rp.matches(path) {
                        let score = rp.specificity();
                        let better = match &best {
                            None => true,
                            Some((bs, bp, _)) => score > *bs || (score == *bs && &m.project < bp),
                        };
                        if better {
                            best = Some((score, m.project.clone(), pat.clone()));
                        }
                    }
                }
            }
        }
    }
    best.map(|(_, project, pat)| (project, pat))
}

// ---------------------------------------------------------------------------
// Conflict detection
// ---------------------------------------------------------------------------

/// Build a representative concrete path for a pattern (params -> a placeholder
/// segment, catch-alls -> one placeholder segment). Used to probe cross-pattern
/// overlap. Returns `None` for a root-only (`/`) pattern.
fn probe_path(rp: &RoutePattern) -> Option<String> {
    if rp.segs.is_empty() {
        return Some("/".into());
    }
    let mut out = String::new();
    for seg in &rp.segs {
        out.push('/');
        match seg {
            Seg::Literal(t) => out.push_str(t),
            Seg::Alt(alts) => out.push_str(&alts[0]),
            Seg::Affix { prefix, suffix } => {
                out.push_str(prefix);
                out.push('x');
                out.push_str(suffix);
            }
            Seg::Param | Seg::CatchAllStar | Seg::CatchAllPlus => out.push_str("seg"),
        }
    }
    Some(out)
}

/// Two patterns CONFLICT when some path matches BOTH with EQUAL specificity — the
/// router then cannot deterministically pick an owner. Nested patterns of
/// differing specificity (`/docs/:path*` vs `/docs/api/:path*`) do NOT conflict
/// (the more specific one wins). Probes both patterns' representative paths.
pub fn patterns_conflict(a: &RoutePattern, b: &RoutePattern) -> bool {
    if a.specificity() != b.specificity() {
        // Differing specificity is always resolvable, even if paths overlap.
        return false;
    }
    for probe in [probe_path(a), probe_path(b)].into_iter().flatten() {
        if a.matches(&probe) && b.matches(&probe) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Full structural + routing validation of a group. Returns the FIRST error.
pub fn validate_group(group: &MfeGroup) -> Result<(), MfeError> {
    // Exactly one default application.
    let defaults = group
        .members
        .iter()
        .filter(|m| m.project == group.host_project)
        .count();
    match defaults {
        0 => return Err(MfeError::MissingDefaultApp),
        1 => {}
        _ => return Err(MfeError::MultipleDefaultApps),
    }

    // Fallback policy.
    if group.fallback_environment == "custom"
        && group
            .custom_fallback_environment_name
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        return Err(MfeError::MissingFallback);
    }

    // Per-member checks + collect (project, compiled-pattern) for conflict pass.
    let mut compiled: Vec<(String, RoutePattern)> = Vec::new();
    for m in &group.members {
        let is_default = m.project == group.host_project;

        if let Some(dr) = m.default_route.as_deref() {
            if !dr.is_empty() && !dr.starts_with('/') {
                return Err(MfeError::InvalidDefaultRoute {
                    project: m.project.clone(),
                    value: dr.to_string(),
                });
            }
        }

        let paths = m.all_paths();
        if !is_default && paths.is_empty() {
            return Err(MfeError::ChildMissingRoute {
                project: m.project.clone(),
            });
        }

        for p in &paths {
            let rp = RoutePattern::compile(p)?; // InvalidRoute on bad syntax / missing leading '/'
            compiled.push((m.project.clone(), rp));
        }

        // assetPrefix requires a matching "/{prefix}/:path*" route on the SAME member.
        if let Some(ap) = m.asset_prefix.as_deref() {
            let ap = ap.trim().trim_matches('/');
            if !ap.is_empty() {
                let want = format!("/{ap}/:path*");
                let has = paths
                    .iter()
                    .any(|p| normalize_pattern(p) == normalize_pattern(&want));
                if !has {
                    return Err(MfeError::AssetPrefixWithoutRoute {
                        project: m.project.clone(),
                        asset_prefix: ap.to_string(),
                    });
                }
            }
        }
    }

    // Cross-child conflict detection (routes from DIFFERENT members only —
    // a member overlapping itself is not a conflict).
    for i in 0..compiled.len() {
        for j in (i + 1)..compiled.len() {
            if compiled[i].0 == compiled[j].0 {
                continue;
            }
            if patterns_conflict(&compiled[i].1, &compiled[j].1) {
                return Err(MfeError::RouteConflict {
                    a: compiled[i].1.as_str().to_string(),
                    b: compiled[j].1.as_str().to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Can `project` be removed from `group`? The default app cannot be removed while
/// child apps remain (they would be orphaned) — remove/reassign children or
/// promote a new default first. Removing the last remaining member (the default
/// alone) IS allowed (the caller then deletes the empty group).
pub fn can_remove_member(group: &MfeGroup, project: &str) -> Result<(), MfeError> {
    if group.member(project).is_none() {
        return Err(MfeError::UnknownMember(project.to_string()));
    }
    let is_default = group.host_project == project;
    let child_count = group
        .members
        .iter()
        .filter(|m| m.project != group.host_project)
        .count();
    if is_default && child_count > 0 {
        return Err(MfeError::MissingDefaultApp);
    }
    Ok(())
}

/// Canonical form of a pattern for equality (collapse `:name`/`:name*` param
/// names so `/a/:x*` == `/a/:y*`).
fn normalize_pattern(p: &str) -> String {
    p.trim_end_matches('/')
        .split('/')
        .map(|s| {
            if let Some(rest) = s.strip_prefix(':') {
                if rest.ends_with('*') {
                    ":*".to_string()
                } else if rest.ends_with('+') {
                    ":+".to_string()
                } else {
                    ":".to_string()
                }
            } else {
                s.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Config generation (Vercel-compatible-ish)
// ---------------------------------------------------------------------------

/// Emit the `microfrontends.json`-shaped config for a group. Applications are
/// keyed by project name; the default app carries `development.fallback`, each
/// child carries `routing` + optional `assetPrefix`/`development`.
pub fn to_vercel_config(group: &MfeGroup) -> serde_json::Value {
    use serde_json::{json, Map, Value};
    let mut applications = Map::new();
    for m in &group.members {
        let mut app = Map::new();
        if !m.is_default() {
            let routing: Vec<Value> = m
                .routing
                .iter()
                .filter(|r| !r.paths.is_empty())
                .map(|r| {
                    let mut o = Map::new();
                    o.insert("paths".into(), json!(r.paths));
                    if let Some(g) = &r.group {
                        o.insert("group".into(), json!(g));
                    }
                    if let Some(f) = &r.flag {
                        o.insert("flag".into(), json!(f));
                    }
                    Value::Object(o)
                })
                .collect();
            if !routing.is_empty() {
                app.insert("routing".into(), Value::Array(routing));
            }
            if let Some(ap) = m.asset_prefix.as_deref().filter(|s| !s.trim().is_empty()) {
                app.insert("assetPrefix".into(), json!(ap));
            }
        }
        if let Some(pn) = m.package_name.as_deref().filter(|s| !s.trim().is_empty()) {
            app.insert("packageName".into(), json!(pn));
        }
        if let Some(dr) = m.default_route.as_deref().filter(|s| !s.trim().is_empty()) {
            app.insert("defaultRoute".into(), json!(dr));
        }
        if m.observability_routing == "this_project" {
            app.insert("observabilityRouting".into(), json!("this_project"));
        }
        if let Some(dev) = &m.development {
            let mut d = Map::new();
            if let Some(local) = dev.local.as_deref().filter(|s| !s.trim().is_empty()) {
                // Emit as a number when it parses as one (Vercel accepts either).
                match local.parse::<u64>() {
                    Ok(n) => d.insert("local".into(), json!(n)),
                    Err(_) => d.insert("local".into(), json!(local)),
                };
            }
            if let Some(task) = dev.task.as_deref().filter(|s| !s.trim().is_empty()) {
                d.insert("task".into(), json!(task));
            }
            if let Some(fb) = dev.fallback.as_deref().filter(|s| !s.trim().is_empty()) {
                d.insert("fallback".into(), json!(fb));
            }
            if !d.is_empty() {
                app.insert("development".into(), Value::Object(d));
            }
        }
        applications.insert(m.project.clone(), Value::Object(app));
    }

    let mut options = Map::new();
    if let Some(port) = group.local_proxy_port {
        options.insert("localProxyPort".into(), json!(port));
    }
    options.insert("disableOverrides".into(), json!(group.disable_overrides));

    json!({
        "$schema": "https://openapi.vercel.sh/microfrontends.json",
        "applications": Value::Object(applications),
        "options": Value::Object(options),
    })
}

// ---------------------------------------------------------------------------
// Fallback-environment resolution
// ---------------------------------------------------------------------------

/// A candidate deployment the resolver can select for a child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployCand {
    pub id: String,
    /// "production" | "preview".
    pub env: String,
    /// The commit sha this deployment was built from (empty if none).
    pub commit: String,
    /// Whether this deployment currently holds the project's production alias.
    pub production: bool,
}

/// The chosen routing target for a child, plus which fallback (if any) applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildTarget {
    pub project: String,
    /// The deployment id to route to.
    pub deployment_id: String,
    /// Which fallback policy fired: "" (exact), "same_environment",
    /// "production", or "custom:<name>".
    pub fallback: String,
}

/// Resolve which of a child project's deployments should serve a request.
///
/// * Production requests ALWAYS route to the child's production deployment.
/// * Preview/custom requests prefer the child's deployment for the SAME commit;
///   absent that, apply the group's `fallback_environment`:
///     - `same_environment`: newest preview deployment of the child, else prod;
///     - `production`: the child's production deployment;
///     - `custom`: a deployment tagged with the custom environment name (matched
///       against `commit`), else production.
///
/// `cands` is every deployment of the child project (newest first is not
/// required; production is identified by `production == true`).
pub fn resolve_child_target(
    group: &MfeGroup,
    child_project: &str,
    request_env: &str, // "production" | "preview"
    request_commit: &str,
    cands: &[DeployCand],
) -> Result<ChildTarget, MfeError> {
    let production = cands
        .iter()
        .find(|c| c.production)
        .or_else(|| cands.iter().find(|c| c.env == "production"));

    if request_env == "production" {
        return production
            .map(|c| ChildTarget {
                project: child_project.into(),
                deployment_id: c.id.clone(),
                fallback: String::new(),
            })
            .ok_or(MfeError::TargetDeploymentNotFound {
                project: child_project.into(),
            });
    }

    // Preview/custom: exact same-commit deployment wins with no fallback.
    if !request_commit.is_empty() {
        if let Some(exact) = cands.iter().find(|c| c.commit == request_commit) {
            return Ok(ChildTarget {
                project: child_project.into(),
                deployment_id: exact.id.clone(),
                fallback: String::new(),
            });
        }
    }

    match group.fallback_environment.as_str() {
        "same_environment" => {
            let preview = cands.iter().find(|c| c.env == "preview" && !c.production);
            let chosen = preview.or(production);
            chosen
                .map(|c| ChildTarget {
                    project: child_project.into(),
                    deployment_id: c.id.clone(),
                    fallback: "same_environment".into(),
                })
                .ok_or(MfeError::TargetDeploymentNotFound {
                    project: child_project.into(),
                })
        }
        "custom" => {
            let name = group
                .custom_fallback_environment_name
                .as_deref()
                .unwrap_or_default();
            let custom = cands.iter().find(|c| !name.is_empty() && c.commit == name);
            let chosen = custom.or(production);
            chosen
                .map(|c| ChildTarget {
                    project: child_project.into(),
                    deployment_id: c.id.clone(),
                    fallback: format!("custom:{name}"),
                })
                .ok_or(MfeError::TargetDeploymentNotFound {
                    project: child_project.into(),
                })
        }
        // "production" (default): route to the child's production deployment.
        _ => production
            .map(|c| ChildTarget {
                project: child_project.into(),
                deployment_id: c.id.clone(),
                fallback: "production".into(),
            })
            .ok_or(MfeError::TargetDeploymentNotFound {
                project: child_project.into(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(project: &str, paths: &[&str]) -> MfeMembership {
        MfeMembership {
            project: project.into(),
            role: "child".into(),
            routing: vec![MfeRoute {
                group: None,
                flag: None,
                paths: paths.iter().map(|s| s.to_string()).collect(),
            }],
            default_route: None,
            package_name: None,
            asset_prefix: None,
            observability_routing: default_observability(),
            development: None,
            created_ms: 0,
            updated_ms: 0,
        }
    }
    fn group(host: &str, members: Vec<MfeMembership>) -> MfeGroup {
        let mut m = vec![MfeMembership {
            project: host.into(),
            role: "default".into(),
            routing: vec![],
            default_route: None,
            package_name: None,
            asset_prefix: None,
            observability_routing: default_observability(),
            development: None,
            created_ms: 0,
            updated_ms: 0,
        }];
        m.extend(members);
        MfeGroup {
            id: "mfe_test".into(),
            name: "app".into(),
            host_project: host.into(),
            members: m,
            enabled: true,
            fallback_environment: "production".into(),
            custom_fallback_environment_name: None,
            disable_overrides: false,
            local_proxy_port: None,
            created_ms: 0,
            updated_ms: 0,
            children: vec![],
        }
    }

    // ---- Path matcher ----
    #[test]
    fn matcher_literal_and_single_param() {
        assert!(RoutePattern::compile("/docs").unwrap().matches("/docs"));
        assert!(!RoutePattern::compile("/docs").unwrap().matches("/docs/x"));
        assert!(!RoutePattern::compile("/docs").unwrap().matches("/other"));
        let p = RoutePattern::compile("/blog/:slug").unwrap();
        assert!(p.matches("/blog/hello"));
        assert!(!p.matches("/blog")); // param needs exactly one segment
        assert!(!p.matches("/blog/a/b"));
    }

    #[test]
    fn matcher_catch_all_star_and_plus() {
        let star = RoutePattern::compile("/docs/:path*").unwrap();
        assert!(star.matches("/docs")); // zero trailing
        assert!(star.matches("/docs/a"));
        assert!(star.matches("/docs/a/b/c"));
        assert!(!star.matches("/other"));
        let plus = RoutePattern::compile("/docs/:path+").unwrap();
        assert!(!plus.matches("/docs")); // needs >=1 trailing
        assert!(plus.matches("/docs/a"));
        assert!(plus.matches("/docs/a/b"));
    }

    #[test]
    fn matcher_affix_and_alternates() {
        let affix = RoutePattern::compile("/file-:name-v1").unwrap();
        assert!(affix.matches("/file-report-v1"));
        assert!(!affix.matches("/file--v1")); // empty middle rejected
        assert!(!affix.matches("/report"));
        let alt = RoutePattern::compile("/:path(a|b)").unwrap();
        assert!(alt.matches("/a"));
        assert!(alt.matches("/b"));
        assert!(!alt.matches("/c"));
    }

    #[test]
    fn matcher_rejects_catchall_not_last() {
        assert!(RoutePattern::compile("/docs/:path*/more").is_err());
        assert!(RoutePattern::compile("docs").is_err()); // missing leading '/'
        assert!(RoutePattern::compile("/a//b").is_err()); // empty segment
    }

    #[test]
    fn matcher_ignores_query_and_fragment() {
        assert!(RoutePattern::compile("/docs/:path*")
            .unwrap()
            .matches("/docs/page?x=1"));
        assert!(RoutePattern::compile("/docs").unwrap().matches("/docs?y=2"));
    }

    // ---- Specificity / resolution ----
    #[test]
    fn nested_prefixes_resolve_by_specificity() {
        let g = group(
            "web",
            vec![
                child("docs", &["/docs/:path*"]),
                child("api", &["/docs/api/:path*"]),
            ],
        );
        // /docs/api/x matches both; the more specific (2 literal segs) wins.
        assert_eq!(
            resolve_child(&g, "/docs/api/users").map(|(p, _)| p),
            Some("api".into())
        );
        // /docs/guide matches only the docs child.
        assert_eq!(
            resolve_child(&g, "/docs/guide").map(|(p, _)| p),
            Some("docs".into())
        );
        // / is unmatched -> default app.
        assert_eq!(resolve_child(&g, "/"), None);
        assert_eq!(resolve_child(&g, "/pricing"), None);
    }

    // ---- Conflict detection ----
    #[test]
    fn conflict_identical_patterns_across_children() {
        let a = RoutePattern::compile("/shop/:path*").unwrap();
        let b = RoutePattern::compile("/shop/:path*").unwrap();
        assert!(patterns_conflict(&a, &b));
    }
    #[test]
    fn no_conflict_when_specificity_differs() {
        let a = RoutePattern::compile("/docs/:path*").unwrap();
        let b = RoutePattern::compile("/docs/api/:path*").unwrap();
        assert!(!patterns_conflict(&a, &b));
    }
    #[test]
    fn conflict_equal_specificity_overlap() {
        // Same shape, both single-param first segment -> equal specificity + overlap.
        let a = RoutePattern::compile("/:a*").unwrap();
        let b = RoutePattern::compile("/:b*").unwrap();
        assert!(patterns_conflict(&a, &b));
    }

    // ---- Validation ----
    #[test]
    fn validate_requires_single_default_and_child_routes() {
        let mut g = group("web", vec![child("docs", &["/docs/:path*"])]);
        assert!(validate_group(&g).is_ok());

        // Child with no routes -> error.
        let g2 = group("web", vec![child("docs", &[])]);
        assert_eq!(
            validate_group(&g2).unwrap_err().code(),
            "MICROFRONTENDS_INVALID_ROUTE"
        );

        // Missing default (host_project not among members).
        g.host_project = "ghost".into();
        assert_eq!(validate_group(&g).unwrap_err(), MfeError::MissingDefaultApp);
    }

    #[test]
    fn validate_rejects_bad_route_syntax_and_conflicts() {
        let g = group("web", vec![child("docs", &["docs/:path*"])]); // missing leading '/'
        assert_eq!(
            validate_group(&g).unwrap_err().code(),
            "MICROFRONTENDS_INVALID_ROUTE"
        );

        let g2 = group(
            "web",
            vec![child("a", &["/shop/:path*"]), child("b", &["/shop/:path*"])],
        );
        assert_eq!(
            validate_group(&g2).unwrap_err().code(),
            "MICROFRONTENDS_ROUTE_CONFLICT"
        );
    }

    #[test]
    fn validate_asset_prefix_requires_route() {
        let mut m = child("docs", &["/docs/:path*"]);
        m.asset_prefix = Some("docs-assets".into());
        let g = group("web", vec![m]);
        assert_eq!(
            validate_group(&g).unwrap_err().code(),
            "MICROFRONTENDS_INVALID_ROUTE"
        );

        let mut m2 = child("docs", &["/docs/:path*", "/docs-assets/:path*"]);
        m2.asset_prefix = Some("docs-assets".into());
        let g2 = group("web", vec![m2]);
        assert!(validate_group(&g2).is_ok());
    }

    #[test]
    fn validate_custom_fallback_requires_name() {
        let mut g = group("web", vec![child("docs", &["/docs/:path*"])]);
        g.fallback_environment = "custom".into();
        assert_eq!(validate_group(&g).unwrap_err(), MfeError::MissingFallback);
        g.custom_fallback_environment_name = Some("staging".into());
        assert!(validate_group(&g).is_ok());
    }

    #[test]
    fn validate_default_route_must_start_with_slash() {
        let mut m = child("docs", &["/docs/:path*"]);
        m.default_route = Some("docs/home".into());
        let g = group("web", vec![m]);
        assert_eq!(
            validate_group(&g).unwrap_err().code(),
            "MICROFRONTENDS_INVALID_ROUTE"
        );
    }

    // ---- Config generation ----
    #[test]
    fn config_matches_vercel_shape() {
        let mut docs = child("docs", &["/docs/:path*"]);
        docs.asset_prefix = Some("docs-assets".into());
        docs.routing[0]
            .paths
            .push("/docs-assets/:path*".to_string());
        docs.development = Some(MfeDevelopment {
            local: Some("3001".into()),
            task: Some("dev".into()),
            fallback: None,
        });
        let mut g = group("web", vec![docs]);
        g.local_proxy_port = Some(3024);
        // default app fallback origin
        g.members[0].development = Some(MfeDevelopment {
            local: None,
            task: None,
            fallback: Some("example.com".into()),
        });

        let cfg = to_vercel_config(&g);
        assert_eq!(
            cfg["$schema"],
            "https://openapi.vercel.sh/microfrontends.json"
        );
        assert_eq!(cfg["options"]["localProxyPort"], 3024);
        assert_eq!(cfg["options"]["disableOverrides"], false);
        assert_eq!(
            cfg["applications"]["web"]["development"]["fallback"],
            "example.com"
        );
        assert_eq!(cfg["applications"]["docs"]["assetPrefix"], "docs-assets");
        assert_eq!(cfg["applications"]["docs"]["development"]["local"], 3001); // numeric
        assert_eq!(cfg["applications"]["docs"]["development"]["task"], "dev");
        assert_eq!(
            cfg["applications"]["docs"]["routing"][0]["paths"][0],
            "/docs/:path*"
        );
        // Default app has no routing key.
        assert!(cfg["applications"]["web"].get("routing").is_none());
    }

    // ---- Fallback resolution ----
    fn cand(id: &str, env: &str, commit: &str, prod: bool) -> DeployCand {
        DeployCand {
            id: id.into(),
            env: env.into(),
            commit: commit.into(),
            production: prod,
        }
    }

    #[test]
    fn production_requests_always_route_to_production() {
        let g = group("web", vec![child("docs", &["/docs/:path*"])]);
        let cands = vec![
            cand("dpl_prod", "production", "aaa", true),
            cand("dpl_prev", "preview", "bbb", false),
        ];
        let t = resolve_child_target(&g, "docs", "production", "bbb", &cands).unwrap();
        assert_eq!(t.deployment_id, "dpl_prod");
        assert_eq!(t.fallback, "");
    }

    #[test]
    fn preview_same_commit_wins_without_fallback() {
        let g = group("web", vec![child("docs", &["/docs/:path*"])]);
        let cands = vec![
            cand("dpl_prod", "production", "aaa", true),
            cand("dpl_c", "preview", "ccc", false),
        ];
        let t = resolve_child_target(&g, "docs", "preview", "ccc", &cands).unwrap();
        assert_eq!(t.deployment_id, "dpl_c");
        assert_eq!(t.fallback, "");
    }

    #[test]
    fn preview_missing_commit_falls_back_to_production() {
        let mut g = group("web", vec![child("docs", &["/docs/:path*"])]);
        g.fallback_environment = "production".into();
        let cands = vec![cand("dpl_prod", "production", "aaa", true)];
        let t = resolve_child_target(&g, "docs", "preview", "zzz", &cands).unwrap();
        assert_eq!(t.deployment_id, "dpl_prod");
        assert_eq!(t.fallback, "production");
    }

    #[test]
    fn preview_same_environment_fallback_prefers_preview() {
        let mut g = group("web", vec![child("docs", &["/docs/:path*"])]);
        g.fallback_environment = "same_environment".into();
        let cands = vec![
            cand("dpl_prod", "production", "aaa", true),
            cand("dpl_prev", "preview", "bbb", false),
        ];
        let t = resolve_child_target(&g, "docs", "preview", "zzz", &cands).unwrap();
        assert_eq!(t.deployment_id, "dpl_prev");
        assert_eq!(t.fallback, "same_environment");
    }

    #[test]
    fn missing_target_deployment_errors() {
        let g = group("web", vec![child("docs", &["/docs/:path*"])]);
        let err = resolve_child_target(&g, "docs", "production", "", &[]).unwrap_err();
        assert_eq!(err.code(), "MICROFRONTENDS_TARGET_DEPLOYMENT_NOT_FOUND");
    }

    // ---- End-to-end group lifecycle (model-level integration) ----
    #[test]
    fn lifecycle_create_add_child_route_remove() {
        // Create a group with default app `web`.
        let mut g = MfeGroup::new("mfe_1".into(), "storefront".into(), "web".into(), 100);
        assert!(validate_group(&g).is_ok());
        assert_eq!(g.default_member().unwrap().project, "web");

        // Add child `docs` with /docs/:path*.
        g.members.push(child("docs", &["/docs/:path*"]));
        g = g.normalized();
        assert!(validate_group(&g).is_ok());

        // Route /docs/page -> docs; / -> default (None).
        assert_eq!(
            resolve_child(&g, "/docs/page").map(|(p, _)| p),
            Some("docs".into())
        );
        assert_eq!(resolve_child(&g, "/"), None);
        assert_eq!(resolve_child(&g, "/pricing"), None);

        // Cannot remove the default while the child remains.
        assert_eq!(
            can_remove_member(&g, "web").unwrap_err(),
            MfeError::MissingDefaultApp
        );
        // Removing the child is fine.
        assert!(can_remove_member(&g, "docs").is_ok());
        g.members.retain(|m| m.project != "docs");
        assert!(validate_group(&g).is_ok());
        // Now the default alone can be removed.
        assert!(can_remove_member(&g, "web").is_ok());
    }

    #[test]
    fn promotion_reassigns_default() {
        let mut g = MfeGroup::new("mfe_2".into(), "app".into(), "web".into(), 0);
        g.members.push(child("docs", &["/docs/:path*"]));
        g = g.normalized();
        // Promote docs to default.
        g.host_project = "docs".into();
        g = g.normalized();
        assert_eq!(g.default_member().unwrap().project, "docs");
        assert_eq!(g.member("web").unwrap().role, "child");
        // web now has no route (was default) -> must add one or it's invalid.
        assert_eq!(
            validate_group(&g).unwrap_err().code(),
            "MICROFRONTENDS_INVALID_ROUTE"
        );
    }

    // ---- Legacy migration ----
    #[test]
    fn legacy_children_migrate_to_members() {
        let g = MfeGroup {
            id: "mfe_legacy".into(),
            name: "app".into(),
            host_project: "web".into(),
            members: vec![],
            enabled: true,
            fallback_environment: default_fallback_env(),
            custom_fallback_environment_name: None,
            disable_overrides: false,
            local_proxy_port: None,
            created_ms: 5,
            updated_ms: 0,
            children: vec![MfeChild {
                project: "docs".into(),
                path_prefix: "/docs".into(),
            }],
        }
        .normalized();
        assert_eq!(g.members.len(), 2);
        assert!(g.default_member().is_some());
        let docs = g.member("docs").unwrap();
        assert_eq!(docs.role, "child");
        assert!(docs.routing[0].paths.contains(&"/docs".to_string()));
        assert!(docs.routing[0].paths.contains(&"/docs/:path*".to_string()));
        assert!(g.children.is_empty());
        // And it routes.
        assert_eq!(
            resolve_child(&g, "/docs/x").map(|(p, _)| p),
            Some("docs".into())
        );
    }
}
