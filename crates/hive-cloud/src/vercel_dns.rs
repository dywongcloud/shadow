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
//!  * **Never dark**: a managed name must never end a pass serving NEITHER
//!    addresses NOR delegation. Delegation transitions are restore-on-failure
//!    transactions whose restores are VERIFIED, an account-wide create block
//!    (witnessed 2026-08-04: Vercel fair-use 402s every create while deletes
//!    keep working) skips every delete-before-create step outright, and a
//!    violation opens a Major incident.
//!  * Exponential backoff on 429/5xx from the Vercel API.
//!
//! Regional/latency steering stays inside `edge.rs` (`order_candidates`) after
//! the client reaches any node — DNS only hands out healthy IPs. The self-hosted
//! Seer (`dnsserver.rs`) is NOT retired: it keeps serving internal/test queries
//! and is the future NS-delegation path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::state::CloudState;

/// How many consecutive unhealthy reconcile passes before a node's records are
/// withdrawn (flap damping).
pub const UNHEALTHY_PASSES_BEFORE_WITHDRAW: u32 = 2;

/// How many consecutive HEALTHY passes a withdrawn node needs before its
/// records are republished (re-add flap damping). Paired with the withdraw
/// threshold this collapses the create/delete treadmill a flapping node
/// otherwise drives across every managed name — live-witnessed 2026-07-29: a
/// post-roll reconvergence flapped five nodes in and out of the healthy set
/// and the reconciler burned 6–15 Vercel writes per ~30s pass, drew sustained
/// 429s, and briefly DELETED real address records mid-treadmill.
pub const HEALTHY_PASSES_BEFORE_REPUBLISH: u32 = 2;

/// Minimum age before an `_acme-challenge.*` TXT unknown to the in-flight
/// challenge store may be swept as an orphan. ACME orders complete in minutes,
/// so 15 minutes is far outside any legitimate order while still protecting a
/// record whose placement just raced the pass's own zone listing.
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
    /// Vercel's creation timestamp (ms epoch) when the API reports it. The ACME
    /// orphan sweeper uses it as a minimum-age gate so a just-placed challenge
    /// is never swept from under an in-flight order.
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
        // The zone exceeds one page (100): a truncated listing makes every
        // record past page 1 look missing, so the diff re-creates it forever —
        // a sustained 429 storm that starves real creates. Follow
        // `pagination.next` (a timestamp cursor passed back as `until`) to
        // exhaustion; a partial read is worse than a failed one.
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

// ---- desired state -----------------------------------------------------------

/// A publishable node: healthy (or inside the damping window) with a public IP.
#[derive(Clone, Debug)]
pub struct PublishNode {
    pub name: String,
    pub ip4: Option<String>,
    pub ip6: Option<String>,
    /// This node CLAIMS to answer authoritative DNS on a public `:53`
    /// (gossiped `NodeInfo::dns_ns`) — a necessary condition for the geo
    /// zone's NS set, never a sufficient one. See `desired_geo_delegation`.
    pub dns_ns: bool,
    /// This node's Seer also serves the platform API zone (gossiped
    /// `NodeInfo::dns_api`) — required, on top of `dns_ns`, to appear in the
    /// `api` label's NS set. See `desired_api_delegation`.
    pub dns_api: bool,
    /// Peers have PROVEN, from their own hosts across the public internet,
    /// that this node currently answers DNS usefully
    /// (`dns_probe::validate_nameservers`). Only this gates the NS set.
    pub dns_validated: bool,
    /// The node's region code (`san-jose`, `bangkok`, …) — carried through from
    /// the registry so the per-region names can be derived from the same
    /// health-damped set every other record already comes from.
    pub region: String,
    /// This node's local dashboard upstream answers within budget (gossiped
    /// `NodeInfo::dashboard`, a live measurement) — gates the apex/`www`
    /// A-set so slow-SSR nodes don't degrade first visits. See
    /// `desired_platform`.
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
    dz.strip_suffix(&format!(".{apps}"))
        .filter(|l| !l.is_empty() && !l.contains('.'))
        .map(str::to_string)
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
/// Eligibility is **proof, not configuration**. `PublishNode::dns_ns` only says
/// the node's own env asked it to bind a public `:53`; `dns_validated` says
/// peers in at least two regions have just queried that address over the public
/// internet and got a usable authoritative answer back
/// (`dns_probe::validate_nameservers`). Publishing on the former alone is what
/// put two dead nameservers into the live `deploy.shadw.app` delegation — one
/// firewalled upstream of its host, one answering with zero records — either of
/// which makes the whole zone resolve intermittently for anyone whose resolver
/// picks it.
///
/// **Below two proven nameservers this publishes NOTHING AND MANAGES NOTHING**,
/// which is a deliberate HOLD, not a withdrawal. Returning an empty managed-name
/// list means the reconciler's diff never considers the delegation records its
/// own, so an already-published delegation is left exactly as it is. The
/// alternative — treating "0 or 1 proven" as the desired state — would delete
/// every NS record for the zone and blackhole every name under it, turning a
/// degraded delegation into a total outage. Stale-but-partly-working beats gone,
/// the same last-known-good rule `reconcile_zone` already applies to addresses;
/// the caller logs and raises an incident so the hold is loud rather than quiet.
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
            // FULLY QUALIFIED, always: Vercel rejects a relative NS target
            // outright — `{"code":"invalid_value","message":"The NS value is
            // not a fully qualified domain name."}` — which is why the glue A
            // records published while every NS create silently failed, leaving
            // the zone undelegated with correct-looking glue in place.
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
    // A delegation with a single nameserver is a single point of failure for
    // every name in the zone. Below two PROVEN nameservers: publish nothing and
    // manage nothing — an unpublished zone keeps resolving through the parent's
    // own records, and an ALREADY-published one is held untouched rather than
    // deleted (see this function's doc for why deleting is the worse failure).
    if out.iter().filter(|r| r.rtype == "NS").count() < 2 {
        return (Vec::new(), Vec::new());
    }
    (out, managed)
}

/// Two-sided damping for the api-label delegation decision — the same
/// constants `publishable` applies to node health, applied here to the one
/// decision whose wrong flip can strand the api name dark. Engagement waits
/// for `HEALTHY_PASSES_BEFORE_REPUBLISH` consecutive at-floor passes (a
/// one-pass attestation blip never starts a cutover), and disengagement waits
/// for `UNHEALTHY_PASSES_BEFORE_WITHDRAW` consecutive zero-declaration passes
/// (a one-pass registry-view blip never plans the NS deletes).
#[derive(Default)]
pub struct DelegationDamping {
    /// Consecutive passes with >=2 PROVEN api-capable nameservers.
    ready: u32,
    /// Consecutive below-floor passes with ZERO api-capable declarations.
    undeclared: u32,
}

