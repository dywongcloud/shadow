//! Shared state for a hive-cloud node: every platform subsystem in one place.

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
use serde::{Deserialize, Serialize};

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
    /// Epoch-ms this route was last refreshed by gossip: a route from a momentarily unreachable but
    /// healthy peer survives a transient gossip miss for [`ROUTE_TTL_MS`] instead of being dropped
    /// instantly (which would 404 the deployment).
    #[serde(default)]
    pub last_seen_ms: u64,
}

/// TTL for gossiped routing mappings: a route survives this long without a refresh. It matches the
/// node-registry staleness window, so the routing table and the node roster age out consistently.
pub const ROUTE_TTL_MS: u64 = 30_000;

/// Merges freshly gossiped routes with the previous table, keeping a route from a peer NOT reached
/// this round only while it is within the TTL. A peer that WAS reached this round is authoritative:
/// its fresh list replaces its old routes, so a genuinely removed deployment still ages out
/// immediately and a route it dropped is never resurrected. Pure and testable.
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

/// Merges the freshly gossiped fleet deployment map with the previous one (keyed by node name), so
/// a node's deployments do NOT vanish on a single missed `/v1/fleet-deployments` fetch: without it
/// a transient gossip miss to a peer wiped its projects from the coordinator, and the dashboard's
/// workflows/runs tables, the deployments list and the per-project proxy all intermittently went
/// empty. A node present in `fresh` is authoritative (we reached it this round: even an empty list
/// means "it genuinely has none now"); a node only in `prev` is carried forward IFF it is still
/// `alive` (present in the registry), and once it ages out of the registry (real staleness, see
/// `NodeRegistry::nodes`) its deployments drop too.
pub fn merge_deployments_ttl(
    prev: &std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>>,
    fresh: std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>>,
    alive: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>> {
    let mut merged = fresh;
    for (node, deps) in prev {
        if merged.contains_key(node) {
            continue;
        }
        if !alive.contains(node) {
            continue;
        }
        merged.insert(node.clone(), deps.clone());
    }
    merged
}

/// One observed request/event, for the dashboard's live log and analytics. It is also the unit of
/// the DURABLE history (`crate::event_log`): this struct is appended to the day-rotated JSONL log,
/// so it round-trips through JSON, and new fields MUST keep `#[serde(default)]` because older
/// day-files on disk have fewer keys and must still deserialize.
#[derive(Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts_ms: u64,
    pub region: String,
    pub method: String,
    pub host: String,
    pub path: String,
    pub status: u16,
    /// allow | waf-deny | bot-block | cache-hit | cron | ...
    pub action: String,
    pub detail: String,
    #[serde(default)]
    pub project: String,
    /// The exact deployment the request host ALIASED to at record time (empty when the host
    /// resolved to no deployment on this node): what scopes a deployment-detail log view to that
    /// deployment's own traffic, including historically (the production alias moves between
    /// deployments on promote, and this field pins each event to the deployment that actually held
    /// the alias when the request landed).
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
    /// Host suffixes this node will route on (the pooled wildcard ingress roots, e.g.
    /// `deployment.shadow.ngrok.pizza`), from `HIVE_DEPLOY_SUFFIXES` (comma-separated; default
    /// `localhost` only, since ngrok is retired and the apps domain is admitted separately by
    /// `host_allowed`). A multi-label Host that does not end with one is a foreign root and is
    /// rejected before routing, so a foreign domain whose first label collides with a real alias
    /// cannot be served.
    pub deploy_suffixes: Vec<String>,
    /// The user-deployment wildcard domain (`*.{apps_domain}` serves every deployment alias, one
    /// label only): `HIVE_APPS_DOMAIN`, default `shadw.app`. Deliberately a SEPARATE registrable
    /// domain from the platform domain, so user content can never set cookies on or shadow the
    /// control plane.
    pub apps_domain: String,
    /// The platform/control-plane domain (`api.{platform_domain}` = admin API).
    /// `HIVE_PLATFORM_DOMAIN`, default `shadw.cloud`.
    pub platform_domain: String,
    /// The per-tenant DATABASE wildcard domain (`*.{db_domain}` gives every tenant database an
    /// external, TLS-SNI-routed endpoint: Postgres wire, Redis wire, HTTP REST). `HIVE_DB_DOMAIN`
    /// (e.g. `downstash.xyz`); EMPTY (unset) disables the DB gateway entirely (no wildcard
    /// DNS/cert, no proxy listeners).
    pub db_domain: String,
    /// Public ingress mode (`HIVE_INGRESS`): `dns` is the default (direct DNS with self-terminated
    /// TLS; ngrok is retired fleet-wide), while `ngrok` and `dual` (both paths live) remain
    /// accepted as explicit break-glass values. The `ngrok` -> `dual` -> `dns` cutover was staged
    /// through this setting.
    pub ingress: String,

    pub waf: Arc<Waf>,
    pub bot: Arc<BotManager>,
    pub bot_policy: RwLock<BotPolicy>,
    pub cdn: Arc<CdnCache>,
    /// Regional runtime/data cache for tenant functions (Vercel Runtime Cache).
    pub runtime_cache: Arc<RuntimeCache>,
    pub limiter: Arc<ConcurrencyLimiter>,
    /// Per-DEPLOYMENT concurrency admission (sliding-window burst budget keyed by target host;
    /// `HIVE_DEPLOY_BURST`, default 1000 admissions per 10 s, generous for real traffic), so one
    /// busy deployment can only exhaust its OWN budget and cannot 503 every other tenant on the
    /// node. It is checked before the node-wide `limiter` backstop, which trips only under genuine
    /// total overload.
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
    /// Subnet-to-location cache backing the authoritative DNS's client-proximity answers (see
    /// [`crate::dns_geo`]). It lives here rather than as a static so it shares the platform's HTTP
    /// client and dies with the state.
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
    pub acme_http01: crate::acme::Http01Store,
    /// Hive's own native Queue backend for the Vercel WDK `World` interface
    /// (managed-world service) -- no external queue dependency.
    pub world_queue: Arc<crate::world_queue::WorldQueue>,
    pub projects: crate::project_settings::ProjectStore,
    /// Durable record of each project's current production host + source —
    /// survives the host node's death (unlike `peer_deployments`, which is
    /// dropped the instant its source node ages out). See
    /// `production_deployments.rs` module doc.
    pub production_deployments: crate::production_deployments::ProductionDeploymentStore,
    pub builds: crate::git::BuildStore,
    /// Per-build cancellation bookkeeping (live OS process group + mirror
    /// target + driving task) — see `git::BuildCancelRegistry`. Deliberately
    /// NOT persisted/gossiped: it only describes an in-flight build's live
    /// process, which does not survive a restart (and a restarted node
    /// already finalizes any Queued/Building record to `Error` on boot).
    pub build_cancels: crate::git::BuildCancelRegistry,
    /// Node-local write-ahead evidence for accepted deployments, production
    /// alias revisions, and retryable lifecycle delivery.
    pub deployment_ledger: Arc<crate::deployment_ledger::DeploymentLedger>,
    /// This node's own signing identity for the deployment integrity chain
    /// (`hive_core::integrity`) — per-node, never fleet-shared. See
    /// `integrity_signer.rs`'s module doc.
    pub integrity_signer: Arc<crate::integrity_signer::IntegritySigner>,
    /// Bounded, durable receiver for exact runtime-artifact packages. Every mutation is serialized
    /// by its owned worker and bound to the current project incarnation before it enters the queue.
    /// Its durable-persistence hook is bound in `new()` once the state Arc exists (the service is
    /// constructed before it): a remote generation Commit publishes a Ready record only when the
    /// SAME durable write the local deploy path performs succeeds, and the hook captures a Weak
    /// state, so it never pins the state in a cycle and fails closed if the state is ever gone.
    pub runtime_artifact_transfer: Arc<crate::runtime_artifact_transfer::TransferService>,
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
    /// Mutable P2P admission allowlist. Seeded from the canonical boot trust
    /// roster; proof-gated hot joins may add identities at runtime.
    pub trusted_peer_ids: hive_p2p::TrustSet,
    /// Boot-immutable active membership used by mesh liveness policy.
    pub expected_peer_ids: std::collections::HashSet<iroh::EndpointId>,
    /// Mixed-rollout bridge allowing relayed roster records to expand only the
    /// mutable admission allowlist. Never affects expected membership.
    pub relayed_trust_compat: bool,
    /// Gossip transport map: peer admin URL -> (node_id, iroh addr_json), learned
    /// from each roster exchange. Lets the gossip loop reach a peer over the iroh
    /// QUIC mesh (when `HIVE_GOSSIP_IROH` is set) instead of HTTP-over-SSH, once it
    /// knows the peer's iroh address. Empty until the first roster is read (which
    /// bootstraps over HTTP).
    pub peer_iroh: RwLock<std::collections::HashMap<String, (String, String)>>,
    /// Control-plane health: epoch-ms of the last SUCCESSFUL gossip sync from any peer (0 = never
    /// synced). The data plane keeps serving from local persisted state when this goes stale (peers
    /// unreachable); this just makes the degradation observable and lets routing fail closed for
    /// unknown deployments.
    pub last_gossip_ok_ms: std::sync::atomic::AtomicU64,
    /// Cross-node routing table: deployment subdomain -> peer nodes that serve it
    /// (learned via gossip). Lets any node route/load-balance requests to the node
    /// that actually hosts a deployment.
    pub peer_routes: RwLock<std::collections::HashMap<String, Vec<PeerRoute>>>,
    /// The same routing table indexed by OWNING NODE: `node -> (host -> route)`. This is the
    /// authoritative copy; `peer_routes` is a derived view rebuilt from it. It exists because route
    /// sync is incremental (a peer whose route set did not change answers with just its
    /// generation), so the gossip round cannot rebuild the table from scratch: a peer's previously
    /// published routes must survive an empty "unchanged" answer intact instead of being dropped
    /// for having sent nothing.
    pub peer_routes_by_node: RwLock<
        std::collections::HashMap<String, std::collections::HashMap<String, PeerRoute>>,
    >,
    /// Last route generation seen from each peer NODE (client side). Sent as
    /// `?since=` so an unchanged peer answers with a generation, not its list.
    pub route_gens: RwLock<std::collections::HashMap<String, u64>>,
    /// Which node id a gossip TARGET last answered for (so the next round can
    /// ask that node for a delta before its identity is known again).
    pub target_node: RwLock<std::collections::HashMap<String, String>>,
    /// This node's own route generation (server side): bumped whenever the set
    /// of hosts it serves changes. Never decreases.
    pub route_pub_gen: std::sync::atomic::AtomicU64,
    /// The host-set hash `route_pub_gen` was last bumped for.
    pub route_pub_hash: RwLock<u64>,
    /// Deployments hosted on each peer node (name -> its deployments), learned via gossip: lets the
    /// dashboard's per-project deployment list show deployments the placement scheduler placed on
    /// OTHER nodes, not just the ones this coordinator hosts locally.
    pub peer_deployments:
        RwLock<std::collections::HashMap<String, Vec<fluid_core::DeploymentInfo>>>,
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
    /// Separate `hive/browser/0` connection pool. ALPN is negotiated per QUIC
    /// connection, so browser invokes never share trusted fleet trunks.
    pub browser_mesh: RwLock<Option<Arc<hive_p2p::BrowserPool>>>,
    /// Leader-owned, short-lived browser serving grants. Browser endpoint ids
    /// stay here and never enter the trusted fleet registry or scheduler.
    pub browser_admissions: crate::browser_admission::BrowserAdmissionStore,
    /// Replicated, TTL-bound coarse presence for admitted browser peers (for
    /// the constellation's satellite markers) — separate from `NodeInfo` and
    /// from `browser_admissions`; never read by placement/scheduling/DNS.
    pub browser_presence: crate::browser_presence::BrowserPresenceStore,
    /// Live relay-set tracker for the bound `iroh` endpoint: diffs [own relay_url + healthy peers'
    /// relay_url + the central backstop] against what is applied through
    /// `Endpoint::insert_relay`/`remove_relay` on a fixed interval (see `spawn_relay_sync_loop` in
    /// main.rs). Set when `iroh` binds (alongside `mesh`); `None` = P2P disabled, nothing to sync.
    pub relay_set: RwLock<Option<Arc<hive_p2p::RelaySet>>>,
    /// Single-owner placement leases for stateful CONTAINER deployments (fenced,
    /// consensus-free). See `lease.rs`.
    pub leases: crate::lease::LeaseStore,
    /// Which nodes hold each container deployment (gossiped), so lease election
    /// (rendezvous hashing) only considers nodes that can actually run it.
    pub container_holders: RwLock<std::collections::HashMap<String, Vec<String>>>,
    pub webhooks: Arc<crate::webhooks::WebhookStore>,
    pub databases: Arc<crate::databases::DatabaseStore>,
    pub queues: Arc<crate::queues::QueueStore>,
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
    /// The CONCRETE Firecracker backend, when this node's isolation backend is
    /// real Firecracker (not mock) — `None` on a mock/dev node. Kept as its
    /// own field, a sibling clone of the Arc `sandboxes` also holds
    /// (constructed together in `new()`, never re-derived), because
    /// `storage_api`'s snapshot routes need Firecracker-specific methods
    /// (`locate_data_image`, `snapshot_dir`) that the generic `CellBackend`
    /// trait object `gw` was built with does not expose.
    pub firecracker: Option<Arc<hive_backend::firecracker::FirecrackerBackend>>,
    /// Platform owner identity (seeds the default team; ops dashboard owner).
    pub owner_email: String,
    /// Every email that mints a `platform_admin` token: `owner_email` plus `HIVE_ADMIN_EMAILS`,
    /// both comma-separated (`HIVE_OWNER_EMAIL` may itself be a list, so it is expanded here:
    /// pushing the unsplit string as one entry would equal nobody's address and silently drop
    /// `platform_admin` on every node configured with the list form). Lowercased at load and
    /// compared case-insensitively at mint (see `admin::mint_token`), so the set is order- and
    /// duplicate-independent. Distinct from `owner_email`, which stays a SINGLE address because it
    /// also seeds the default team and names the team creator: admin is a strictly wider set that
    /// grants the operator console and the browser public-node capability without touching team
    /// ownership.
    pub admin_emails: Vec<String>,

    /// Request-event history: a hot in-memory ring (the bounded default poll)
    /// plus a durable day-rotated JSONL log that is what makes a ranged
    /// `/v1/logs` query reach further back than the ring. See
    /// [`crate::event_log`].
    event_log: crate::event_log::EventLog,
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
    suffixes
        .iter()
        .any(|s| h == *s || h.ends_with(&format!(".{s}")))
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
    let d = apps_domain
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if h == d {
        return true;
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
        assert!(host_matches_apps_domain(
            "my-app-git-main-team.shadw.app",
            "shadw.app"
        ));
        assert!(!host_matches_apps_domain("a.b.shadw.app", "shadw.app"));
        assert!(!host_matches_apps_domain(".shadw.app", "shadw.app"));
        assert!(!host_matches_apps_domain("shadw.app.evil.com", "shadw.app"));
        assert!(!host_matches_apps_domain("notshadw.app", "shadw.app"));
        // user apps must NEVER be served from the platform domain
        assert!(!host_matches_apps_domain("app.shadw.cloud", "shadw.app"));
        assert!(!host_matches_apps_domain("anything", ""));
    }
}

