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
    /// The exact deployment the request host ALIASED to at record time (empty
    /// when the host resolved to no deployment on this node) — what scopes a
    /// deployment-detail log view to that deployment's own traffic, including
    /// historically (the production alias moves between deployments on promote;
    /// this field pins each event to the deployment that actually held the
    /// alias when the request landed).
    #[serde(default)]
    pub deployment: String,
    /// Correlation id for tracing a request across nodes (accepted from
    /// `x-hive-request-id` or generated at ingress; forwarded over the mesh).
    #[serde(default)]
    pub request_id: String,
}

pub struct CloudState {
    pub region: String,
    pub node_name: String,
    /// epoch-ms this process started — the reference point `mesh_health`'s
    /// `uptime_ms` is computed from.
    pub boot_ms: u64,
    pub public_base: String,
    /// Host suffixes this node will route on (the pooled wildcard ingress roots,
    /// e.g. `deployment.shadow.ngrok.pizza`). A multi-label Host that doesn't end
    /// with one is a foreign root and is rejected before routing — so a foreign
    /// domain whose first label collides with a real alias can't be served.
    /// Configured via `HIVE_DEPLOY_SUFFIXES` (comma-separated).
    pub deploy_suffixes: Vec<String>,
    /// The user-deployment wildcard domain (`*.{apps_domain}` serves every
    /// deployment alias, one label only). `HIVE_APPS_DOMAIN`, default `shadw.app`.
    /// Deliberately a SEPARATE registrable domain from the platform domain so user
    /// content can never set cookies on / shadow the control plane.
    pub apps_domain: String,
    /// The platform/control-plane domain (`api.{platform_domain}` = admin API).
    /// `HIVE_PLATFORM_DOMAIN`, default `shadw.cloud`.
    pub platform_domain: String,
    /// The per-tenant DATABASE wildcard domain (`*.{db_domain}` gives every tenant
    /// database an external, TLS-SNI-routed endpoint — cross-protocol: Postgres
    /// wire, Redis wire, HTTP REST). `HIVE_DB_DOMAIN` (e.g. `downstash.xyz`); EMPTY
    /// disables the DB gateway entirely (no wildcard DNS/cert, no proxy listeners).
    pub db_domain: String,
    /// Public ingress mode: `ngrok` (today), `dual` (both paths live), `dns`
    /// (direct DNS + self-terminated TLS; ngrok retired). `HIVE_INGRESS`.
    pub ingress: String,