/// The api-label delegation decision for one pass. Mirrors the geo path's
/// below-floor HOLD: while the proven-capable set is short, whatever is
/// published stays published — stale-but-answering beats dark, always.
#[derive(Debug, PartialEq)]
pub enum ApiDelegation {
    /// Publish this NS set on `api` (>=2 proven api-capable nameservers,
    /// stable for `HEALTHY_PASSES_BEFORE_REPUBLISH` consecutive passes).
    Delegate(Vec<DesiredRecord>),
    /// Change NOTHING on the `api` name this pass: a delegation is (or may
    /// be) published but the capable set is below the floor, so the name is
    /// left exactly as it is — the reconciler neither manages it nor desires
    /// its flat set (child address records would veto a later NS
    /// re-creation). Loud: the caller logs and incidents the hold.
    Hold,
    /// No delegation needs protecting: manage `api` and desire the flat A
    /// set — the pre-cutover behaviour, and the shape that also drives a TRUE
    /// disengagement's NS deletes through the phase-0 transaction.
    Flat,
}

/// NS-only delegation for the `api` label in the PLATFORM zone, handing the
/// API host to the fleet's own geo/health-aware nameservers instead of the
/// flat round-robin A set `desired_platform` publishes.
///
/// Differences from `desired_geo_delegation`, both deliberate:
/// * **No glue records.** The NS targets (`ns-<node>.{apps}`) live in the APPS
///   zone, where the deploy-zone delegation already publishes and maintains
///   their A/AAAA; emitting `ns-<node>` names into the PLATFORM zone would
///   create orphan records no resolver ever consults.
/// * **Eligibility additionally requires the gossiped `dns_api` capability** —
///   an older binary answers the deploy zone but would NXDOMAIN
///   `api.{platform}`, so it must never be named here. Seer's own apex NS/SOA
///   for the api zone applies the same capability filter
///   (`apex_ns_names(_, true)`); the parent set is deliberately STRICTER (it
///   also demands peer attestation, below) — same shape as the geo path,
///   where the delegation the parent publishes is the reachability gate.
///
/// Eligibility is PROOF, not configuration, exactly like the geo path: a node
/// enters the NS set only when peers have proven it answers DNS
/// (`PublishNode::dns_validated`), never on its own `dns_ns` claim. And the
/// below-floor behaviour is the geo path's HOLD, not a disengagement —
/// live-witnessed 2026-08-04: a publishable-set dip below the floor was
/// treated as a true disengagement, so the pass DELETED six published NS
/// records while Vercel was 402-ing every create, leaving `api.shadw.cloud`
/// serving neither addresses nor delegation.
///
/// `declared` is computed from the UNDAMPED registry view (any node with
/// `dns_ns && dns_api`) so an unhealthy-but-present fleet still counts as
/// declaring. `delegated_now` is whether the platform zone published NS on
/// `api` at the last successful reconcile (`None` = not yet observed — the
/// safe-direction answer, Hold: a freshly-elected leader must never plan NS
/// deletes on its first pass).
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
    // Below the floor there is nothing safe to CHANGE unless nothing is
    // delegated: a published (or unobserved) delegation is held untouched,
    // otherwise the flat A set keeps serving — the safe, self-healing
    // fallback both before the fleet rolls this capability and if the
    // capable set ever shrinks for real.
    let below_floor = |delegated_now: Option<bool>| {
        if delegated_now != Some(false) {
            ApiDelegation::Hold
        } else {
            ApiDelegation::Flat
        }
    };
    if out.len() >= 2 {
        damping.undeclared = 0;
        damping.ready = damping.ready.saturating_add(1);
        if damping.ready >= HEALTHY_PASSES_BEFORE_REPUBLISH {
            return ApiDelegation::Delegate(out);
        }
        // Capable but not yet STABLE: a cutover started on a one-pass
        // attestation blip is the most dangerous write this reconciler owns.
        return below_floor(delegated_now);
    }
    damping.ready = 0;
    if declared {
        // The fleet still DECLARES api-serving nameservers but cannot
        // currently PROVE two: never plan a disengagement on a proof dip.
        damping.undeclared = 0;
        return below_floor(delegated_now);
    }
    // Nobody even DECLARES the capability (fleet rolled back / capability
    // removed): a true disengagement — damped, so a one-pass registry blip
    // never plans the NS deletes.
    damping.undeclared = damping.undeclared.saturating_add(1);
    if damping.undeclared >= UNHEALTHY_PASSES_BEFORE_WITHDRAW {
        return ApiDelegation::Flat;
    }
    below_floor(delegated_now)
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
        let label = label
            .split('.')
            .next()
            .unwrap_or(label)
            .trim()
            .to_ascii_lowercase();
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

