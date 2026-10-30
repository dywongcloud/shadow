//! Vercel DNS reconciler: a leader-elected loop syncing healthy node public IPs to Vercel DNS
//! (plain authoritative DNS: a records API, no health checks, no geo steering) for the `api`,
//! wildcard and apex sets, `relay.`/`discovery.` from env IP lists, and the DB gateway zone (one
//! record per live database slug at its OWNER node; an owner that left the registry is a critical
//! incident, never repaired).
//!
//! Safety properties: delta-only writes; flap damping (`UNHEALTHY_PASSES_BEFORE_WITHDRAW` /
//! `HEALTHY_PASSES_BEFORE_REPUBLISH`); never publish an empty set (keep last-known-good and raise
//! an incident); NEVER DARK (no managed name ends a pass with neither addresses nor delegation:
//! restore-on-failure transactions, and an account-wide create block skips every
//! delete-before-create step); exponential backoff on 429/5xx. Regional steering stays in
//! `edge.rs`; the self-hosted Seer (`dnsserver.rs`) serves the delegated zones.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::state::CloudState;

/// Fewest PROVEN nameservers a delegation may be published with.
const MIN_PROVEN_NAMESERVERS: usize = 2;

/// How many consecutive unhealthy reconcile passes before a node's records are
/// withdrawn (flap damping).
pub const UNHEALTHY_PASSES_BEFORE_WITHDRAW: u32 = 2;

/// Consecutive HEALTHY passes before a withdrawn node's records return; with the withdraw threshold
/// it ends a flapping node's create/delete treadmill (429s, briefly deleted address records).
pub const HEALTHY_PASSES_BEFORE_REPUBLISH: u32 = 2;

/// Minimum age before an unknown `_acme-challenge.*` TXT may be swept as an orphan: far beyond any
/// legitimate order, and it spares a record that just raced the pass's own zone listing.
const ACME_ORPHAN_MIN_AGE_MS: u64 = 15 * 60 * 1000;

/// A DNS record as it exists at Vercel (subset we care about).
#[derive(Clone, Debug, PartialEq)]
pub struct RecordView {
    pub id: String,
    /// Subdomain relative to the zone: `""` (apex), `"api"`, `"*"`, …
    pub name: String,
    /// `A` | `AAAA` | `TXT` | …
    pub rtype: String,
    pub value: String,
    /// Vercel's creation timestamp (ms epoch) when the API reports it: the ACME orphan sweeper's
    /// minimum-age gate, so a just-placed challenge is never swept from under an in-flight order.
    pub created_ms: Option<u64>,
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
        let token = std::env::var("VERCEL_API_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())?;
        let team_id = std::env::var("VERCEL_TEAM_ID")
            .ok()
            .filter(|s| !s.is_empty());
        Some(Self {
            http,
            token,
            team_id,
        })
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
        let mut out: Vec<RecordView> = Vec::new();
        let mut until: Option<u64> = None;
        for _ in 0..50 {
            let mut rest = format!("domains/{domain}/records?limit=100");
            if let Some(u) = until {
                rest.push_str(&format!("&until={u}"));
            }
            let resp = self
                .http
                .get(self.url("v4", &rest))
                .bearer_auth(&self.token)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                anyhow::bail!(
                    "vercel list {domain}: {status}{}",
                    if retryable(status) {
                        " (retryable)"
                    } else {
                        ""
                    }
                );
            }
            let v: serde_json::Value = resp.json().await?;
            let page: Vec<RecordView> = v
                .get("records")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| {
                            Some(RecordView {
                                id: r.get("id").or_else(|| r.get("uid"))?.as_str()?.to_string(),
                                name: r.get("name")?.as_str()?.to_string(),
                                rtype: r.get("type")?.as_str()?.to_string(),
                                value: r.get("value")?.as_str()?.to_string(),
                                created_ms: r
                                    .get("created")
                                    .and_then(|c| c.as_u64())
                                    .or_else(|| r.get("createdAt").and_then(|c| c.as_u64())),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let empty = page.is_empty();
            out.extend(page);
            until = v
                .get("pagination")
                .and_then(|p| p.get("next"))
                .and_then(|n| n.as_u64());
            if empty || until.is_none() {
                break;
            }
        }
        Ok(out)
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
            anyhow::bail!(
                "vercel create {domain} {} {}: {status}{}",
                rec.rtype,
                rec.name,
                if retryable(status) {
                    " (retryable)"
                } else {
                    ""
                }
            );
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v.get("uid")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string())
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
            anyhow::bail!(
                "vercel delete {domain}/{id}: {status}{}",
                if retryable(status) {
                    " (retryable)"
                } else {
                    ""
                }
            );
        }
        Ok(())
    }
}

/// A publishable node: healthy (or inside the damping window) with a public IP.
#[derive(Clone, Debug)]
pub struct PublishNode {
    pub name: String,
    pub ip4: Option<String>,
    pub ip6: Option<String>,
    /// This node CLAIMS to answer authoritative DNS on a public `:53` (`NodeInfo::dns_ns`):
    /// necessary for the geo NS set, never sufficient (see `desired_geo_delegation`).
    pub dns_ns: bool,
    /// This node's Seer also serves the platform API zone (gossiped `NodeInfo::dns_api`): required
    /// on top of `dns_ns` for the `api` label's NS set (see `desired_api_delegation`).
    pub dns_api: bool,
    /// Peers have PROVEN, from their own hosts across the public internet, that this node answers
    /// DNS usefully (`dns_probe::validate_nameservers`); only this gates the NS set.
    pub dns_validated: bool,
    /// The node's region code (`san-jose`, `bangkok`, ...) from the registry, so per-region names
    /// derive from the same health-damped set as every other record.
    pub region: String,
    /// This node's dashboard upstream answers within budget (`NodeInfo::dashboard`): gates the
    /// apex/`www` A-set, falling back to every publishable node below two capable nodes.
    pub dashboard: bool,
}

/// Desired records for the APPS zone (`*.{apps}` + apex), TTL 60.
pub fn desired_apps(nodes: &[PublishNode]) -> Vec<DesiredRecord> {
    let mut out = Vec::new();
    for name in ["*", ""] {
        for n in nodes {
            if let Some(ip) = &n.ip4 {
                out.push(DesiredRecord {
                    name: name.into(),
                    rtype: "A".into(),
                    value: ip.clone(),
                    ttl: 60,
                });
            }
            if let Some(ip) = &n.ip6 {
                out.push(DesiredRecord {
                    name: name.into(),
                    rtype: "AAAA".into(),
                    value: ip.clone(),
                    ttl: 60,
                });
            }
        }
    }
    out
}

/// The label the geo zone hangs off inside the apps zone, from `HIVE_DEPLOY_ZONE`
/// (`deploy.shadw.app` in `shadw.app` gives `deploy`); `None` when unset or not a child of it.
pub fn geo_label(deploy_zone: &str, apps_domain: &str) -> Option<String> {
    let dz = deploy_zone.trim().trim_matches('.').to_lowercase();
    let apps = apps_domain.trim().trim_matches('.').to_lowercase();
    if dz.is_empty() || apps.is_empty() {
        return None;
    }
    dz.strip_suffix(&format!(".{apps}"))
        .filter(|l| !l.is_empty() && !l.contains('.'))
        .map(str::to_string)
}

/// Stable nameserver label `ns-<node-name>`: the record set is a pure function of the registry,
/// since a renumbering index would silently repoint an NS at a different machine.
pub fn ns_label(node_name: &str) -> String {
    format!("ns-{}", node_name.trim().to_lowercase())
}

/// Geo-zone delegation: NS records on `<label>` at per-node nameservers in the apps zone (fully
/// qualified: Vercel rejects relative targets) plus glue. Eligibility is proof (`dns_validated`),
/// not the `dns_ns` claim; below `MIN_PROVEN_NAMESERVERS` it publishes AND MANAGES NOTHING, a
/// deliberate HOLD (fewer NS would blackhole the zone) that the caller logs and raises.
pub fn desired_geo_delegation(
    nodes: &[PublishNode],
    label: &str,
    apps_domain: &str,
) -> (Vec<DesiredRecord>, Vec<String>) {
    let mut out = Vec::new();
    let mut managed = vec![label.to_string()];
    for n in nodes.iter().filter(|n| n.dns_ns && n.dns_validated) {
        let ns = ns_label(&n.name);
        if let Some(ip) = &n.ip4 {
            out.push(DesiredRecord {
                name: ns.clone(),
                rtype: "A".into(),
                value: ip.clone(),
                ttl: 300,
            });
        }
        if let Some(ip) = &n.ip6 {
            out.push(DesiredRecord {
                name: ns.clone(),
                rtype: "AAAA".into(),
                value: ip.clone(),
                ttl: 300,
            });
        }
        if n.ip4.is_some() || n.ip6.is_some() {
            let target = format!("{ns}.{}", apps_domain.trim().trim_matches('.'));
            out.push(DesiredRecord {
                name: label.to_string(),
                rtype: "NS".into(),
                value: target,
                ttl: 300,
            });
            managed.push(ns);
        }
    }
    if out.iter().filter(|r| r.rtype == "NS").count() < MIN_PROVEN_NAMESERVERS {
        return (Vec::new(), Vec::new());
    }
    (out, managed)
}

/// Two-sided damping of the api-label delegation (the constants of `publishable`): a one-pass blip
/// never starts a cutover or plans NS deletes.
#[derive(Default)]
pub struct DelegationDamping {
    /// Consecutive passes with >=2 PROVEN api-capable nameservers.
    ready: u32,
    /// Consecutive below-floor passes with ZERO api-capable declarations.
    undeclared: u32,
}

/// The api-label delegation decision for one pass; below the proven-capable floor it HOLDS whatever
/// is published (stale-but-answering beats dark), mirroring the geo path.
#[derive(Debug, PartialEq)]
pub enum ApiDelegation {
    /// Publish this NS set on `api` (>=2 proven api-capable nameservers,
    /// stable for `HEALTHY_PASSES_BEFORE_REPUBLISH` consecutive passes).
    Delegate(Vec<DesiredRecord>),
    /// Change NOTHING on `api` this pass: a delegation is (or may be) published but the capable set
    /// is below the floor, so the name is neither managed nor given its flat set (child address
    /// records would veto a later NS re-creation). The caller raises an incident.
    Hold,
    /// No delegation to protect: manage `api` and desire the flat A set (the pre-cutover behaviour,
    /// which also drives a TRUE disengagement's NS deletes through phase 0).
    Flat,
}

/// NS-only delegation of the `api` label in the PLATFORM zone to the fleet's nameservers instead of
/// the flat A set: no glue, and it also needs the gossiped `dns_api` capability. Eligibility is
/// proof; below the floor it HOLDS. `declared` is the UNDAMPED registry view; `delegated_now` is
/// whether NS was published at the last reconcile (`None` = unobserved, which holds: a fresh leader
/// never plans NS deletes on its first pass).
pub fn desired_api_delegation(
    nodes: &[PublishNode],
    apps_domain: &str,
    declared: bool,
    delegated_now: Option<bool>,
    damping: &mut DelegationDamping,
) -> ApiDelegation {
    let apps = apps_domain.trim().trim_matches('.');
    let mut out: Vec<DesiredRecord> = nodes
        .iter()
        .filter(|n| {
            n.dns_ns && n.dns_api && n.dns_validated && (n.ip4.is_some() || n.ip6.is_some())
        })
        .map(|n| DesiredRecord {
            name: "api".into(),
            rtype: "NS".into(),
            value: format!("{}.{apps}", ns_label(&n.name)),
            ttl: 300,
        })
        .collect();
    out.sort_by(|a, b| a.value.cmp(&b.value));
    let below_floor = |delegated_now: Option<bool>| {
        if delegated_now != Some(false) {
            ApiDelegation::Hold
        } else {
            ApiDelegation::Flat
        }
    };
    if out.len() >= MIN_PROVEN_NAMESERVERS {
        damping.undeclared = 0;
        damping.ready = damping.ready.saturating_add(1);
        if damping.ready >= HEALTHY_PASSES_BEFORE_REPUBLISH {
            return ApiDelegation::Delegate(out);
        }
        return below_floor(delegated_now);
    }
    damping.ready = 0;
    if declared {
        damping.undeclared = 0;
        return below_floor(delegated_now);
    }
    damping.undeclared = damping.undeclared.saturating_add(1);
    if damping.undeclared >= UNHEALTHY_PASSES_BEFORE_WITHDRAW {
        return ApiDelegation::Flat;
    }
    below_floor(delegated_now)
}

