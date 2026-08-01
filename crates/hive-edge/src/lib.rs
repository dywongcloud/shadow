//! `hive-edge` — the edge/platform features layered on top of Hive + Fluid:
//! WAF, bot management, CDN cache, regions/node registry, cron, and workflows.
//! These are the Vercel-style platform capabilities the `hive-cloud` node wires
//! into one request pipeline + control surface.

pub mod bot;
pub mod cdn;
pub mod concurrency;
pub mod cron;
pub mod ratelimit;
pub mod region;
pub mod routing;
pub mod runtime_cache;
pub mod waf;
pub mod workflows;

pub(crate) fn default_true_pub() -> bool {
    true
}

pub use bot::{BotClass, BotManager, BotPolicy};
pub use cdn::{CacheState, CdnCache, Lookup};
pub use concurrency::{ConcurrencyLimiter, ConcurrencyStats, Plan};
pub use cron::{CronJob, CronScheduler};
pub use ratelimit::{RateLimitStats, RateLimiter};
pub use region::{
    continent_of, haversine_km, select_relay_hint, NodeInfo, NodeRegistry, CENTRAL_RELAY_URL,
};
pub use routing::{Redirect, Rewrite, RouteOutcome, Router};
pub use runtime_cache::RuntimeCache;
pub use waf::{RequestCtx, Verdict, Waf, WafAction, WafMatch, WafRule};
pub use workflows::{
    RunStatus, StepInvoker, WorkflowDef, WorkflowEngine, WorkflowRun, WorkflowStep,
};
