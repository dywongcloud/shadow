//! Enterprise feature suite (Vercel-parity): account-level IP blocking, custom
//! SIEM audit-log streaming, SAML SSO, SCIM directory sync, deployment password
//! protection + protection scope ("only production"), microfrontends, and
//! Conformance. All team-scoped, all Enterprise-plan-gated at the API layer, and
//! persisted (secrets AEAD-encrypted at rest via [`crate::secrets`]).
//!
//! One `EnterpriseStore` holds every sub-feature so persistence is a single
//! snapshot/load pair. Enforcement lives where it belongs: IP blocking + the
//! password gate in `edge.rs`, SIEM streaming off `audit.rs::record`, SCIM/SAML
//! provisioning into `teams.rs`, conformance computed from the build/service graph.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use hive_core::now_ms;

// ---------------------------------------------------------------------------
// Account-level IP blocking (Vercel WAF / IP blocking)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpBlock {
    pub id: String,
    /// IPv4/IPv6 address or CIDR-ish prefix (prefix match, e.g. "203.0.113." or
    /// a full address). Matching is a normalized string prefix — good enough for
    /// operator denylists without a full CIDR engine.
    pub prefix: String,
    #[serde(default)]
    pub note: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub created_ms: u64,
}
fn yes() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Custom SIEM log streaming (Audit Log → external SIEM)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SiemConfig {
    /// "http" (generic JSON POST) | "datadog" | "splunk-hec"
    #[serde(default = "http_fmt")]
    pub format: String,
    pub endpoint: String,
    /// Bearer/API token — stored `enc:v1:`-encrypted, never returned in the clear.
    #[serde(default)]
    pub token_enc: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub updated_ms: u64,
    /// Observability counters (not persisted meaningfully — reset on restart).
    #[serde(default)]
    pub delivered: u64,
    #[serde(default)]
    pub failed: u64,
}
fn http_fmt() -> String {
    "http".into()
}

// ---------------------------------------------------------------------------
// SAML SSO
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SamlConfig {
    pub idp_entity_id: String,
    /// IdP SSO (login) URL the SP redirects users to.
    pub sso_url: String,
    /// IdP signing cert (PEM) — public, stored as-is.
    #[serde(default)]
    pub x509_cert: String,
    #[serde(default)]
    pub enabled: bool,
    /// When true, non-SSO sign-in is refused for this team (enforced SSO).
    #[serde(default)]
    pub enforced: bool,
    /// Auto-provision a team Member on first successful assertion.
    #[serde(default = "yes")]
    pub auto_provision: bool,
    #[serde(default)]
    pub updated_ms: u64,
}