/// True for an alias minted PER DEPLOYMENT or PER BRANCH (`dpl-<id>`, `<project>-<sha>`,
/// `<project>-git-<branch>`): these labels grow with deployments and ride the wildcard, so only a
/// project's own PRODUCTION label is worth a record.
pub(crate) fn is_preview_label(label: &str) -> bool {
    label.starts_with("dpl-") || label.contains("-git-") || is_commit_alias(label)
}

/// An immutable per-commit alias like `myapp-c7416ec` (`<prefix>-<6..12 hex>`; `-git-` labels are
/// not). A project genuinely named `foo-abc123` is misclassified, which only keeps the wildcard
/// behaviour.
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

/// Deployment-affinity records for the APPS zone: one SPECIFIC A/AAAA per served host label at the
/// node that hosts it (it beats the wildcard, which stays the fallback). Only single-label names
/// and PUBLISHABLE nodes; `owners` is `(label, node_name)`, first writer wins (local READY labels,
/// then peer routes, then labels held but not READY); `cap` bounds the records (Vercel rate-limits
/// creates).
pub fn desired_apps_affinity(
    owners: &[(String, String)],
    nodes: &[PublishNode],
    cap: usize,
) -> (Vec<DesiredRecord>, Vec<String>) {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (label, node) in owners {
        if label.contains('.') {
            continue;
        }
        let label = label.split(':').next().unwrap_or(label);
        let label = label
            .split('.')
            .next()
            .unwrap_or(label)
            .trim()
            .to_ascii_lowercase();
        if label.is_empty() || label == "*" || label == "www" {
            continue;
        }
        if is_preview_label(&label) {
            continue;
        }
        if !seen.insert(label.clone()) {
            continue;
        }
        pairs.push((label, node.clone()));
    }
    pairs.sort();
    pairs.truncate(cap);

    let mut out = Vec::new();
    let mut managed = Vec::new();
    for (label, node) in pairs {
        let Some(n) = nodes.iter().find(|n| n.name == node) else {
            continue;
        };
        if n.ip4.is_none() && n.ip6.is_none() {
            continue;
        }
        managed.push(label.clone());
        if let Some(ip) = &n.ip4 {
            out.push(DesiredRecord {
                name: label.clone(),
                rtype: "A".into(),
                value: ip.clone(),
                ttl: 60,
            });
        }
        if let Some(ip) = &n.ip6 {
            out.push(DesiredRecord {
                name: label.clone(),
                rtype: "AAAA".into(),
                value: ip.clone(),
                ttl: 60,
            });
        }
    }
    (out, managed)
}

/// Per-region names `<prefix><region>` resolving to every publishable node in that region. Returns
/// `(records, names)`: the names MUST join the zone's managed list or the reconciler can never diff
/// or withdraw them. Regions that are not DNS-label-shaped are skipped; a collision with a project
/// label loses to its affinity record.
pub fn desired_region_names(
    nodes: &[PublishNode],
    prefix: &str,
) -> (Vec<DesiredRecord>, Vec<String>) {
    let mut by_region: std::collections::BTreeMap<String, Vec<&PublishNode>> = Default::default();
    for n in nodes {
        let r = n.region.trim().to_ascii_lowercase();
        if r.is_empty()
            || !r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            || r.starts_with('-')
            || r.ends_with('-')
        {
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
                out.push(DesiredRecord {
                    name: name.clone(),
                    rtype: "A".into(),
                    value: ip.clone(),
                    ttl: 60,
                });
            }
            if let Some(ip) = &n.ip6 {
                out.push(DesiredRecord {
                    name: name.clone(),
                    rtype: "AAAA".into(),
                    value: ip.clone(),
                    ttl: 60,
                });
            }
        }
    }
    (out, names)
}

/// Desired records for the PLATFORM zone: `api.` (TTL 60) plus `relay.`/`discovery.` (TTL 300, env
/// IP lists); `api`, `admin`, `webhook`, `sms` share the gateway set, `dashboard` adds the apex and
/// `www`, and `delegate_api` withholds the api flat set (NS-delegated or HELD: records at a zone
/// cut are occluded and the diff would fight itself).
pub fn desired_platform(
    nodes: &[PublishNode],
    relay_ips: &[String],
    discovery_ips: &[String],
    dashboard: bool,
    delegate_api: bool,
) -> Vec<DesiredRecord> {
    let mut out = Vec::new();
    let names: Vec<&str> = if delegate_api {
        vec!["admin", "webhook", "sms"]
    } else {
        vec!["api", "admin", "webhook", "sms"]
    };
    for name in names {
        for n in nodes {
            if let Some(ip) = &n.ip4 {
                out.push(DesiredRecord {
                    name: name.into(),
                    rtype: "A".into(),
                    value: ip.clone(),
                    ttl: 60,
                });
            }
            if let Some(ip) = &n.ip6 {
                out.push(DesiredRecord {
                    name: name.into(),
                    rtype: "AAAA".into(),
                    value: ip.clone(),
                    ttl: 60,
                });
            }
        }
    }
    if dashboard {
        let capable: Vec<&PublishNode> = nodes.iter().filter(|n| n.dashboard).collect();
        let apex_set: Vec<&PublishNode> = if capable.len() >= 2 {
            capable
        } else {
            nodes.iter().collect()
        };
        for name in ["", "www"] {
            for n in &apex_set {
                if let Some(ip) = &n.ip4 {
                    out.push(DesiredRecord {
                        name: name.into(),
                        rtype: "A".into(),
                        value: ip.clone(),
                        ttl: 60,
                    });
                }
                if let Some(ip) = &n.ip6 {
                    out.push(DesiredRecord {
                        name: name.into(),
                        rtype: "AAAA".into(),
                        value: ip.clone(),
                        ttl: 60,
                    });
                }
            }
        }
    }
    for (sub, ips) in [("relay", relay_ips), ("discovery", discovery_ips)] {
        for ip in ips {
            let rtype = if ip.contains(':') { "AAAA" } else { "A" };
            out.push(DesiredRecord {
                name: sub.into(),
                rtype: rtype.into(),
                value: ip.clone(),
                ttl: 300,
            });
        }
    }
    out
}