/// Default TTL for control-plane degradation: if no peer gossip has succeeded within this window
/// (and peers are configured), the node reports degraded. The gossip loop runs on a ~5 s cadence,
/// so 30 s tolerates a few missed rounds.
pub const CP_DEGRADED_TTL_MS: u64 = 30_000;

/// Pure core of control-plane degradation. Degraded = peers are configured (so we EXPECT gossip)
/// but the last successful sync is older than the TTL (or never happened); a single-node deployment
/// (no peers) is never degraded. Unit-testable without a node.
pub fn cp_degraded(last_ok_ms: u64, peer_count: usize, now_ms: u64, ttl_ms: u64) -> bool {
    if peer_count == 0 {
        return false;
    }
    last_ok_ms == 0 || now_ms.saturating_sub(last_ok_ms) > ttl_ms
}

/// Live mesh-membership health, as reported by `CloudState::mesh_health`.
#[derive(Clone, Serialize)]
pub struct MeshHealth {
    /// Boot-configured active peers, excluding this node. A nonempty
    /// `HIVE_EXPECTED_NODE_IDS` supplies the roster; otherwise the validated
    /// boot `HIVE_TRUSTED_NODE_IDS` snapshot is used.
    pub expected_peers: usize,
    /// Of those, how many are visible + healthy in the LIVE gossip-derived
    /// registry right now.
    pub visible_healthy_peers: usize,
    /// Gossip-fresh expected peers regardless of health verdict — see
    /// `mesh_health` for why this exists (wedge vs outage discrimination).
    pub audible_peers: usize,
    /// Healthy expected peers this node can DIRECTLY exchange with (not observer-local cold).
    /// Gossip restoration keeps a wedged peer service-`healthy`, so `visible_healthy_peers` alone
    /// reports a transport-wedged node as converged: this is the field the never-converged boot
    /// wedge hides behind, and `isolated` derives from it, not from the restorable count.
    pub direct_reachable_peers: usize,
    /// True iff peers were expected but NONE are currently directly reachable (see
    /// `mesh_isolated`).
    pub isolated: bool,
    /// ms since this process started: turns "sees only self" from an opaque log line into an
    /// answerable question, still inside the normal convergence window (a healthy restart converged
    /// in about 58 s) or stuck (the counter climbing past several minutes while
    /// `visible_healthy_peers` stays 0).
    pub uptime_ms: u64,
}

