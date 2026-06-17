//! Shared state for a hive-cloud node: every platform subsystem in one place.

use std::collections::VecDeque;
use std::sync::Arc;

use fluid_compute::Fluid;
use fluid_gateway::Gateway;
use hive_controlplane::Hive;
use hive_core::now_ms;
use hive_edge::{
    bot::BotPolicy, BotManager, CdnCache, ConcurrencyLimiter, CronScheduler, NodeRegistry, Router,
    Waf, WorkflowEngine,
};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

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
}

pub struct CloudState {
    pub region: String,
    pub node_name: String,
    pub public_base: String,

    pub waf: Arc<Waf>,
    pub bot: Arc<BotManager>,
    pub bot_policy: RwLock<BotPolicy>,
    pub cdn: Arc<CdnCache>,
    pub limiter: Arc<ConcurrencyLimiter>,
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
    pub webhooks: Arc<crate::webhooks::WebhookStore>,
    pub databases: Arc<crate::databases::DatabaseStore>,
    pub metrics: crate::metrics::MetricsStore,
    pub incidents: crate::incidents::IncidentStore,
    pub securelinks: crate::securelink::SecureLinkStore,
    /// Platform owner identity (seeds the default team; ops dashboard owner).
    pub owner_email: String,

    events: Mutex<VecDeque<Event>>,
    req_count: Mutex<u64>,
    blocked_count: Mutex<u64>,
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
        let teams = crate::teams::TeamStore::new();
        teams.ensure_seed(&owner_email);
        Arc::new(CloudState {
            region,
            node_name,
            public_base,
            waf,
            bot,
            bot_policy: RwLock::new(BotPolicy::default()),
            cdn,
            limiter,
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
            webhooks: Arc::new(crate::webhooks::WebhookStore::new()),
            databases: Arc::new(crate::databases::DatabaseStore::new()),
            metrics: crate::metrics::MetricsStore::new(),
            incidents: crate::incidents::IncidentStore::new(),
            securelinks: crate::securelink::SecureLinkStore::new(),
            owner_email,
            events: Mutex::new(VecDeque::with_capacity(512)),
            req_count: Mutex::new(0),
            blocked_count: Mutex::new(0),
        })
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
        }
    }
}