/// Vercel stores a hostname-valued record (NS, CNAME) with a trailing root dot, so a raw compare
/// never matches and the same record would be deleted and re-created on every pass.
fn norm_value(value: &str) -> String {
    value.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// The delta between existing and wanted records, touching ONLY managed names and ONLY A/AAAA/NS (a
/// TXT such as `_acme-challenge` is never ours to delete); an ALIAS squatting on a managed name is
/// deleted only when addresses are about to be published for it. Pure.
pub fn diff(
    current: &[RecordView],
    desired: &[DesiredRecord],
    managed_names: &[&str],
) -> (Vec<DesiredRecord>, Vec<String>) {
    let managed = |name: &str| managed_names.contains(&name);
    let addr_type = |t: &str| t == "A" || t == "AAAA" || t == "NS";

    let have: Vec<&RecordView> = current
        .iter()
        .filter(|r| managed(&r.name) && addr_type(&r.rtype))
        .collect();

    let alias_deletes: Vec<String> = current
        .iter()
        .filter(|r| {
            r.rtype == "ALIAS"
                && managed(&r.name)
                && desired
                    .iter()
                    .any(|d| d.name == r.name && (d.rtype == "A" || d.rtype == "AAAA"))
        })
        .map(|r| r.id.clone())
        .collect();

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

/// Observable reconciler counters (surfaced via the admin API).
#[derive(Default)]
pub struct ReconcilerStats {
    pub passes: AtomicU64,
    pub creates: AtomicU64,
    pub deletes: AtomicU64,
    pub api_errors: AtomicU64,
    pub empty_set_blocks: AtomicU64,
    pub per_name_holds: AtomicU64,
    /// Deployment-affinity records the last pass published: host labels that resolve straight to
    /// their owner instead of the all-nodes wildcard.
    pub affinity_records: AtomicU64,
    /// How many deterministic per-region names the last pass published (one per
    /// region with at least one publishable node).
    pub region_records: AtomicU64,
    pub last_pass_ms: AtomicU64,
    /// NS+glue records published for the delegated geo zone (0 = undelegated).
    pub geo_delegation_records: AtomicU64,
    /// NS records published for the `api` delegation this pass (0 = flat A set); acme.rs's
    /// delegated-zone gate keys on this LIVE state, never static zone config.
    pub api_delegation_records: AtomicU64,
    /// Nodes whose `dns_ns` claim is currently backed by peer proof.
    pub geo_ns_validated: AtomicU64,
    /// Nodes CLAIMING `dns_ns` that no peer set currently proves — the count an
    /// operator watches after a rollout or a security-group change.
    pub geo_ns_unproven: AtomicU64,
    /// Passes that HELD an existing delegation because fewer than two nameservers were proven (a
    /// climbing number means the zone runs on last-known-good NS records).
    pub geo_delegation_holds: AtomicU64,
    /// Passes that HELD the `api` label exactly as published because the proven api-capable set
    /// fell below the floor (the api counterpart of `geo_delegation_holds`).
    pub api_delegation_holds: AtomicU64,
    /// Disengagement NS deletes and cutovers SKIPPED while the create-health circuit was open: a
    /// climbing counter means delegations are deliberately left in place rather than stranding
    /// names dark.
    pub create_circuit_skips: AtomicU64,
    /// Delegation cutovers completed by the never-dark transaction (flat
    /// addresses removed AND the full NS set confirmed created, same pass).
    pub delegation_cutovers: AtomicU64,
    /// Cutover attempts rolled back (an NS create failed after the flat addresses were removed, so
    /// they were restored): climbing means a delegation is RETRYING, not stranding.
    pub delegation_cutover_rollbacks: AtomicU64,
    /// Orphaned `_acme-challenge.*` TXT records swept (no in-flight order).
    pub acme_orphans_swept: AtomicU64,
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
    api_delegation_records: AtomicU64::new(0),
    geo_ns_validated: AtomicU64::new(0),
    geo_ns_unproven: AtomicU64::new(0),
    geo_delegation_holds: AtomicU64::new(0),
    api_delegation_holds: AtomicU64::new(0),
    create_circuit_skips: AtomicU64::new(0),
    delegation_cutovers: AtomicU64::new(0),
    delegation_cutover_rollbacks: AtomicU64::new(0),
    acme_orphans_swept: AtomicU64::new(0),
};

/// Upper bound on deployment-affinity records (Vercel rate-limits creates and the wildcard already
/// resolves every label). Deliberately modest: a much larger cap drew a sustained 429 storm that
/// aborted every pass.
const APPS_AFFINITY_CAP: usize = 60;

/// Delay between record creates: Vercel rate-limits creates individually, so an unpaced burst 429s
/// partway and the pass makes no progress.
const CREATE_PACING: std::time::Duration = std::time::Duration::from_millis(1100);

fn env_ips(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The registry facts one reconcile pass needs about a node, named rather than a positional tuple
/// (two adjacent fields share a type, the shape where a call site silently swaps them).
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
    /// Gossiped `NodeInfo::dns_api` — see `PublishNode::dns_api`.
    pub dns_api: bool,
    /// Currently PROVEN to serve DNS from off its own host, per
    /// `dns_probe::validate_nameservers` over the gossiped peer attestations.
    pub dns_validated: bool,
    /// Gossiped `NodeInfo::dashboard` — see `PublishNode::dashboard`.
    pub dashboard: bool,
}

/// Two-sided flap-damping state for `publishable`: one struct so the withdraw streaks, republish
/// streaks and withdrawn set cannot drift apart across passes.
#[derive(Default)]
pub struct PublishDamping {
    /// Consecutive-unhealthy streak per node (drives withdrawal).
    unhealthy: HashMap<String, u32>,
    /// Consecutive-healthy streak per WITHHELD node (drives re-admission).
    healthy: HashMap<String, u32>,
    /// Nodes currently withdrawn; the ONLY way back is `HEALTHY_PASSES_BEFORE_REPUBLISH`
    /// consecutive healthy passes.
    withheld: std::collections::HashSet<String>,
}

/// The publishable node set with two-sided flap damping: withdrawal after
/// `UNHEALTHY_PASSES_BEFORE_WITHDRAW` unhealthy passes stays fast (a dead node drains), re-entry
/// takes `HEALTHY_PASSES_BEFORE_REPUBLISH` healthy ones; a node first seen healthy publishes at
/// once. Pure.
pub fn publishable(nodes: &[NodeView], damping: &mut PublishDamping) -> Vec<PublishNode> {
    let mut out = Vec::new();
    for n in nodes {
        if n.ip4.is_none() && n.ip6.is_none() {
            continue;
        }
        if n.healthy {
            damping.unhealthy.insert(n.name.clone(), 0);
            if damping.withheld.contains(&n.name) {
                let streak = damping.healthy.entry(n.name.clone()).or_insert(0);
                *streak = streak.saturating_add(1);
                if *streak >= HEALTHY_PASSES_BEFORE_REPUBLISH {
                    damping.withheld.remove(&n.name);
                    damping.healthy.remove(&n.name);
                } else {
                    continue;
                }
            }
        } else {
            damping.healthy.insert(n.name.clone(), 0);
            let streak = damping.unhealthy.entry(n.name.clone()).or_insert(0);
            *streak = streak.saturating_add(1);
            if *streak >= UNHEALTHY_PASSES_BEFORE_WITHDRAW {
                damping.withheld.insert(n.name.clone());
            }
            if damping.withheld.contains(&n.name) {
                continue;
            }
        }
        out.push(PublishNode {
            name: n.name.clone(),
            ip4: n.ip4.clone(),
            ip6: n.ip6.clone(),
            region: n.region.clone(),
            dns_ns: n.dns_ns,
            dns_api: n.dns_api,
            dns_validated: n.dns_validated,
            dashboard: n.dashboard,
        });
    }
    let known: std::collections::HashSet<&String> = nodes.iter().map(|n| &n.name).collect();
    damping.unhealthy.retain(|k, _| known.contains(k));
    damping.healthy.retain(|k, _| known.contains(k));
    damping.withheld.retain(|k| known.contains(k));
    out
}

/// The write plan for one pass, sequenced so no name ends with NEITHER addresses NOR delegation
/// (Vercel forbids NS beside other records): cutovers are restore-on-failure transactions, a
/// disengagement deletes NS BEFORE creating flat addresses with a symmetric restore, and both are
/// skipped while the create-health circuit is open. Pure.
struct WritePlan {
    /// NS deletes that must precede any address create: a TRUE disengagement only (no desired NS);
    /// a rotation on a still-delegated name creates the replacement NS first.
    ns_deletes_first: Vec<String>,
    /// Per delegated name: the records blocking its NS creation (addresses plus any squatter:
    /// foreign NS, CNAME, ALIAS, stray TXT) and the NS records to create, run as ONE transaction
    /// with full restore on failure.
    cutovers: Vec<(String, Vec<RecordView>, Vec<DesiredRecord>)>,
    /// Everything else, in the usual creates-then-deletes order.
    creates: Vec<DesiredRecord>,
    deletes: Vec<String>,
}

fn plan_writes(
    current: &[RecordView],
    desired: &[DesiredRecord],
    managed_names: &[&str],
    mut creates: Vec<DesiredRecord>,
    mut deletes: Vec<String>,
) -> WritePlan {
    let by_id: HashMap<&str, &RecordView> = current.iter().map(|r| (r.id.as_str(), r)).collect();
    let norm = |v: &str| v.trim().trim_end_matches('.').to_ascii_lowercase();
    let mut cutovers = Vec::new();
    let delegated: Vec<&str> = managed_names
        .iter()
        .copied()
        .filter(|n| desired.iter().any(|d| d.name == *n && d.rtype == "NS"))
        .collect();
    for name in delegated {
        let missing_ns: Vec<DesiredRecord> = desired
            .iter()
            .filter(|d| d.name == name && d.rtype == "NS")
            .filter(|d| {
                !current
                    .iter()
                    .any(|r| r.name == name && r.rtype == "NS" && norm(&r.value) == norm(&d.value))
            })
            .cloned()
            .collect();
        if missing_ns.is_empty() {
            continue;
        }
        let blockers: Vec<RecordView> = current
            .iter()
            .filter(|r| r.name == name)
            .filter(|r| match r.rtype.as_str() {
                "A" | "AAAA" => true,
                "NS" => false,
                _ => true,
            })
            .map(|r| (*r).clone())
            .collect();
        if blockers.is_empty() {
            continue;
        }
        let blocker_ids: std::collections::HashSet<&str> =
            blockers.iter().map(|r| r.id.as_str()).collect();
        deletes.retain(|id| !blocker_ids.contains(id.as_str()));
        creates.retain(|c| {
            !(c.name == name && c.rtype == "NS" && missing_ns.iter().any(|m| m.value == c.value))
        });
        cutovers.push((name.to_string(), blockers, missing_ns));
    }
    let (ns_first, rest): (Vec<String>, Vec<String>) = deletes.into_iter().partition(|id| {
        by_id
            .get(id.as_str())
            .map(|r| {
                r.rtype == "NS" && !desired.iter().any(|d| d.name == r.name && d.rtype == "NS")
            })
            .unwrap_or(false)
    });
    WritePlan {
        ns_deletes_first: ns_first,
        cutovers,
        creates,
        deletes: rest,
    }
}

/// Is this `_acme-challenge.*` TXT an orphan candidate? The replicated challenge store is the
/// source of truth; a record under the age gate or of unreported age is KEPT (a delete is forever,
/// and failing open could sweep a live challenge after a leadership flap). The sweep runs every
/// pass, since a surviving TXT vetoes a later delegation (409). Pure.
fn is_orphan_candidate(in_flight: bool, created_ms: Option<u64>, now_ms: u64) -> bool {
    if in_flight {
        return false;
    }
    match created_ms {
        Some(c) => now_ms.saturating_sub(c) >= ACME_ORPHAN_MIN_AGE_MS,
        None => false,
    }
}

/// Cross-pass guard state threaded through `reconcile_zone`: the create-health circuit and the
/// never-dark alarm's edge trigger.
#[derive(Default)]
struct ReconcileGuards {
    /// Consecutive zone-passes whose creates ALL failed; non-zero means the circuit is OPEN (an
    /// account-wide create block still allows deletes), so delete-before-create sequences, whose
    /// restore is itself a create, are skipped: a stale-but-answering delegation beats a dark name.
    create_failing_passes: u32,
    /// Whether a create has SUCCEEDED during this leadership tenure; `false` (unknown) is treated
    /// like an open circuit, so a fresh leader never starts a sequence whose rollback is a create
    /// until it has proven one. Nothing is inherited from a departing leader.
    create_proven: bool,
    /// Delete-first steps skipped SOLELY for unproven create health (circuit closed), counted so an
    /// indefinite hold becomes an operator-visible incident instead of a silently parked cutover.
    proof_holds: u32,
}

/// Stale per-deployment records removed from the apps zone per pass (`dpl-` and per-commit labels
/// ride the wildcard; `-git-` records only once the deploy zone has a certificate). Deletes share
/// the rate-limited API.
const STALE_LABEL_SWEEP_PER_PASS: usize = 20;

/// Dead-host pins removed per pass: a specific record BEATS the wildcard, so a label pinned to a
/// host that no longer exists resolves only to a dead IP; an A/AAAA whose value we no longer desire
/// is managed with nothing desired and the diff deletes it.
const DEAD_PIN_SWEEP_PER_PASS: usize = 40;

/// Validity of one pass's incident observation: three passes at the backoff ceiling (5 min).
const INCIDENT_TTL_MS: u64 = 3 * 5 * 60 * 1000;

/// Delete-first steps skipped for want of create-health proof before the hold itself is alarmed (~5
/// minutes at the default cadence): long enough that a handover never alarms, short enough that a
/// parked delegation reaches an operator.
const CREATE_PROOF_ALARM_HOLDS: u32 = 10;

impl ReconcileGuards {
    fn creates_blocked(&self) -> bool {
        self.create_failing_passes > 0 || !self.create_proven
    }

    /// Why `creates_blocked()` is true — the two causes need different
    /// operator responses (a blocked account vs. an unexercised new leader).
    fn block_reason(&self) -> &'static str {
        if self.create_failing_passes > 0 {
            "creates are failing account-wide"
        } else {
            "create health is unproven since this node took the DNS leadership"
        }
    }

    /// This node just (re)took DNS leadership: only the PROOF state resets; the failure counters
    /// and incident edge-triggers are this node's own observations and stay, since each fails safe.
    fn begin_tenure(&mut self) {
        self.create_proven = false;
        self.proof_holds = 0;
    }

    /// Fold one zone-pass's create outcomes into the circuit. A pass with no
    /// creates says nothing; any success closes the circuit immediately.
    fn record_pass(&mut self, attempted: usize, succeeded: usize) {
        if succeeded > 0 {
            self.create_failing_passes = 0;
            self.create_proven = true;
            self.proof_holds = 0;
        } else if attempted > 0 {
            self.create_failing_passes = self.create_failing_passes.saturating_add(1);
        }
    }

    /// One delete-first step was skipped: only a skip for MISSING PROOF counts here (an open
    /// circuit is already alarmed by the create-failure incident).
    fn note_skip(&mut self, cloud: &Arc<CloudState>, domain: &str) {
        if self.create_proven || self.create_failing_passes > 0 {
            return;
        }
        self.proof_holds = self.proof_holds.saturating_add(1);
        if self.proof_holds < CREATE_PROOF_ALARM_HOLDS {
            return;
        }
        cloud.incidents.open(crate::incidents::OpenReq {
            condition: "dns:create-proof-hold".into(),
            ttl_ms: INCIDENT_TTL_MS,
            title: "DNS delegation transition parked: create health unproven".into(),
            severity: crate::incidents::Severity::Major,
            affected: vec!["dns".into()],
            message: format!(
                "The DNS leader has skipped {} delete-before-create step(s) in {domain} because no create has \
                 succeeded since it took leadership, so a rollback (which is itself a create) could not be \
                 guaranteed. Nothing is dark — the existing records keep serving — but the delegation change is \
                 not progressing. It clears itself the moment any create succeeds; if none is pending, check the \
                 Vercel account's create health.",
                self.proof_holds
            ),
        });
    }
}

/// The NEVER-DARK alarm: a MANAGED name this pass desired records for must end it with at least one
/// published (a name with nothing desired is a withdrawal, not an outage). Level-triggered through
/// `incidents::open`/`clear`; `end_count` is the pass-start listing folded with this pass's
/// CONFIRMED writes, never a re-list (it would race Vercel's eventually-consistent listing).
fn alarm_dark_names(
    domain: &str,
    desired: &[DesiredRecord],
    managed_names: &[&str],
    end_count: &HashMap<&str, usize>,
    cloud: &Arc<CloudState>,
) {
    for name in managed_names {
        let wants = desired
            .iter()
            .any(|d| d.name == *name && (d.rtype == "A" || d.rtype == "AAAA" || d.rtype == "NS"));
        let key = format!("dns:dark:{name}.{domain}");
        if !wants || end_count.get(*name).copied().unwrap_or(0) > 0 {
            cloud.incidents.clear(
                &key,
                "The name publishes records again (or nothing is desired for it any more).",
            );
            continue;
        }
        tracing::error!(
            %domain,
            name = %name,
            "DNS reconciler: managed name ends the pass with NEITHER addresses NOR delegation published — it is DARK"
        );
        cloud.incidents.open(crate::incidents::OpenReq {
            condition: key,
            ttl_ms: INCIDENT_TTL_MS,
            title: format!(
                "DNS name dark: {name}.{domain} serves neither addresses nor delegation"
            ),
            severity: crate::incidents::Severity::Major,
            affected: vec!["dns".into()],
            message: format!(
                "The reconcile pass desired records for {name}.{domain} but confirmed none published at its end — \
                 every create failed (an account-wide create block fails creates while still allowing deletes) or \
                 a delegation transition was interrupted. The name resolves to nothing. Check the Vercel account's \
                 create health and this node's reconcile logs."
            ),
        });
    }
}

/// One reconcile pass over one zone: returns the pass-start listing (the caller reads live
/// delegation state from it), `Err` on API failure (the caller backs off). Phase 0 runs a
/// disengagement's NS deletes, phase 1 the cutovers, phase 2 the ordinary creates-then-deletes; an
/// address delete is skipped when every create for its name failed (stale beats dark), and a
/// managed name with ZERO desired addresses keeps its records unless an NS delegation is desired
/// for it.
async fn reconcile_zone<A: DnsApi>(
    api: &A,
    domain: &str,
    desired: &[DesiredRecord],
    managed_names: &[&str],
    cloud: &Arc<CloudState>,
    guards: &mut ReconcileGuards,
) -> anyhow::Result<Vec<RecordView>> {
    let current = api.list(domain).await?;
    let deploy_ready = crate::dnsserver::deploy_zone()
        .map(crate::acme::zone_tls_ready)
        .unwrap_or(false);
    let sweep: Vec<String> = if domain.eq_ignore_ascii_case(cloud.apps_domain.trim().trim_matches('.')) {
        let mut names: Vec<String> = current
            .iter()
            .filter(|r| r.rtype == "A" || r.rtype == "AAAA")
            .map(|r| r.name.clone())
            .filter(|n| !n.contains('.') && is_preview_label(n))
            .filter(|n| deploy_ready || !n.contains("-git-"))
            .filter(|n| !managed_names.contains(&n.as_str()))
            .collect();
        names.sort();
        names.dedup();
        names.truncate(STALE_LABEL_SWEEP_PER_PASS);
        names
    } else {
        Vec::new()
    };
    if !sweep.is_empty() {
        tracing::info!(%domain, count = sweep.len(), "DNS reconciler: sweeping stale per-deployment labels (dpl-/commit aliases ride the wildcard)");
    }
    let dead_pins: Vec<String> = if domain.eq_ignore_ascii_case(cloud.apps_domain.trim().trim_matches('.')) {
        let mut allowed: std::collections::HashSet<String> = desired
            .iter()
            .filter(|r| r.rtype == "A" || r.rtype == "AAAA")
            .map(|r| r.value.clone())
            .collect();
        for ip in env_ips("HIVE_RELAY_IPS")
            .iter()
            .chain(env_ips("HIVE_DISCOVERY_IPS").iter())
        {
            allowed.insert(ip.clone());
        }
        let mut names: Vec<String> = current
            .iter()
            .filter(|r| (r.rtype == "A" || r.rtype == "AAAA") && !allowed.contains(&r.value))
            .map(|r| r.name.clone())
            .filter(|n| !managed_names.contains(&n.as_str()))
            .collect();
        names.sort();
        names.dedup();
        names.truncate(DEAD_PIN_SWEEP_PER_PASS);
        names
    } else {
        Vec::new()
    };
    if !dead_pins.is_empty() {
        tracing::info!(%domain, count = dead_pins.len(), "DNS reconciler: sweeping pins to hosts that no longer exist (a specific record beats the wildcard, so these names were dark)");
    }
    let managed_all: Vec<&str> = managed_names
        .iter()
        .copied()
        .chain(sweep.iter().map(String::as_str))
        .chain(dead_pins.iter().map(String::as_str))
        .collect();
    if !desired.iter().any(|r| r.rtype == "A" || r.rtype == "AAAA") {
        STATS.empty_set_blocks.fetch_add(1, Ordering::Relaxed);
        tracing::error!(%domain, "DNS reconciler: desired record set is EMPTY — keeping last-known-good records and raising an incident");
        cloud.incidents.open(crate::incidents::OpenReq {
            condition: format!("dns:no-publishable-nodes:{domain}"),
            ttl_ms: INCIDENT_TTL_MS,
            title: format!("DNS reconciler: no publishable nodes for {domain}"),
            severity: crate::incidents::Severity::Major,
            affected: vec!["dns".into()],
            message: "Desired DNS record set is empty (no healthy nodes with public IPs). Keeping last-known-good records published.".into(),
        });
        return Ok(current);
    }
    cloud.incidents.clear(
        &format!("dns:no-publishable-nodes:{domain}"),
        "Healthy nodes with public addresses are publishable again.",
    );
    let (creates, deletes) = diff(&current, desired, &managed_all);
    let empty_names: std::collections::HashSet<&str> = managed_names
        .iter()
        .copied()
        .filter(|n| {
            !desired
                .iter()
                .any(|d| d.name == *n && (d.rtype == "A" || d.rtype == "AAAA" || d.rtype == "NS"))
        })
        .collect();
    let deletes: Vec<String> = if empty_names.is_empty() {
        deletes
    } else {
        let by_id: HashMap<&str, &RecordView> =
            current.iter().map(|r| (r.id.as_str(), r)).collect();
        let (held, kept): (Vec<String>, Vec<String>) = deletes.into_iter().partition(|id| {
            by_id
                .get(id.as_str())
                .map(|r| {
                    (r.rtype == "A" || r.rtype == "AAAA") && empty_names.contains(r.name.as_str())
                })
                .unwrap_or(false)
        });
        if !held.is_empty() {
            STATS
                .per_name_holds
                .fetch_add(held.len() as u64, Ordering::Relaxed);
            tracing::error!(
                %domain,
                held = held.len(),
                names = ?empty_names,
                "DNS reconciler: desired set has NO addresses for managed name(s) — holding their last-known-good records instead of deleting"
            );
        }
        kept
    };
    let now_ms = hive_core::now_ms();
    for r in current.iter().filter(|r| {
        r.rtype == "TXT" && (r.name == "_acme-challenge" || r.name.starts_with("_acme-challenge."))
    }) {
        let fqdn = format!("{}.{}", r.name, domain);
        if !is_orphan_candidate(
            !cloud.acme_challenges.lookup(&fqdn).is_empty(),
            r.created_ms,
            now_ms,
        ) {
            continue;
        }
        match api.delete(domain, &r.id).await {
            Ok(_) => {
                STATS.acme_orphans_swept.fetch_add(1, Ordering::Relaxed);
                tracing::info!(%domain, name = %r.name, id = %r.id, "swept orphaned ACME dns-01 TXT (no in-flight order)");
            }
            Err(e) => {
                tracing::warn!(%domain, id = %r.id, error = %e, "ACME orphan sweep delete failed; retried next pass");
            }
        }
    }

    let plan = plan_writes(&current, desired, &managed_all, creates, deletes);
    let mut end_count: HashMap<&str, usize> = HashMap::new();
    for r in current
        .iter()
        .filter(|r| r.rtype == "A" || r.rtype == "AAAA" || r.rtype == "NS")
    {
        *end_count.entry(r.name.as_str()).or_insert(0) += 1;
    }
    if plan.ns_deletes_first.is_empty()
        && plan.cutovers.is_empty()
        && plan.creates.is_empty()
        && plan.deletes.is_empty()
    {
        alarm_dark_names(domain, desired, managed_names, &end_count, cloud);
        return Ok(current);
    }
    let mut failed: Option<anyhow::Error> = None;
    let mut creates_attempted = 0usize;
    let mut creates_succeeded = 0usize;

    let by_id: HashMap<&str, &RecordView> = current.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut disengaged_ns: HashMap<&str, Vec<&RecordView>> = HashMap::new();
    if !plan.ns_deletes_first.is_empty() && guards.creates_blocked() {
        let names: std::collections::BTreeSet<&str> = plan
            .ns_deletes_first
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).map(|r| r.name.as_str()))
            .collect();
        STATS
            .create_circuit_skips
            .fetch_add(plan.ns_deletes_first.len() as u64, Ordering::Relaxed);
        tracing::error!(
            %domain,
            names = ?names,
            reason = guards.block_reason(),
            "SKIPPING the disengagement NS deletes; the existing delegation stays (a stale-but-answering delegation beats a dark name, always)"
        );
        guards.note_skip(cloud, domain);
    } else {
        for id in &plan.ns_deletes_first {
            if let Err(e) = api.delete(domain, id).await {
                tracing::warn!(%domain, id = %id, error = %e, "DNS delete failed; continuing (retried next pass)");
                if failed.is_none() {
                    failed = Some(e);
                }
                continue;
            }
            STATS.deletes.fetch_add(1, Ordering::Relaxed);
            if let Some(r) = by_id.get(id.as_str()) {
                if let Some(c) = end_count.get_mut(r.name.as_str()) {
                    *c = (*c).saturating_sub(1);
                }
                disengaged_ns.entry(r.name.as_str()).or_default().push(r);
            }
        }
    }

    for (name, addr_records, ns_records) in &plan.cutovers {
        if guards.creates_blocked() {
            STATS.create_circuit_skips.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                %domain,
                name = %name,
                reason = guards.block_reason(),
                "SKIPPING the delegation cutover; the flat set stays (unless creates are known-good the rollback could not restore it)"
            );
            guards.note_skip(cloud, domain);
            continue;
        }
        let mut deleted_addr: Vec<&RecordView> = Vec::new();
        for r in addr_records {
            match api.delete(domain, &r.id).await {
                Ok(_) => {
                    STATS.deletes.fetch_add(1, Ordering::Relaxed);
                    if let Some(c) = end_count.get_mut(r.name.as_str()) {
                        *c = (*c).saturating_sub(1);
                    }
                    deleted_addr.push(r);
                }
                Err(e) => {
                    tracing::warn!(%domain, name = %name, id = %r.id, error = %e, "cutover: address delete failed; continuing (retried next pass)");
                    if failed.is_none() {
                        failed = Some(e);
                    }
                }
            }
        }
        let mut created_ns_ids: Vec<String> = Vec::new();
        let mut ns_ok = true;
        for (i, rec) in ns_records.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(CREATE_PACING).await;
            }
            creates_attempted += 1;
            match api.create(domain, rec).await {
                Ok(id) => {
                    creates_succeeded += 1;
                    STATS.creates.fetch_add(1, Ordering::Relaxed);
                    *end_count.entry(rec.name.as_str()).or_insert(0) += 1;
                    created_ns_ids.push(id);
                }
                Err(e) => {
                    tracing::warn!(%domain, name = %name, error = %e, "delegation cutover: NS create failed — rolling back");
                    if failed.is_none() {
                        failed = Some(e);
                    }
                    ns_ok = false;
                    break;
                }
            }
        }
        if ns_ok {
            STATS.delegation_cutovers.fetch_add(1, Ordering::Relaxed);
            tracing::info!(%domain, name = %name, ns = ns_records.len(), "delegation cutover complete: flat addresses removed, full NS set confirmed created");
            continue;
        }
        STATS
            .delegation_cutover_rollbacks
            .fetch_add(1, Ordering::Relaxed);
        let leftover_ns: Vec<&RecordView> = current
            .iter()
            .filter(|r| r.name == *name && r.rtype == "NS")
            .collect();
        for id in created_ns_ids
            .iter()
            .map(|s| s.as_str())
            .chain(leftover_ns.iter().map(|r| r.id.as_str()))
        {
            match api.delete(domain, id).await {
                Ok(_) => {
                    if let Some(c) = end_count.get_mut(name.as_str()) {
                        *c = (*c).saturating_sub(1);
                    }
                }
                Err(e) => {
                    tracing::warn!(%domain, name = %name, id = %id, error = %e, "cutover rollback: partial-NS delete failed; continuing");
                    if failed.is_none() {
                        failed = Some(e);
                    }
                }
            }
        }
        let mut restore_failed = 0usize;
        for r in &deleted_addr {
            let restore = DesiredRecord {
                name: r.name.clone(),
                rtype: r.rtype.clone(),
                value: r.value.clone(),
                ttl: 60,
            };
            tokio::time::sleep(CREATE_PACING).await;
            creates_attempted += 1;
            match api.create(domain, &restore).await {
                Ok(_) => {
                    creates_succeeded += 1;
                    STATS.creates.fetch_add(1, Ordering::Relaxed);
                    *end_count.entry(r.name.as_str()).or_insert(0) += 1;
                }
                Err(e) => {
                    restore_failed += 1;
                    tracing::warn!(%domain, name = %r.name, error = %e, "cutover rollback: address restore failed (retried next pass)");
                    if failed.is_none() {
                        failed = Some(e);
                    }
                }
            }
        }
        if restore_failed == 0 {
            tracing::error!(%domain, name = %name, "delegation cutover ROLLED BACK — flat addresses restored, NS delegation retries next pass");
        } else {
            tracing::error!(%domain, name = %name, restore_failed, "delegation cutover rollback INCOMPLETE — address restores failed; the never-dark alarm below opens an incident if the name now has neither addresses nor delegation");
        }
    }

    let mut created = 0usize;
    let mut addr_create_ok: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut addr_create_failed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (i, rec) in plan.creates.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(CREATE_PACING).await;
        }
        creates_attempted += 1;
        match api.create(domain, rec).await {
            Ok(_) => {
                created += 1;
                creates_succeeded += 1;
                STATS.creates.fetch_add(1, Ordering::Relaxed);
                if rec.rtype == "A" || rec.rtype == "AAAA" {
                    addr_create_ok.insert(rec.name.as_str());
                }
                *end_count.entry(rec.name.as_str()).or_insert(0) += 1;
            }
            Err(e) => {
                tracing::warn!(%domain, name = %rec.name, rtype = %rec.rtype, error = %e, "DNS create failed; continuing (retried next pass)");
                if rec.rtype == "A" || rec.rtype == "AAAA" {
                    addr_create_failed.insert(rec.name.as_str());
                }
                if failed.is_none() {
                    failed = Some(e);
                }
            }
        }
    }
    if created > 0 {
        tracing::info!(%domain, created, remaining = plan.creates.len() - created, "DNS reconciler: records created");
    }
    for (name, ns_records) in disengaged_ns {
        if !addr_create_failed.contains(name) || addr_create_ok.contains(name) {
            continue;
        }
        STATS
            .delegation_cutover_rollbacks
            .fetch_add(1, Ordering::Relaxed);
        let mut restore_failed: Vec<String> = Vec::new();
        for r in ns_records {
            let restore = DesiredRecord {
                name: r.name.clone(),
                rtype: "NS".into(),
                value: r.value.clone(),
                ttl: 300,
            };
            tokio::time::sleep(CREATE_PACING).await;
            creates_attempted += 1;
            match api.create(domain, &restore).await {
                Ok(_) => {
                    creates_succeeded += 1;
                    STATS.creates.fetch_add(1, Ordering::Relaxed);
                    *end_count.entry(r.name.as_str()).or_insert(0) += 1;
                }
                Err(e) => {
                    tracing::warn!(%domain, name = %name, error = %e, "disengagement rollback: NS restore failed (retried next pass)");
                    restore_failed.push(r.value.clone());
                    if failed.is_none() {
                        failed = Some(e);
                    }
                }
            }
        }
        if restore_failed.is_empty() {
            tracing::error!(%domain, name = %name, "disengagement ROLLED BACK — NS delegation restored, flat addresses retry next pass");
        } else {
            tracing::error!(%domain, name = %name, failed_records = ?restore_failed, "disengagement rollback INCOMPLETE — {name} has neither addresses nor delegation");
            cloud.incidents.open(crate::incidents::OpenReq {
                condition: format!("dns:disengage-rollback:{name}.{domain}"),
                ttl_ms: INCIDENT_TTL_MS,
                title: format!(
                    "DNS disengagement rollback incomplete: {name}.{domain} is dark"
                ),
                severity: crate::incidents::Severity::Major,
                affected: vec!["dns".into()],
                message: format!(
                    "The reconciler deleted the NS delegation for {name}.{domain} to restore the flat address set, \
                     but every address create failed AND {} NS restore(s) failed: {}. The name currently serves \
                     neither addresses nor delegation. Check the Vercel account's create health — a fair-use/402 \
                     block fails creates while still allowing deletes.",
                    restore_failed.len(),
                    restore_failed.join(", ")
                ),
            });
        }
    }
    let mut dark_deletes_skipped = 0usize;
    let mut deleted_ok = 0usize;
    for id in &plan.deletes {
        if let Some(r) = by_id.get(id.as_str()) {
            let is_addr = r.rtype == "A" || r.rtype == "AAAA";
            if is_addr
                && addr_create_failed.contains(r.name.as_str())
                && !addr_create_ok.contains(r.name.as_str())
            {
                dark_deletes_skipped += 1;
                tracing::warn!(
                    %domain,
                    name = %r.name,
                    rtype = %r.rtype,
                    "DNS delete SKIPPED to keep the name resolvable — every address create for \
                     it failed this pass (keeping the stale record beats going dark)"
                );
                continue;
            }
        }
        if let Err(e) = api.delete(domain, id).await {
            tracing::warn!(%domain, id = %id, error = %e, "DNS delete failed; continuing (retried next pass)");
            if failed.is_none() {
                failed = Some(e);
            }
            continue;
        }
        STATS.deletes.fetch_add(1, Ordering::Relaxed);
        deleted_ok += 1;
        if let Some(r) = by_id.get(id.as_str()) {
            if r.rtype == "A" || r.rtype == "AAAA" || r.rtype == "NS" {
                if let Some(c) = end_count.get_mut(r.name.as_str()) {
                    *c = (*c).saturating_sub(1);
                }
            }
        }
    }
    guards.record_pass(creates_attempted, creates_succeeded);
    if guards.create_failing_passes == 0 {
        cloud.incidents.clear(
            "dns:creates-failing",
            "A create succeeded again — the account-wide create block has lifted.",
        );
    }
    if guards.create_proven {
        cloud.incidents.clear(
            "dns:create-proof-hold",
            "A create succeeded under this leadership tenure — delete-first steps resume.",
        );
    }
    if guards.create_failing_passes >= 2 {
        cloud.incidents.open(crate::incidents::OpenReq {
            condition: "dns:creates-failing".into(),
            ttl_ms: INCIDENT_TTL_MS,
            title: "Vercel DNS creates failing account-wide".into(),
            severity: crate::incidents::Severity::Major,
            affected: vec!["dns".into()],
            message: format!(
                "Every create in {} consecutive zone reconcile pass(es) failed while deletes still work — the \
                 fair-use/402 block shape (witnessed 2026-08-04). Disengagement NS-deletes and delegation cutovers \
                 are being SKIPPED so no managed name is stranded dark. Investigate the Vercel account before any \
                 delegation change.",
                guards.create_failing_passes
            ),
        });
    }
    alarm_dark_names(domain, desired, managed_names, &end_count, cloud);
    if let Some(e) = failed {
        return Err(e);
    }
    let published: Vec<String> = desired
        .iter()
        .map(|r| format!("{} {} {}", r.name, r.rtype, r.value))
        .collect();
    tracing::info!(
        %domain,
        created = created,
        deleted = deleted_ok,
        planned_deletes = plan.deletes.len(),
        deletes_skipped_to_avoid_dark = dark_deletes_skipped,
        published = ?published,
        "DNS reconciled"
    );
    Ok(current)
}

