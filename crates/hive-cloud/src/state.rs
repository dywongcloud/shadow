//! Shared state for a hive-cloud node: every platform subsystem in one place.

use std::collections::VecDeque;
use std::sync::Arc;

use fluid_compute::Fluid;
use fluid_gateway::Gateway;
use hive_controlplane::Hive;
use hive_core::now_ms;
use hive_edge::{
    bot::BotPolicy, BotManager, CdnCache, ConcurrencyLimiter, CronScheduler, NodeRegistry,
    RateLimiter, Router, RuntimeCache, Waf, WorkflowEngine,
};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

/// A peer node that serves a given deployment (for mesh routing/load-balancing).
#[derive(Clone, Serialize)]
pub struct PeerRoute {
    pub node_id: String,
    pub region: String,
    /// The peer's public gateway base URL (e.g. http://192.168.1.20:8787).
    pub gateway: String,
    /// Last-known round-trip latency (ms) for anycast ordering.
    pub latency_ms: u64,
    pub healthy: bool,
    /// Epoch-ms this route was last refreshed by gossip (#24). Lets a route from a
    /// momentarily-unreachable-but-healthy peer survive a transient gossip miss for
    /// a TTL instead of being dropped instantly (which would 404 the deployment).
    #[serde(default)]
    pub last_seen_ms: u64,
}

/// TTL for gossiped routing mappings (#24): a route survives this long without a
/// refresh before it's dropped. Matches the node-registry staleness window so the
/// routing table and the node roster age out consistently.
pub const ROUTE_TTL_MS: u64 = 30_000;

/// Merge freshly-gossiped routes with the previous table, keeping a route from a
/// peer NOT reached this round only while it's within the TTL (#24). A peer that
/// WAS reached this round is authoritative — its fresh list replaces its old
/// routes, so a genuinely-removed deployment still ages out immediately. Pure +
/// testable.
pub fn merge_routes_ttl(
    prev: &std::collections::HashMap<String, Vec<PeerRoute>>,
    fresh: std::collections::HashMap<String, Vec<PeerRoute>>,
    seen_nodes: &std::collections::HashSet<String>,
    now: u64,
    ttl_ms: u64,
) -> std::collections::HashMap<String, Vec<PeerRoute>> {
    let mut merged = fresh;
    for (host, routes) in prev {
        for r in routes {
            // Reached-this-round peers are authoritative (don't resurrect dropped
            // routes); only carry forward unreached peers within the TTL.
            if seen_nodes.contains(&r.node_id) || now.saturating_sub(r.last_seen_ms) >= ttl_ms {
                continue;
            }
            let entry = merged.entry(host.clone()).or_default();
            if !entry.iter().any(|e| e.node_id == r.node_id) {
                entry.push(r.clone());
            }
        }
    }
    merged
}

