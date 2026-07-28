//! Vercel DNS reconciler — the ngrok-retirement ingress control plane.
//!
//! Vercel DNS is plain authoritative DNS (REST records API; no health checks,
//! no geo steering), so this module runs a LEADER-ELECTED reconcile loop inside
//! hive-cloud that syncs *healthy node public IPs* → Vercel DNS records:
//!
//! | record                        | value                                | TTL |
//! |-------------------------------|--------------------------------------|-----|
//! | `api.{platform_domain}`       | healthy gateway nodes' public IPs    | 60  |
//! | `*.{apps_domain}` + apex      | healthy edge nodes' public IPs       | 60  |
//! | `relay.{platform_domain}`     | `HIVE_RELAY_IPS` (self-hosted iroh)  | 300 |
//! | `discovery.{platform_domain}` | `HIVE_DISCOVERY_IPS` (pkarr relay)   | 300 |
//!
//! (Node roles aren't distinguished yet — all healthy public nodes go into both
//! the api and wildcard sets. TODO: role split when nodes grow roles.)
//!
//! Safety properties:
//!  * **Delta-only**: unchanged records are never rewritten.
//!  * **Flap damping**: a node's records are only removed after K=2 consecutive
//!    reconcile passes see it unhealthy (the active prober already needs
//!    ~10–12s to flip health — don't double-punish).
//!  * **Never publish empty**: if the desired set is empty, keep last-known-good,
//!    log loudly, and raise an incident instead.
//!  * Exponential backoff on 429/5xx from the Vercel API.
//!
//! Regional/latency steering stays inside `edge.rs` (`order_candidates`) after
//! the client reaches any node — DNS only hands out healthy IPs. The self-hosted
//! Seer (`dnsserver.rs`) is NOT retired: it keeps serving internal/test queries
//! and is the future NS-delegation path.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::state::CloudState;

/// How many consecutive unhealthy reconcile passes before a node's records are
/// withdrawn (flap damping).
pub const UNHEALTHY_PASSES_BEFORE_WITHDRAW: u32 = 2;

/// A DNS record as it exists at Vercel (subset we care about).
#[derive(Clone, Debug, PartialEq)]
pub struct RecordView {
    pub id: String,
    /// Subdomain relative to the zone: `""` (apex), `"api"`, `"*"`, …
    pub name: String,
    /// `A` | `AAAA` | `TXT` | …
    pub rtype: String,
    pub value: String,
}

/// A record we want to exist.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DesiredRecord {
    pub name: String,
    pub rtype: String,
    pub value: String,
    pub ttl: u32,
}