/// Pure core of mesh-isolation detection (unit-testable without a node). It compares the LIVE,
/// gossip-derived set of healthy peer identities visible right now against the boot-immutable
/// expected active peer set, rather than this node's own self-reported `--peer` CLI list:
/// `cp_degraded` is structurally blind to a node launched with zero `--peer` args (peers announce
/// themselves to it instead), which has `peers.len() == 0` by construction and so reports "not
/// degraded" even when it is fully isolated from the fleet it was supposed to join
/// (gossip-signature enforcement rejected its unsigned packets on every OTHER node and logged only
/// on the REJECTING side, so nothing surfaced that it could see zero of its expected fleet).
///
/// `expected_peers` comes from `HIVE_EXPECTED_NODE_IDS` when nonempty, otherwise from an immutable
/// boot snapshot of `HIVE_TRUSTED_NODE_IDS`. It never follows runtime authorization or discovery
/// state, so a node that should have peers but currently sees none is isolated, full stop.
pub fn mesh_isolated(expected_peers: usize, visible_healthy_peers: usize) -> bool {
    expected_peers > 0 && visible_healthy_peers == 0
}

fn configured_endpoint_ids(
    name: &str,
) -> anyhow::Result<std::collections::HashSet<iroh::EndpointId>> {
    let raw = match std::env::var(name) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(std::collections::HashSet::new()),
        Err(error) => return Err(anyhow::anyhow!("{name} is not valid UTF-8: {error}")),
    };
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry.parse::<iroh::EndpointId>().map_err(|error| {
                anyhow::anyhow!("{name} contains invalid endpoint id {entry:?}: {error}")
            })
        })
        .collect()
}