/// Desired records for the PLATFORM zone: `api.` (TTL 60, all publishable
/// nodes) + `relay.`/`discovery.` (TTL 300, from env IP lists — those services
/// run on operator-chosen nodes, not every gateway).
pub fn desired_platform(
    nodes: &[PublishNode],
    relay_ips: &[String],
    discovery_ips: &[String],
    dashboard: bool,    // publish apex + www too (nodes reverse-proxy the dashboard)
    delegate_api: bool, // withhold api's flat A set: NS-delegated to Seer, or HELD below the floor (see desired_api_delegation)
) -> Vec<DesiredRecord> {
    let mut out = Vec::new();
    // `api` = developer/API-key surface, `admin` = ops/admin console surface,
    // `webhook` = incoming GitOps/OpenEdge build-notification receiver
    // (OPENEDGE_WEBHOOK_URL) — all three resolve to the gateway nodes (same
    // host-switch dispatch), published together.
    // `sms` = the self-hosted SMS-fallback service (a platform-deployed app the
    // edge routes by Host alias) — same gateway-node A/AAAA set as the rest.
    // When the api label is NS-delegated (see `desired_api_delegation`), its
    // flat A/AAAA set is withheld: address records at a zone cut are occluded
    // by the delegation anyway, and publishing both makes the diff fight
    // itself every pass.
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
        // Apex + `www` land a browser's FIRST paint, so they are gated on the
        // gossiped live-measured dashboard capability: a healthy node whose
        // local dashboard SSR runs seconds slow (witnessed: sao-paulo at 4–6s)
        // degrades ~1/N of all first visits merely by being in this set. Floor
        // discipline mirrors the NS set: below two capable nodes the gate
        // falls back to every publishable node — a part-rolled or
        // probe-degraded fleet serves exactly as before rather than collapsing
        // the apex onto one node (or zero).
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
                && desired
                    .iter()
                    .any(|d| d.name == r.name && (d.rtype == "A" || d.rtype == "AAAA"))
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
    /// NS records published for the `api` label delegation this pass
    /// (0 = not delegated — the flat A set is published instead). Read by
    /// acme.rs's delegated-zone best-effort gate, which must key on the LIVE
    /// delegation state, never static zone config.
    pub api_delegation_records: AtomicU64,
    /// Nodes whose `dns_ns` claim is currently backed by peer proof.
    pub geo_ns_validated: AtomicU64,
    /// Nodes CLAIMING `dns_ns` that no peer set currently proves — the count an
    /// operator watches after a rollout or a security-group change.
    pub geo_ns_unproven: AtomicU64,
    /// Passes that HELD an existing delegation because fewer than two
    /// nameservers were proven (see `desired_geo_delegation`). A number that
    /// keeps climbing means the zone is running on last-known-good NS records.
    pub geo_delegation_holds: AtomicU64,
    /// Passes that HELD the `api` label exactly as published because the
    /// proven api-capable set dropped below the floor (see
    /// `desired_api_delegation`). The api counterpart of `geo_delegation_holds`.
    pub api_delegation_holds: AtomicU64,
    /// Disengagement NS-deletes / delegation cutovers SKIPPED while the
    /// create-health circuit was open (Vercel refusing creates account-wide
    /// while still allowing deletes). A climbing counter means the reconciler
    /// is deliberately leaving delegations in place rather than stranding
    /// names dark.
    pub create_circuit_skips: AtomicU64,
    /// Delegation cutovers completed by the never-dark transaction (flat
    /// addresses removed AND the full NS set confirmed created, same pass).
    pub delegation_cutovers: AtomicU64,
    /// Cutover attempts rolled back: an NS create failed after the flat
    /// addresses were removed, so the addresses were immediately restored.
    /// A climbing counter means a delegation is RETRYING, not stranding.
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
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
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
    /// Gossiped `NodeInfo::dns_api` — see `PublishNode::dns_api`.
    pub dns_api: bool,
    /// Currently PROVEN to serve DNS from off its own host, per
    /// `dns_probe::validate_nameservers` over the gossiped peer attestations.
    pub dns_validated: bool,
    /// Gossiped `NodeInfo::dashboard` — see `PublishNode::dashboard`.
    pub dashboard: bool,
}

/// Two-sided flap-damping state for `publishable`. One struct so the withdraw
/// streaks, republish streaks, and withdrawn set can't drift apart across
/// passes.
#[derive(Default)]
pub struct PublishDamping {
    /// Consecutive-unhealthy streak per node (drives withdrawal).
    unhealthy: HashMap<String, u32>,
    /// Consecutive-healthy streak per WITHHELD node (drives re-admission).
    healthy: HashMap<String, u32>,
    /// Nodes currently withdrawn from the published set. Once here, the ONLY
    /// way back is `HEALTHY_PASSES_BEFORE_REPUBLISH` consecutive healthy
    /// passes — a single damped unhealthy pass must not fling the door back
    /// open, or a flapping node keeps churning writes at half rate.
    withheld: std::collections::HashSet<String>,
}