/// Node-local sidecar for the flap memory under `persist::data_dir()`, like `dns_geo.json`: derived
/// node-local state that must NOT ride `PlatformSnapshot`, `store_sync` or any gossip snapshot arm.
const DAMPING_FILE: &str = "dns_damping.json";
const DAMPING_FORMAT_VERSION: u32 = 1;
/// Damping is a short-horizon memory, not a durable fact: a snapshot older than this describes a
/// fleet that has since changed shape and is dropped, not replayed.
const DAMPING_MAX_AGE_MS: u64 = 60 * 60 * 1000;
/// Cap enforced on LOAD as well as at runtime, so no on-disk file can reload
/// past it (the `dns_geo` `MAX_ENTRIES` rule).
const DAMPING_MAX_NODES: usize = 512;
const DAMPING_MAX_FILE_BYTES: u64 = 1 << 20;

/// On-disk shape of the flap memory: `BTreeMap` + a sorted `Vec` so equal state serializes to equal
/// BYTES; that byte-compare is the write gate, so an unchanged pass touches no disk.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct DampingDisk {
    #[serde(default)]
    v: u32,
    #[serde(default)]
    saved_ms: u64,
    #[serde(default)]
    unhealthy: std::collections::BTreeMap<String, u32>,
    #[serde(default)]
    healthy: std::collections::BTreeMap<String, u32>,
    #[serde(default)]
    withheld: Vec<String>,
    #[serde(default)]
    api_ready: u32,
    #[serde(default)]
    api_undeclared: u32,
}