fn configured_bool_default_true(name: &str) -> anyhow::Result<bool> {
    let raw = match std::env::var(name) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(true),
        Err(error) => return Err(anyhow::anyhow!("{name} is not valid UTF-8: {error}")),
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow::anyhow!(
            "{name} must be one of 1,true,yes,on,0,false,no,off"
        )),
    }
}

impl CloudState {
    /// The control-plane owner as a FORWARDING target, plus where the answer came from (see
    /// `Cluster::control_plane_owner_with_source`). With `HIVE_CP_OWNER_CHAIN` set: the strict
    /// chain owner, else the first gossip-present chain entry that is not this node (a forwarding
    /// guess, [`crate::cluster::OwnerSource::Fallback`]), else `None`. Without a chain: the
    /// identity election, or this node when nothing is electable (single-node dev). It never
    /// resolves to THIS node unless this node is the real owner: a chain that went dark in a node's
    /// own view must not make it a self-serving writer.
    ///
    /// Every authoritative resolution feeds the cluster's observed-owner tracker so the fencing
    /// epoch bumps exactly on real ownership transitions; a fallback forwarding guess is never
    /// observed as an owner.
    pub fn control_plane_leader_with_source(
        &self,
    ) -> Option<(String, crate::cluster::OwnerSource)> {
        let chain = crate::cluster::Cluster::owner_chain_from_env();
        let pref = std::env::var("HIVE_CP_LEADER").ok();
        let resolved = crate::cluster::Cluster::control_plane_owner_with_source(
            &chain,
            pref.as_deref(),
            &self.registry.nodes(),
            &self.node_name,
        )
        .or_else(|| {
            chain.is_empty().then(|| {
                (
                    self.node_name.clone(),
                    crate::cluster::OwnerSource::Election,
                )
            })
        });
        if let Some((owner, source)) = &resolved {
            if source.is_authority() {
                self.cluster.observe_owner(owner);
            }
        }
        resolved
    }

    /// Where request-path code forwards a leader-only read or write: see
    /// [`Self::control_plane_leader_with_source`]. `None` means no owner is
    /// resolvable from this node's view (a configured chain is dark here) —
    /// callers answer a retryable refusal or serve their local best effort,
    /// never treat themselves as the owner. Background single-writer jobs do
    /// NOT use this; they ask `leadership::may_act`.
    pub fn control_plane_leader(&self) -> Option<String> {
        self.control_plane_leader_with_source()
            .map(|(owner, _)| owner)
    }

    /// True when THIS node is the control-plane owner for REQUEST-PATH
    /// purposes (serve a mutation locally instead of forwarding it).
    ///
    /// Gated on mesh freshness first: a node that can't currently see its
    /// expected peers (`mesh_health().isolated`) must never assert leadership
    /// from that view; `admin_ingress` already fails mutations closed (503)
    /// when the resolved leader is unreachable, so refusing here safely defers
    /// rather than risking a stale-data write. A forwarding guess
    /// (`OwnerSource::Fallback`) is never this node by construction.
    pub fn is_control_plane_leader(&self) -> bool {
        if self.mesh_health().isolated {
            return false;
        }
        matches!(
            self.control_plane_leader_with_source(),
            Some((owner, source)) if source.is_authority() && owner == self.node_name
        )
    }