/// Merge the freshly-gossiped fleet deployment map with the previous one so a node's
/// deployments DON'T vanish on a single missed `/v1/fleet-deployments` fetch (the map
/// is keyed by node name). Without this, a transient gossip miss to a peer wiped its
/// projects from the coordinator → the dashboard's workflows/runs tables, the
/// deployments list, and the per-project proxy all intermittently went empty.
///
/// Rules: a node present in `fresh` is authoritative (we reached it this round — even an
/// empty list means "it genuinely has none now"). A node only in `prev` is carried
/// forward IFF it's still `alive` (present in the registry); once it ages out of the
/// registry (real staleness — see `NodeRegistry::nodes`), its deployments drop too.
pub fn merge_deployments_ttl(
    prev: &std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>>,
    fresh: std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>>,
    alive: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>> {
    let mut merged = fresh;
    for (node, deps) in prev {
        if merged.contains_key(node) {
            continue; // reached this round → authoritative
        }
        if !alive.contains(node) {
            continue; // node aged out of the registry → drop its deployments
        }
        merged.insert(node.clone(), deps.clone()); // alive but missed this round → keep
    }
    merged
}

/// One observed request/event, for the dashboard's live log + analytics.
#[derive(Clone, Serialize)]
pub struct Event {
    pub ts_ms: u64,
    pub region: String,
    pub method: String,
    pub host: String,
    pub path: String,
    pub status: u16,
    pub action: String, // allow | waf-deny | bot-block | cache-hit | cron | ...
    pub detail: String,
    #[serde(default)]
    pub project: String,
    /// Correlation id for tracing a request across nodes (accepted from
    /// `x-hive-request-id` or generated at ingress; forwarded over the mesh).
    #[serde(default)]
    pub request_id: String,
}

pub struct CloudState {
    pub region: String,
    pub node_name: String,
    pub public_base: String,
    /// Host suffixes this node will route on (the pooled wildcard ingress roots,
    /// e.g. `deployment.shadow.ngrok.pizza`). A multi-label Host that doesn't end
    /// with one is a foreign root and is rejected before routing — so a foreign
    /// domain whose first label collides with a real alias can't be served.
    /// Configured via `HIVE_DEPLOY_SUFFIXES` (comma-separated).
    pub deploy_suffixes: Vec<String>,

    pub waf: Arc<Waf>,
    pub bot: Arc<BotManager>,
    pub bot_policy: RwLock<BotPolicy>,
    pub cdn: Arc<CdnCache>,
    /// Regional runtime/data cache for tenant functions (Vercel Runtime Cache).
    pub runtime_cache: Arc<RuntimeCache>,
    pub limiter: Arc<ConcurrencyLimiter>,
    /// Per-IP L7 rate limiter (DDoS mitigation) at the edge.
    pub ratelimit: Arc<RateLimiter>,
    pub router: Arc<Router>,
    pub registry: Arc<NodeRegistry>,
    pub cron: Arc<CronScheduler>,
    pub workflows: Arc<WorkflowEngine>,

    pub gw: Arc<Gateway>,
    pub fluid: Arc<Fluid>,
    pub hive: Arc<Hive>,
    pub http: reqwest::Client,
    pub projects: crate::project_settings::ProjectStore,
    pub builds: crate::git::BuildStore,
    pub cluster: Arc<crate::cluster::Cluster>,
    pub teams: crate::teams::TeamStore,
    pub gitops: crate::gitops::GitOpsStore,
    /// Mesh peer admin URLs (for P2P build-cache pulls).
    pub peers: RwLock<Vec<String>>,
    /// node name -> that node's admin URL (learned via gossip). Lets the placement
    /// scheduler dispatch a deploy to a specific target node's admin.
    pub node_admins: RwLock<std::collections::HashMap<String, String>>,
    /// Trusted peer iroh `EndpointId`s for P2P admission (#20). Seeded from
    /// `HIVE_TRUSTED_NODE_IDS` and augmented from the gossip roster (whose iroh
    /// addrs arrive over the operator-controlled, SSH-tunneled admin channel — a
    /// sound trust root). The iroh accept loop rejects any peer not in here when
    /// `HIVE_PEER_TRUST` enforcement is on.
    pub trusted_peer_ids: hive_p2p::TrustSet,
    /// Gossip transport map: peer admin URL -> (node_id, iroh addr_json), learned
    /// from each roster exchange. Lets the gossip loop reach a peer over the iroh
    /// QUIC mesh (when `HIVE_GOSSIP_IROH` is set) instead of HTTP-over-SSH, once it
    /// knows the peer's iroh address. Empty until the first roster is read (which
    /// bootstraps over HTTP).
    pub peer_iroh: RwLock<std::collections::HashMap<String, (String, String)>>,
    /// Control-plane health (#25): epoch-ms of the last SUCCESSFUL gossip sync from
    /// any peer. The data plane keeps serving from local persisted state when this
    /// goes stale (peers unreachable) — this just makes the degradation observable
    /// and lets routing fail closed for unknown deployments. 0 = never synced.
    pub last_gossip_ok_ms: std::sync::atomic::AtomicU64,
    /// Cross-node routing table: deployment subdomain -> peer nodes that serve it
    /// (learned via gossip). Lets any node route/load-balance requests to the node
    /// that actually hosts a deployment.
    pub peer_routes: RwLock<std::collections::HashMap<String, Vec<PeerRoute>>>,
    /// Deployments hosted on each peer node (name -> its deployments), learned via
    /// gossip. Lets the dashboard's per-project deployment list show deployments
    /// that the placement scheduler placed on OTHER nodes (e.g. the default
    /// San-Jose placement), not just the ones this coordinator hosts locally.
    pub peer_deployments: RwLock<std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>>>,
    /// This node's iroh P2P endpoint (real QUIC mesh transport), if bound. Used to
    /// dial peers and tunnel cross-node requests over QUIC (with HTTP fallback).
    pub iroh: RwLock<Option<hive_p2p::Endpoint>>,
    /// Pooled cross-node mesh transport: one persistent iroh QUIC connection per
    /// peer, a NEW stream per request (no per-request handshake). Set when `iroh`
    /// binds; `None` = P2P disabled (HTTP mesh still routes). See [`hive_p2p::PeerPool`].
    pub mesh: RwLock<Option<Arc<hive_p2p::PeerPool>>>,
    /// Single-owner placement leases for stateful CONTAINER deployments (fenced,
    /// consensus-free). See `lease.rs`.
    pub leases: crate::lease::LeaseStore,
    /// Which nodes hold each container deployment (gossiped), so lease election
    /// (rendezvous hashing) only considers nodes that can actually run it.
    pub container_holders: RwLock<std::collections::HashMap<String, Vec<String>>>,
    pub webhooks: Arc<crate::webhooks::WebhookStore>,
    pub databases: Arc<crate::databases::DatabaseStore>,
    pub metrics: crate::metrics::MetricsStore,
    pub incidents: crate::incidents::IncidentStore,
    pub securelinks: crate::securelink::SecureLinkStore,
    pub apikeys: crate::apikeys::ApiKeyStore,
    pub identity: crate::identity::IdentityStore,
    pub domains: crate::dns::DomainStore,
    pub docs: crate::docstore::DocStore,
    pub billing: crate::billing::BillingStore,
    pub audit: crate::audit::AuditLog,
    pub notifications: crate::notifications::NotificationStore,
    /// Platform owner identity (seeds the default team; ops dashboard owner).
    pub owner_email: String,

    events: Mutex<VecDeque<Event>>,
    req_count: Mutex<u64>,
    blocked_count: Mutex<u64>,
}

/// Core of [`CloudState::host_allowed`] (a free fn so it's unit-testable without
/// constructing a whole node). A bare/single-label or empty host is allowed
/// (local/direct/default deployment); a multi-label host must equal or end with
/// `.<suffix>` for some configured suffix, else it's a foreign root.
fn host_has_allowed_suffix(host: &str, suffixes: &[String]) -> bool {
    let h = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    if h.is_empty() || !h.contains('.') {
        return true;
    }
    suffixes.iter().any(|s| h == *s || h.ends_with(&format!(".{s}")))
}

/// Default TTL for control-plane degradation (#25): if no peer gossip has succeeded
/// within this window (and peers are configured), the node reports degraded. The
/// gossip loop runs on a ~5s cadence, so 30s tolerates a few missed rounds.
pub const CP_DEGRADED_TTL_MS: u64 = 30_000;

/// Pure core of control-plane degradation (#25). Degraded = we have peers configured
/// (so we EXPECT gossip) but the last successful sync is older than the TTL (or never
/// happened). A single-node deployment (no peers) is never "degraded". Unit-testable
/// without a node.
pub fn cp_degraded(last_ok_ms: u64, peer_count: usize, now_ms: u64, ttl_ms: u64) -> bool {
    if peer_count == 0 {
        return false; // standalone node — no control plane to be degraded
    }
    last_ok_ms == 0 || now_ms.saturating_sub(last_ok_ms) > ttl_ms
}

impl CloudState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        region: String,
        node_name: String,
        public_base: String,
        waf: Arc<Waf>,
        bot: Arc<BotManager>,
        cdn: Arc<CdnCache>,
        limiter: Arc<ConcurrencyLimiter>,
        router: Arc<Router>,
        registry: Arc<NodeRegistry>,
        cron: Arc<CronScheduler>,
        workflows: Arc<WorkflowEngine>,
        gw: Arc<Gateway>,
        fluid: Arc<Fluid>,
        hive: Arc<Hive>,
    ) -> Arc<CloudState> {
        let cluster = crate::cluster::Cluster::new(node_name.clone());
        let owner_email =
            std::env::var("HIVE_OWNER_EMAIL").unwrap_or_else(|_| "owner@hive.cloud".into());
        // Allowed wildcard ingress roots. Default to the pooled deployment domain
        // plus `localhost` for local dev; override with HIVE_DEPLOY_SUFFIXES.
        let deploy_suffixes = std::env::var("HIVE_DEPLOY_SUFFIXES")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                // Legacy region-agnostic zone + the per-region ingress zones
                // (<dep>.<code>.ngrok.pizza, code = iad/sin/sfo/lax) + localhost.
                vec![
                    "deployment.shadow.ngrok.pizza".into(),
                    "iad.ngrok.pizza".into(),
                    "sin.ngrok.pizza".into(),
                    "sfo.ngrok.pizza".into(),
                    "lax.ngrok.pizza".into(),
                    "localhost".into(),
                ]
            });
        let teams = crate::teams::TeamStore::new();
        teams.ensure_seed(&owner_email);
        Arc::new(CloudState {
            region,
            node_name,
            public_base,
            deploy_suffixes,
            waf,
            bot,
            bot_policy: RwLock::new(BotPolicy::default()),
            cdn,
            runtime_cache: Arc::new(RuntimeCache::new()),
            limiter,
            ratelimit: Arc::new(RateLimiter::new(100, 10_000)),
            router,
            registry,
            cron,
            workflows,
            gw,
            fluid,
            hive,
            http: reqwest::Client::new(),
            projects: crate::project_settings::ProjectStore::new(),
            builds: crate::git::BuildStore::new(),
            cluster,
            teams,
            gitops: crate::gitops::GitOpsStore::new(),
            peers: RwLock::new(Vec::new()),
            node_admins: RwLock::new(std::collections::HashMap::new()),
            trusted_peer_ids: {
                // Seed the P2P trust allowlist from HIVE_TRUSTED_NODE_IDS (#20).
                let mut set = std::collections::HashSet::new();
                if let Ok(ids) = std::env::var("HIVE_TRUSTED_NODE_IDS") {
                    for id in ids.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                        set.insert(id.to_string());
                    }
                }
                std::sync::Arc::new(std::sync::RwLock::new(set))
            },
            peer_iroh: RwLock::new(std::collections::HashMap::new()),
            last_gossip_ok_ms: std::sync::atomic::AtomicU64::new(0),
            peer_routes: RwLock::new(std::collections::HashMap::new()),
            peer_deployments: RwLock::new(std::collections::HashMap::new()),
            iroh: RwLock::new(None),
            mesh: RwLock::new(None),
            leases: crate::lease::LeaseStore::new(),
            container_holders: RwLock::new(std::collections::HashMap::new()),
            webhooks: Arc::new(crate::webhooks::WebhookStore::new()),
            databases: Arc::new(crate::databases::DatabaseStore::new()),
            metrics: crate::metrics::MetricsStore::new(),
            incidents: crate::incidents::IncidentStore::new(),
            securelinks: crate::securelink::SecureLinkStore::new(),
            apikeys: crate::apikeys::ApiKeyStore::new(),
            identity: crate::identity::IdentityStore::new(),
            domains: crate::dns::DomainStore::new(),
            docs: crate::docstore::DocStore::new(),
            billing: crate::billing::BillingStore::new(),
            audit: crate::audit::AuditLog::new(crate::persist::data_dir().join("audit.jsonl")),
            notifications: crate::notifications::NotificationStore::new(),
            owner_email,
            events: Mutex::new(VecDeque::with_capacity(512)),
            req_count: Mutex::new(0),
            blocked_count: Mutex::new(0),
        })
    }

    /// Mark a successful control-plane (gossip) sync (#25).
    pub fn mark_gossip_ok(&self) {
        self.last_gossip_ok_ms
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Epoch-ms of the last successful gossip sync (0 = never).
    pub fn last_gossip_ms(&self) -> u64 {
        self.last_gossip_ok_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the control plane is degraded: we have peers configured but haven't
    /// synced from any within the TTL. (Serving continues from local persisted state
    /// regardless — this is observability + a signal for fail-closed decisions.)
    pub fn control_plane_degraded(&self, ttl_ms: u64) -> bool {
        let peers = self.peers.read().len();
        cp_degraded(self.last_gossip_ms(), peers, now_ms(), ttl_ms)
    }

    pub fn record(&self, ev: Event) {
        *self.req_count.lock() += 1;
        if ev.action == "waf-deny" || ev.action == "bot-block" {
            *self.blocked_count.lock() += 1;
        }
        self.metrics.record(&ev);
        let mut q = self.events.lock();
        if q.len() >= 500 {
            q.pop_front();
        }
        q.push_back(ev);
    }

    pub fn recent_events(&self, limit: usize) -> Vec<Event> {
        let q = self.events.lock();
        q.iter().rev().take(limit).cloned().collect()
    }

    pub fn counters(&self) -> (u64, u64) {
        (*self.req_count.lock(), *self.blocked_count.lock())
    }

    /// Whether this node should route on `host`. A bare/single-label or empty
    /// host (local/direct/default deployment) is allowed; any multi-label host
    /// must end with a configured deploy suffix, else it's a foreign root.
    pub fn host_allowed(&self, host: &str) -> bool {
        host_has_allowed_suffix(host, &self.deploy_suffixes)
    }

    pub fn event(&self, region: &str, method: &str, host: &str, path: &str, status: u16, action: &str, detail: &str) -> Event {
        // Resolve which project this event belongs to (from the request host),
        // so project-scoped logs work regardless of how the request arrived.
        let project = self
            .gw
            .project_for_host(host)
            .unwrap_or_else(|| detail.to_string());
        Event {
            ts_ms: now_ms(),
            region: region.to_string(),
            method: method.to_string(),
            host: host.to_string(),
            path: path.to_string(),
            status,
            action: action.to_string(),
            detail: detail.to_string(),
            project,
            request_id: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{cp_degraded, host_has_allowed_suffix, merge_deployments_ttl};

    fn suffixes() -> Vec<String> {
        vec!["deployment.shadow.ngrok.pizza".into(), "localhost".into()]
    }

    fn dep(proj: &str) -> Vec<fluid_core::DeploymentInfo> {
        vec![serde_json::from_value(serde_json::json!({
            "id": format!("d-{proj}"), "project": proj, "functions": [],
            "created_at_ms": 0u64, "alias": ""
        }))
        .unwrap()]
    }

    #[test]
    fn deployments_ttl_carries_alive_drops_dead_respects_authoritative_empty() {
        use std::collections::{HashMap, HashSet};
        let mut prev: HashMap<String, Vec<fluid_core::DeploymentInfo>> = HashMap::new();
        prev.insert("fc-virginia".into(), dep("shoomoo"));
        prev.insert("dead-node".into(), dep("ghost"));

        // Round where we reached NO peer: alive node survives, aged-out node drops.
        let alive: HashSet<String> = ["fc-virginia".to_string()].into_iter().collect();
        let merged = merge_deployments_ttl(&prev, HashMap::new(), &alive);
        assert!(merged.contains_key("fc-virginia"), "alive-but-unreached node carried forward");
        assert!(!merged.contains_key("dead-node"), "node gone from registry is dropped");

        // Reached this round with an empty list → authoritative (node truly has none now).
        let mut fresh: HashMap<String, Vec<fluid_core::DeploymentInfo>> = HashMap::new();
        fresh.insert("fc-virginia".into(), vec![]);
        let merged = merge_deployments_ttl(&prev, fresh, &alive);
        assert!(merged.get("fc-virginia").unwrap().is_empty(), "reached node's empty list wins");
    }

    #[test]
    fn allowed_suffixes_route_foreign_roots_rejected() {
        let s = suffixes();
        // Legit wildcard ingress hosts.
        assert!(host_has_allowed_suffix("myapp.deployment.shadow.ngrok.pizza", &s));
        assert!(host_has_allowed_suffix("dpl-abc.deployment.shadow.ngrok.pizza:443", &s));
        assert!(host_has_allowed_suffix("myapp.localhost", &s));
        assert!(host_has_allowed_suffix("myapp.localhost:8787", &s));
        // Bare / empty / direct hosts are allowed (no foreign root to spoof).
        assert!(host_has_allowed_suffix("foobar", &s));
        assert!(host_has_allowed_suffix("", &s));
        // Foreign roots are rejected even if the first label collides with an alias.
        assert!(!host_has_allowed_suffix("myapp.evil.com", &s));
        assert!(!host_has_allowed_suffix("foobar.evil.com", &s));
        assert!(!host_has_allowed_suffix("deployment.shadow.ngrok.pizza.evil.com", &s));
    }

    #[test]
    fn route_ttl_merge_survives_transient_miss_but_drops_stale() {
        use super::{merge_routes_ttl, PeerRoute};
        use std::collections::{HashMap, HashSet};
        let mk = |node: &str, seen: u64| PeerRoute {
            node_id: node.into(), region: "r".into(), gateway: format!("http://{node}"),
            latency_ms: 1, healthy: true, last_seen_ms: seen,
        };
        let now = 1_000_000u64;
        let ttl = 30_000u64;
        // prev: host "app" served by peer-X (seen recently) and peer-Y (seen long ago).
        let mut prev: HashMap<String, Vec<PeerRoute>> = HashMap::new();
        prev.insert("app".into(), vec![mk("peer-x", now - 5_000), mk("peer-y", now - 90_000)]);

        // This round we reached only peer-z (serves "app"); X and Y were NOT reached.
        let mut fresh: HashMap<String, Vec<PeerRoute>> = HashMap::new();
        fresh.insert("app".into(), vec![mk("peer-z", now)]);
        let seen: HashSet<String> = ["peer-z".to_string()].into_iter().collect();

        let merged = merge_routes_ttl(&prev, fresh, &seen, now, ttl);
        let nodes: HashSet<&str> = merged["app"].iter().map(|r| r.node_id.as_str()).collect();
        assert!(nodes.contains("peer-z"), "freshly-gossiped route present");
        assert!(nodes.contains("peer-x"), "transient-miss peer kept within TTL");
        assert!(!nodes.contains("peer-y"), "stale (>TTL) peer dropped");

        // If peer-x IS reached this round but no longer serves "app", it must drop
        // (reached peer is authoritative) — fresh has no app route for it.
        let mut fresh2: HashMap<String, Vec<PeerRoute>> = HashMap::new();
        let seen2: HashSet<String> = ["peer-x".to_string()].into_iter().collect();
        let merged2 = merge_routes_ttl(&prev, fresh2.drain().collect(), &seen2, now, ttl);
        let nodes2: HashSet<&str> = merged2.get("app").map(|v| v.iter().map(|r| r.node_id.as_str()).collect()).unwrap_or_default();
        assert!(!nodes2.contains("peer-x"), "reached peer that dropped the deployment ages out immediately");
    }

    #[test]
    fn control_plane_degradation_ttl() {
        let ttl = 30_000u64;
        let now = 1_000_000u64;
        // Standalone node (no peers) is never degraded, even with no gossip.
        assert!(!cp_degraded(0, 0, now, ttl));
        assert!(!cp_degraded(now - 999_999, 0, now, ttl));
        // Peers configured but never synced => degraded.
        assert!(cp_degraded(0, 3, now, ttl));
        // Fresh sync within TTL => healthy.
        assert!(!cp_degraded(now - 5_000, 3, now, ttl));
        // Exactly at TTL boundary is still healthy (strictly-greater is degraded).
        assert!(!cp_degraded(now - ttl, 3, now, ttl));
        // Stale beyond TTL => degraded.
        assert!(cp_degraded(now - (ttl + 1), 3, now, ttl));
        // Clock skew (last_ok in the future) saturates to 0 elapsed => healthy.
        assert!(!cp_degraded(now + 10_000, 3, now, ttl));
    }
}