    pub waf: Arc<Waf>,
    pub bot: Arc<BotManager>,
    pub bot_policy: RwLock<BotPolicy>,
    pub cdn: Arc<CdnCache>,
    /// Regional runtime/data cache for tenant functions (Vercel Runtime Cache).
    pub runtime_cache: Arc<RuntimeCache>,
    pub limiter: Arc<ConcurrencyLimiter>,
    /// Per-DEPLOYMENT concurrency admission (sliding-window burst budget keyed by
    /// target host), so one busy deployment can only exhaust its OWN budget and
    /// cannot 503 every other tenant on the node. Checked before the node-wide
    /// `limiter` backstop (which now trips only under genuine total overload).
    pub admission: Arc<RateLimiter>,
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
    /// Subnet → location cache backing the authoritative DNS's client-proximity
    /// answers (see [`crate::dns_geo`]). Lives here rather than as a static so
    /// it shares the platform's HTTP client and dies with the state.
    pub dns_geo: Arc<crate::dns_geo::GeoCache>,
    /// What THIS node has directly PROVEN about its peers' nameservers by
    /// querying their public `:53` from here (see [`crate::dns_probe`]).
    /// Deliberately node-local — it IS this node's vantage, and the whole
    /// mechanism depends on vantages being independent. Only the derived
    /// attestation list is gossiped (`NodeInfo::dns_attest`).
    pub dns_probes: Arc<crate::dns_probe::NsProbes>,
    /// Pending ACME DNS-01 challenge TXT values for zones Seer itself answers
    /// (the deploy zone, `api.{platform}`) — written on the leader at issuance,
    /// replicated to followers via `store_sync` so ANY advertised nameserver
    /// node can answer Let's Encrypt's TXT query. See [`crate::acme::AcmeChallengeStore`].
    pub acme_challenges: crate::acme::AcmeChallengeStore,
    /// Hive's own native Queue backend for the Vercel WDK `World` interface
    /// (managed-world service) -- no external queue dependency.
    pub world_queue: Arc<crate::world_queue::WorldQueue>,
    pub projects: crate::project_settings::ProjectStore,
    pub builds: crate::git::BuildStore,
    pub cluster: Arc<crate::cluster::Cluster>,
    pub teams: crate::teams::TeamStore,
    pub gitops: crate::gitops::GitOpsStore,
    /// Reverse index (repo -> connected project names) accelerating
    /// `admin::git_webhook`'s per-delivery project match to O(1). See
    /// `gitops::GitRepoIndex` for what keeps it in sync.
    pub git_index: crate::gitops::GitRepoIndex,
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
    /// Per-project last-seen tracked-branch HEAD SHA for the webhook-less git
    /// auto-deploy poller (`git::spawn_git_poll_reconcile`). Keyed by project.
    /// Seeded from the deployed commit on first sight so an already-current
    /// project never spuriously redeploys on boot; a value change is a real push.
    /// Purely in-memory — after a leader restart/failover it re-baselines from
    /// the deployment record, so it never double-deploys an already-built HEAD.
    pub git_poll_seen: RwLock<std::collections::HashMap<String, String>>,
    /// This node's iroh P2P endpoint (real QUIC mesh transport), if bound. Used to
    /// dial peers and tunnel cross-node requests over QUIC (with HTTP fallback).
    pub iroh: RwLock<Option<hive_p2p::Endpoint>>,
    /// Pooled cross-node mesh transport: one persistent iroh QUIC connection per
    /// peer, a NEW stream per request (no per-request handshake). Set when `iroh`
    /// binds; `None` = P2P disabled (HTTP mesh still routes). See [`hive_p2p::PeerPool`].
    pub mesh: RwLock<Option<Arc<hive_p2p::PeerPool>>>,
    /// Live relay-set tracker for the bound `iroh` endpoint (dynamic-relay-list):
    /// diffs [own relay_url + healthy peers' relay_url + the central backstop]
    /// against what's actually applied via `Endpoint::insert_relay`/`remove_relay`
    /// on a fixed interval — see `spawn_relay_sync_loop` in main.rs. Set when
    /// `iroh` binds (alongside `mesh`); `None` = P2P disabled, nothing to sync.
    pub relay_set: RwLock<Option<Arc<hive_p2p::RelaySet>>>,
    /// Single-owner placement leases for stateful CONTAINER deployments (fenced,
    /// consensus-free). See `lease.rs`.
    pub leases: crate::lease::LeaseStore,
    /// Which nodes hold each container deployment (gossiped), so lease election
    /// (rendezvous hashing) only considers nodes that can actually run it.
    pub container_holders: RwLock<std::collections::HashMap<String, Vec<String>>>,
    pub webhooks: Arc<crate::webhooks::WebhookStore>,
    pub databases: Arc<crate::databases::DatabaseStore>,
    /// Managed-inference runtime (this node's own llama-server children +
    /// endpoint statuses) — see `inference::spawn_reconcile`.
    pub inference: crate::inference::InferenceRuntime,
    pub metrics: crate::metrics::MetricsStore,
    /// Short-TTL cache for expensive fleet-fan-out reads — see `resp_cache`'s
    /// module doc for why this exists (client-side caching alone doesn't
    /// de-dupe across tabs/users hitting the same tenant's expensive view).
    pub resp_cache: crate::resp_cache::ResponseCache,
    pub incidents: crate::incidents::IncidentStore,
    pub securelinks: crate::securelink::SecureLinkStore,
    pub apikeys: crate::apikeys::ApiKeyStore,
    pub integrations: crate::integrations::IntegrationStore,
    pub svcgraph: crate::svcgraph::ServiceGraphStore,
    pub identity: crate::identity::IdentityStore,
    pub domains: crate::dns::DomainStore,
    pub docs: crate::docstore::DocStore,
    pub billing: crate::billing::BillingStore,
    pub audit: crate::audit::AuditLog,
    pub notifications: crate::notifications::NotificationStore,
    /// Web-push subscriptions + SMS targets + delivery watermarks (see
    /// [`crate::push`]) — leader-synced, mutations leader-gated.
    pub push: crate::push::PushStore,
    /// Enterprise feature suite (IP blocking, SIEM, SAML, SCIM, deployment
    /// protection, microfrontends, conformance). See [`crate::enterprise`].
    pub enterprise: Arc<crate::enterprise::EnterpriseStore>,
    /// Platform-native Sandboxes (isolated on-demand Linux environments) — real
    /// Firecracker microVMs when this node's isolation backend supports it,
    /// honestly reporting "simulated" on mock/dev nodes otherwise (never a
    /// silent downgrade to a different, undisclosed isolation technology).
    pub sandboxes: Arc<crate::sandboxes_platform::PlatformSandboxProvider>,
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

/// Apps-domain host rule (`*.{apps_domain}`): the apex is allowed (landing/
/// redirect) and a subdomain is allowed ONLY when it is exactly ONE label —
/// Vercel-DNS wildcards match a single label, and every gateway alias is a
/// single dash-flattened label. Case-insensitive; `:port` stripped. Pure for
/// unit tests.
pub fn host_matches_apps_domain(host: &str, apps_domain: &str) -> bool {
    if apps_domain.is_empty() {
        return false;
    }
    let h = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let d = apps_domain.trim().trim_start_matches('.').to_ascii_lowercase();
    if h == d {
        return true; // apex
    }
    match h.strip_suffix(&format!(".{d}")) {
        Some(label) => !label.is_empty() && !label.contains('.'),
        None => false,
    }
}

#[cfg(test)]
mod apps_domain_tests {
    use super::*;