    /// THE forwarding decision for a request-path read or write that belongs
    /// to the control-plane owner: the owner (or forwarding guess) when that is
    /// ANOTHER node, resolved exactly once per call so the answer cannot flip
    /// between a "am I the leader" test and a "who is" lookup. `None` = do not
    /// forward: this node is the resolved owner (isolated or not — a hop to
    /// itself buys nothing), or no owner is resolvable from this node's view
    /// (reads serve their local best effort, writes answer a retryable 503).
    /// Every read proxy and owner-forward uses this, never a hand-rolled pair
    /// of `is_control_plane_leader()` / `control_plane_leader()` calls.
    pub fn leader_forward_target(&self) -> Option<String> {
        self.control_plane_leader()
            .filter(|leader| !leader.is_empty() && *leader != self.node_name)
    }

    /// The leader's [`NodeInfo`] (for its public IP), or None when this node is the
    /// leader or the leader isn't resolvable in the registry.
    pub fn leader_node(&self) -> Option<hive_edge::NodeInfo> {
        let leader = self.leader_forward_target()?;
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
        sandbox_backend: Option<Arc<dyn hive_backend::CellBackend>>,
    ) -> Arc<CloudState> {
        let configured_trusted_peer_ids = configured_endpoint_ids("HIVE_TRUSTED_NODE_IDS")
            .unwrap_or_else(|error| panic!("invalid mesh trust configuration: {error}"));
        let configured_expected_peer_ids = configured_endpoint_ids("HIVE_EXPECTED_NODE_IDS")
            .unwrap_or_else(|error| panic!("invalid mesh membership configuration: {error}"));
        let expected_peer_ids = if configured_expected_peer_ids.is_empty() {
            configured_trusted_peer_ids.clone()
        } else {
            configured_expected_peer_ids
        };
        let relayed_trust_compat = configured_bool_default_true("HIVE_RELAYED_TRUST_COMPAT")
            .unwrap_or_else(|error| {
                panic!("invalid mesh trust compatibility configuration: {error}")
            });
        if relayed_trust_compat {
            tracing::warn!(
                "relayed peer records may expand mutable authorization during mixed rollout; \
                 set HIVE_RELAYED_TRUST_COMPAT=0 after every node has the complete trust roster"
            );
        }
        let cluster = crate::cluster::Cluster::new(node_name.clone());
        let deployment_ledger = crate::deployment_ledger::DeploymentLedger::open(
            crate::persist::data_dir().join("deployment-ledger-v1.json"),
            &node_name,
        )
        .unwrap_or_else(|error| panic!("deployment ledger failed closed: {error:#}"));
        let integrity_signer = Arc::new(
            crate::integrity_signer::IntegritySigner::open_or_create(&node_name)
                .unwrap_or_else(|error| panic!("integrity signing key failed closed: {error:#}")),
        );
        let runtime_artifact_transfer = crate::runtime_artifact_transfer::TransferService::open(
            crate::persist::data_dir().join("runtime-artifacts-v1"),
            node_name.clone(),
            deployment_ledger.boot_nonce(),
            gw.clone(),
        )
        .unwrap_or_else(|error| {
            tracing::error!(
                %error,
                "runtime artifact transfer receiver is disabled; node serving remains available"
            );
            crate::runtime_artifact_transfer::TransferService::disabled(format!(
                "runtime artifact transfer receiver failed to initialize: {error:#}"
            ))
        });
        let owner_email =
            std::env::var("HIVE_OWNER_EMAIL").unwrap_or_else(|_| "owner@hive.cloud".into());
        let admin_emails: Vec<String> = {
            let mut v: Vec<String> = std::env::var("HIVE_ADMIN_EMAILS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            for owner in owner_email
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
            {
                if !v.contains(&owner) {
                    v.push(owner);
                }
            }
            v
        };
        let deploy_suffixes = std::env::var("HIVE_DEPLOY_SUFFIXES")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec!["localhost".into()]);
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
        let db_domain = std::env::var("HIVE_DB_DOMAIN")
            .ok()
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let ingress = std::env::var("HIVE_INGRESS")
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| matches!(s.as_str(), "ngrok" | "dual" | "dns"))
            .unwrap_or_else(|| "dns".into());
        let teams = crate::teams::TeamStore::new();
        teams.ensure_seed(&owner_email);
        let region_for_sandboxes = region.clone();
        let node_name_for_sandboxes = node_name.clone();
        let http = reqwest::Client::new();
        let state = Arc::new(CloudState {
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
            admission: Arc::new(RateLimiter::new(
                std::env::var("HIVE_DEPLOY_BURST")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1000),
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
            http: http.clone(),
            dns_geo: crate::dns_geo::GeoCache::spawn(http),
            dns_probes: Arc::new(crate::dns_probe::NsProbes::new()),
            acme_challenges: crate::acme::AcmeChallengeStore::new(),
            acme_http01: crate::acme::Http01Store::new(),
            world_queue: crate::world_queue::WorldQueue::new(),
            projects: crate::project_settings::ProjectStore::new(),
            production_deployments: crate::production_deployments::ProductionDeploymentStore::new(),
            builds: crate::git::BuildStore::new(),
            build_cancels: crate::git::BuildCancelRegistry::new(),
            deployment_ledger,
            integrity_signer,
            runtime_artifact_transfer,
            cluster,
            teams,
            gitops: crate::gitops::GitOpsStore::new(),
            git_index: crate::gitops::GitRepoIndex::new(),
            peers: RwLock::new(Vec::new()),
            node_admins: RwLock::new(std::collections::HashMap::new()),
            trusted_peer_ids: std::sync::Arc::new(std::sync::RwLock::new(
                configured_trusted_peer_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect(),
            )),
            expected_peer_ids,
            relayed_trust_compat,
            peer_iroh: RwLock::new(std::collections::HashMap::new()),
            last_gossip_ok_ms: std::sync::atomic::AtomicU64::new(0),
            peer_routes: RwLock::new(std::collections::HashMap::new()),
        peer_routes_by_node: RwLock::new(std::collections::HashMap::new()),
        route_gens: RwLock::new(std::collections::HashMap::new()),
        target_node: RwLock::new(std::collections::HashMap::new()),
        route_pub_gen: std::sync::atomic::AtomicU64::new(0),
        route_pub_hash: RwLock::new(0),
            peer_deployments: RwLock::new(std::collections::HashMap::new()),
            git_poll_seen: RwLock::new(std::collections::HashMap::new()),
            iroh: RwLock::new(None),
            mesh: RwLock::new(None),
            browser_mesh: RwLock::new(None),
            browser_admissions: crate::browser_admission::BrowserAdmissionStore::new(),
            browser_presence: crate::browser_presence::BrowserPresenceStore::new(),
            relay_set: RwLock::new(None),
            leases: crate::lease::LeaseStore::new(),
            container_holders: RwLock::new(std::collections::HashMap::new()),
            webhooks: Arc::new(crate::webhooks::WebhookStore::new()),
            databases: Arc::new(crate::databases::DatabaseStore::new()),
            queues: crate::queues::QueueStore::new(),
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
            sandboxes: Arc::new(crate::sandboxes_platform::PlatformSandboxProvider::new(
                region_for_sandboxes,
                sandbox_backend,
                node_name_for_sandboxes,
            )),
            firecracker,
            owner_email,
            admin_emails,
            event_log: crate::event_log::EventLog::new(
                crate::event_log::data_subdir(&crate::persist::data_dir()),
            ),
            req_count: Mutex::new(0),
            blocked_count: Mutex::new(0),
        });
        {
            let weak_state = Arc::downgrade(&state);
            state
                .runtime_artifact_transfer
                .bind_persist(Arc::new(move || {
                    weak_state
                        .upgrade()
                        .map(|state| crate::persist::persist_durable(&state))
                        .unwrap_or(false)
                }));
        }
        state
    }