/// The publishable node set with two-sided flap damping: a node stays published
/// while its consecutive-unhealthy streak is below the withdraw threshold, and
/// — once withdrawn — re-enters only after `HEALTHY_PASSES_BEFORE_REPUBLISH`
/// consecutive healthy passes. Withdrawals stay fast (a dead node must drain),
/// re-adds are what get damped (a returning node must prove it's stable), so a
/// flapping node can no longer drive a create/delete write treadmill against
/// the DNS API. A node seen for the FIRST time healthy publishes immediately —
/// new nodes must not wait. Pure; unit-tested.
pub fn publishable(nodes: &[NodeView], damping: &mut PublishDamping) -> Vec<PublishNode> {
    let mut out = Vec::new();
    for n in nodes {
        if n.ip4.is_none() && n.ip6.is_none() {
            continue; // never publishable without a public IP
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
                    continue; // still earning its way back into the set
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
    // Drop damping state for nodes that vanished from the registry entirely.
    let known: std::collections::HashSet<&String> = nodes.iter().map(|n| &n.name).collect();
    damping.unhealthy.retain(|k, _| known.contains(k));
    damping.healthy.retain(|k, _| known.contains(k));
    damping.withheld.retain(|k| known.contains(k));
    out
}

/// The write plan for one pass, ordered so neither a delegation cutover nor a
/// disengagement can strand a name with NEITHER addresses NOR delegation at
/// the parent. Vercel forbids NS records coexisting with any other record on
/// one name — and forbids CREATING one while anything else sits on the name
/// (live-witnessed 2026-07-29: every NS create for `api` 409'd against the
/// flat A set, and the hold-exempted A-delete then stranded `api.shadw.cloud`
/// dark for ~90 minutes) — so both transitions are sequenced explicitly:
/// cutovers run as restore-on-failure transactions, and disengagement deletes
/// NS BEFORE the flat addresses are re-created (with a symmetric NS restore
/// when those address creates fail). Both delete-first sequences are SKIPPED
/// outright while the create-health circuit is open (see `ReconcileGuards`) —
/// under an account-wide create block a restore is itself a create, so the
/// transaction could not complete. Pure; unit-tested.
struct WritePlan {
    /// NS deletes that must precede any address create (a TRUE disengagement
    /// only: the name has NO desired NS, so the delegation frees it for the
    /// flat set). A rotation on a name that STAYS delegated is deliberately
    /// NOT hoisted — the replacement NS is created before the old one is
    /// removed, so the set never dips below its target count.
    ns_deletes_first: Vec<String>,
    /// Per delegated name: every record currently blocking its NS creation
    /// (the address set PLUS any squatter — foreign NS target, CNAME, ALIAS,
    /// stray TXT; any of them 409s the NS create) + the NS records to create,
    /// executed as ONE transaction with full restore on failure.
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
    // Names this pass wants NS-delegated (desired carries NS for them).
    let delegated: Vec<&str> = managed_names
        .iter()
        .copied()
        .filter(|n| desired.iter().any(|d| d.name == *n && d.rtype == "NS"))
        .collect();
    for name in delegated {
        // Desired NS not yet present at the parent.
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
            continue; // already fully delegated — normal flow
        }
        // EVERYTHING currently on the name that an NS create would 409
        // against: the address set AND any squatter (CNAME, ALIAS, stray
        // TXT). Leaving a squatter out of the transaction deadlocks the
        // cutover into a rollback loop every pass while the squatter blocks
        // NS creation forever (adversarial-review confirmed). NS records are
        // NEVER blockers — an RRset grows by adding members (the live 8-NS
        // deploy delegation was built exactly that way), so a foreign target
        // rides the normal creates-then-deletes flow instead.
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
            continue; // nothing blocks — the NS creates flow normally
        }
        let blocker_ids: std::collections::HashSet<&str> =
            blockers.iter().map(|r| r.id.as_str()).collect();
        deletes.retain(|id| !blocker_ids.contains(id.as_str()));
        creates.retain(|c| {
            !(c.name == name && c.rtype == "NS" && missing_ns.iter().any(|m| m.value == c.value))
        });
        cutovers.push((name.to_string(), blockers, missing_ns));
    }
    // NS deletes are hoisted ONLY for names with NO desired NS (a true
    // disengagement freeing the name for its flat set). Rotation on a name
    // that stays delegated keeps normal creates-then-deletes order.
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

/// Is this `_acme-challenge.*` TXT an orphan candidate? The replicated
/// challenge store is the placement source of truth; the 15-minute age gate is
/// benefit-of-the-doubt for placement racing the pass's own zone listing — and
/// a record whose age the API does not report (Vercel's schema marks
/// `created`/`createdAt` NULLABLE) gets the SAME doubt: a delete is forever,
/// so doubt always means KEEP (adversarial-review confirmed the fail-open
/// alternative could sweep a live in-flight challenge after a leadership
/// flap). Pure; unit-tested.
fn is_orphan_candidate(in_flight: bool, created_ms: Option<u64>, now_ms: u64) -> bool {
    if in_flight {
        return false;
    }
    match created_ms {
        Some(c) => now_ms.saturating_sub(c) >= ACME_ORPHAN_MIN_AGE_MS,
        None => false,
    }
}

/// Cross-pass guard state threaded through `reconcile_zone`: the create-health
/// circuit and the never-dark alarm's edge trigger. One struct so both travel
/// together across the zone reconciles of a pass.
#[derive(Default)]
struct ReconcileGuards {
    /// Consecutive zone-passes whose creates ALL failed (>=1 attempted, 0
    /// succeeded). While non-zero the circuit is OPEN: Vercel is refusing
    /// creates account-wide while still allowing deletes — live-witnessed
    /// 2026-08-04 as a fair-use 402 block. Under that asymmetry any
    /// delete-before-create sequence (the disengagement's NS-deletes-first,
    /// the cutover's address-deletes-first) is unrecoverable BY CONSTRUCTION:
    /// the restore is itself a create. An open circuit therefore skips those
    /// deletes outright — a stale-but-answering delegation beats a dark name,
    /// always.
    create_failing_passes: u32,
    /// Edge trigger for the account-wide create-failure incident.
    create_incident_open: bool,
    /// Managed names already alarmed as dark (`"{domain}\0{name}"`) — the edge
    /// trigger so a sustained outage opens ONE incident per name, not one per
    /// pass. Cleared once the name has records again, so a LATER outage of
    /// the same name alarms afresh.
    dark_alarmed: std::collections::HashSet<String>,
    /// Whether a create has SUCCEEDED during this node's current leadership
    /// tenure. `false` is the UNKNOWN state, and unknown is treated exactly
    /// like an open circuit — for the same reason `api_ns_published: None`
    /// means Hold: a leader that has not itself proven the account accepts
    /// creates must never start a sequence whose rollback is a create.
    ///
    /// This is why the circuit needs no cross-node inheritance. A departing
    /// leader's counter is HEARSAY about an account this node has not
    /// exercised (and it is stale by the whole handover gap); the same
    /// proof-beats-claim rule `dns_validated` applies to nameservers applies
    /// here — the new leader re-proves within its first pass that lands any
    /// create, and holds every delete-before-create sequence until it does.
    create_proven: bool,
    /// Delete-first steps skipped SOLELY because create health is unproven
    /// this tenure (the circuit itself is closed). Counted so an indefinite
    /// hold — a quiet zone whose only pending change IS the delegation
    /// transition, so no create is ever attempted to prove health — becomes
    /// an operator-visible incident instead of a silently parked cutover.
    proof_holds: u32,
    /// Edge trigger for that incident.
    proof_incident_open: bool,
}

/// Delete-first steps skipped for want of create-health proof before the hold
/// itself is alarmed. At most one skip per zone per pass, so this is roughly
/// five minutes at the default cadence: long enough that an ordinary handover
/// (whose next create lands within a pass or two and proves the account) never
/// alarms, short enough that a delegation transition parked because NOTHING
/// else in the zone needs creating reaches an operator instead of sitting
/// silently. Nothing is dark while it holds — the existing records serve.
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

    /// This node just (re)took DNS leadership. Only the PROOF state resets:
    /// the failure counters and incident edge-triggers are this node's own
    /// observations and stay, because every one of them fails safe.
    fn begin_tenure(&mut self) {
        self.create_proven = false;
        self.proof_holds = 0;
        self.proof_incident_open = false;
    }

    /// Fold one zone-pass's create outcomes into the circuit. A pass with no
    /// creates says nothing; any success closes the circuit immediately.
    fn record_pass(&mut self, attempted: usize, succeeded: usize) {
        if succeeded > 0 {
            self.create_failing_passes = 0;
            self.create_incident_open = false;
            self.create_proven = true;
            self.proof_holds = 0;
            self.proof_incident_open = false;
        } else if attempted > 0 {
            self.create_failing_passes = self.create_failing_passes.saturating_add(1);
        }
    }

    /// One delete-first step was skipped. Only a skip attributable to MISSING
    /// PROOF counts here — a skip under a genuinely open circuit is already
    /// alarmed by the account-wide create-failure incident.
    fn note_skip(&mut self, cloud: &Arc<CloudState>, domain: &str) {
        if self.create_proven || self.create_failing_passes > 0 {
            return;
        }
        self.proof_holds = self.proof_holds.saturating_add(1);
        if self.proof_holds < CREATE_PROOF_ALARM_HOLDS || self.proof_incident_open {
            return;
        }
        self.proof_incident_open = true;
        cloud.incidents.open(crate::incidents::OpenReq {
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

/// The NEVER-DARK invariant alarm: a MANAGED name this pass desired A/AAAA/NS
/// records for must end the pass with at least one of them published. Names
/// with nothing desired are skipped — an operator emptying a name is a
/// withdrawal, not an outage. Edge-triggered per name (`guards.dark_alarmed`)
/// so a sustained block opens one incident, not one per pass.
///
/// `end_count` is the pass-start listing folded with this pass's CONFIRMED
/// writes — never a re-list, which would race Vercel's eventually-consistent
/// listing and could miss a just-created record (the same race the ACME
/// orphan sweeper's age gate exists for), crying wolf on a dark-name alarm.
fn alarm_dark_names(
    domain: &str,
    desired: &[DesiredRecord],
    managed_names: &[&str],
    end_count: &HashMap<&str, usize>,
    guards: &mut ReconcileGuards,
    cloud: &Arc<CloudState>,
) {
    for name in managed_names {
        let wants = desired
            .iter()
            .any(|d| d.name == *name && (d.rtype == "A" || d.rtype == "AAAA" || d.rtype == "NS"));
        let key = format!("{domain}\u{0}{name}");
        if !wants || end_count.get(*name).copied().unwrap_or(0) > 0 {
            guards.dark_alarmed.remove(&key);
            continue;
        }
        tracing::error!(
            %domain,
            name = %name,
            "DNS reconciler: managed name ends the pass with NEITHER addresses NOR delegation published — it is DARK"
        );
        if guards.dark_alarmed.insert(key) {
            cloud.incidents.open(crate::incidents::OpenReq {
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
}

/// One reconcile pass over one zone. Returns the pass-start zone listing on
/// success (the caller reads live delegation state from it — see
/// `api_ns_published` in the reconcile loop); Err on API failure (caller
/// backs off).
async fn reconcile_zone<A: DnsApi>(
    api: &A,
    domain: &str,
    desired: &[DesiredRecord],
    managed_names: &[&str],
    cloud: &Arc<CloudState>,
    guards: &mut ReconcileGuards,
) -> anyhow::Result<Vec<RecordView>> {
    let current = api.list(domain).await?;
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
        return Ok(current);
    }
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
    //
    // A desired NS delegation counts as "this name is deliberately
    // address-less at the parent" — its addresses live in the child zone.
    // Without that exemption this hold DEADLOCKS the delegation cutover,
    // live-witnessed on the api label: the hold refused to delete the flat
    // api A set (zero desired A/AAAA once delegate_api engaged) while Vercel
    // answered every NS create with 409 Conflict against those very held
    // records — NS and address records cannot coexist on one name. The hold
    // still protects every name that lost its addresses WITHOUT a delegation
    // taking their place.
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
    // ACME DNS-01 orphan sweep — runs EVERY pass, converged or not. Issuance
    // cleanup races Vercel's eventually-consistent listing (create returns
    // before list shows the record), so a finished order's TXT can survive
    // cleanup and sit under its name forever — live-witnessed 2026-07-29: an
    // orphaned `_acme-challenge.api` TXT made Vercel answer every NS create
    // for the api delegation with 409 record_conflicts ("child records
    // exist"), stranding api.shadw.cloud dark. Any `_acme-challenge.*` TXT
    // whose value is NOT an in-flight challenge (the replicated store is the
    // placement source of truth) and older than the minimum age is an orphan:
    // delete it here, where the zone listing already exists, instead of
    // letting it veto a future delegation.
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

    let plan = plan_writes(&current, desired, managed_names, creates, deletes);
    // Projected end-of-pass A/AAAA/NS count per managed name, seeded from the
    // pass-start listing and folded with each CONFIRMED write below — the
    // input to the never-dark alarm at the end of the pass.
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
        // Converged — write nothing. The alarm still runs: it clears the
        // edge trigger for names that have recovered.
        alarm_dark_names(domain, desired, managed_names, &end_count, guards, cloud);
        return Ok(current);
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
    let mut creates_attempted = 0usize;
    let mut creates_succeeded = 0usize;

    // Phase 0: a disengaging delegation's NS deletes free the name for any
    // address creates below (Vercel forbids NS and address records coexisting
    // on one name). The deleted NS records are REMEMBERED per name: if the
    // flat-address creates for that name then ALL fail (sustained 429s), the
    // delegation is restored below — the never-dark rule is symmetric, or a
    // fleet roll dipping the capable set below floor would strand the api
    // label for a whole backoff interval (adversarial-review confirmed).
    //
    // CIRCUIT: when Vercel refuses creates account-wide (the 2026-08-04
    // fair-use 402 block: EVERY create fails while deletes keep working) this
    // delete-first sequence is unrecoverable by construction — the rollback's
    // NS restore is itself a create. An open circuit therefore SKIPS the NS
    // deletes and leaves the existing delegation in place.
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
                // Hoisted deletes are NS records by construction (see
                // plan_writes), so every confirmed delete shrinks the name's
                // projected delegation count.
                if let Some(c) = end_count.get_mut(r.name.as_str()) {
                    *c = (*c).saturating_sub(1);
                }
                disengaged_ns.entry(r.name.as_str()).or_default().push(r);
            }
        }
    }

    // Phase 1: delegation cutovers as restore-on-failure transactions — the
    // NEVER-DARK rule. The name's flat addresses must go before Vercel accepts
    // its NS records, so the dark window is bounded to this one transaction;
    // if ANY NS create fails, every NS placed here is removed and every
    // address record restored immediately, leaving the name exactly as it was
    // (last-known-good) for next pass's retry.
    for (name, addr_records, ns_records) in &plan.cutovers {
        // Same circuit as phase 0: under an account-wide create block the
        // rollback's address restores are themselves creates, so the whole
        // transaction is unrecoverable by construction — don't start it.
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
        // Roll back: remove the partial NS (both what this pass created AND
        // any leftovers already present — either blocks the address restores)
        // before re-creating the flat addresses.
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
        // Log what ACTUALLY happened: an unverifiable "restored" claim is how
        // a name goes dark behind a green-looking log line.
        if restore_failed == 0 {
            tracing::error!(%domain, name = %name, "delegation cutover ROLLED BACK — flat addresses restored, NS delegation retries next pass");
        } else {
            tracing::error!(%domain, name = %name, restore_failed, "delegation cutover rollback INCOMPLETE — address restores failed; the never-dark alarm below opens an incident if the name now has neither addresses nor delegation");
        }
    }

    // Phase 2: the ordinary creates-then-deletes flow for everything else.
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
                // diff only emits A/AAAA/NS creates, so every confirmed create
                // grows the name's projected published count.
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
    // Disengagement rollback (symmetric never-dark): a name whose NS records
    // were deleted in phase 0 and whose flat-address creates then ALL failed
    // gets its delegation BACK — the child zone keeps answering while the
    // flat set retries next pass. When at least one address create succeeded,
    // coexistence forbids the NS restore and the partial address set serves
    // (degraded, not dark) until the next pass completes it.
    //
    // The restore is VERIFIED per record and the outcome reported honestly —
    // live-witnessed 2026-08-04: under Vercel's fair-use block every NS
    // re-create 402'd, each failure was only WARN-logged, and the pass then
    // logged "ROLLED BACK — NS delegation restored" while api.shadw.cloud
    // served NEITHER addresses NOR delegation. Any failed restore now says
    // INCOMPLETE and opens a Major incident naming the name and the records.
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
    // NEVER-DARK, applied PER NAME to address records.
    //
    // `addr_create_ok` / `addr_create_failed` were already computed above but
    // nothing consulted them here, so an ordinary address ROTATION could empty a
    // name: the replacement A create returns 429, the old A deletes succeed
    // anyway, and the name ends the pass resolving to nothing. That is the exact
    // sequence witnessed on 2026-08-08 —
    //     WARN  DNS create failed ... 429 Too Many Requests (retryable)
    //     ERROR managed name ends the pass with NEITHER addresses NOR
    //           delegation published — it is DARK   name=shoomoo
    // — and it surfaces to a user as an intermittent FUNCTION_NO_RESPONSE on a
    // deployment that is running perfectly (every node served that app 200 while
    // its name was dark).
    //
    // Creates-before-deletes alone does not prevent this; the delete must also be
    // CONDITIONAL on its own name's create having landed. A STALE address is
    // strictly better than none: it still resolves to a node that serves, and the
    // next pass rotates it once the API stops refusing. Deletes for names whose
    // create succeeded, and every non-address delete, are unaffected.
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
        deleted_ok += 1;
        if let Some(r) = by_id.get(id.as_str()) {
            if r.rtype == "A" || r.rtype == "AAAA" || r.rtype == "NS" {
                if let Some(c) = end_count.get_mut(r.name.as_str()) {
                    *c = (*c).saturating_sub(1);
                }
            }
        }
    }
    // Fold this pass's create outcomes into the circuit BEFORE the Err return
    // below: a failing pass must still arm the guard for the next one.
    guards.record_pass(creates_attempted, creates_succeeded);
    if guards.create_failing_passes >= 2 && !guards.create_incident_open {
        guards.create_incident_open = true;
        cloud.incidents.open(crate::incidents::OpenReq {
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
    alarm_dark_names(domain, desired, managed_names, &end_count, guards, cloud);
    if let Some(e) = failed {
        // Partial progress already landed; surface the failure so the caller
        // backs off and the operator sees it.
        return Err(e);
    }
    let published: Vec<String> = desired
        .iter()
        .map(|r| format!("{} {} {}", r.name, r.rtype, r.value))
        .collect();
    // Report what ACTUALLY happened. This logged `deleted = plan.deletes.len()`
    // — the PLANNED count — so failed and skipped deletes were reported as
    // completed, and a pass that changed nothing read identically to one that
    // changed everything. `planned_deletes` is kept so a growing gap between
    // planned and done is visible, which is what a create/delete treadmill
    // against a rate-limited API looks like from the outside.
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

/// Node-local sidecar holding the flap memory, under `persist::data_dir()`
/// exactly like `dns_geo.json`: derived node-local state that must NOT ride
/// `PlatformSnapshot`/`store_sync` or any gossip snapshot arm.
const DAMPING_FILE: &str = "dns_damping.json";
const DAMPING_FORMAT_VERSION: u32 = 1;
/// Damping is a SHORT-HORIZON memory of flapping, not a durable fact. A
/// snapshot older than this describes a fleet that has since changed shape, so
/// it is dropped rather than replayed — an hour-old `withheld` set would
/// suppress a node that has been healthy all night.
const DAMPING_MAX_AGE_MS: u64 = 60 * 60 * 1000;
/// Cap enforced on LOAD as well as at runtime, so no on-disk file can reload
/// past it (the `dns_geo` `MAX_ENTRIES` rule).
const DAMPING_MAX_NODES: usize = 512;
const DAMPING_MAX_FILE_BYTES: u64 = 1 << 20;

/// On-disk shape of the flap memory. `BTreeMap` + a sorted `Vec` so equal state
/// serializes to equal BYTES — that byte-compare is the write gate, so an
/// unchanged pass touches no disk at all.
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

/// The reconciler's cross-pass flap memory, made to survive a leader handover.
///
/// Everything in here is a function of THIS node's own registry view — health
/// verdicts are per-observer (peers legitimately disagree), and `publishable`
/// is fed by the same view the acting leader would use — so the memory is
/// warmed on EVERY node that could ever act (`spawn_reconciler` only runs where
/// a `VERCEL_API_TOKEN` makes acting possible), not only on the leader. A newly
/// elected leader therefore starts with the streaks and `withheld` set it has
/// been maintaining all along, instead of a zeroed counter that lets a flapping
/// node straight back into the published set — the create/delete treadmill that
/// drew sustained 429s on 2026-07-29, and which the damping exists to kill.
///
/// Replication is deliberately NOT used: adopting a peer's streaks would mean
/// publishing on health verdicts THIS node never made, and the state is
/// reconstructible locally by construction. The durable half covers the case
/// warming cannot — a fleet-wide binary roll restarts every node at once, which
/// is exactly the reconvergence that flaps nodes — so the file is node-local
/// (no replication) and age-gated.
struct DampingMemory {
    publish: PublishDamping,
    api: DelegationDamping,
    /// Last bytes written; the change gate (empty = never written this boot).
    written: Vec<u8>,
    enabled: bool,
}

impl DampingMemory {
    /// Load the persisted memory. EVERY failure mode — missing, unreadable,
    /// oversized, corrupt, wrong version, stale — degrades to an empty memory
    /// plus a log line: a bad scratch file must cost one flap window, never a
    /// boot failure on the node that publishes the fleet's DNS.
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
        // Absent is the normal first-boot case, not an error worth logging.
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
            tracing::info!(age_ms = age, "dns damping: persisted flap memory is stale; starting empty");
            return out;
        }
        out.publish.unhealthy = disk
            .unhealthy
            .into_iter()
            .take(DAMPING_MAX_NODES)
            .collect();
        out.publish.healthy = disk.healthy.into_iter().take(DAMPING_MAX_NODES).collect();
        out.publish.withheld = disk.withheld.into_iter().take(DAMPING_MAX_NODES).collect();
        out.api.ready = disk.api_ready;
        out.api.undeclared = disk.api_undeclared;
        // Seed the write gate with what the file already holds, so a boot that
        // changes nothing writes nothing.
        out.written = serde_json::to_vec(&out.rows()).unwrap_or_default();
        tracing::info!(
            withheld = out.publish.withheld.len(),
            api_ready = out.api.ready,
            age_ms = age,
            "dns damping: resumed persisted flap memory"
        );
        out
    }

    /// The comparable (timestamp-free) on-disk projection of the live memory.
    ///
    /// Every streak is CLAMPED to the threshold it feeds. The live counters
    /// grow without bound (a node unhealthy for a day, a delegation ready for a
    /// week) while every read of them is a `>=` against a K of 2, so clamping
    /// is behaviour-identical and it is what keeps the byte-compare write gate
    /// meaningful — otherwise a monotonically ticking counter would rewrite the
    /// file every single pass forever.
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

    /// Persist when — and only when — the memory actually changed. Atomic
    /// temp-file + fsync + rename, the durability shape every sidecar under the
    /// data dir uses (`persist::save`, `dns_geo`), on `spawn_blocking` for the
    /// same reason `dns_geo` does: the write ends in an fsync, which must not
    /// run on a runtime worker.
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

/// Leader-elected reconcile loop. Runs on every node; only the elected leader
/// (same election as the billing meter: lowest healthy iroh identity) acts.
/// Enabled when a `VERCEL_API_TOKEN` is present AND (`HIVE_INGRESS != ngrok` or
/// `HIVE_DNS_RECONCILE=1` for pre-cutover testing).
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
        // Flap memory: warmed on every pass on EVERY node and durable
        // node-locally, so a leader handover inherits it instead of zeroing it
        // (see `DampingMemory`).
        let mut memory = DampingMemory::load();
        let mut guards = ReconcileGuards::default();
        // Whether the platform zone published NS records on `api` at the last
        // successful reconcile of that zone (None = not yet observed). This is
        // the ground truth the api-delegation decision holds or falls back
        // from — a freshly-elected leader must never plan NS deletes on its
        // first pass, so the unknown state is the safe-direction Hold. The
        // observation refreshes only on a pass with ZERO failed writes; a
        // stale Some(true) can hold the api name for one extra interval after
        // a block lifts (the first clean pass corrects it) — benign, because
        // while writes are still failing the flat-set creates would fail too.
        //
        // Deliberately NOT inherited across a handover either: it is an
        // observation of a zone this node has not listed since it lost
        // leadership, during which another leader may have changed the
        // delegation. Losing leadership resets it to the same unknown a fresh
        // process starts with, so the first-pass Hold is preserved exactly.
        let mut api_ns_published: Option<bool> = None;
        let mut backoff: u64 = 0; // consecutive failures
                                  // Edge-trigger for the "delegation held" incident (see the call site).
        let mut geo_hold_active = false;
        let mut api_hold_active = false;
        // Leadership tenure edge: `false` → `true` is this node taking over.
        let mut was_leader = false;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
        loop {
            tick.tick().await;
            if backoff > 0 && was_leader {
                // Exponential backoff on API failure: 30s * 2^n, capped at 5 min.
                // Leader-only: a follower makes no API calls, so it has nothing
                // to back off from and must keep warming its damping on the
                // regular cadence.
                let extra = (interval << backoff.min(4)).min(300);
                tokio::time::sleep(std::time::Duration::from_secs(
                    extra.saturating_sub(interval),
                ))
                .await;
            }
            // Nameserver eligibility is decided ONCE per pass, from the same
            // registry snapshot everything else in the pass uses, by the same
            // function `GET /v1/dns/stats` calls — an operator must never be
            // looking at a different verdict than the one being published.
            let registry_nodes = cloud.registry.nodes();
            // Same single-writer resolution as admin mutations, ACME and the
            // billing meter (owner chain first, health+addressability gated;
            // identity election fallback) — one designation for every
            // single-writer role, structurally closing the CP-vs-DNS pin drift
            // (proposal step 6). `HIVE_DNS_LEADER_NODE` remains honored as a
            // deliberate LEGACY split-pin on the fallback path (health-gated,
            // never a raw unguarded check — an unguarded pin silently freezes
            // published DNS if the pinned node dies).
            let dns_pref = std::env::var("HIVE_DNS_LEADER_NODE")
                .ok()
                .filter(|s| !s.trim().is_empty());
            let chain = crate::cluster::Cluster::owner_chain_from_env();
            let pref = dns_pref.or_else(|| std::env::var("HIVE_CP_LEADER").ok());
            let leader = crate::cluster::Cluster::control_plane_owner(
                &chain,
                pref.as_deref(),
                &registry_nodes,
            );
            let is_leader = leader.as_deref() == Some(cloud.node_name.as_str());
            // Convergence guard: a node that was told to JOIN an existing fleet
            // (HIVE_BOOTSTRAP_PEERS set) but currently sees ONLY itself in the
            // registry has not yet synced gossip — its view of the healthy set
            // is a transient single-self, and reconciling from it would DELETE
            // every peer's A-record and publish only this node's IP fleet-wide
            // (live-observed: a freshly-joined 9th node clobbered
            // shadw.cloud/*.shadw.app to only-itself for ~30s until the real
            // leader re-reconciled). A founding node (no bootstrap peers) has
            // no peers to clobber, so it is exempt and still bootstraps DNS.
            //
            // It gates the damping WARM-UP below too: `publishable` drops the
            // streaks of nodes absent from the registry, so folding a
            // single-self view in would wipe the very `withheld` set the warm
            // memory exists to carry across a handover.
            let has_bootstrap = std::env::var("HIVE_BOOTSTRAP_PEERS")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            if has_bootstrap && registry_nodes.len() <= 1 {
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
            // ---- warm the flap memory: EVERY node, leader or not ----
            //
            // Both damping structures are pure functions of this node's own
            // registry view, and both are two-sided precisely so a flapping
            // node cannot drive a create/delete treadmill against the Vercel
            // API. Folding the view in on every pass — rather than only while
            // holding leadership — is what makes a handover a non-event: the
            // node that takes over has been counting the same streaks all
            // along, so its first pass applies the SAME withdraw/republish
            // damping the outgoing leader was applying, instead of a zeroed
            // counter that republishes every withheld node at once. This runs
            // before the leader gate for that reason and must stay there.
            //
            // Nothing here writes to the DNS API or to STATS: a follower's
            // pass is a read of its own registry plus an in-memory fold. The
            // `api_delegation_records` / `geo_ns_*` gauges stay leader-only
            // because acme.rs keys its delegated-zone behaviour off them.
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
                // Tenure end. Everything scoped to "while I was the writer" is
                // now stale evidence about a zone someone else is changing:
                // the backoff counts API failures this node is no longer
                // making, and the delegation observation is a listing this
                // node will not refresh. Both drop to their unknown state so a
                // return to leadership re-proves rather than resumes.
                was_leader = false;
                backoff = 0;
                api_ns_published = None;
                continue;
            }
            if !was_leader {
                was_leader = true;
                guards.begin_tenure();
                // The two "delegation held" incidents are edge-triggered per
                // WRITER: carrying a previous tenure's flag across a foreign
                // tenure would silently suppress the announcement that the
                // hold is still in force under the new leader. Re-arming costs
                // at most one incident per handover and always errs toward
                // telling the operator.
                geo_hold_active = false;
                api_hold_active = false;
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
            // Deployment affinity: point each served label straight at its host
            // node so a client lands on the owner instead of a random node that
            // then has to forward. Precedence (first writer wins):
            //   1. local labels whose deployment is READY — authoritative,
            //   2. the gossiped peer route table,
            //   3. local labels we hold but cannot serve READY.
            //
            // Tier 3 is why this is ordered rather than a single `served_hosts()`
            // pass. A specific record BEATS the wildcard, so attributing a label
            // to a node holding only a failed build or an orphaned `Building…`
            // placeholder pins every client to the one node that cannot answer —
            // exactly what stranded `archive-zip.shadw.app` on fc-sanjose's
            // `Error` placeholder while fc-sanjose-gpu-1 served the Ready
            // deployment (2026-08-05). It stays LAST rather than being dropped
            // so a label no peer route covers still gets an answer.
            let ready_local: Vec<String> = cloud.gw.served_hosts_ready();
            let mut owners: Vec<(String, String)> = ready_local
                .iter()
                .map(|h| (h.clone(), cloud.node_name.clone()))
                .collect();
            {
                let routes = cloud.peer_routes.read();
                for (label, rs) in routes.iter() {
                    // Lowest-latency healthy route wins when several nodes serve
                    // the same label (replicas): that is the best single answer
                    // we can give, and the wildcard still covers the rest.
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
            // Per-region names in BOTH zones: `<region>.<apps>` for app traffic
            // and `api-<region>.<platform>` for the API/control surface.
            //
            // COLLISION: a project literally named `san-jose` owns that same
            // single label in the apps zone. A real deployment is user-visible
            // and predates this feature, so the affinity record wins and the
            // region name is dropped for that one region — publishing both would
            // put two different A sets under one name and break the deployment.
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
            // Geo-zone delegation: hand the deploy zone to the fleet's own
            // nameservers so Seer's geo/health-aware answers are actually
            // reachable (Vercel DNS itself has no geo routing). Published into
            // the apps zone because the deploy zone is a child of it, and only
            // for nodes PEERS HAVE PROVEN answer DNS, never for nodes that
            // merely claim to (see desired_geo_delegation).
            let geo_zone_label =
                crate::dnsserver::deploy_zone().and_then(|dz| geo_label(dz, &cloud.apps_domain));
            let (geo_records, geo_names) = geo_zone_label
                .as_deref()
                .map(|label| desired_geo_delegation(&publish, label, &cloud.apps_domain))
                .unwrap_or_default();
            STATS
                .geo_delegation_records
                .store(geo_records.len() as u64, Ordering::Relaxed);
            // A HELD delegation must be loud. `desired_geo_delegation` returning
            // nothing while nodes still declare `dns_ns` means the zone is
            // running on last-known-good NS records that nobody can currently
            // prove — publishing the (0 or 1) proven ones would blackhole it,
            // and staying silent about it would leave an operator believing the
            // delegation is healthy. Incident on the TRANSITION only: this
            // condition persists for as long as the proof is missing, and
            // `incidents::open` does not dedup, so per-pass would bury the
            // incident list.
            if geo_zone_label.is_some() && geo_records.is_empty() && nodes.iter().any(|n| n.dns_ns)
            {
                STATS.geo_delegation_holds.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    proven = proven.len(),
                    unproven = ?unproven,
                    "geo delegation HELD: fewer than 2 nameservers are currently proven reachable — keeping the published NS records rather than deleting them (deleting would blackhole the whole zone)"
                );
                if !geo_hold_active {
                    geo_hold_active = true;
                    cloud.incidents.open(crate::incidents::OpenReq {
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
                }
            } else {
                geo_hold_active = false;
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
            // Geo-DNS for the API host: when >=2 `dns_api`-capable nameservers
            // are PEER-PROVEN (and have been for HEALTHY_PASSES_BEFORE_REPUBLISH
            // consecutive passes), delegate the `api` label to Seer (health-
            // aware, proximity-ordered answers) and withhold the flat
            // round-robin A set. Below the floor the pass HOLDS whatever is
            // published — a proof dip must never plan a disengagement
            // (live-witnessed 2026-08-04: that disengagement deleted six NS
            // records into an account-wide create block and stranded
            // api.shadw.cloud dark). With no delegation published, the flat
            // set stays — self-healing in both directions as the fleet rolls
            // or the capable set shrinks.
            //
            // The decision itself was taken in the warm-up block above (its
            // two-sided damping has to advance on every node, so a handover
            // does not restart the engagement/disengagement streaks); only the
            // gauges and the write plan it drives are leader-side.
            let (api_records, api_hold) = match api_decision {
                ApiDelegation::Delegate(records) => {
                    STATS
                        .api_delegation_records
                        .store(records.len() as u64, Ordering::Relaxed);
                    (records, false)
                }
                // HOLD: the gauge is deliberately left untouched — it keys
                // acme.rs's live-delegation gate, and the whole point of the
                // hold is that the LIVE delegation is still out there.
                ApiDelegation::Hold => (Vec::new(), true),
                ApiDelegation::Flat => {
                    STATS.api_delegation_records.store(0, Ordering::Relaxed);
                    (Vec::new(), false)
                }
            };
            // A HELD api delegation must be loud, same rule as the geo hold:
            // the name is running on last-known-good NS records nobody can
            // currently prove. Incident on the TRANSITION only
            // (`incidents::open` does not dedup), and only when a delegation
            // is actually published — a hold over an UNOBSERVED name (a fresh
            // leader's first passes) is the safe-direction default, not an
            // incident.
            if api_hold {
                STATS.api_delegation_holds.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    proven = proven.len(),
                    declared = api_declared,
                    delegated = ?api_ns_published,
                    "api delegation HELD: fewer than 2 proven api-capable nameservers — the api name is left exactly as published (changing it on a proof dip is how a name goes dark)"
                );
                if !api_hold_active && api_ns_published == Some(true) {
                    api_hold_active = true;
                    cloud.incidents.open(crate::incidents::OpenReq {
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
                api_hold_active = false;
            }
            // During a HOLD the flat set is withheld too: child address records
            // under the live delegation would be occluded AND would veto a
            // later NS re-creation (Vercel's coexistence rule).
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
            let mut platform_managed: Vec<String> =
                ["api", "admin", "webhook", "sms", "relay", "discovery"]
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
            // During an api HOLD the name is UNMANAGED this pass: the diff must
            // never see its records, or it would plan the very disengagement
            // the hold exists to prevent.
            if api_hold {
                platform_managed.retain(|n| n != "api");
            }
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
                for (db_host, host_node) in dir {
                    let Some(slug) = db_host.strip_suffix(&suffix) else {
                        continue;
                    };
                    if !seen.insert(slug.to_string()) {
                        continue;
                    }
                    let Some(node) = all_nodes.iter().find(|n| n.name == host_node) else {
                        continue;
                    };
                    managed.push(slug.to_string());
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
            backoff = if ok { 0 } else { (backoff + 1).min(6) };
            STATS.passes.fetch_add(1, Ordering::Relaxed);
            STATS
                .last_pass_ms
                .store(hive_core::now_ms(), Ordering::Relaxed);
        }
    });
}

// ---- tests ---------------------------------------------------------------------

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