    #[test]
    fn apps_domain_hosts_one_label_only() {
        // apex + one-label subdomain allowed; deeper labels rejected (Vercel-DNS
        // wildcard matches ONE label); case-insensitive; :port stripped.
        assert!(host_matches_apps_domain("shadw.app", "shadw.app"));
        assert!(host_matches_apps_domain("myapp.shadw.app", "shadw.app"));
        assert!(host_matches_apps_domain("MyApp.Shadw.App:443", "shadw.app"));
        assert!(host_matches_apps_domain("my-app-git-main-team.shadw.app", "shadw.app"));
        assert!(!host_matches_apps_domain("a.b.shadw.app", "shadw.app"));
        assert!(!host_matches_apps_domain(".shadw.app", "shadw.app"));
        assert!(!host_matches_apps_domain("shadw.app.evil.com", "shadw.app"));
        assert!(!host_matches_apps_domain("notshadw.app", "shadw.app"));
        // user apps must NEVER be served from the platform domain
        assert!(!host_matches_apps_domain("app.shadw.cloud", "shadw.app"));
        assert!(!host_matches_apps_domain("anything", ""));
    }
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

/// Live mesh-membership health, as reported by `CloudState::mesh_health`.
#[derive(Clone, Serialize)]
pub struct MeshHealth {
    /// Peers this node is CONFIGURED to expect (from `HIVE_TRUSTED_NODE_IDS`,
    /// excluding self) — static ground truth, independent of how this node was
    /// launched.
    pub expected_peers: usize,
    /// Of those, how many are visible + healthy in the LIVE gossip-derived
    /// registry right now.
    pub visible_healthy_peers: usize,
    /// True iff peers were expected but NONE are currently visible — the exact
    /// shape of the node-a/node-b isolation incident (see `mesh_isolated`).
    pub isolated: bool,
    /// ms since this process started. Turns "sees only self" from an opaque
    /// log line into an answerable question: still within the normal
    /// convergence window, or has it been stuck? Measured live this session:
    /// a healthy restart converged (first successful DNS reconcile) in ~58s;
    /// a node stuck past several minutes of this counter climbing while
    /// `visible_healthy_peers` stays 0 is the genuinely-stuck signal the old
    /// silent skip-log gave no way to distinguish from "still warming up".
    pub uptime_ms: u64,
}

/// Pure core of mesh-isolation detection (unit-testable without a node). Compares
/// the LIVE, gossip-derived set of healthy peer identities actually visible right
/// now against the EXPECTED trusted peer set, rather than this node's own
/// self-reported `--peer` CLI list.
///
/// `cp_degraded` above is structurally blind to a specific failure class: a node
/// launched with zero `--peer` args (peers "announce themselves" to it instead,
/// per `dev-cluster.sh`'s own comment) has `peers.len() == 0` by construction, so
/// `cp_degraded` reports "not degraded" — "standalone node — no control plane to
/// be degraded" — even when that node is fully isolated from an 8-node fleet it
/// was supposed to join. This happened: an orphaned dev launch script started
/// node-a this way, gossip-signature enforcement silently rejected its unsigned
/// packets on every OTHER node (logged only on the REJECTING side), and nothing
/// ever surfaced "node-a can see zero of its expected fleet."
///
/// `expected_peers` here comes from static config (`HIVE_TRUSTED_NODE_IDS`,
/// present regardless of how the node was launched) instead of the dynamic peer
/// list, so this closes the blind spot: a node with peers configured that
/// currently sees none of them is isolated, full stop.
pub fn mesh_isolated(expected_peers: usize, visible_healthy_peers: usize) -> bool {
    expected_peers > 0 && visible_healthy_peers == 0
}

impl CloudState {
    /// The current control-plane WRITE authority (owner). Resolution order (see
    /// `cluster.rs` module doc): the operator-curated `HIVE_CP_OWNER_CHAIN`
    /// (first healthy+public entry wins — static ownership, no open election),
    /// falling back to the identity election (with the legacy `HIVE_CP_LEADER`
    /// pin) only when no chain is configured or every chain entry is dark.
    /// Returns the owner's node name, or this node when nothing is resolvable
    /// (single-node / no peers). Every resolution feeds the cluster's
    /// observed-owner tracker so the fencing epoch bumps exactly on real
    /// ownership transitions.
    pub fn control_plane_leader(&self) -> String {
        let chain = crate::cluster::Cluster::owner_chain_from_env();
        let pref = std::env::var("HIVE_CP_LEADER").ok();
        let owner =
            crate::cluster::Cluster::control_plane_owner(&chain, pref.as_deref(), &self.registry.nodes())
                .unwrap_or_else(|| self.node_name.clone());
        self.cluster.observe_owner(&owner);
        owner
    }