// ---------------------------------------------------------------------------
// SCIM 2.0 directory sync
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ScimUser {
    pub id: String,
    pub user_name: String, // usually the email
    #[serde(default)]
    pub display_name: String,
    #[serde(default = "yes")]
    pub active: bool,
    #[serde(default)]
    pub created_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ScimTenant {
    /// SCIM bearer token the IdP presents — `enc:v1:`-encrypted at rest.
    #[serde(default)]
    pub token_enc: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub users: Vec<ScimUser>,
    #[serde(default)]
    pub updated_ms: u64,
}

// ---------------------------------------------------------------------------
// Deployment protection (password + scope; "only production")
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployProtection {
    /// "off" | "standard" (SSO/membership) | "password"
    #[serde(default = "off_mode")]
    pub mode: String,
    /// Which deployments are protected: "preview" (default, all non-prod) |
    /// "all" | "production" (only production deployments — Vercel option).
    #[serde(default = "preview_scope")]
    pub scope: String,
    /// bcrypt-ish hash of the shared password (mode="password"). Never returned.
    #[serde(default)]
    pub password_hash: String,
    #[serde(default)]
    pub updated_ms: u64,
}
fn off_mode() -> String {
    "off".into()
}
fn preview_scope() -> String {
    "preview".into()
}
impl Default for DeployProtection {
    fn default() -> Self {
        DeployProtection { mode: "off".into(), scope: "preview".into(), password_hash: String::new(), updated_ms: 0 }
    }
}
impl DeployProtection {
    /// Does this config protect a deployment whose environment is `target`
    /// ("production"|"preview")?
    pub fn protects(&self, target: &str) -> bool {
        if self.mode == "off" {
            return false;
        }
        let is_prod = target.eq_ignore_ascii_case("production");
        match self.scope.as_str() {
            "all" => true,
            "production" => is_prod,
            _ => !is_prod, // "preview"
        }
    }
}

// ---------------------------------------------------------------------------
// Microfrontends (path-composed multi-project apps)
// ---------------------------------------------------------------------------
// The rich model + path matcher + validation + config generation live in
// `crate::microfrontends`; this store holds + gossips the groups. Re-exported so
// existing `crate::enterprise::{MfeGroup, MfeChild}` import paths keep resolving.
pub use crate::microfrontends::{MfeChild, MfeGroup};

// ---------------------------------------------------------------------------
// Conformance (org-wide project checks)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConformancePolicy {
    /// Rule id -> required (must pass) / advisory. Keyed by our built-in rule ids.
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub updated_ms: u64,
}
impl Default for ConformancePolicy {
    fn default() -> Self {
        // Sensible enterprise defaults — all built-in rules required.
        ConformancePolicy {
            required: RULES.iter().map(|r| r.0.to_string()).collect(),
            enabled: true,
            updated_ms: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConformanceResult {
    pub rule: String,
    pub title: String,
    pub passed: bool,
    pub required: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConformanceReport {
    pub project: String,
    pub deployment_id: String,
    pub results: Vec<ConformanceResult>,
    pub passed: bool, // all REQUIRED rules passed
    pub score: u32,   // % of all rules passed
    pub ran_ms: u64,
}

/// Built-in conformance rules: (id, title). The evaluator (in admin.rs) fills in
/// pass/fail from the deployment's service graph + settings.
pub const RULES: &[(&str, &str)] = &[
    ("https-only", "Serves over HTTPS with HSTS"),
    ("env-no-plaintext-secrets", "No plaintext secrets in env (sensitive flag set)"),
    ("firewall-enabled", "WAF / firewall protection enabled"),
    ("preview-protected", "Preview deployments are access-protected"),
    ("has-production", "Has a healthy production deployment"),
    ("pinned-runtime", "Build pins a framework / runtime (not 'unknown')"),
];

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// The edge-enforcement subset of enterprise config, gossiped mesh-wide so ANY
/// node — region ingress or the deployment's hosting node — enforces IP blocks
/// and deployment protection, regardless of which node the config was authored
/// on. Contains NO secrets: block prefixes, protection modes/scopes, and the
/// salted one-way password hash (needed for remote verification).
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EdgeExport {
    #[serde(default)]
    pub ip_blocks: HashMap<String, Vec<IpBlock>>,
    #[serde(default)]
    pub protection: HashMap<String, DeployProtection>,
    /// Microfrontend groups (team -> groups) so a request entering ANY node can
    /// MFE-route, not just the node that authored the group.
    #[serde(default)]
    pub mfe: HashMap<String, Vec<MfeGroup>>,
}

#[derive(Default)]
pub struct EnterpriseStore {
    ip_blocks: RwLock<HashMap<String, Vec<IpBlock>>>, // team -> blocks
    siem: RwLock<HashMap<String, SiemConfig>>,        // team -> siem
    saml: RwLock<HashMap<String, SamlConfig>>,        // team -> saml
    scim: RwLock<HashMap<String, ScimTenant>>,        // team -> scim
    protection: RwLock<HashMap<String, DeployProtection>>, // project -> protection
    mfe: RwLock<HashMap<String, Vec<MfeGroup>>>,      // team -> microfrontend groups
    conformance: RwLock<HashMap<String, ConformancePolicy>>, // team -> policy
    reports: RwLock<HashMap<String, ConformanceReport>>,     // deployment_id -> report
    /// Gossip-replicated edge config from peers (node name -> their export).
    /// Refreshed every gossip round; NOT persisted (rebuilt from the mesh).
    peer_edge: RwLock<HashMap<String, EdgeExport>>,
}

/// Argon2id password hash → PHC string (`$argon2id$v=19$...`), embedding a random
/// salt + params. Memory-hard: a leaked hash is not cheaply brute-forceable
/// (unlike the previous `sha256(project ":" password)`). The PHC string is
/// self-describing, so a hash minted once on the owning node stays verifiable on
/// every peer it is gossiped to — no node-local key material required.
fn password_hash(_project: &str, password: &str) -> String {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;
    let salt = SaltString::generate(&mut OsRng);
    match Argon2::default().hash_password(password.as_bytes(), &salt) {
        Ok(h) => h.to_string(),
        Err(_) => String::new(),
    }
}

/// Verify a plaintext against a stored password representation, transparently
/// handling all three on-disk formats: current Argon2id PHC strings, the legacy
/// per-node AEAD envelope (`enc:v1:`), and the legacy portable
/// `sha256(project ":" password)` hex. All comparisons are constant-time
/// (Argon2 verify is inherently so; the others via `ct_eq`).
fn password_matches(project: &str, stored: &str, submitted: &str) -> bool {
    if stored.is_empty() {
        return false;
    }
    if stored.starts_with("$argon2") {
        use argon2::password_hash::{PasswordHash, PasswordVerifier};
        use argon2::Argon2;
        return match PasswordHash::new(stored) {
            Ok(parsed) => Argon2::default().verify_password(submitted.as_bytes(), &parsed).is_ok(),
            Err(_) => false,
        };
    }
    if stored.starts_with("enc:v1:") {
        return crate::admin::ct_eq(&crate::secrets::decrypt(stored), submitted);
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(project.as_bytes());
    h.update(b":");
    h.update(submitted.as_bytes());
    let legacy: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    crate::admin::ct_eq(&legacy, stored)
}

fn norm(team: &str) -> String {
    if team.trim().is_empty() { "personal".into() } else { team.trim().to_string() }
}

impl EnterpriseStore {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- IP blocking ----
    pub fn ip_blocks(&self, team: &str) -> Vec<IpBlock> {
        self.ip_blocks.read().get(&norm(team)).cloned().unwrap_or_default()
    }
    pub fn add_ip_block(&self, team: &str, prefix: &str, note: &str) -> IpBlock {
        let b = IpBlock {
            id: format!("ipb_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            prefix: prefix.trim().to_string(),
            note: note.to_string(),
            enabled: true,
            created_ms: now_ms(),
        };
        self.ip_blocks.write().entry(norm(team)).or_default().push(b.clone());
        b
    }
    pub fn remove_ip_block(&self, team: &str, id: &str) {
        if let Some(v) = self.ip_blocks.write().get_mut(&norm(team)) {
            v.retain(|b| b.id != id);
        }
    }
    /// Is `ip` allowed for `team`? Consults BOTH locally-authored blocks and the
    /// gossip-replicated blocks from peers (config is authored on the dashboard
    /// coordinator, but enforcement happens on whatever node the request enters).
    pub fn ip_allowed(&self, team: &str, ip: &str) -> bool {
        let t = norm(team);
        let hit = |v: &Vec<IpBlock>| v.iter().any(|b| b.enabled && !b.prefix.is_empty() && ip.starts_with(&b.prefix));
        if self.ip_blocks.read().get(&t).map(&hit).unwrap_or(false) {
            return false;
        }
        !self.peer_edge.read().values().filter_map(|e| e.ip_blocks.get(&t)).any(|v| hit(v))
    }
    /// Any team block this ip on ANY team? (fast pre-filter when team unknown).
    pub fn any_team_blocks(&self, ip: &str) -> bool {
        let hit = |b: &IpBlock| b.enabled && !b.prefix.is_empty() && ip.starts_with(&b.prefix);
        self.ip_blocks.read().values().flatten().any(hit)
            || self.peer_edge.read().values().flat_map(|e| e.ip_blocks.values()).flatten().any(hit)
    }

    // ---- mesh replication of the edge-enforcement subset ----
    /// This node's LOCALLY-AUTHORED edge config (never the replicated overlay —
    /// re-exporting peer data would echo stale copies around the mesh forever).
    pub fn edge_export(&self) -> EdgeExport {
        EdgeExport {
            ip_blocks: self.ip_blocks.read().clone(),
            protection: self.protection.read().clone(),
            mfe: self.mfe.read().clone(),
        }
    }
    /// Ingest a peer's edge export (refreshed each gossip round; replaces that
    /// peer's previous copy wholesale so deletions propagate).
    pub fn ingest_peer_edge(&self, node: &str, export: EdgeExport) {
        self.peer_edge.write().insert(node.to_string(), export);
    }

    // ---- SIEM ----
    pub fn siem(&self, team: &str) -> Option<SiemConfig> {
        self.siem.read().get(&norm(team)).cloned()
    }
    pub fn set_siem(&self, team: &str, format: &str, endpoint: &str, token_plain: Option<&str>, enabled: bool) -> SiemConfig {
        let mut m = self.siem.write();
        let e = m.entry(norm(team)).or_default();
        e.format = format.to_string();
        e.endpoint = endpoint.to_string();
        if let Some(tok) = token_plain.filter(|t| !t.is_empty()) {
            e.token_enc = crate::secrets::encrypt(tok);
        }
        e.enabled = enabled;
        e.updated_ms = now_ms();
        e.clone()
    }
    /// All enabled SIEM configs with the token DECRYPTED (for the streamer only).
    fn siem_targets(&self, team: &str) -> Option<(SiemConfig, String)> {
        let c = self.siem.read().get(&norm(team)).cloned()?;
        if !c.enabled || c.endpoint.is_empty() {
            return None;
        }
        let tok = crate::secrets::decrypt(&c.token_enc);
        Some((c, tok))
    }

    // ---- SAML ----
    pub fn saml(&self, team: &str) -> SamlConfig {
        self.saml.read().get(&norm(team)).cloned().unwrap_or_default()
    }
    pub fn set_saml(&self, team: &str, c: SamlConfig) -> SamlConfig {
        let mut c = c;
        c.updated_ms = now_ms();
        self.saml.write().insert(norm(team), c.clone());
        c
    }

    // ---- SCIM ----
    pub fn scim(&self, team: &str) -> ScimTenant {
        self.scim.read().get(&norm(team)).cloned().unwrap_or_default()
    }
    /// Enable SCIM and (re)issue a bearer token. Returns the PLAINTEXT token once.
    pub fn scim_enable(&self, team: &str) -> String {
        let token = format!("scim_{}", uuid::Uuid::new_v4().simple());
        let mut m = self.scim.write();
        let t = m.entry(norm(team)).or_default();
        t.token_enc = crate::secrets::encrypt(&token);
        t.enabled = true;
        t.updated_ms = now_ms();
        token
    }
    pub fn scim_disable(&self, team: &str) {
        if let Some(t) = self.scim.write().get_mut(&norm(team)) {
            t.enabled = false;
        }
    }
    /// Resolve the team that owns a SCIM bearer token (constant-ish lookup).
    pub fn scim_team_for_token(&self, token: &str) -> Option<String> {
        self.scim
            .read()
            .iter()
            .find(|(_, t)| t.enabled && crate::admin::ct_eq(&crate::secrets::decrypt(&t.token_enc), token))
            .map(|(team, _)| team.clone())
    }
    pub fn scim_upsert_user(&self, team: &str, user_name: &str, display_name: &str, active: bool) -> ScimUser {
        let mut m = self.scim.write();
        let t = m.entry(norm(team)).or_default();
        if let Some(u) = t.users.iter_mut().find(|u| u.user_name.eq_ignore_ascii_case(user_name)) {
            u.display_name = display_name.to_string();
            u.active = active;
            return u.clone();
        }
        let u = ScimUser {
            id: format!("scu_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            user_name: user_name.to_string(),
            display_name: display_name.to_string(),
            active,
            created_ms: now_ms(),
        };
        t.users.push(u.clone());
        u
    }
    pub fn scim_deactivate_user(&self, team: &str, id: &str) -> bool {
        if let Some(t) = self.scim.write().get_mut(&norm(team)) {
            if let Some(u) = t.users.iter_mut().find(|u| u.id == id) {
                u.active = false;
                return true;
            }
        }
        false
    }
    /// Fully ERASE a SCIM user record (email/`user_name` + `display_name`),
    /// rather than the soft `active=false` flip `scim_deactivate_user` does.
    /// An IdP issuing SCIM `DELETE /Users/:id` is signaling real deprovisioning
    /// — often itself driven by an employee offboarding or a data-subject
    /// erasure request — and expects the receiving system to actually forget
    /// the user, not keep their PII in the directory indefinitely (surfaced
    /// via every future `GET /Users/:id` and platform snapshot/backup).
    pub fn scim_purge_user(&self, team: &str, id: &str) -> bool {
        if let Some(t) = self.scim.write().get_mut(&norm(team)) {
            let before = t.users.len();
            t.users.retain(|u| u.id != id);
            return t.users.len() != before;
        }
        false
    }
    pub fn scim_user(&self, team: &str, id: &str) -> Option<ScimUser> {
        self.scim.read().get(&norm(team))?.users.iter().find(|u| u.id == id).cloned()
    }

    // ---- Deployment protection ----
    /// Effective protection for a project: locally-authored config wins; else the
    /// freshest gossip-replicated copy from a peer (the dashboard coordinator
    /// authors the config, but the HOSTING node enforces the gate).
    pub fn protection(&self, project: &str) -> DeployProtection {
        if let Some(p) = self.protection.read().get(project) {
            return p.clone();
        }
        self.peer_edge
            .read()
            .values()
            .filter_map(|e| e.protection.get(project))
            .max_by_key(|p| p.updated_ms)
            .cloned()
            .unwrap_or_default()
    }
    pub fn set_protection(&self, project: &str, mode: &str, scope: &str, password_plain: Option<&str>) -> DeployProtection {
        let mut m = self.protection.write();
        let p = m.entry(project.to_string()).or_default();
        p.mode = mode.to_string();
        p.scope = scope.to_string();
        if let Some(pw) = password_plain.filter(|p| !p.is_empty()) {
            // Deterministic salted hash — PORTABLE across nodes (unlike a per-node
            // AEAD envelope), so a peer hosting the deployment can verify the gate
            // from the gossiped edge-export. Never reversible; never the plaintext.
            p.password_hash = password_hash(project, pw);
        }
        if mode == "off" {
            p.password_hash.clear();
        }
        p.updated_ms = now_ms();
        p.clone()
    }
    /// Verify a submitted password against the stored representation (local or
    /// replicated). Handles Argon2id (current), AEAD envelope and legacy sha256.
    pub fn verify_password(&self, project: &str, submitted: &str) -> bool {
        let stored = self.protection(project).password_hash;
        password_matches(project, &stored, submitted)
    }
    /// Seal a validated password into an opaque cookie value. Returns the STORED
    /// hash itself (an Argon2id PHC string — never the plaintext, never a fresh
    /// re-hash which would differ under the random salt), so the cookie is
    /// comparable by constant-time equality on every node that has the replica
    /// and auto-invalidates when the password (hence the stored hash) changes.
    pub fn seal_password_cookie(&self, project: &str, _submitted: &str) -> String {
        self.protection(project).password_hash
    }
    /// Verify a `hive_pw` cookie against the project's current password hash.
    /// (Invalidates automatically when the password changes.)
    pub fn verify_password_cookie(&self, project: &str, cookie_val: &str) -> bool {
        let stored = self.protection(project).password_hash;
        !stored.is_empty() && !cookie_val.is_empty() && crate::admin::ct_eq(cookie_val, &stored)
    }

    // ---- Microfrontends ----
    /// A team's groups, LOCAL + gossiped-from-peers, deduped by id (local wins),
    /// each normalized (legacy children migrated, roles stamped). This is the read
    /// used by the API and the edge so MFE routing works on every ingress node,
    /// not only the authoring one.
    pub fn mfe_groups(&self, team: &str) -> Vec<MfeGroup> {
        let team = norm(team);
        let mut out: Vec<MfeGroup> = self.mfe.read().get(&team).cloned().unwrap_or_default();
        let mut seen: std::collections::HashSet<String> = out.iter().map(|g| g.id.clone()).collect();
        for exp in self.peer_edge.read().values() {
            if let Some(groups) = exp.mfe.get(&team) {
                for g in groups {
                    if seen.insert(g.id.clone()) {
                        out.push(g.clone());
                    }
                }
            }
        }
        out.into_iter().map(|g| g.normalized()).collect()
    }
    /// Only the LOCAL groups (no peer merge) — for the authoritative write/read of
    /// the owning node (delete-conflict checks, config generation).
    pub fn mfe_groups_local(&self, team: &str) -> Vec<MfeGroup> {
        self.mfe.read().get(&norm(team)).cloned().unwrap_or_default().into_iter().map(|g| g.normalized()).collect()
    }
    /// Fast pre-filter for the edge: does ANY team (local OR peer) have an enabled
    /// group with at least one child? Cheap gate so the hot path skips MFE work.
    pub fn has_mfe(&self) -> bool {
        let has = |m: &HashMap<String, Vec<MfeGroup>>| {
            m.values().any(|v| {
                v.iter()
                    .any(|g| g.enabled && (g.members.iter().any(|mm| mm.project != g.host_project) || !g.children.is_empty()))
            })
        };
        has(&self.mfe.read()) || self.peer_edge.read().values().any(|e| has(&e.mfe))
    }
    /// The enabled group whose default (host) project is `host_project`, if any —
    /// merged across local + peers. The edge then resolves the child + fallback via
    /// [`crate::microfrontends`].
    pub fn mfe_group_for_host(&self, team: &str, host_project: &str) -> Option<MfeGroup> {
        self.mfe_groups(team).into_iter().find(|g| g.enabled && g.host_project == host_project)
    }
    /// A single group by id within a team (local + peers, normalized).
    pub fn mfe_group(&self, team: &str, id: &str) -> Option<MfeGroup> {
        self.mfe_groups(team).into_iter().find(|g| g.id == id)
    }
    /// The group a project currently belongs to (as default or child), if any.
    pub fn mfe_group_of_project(&self, team: &str, project: &str) -> Option<MfeGroup> {
        self.mfe_groups(team).into_iter().find(|g| g.members.iter().any(|m| m.project == project))
    }
    pub fn set_mfe_group(&self, team: &str, mut g: MfeGroup) -> MfeGroup {
        g = g.normalized();
        let mut m = self.mfe.write();
        let v = m.entry(norm(team)).or_default();
        if let Some(existing) = v.iter_mut().find(|x| x.id == g.id) {
            *existing = g.clone();
        } else {
            v.push(g.clone());
        }
        g
    }
    pub fn remove_mfe_group(&self, team: &str, id: &str) {
        if let Some(v) = self.mfe.write().get_mut(&norm(team)) {
            v.retain(|g| g.id != id);
        }
    }

    // ---- Conformance ----
    pub fn conformance_policy(&self, team: &str) -> ConformancePolicy {
        self.conformance.read().get(&norm(team)).cloned().unwrap_or_default()
    }
    pub fn set_conformance_policy(&self, team: &str, p: ConformancePolicy) -> ConformancePolicy {
        let mut p = p;
        p.updated_ms = now_ms();
        self.conformance.write().insert(norm(team), p.clone());
        p
    }
    pub fn put_report(&self, r: ConformanceReport) {
        self.reports.write().insert(r.deployment_id.clone(), r);
    }
    pub fn report(&self, deployment_id: &str) -> Option<ConformanceReport> {
        self.reports.read().get(deployment_id).cloned()
    }

    // ---- persistence ----
    pub fn snapshot(&self) -> EnterpriseSnapshot {
        EnterpriseSnapshot {
            ip_blocks: self.ip_blocks.read().clone(),
            siem: self.siem.read().clone(),
            saml: self.saml.read().clone(),
            scim: self.scim.read().clone(),
            protection: self.protection.read().clone(),
            mfe: self.mfe.read().clone(),
            conformance: self.conformance.read().clone(),
            reports: self.reports.read().values().cloned().collect(),
        }
    }
    pub fn load(&self, s: EnterpriseSnapshot) {
        *self.ip_blocks.write() = s.ip_blocks;
        *self.siem.write() = s.siem;
        *self.saml.write() = s.saml;
        *self.scim.write() = s.scim;
        *self.protection.write() = s.protection;
        *self.mfe.write() = s.mfe;
        *self.conformance.write() = s.conformance;
        *self.reports.write() = s.reports.into_iter().map(|r| (r.deployment_id.clone(), r)).collect();
    }
}

/// Serializable snapshot of all enterprise state (secrets already AEAD-encrypted
/// in the structs — token_enc / password_hash are `enc:v1:` envelopes).
#[derive(Default, Serialize, Deserialize)]
pub struct EnterpriseSnapshot {
    #[serde(default)]
    pub ip_blocks: HashMap<String, Vec<IpBlock>>,
    #[serde(default)]
    pub siem: HashMap<String, SiemConfig>,
    #[serde(default)]
    pub saml: HashMap<String, SamlConfig>,
    #[serde(default)]
    pub scim: HashMap<String, ScimTenant>,
    #[serde(default)]
    pub protection: HashMap<String, DeployProtection>,
    #[serde(default)]
    pub mfe: HashMap<String, Vec<MfeGroup>>,
    #[serde(default)]
    pub conformance: HashMap<String, ConformancePolicy>,
    #[serde(default)]
    pub reports: Vec<ConformanceReport>,
}

// ---------------------------------------------------------------------------
// SIEM streamer — a global async sink fed by audit.rs::record.
// ---------------------------------------------------------------------------

static SIEM_TX: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<crate::audit::AuditEntry>> = std::sync::OnceLock::new();

/// Start the background SIEM streamer. Every audit entry emitted via
/// [`siem_emit`] is looked up against its tenant's [`SiemConfig`] and, if
/// enabled, POSTed to the configured endpoint (best-effort, never blocks the
/// audit write path). Called once at boot with the shared state + http client.
pub fn spawn_siem_streamer(store: Arc<EnterpriseStore>, http: reqwest::Client) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::audit::AuditEntry>();
    if SIEM_TX.set(tx).is_err() {
        return; // already started
    }
    tokio::spawn(async move {
        while let Some(entry) = rx.recv().await {
            let Some((cfg, token)) = store.siem_targets(&entry.tenant) else {
                continue;
            };
            let ok = deliver_siem(&http, &cfg, &token, &entry).await;
            // Update counters (best-effort; not persisted).
            if let Some(c) = store.siem.write().get_mut(&norm(&entry.tenant)) {
                if ok {
                    c.delivered += 1;
                } else {
                    c.failed += 1;
                }
            }
        }
    });
}

/// Emit an audit entry to the SIEM streamer (no-op if streaming isn't up).
pub fn siem_emit(entry: &crate::audit::AuditEntry) {
    if let Some(tx) = SIEM_TX.get() {
        let _ = tx.send(entry.clone());
    }
}

async fn deliver_siem(http: &reqwest::Client, cfg: &SiemConfig, token: &str, e: &crate::audit::AuditEntry) -> bool {
    // Vendor-shaped payloads so a real Datadog/Splunk endpoint accepts them.
    let body = match cfg.format.as_str() {
        "datadog" => serde_json::json!({
            "ddsource": "shadw", "service": "platform", "ddtags": format!("team:{}", e.tenant),
            "message": format!("{} {} {} {}", e.action, e.entity, e.entity_id, e.detail),
            "seq": e.seq, "ts": e.ts_ms, "actor": e.actor, "action": e.action, "entity": e.entity, "entity_id": e.entity_id,
        }),
        "splunk-hec" => serde_json::json!({
            "time": e.ts_ms / 1000, "source": "shadw", "sourcetype": "shadw:audit",
            "event": e,
        }),
        _ => serde_json::to_value(e).unwrap_or_default(),
    };
    let mut req = http.post(&cfg.endpoint).json(&body).timeout(std::time::Duration::from_secs(8));
    if !token.is_empty() {
        req = match cfg.format.as_str() {
            "datadog" => req.header("DD-API-KEY", token),
            "splunk-hec" => req.header("Authorization", format!("Splunk {token}")),
            _ => req.header("Authorization", format!("Bearer {token}")),
        };
    }
    matches!(req.send().await, Ok(r) if r.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_block_prefix_match() {
        let s = EnterpriseStore::new();
        s.add_ip_block("acme", "203.0.113.", "bad actor");
        assert!(!s.ip_allowed("acme", "203.0.113.7"));
        assert!(s.ip_allowed("acme", "203.0.114.7"));
        assert!(s.ip_allowed("other", "203.0.113.7")); // scoped per team
        assert!(s.any_team_blocks("203.0.113.7"));
    }

    #[test]
    fn protection_scope_semantics() {
        let mut p = DeployProtection::default();
        p.mode = "password".into();
        p.scope = "preview".into();
        assert!(p.protects("preview") && !p.protects("production"));
        p.scope = "production".into();
        assert!(p.protects("production") && !p.protects("preview"));
        p.scope = "all".into();
        assert!(p.protects("production") && p.protects("preview"));
        p.mode = "off".into();
        assert!(!p.protects("preview"));
    }

    #[test]
    fn password_roundtrip_and_scim_token() {
        let s = EnterpriseStore::new();
        s.set_protection("proj", "password", "preview", Some("hunter2"));
        assert!(s.verify_password("proj", "hunter2"));
        assert!(!s.verify_password("proj", "wrong"));
        // stored value is an encrypted envelope, never the plaintext
        assert!(s.protection("proj").password_hash.starts_with("enc:v1:") || s.protection("proj").password_hash != "hunter2");
        let tok = s.scim_enable("acme");
        assert_eq!(s.scim_team_for_token(&tok).as_deref(), Some("acme"));
        assert!(s.scim_team_for_token("nope").is_none());
    }

    #[test]
    fn password_hash_is_argon2id_and_verifies_legacy_sha256_too() {
        // The stored deployment-protection password is now a memory-hard Argon2id
        // PHC string (was a brute-forceable sha256(project:password)). Verify:
        // (1) the stored form is a `$argon2id$` PHC string, not the plaintext or
        // a bare sha256 hex; (2) it verifies correctly; (3) each hash is salted
        // (two hashes of the same password differ); (4) the cookie seal returns
        // the STORED hash (comparable by ct_eq); (5) a legacy sha256 hash still
        // verifies (backward compatibility for pre-upgrade stored values).
        let s = EnterpriseStore::new();
        s.set_protection("proj", "password", "preview", Some("hunter2"));
        let stored = s.protection("proj").password_hash;
        assert!(stored.starts_with("$argon2id$"), "stored must be an Argon2id PHC string, got {stored}");
        assert!(s.verify_password("proj", "hunter2"));
        assert!(!s.verify_password("proj", "wrong"));
        // Salted: a second hash of the same password is a different string.
        assert_ne!(password_hash("proj", "hunter2"), password_hash("proj", "hunter2"));
        // Cookie seal returns the stored hash (constant-time comparable on peers).
        assert_eq!(s.seal_password_cookie("proj", "hunter2"), stored);
        assert!(s.verify_password_cookie("proj", &stored));
        // Legacy sha256(project ":" password) hex still verifies via the fallback.
        let legacy: String = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"legacyproj:");
            h.update(b"s3cret");
            h.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        assert!(password_matches("legacyproj", &legacy, "s3cret"));
        assert!(!password_matches("legacyproj", &legacy, "nope"));
    }

    #[test]
    fn scim_purge_user_fully_erases_pii_unlike_deactivate() {
        // REGRESSION TEST for a real, confirmed GDPR Art.17 gap: SCIM
        // `DELETE /Users/:id` used to only flip `active=false`
        // (scim_deactivate_user) — the former employee's real name/email
        // stayed in the directory forever, retrievable via GET and included
        // in every future platform snapshot/backup, even though an IdP
        // issuing SCIM DELETE is signaling real deprovisioning.
        let s = EnterpriseStore::new();
        s.scim_enable("acme");
        let u = s.scim_upsert_user("acme", "exemployee@example.com", "Real Name", true);

        // Deactivate alone (the OLD behavior) must NOT erase the record —
        // confirms the two operations are genuinely distinct.
        assert!(s.scim_deactivate_user("acme", &u.id));
        assert!(s.scim_user("acme", &u.id).is_some(), "deactivate must not erase the record");

        // The real fix: purge actually removes it.
        assert!(s.scim_purge_user("acme", &u.id));
        assert!(s.scim_user("acme", &u.id).is_none(), "purge must fully erase the user record (email/display name)");

        // Idempotent / honest return value: purging an already-gone id reports false.
        assert!(!s.scim_purge_user("acme", &u.id));
        // Purging in the wrong team never finds it either.
        assert!(!s.scim_purge_user("other-team", "nonexistent"));
    }
}