/// The reconciler's cross-pass flap memory, surviving a leader handover. It follows THIS node's own
/// registry view (health verdicts are per-observer), so it is warmed on every node that could act;
/// replication is deliberately NOT used (a peer's streaks would publish on verdicts this node never
/// made), and the node-local, age-gated file covers a fleet-wide binary roll.
struct DampingMemory {
    publish: PublishDamping,
    api: DelegationDamping,
    /// Last bytes written; the change gate (empty = never written this boot).
    written: Vec<u8>,
    enabled: bool,
}

impl DampingMemory {
    /// Loads the persisted memory; EVERY failure degrades to an empty memory plus a log line: a bad
    /// scratch file costs one flap window, never a boot failure on the node that publishes the
    /// fleet's DNS.
    fn load() -> Self {
        let enabled = std::env::var("HIVE_DNS_DAMPING_PERSIST")
            .map(|v| v.trim() != "0")
            .unwrap_or(true);
        let mut out = Self {
            publish: PublishDamping::default(),
            api: DelegationDamping::default(),
            written: Vec::new(),
            enabled,
        };
        if !enabled {
            return out;
        }
        let path = crate::persist::data_dir().join(DAMPING_FILE);
        let Ok(meta) = std::fs::metadata(&path) else {
            return out;
        };
        if meta.len() > DAMPING_MAX_FILE_BYTES {
            tracing::warn!(bytes = meta.len(), path = %path.display(), "dns damping: file too large; starting empty");
            return out;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "dns damping: file unreadable; starting empty");
                return out;
            }
        };
        let disk: DampingDisk = match serde_json::from_str(&raw) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "dns damping: file corrupt; starting empty");
                return out;
            }
        };
        if disk.v != DAMPING_FORMAT_VERSION {
            tracing::warn!(
                found = disk.v,
                want = DAMPING_FORMAT_VERSION,
                "dns damping: file version mismatch; starting empty"
            );
            return out;
        }
        let age = hive_core::now_ms().saturating_sub(disk.saved_ms);
        if age > DAMPING_MAX_AGE_MS {
            tracing::info!(
                age_ms = age,
                "dns damping: persisted flap memory is stale; starting empty"
            );
            return out;
        }
        out.publish.unhealthy = disk.unhealthy.into_iter().take(DAMPING_MAX_NODES).collect();
        out.publish.healthy = disk.healthy.into_iter().take(DAMPING_MAX_NODES).collect();
        out.publish.withheld = disk.withheld.into_iter().take(DAMPING_MAX_NODES).collect();
        out.api.ready = disk.api_ready;
        out.api.undeclared = disk.api_undeclared;
        out.written = serde_json::to_vec(&out.rows()).unwrap_or_default();
        tracing::info!(
            withheld = out.publish.withheld.len(),
            api_ready = out.api.ready,
            age_ms = age,
            "dns damping: resumed persisted flap memory"
        );
        out
    }

    /// The comparable (timestamp-free) on-disk projection: every streak is CLAMPED to the threshold
    /// it feeds, which keeps the byte-compare write gate meaningful (a ticking counter would
    /// rewrite the file every pass).
    fn rows(&self) -> DampingDisk {
        let mut withheld: Vec<String> = self.publish.withheld.iter().cloned().collect();
        withheld.sort();
        DampingDisk {
            v: DAMPING_FORMAT_VERSION,
            saved_ms: 0,
            unhealthy: self
                .publish
                .unhealthy
                .iter()
                .map(|(k, v)| (k.clone(), (*v).min(UNHEALTHY_PASSES_BEFORE_WITHDRAW)))
                .collect(),
            healthy: self
                .publish
                .healthy
                .iter()
                .map(|(k, v)| (k.clone(), (*v).min(HEALTHY_PASSES_BEFORE_REPUBLISH)))
                .collect(),
            withheld,
            api_ready: self.api.ready.min(HEALTHY_PASSES_BEFORE_REPUBLISH),
            api_undeclared: self.api.undeclared.min(UNHEALTHY_PASSES_BEFORE_WITHDRAW),
        }
    }

    /// Persists when, and only when, the memory changed: atomic temp-file + fsync + rename on
    /// `spawn_blocking`, since the write ends in an fsync that must not run on a runtime worker.
    async fn persist(&mut self) {
        if !self.enabled {
            return;
        }
        let mut disk = self.rows();
        let Ok(gate) = serde_json::to_vec(&disk) else {
            return;
        };
        if gate == self.written {
            return;
        }
        disk.saved_ms = hive_core::now_ms();
        let Ok(json) = serde_json::to_vec(&disk) else {
            return;
        };
        match tokio::task::spawn_blocking(move || write_atomic(DAMPING_FILE, &json)).await {
            Ok(Ok(())) => self.written = gate,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "dns damping: save failed; flap memory is in-process only until it succeeds")
            }
            Err(e) => {
                tracing::warn!(error = %e, "dns damping: save task failed; flap memory is in-process only until it succeeds")
            }
        }
    }
}

