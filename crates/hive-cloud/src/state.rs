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
        Event {
            ts_ms: now_ms(),
            region: region.to_string(),
            method: method.to_string(),
            host: host.to_string(),
            path: path.to_string(),
            status,
            action: action.to_string(),
            detail: detail.to_string(),
        }
    }
}