    /// True when THIS node is the control-plane leader.
    ///
    /// Gated on mesh freshness first: `billing_leader` recomputes the election
    /// fresh from `self.registry.nodes()` on EVERY call, with no persisted or
    /// gossiped epoch/term — there is nothing that fences a stale computation
    /// against a fresher one elsewhere in the mesh. That's fine for a node with
    /// current gossip data (the common case: it converges within one ~5s
    /// round). It's dangerous for a node whose OWN view is stale or isolated —
    /// most concretely, a node in the first moments after a restart, before its
    /// gossip loop has resynced. Such a node's `registry.nodes()` can still
    /// show itself as the (or a) healthy lowest-identity candidate purely
    /// because it hasn't yet learned the rest of the mesh already elected
    /// someone else — a real, if short (bounded by one gossip round), split-
    /// brain window on every leader restart. A node that can't currently see
    /// its expected peers (`mesh_health().isolated`) must never assert
    /// leadership from that view; `admin_ingress` already fails mutations
    /// closed (503) when the resolved leader is unreachable, so refusing here
    /// safely defers rather than risking a stale-data write.
    pub fn is_control_plane_leader(&self) -> bool {
        if self.mesh_health().isolated {
            return false;
        }
        self.control_plane_leader() == self.node_name
    }

    /// The leader's [`NodeInfo`] (for its public IP), or None when this node is the
    /// leader or the leader isn't resolvable in the registry.
    pub fn leader_node(&self) -> Option<hive_edge::NodeInfo> {
        let leader = self.control_plane_leader();
        if leader == self.node_name {
            return None;
        }
        self.registry.nodes().into_iter().find(|n| n.name == leader)
    }

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
        firecracker: Option<Arc<hive_backend::firecracker::FirecrackerBackend>>,
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
                // ngrok is fully retired: the apps domain (handled separately
                // in host_allowed) is the only public root; keep localhost for
                // local dev. Extra roots come only from HIVE_DEPLOY_SUFFIXES.
                vec!["localhost".into()]
            });
        // Real-DNS ingress config (ngrok retirement): the two-domain split is
        // deliberate (user content on apps_domain can never touch the platform
        // domain). `HIVE_INGRESS` stages the cutover: ngrok -> dual -> dns.
        let apps_domain = std::env::var("HIVE_APPS_DOMAIN")
            .ok()
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "shadw.app".into());
        let platform_domain = std::env::var("HIVE_PLATFORM_DOMAIN")
            .ok()
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "shadw.cloud".into());
        // Per-tenant DB gateway domain — EMPTY (unset) = gateway disabled.
        let db_domain = std::env::var("HIVE_DB_DOMAIN")
            .ok()
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let ingress = std::env::var("HIVE_INGRESS")
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| matches!(s.as_str(), "ngrok" | "dual" | "dns"))
            // ngrok is retired fleet-wide: real DNS ingress is the default.
            // ("ngrok"/"dual" remain accepted as explicit break-glass values.)
            .unwrap_or_else(|| "dns".into());
        let teams = crate::teams::TeamStore::new();
        teams.ensure_seed(&owner_email);
        let region_for_sandboxes = region.clone();
        Arc::new(CloudState {
            region,
            node_name,
            boot_ms: hive_core::now_ms(),
            public_base,
            deploy_suffixes,
            apps_domain,
            platform_domain,
            db_domain,
            ingress,
            waf,
            bot,
            bot_policy: RwLock::new(BotPolicy::default()),
            cdn,
            runtime_cache: Arc::new(RuntimeCache::new()),
            limiter,
            // Per-deployment burst budget (host-keyed). Default: 1000 admissions
            // / 10s per deployment — generous for real traffic, isolating any
            // single deployment's surge from every other tenant on the node.
            admission: Arc::new(RateLimiter::new(
                std::env::var("HIVE_DEPLOY_BURST").ok().and_then(|v| v.parse().ok()).unwrap_or(1000),
                10_000,
            )),
            ratelimit: Arc::new(RateLimiter::new(100, 10_000)),
            router,
            registry,
            cron,
            workflows,
            gw,
            fluid,
            hive,
            http: reqwest::Client::new(),
            dns_geo: crate::dns_geo::GeoCache::spawn(reqwest::Client::new()),
            dns_probes: Arc::new(crate::dns_probe::NsProbes::new()),
            acme_challenges: crate::acme::AcmeChallengeStore::new(),
            world_queue: crate::world_queue::WorldQueue::new(),
            projects: crate::project_settings::ProjectStore::new(),
            builds: crate::git::BuildStore::new(),
            cluster,
            teams,
            gitops: crate::gitops::GitOpsStore::new(),
            git_index: crate::gitops::GitRepoIndex::new(),
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
            git_poll_seen: RwLock::new(std::collections::HashMap::new()),
            iroh: RwLock::new(None),
            mesh: RwLock::new(None),
            relay_set: RwLock::new(None),
            leases: crate::lease::LeaseStore::new(),
            container_holders: RwLock::new(std::collections::HashMap::new()),
            webhooks: Arc::new(crate::webhooks::WebhookStore::new()),
            databases: Arc::new(crate::databases::DatabaseStore::new()),
            inference: crate::inference::InferenceRuntime::default(),
            metrics: crate::metrics::MetricsStore::new(),
            resp_cache: crate::resp_cache::ResponseCache::new(),
            incidents: crate::incidents::IncidentStore::new(),
            securelinks: crate::securelink::SecureLinkStore::new(),
            apikeys: crate::apikeys::ApiKeyStore::new(),
            integrations: crate::integrations::IntegrationStore::new(),
            svcgraph: crate::svcgraph::ServiceGraphStore::new(),
            identity: crate::identity::IdentityStore::new(),
            domains: crate::dns::DomainStore::new(),
            docs: crate::docstore::DocStore::new(),
            billing: crate::billing::BillingStore::new(),
            audit: crate::audit::AuditLog::new(crate::persist::data_dir().join("audit.jsonl")),
            notifications: crate::notifications::NotificationStore::new(),
            push: crate::push::PushStore::new(),
            enterprise: Arc::new(crate::enterprise::EnterpriseStore::new()),
            sandboxes: Arc::new(crate::sandboxes_platform::PlatformSandboxProvider::new(region_for_sandboxes, firecracker)),
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

    /// Live mesh-membership health (see `mesh_isolated`'s doc for why this exists
    /// alongside `control_plane_degraded`, which a zero-`--peer` launch fools).
    pub fn mesh_health(&self) -> MeshHealth {
        let self_pid = self.registry.me().peer_id;
        let expected: std::collections::HashSet<String> = self
            .trusted_peer_ids
            .read()
            .map(|g| g.iter().filter(|id| Some((*id).as_str()) != self_pid.as_deref()).cloned().collect())
            .unwrap_or_default();
        let visible: std::collections::HashSet<String> = self
            .registry
            .nodes()
            .into_iter()
            .filter(|n| !n.is_self && n.healthy)
            .filter_map(|n| n.peer_id)
            .filter(|pid| expected.contains(pid))
            .collect();
        MeshHealth {
            expected_peers: expected.len(),
            visible_healthy_peers: visible.len(),
            isolated: mesh_isolated(expected.len(), visible.len()),
            uptime_ms: hive_core::now_ms().saturating_sub(self.boot_ms),
        }
    }

    pub fn record(&self, ev: Event) {
        *self.req_count.lock() += 1;
        if ev.action == "waf-deny" || ev.action == "bot-block" {
            *self.blocked_count.lock() += 1;
        }
        // Attribute the event to the OWNING TENANT (the team that owns the project),
        // so per-tenant metrics reads can't leak across tenants. Projectless events
        // (host-rejected, platform-internal) go to the system tenant.
        let mtenant = if ev.project.is_empty() {
            crate::metrics::SYSTEM_TENANT.to_string()
        } else {
            crate::admin::norm(&self.projects.team_of(&ev.project)).to_string()
        };
        self.metrics.record(&ev, &mtenant);
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
    /// host (local/direct/default deployment) is allowed; under real-DNS ingress
    /// the apps domain (`*.{HIVE_APPS_DOMAIN}`, one label + apex) is allowed; any
    /// other multi-label host must end with a configured deploy suffix (the ngrok
    /// zones — kept while `HIVE_INGRESS != dns`), else it's a foreign root.
    pub fn host_allowed(&self, host: &str) -> bool {
        // The apps-domain rule only applies under real-DNS ingress (same gate as
        // `deploy_url`). In ngrok mode `*.{apps_domain}` never reaches this node, so
        // keep host admission byte-identical to the pre-apps-domain behavior.
        (self.ingress != "ngrok" && host_matches_apps_domain(host, &self.apps_domain))
            || host_has_allowed_suffix(host, &self.deploy_suffixes)
            // A CUSTOM DOMAIN attached to a project (`POST /v1/projects/:p/domains`)
            // was rejected here as "a foreign root" 100% of the time — this gate
            // predates that feature and was never taught about it. Two admission
            // paths, matching the two ways this node can actually serve a host:
            // 1. `serves_host` — this node itself owns the deployment (its OWN
            //    `c.gw.aliases` has the entry directly).
            // 2. `peer_routes` — the MESH GOSSIP already knows some OTHER node
            //    owns it (the exact mechanism that already makes every node
            //    admit `*.{apps_domain}` hosts it doesn't own locally and
            //    mesh-proxy them to the real owner — apps_domain gets a
            //    blanket admission above, but a customer's OWN custom domain
            //    correctly does NOT, so it needs this per-alias existence
            //    check instead). Once the real owner's periodic `serve_hosts`
            //    publish carries a freshly-attached domain (see
            //    `project_domain_add`, which now applies the alias ON the
            //    owning node instead of wherever the admin write happened to
            //    land), every OTHER node's `peer_routes` picks it up on the
            //    next gossip cycle and can admit + mesh-proxy it too.
            || self.gw.serves_host(host)
            || {
                let sub = host.split(':').next().unwrap_or(host).split('.').next().unwrap_or(host);
                self.peer_routes.read().contains_key(sub)
            }
    }

    /// Public URL for a deployment alias. Real-DNS ingress (`HIVE_INGRESS !=
    /// ngrok`) emits `https://<label>.{apps_domain}` (single-label wildcard);
    /// ngrok mode keeps today's `https://<alias>` (the UI session-maps domains).
    pub fn deploy_url(&self, alias: &str) -> String {
        if self.ingress != "ngrok" {
            let sub = alias.split('.').next().unwrap_or(alias);
            format!("https://{}.{}", sub, self.apps_domain)
        } else {
            format!("https://{alias}")
        }
    }

    /// Public base URL of the platform API (`https://api.{platform_domain}` under
    /// real-DNS ingress; the internal gateway base otherwise).
    pub fn api_base(&self) -> String {
        if self.ingress != "ngrok" {
            format!("https://api.{}", self.platform_domain)
        } else {
            self.public_base.clone()
        }
    }

    pub fn event(&self, region: &str, method: &str, host: &str, path: &str, status: u16, action: &str, detail: &str) -> Event {
        // EXACT attribution only (no default-deployment fallback, no detail
        // fallback): an event belongs to a project/deployment iff the request
        // host's subdomain actually ALIASES one of this node's deployments.
        // The old `project_for_host` path used the serving resolution, whose
        // default-deployment fallback stamped every unmatched host — platform
        // bot probes, other tenants' DB hosts, peer-hosted projects routed
        // through this node — with this node's default project, leaking foreign
        // request lines into that project's log view and corrupting its tenant's
        // metrics attribution. Unresolved hosts stay UNATTRIBUTED (empty
        // project/deployment) and are operator-only in the logs API.
        let (deployment, project) = self.gw.attribution_for_host(host).unwrap_or_default();
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
            deployment,
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

    #[test]
    fn mesh_isolated_closes_the_zero_peer_cli_blind_spot() {
        // The exact node-a shape: no --peer CLI args at all, so `cp_degraded`
        // (peer_count from self.peers) would report "not degraded" — but
        // `mesh_isolated` uses the static trusted-peer-id EXPECTATION instead,
        // so it correctly flags isolation even when cp_degraded is blind to it.
        assert!(super::mesh_isolated(7, 0), "peers expected, none visible = isolated");
        // Standalone-by-design (genuinely zero trusted peers configured) is never
        // isolated — nothing was expected.
        assert!(!super::mesh_isolated(0, 0));
        // Seeing at least one expected peer, even far short of the full set
        // (a mesh mid-reconverge), is not isolation.
        assert!(!super::mesh_isolated(7, 1));
        assert!(!super::mesh_isolated(7, 7));
    }
}