fn write_atomic(name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = crate::persist::data_dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let f = std::fs::File::create(&tmp)?;
        let mut w = std::io::BufWriter::new(&f);
        w.write_all(bytes)?;
        w.flush()?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(name))
}

/// Names the reconciler owns in the PLATFORM zone: every name
/// `desired_platform`/`desired_region_names` publishes MUST be listed, or `diff()` never updates or
/// withdraws it. `api` is UNMANAGED during a HOLD, so the diff cannot plan the disengagement the
/// hold exists to prevent.
fn platform_managed_names(
    dashboard: bool,
    region_names: Vec<String>,
    api_hold: bool,
) -> Vec<String> {
    let mut managed: Vec<String> = ["api", "admin", "webhook", "sms", "relay", "discovery"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if dashboard {
        managed.push(String::new());
        managed.push("www".into());
    }
    managed.extend(region_names);
    if api_hold {
        managed.retain(|n| n != "api");
    }
    managed
}

/// The reconcile loop. It runs on EVERY node but writes only where `leadership::may_act(DnsWrite)`
/// admits; the flap memory is warmed on every pass BEFORE that gate, and the
/// `api_delegation_records`/`geo_ns_*` gauges stay leader-only. Enabled with `VERCEL_API_TOKEN` and
/// (`HIVE_INGRESS != ngrok` or `HIVE_DNS_RECONCILE=1`); `HIVE_DNS_RECONCILE_SECS` cadence,
/// exponential backoff capped at 5 min on the writer only. A takeover is a new ownership TERM,
/// never a `may_act` edge: only a new term resets the tenure-scoped observations (backoff,
/// `api_ns_published`), so a fresh leader's first pass HOLDS. A node told to JOIN a fleet
/// (`HIVE_BOOTSTRAP_PEERS`) that sees only itself has not synced gossip and neither reconciles nor
/// warms the memory.
pub fn spawn_reconciler(cloud: Arc<CloudState>) {
    let forced = std::env::var("HIVE_DNS_RECONCILE")
        .map(|v| v == "1")
        .unwrap_or(false);
    if cloud.ingress == "ngrok" && !forced {
        return;
    }
    let Some(api) = VercelApi::from_env(cloud.http.clone()) else {
        tracing::warn!("DNS reconciler enabled but VERCEL_API_TOKEN is not set — not starting");
        return;
    };
    let interval = std::env::var("HIVE_DNS_RECONCILE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30u64);
    tracing::info!(interval, apps = %cloud.apps_domain, platform = %cloud.platform_domain, "Vercel DNS reconciler up (leader-elected)");
    tokio::spawn(async move {
        let mut memory = DampingMemory::load();
        let mut guards = ReconcileGuards::default();
        let mut api_ns_published: Option<bool> = None;
        let mut consecutive_failures: u64 = 0;
        let mut leader_term: Option<u64> = None;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
        loop {
            tick.tick().await;
            if consecutive_failures > 0 && leader_term.is_some() {
                let extra = (interval << consecutive_failures.min(4)).min(300);
                tokio::time::sleep(std::time::Duration::from_secs(
                    extra.saturating_sub(interval),
                ))
                .await;
            }
            let registry_nodes = cloud.registry.nodes();
            let verdict = crate::leadership::check(&cloud, crate::leadership::Job::DnsWrite);
            let is_leader = verdict.allowed;
            let has_bootstrap = std::env::var("HIVE_BOOTSTRAP_PEERS")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            let sees_only_self = registry_nodes.len() <= 1;
            if has_bootstrap && sees_only_self {
                if is_leader {
                    tracing::warn!("DNS reconcile skipped: mesh not yet converged (registry sees only self despite HIVE_BOOTSTRAP_PEERS) — refusing to clobber peer records");
                }
                continue;
            }
            let verdicts = crate::dns_probe::validate_nameservers(&registry_nodes);
            let proven: std::collections::HashSet<&str> = verdicts
                .iter()
                .filter(|v| v.validated)
                .map(|v| v.node.as_str())
                .collect();
            let unproven: Vec<&str> = verdicts
                .iter()
                .filter(|v| !v.validated)
                .map(|v| v.node.as_str())
                .collect();
            let nodes: Vec<NodeView> = registry_nodes
                .iter()
                .map(|n| NodeView {
                    dns_ns: n.dns_ns.is_some(),
                    dns_api: n.dns_api,
                    dns_validated: proven.contains(n.name.as_str()),
                    dashboard: n.dashboard,
                    name: n.name.clone(),
                    healthy: n.healthy,
                    ip4: n.public_ip.clone(),
                    ip6: n.public_ip6.clone(),
                    region: n.region.clone(),
                })
                .collect();
            let publish = publishable(&nodes, &mut memory.publish);
            let api_declared = nodes.iter().any(|n| n.dns_ns && n.dns_api);
            let api_decision = desired_api_delegation(
                &publish,
                &cloud.apps_domain,
                api_declared,
                api_ns_published,
                &mut memory.api,
            );
            memory.persist().await;
            if !is_leader {
                if verdict.term.is_none() {
                    leader_term = None;
                    consecutive_failures = 0;
                    api_ns_published = None;
                }
                continue;
            }
            if leader_term != verdict.term {
                leader_term = verdict.term;
                guards.begin_tenure();
                tracing::info!(
                    withheld = memory.publish.withheld.len(),
                    api_ready = memory.api.ready,
                    "DNS reconciler: took leadership — resuming the warmed flap memory, holding delete-before-create steps until a create proves the account accepts them"
                );
            }
            STATS
                .geo_ns_validated
                .store(proven.len() as u64, Ordering::Relaxed);
            STATS
                .geo_ns_unproven
                .store(unproven.len() as u64, Ordering::Relaxed);
            let relay_ips = env_ips("HIVE_RELAY_IPS");
            let discovery_ips = env_ips("HIVE_DISCOVERY_IPS");
            let mut apps = desired_apps(&publish);
            let ready_local: Vec<String> = cloud.gw.served_hosts_ready();
            let mut owners: Vec<(String, String)> = ready_local
                .iter()
                .map(|h| (h.clone(), cloud.node_name.clone()))
                .collect();
            {
                let routes = cloud.peer_routes.read();
                for (label, rs) in routes.iter() {
                    if let Some(best) = rs.iter().filter(|r| r.healthy).min_by_key(|r| r.latency_ms)
                    {
                        owners.push((label.clone(), best.node_id.clone()));
                    }
                }
            }
            {
                let ready: std::collections::HashSet<&str> =
                    ready_local.iter().map(|s| s.as_str()).collect();
                for h in cloud.gw.served_hosts() {
                    if !ready.contains(h.as_str()) {
                        owners.push((h, cloud.node_name.clone()));
                    }
                }
            }
            let (affinity, affinity_names) =
                desired_apps_affinity(&owners, &publish, APPS_AFFINITY_CAP);
            let affinity_count = affinity_names.len();
            apps.extend(affinity);
            let (apps_region, apps_region_names) = desired_region_names(&publish, "");
            let claimed: std::collections::HashSet<&str> =
                affinity_names.iter().map(|s| s.as_str()).collect();
            let dropped: Vec<&String> = apps_region_names
                .iter()
                .filter(|n| claimed.contains(n.as_str()))
                .collect();
            if !dropped.is_empty() {
                tracing::warn!(
                    names = ?dropped,
                    "per-region apps name(s) collide with a deployment label — deployment wins, region name not published"
                );
            }
            apps.extend(
                apps_region
                    .into_iter()
                    .filter(|r| !claimed.contains(r.name.as_str())),
            );
            let mut apps_managed: Vec<String> = vec!["*".into(), String::new()];
            apps_managed.extend(
                apps_region_names
                    .into_iter()
                    .filter(|n| !claimed.contains(n.as_str())),
            );
            apps_managed.extend(affinity_names);
            let geo_zone_label =
                crate::dnsserver::deploy_zone().and_then(|dz| geo_label(dz, &cloud.apps_domain));
            let (geo_records, geo_names) = geo_zone_label
                .as_deref()
                .map(|label| desired_geo_delegation(&publish, label, &cloud.apps_domain))
                .unwrap_or_default();
            STATS
                .geo_delegation_records
                .store(geo_records.len() as u64, Ordering::Relaxed);
            if geo_zone_label.is_some() && geo_records.is_empty() && nodes.iter().any(|n| n.dns_ns)
            {
                STATS.geo_delegation_holds.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    proven = proven.len(),
                    unproven = ?unproven,
                    "geo delegation HELD: fewer than 2 nameservers are currently proven reachable — keeping the published NS records rather than deleting them (deleting would blackhole the whole zone)"
                );
                cloud.incidents.open(crate::incidents::OpenReq {
                    condition: "dns:geo-hold".into(),
                    ttl_ms: INCIDENT_TTL_MS,
                    title: "Geo-DNS delegation held: fewer than 2 proven nameservers".into(),
                    severity: crate::incidents::Severity::Major,
                    affected: vec!["dns".into()],
                    message: format!(
                        "Nameservers declaring dns_ns but currently unproven from peer vantages: {}. \
                         The existing NS records are being held (not deleted) so the zone keeps resolving. \
                         See GET /v1/dns/stats for the per-node evidence.",
                        if unproven.is_empty() { "-".to_string() } else { unproven.join(", ") }
                    ),
                });
            } else {
                cloud.incidents.clear(
                    "dns:geo-hold",
                    "At least two nameservers are proven from peer vantages again; the geo delegation is no longer held.",
                );
            }
            apps.extend(geo_records);
            apps_managed.extend(geo_names);
            let apps_managed_refs: Vec<&str> = apps_managed.iter().map(|s| s.as_str()).collect();
            STATS
                .affinity_records
                .store(affinity_count as u64, Ordering::Relaxed);
            let dashboard = std::env::var("HIVE_DASHBOARD_UPSTREAM")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            let (api_records, api_hold) = match api_decision {
                ApiDelegation::Delegate(records) => {
                    STATS
                        .api_delegation_records
                        .store(records.len() as u64, Ordering::Relaxed);
                    (records, false)
                }
                ApiDelegation::Hold => (Vec::new(), true),
                ApiDelegation::Flat => {
                    STATS.api_delegation_records.store(0, Ordering::Relaxed);
                    (Vec::new(), false)
                }
            };
            if api_hold {
                STATS.api_delegation_holds.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    proven = proven.len(),
                    declared = api_declared,
                    delegated = ?api_ns_published,
                    "api delegation HELD: fewer than 2 proven api-capable nameservers — the api name is left exactly as published (changing it on a proof dip is how a name goes dark)"
                );
                if api_ns_published == Some(true) {
                    cloud.incidents.open(crate::incidents::OpenReq {
                        condition: "dns:api-hold".into(),
                        ttl_ms: INCIDENT_TTL_MS,
                        title: "API delegation held: fewer than 2 proven api-capable nameservers".into(),
                        severity: crate::incidents::Severity::Major,
                        affected: vec!["dns".into()],
                        message: "The api label's published NS delegation is being HELD (not disengaged) because \
                                  the peer-proven api-capable nameserver set dropped below the floor. The name \
                                  keeps resolving through the child zone. See GET /v1/dns/stats for the per-node \
                                  evidence."
                            .into(),
                    });
                }
            } else {
                cloud.incidents.clear(
                    "dns:api-hold",
                    "At least two api-capable nameservers are proven again; the api delegation is no longer held.",
                );
            }
            let delegate_api = api_hold || !api_records.is_empty();
            let mut platform = desired_platform(
                &publish,
                &relay_ips,
                &discovery_ips,
                dashboard,
                delegate_api,
            );
            platform.extend(api_records);
            let (platform_region, platform_region_names) = desired_region_names(&publish, "api-");
            platform.extend(platform_region);
            STATS
                .region_records
                .store(platform_region_names.len() as u64, Ordering::Relaxed);

            let mut ok = true;
            if let Err(e) = reconcile_zone(
                &api,
                &cloud.apps_domain,
                &apps,
                &apps_managed_refs,
                &cloud,
                &mut guards,
            )
            .await
            {
                STATS.api_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e, zone = %cloud.apps_domain, "DNS reconcile failed");
                ok = false;
            }
            let platform_managed =
                platform_managed_names(dashboard, platform_region_names, api_hold);
            let platform_managed_refs: Vec<&str> =
                platform_managed.iter().map(|s| s.as_str()).collect();
            match reconcile_zone(
                &api,
                &cloud.platform_domain,
                &platform,
                &platform_managed_refs,
                &cloud,
                &mut guards,
            )
            .await
            {
                Ok(listing) => {
                    api_ns_published =
                        Some(listing.iter().any(|r| r.name == "api" && r.rtype == "NS"));
                }
                Err(e) => {
                    STATS.api_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %e, zone = %cloud.platform_domain, "DNS reconcile failed");
                    ok = false;
                }
            }
            if !cloud.db_domain.is_empty() {
                let mut db_desired = desired_apps(&publish);
                let mut managed: Vec<String> = vec!["*".into(), String::new()];
                let all_nodes = cloud.registry.nodes();
                let suffix = format!(".{}", cloud.db_domain);
                let mut dir: Vec<(String, String)> = cloud
                    .databases
                    .list(None)
                    .into_iter()
                    .filter(|d| !d.db_host.is_empty() && !d.host_node.is_empty())
                    .map(|d| (d.db_host, d.host_node))
                    .collect();
                let peer_names: Vec<String> = all_nodes
                    .iter()
                    .filter(|n| n.name != cloud.node_name && n.healthy)
                    .map(|n| n.name.clone())
                    .collect();
                let peer_results = futures::future::join_all(peer_names.iter().map(|name| {
                    let cloud = cloud.clone();
                    let name = name.clone();
                    async move {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(8),
                            crate::admin::fetch_from_host(&cloud, &name, "/v1/db-directory", ""),
                        )
                        .await
                        .ok()
                        .flatten()
                    }
                }))
                .await;
                for v in peer_results.into_iter().flatten() {
                    for e in v.as_array().map(|a| a.as_slice()).unwrap_or_default() {
                        let host = e
                            .get("db_host")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default();
                        let hn = e
                            .get("host_node")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default();
                        if !host.is_empty() && !hn.is_empty() {
                            dir.push((host.to_string(), hn.to_string()));
                        }
                    }
                }
                let mut seen = std::collections::HashSet::new();
                let mut stranded: Vec<(String, String)> = Vec::new();
                let mut addressless: Vec<(String, String)> = Vec::new();
                const DB_STRANDED_CONDITION: &str = "db:stranded-host";
                const DB_STRANDED_TTL_MS: u64 = 30 * 60 * 1000;
                for (db_host, host_node) in dir {
                    let Some(slug) = db_host.strip_suffix(&suffix) else {
                        continue;
                    };
                    if !seen.insert(slug.to_string()) {
                        continue;
                    }
                    managed.push(slug.to_string());
                    let Some(node) = all_nodes.iter().find(|n| n.name == host_node) else {
                        stranded.push((slug.to_string(), host_node.clone()));
                        continue;
                    };
                    if node.public_ip.is_none() && node.public_ip6.is_none() {
                        addressless.push((slug.to_string(), host_node.clone()));
                    }
                    if let Some(ip) = &node.public_ip {
                        db_desired.push(DesiredRecord {
                            name: slug.into(),
                            rtype: "A".into(),
                            value: ip.clone(),
                            ttl: 60,
                        });
                    }
                    if let Some(ip) = &node.public_ip6 {
                        db_desired.push(DesiredRecord {
                            name: slug.into(),
                            rtype: "AAAA".into(),
                            value: ip.clone(),
                            ttl: 60,
                        });
                    }
                }
                for (slug, host) in &stranded {
                    tracing::error!(
                        slug = %slug,
                        host_node = %host,
                        "database stranded: its host_node has left the registry, so no valid A \
                         record exists for it and no node can serve it. The data lives only on \
                         that (vanished) node — OPERATOR DECISION REQUIRED; nothing was \
                         re-hosted and nothing was deleted"
                    );
                }
                for (slug, host) in &addressless {
                    tracing::warn!(
                        slug = %slug,
                        host_node = %host,
                        "database unpublishable: its host is in the registry but advertises no \
                         public address, so no A/AAAA record can be written for it"
                    );
                }
                if stranded.is_empty() && addressless.is_empty() {
                    cloud.incidents.clear(
                        DB_STRANDED_CONDITION,
                        "every managed database's host is present in the registry and advertises a \
                         public address",
                    );
                } else {
                    let mut affected: Vec<String> = stranded
                        .iter()
                        .chain(addressless.iter())
                        .map(|(slug, host)| format!("{slug}@{host}"))
                        .collect();
                    affected.sort();
                    affected.dedup();
                    let message = format!(
                        "{} managed database(s) cannot be given a valid <slug>.{} A record: {} sit \
                         on a host_node that has LEFT the registry (their data lived only on that \
                         node, which no longer exists — presumed unrecoverable without the cloud \
                         account's own disk/snapshot), {} on a host in the registry that advertises \
                         no public address. Recovery needs the host or its disk/volume back: \
                         re-provisioning under the same slug yields an EMPTY database under the same \
                         name. Nothing was deleted, re-hosted or truncated — operator decision.",
                        stranded.len() + addressless.len(),
                        cloud.db_domain,
                        stranded.len(),
                        addressless.len(),
                    );
                    cloud.incidents.open(crate::incidents::OpenReq {
                        title: "Managed database stranded on a host that cannot be published"
                            .to_string(),
                        severity: crate::incidents::Severity::Critical,
                        affected,
                        message,
                        condition: DB_STRANDED_CONDITION.to_string(),
                        ttl_ms: DB_STRANDED_TTL_MS,
                    });
                }
                let managed_refs: Vec<&str> = managed.iter().map(|s| s.as_str()).collect();
                if let Err(e) = reconcile_zone(
                    &api,
                    &cloud.db_domain,
                    &db_desired,
                    &managed_refs,
                    &cloud,
                    &mut guards,
                )
                .await
                {
                    STATS.api_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %e, zone = %cloud.db_domain, "DNS reconcile failed");
                    ok = false;
                }
            }
            consecutive_failures = if ok {
                0
            } else {
                (consecutive_failures + 1).min(6)
            };
            STATS.passes.fetch_add(1, Ordering::Relaxed);
            STATS
                .last_pass_ms
                .store(hive_core::now_ms(), Ordering::Relaxed);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rv(id: &str, name: &str, t: &str, v: &str) -> RecordView {
        RecordView {
            id: id.into(),
            name: name.into(),
            rtype: t.into(),
            value: v.into(),
            created_ms: None,
        }
    }
    fn dr(name: &str, t: &str, v: &str) -> DesiredRecord {
        DesiredRecord {
            name: name.into(),
            rtype: t.into(),
            value: v.into(),
            ttl: 60,
        }
    }

    #[test]
    fn diff_creates_missing_and_deletes_stale() {
        let current = vec![
            rv("1", "api", "A", "1.1.1.1"),
            rv("2", "api", "A", "2.2.2.2"),
        ];
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
        assert!(
            c.is_empty() && d.is_empty(),
            "converged sets must write nothing"
        );
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
        assert_eq!(
            d,
            vec!["1".to_string()],
            "only the managed A record is replaced"
        );
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
    fn damping_withdraws_after_k_and_damps_readd() {
        let mut damping = PublishDamping::default();
        let view = |healthy| {
            vec![NodeView {
                dashboard: false,
                dns_ns: false,
                dns_api: false,
                dns_validated: false,
                name: "n1".into(),
                healthy,
                ip4: Some("1.1.1.1".into()),
                ip6: None,
                region: "san-jose".into(),
            }]
        };
        let (up, down) = (view(true), view(false));
        // First-sight healthy publishes immediately — new nodes never wait.
        assert_eq!(publishable(&up, &mut damping).len(), 1);
        // 1st unhealthy pass: still published (withdraw damping)
        assert_eq!(publishable(&down, &mut damping).len(), 1);
        // 2nd unhealthy pass: withdrawn
        assert_eq!(publishable(&down, &mut damping).len(), 0);
        // Re-add is damped: one healthy pass is not enough...
        assert_eq!(publishable(&up, &mut damping).len(), 0);
        // ...two consecutive healthy passes republish.
        assert_eq!(publishable(&up, &mut damping).len(), 1);
        // And once back, the withdraw window re-arms.
        assert_eq!(publishable(&down, &mut damping).len(), 1);
    }

    #[test]
    fn damping_flapping_node_stays_withheld() {
        // The treadmill this two-sided damping exists to kill: a node flapping
        // healthy/unhealthy must NOT re-enter on every healthy blip — once
        // withheld, only sustained health republishes it.
        let mut damping = PublishDamping::default();
        let view = |healthy| {
            vec![NodeView {
                dashboard: false,
                dns_ns: false,
                dns_api: false,
                dns_validated: false,
                name: "flap".into(),
                healthy,
                ip4: Some("1.1.1.1".into()),
                ip6: None,
                region: "san-jose".into(),
            }]
        };
        let (up, down) = (view(true), view(false));
        assert_eq!(publishable(&up, &mut damping).len(), 1);
        // Drive it out: two unhealthy passes.
        assert_eq!(publishable(&down, &mut damping).len(), 1);
        assert_eq!(publishable(&down, &mut damping).len(), 0);
        // Flap: healthy, unhealthy, healthy — a single damped unhealthy pass
        // must not fling the door back open either.
        assert_eq!(publishable(&up, &mut damping).len(), 0);
        assert_eq!(publishable(&down, &mut damping).len(), 0);
        assert_eq!(publishable(&up, &mut damping).len(), 0);
        // Only sustained health returns it.
        assert_eq!(publishable(&up, &mut damping).len(), 1);
    }

    #[test]
    fn plan_writes_packs_cutover_transaction() {
        // Delegation cutover: api has flat A records at the parent, desired
        // wants NS instead — the pair must come out of the ordinary flow as
        // ONE restore-on-failure transaction.
        let current = vec![
            rv("a1", "api", "A", "9.9.9.9"),
            rv("a2", "api", "A", "8.8.8.8"),
        ];
        let desired = vec![
            dr("api", "NS", "ns-n1.shadw.app"),
            dr("api", "NS", "ns-n2.shadw.app"),
        ];
        let (creates, deletes) = diff(&current, &desired, &["api"]);
        assert_eq!(creates.len(), 2);
        assert_eq!(deletes.len(), 2);
        let plan = plan_writes(&current, &desired, &["api"], creates, deletes);
        assert_eq!(plan.cutovers.len(), 1);
        let (name, addr, ns) = &plan.cutovers[0];
        assert_eq!(name, "api");
        assert_eq!(addr.len(), 2, "both flat addresses ride the transaction");
        assert_eq!(ns.len(), 2, "both missing NS ride the transaction");
        assert!(
            plan.creates.is_empty() && plan.deletes.is_empty() && plan.ns_deletes_first.is_empty()
        );
    }

    #[test]
    fn plan_writes_ns_deletes_precede_address_creates() {
        // Disengagement: api is delegated (NS present), desired wants the flat
        // A set back — the NS delete must run FIRST or the A create 409s.
        let current = vec![
            rv("n1", "api", "NS", "ns-n1.shadw.app."),
            rv("n2", "api", "NS", "ns-n2.shadw.app."),
        ];
        let desired = vec![dr("api", "A", "1.1.1.1")];
        let (creates, deletes) = diff(&current, &desired, &["api"]);
        let plan = plan_writes(&current, &desired, &["api"], creates, deletes);
        assert_eq!(plan.ns_deletes_first.len(), 2);
        assert_eq!(plan.creates.len(), 1);
        assert!(plan.cutovers.is_empty() && plan.deletes.is_empty());
    }

    #[test]
    fn plan_writes_complete_delegation_flows_normally() {
        // Already fully delegated (both NS present) with a leftover address
        // record to clean: no transaction needed, ordinary flow.
        let current = vec![
            rv("n1", "api", "NS", "ns-n1.shadw.app."),
            rv("n2", "api", "NS", "ns-n2.shadw.app."),
            rv("a1", "api", "A", "9.9.9.9"),
        ];
        let desired = vec![
            dr("api", "NS", "ns-n1.shadw.app"),
            dr("api", "NS", "ns-n2.shadw.app"),
        ];
        let (creates, deletes) = diff(&current, &desired, &["api"]);
        let plan = plan_writes(&current, &desired, &["api"], creates, deletes);
        assert!(
            plan.cutovers.is_empty(),
            "complete NS set = no cutover transaction"
        );
        assert!(plan.creates.is_empty());
        assert_eq!(
            plan.deletes.len(),
            1,
            "the stale address delete flows normally"
        );
    }

    #[test]
    fn plan_writes_rotation_does_not_hoist_ns_deletes() {
        // NS-target ROTATION on a name that stays delegated: the replacement
        // must be created BEFORE the old target is removed, or the set dips
        // below its target count (adversarial-review confirmed the hoist
        // opened a below-floor window). Hoisting is only for names with NO
        // desired NS (true disengagement).
        let current = vec![
            rv("n1", "api", "NS", "ns-old.shadw.app."),
            rv("n2", "api", "NS", "ns-old2.shadw.app."),
        ];
        let desired = vec![
            dr("api", "NS", "ns-new.shadw.app"),
            dr("api", "NS", "ns-old2.shadw.app"),
        ];
        let (creates, deletes) = diff(&current, &desired, &["api"]);
        let plan = plan_writes(&current, &desired, &["api"], creates, deletes);
        assert!(
            plan.ns_deletes_first.is_empty(),
            "rotation keeps creates-then-deletes order"
        );
        assert!(
            plan.cutovers.is_empty(),
            "no address blockers -> no transaction"
        );
        assert_eq!(
            plan.creates.len(),
            1,
            "the replacement NS flows as a normal create"
        );
        assert_eq!(
            plan.deletes.len(),
            1,
            "the retired NS flows as a normal delete"
        );
    }

    #[test]
    fn plan_writes_packs_squatters_into_cutover() {
        // A squatter that is neither an address nor a desired NS (CNAME,
        // ALIAS, stray TXT, foreign NS target) 409s the NS create exactly
        // like an address does — it must ride the transaction's
        // delete+restore set or the cutover rollback-loops forever.
        let current = vec![
            rv("a1", "api", "A", "9.9.9.9"),
            rv("s1", "api", "CNAME", "squat.example.com."),
            rv("s2", "api", "TXT", "verify-me"),
            rv("s3", "api", "NS", "ns-foreign.shadw.app."),
        ];
        let desired = vec![
            dr("api", "NS", "ns-n1.shadw.app"),
            dr("api", "NS", "ns-n2.shadw.app"),
        ];
        let (creates, deletes) = diff(&current, &desired, &["api"]);
        let plan = plan_writes(&current, &desired, &["api"], creates, deletes);
        assert_eq!(plan.cutovers.len(), 1);
        let (name, blockers, ns) = &plan.cutovers[0];
        assert_eq!(name, "api");
        assert_eq!(
            blockers.len(),
            3,
            "address + CNAME + stray TXT ride the transaction"
        );
        assert_eq!(ns.len(), 2);
        assert!(plan.creates.is_empty());
        assert_eq!(
            plan.deletes.len(),
            1,
            "the foreign NS target is no blocker — it leaves as a normal delete"
        );
    }

    #[test]
    fn orphan_candidate_conservative_rules() {
        let now = 1_000_000u64;
        let old = Some(now - ACME_ORPHAN_MIN_AGE_MS - 1);
        let young = Some(now - ACME_ORPHAN_MIN_AGE_MS + 1);
        assert!(
            !is_orphan_candidate(true, old, now),
            "in-flight is never swept"
        );
        assert!(!is_orphan_candidate(true, None, now));
        assert!(
            is_orphan_candidate(false, old, now),
            "store-unknown + provably old = orphan"
        );
        assert!(
            !is_orphan_candidate(false, young, now),
            "store-unknown but brand-new: keep"
        );
        assert!(
            !is_orphan_candidate(false, None, now),
            "age unknowable (schema-nullable): keep — deletes are forever"
        );
    }

    #[test]
    fn no_public_ip_never_published() {
        let mut damping = PublishDamping::default();
        let nodes = vec![NodeView {
            dns_ns: false,
            dns_api: false,
            dns_validated: false,
            dashboard: false,
            name: "nat-node".into(),
            healthy: true,
            ip4: None,
            ip6: None,
            region: "bangkok".into(),
        }];
        assert!(publishable(&nodes, &mut damping).is_empty());
    }

    #[test]
    fn desired_sets_cover_wildcard_apex_api_relay() {
        let nodes = vec![PublishNode {
            dns_ns: false,
            dns_api: false,
            dns_validated: false,
            dashboard: false,
            name: "n1".into(),
            ip4: Some("1.1.1.1".into()),
            ip6: Some("::1".into()),
            region: "san-jose".into(),
        }];
        let apps = desired_apps(&nodes);
        assert!(apps.contains(&DesiredRecord {
            name: "*".into(),
            rtype: "A".into(),
            value: "1.1.1.1".into(),
            ttl: 60
        }));
        assert!(apps.contains(&DesiredRecord {
            name: "".into(),
            rtype: "AAAA".into(),
            value: "::1".into(),
            ttl: 60
        }));
        let plat = desired_platform(
            &nodes,
            &["2.2.2.2".into()],
            &["3.3.3.3".into()],
            true,
            false,
        );
        assert!(
            plat.contains(&DesiredRecord {
                name: "".into(),
                rtype: "A".into(),
                value: "1.1.1.1".into(),
                ttl: 60
            }),
            "apex published when dashboard hosting on"
        );
        assert!(plat.contains(&DesiredRecord {
            name: "www".into(),
            rtype: "A".into(),
            value: "1.1.1.1".into(),
            ttl: 60
        }));
        assert!(plat.contains(&DesiredRecord {
            name: "api".into(),
            rtype: "A".into(),
            value: "1.1.1.1".into(),
            ttl: 60
        }));
        assert!(
            plat.contains(&DesiredRecord {
                name: "admin".into(),
                rtype: "A".into(),
                value: "1.1.1.1".into(),
                ttl: 60
            }),
            "ops console host published"
        );
        assert!(plat.contains(&DesiredRecord {
            name: "relay".into(),
            rtype: "A".into(),
            value: "2.2.2.2".into(),
            ttl: 300
        }));
        assert!(plat.contains(&DesiredRecord {
            name: "discovery".into(),
            rtype: "A".into(),
            value: "3.3.3.3".into(),
            ttl: 300
        }));
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
            Self {
                records: Mutex::new(records),
                fail_lists: Mutex::new(0),
                next_id: Mutex::new(100),
                creates: Mutex::new(0),
                deletes: Mutex::new(0),
            }
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
            self.records.lock().unwrap().push(RecordView {
                id: rid.clone(),
                name: rec.name.clone(),
                rtype: rec.rtype.clone(),
                value: rec.value.clone(),
                created_ms: None,
            });
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
        for r in &c {
            api.create("z", r).await.unwrap();
        }
        for id in &d {
            api.delete("z", id).await.unwrap();
        }
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
        assert!(
            api.list("z").await.is_err(),
            "429 must surface as an error (caller backs off)"
        );
        // Records untouched by the failed pass.
        assert_eq!(api.records.lock().unwrap().len(), 1);
    }
}