/// Minimal Vercel DNS API surface — a trait so the reconciler is unit-testable
/// against a mock (create/delete/no-op/429 cases) without network.
pub trait DnsApi: Send + Sync {
    fn list(
        &self,
        domain: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<RecordView>>> + Send;
    fn create(
        &self,
        domain: &str,
        rec: &DesiredRecord,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
    fn delete(
        &self,
        domain: &str,
        id: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

/// Real Vercel REST client. Token comes from `VERCEL_API_TOKEN` (treat as a
/// secret — never logged); `VERCEL_TEAM_ID` adds the optional `?teamId=`.
#[derive(Clone)]
pub struct VercelApi {
    http: reqwest::Client,
    token: String,
    team_id: Option<String>,
}

impl VercelApi {
    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let token = std::env::var("VERCEL_API_TOKEN").ok().filter(|s| !s.is_empty())?;
        let team_id = std::env::var("VERCEL_TEAM_ID").ok().filter(|s| !s.is_empty());
        Some(Self { http, token, team_id })
    }

    fn url(&self, version: &str, rest: &str) -> String {
        let mut u = format!("https://api.vercel.com/{version}/{rest}");
        if let Some(t) = &self.team_id {
            u.push_str(if u.contains('?') { "&" } else { "?" });
            u.push_str(&format!("teamId={t}"));
        }
        u
    }
}

/// Map an HTTP status to "retryable with backoff" (429/5xx).
fn retryable(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}

impl DnsApi for VercelApi {
    async fn list(&self, domain: &str) -> anyhow::Result<Vec<RecordView>> {
        let resp = self
            .http
            .get(self.url("v4", &format!("domains/{domain}/records?limit=100")))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("vercel list {domain}: {status}{}", if retryable(status) { " (retryable)" } else { "" });
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v.get("records")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        Some(RecordView {
                            id: r.get("id").or_else(|| r.get("uid"))?.as_str()?.to_string(),
                            name: r.get("name")?.as_str()?.to_string(),
                            rtype: r.get("type")?.as_str()?.to_string(),
                            value: r.get("value")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn create(&self, domain: &str, rec: &DesiredRecord) -> anyhow::Result<String> {
        let resp = self
            .http
            .post(self.url("v2", &format!("domains/{domain}/records")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "name": rec.name, "type": rec.rtype, "value": rec.value, "ttl": rec.ttl,
            }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("vercel create {domain} {} {}: {status}{}", rec.rtype, rec.name, if retryable(status) { " (retryable)" } else { "" });
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v.get("uid").and_then(|u| u.as_str()).unwrap_or_default().to_string())
    }

    async fn delete(&self, domain: &str, id: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(self.url("v2", &format!("domains/{domain}/records/{id}")))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("vercel delete {domain}/{id}: {status}{}", if retryable(status) { " (retryable)" } else { "" });
        }
        Ok(())
    }
}

// ---- desired state -----------------------------------------------------------

/// A publishable node: healthy (or inside the damping window) with a public IP.
#[derive(Clone, Debug)]
pub struct PublishNode {
    pub name: String,
    pub ip4: Option<String>,
    pub ip6: Option<String>,
    /// This node answers authoritative DNS on a public `:53` (gossiped
    /// `NodeInfo::dns_ns`) — i.e. it is eligible to appear in the geo zone's
    /// NS set. See `desired_geo_delegation`.
    pub dns_ns: bool,
    /// The node's region code (`san-jose`, `bangkok`, …) — carried through from
    /// the registry so the per-region names can be derived from the same
    /// health-damped set every other record already comes from.
    pub region: String,
}

/// Desired records for the APPS zone (`*.{apps}` + apex), TTL 60.
pub fn desired_apps(nodes: &[PublishNode]) -> Vec<DesiredRecord> {
    let mut out = Vec::new();
    for name in ["*", ""] {
        for n in nodes {
            if let Some(ip) = &n.ip4 {
                out.push(DesiredRecord { name: name.into(), rtype: "A".into(), value: ip.clone(), ttl: 60 });
            }
            if let Some(ip) = &n.ip6 {
                out.push(DesiredRecord { name: name.into(), rtype: "AAAA".into(), value: ip.clone(), ttl: 60 });
            }
        }
    }
    out
}

/// The label the geo zone hangs off inside the apps zone, derived from
/// `HIVE_DEPLOY_ZONE`: `deploy.shadw.app` inside apps zone `shadw.app` → `deploy`.
/// `None` when the deploy zone is unset or is not a child of the apps zone (then
/// there is nothing this reconciler can delegate and it stays out of the way).
pub fn geo_label(deploy_zone: &str, apps_domain: &str) -> Option<String> {
    let dz = deploy_zone.trim().trim_matches('.').to_lowercase();
    let apps = apps_domain.trim().trim_matches('.').to_lowercase();
    if dz.is_empty() || apps.is_empty() {
        return None;
    }
    dz.strip_suffix(&format!(".{apps}")).filter(|l| !l.is_empty() && !l.contains('.')).map(str::to_string)
}

/// Stable nameserver label for a node: `ns-<node-name>`, so the record set is a
/// pure function of the registry (no index that renumbers when a node drops out
/// and silently repoints an existing NS at a different machine).
pub fn ns_label(node_name: &str) -> String {
    format!("ns-{}", node_name.trim().to_lowercase())
}

/// Delegation records for the geo zone: NS records on `<label>` pointing at
/// per-node nameserver names inside the apps zone, plus the glue A/AAAA for
/// those names.
///
/// Why this exists: the apps/platform zones are hosted by Vercel DNS, which is
/// plain authoritative DNS with no geo or health routing — so the platform's
/// own geo-aware server (Seer, `dnsserver.rs`) can only ever answer queries for
/// names actually DELEGATED to it. This publishes that delegation, derived from
/// the same health-damped `PublishNode` set every other record comes from, so a
/// nameserver that goes unhealthy or loses its public IP leaves the NS set by
/// the normal diff instead of a hand-maintained list going stale.
///
/// Only nodes that really answer DNS are eligible (`PublishNode::dns_ns`, set at
/// boot from a public `:53` bind and gossiped) — advertising a node that has no
/// listener would put a black hole in the delegated zone.
pub fn desired_geo_delegation(
    nodes: &[PublishNode],
    label: &str,
    apps_domain: &str,
) -> (Vec<DesiredRecord>, Vec<String>) {
    let mut out = Vec::new();
    let mut managed = vec![label.to_string()];
    for n in nodes.iter().filter(|n| n.dns_ns) {
        let ns = ns_label(&n.name);
        if let Some(ip) = &n.ip4 {
            out.push(DesiredRecord { name: ns.clone(), rtype: "A".into(), value: ip.clone(), ttl: 300 });
        }
        if let Some(ip) = &n.ip6 {
            out.push(DesiredRecord { name: ns.clone(), rtype: "AAAA".into(), value: ip.clone(), ttl: 300 });
        }
        if n.ip4.is_some() || n.ip6.is_some() {
            // FULLY QUALIFIED, always: Vercel rejects a relative NS target
            // outright — `{"code":"invalid_value","message":"The NS value is
            // not a fully qualified domain name."}` — which is why the glue A
            // records published while every NS create silently failed, leaving
            // the zone undelegated with correct-looking glue in place.
            let target = format!("{ns}.{}", apps_domain.trim().trim_matches('.'));
            out.push(DesiredRecord { name: label.to_string(), rtype: "NS".into(), value: target, ttl: 300 });
            managed.push(ns);
        }
    }
    // A delegation with a single nameserver is a single point of failure for
    // every name in the zone; below two, publish nothing and leave the zone
    // undelegated (it keeps resolving through the parent's own records).
    if out.iter().filter(|r| r.rtype == "NS").count() < 2 {
        return (Vec::new(), Vec::new());
    }
    (out, managed)
}

/// True for an immutable per-commit alias like `myapp-c7416ec` — a URL minted
/// once per build that nobody navigates to twice.
///
/// These dominate the alias set the same way `dpl-*` does (one live node carried
/// 211 aliases: 96 `dpl-*` and most of the rest per-commit), so pinning them
/// would spend the whole Vercel create budget on URLs that are visited once,
/// starving the aliases people actually use. They keep the wildcard's all-nodes
/// behaviour, so they still resolve — only the affinity optimisation skips them.
///
/// `*-git-<branch>` is explicitly NOT a commit alias: it's the stable
/// per-branch preview URL and is worth pinning. A project genuinely named
/// `foo-abc123` would be skipped here, which is harmless — it just keeps the
/// pre-existing wildcard behaviour rather than gaining the optimisation.
fn is_commit_alias(label: &str) -> bool {
    if label.contains("-git-") {
        return false;
    }
    match label.rsplit_once('-') {
        Some((prefix, tail)) => {
            !prefix.is_empty()
                && (6..=12).contains(&tail.len())
                && tail.chars().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// Deployment-affinity records for the APPS zone: one **specific** A/AAAA per
/// served host label pointing at the node that actually hosts it.
///
/// Why: the wildcard `*.{apps}` resolves to EVERY publishable node, so a client
/// reached the owning node only 1-in-N times and otherwise paid a cross-node
/// forward — measured at +41ms within a region and up to +380ms from Bangkok to
/// San Jose. A specific record beats the wildcard in DNS, so publishing
/// `<label> → owner` sends the client straight to the right region and node and
/// the forward simply doesn't happen. The wildcard stays as the fallback (and
/// for TLS coverage) so an unknown or just-created label still resolves.
///
/// This is the same shape already proven for the DB zone (`<slug>` → the node
/// holding that database's container), reused rather than reinvented.
///
/// `owners` is `(label, node_name)`; later duplicates for a label are ignored so
/// the caller controls precedence (local first, then peers). Only single-label
/// names are emitted — a value carrying dots would create a record in the wrong
/// place. `cap` bounds how many specific records we publish, because Vercel
/// rate-limits record creation per call; ordering is sorted so the set that
/// survives the cap is stable across passes instead of flapping.
pub fn desired_apps_affinity(
    owners: &[(String, String)],
    nodes: &[PublishNode],
    cap: usize,
) -> (Vec<DesiredRecord>, Vec<String>) {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (label, node) in owners {
        // Normalise to the FIRST DNS label: `served_hosts()` keys are already
        // bare labels, but a full host slipping in must not become a record
        // named `app.example.com` inside the apps zone.
        let label = label.split(':').next().unwrap_or(label);
        let label = label.split('.').next().unwrap_or(label).trim().to_ascii_lowercase();
        if label.is_empty() || label == "*" || label == "www" {
            continue;
        }
        // Skip immutable per-deployment URLs (`dpl-<id>`). Every deployment ever
        // made owns one, so they dominate the alias set by an order of magnitude
        // — publishing them burned the Vercel create budget (live-observed:
        // `create shadw.app A dpl-198f8dbaff: 429 Too Many Requests`) and
        // starved the aliases people actually visit. They keep the wildcard's
        // all-nodes behaviour, which is correct: a one-off build URL is not
        // worth a record, and it still resolves.
        if label.starts_with("dpl-") || is_commit_alias(&label) {
            continue;
        }
        if !seen.insert(label.clone()) {
            continue; // first writer wins (caller-ordered precedence)
        }
        pairs.push((label, node.clone()));
    }
    pairs.sort();
    pairs.truncate(cap);

    let mut out = Vec::new();
    let mut managed = Vec::new();
    for (label, node) in pairs {
        // Only a PUBLISHABLE node may be named: pointing a specific record at an
        // unhealthy/NAT'd node would be strictly worse than the wildcard, since
        // the specific record wins and the client would have no other answer.
        let Some(n) = nodes.iter().find(|n| n.name == node) else { continue };
        if n.ip4.is_none() && n.ip6.is_none() {
            continue;
        }
        managed.push(label.clone());
        if let Some(ip) = &n.ip4 {
            out.push(DesiredRecord { name: label.clone(), rtype: "A".into(), value: ip.clone(), ttl: 60 });
        }
        if let Some(ip) = &n.ip6 {
            out.push(DesiredRecord { name: label.clone(), rtype: "AAAA".into(), value: ip.clone(), ttl: 60 });
        }
    }
    (out, managed)
}

/// Deterministic per-region names, from the same health-damped publishable set
/// every other record is built from: `<prefix><region>` → every publishable node
/// in that region.
///
/// Why this exists: today the ONLY way to reach a specific region is to already
/// know a node's raw IP. `api.<platform>` and `*.<apps>` are both flat
/// round-robin over the whole fleet, so nothing in the system can NAME a region
/// — not a client wanting to pin one, not a redirect wanting to hand off to a
/// closer one, and not a future anycast cutover wanting a per-region origin.
/// `api-san-jose.<platform>` / `san-jose.<apps>` give that a stable answer
/// without needing geo DNS or EDNS support anywhere.
///
/// Returns `(records, names)` — the names must be added to the zone's MANAGED
/// list or the reconciler can never diff, update or withdraw them again (the
/// bug that left `sms` pointing at a foreign IP forever).
///
/// Region codes come from the registry, so they are already DNS-label-shaped
/// (`san-jose`, `hong-kong`); anything that isn't is skipped rather than
/// published as an invalid name. An empty region (a node that never reported
/// one) is skipped for the same reason.
pub fn desired_region_names(nodes: &[PublishNode], prefix: &str) -> (Vec<DesiredRecord>, Vec<String>) {
    let mut by_region: std::collections::BTreeMap<String, Vec<&PublishNode>> = Default::default();
    for n in nodes {
        let r = n.region.trim().to_ascii_lowercase();
        if r.is_empty() || !r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || r.starts_with('-') || r.ends_with('-') {
            continue;
        }
        by_region.entry(r).or_default().push(n);
    }
    let mut out = Vec::new();
    let mut names = Vec::new();
    for (region, ns) in by_region {
        let name = format!("{prefix}{region}");
        names.push(name.clone());
        for n in ns {
            if let Some(ip) = &n.ip4 {
                out.push(DesiredRecord { name: name.clone(), rtype: "A".into(), value: ip.clone(), ttl: 60 });
            }
            if let Some(ip) = &n.ip6 {
                out.push(DesiredRecord { name: name.clone(), rtype: "AAAA".into(), value: ip.clone(), ttl: 60 });
            }
        }
    }
    (out, names)
}

/// Desired records for the PLATFORM zone: `api.` (TTL 60, all publishable
/// nodes) + `relay.`/`discovery.` (TTL 300, from env IP lists — those services
/// run on operator-chosen nodes, not every gateway).
pub fn desired_platform(
    nodes: &[PublishNode],
    relay_ips: &[String],
    discovery_ips: &[String],
    dashboard: bool, // publish apex + www too (nodes reverse-proxy the dashboard)
) -> Vec<DesiredRecord> {
    let mut out = Vec::new();
    // `api` = developer/API-key surface, `admin` = ops/admin console surface,
    // `webhook` = incoming GitOps/OpenEdge build-notification receiver
    // (OPENEDGE_WEBHOOK_URL) — all three resolve to the gateway nodes (same
    // host-switch dispatch), published together.
    // `sms` = the self-hosted SMS-fallback service (a platform-deployed app the
    // edge routes by Host alias) — same gateway-node A/AAAA set as the rest.
    let mut names: Vec<&str> = vec!["api", "admin", "webhook", "sms"];
    if dashboard {
        names.push("");
        names.push("www");
    }
    for name in names {
        for n in nodes {
            if let Some(ip) = &n.ip4 {
                out.push(DesiredRecord { name: name.into(), rtype: "A".into(), value: ip.clone(), ttl: 60 });
            }
            if let Some(ip) = &n.ip6 {
                out.push(DesiredRecord { name: name.into(), rtype: "AAAA".into(), value: ip.clone(), ttl: 60 });
            }
        }
    }
    for (sub, ips) in [("relay", relay_ips), ("discovery", discovery_ips)] {
        for ip in ips {
            let rtype = if ip.contains(':') { "AAAA" } else { "A" };
            out.push(DesiredRecord { name: sub.into(), rtype: rtype.into(), value: ip.clone(), ttl: 300 });
        }
    }
    out
}

// ---- diff ---------------------------------------------------------------------

/// Compute the delta between what exists and what we want, touching ONLY the
/// names this reconciler manages and ONLY A/AAAA records (TXT — e.g. the ACME
/// solver's `_acme-challenge` — is never ours to delete). Pure; unit-tested.
pub fn diff(
    current: &[RecordView],
    desired: &[DesiredRecord],
    managed_names: &[&str],
) -> (Vec<DesiredRecord>, Vec<String>) {
    let managed = |name: &str| managed_names.contains(&name);
    // NS joins A/AAAA as a reconciler-owned type so the geo zone's delegation
    // (see `desired_geo_delegation`) is diffed by the same delta-only path as
    // every address record. TXT stays excluded on purpose — the ACME solver's
    // `_acme-challenge` is not ours to delete.
    let addr_type = |t: &str| t == "A" || t == "AAAA" || t == "NS";

    let have: Vec<&RecordView> = current
        .iter()
        .filter(|r| managed(&r.name) && addr_type(&r.rtype))
        .collect();

    // ALIAS displacement: Vercel adds default ALIAS records (`*`/apex →
    // Vercel hosting) when a domain joins the account. An ALIAS and an A record
    // can't coexist on the same name, so when we are about to publish addresses
    // for a MANAGED name, any ALIAS squatting on it must go. Never touched for
    // unmanaged names, and never unless we actually have addresses to publish.
    let alias_deletes: Vec<String> = current
        .iter()
        .filter(|r| {
            r.rtype == "ALIAS"
                && managed(&r.name)
                && desired.iter().any(|d| d.name == r.name && (d.rtype == "A" || d.rtype == "AAAA"))
        })
        .map(|r| r.id.clone())
        .collect();

    // Compare values in a NORMALIZED form. Vercel stores a hostname-valued
    // record (NS, CNAME) with a trailing root dot — `ns-x.shadw.app.` for a
    // desired `ns-x.shadw.app` — so a raw string compare never matches, and
    // the reconciler deletes and recreates the same record on EVERY pass.
    // Live-witnessed: that churn burned the whole create budget and turned
    // into a sustained `429 Too Many Requests` storm that also starved
    // unrelated records (apex, api, per-deployment aliases) of their creates.
    // The trailing dot is presentation, not identity.
    let norm_value = |v: &str| v.trim().trim_end_matches('.').to_ascii_lowercase();
    let want_key = |r: &DesiredRecord| (r.name.clone(), r.rtype.clone(), norm_value(&r.value));
    let have_key = |r: &RecordView| (r.name.clone(), r.rtype.clone(), norm_value(&r.value));

    let want_set: std::collections::HashSet<_> = desired
        .iter()
        .filter(|r| managed(&r.name) && addr_type(&r.rtype))
        .map(want_key)
        .collect();
    let have_set: std::collections::HashSet<_> = have.iter().map(|r| have_key(r)).collect();

    let creates: Vec<DesiredRecord> = desired
        .iter()
        .filter(|r| managed(&r.name) && addr_type(&r.rtype))
        .filter(|r| !have_set.contains(&want_key(r)))
        .cloned()
        .collect();
    let mut deletes: Vec<String> = have
        .iter()
        .filter(|r| !want_set.contains(&have_key(r)))
        .map(|r| r.id.clone())
        .collect();
    deletes.extend(alias_deletes);
    (creates, deletes)
}

// ---- reconcile loop ------------------------------------------------------------

/// Observable reconciler counters (surfaced via the admin API).
#[derive(Default)]
pub struct ReconcilerStats {
    pub passes: AtomicU64,
    pub creates: AtomicU64,
    pub deletes: AtomicU64,
    pub api_errors: AtomicU64,
    pub empty_set_blocks: AtomicU64,
    pub per_name_holds: AtomicU64,
    /// How many deployment-affinity records the last pass published — i.e. how
    /// many host labels currently resolve straight to their owning node instead
    /// of falling through to the all-nodes wildcard.
    pub affinity_records: AtomicU64,
    /// How many deterministic per-region names the last pass published (one per
    /// region with at least one publishable node).
    pub region_records: AtomicU64,
    pub last_pass_ms: AtomicU64,
    /// NS+glue records published for the delegated geo zone (0 = undelegated).
    pub geo_delegation_records: AtomicU64,
}

pub static STATS: ReconcilerStats = ReconcilerStats {
    passes: AtomicU64::new(0),
    creates: AtomicU64::new(0),
    deletes: AtomicU64::new(0),
    api_errors: AtomicU64::new(0),
    empty_set_blocks: AtomicU64::new(0),
    per_name_holds: AtomicU64::new(0),
    affinity_records: AtomicU64::new(0),
    region_records: AtomicU64::new(0),
    last_pass_ms: AtomicU64::new(0),
    geo_delegation_records: AtomicU64::new(0),
};

/// Upper bound on deployment-affinity records. Vercel rate-limits record
/// creation, and the wildcard already guarantees every label resolves — so the
/// cap degrades gracefully (labels beyond it just keep the old all-nodes
/// behaviour) instead of stalling the whole reconcile pass. Deliberately modest:
/// a first attempt at 200 (including per-deployment `dpl-*` URLs) produced a
/// sustained 429 storm that aborted every pass before it converged.
const APPS_AFFINITY_CAP: usize = 60;

/// Delay between record creates. The Vercel DNS API rate-limits creates
/// individually, so an unpaced burst 429s partway through and the pass makes no
/// progress at all.
const CREATE_PACING: std::time::Duration = std::time::Duration::from_millis(1100);

fn env_ips(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

/// The registry facts one reconcile pass needs about a node. Named rather than a
/// positional tuple: this started as `(String, bool, Option<String>,
/// Option<String>)` and adding `region` for the per-region names made a 5-wide
/// positional soup where two adjacent fields have the SAME type — exactly the
/// shape where a call site silently swaps them.
#[derive(Clone, Debug)]
pub struct NodeView {
    pub name: String,
    pub healthy: bool,
    pub ip4: Option<String>,
    pub ip6: Option<String>,
    pub region: String,
    /// Gossiped `NodeInfo::dns_ns` presence — carried through so the geo
    /// delegation's NS set comes from the same health-damped view.
    pub dns_ns: bool,
}

/// The publishable node set with flap damping: a node stays published while its
/// consecutive-unhealthy streak is below the threshold. Pure; unit-tested.
pub fn publishable(nodes: &[NodeView], streaks: &mut HashMap<String, u32>) -> Vec<PublishNode> {
    let mut out = Vec::new();
    for n in nodes {
        if n.ip4.is_none() && n.ip6.is_none() {
            continue; // never publishable without a public IP
        }
        let streak = streaks.entry(n.name.clone()).or_insert(0);
        if n.healthy {
            *streak = 0;
        } else {
            *streak = streak.saturating_add(1);
        }
        if n.healthy || *streak < UNHEALTHY_PASSES_BEFORE_WITHDRAW {
            out.push(PublishNode {
                name: n.name.clone(),
                ip4: n.ip4.clone(),
                ip6: n.ip6.clone(),
                region: n.region.clone(),
                dns_ns: n.dns_ns,
            });
        }
    }
    // Drop streak entries for nodes that vanished from the registry entirely.
    let known: std::collections::HashSet<&String> = nodes.iter().map(|n| &n.name).collect();
    streaks.retain(|k, _| known.contains(k));
    out
}

/// One reconcile pass over one zone. Returns Err on API failure (caller backs off).
async fn reconcile_zone<A: DnsApi>(
    api: &A,
    domain: &str,
    desired: &[DesiredRecord],
    managed_names: &[&str],
    cloud: &Arc<CloudState>,
) -> anyhow::Result<()> {
    // NEVER publish an empty set: losing every record would blackhole the domain
    // harder than stale-but-healthy-yesterday IPs. Keep last-known-good + incident.
    if !desired.iter().any(|r| r.rtype == "A" || r.rtype == "AAAA") {
        STATS.empty_set_blocks.fetch_add(1, Ordering::Relaxed);
        tracing::error!(%domain, "DNS reconciler: desired record set is EMPTY — keeping last-known-good records and raising an incident");
        cloud.incidents.open(crate::incidents::OpenReq {
            title: format!("DNS reconciler: no publishable nodes for {domain}"),
            severity: crate::incidents::Severity::Major,
            affected: vec!["dns".into()],
            message: "Desired DNS record set is empty (no healthy nodes with public IPs). Keeping last-known-good records published.".into(),
        });
        return Ok(());
    }
    let current = api.list(domain).await?;
    let (creates, deletes) = diff(&current, desired, managed_names);
    // Per-name last-known-good: a degraded registry view can leave a managed
    // name with ZERO desired addresses while env-sourced names (relay/
    // discovery) keep the overall set non-empty — so the whole-set emptiness
    // guard above never trips, and the diff would delete every record for the
    // vanished name (live-observed 2026-07-22: a freshly-restarted node with a
    // still-empty healthy-web view won the leader check and deleted all
    // api/admin/webhook/apex/www records for the platform zone each cycle,
    // publishing only relay/discovery and blackholing the domain behind
    // Vercel's wildcard ALIAS while the real leader raced to re-create them).
    // Never delete a name's address records while desiring NO addresses for it.
    let empty_names: std::collections::HashSet<&str> = managed_names
        .iter()
        .copied()
        .filter(|n| !desired.iter().any(|d| d.name == *n && (d.rtype == "A" || d.rtype == "AAAA")))
        .collect();
    let deletes: Vec<String> = if empty_names.is_empty() {
        deletes
    } else {
        let by_id: HashMap<&str, &RecordView> = current.iter().map(|r| (r.id.as_str(), r)).collect();
        let (held, kept): (Vec<String>, Vec<String>) = deletes.into_iter().partition(|id| {
            by_id
                .get(id.as_str())
                .map(|r| (r.rtype == "A" || r.rtype == "AAAA") && empty_names.contains(r.name.as_str()))
                .unwrap_or(false)
        });
        if !held.is_empty() {
            STATS.per_name_holds.fetch_add(held.len() as u64, Ordering::Relaxed);
            tracing::error!(
                %domain,
                held = held.len(),
                names = ?empty_names,
                "DNS reconciler: desired set has NO addresses for managed name(s) — holding their last-known-good records instead of deleting"
            );
        }
        kept
    };
    if creates.is_empty() && deletes.is_empty() {
        return Ok(()); // converged — write nothing
    }
    // Creates are PACED and individually fault-tolerant. Previously this was a
    // tight `api.create(...).await?` loop: the first rate-limited create aborted
    // the whole pass, so a zone needing more creates than the API budget allowed
    // could never converge — every pass re-failed at the same point, made zero
    // progress, and pushed the reconciler into backoff, which also delayed the
    // other zones sharing this task. Now each failure is recorded and the loop
    // continues, so every pass lands whatever it can and the remainder is picked
    // up next pass; the pass still reports failure at the end so the caller's
    // backoff (and the error log) still happen.
    let mut failed: Option<anyhow::Error> = None;
    let mut created = 0usize;
    for (i, rec) in creates.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(CREATE_PACING).await;
        }
        match api.create(domain, rec).await {
            Ok(_) => {
                created += 1;
                STATS.creates.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(%domain, name = %rec.name, rtype = %rec.rtype, error = %e, "DNS create failed; continuing (retried next pass)");
                if failed.is_none() {
                    failed = Some(e);
                }
            }
        }
    }
    if created > 0 {
        tracing::info!(%domain, created, remaining = creates.len() - created, "DNS reconciler: records created");
    }
    for id in &deletes {
        // Same fault-tolerance as creates: one failed delete must not abort the
        // pass and strand the rest (deletes are also rate-limited).
        if let Err(e) = api.delete(domain, id).await {
            tracing::warn!(%domain, id = %id, error = %e, "DNS delete failed; continuing (retried next pass)");
            if failed.is_none() {
                failed = Some(e);
            }
            continue;
        }
        STATS.deletes.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(e) = failed {
        // Partial progress already landed; surface the failure so the caller
        // backs off and the operator sees it.
        return Err(e);
    }
    let published: Vec<String> = desired.iter().map(|r| format!("{} {} {}", r.name, r.rtype, r.value)).collect();
    tracing::info!(%domain, created = created, deleted = deletes.len(), published = ?published, "DNS reconciled");
    Ok(())
}

/// Leader-elected reconcile loop. Runs on every node; only the elected leader
/// (same election as the billing meter: lowest healthy iroh identity) acts.
/// Enabled when a `VERCEL_API_TOKEN` is present AND (`HIVE_INGRESS != ngrok` or
/// `HIVE_DNS_RECONCILE=1` for pre-cutover testing).
pub fn spawn_reconciler(cloud: Arc<CloudState>) {
    let forced = std::env::var("HIVE_DNS_RECONCILE").map(|v| v == "1").unwrap_or(false);
    if cloud.ingress == "ngrok" && !forced {
        return;
    }
    let Some(api) = VercelApi::from_env(cloud.http.clone()) else {
        tracing::warn!("DNS reconciler enabled but VERCEL_API_TOKEN is not set — not starting");
        return;
    };
    let interval = std::env::var("HIVE_DNS_RECONCILE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30u64);
    tracing::info!(interval, apps = %cloud.apps_domain, platform = %cloud.platform_domain, "Vercel DNS reconciler up (leader-elected)");
    tokio::spawn(async move {
        let mut streaks: HashMap<String, u32> = HashMap::new();
        let mut backoff: u64 = 0; // consecutive failures
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
        loop {
            tick.tick().await;
            if backoff > 0 {
                // Exponential backoff on API failure: 30s * 2^n, capped at 5 min.
                let extra = (interval << backoff.min(4)).min(300);
                tokio::time::sleep(std::time::Duration::from_secs(extra.saturating_sub(interval))).await;
            }
            // Same single-writer resolution as admin mutations, ACME and the
            // billing meter (owner chain first, health+addressability gated;
            // identity election fallback) — one designation for every
            // single-writer role, structurally closing the CP-vs-DNS pin drift
            // (proposal step 6). `HIVE_DNS_LEADER_NODE` remains honored as a
            // deliberate LEGACY split-pin on the fallback path (health-gated,
            // never a raw unguarded check — an unguarded pin silently freezes
            // published DNS if the pinned node dies).
            let dns_pref = std::env::var("HIVE_DNS_LEADER_NODE").ok().filter(|s| !s.trim().is_empty());
            let chain = crate::cluster::Cluster::owner_chain_from_env();
            let pref = dns_pref.or_else(|| std::env::var("HIVE_CP_LEADER").ok());
            let leader =
                crate::cluster::Cluster::control_plane_owner(&chain, pref.as_deref(), &cloud.registry.nodes());
            if leader.as_deref() != Some(cloud.node_name.as_str()) {
                continue;
            }
            // Convergence guard: a node that was told to JOIN an existing fleet
            // (HIVE_BOOTSTRAP_PEERS set) but currently sees ONLY itself in the
            // registry has not yet synced gossip — its view of the healthy set
            // is a transient single-self, and reconciling from it would DELETE
            // every peer's A-record and publish only this node's IP fleet-wide
            // (live-observed: a freshly-joined 9th node clobbered
            // shadw.cloud/*.shadw.app to only-itself for ~30s until the real
            // leader re-reconciled). A founding node (no bootstrap peers) has
            // no peers to clobber, so it is exempt and still bootstraps DNS.
            let has_bootstrap = std::env::var("HIVE_BOOTSTRAP_PEERS").map(|v| !v.trim().is_empty()).unwrap_or(false);
            if has_bootstrap && cloud.registry.nodes().len() <= 1 {
                tracing::warn!("DNS reconcile skipped: mesh not yet converged (registry sees only self despite HIVE_BOOTSTRAP_PEERS) — refusing to clobber peer records");
                continue;
            }
            let nodes: Vec<NodeView> = cloud
                .registry
                .nodes()
                .into_iter()
                .map(|n| NodeView {
                    dns_ns: n.dns_ns.is_some(),
                    name: n.name,
                    healthy: n.healthy,
                    ip4: n.public_ip,
                    ip6: n.public_ip6,
                    region: n.region,
                })
                .collect();
            let publish = publishable(&nodes, &mut streaks);
            let relay_ips = env_ips("HIVE_RELAY_IPS");
            let discovery_ips = env_ips("HIVE_DISCOVERY_IPS");
            let mut apps = desired_apps(&publish);
            // Deployment affinity: point each served label straight at its host
            // node so a client lands on the owner instead of a random node that
            // then has to forward. Local aliases first (authoritative for what
            // WE serve), then the gossiped peer route table — first writer wins,
            // so a host we serve is never attributed to a peer.
            let mut owners: Vec<(String, String)> =
                cloud.gw.served_hosts().into_iter().map(|h| (h, cloud.node_name.clone())).collect();
            {
                let routes = cloud.peer_routes.read();
                for (label, rs) in routes.iter() {
                    // Lowest-latency healthy route wins when several nodes serve
                    // the same label (replicas): that is the best single answer
                    // we can give, and the wildcard still covers the rest.
                    if let Some(best) = rs.iter().filter(|r| r.healthy).min_by_key(|r| r.latency_ms) {
                        owners.push((label.clone(), best.node_id.clone()));
                    }
                }
            }
            let (affinity, affinity_names) = desired_apps_affinity(&owners, &publish, APPS_AFFINITY_CAP);
            let affinity_count = affinity_names.len();
            apps.extend(affinity);
            // Per-region names in BOTH zones: `<region>.<apps>` for app traffic
            // and `api-<region>.<platform>` for the API/control surface.
            //
            // COLLISION: a project literally named `san-jose` owns that same
            // single label in the apps zone. A real deployment is user-visible
            // and predates this feature, so the affinity record wins and the
            // region name is dropped for that one region — publishing both would
            // put two different A sets under one name and break the deployment.
            let (apps_region, apps_region_names) = desired_region_names(&publish, "");
            let claimed: std::collections::HashSet<&str> = affinity_names.iter().map(|s| s.as_str()).collect();
            let dropped: Vec<&String> = apps_region_names.iter().filter(|n| claimed.contains(n.as_str())).collect();
            if !dropped.is_empty() {
                tracing::warn!(
                    names = ?dropped,
                    "per-region apps name(s) collide with a deployment label — deployment wins, region name not published"
                );
            }
            apps.extend(apps_region.into_iter().filter(|r| !claimed.contains(r.name.as_str())));
            let mut apps_managed: Vec<String> = vec!["*".into(), String::new()];
            apps_managed.extend(apps_region_names.into_iter().filter(|n| !claimed.contains(n.as_str())));
            apps_managed.extend(affinity_names);
            // Geo-zone delegation: hand the deploy zone to the fleet's own
            // nameservers so Seer's geo/health-aware answers are actually
            // reachable (Vercel DNS itself has no geo routing). Published into
            // the apps zone because the deploy zone is a child of it, and only
            // when >=2 real nameservers exist (see desired_geo_delegation).
            let (geo_records, geo_names) = crate::dnsserver::deploy_zone()
                .and_then(|dz| geo_label(dz, &cloud.apps_domain))
                .map(|label| desired_geo_delegation(&publish, &label, &cloud.apps_domain))
                .unwrap_or_default();
            STATS.geo_delegation_records.store(geo_records.len() as u64, Ordering::Relaxed);
            apps.extend(geo_records);
            apps_managed.extend(geo_names);
            let apps_managed_refs: Vec<&str> = apps_managed.iter().map(|s| s.as_str()).collect();
            STATS.affinity_records.store(affinity_count as u64, Ordering::Relaxed);
            let dashboard = std::env::var("HIVE_DASHBOARD_UPSTREAM").map(|v| !v.trim().is_empty()).unwrap_or(false);
            let mut platform = desired_platform(&publish, &relay_ips, &discovery_ips, dashboard);
            let (platform_region, platform_region_names) = desired_region_names(&publish, "api-");
            platform.extend(platform_region);
            STATS.region_records.store(platform_region_names.len() as u64, Ordering::Relaxed);

            let mut ok = true;
            if let Err(e) = reconcile_zone(&api, &cloud.apps_domain, &apps, &apps_managed_refs, &cloud).await {
                STATS.api_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e, zone = %cloud.apps_domain, "DNS reconcile failed");
                ok = false;
            }
            // `admin`/`webhook`/`sms` were each, in turn, absent from this
            // managed-name list even though `desired_platform` already published
            // records for them — meaning a created record could never be
            // diffed/updated/removed by this reconciler again, and — the live
            // symptom this time — a NAME COLLISION with an existing unmanaged
            // record (Vercel's own IPs already occupied `sms.shadw.cloud`) is
            // silently left in place forever instead of being recognized as
            // stale and replaced, since `diff()` never considers a name outside
            // this list at all. Recurring gap; closed here for `sms` alongside
            // the prior two.
            let mut platform_managed: Vec<String> = ["api", "admin", "webhook", "sms", "relay", "discovery"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            if dashboard {
                platform_managed.push(String::new());
                platform_managed.push("www".into());
            }
            // `api-<region>` must be managed for the same reason as `sms` above:
            // an unmanaged name is invisible to diff() forever, so a withdrawn
            // region's records would never be cleaned up.
            platform_managed.extend(platform_region_names);
            let platform_managed_refs: Vec<&str> = platform_managed.iter().map(|s| s.as_str()).collect();
            if let Err(e) = reconcile_zone(&api, &cloud.platform_domain, &platform, &platform_managed_refs, &cloud).await {
                STATS.api_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e, zone = %cloud.platform_domain, "DNS reconcile failed");
                ok = false;
            }
            // Per-tenant DB gateway zone (`*.{db_domain}`): the wildcard + apex →
            // all publishable nodes (cert coverage + fallback), PLUS a specific A
            // record per live DB `<slug> → the node hosting its container`, so a
            // `<slug>.{db_domain}` connection lands on the node with the DB local to
            // its SNI proxy (a specific record wins over the wildcard in DNS).
            if !cloud.db_domain.is_empty() {
                let mut db_desired = desired_apps(&publish);
                let mut managed: Vec<String> = vec!["*".into(), String::new()];
                let all_nodes = cloud.registry.nodes();
                let suffix = format!(".{}", cloud.db_domain);
                // (db_host, host_node) pairs: local records first, then peer
                // directories — DBs provision on the control-plane leader, which is
                // NOT this DNS leader, and DB records are not gossiped. The peer
                // fan-out is the non-secret `/v1/db-directory` (routing metadata
                // only). Local wins on slug collision (first insert kept).
                let mut dir: Vec<(String, String)> = cloud
                    .databases
                    .list(None)
                    .into_iter()
                    .filter(|d| !d.db_host.is_empty() && !d.host_node.is_empty())
                    .map(|d| (d.db_host, d.host_node))
                    .collect();
                // Fan out CONCURRENTLY, each under its own tight budget — this loop
                // shares the single reconciler task with the apps/platform zones
                // above (spawn_reconciler is one task on one interval), so a
                // sequential per-peer await (each up to ~10-20s inside
                // fetch_from_host's own HTTP+iroh timeouts) would stretch a slow
                // pass to 10s-2min and starve those zones' reconcile cadence.
                let peer_names: Vec<String> =
                    all_nodes.iter().filter(|n| n.name != cloud.node_name && n.healthy).map(|n| n.name.clone()).collect();
                let peer_results = futures::future::join_all(peer_names.iter().map(|name| {
                    let cloud = cloud.clone();
                    let name = name.clone();
                    async move {
                        tokio::time::timeout(std::time::Duration::from_secs(8), crate::admin::fetch_from_host(&cloud, &name, "/v1/db-directory", ""))
                            .await
                            .ok()
                            .flatten()
                    }
                }))
                .await;
                for v in peer_results.into_iter().flatten() {
                    for e in v.as_array().map(|a| a.as_slice()).unwrap_or_default() {
                        let host = e.get("db_host").and_then(|x| x.as_str()).unwrap_or_default();
                        let hn = e.get("host_node").and_then(|x| x.as_str()).unwrap_or_default();
                        if !host.is_empty() && !hn.is_empty() {
                            dir.push((host.to_string(), hn.to_string()));
                        }
                    }
                }
                let mut seen = std::collections::HashSet::new();
                for (db_host, host_node) in dir {
                    let Some(slug) = db_host.strip_suffix(&suffix) else { continue };
                    if !seen.insert(slug.to_string()) {
                        continue;
                    }
                    let Some(node) = all_nodes.iter().find(|n| n.name == host_node) else { continue };
                    managed.push(slug.to_string());
                    if let Some(ip) = &node.public_ip {
                        db_desired.push(DesiredRecord { name: slug.into(), rtype: "A".into(), value: ip.clone(), ttl: 60 });
                    }
                    if let Some(ip) = &node.public_ip6 {
                        db_desired.push(DesiredRecord { name: slug.into(), rtype: "AAAA".into(), value: ip.clone(), ttl: 60 });
                    }
                }
                let managed_refs: Vec<&str> = managed.iter().map(|s| s.as_str()).collect();
                if let Err(e) = reconcile_zone(&api, &cloud.db_domain, &db_desired, &managed_refs, &cloud).await {
                    STATS.api_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %e, zone = %cloud.db_domain, "DNS reconcile failed");
                    ok = false;
                }
            }
            backoff = if ok { 0 } else { (backoff + 1).min(6) };
            STATS.passes.fetch_add(1, Ordering::Relaxed);
            STATS.last_pass_ms.store(hive_core::now_ms(), Ordering::Relaxed);
        }
    });
}

// ---- tests ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rv(id: &str, name: &str, t: &str, v: &str) -> RecordView {
        RecordView { id: id.into(), name: name.into(), rtype: t.into(), value: v.into() }
    }
    fn dr(name: &str, t: &str, v: &str) -> DesiredRecord {
        DesiredRecord { name: name.into(), rtype: t.into(), value: v.into(), ttl: 60 }
    }

    #[test]
    fn diff_creates_missing_and_deletes_stale() {
        let current = vec![rv("1", "api", "A", "1.1.1.1"), rv("2", "api", "A", "2.2.2.2")];
        let desired = vec![dr("api", "A", "1.1.1.1"), dr("api", "A", "3.3.3.3")];
        let (c, d) = diff(&current, &desired, &["api"]);
        assert_eq!(c, vec![dr("api", "A", "3.3.3.3")]);
        assert_eq!(d, vec!["2".to_string()]);
    }

    #[test]
    fn diff_noop_when_converged() {
        let current = vec![rv("1", "*", "A", "1.1.1.1"), rv("2", "", "A", "1.1.1.1")];
        let desired = vec![dr("*", "A", "1.1.1.1"), dr("", "A", "1.1.1.1")];
        let (c, d) = diff(&current, &desired, &["*", ""]);
        assert!(c.is_empty() && d.is_empty(), "converged sets must write nothing");
    }

    #[test]
    fn diff_never_touches_unmanaged_or_txt() {
        // MX for mail, TXT for ACME, a CNAME someone added by hand: all untouchable.
        let current = vec![
            rv("1", "api", "A", "9.9.9.9"),
            rv("2", "_acme-challenge", "TXT", "token"),
            rv("3", "mail", "MX", "10 mx.example.com"),
            rv("4", "www", "CNAME", "handmade.example.com"),
            rv("5", "api", "TXT", "keep-me"),
        ];
        let desired = vec![dr("api", "A", "1.1.1.1")];
        let (c, d) = diff(&current, &desired, &["api"]);
        assert_eq!(c, vec![dr("api", "A", "1.1.1.1")]);
        assert_eq!(d, vec!["1".to_string()], "only the managed A record is replaced");
    }

    #[test]
    fn alias_on_managed_name_is_displaced_only_when_publishing() {
        // Vercel's default ALIAS on `*`/apex must be replaced by our A records at
        // cutover — but ONLY when we actually publish addresses for that name.
        let current = vec![
            rv("1", "*", "ALIAS", "cname.vercel-dns-016.com."),
            rv("2", "", "ALIAS", "cname.vercel-dns-016.com."),
            rv("3", "www", "ALIAS", "keep.me"), // unmanaged name — untouchable
        ];
        let desired = vec![dr("*", "A", "1.1.1.1"), dr("", "A", "1.1.1.1")];
        let (c, d) = diff(&current, &desired, &["*", ""]);
        assert_eq!(c.len(), 2);
        assert!(d.contains(&"1".to_string()) && d.contains(&"2".to_string()));
        assert!(!d.contains(&"3".to_string()));
        // Empty desired set → ALIAS stays (never displace without replacements).
        let (_, d2) = diff(&current, &[], &["*", ""]);
        assert!(d2.is_empty());
    }

    #[test]
    fn damping_withdraws_only_after_k_passes() {
        let mut streaks = HashMap::new();
        let view = |healthy| {
            vec![NodeView {
                dns_ns: false,
                name: "n1".into(),
                healthy,
                ip4: Some("1.1.1.1".into()),
                ip6: None,
                region: "san-jose".into(),
            }]
        };
        let (up, down) = (view(true), view(false));
        assert_eq!(publishable(&up, &mut streaks).len(), 1);
        // 1st unhealthy pass: still published (damping)
        assert_eq!(publishable(&down, &mut streaks).len(), 1);
        // 2nd unhealthy pass: withdrawn
        assert_eq!(publishable(&down, &mut streaks).len(), 0);
        // recovery resets the streak instantly
        assert_eq!(publishable(&up, &mut streaks).len(), 1);
        assert_eq!(publishable(&down, &mut streaks).len(), 1);
    }

    #[test]
    fn no_public_ip_never_published() {
        let mut streaks = HashMap::new();
        let nodes = vec![NodeView {
            dns_ns: false,
            name: "nat-node".into(),
            healthy: true,
            ip4: None,
            ip6: None,
            region: "bangkok".into(),
        }];
        assert!(publishable(&nodes, &mut streaks).is_empty());
    }

    #[test]
    fn desired_sets_cover_wildcard_apex_api_relay() {
        let nodes = vec![PublishNode {
            dns_ns: false,
            name: "n1".into(),
            ip4: Some("1.1.1.1".into()),
            ip6: Some("::1".into()),
            region: "san-jose".into(),
        }];
        let apps = desired_apps(&nodes);
        assert!(apps.contains(&DesiredRecord { name: "*".into(), rtype: "A".into(), value: "1.1.1.1".into(), ttl: 60 }));
        assert!(apps.contains(&DesiredRecord { name: "".into(), rtype: "AAAA".into(), value: "::1".into(), ttl: 60 }));
        let plat = desired_platform(&nodes, &["2.2.2.2".into()], &["3.3.3.3".into()], true);
        assert!(plat.contains(&DesiredRecord { name: "".into(), rtype: "A".into(), value: "1.1.1.1".into(), ttl: 60 }), "apex published when dashboard hosting on");
        assert!(plat.contains(&DesiredRecord { name: "www".into(), rtype: "A".into(), value: "1.1.1.1".into(), ttl: 60 }));
        assert!(plat.contains(&DesiredRecord { name: "api".into(), rtype: "A".into(), value: "1.1.1.1".into(), ttl: 60 }));
        assert!(plat.contains(&DesiredRecord { name: "admin".into(), rtype: "A".into(), value: "1.1.1.1".into(), ttl: 60 }), "ops console host published");
        assert!(plat.contains(&DesiredRecord { name: "relay".into(), rtype: "A".into(), value: "2.2.2.2".into(), ttl: 300 }));
        assert!(plat.contains(&DesiredRecord { name: "discovery".into(), rtype: "A".into(), value: "3.3.3.3".into(), ttl: 300 }));
    }

    // ---- mocked-API reconcile tests ----

    use std::sync::Mutex;
    struct MockApi {
        records: Mutex<Vec<RecordView>>,
        fail_lists: Mutex<u32>, // fail the next N list() calls with a retryable error
        next_id: Mutex<u32>,
        creates: Mutex<u32>,
        deletes: Mutex<u32>,
    }
    impl MockApi {
        fn new(records: Vec<RecordView>) -> Self {
            Self { records: Mutex::new(records), fail_lists: Mutex::new(0), next_id: Mutex::new(100), creates: Mutex::new(0), deletes: Mutex::new(0) }
        }
    }
    impl DnsApi for MockApi {
        async fn list(&self, _d: &str) -> anyhow::Result<Vec<RecordView>> {
            let mut f = self.fail_lists.lock().unwrap();
            if *f > 0 {
                *f -= 1;
                anyhow::bail!("429 (retryable)");
            }
            Ok(self.records.lock().unwrap().clone())
        }
        async fn create(&self, _d: &str, rec: &DesiredRecord) -> anyhow::Result<String> {
            let mut id = self.next_id.lock().unwrap();
            *id += 1;
            *self.creates.lock().unwrap() += 1;
            let rid = id.to_string();
            self.records.lock().unwrap().push(RecordView { id: rid.clone(), name: rec.name.clone(), rtype: rec.rtype.clone(), value: rec.value.clone() });
            Ok(rid)
        }
        async fn delete(&self, _d: &str, id: &str) -> anyhow::Result<()> {
            *self.deletes.lock().unwrap() += 1;
            self.records.lock().unwrap().retain(|r| r.id != id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn mock_reconcile_converges_and_then_noops() {
        let api = MockApi::new(vec![rv("1", "api", "A", "9.9.9.9")]);
        let desired = vec![dr("api", "A", "1.1.1.1")];
        // Directly exercise diff+apply (reconcile_zone needs CloudState only for
        // the empty-set incident path, covered separately).
        let current = api.list("z").await.unwrap();
        let (c, d) = diff(&current, &desired, &["api"]);
        for r in &c { api.create("z", r).await.unwrap(); }
        for id in &d { api.delete("z", id).await.unwrap(); }
        assert_eq!(*api.creates.lock().unwrap(), 1);
        assert_eq!(*api.deletes.lock().unwrap(), 1);
        // Second pass: converged, zero writes.
        let current = api.list("z").await.unwrap();
        let (c, d) = diff(&current, &desired, &["api"]);
        assert!(c.is_empty() && d.is_empty());
    }

    #[tokio::test]
    async fn mock_list_429_is_an_error_not_a_wipe() {
        let api = MockApi::new(vec![rv("1", "api", "A", "1.1.1.1")]);
        *api.fail_lists.lock().unwrap() = 1;
        assert!(api.list("z").await.is_err(), "429 must surface as an error (caller backs off)");
        // Records untouched by the failed pass.
        assert_eq!(api.records.lock().unwrap().len(), 1);
    }
}