    /// Marks a successful control-plane (gossip) sync.
    pub fn mark_gossip_ok(&self) {
        self.last_gossip_ok_ms
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Epoch-ms of the last successful gossip sync (0 = never).
    pub fn last_gossip_ms(&self) -> u64 {
        self.last_gossip_ok_ms
            .load(std::sync::atomic::Ordering::Relaxed)
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
        let self_id = self
            .registry
            .me()
            .peer_id
            .and_then(|id| id.parse::<iroh::EndpointId>().ok());
        let mut expected = self.expected_peer_ids.clone();
        if let Some(self_id) = self_id {
            expected.remove(&self_id);
        }
        let fresh_nodes = self.registry.nodes();
        let audible: std::collections::HashSet<iroh::EndpointId> = fresh_nodes
            .iter()
            .filter(|node| !node.is_self)
            .filter_map(|node| node.peer_id.as_deref())
            .filter_map(|id| id.parse().ok())
            .collect();
        let healthy: std::collections::HashSet<iroh::EndpointId> = fresh_nodes
            .iter()
            .filter(|node| !node.is_self && node.healthy)
            .filter_map(|node| node.peer_id.as_deref())
            .filter_map(|id| id.parse().ok())
            .collect();
        let direct = self.direct_reachable_ids();
        let audible_peers = expected.intersection(&audible).count();
        let visible_healthy_peers = expected.intersection(&healthy).count();
        let direct_reachable_peers = expected.intersection(&direct).count();
        MeshHealth {
            audible_peers,
            expected_peers: expected.len(),
            visible_healthy_peers,
            direct_reachable_peers,
            isolated: mesh_isolated(expected.len(), direct_reachable_peers),
            uptime_ms: hive_core::now_ms().saturating_sub(self.boot_ms),
        }
    }

    /// Endpoint ids this node has DIRECT, fresh OUTBOUND transport evidence
    /// for: a successful dial/trunk inside `HIVE_MESH_DIRECT_FRESH_SECS` (120)
    /// that is newer than the last failure. Gossip relayed through a third node
    /// never counts. `mesh_health`'s `direct_reachable_peers` / `isolated`.
    pub fn direct_reachable_ids(&self) -> std::collections::HashSet<iroh::EndpointId> {
        let direct_freshness = Self::direct_freshness();
        self.mesh
            .read()
            .clone()
            .map(|pool| pool.dial_evidence_snapshot())
            .unwrap_or_default()
            .into_iter()
            .filter(|evidence| {
                evidence.last_success_ago.is_some_and(|success| {
                    success <= direct_freshness
                        && evidence
                            .last_failure_ago
                            .is_none_or(|failure| success < failure)
                })
            })
            .filter_map(|evidence| evidence.endpoint_id.parse().ok())
            .collect()
    }

    /// How recent DIRECT evidence (a dial, or a self-report received first
    /// hand) must be to count: `HIVE_MESH_DIRECT_FRESH_SECS`, 120 s.
    pub(crate) fn direct_freshness() -> std::time::Duration {
        std::time::Duration::from_secs(
            std::env::var("HIVE_MESH_DIRECT_FRESH_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|seconds| *seconds > 0)
                .unwrap_or(120),
        )
    }

    /// [`Self::direct_reachable_ids`] plus every peer whose OWN self-report
    /// reached this node directly inside the same freshness window (an
    /// inbound announce or the first entry of a `/v1/nodes` reply —
    /// `NodeRegistry::gossip_evidence_snapshot`; relayed copies never count).
    /// The leadership voter quorum: a working inbound link is contact even
    /// while this node's own outbound dials to that peer are failing.
    pub fn direct_contact_ids(&self) -> std::collections::HashSet<iroh::EndpointId> {
        let freshness = Self::direct_freshness();
        let mut ids = self.direct_reachable_ids();
        ids.extend(
            self.registry
                .gossip_evidence_snapshot()
                .into_iter()
                .filter(|evidence| evidence.last_received_ago <= freshness)
                .filter_map(|evidence| evidence.endpoint_id.parse::<iroh::EndpointId>().ok()),
        );
        ids
    }

    /// Records one request event: counts it (and counts a WAF or bot block), attributes it to the
    /// OWNING TENANT (the team that owns the project, so per-tenant metrics reads cannot leak
    /// across tenants; projectless events, host-rejected or platform-internal, go to the system
    /// tenant) and appends it to the event log.
    pub fn record(&self, ev: Event) {
        *self.req_count.lock() += 1;
        if ev.action == "waf-deny" || ev.action == "bot-block" {
            *self.blocked_count.lock() += 1;
        }
        let mtenant = if ev.project.is_empty() {
            crate::metrics::SYSTEM_TENANT.to_string()
        } else {
            crate::admin::norm(&self.projects.team_of(&ev.project)).to_string()
        };
        self.metrics.record(&ev, &mtenant);
        self.event_log.append(ev);
    }

    /// Newest-first events, bounded by `limit`. Serves the unbounded (default
    /// poll) case straight out of the ring — no disk.
    pub fn recent_events(&self, limit: usize) -> Vec<Event> {
        self.event_log.recent(limit)
    }

    /// Filtered query over the full retained history. A window the ring does
    /// not cover is read from the durable JSONL log; the caller is responsible
    /// for running this off a serving tokio worker (see `admin::logs`).
    pub fn query_events(
        &self,
        f: &crate::event_log::EventFilter,
        facts: &crate::event_log::DeployFacts,
    ) -> Vec<Event> {
        self.event_log.query(f, facts)
    }

    /// Oldest event this node can still answer for (0 when unknown).
    pub fn events_oldest_ms(&self) -> u64 {
        self.event_log.oldest_ms()
    }

    /// Reap day-files past retention. Called on boot and on day rotation.
    pub fn sweep_event_log(&self) {
        self.event_log.sweep();
    }

    pub fn counters(&self) -> (u64, u64) {
        (*self.req_count.lock(), *self.blocked_count.lock())
    }

    /// Whether this node should route on `host`. A bare/single-label or empty host
    /// (local/direct/default deployment) is allowed; under real-DNS ingress the apps domain
    /// (`*.{HIVE_APPS_DOMAIN}`, one label plus the apex) is allowed (gated like `deploy_url`: in
    /// ngrok mode `*.{apps_domain}` never reaches this node, so admission stays byte-identical to
    /// the pre-apps-domain behaviour); any other multi-label host must end with a configured deploy
    /// suffix, else it is a foreign root.
    ///
    /// A CUSTOM DOMAIN attached to a project (`POST /v1/projects/:p/domains`) is admitted by two
    /// paths that match the two ways this node can serve a host: `serves_host` (this node itself
    /// owns the deployment: its OWN `c.gw.aliases` has the entry) and `peer_routes` (the mesh
    /// gossip already knows some OTHER node owns it, the same mechanism that makes every node admit
    /// `*.{apps_domain}` hosts it does not own and mesh-proxy them to the real owner). A customer's
    /// own custom domain gets no blanket admission, so it needs this per-alias existence check:
    /// once the real owner's periodic `serve_hosts` publish carries a freshly attached domain (see
    /// `project_domain_add`, which applies the alias ON the owning node), every OTHER node's
    /// `peer_routes` picks it up on the next gossip cycle and can admit and mesh-proxy it too.
    /// `peer_routes` is matched full-host first (custom domains), then by the platform label
    /// scheme: a peer publishing `numo.gg` admits exactly that host, never the whole `numo.*` label
    /// space.
    pub fn host_allowed(&self, host: &str) -> bool {
        (self.ingress != "ngrok" && host_matches_apps_domain(host, &self.apps_domain))
            || host_has_allowed_suffix(host, &self.deploy_suffixes)
            || self.gw.serves_host(host)
            || {
                let h = host.split(':').next().unwrap_or(host);
                let routes = self.peer_routes.read();
                routes.contains_key(&h.to_ascii_lowercase())
                    || routes.contains_key(h.split('.').next().unwrap_or(h))
            }
    }

    /// Public URL for a deployment alias. Real-DNS ingress (`HIVE_INGRESS != ngrok`) emits
    /// `https://<label>.{apps_domain}` (single-label wildcard); ngrok mode keeps `https://<alias>`
    /// (the UI session-maps domains).
    ///
    /// PREVIEWS go under the fleet-served deploy zone when one is configured AND its certificate is
    /// installed here: Seer answers any label in that zone with affinity computed per query from
    /// the route table, so a preview costs zero DNS records and zero extra certificates however
    /// many deployments exist, while production keeps the project's own namespace on the apps
    /// domain. It is gated on the certificate on purpose (a name that resolves but fails TLS is
    /// worse than one that resolves with an extra hop), so until `*.{deploy_zone}` is installed it
    /// keeps the previous behaviour rather than minting unusable URLs; old preview links on the
    /// apps domain keep resolving through its wildcard either way.
    pub fn deploy_url(&self, alias: &str) -> String {
        if self.ingress != "ngrok" {
            let sub = alias.split('.').next().unwrap_or(alias);
            if let Some(dz) = crate::dnsserver::deploy_zone() {
                if crate::vercel_dns::is_preview_label(sub)
                    && crate::acme::zone_tls_ready(dz)
                {
                    return format!("https://{sub}.{dz}");
                }
            }
            format!("https://{sub}.{}", self.apps_domain)
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

    /// Builds an [`Event`] with EXACT attribution only (no default-deployment fallback, no detail
    /// fallback): an event belongs to a project/deployment iff the request host's subdomain
    /// actually ALIASES one of this node's deployments. The serving resolution's default-deployment
    /// fallback stamped every unmatched host (platform bot probes, other tenants' DB hosts,
    /// peer-hosted projects routed through this node) with this node's default project, leaking
    /// foreign request lines into that project's log view and corrupting its tenant's metrics
    /// attribution; unresolved hosts stay UNATTRIBUTED (empty project/deployment) and are
    /// operator-only in the logs API.
    pub fn event(
        &self,
        region: &str,
        method: &str,
        host: &str,
        path: &str,
        status: u16,
        action: &str,
        detail: &str,
    ) -> Event {
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
        assert!(
            merged.contains_key("fc-virginia"),
            "alive-but-unreached node carried forward"
        );
        assert!(
            !merged.contains_key("dead-node"),
            "node gone from registry is dropped"
        );

        // Reached this round with an empty list → authoritative (node truly has none now).
        let mut fresh: HashMap<String, Vec<fluid_core::DeploymentInfo>> = HashMap::new();
        fresh.insert("fc-virginia".into(), vec![]);
        let merged = merge_deployments_ttl(&prev, fresh, &alive);
        assert!(
            merged.get("fc-virginia").unwrap().is_empty(),
            "reached node's empty list wins"
        );
    }

    #[test]
    fn allowed_suffixes_route_foreign_roots_rejected() {
        let s = suffixes();
        // Legit wildcard ingress hosts.
        assert!(host_has_allowed_suffix(
            "myapp.deployment.shadow.ngrok.pizza",
            &s
        ));
        assert!(host_has_allowed_suffix(
            "dpl-abc.deployment.shadow.ngrok.pizza:443",
            &s
        ));
        assert!(host_has_allowed_suffix("myapp.localhost", &s));
        assert!(host_has_allowed_suffix("myapp.localhost:8787", &s));
        // Bare / empty / direct hosts are allowed (no foreign root to spoof).
        assert!(host_has_allowed_suffix("foobar", &s));
        assert!(host_has_allowed_suffix("", &s));
        // Foreign roots are rejected even if the first label collides with an alias.
        assert!(!host_has_allowed_suffix("myapp.evil.com", &s));
        assert!(!host_has_allowed_suffix("foobar.evil.com", &s));
        assert!(!host_has_allowed_suffix(
            "deployment.shadow.ngrok.pizza.evil.com",
            &s
        ));
    }

    #[test]
    fn route_ttl_merge_survives_transient_miss_but_drops_stale() {
        use super::{merge_routes_ttl, PeerRoute};
        use std::collections::{HashMap, HashSet};
        let mk = |node: &str, seen: u64| PeerRoute {
            node_id: node.into(),
            region: "r".into(),
            gateway: format!("http://{node}"),
            latency_ms: 1,
            healthy: true,
            last_seen_ms: seen,
        };
        let now = 1_000_000u64;
        let ttl = 30_000u64;
        // prev: host "app" served by peer-X (seen recently) and peer-Y (seen long ago).
        let mut prev: HashMap<String, Vec<PeerRoute>> = HashMap::new();
        prev.insert(
            "app".into(),
            vec![mk("peer-x", now - 5_000), mk("peer-y", now - 90_000)],
        );

        // This round we reached only peer-z (serves "app"); X and Y were NOT reached.
        let mut fresh: HashMap<String, Vec<PeerRoute>> = HashMap::new();
        fresh.insert("app".into(), vec![mk("peer-z", now)]);
        let seen: HashSet<String> = ["peer-z".to_string()].into_iter().collect();

        let merged = merge_routes_ttl(&prev, fresh, &seen, now, ttl);
        let nodes: HashSet<&str> = merged["app"].iter().map(|r| r.node_id.as_str()).collect();
        assert!(nodes.contains("peer-z"), "freshly-gossiped route present");
        assert!(
            nodes.contains("peer-x"),
            "transient-miss peer kept within TTL"
        );
        assert!(!nodes.contains("peer-y"), "stale (>TTL) peer dropped");

        // If peer-x IS reached this round but no longer serves "app", it must drop
        // (reached peer is authoritative) — fresh has no app route for it.
        let mut fresh2: HashMap<String, Vec<PeerRoute>> = HashMap::new();
        let seen2: HashSet<String> = ["peer-x".to_string()].into_iter().collect();
        let merged2 = merge_routes_ttl(&prev, fresh2.drain().collect(), &seen2, now, ttl);
        let nodes2: HashSet<&str> = merged2
            .get("app")
            .map(|v| v.iter().map(|r| r.node_id.as_str()).collect())
            .unwrap_or_default();
        assert!(
            !nodes2.contains("peer-x"),
            "reached peer that dropped the deployment ages out immediately"
        );
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
        assert!(
            super::mesh_isolated(7, 0),
            "peers expected, none visible = isolated"
        );
        // Standalone-by-design (genuinely zero trusted peers configured) is never
        // isolated — nothing was expected.
        assert!(!super::mesh_isolated(0, 0));
        // Seeing at least one expected peer, even far short of the full set
        // (a mesh mid-reconverge), is not isolation.
        assert!(!super::mesh_isolated(7, 1));
        assert!(!super::mesh_isolated(7, 7));
    }
}
