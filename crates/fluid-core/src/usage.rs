//! ONE canonical catalog of the billable constructs the usage page exposes.
//!
//! This is DATA, not pricing logic. Public prices live in billing::RATE_CARD; COGS live in
//! billing::COST_CARD and never cross the tenant boundary. A row here carries the stable wire
//! key, the exact label/group/unit the page prints, the raw-units-per-billed-unit divisor, WHERE
//! the counter is measured, and the honest state of that measurement.
//!
//! Five states, none of which fabricates a number:
//!   * Billed       a real cumulative counter, a public price and a COGS basis clearing
//!                  MIN_GROSS_MARGIN_BPS. Charged only by the single may_act(BillingMeter)
//!                  writer behind HIVE_BILLING_METER_RESOURCES.
//!   * Indexed      a real cumulative counter with no public price. A counter, not a sale:
//!                  shown with its measured value and $0.00, never charged.
//!   * Unmetered    a construct this platform does not measure. Rendered Not metered / -,
//!                  never as an invented zero.
//!   * Subscription a subscription licence, read from the plan ledger, never from a meter.
//!   * Legacy       an alias of one of the twelve legacy UsageTotals counters. Value and
//!                  price come from the legacy fields, so no construct is charged twice.
//!
//! fluid-core is the lowest crate both fluid-gateway and hive-cloud (and hive-contract, for the
//! signed sample keys) already depend on, so the process-global measurement hook lives here too.
//! It is installed once at boot by hive-cloud and funnels every increment into
//! usage_metering::record, the ONE writer of the node-local meter.

use serde::Serialize;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageState {
    Billed,
    Indexed,
    Unmetered,
    Subscription,
    Legacy,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Sku {
    pub key: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub unit: &'static str,
    /// Raw counter units in ONE billed unit.
    pub per: f64,
    /// Where the counter is measured, or why it is not. Shown in the usage page.
    pub source: &'static str,
    pub state: UsageState,
    /// Key in UsageTotals.skus / TenantUsage.skus (Billed and Indexed rows).
    pub counter: Option<&'static str>,
    /// Legacy UsageTotals field (Legacy rows).
    pub legacy: Option<&'static str>,
    /// Legacy RateCard field (Legacy rows). Prices come ONLY from the rate card.
    pub legacy_rate: Option<&'static str>,
}

/// Decimal GB-month in byte-milliseconds, the same divisor as billing::BYTE_MS_PER_GB_MONTH.
pub const BYTE_MS_PER_GB_MONTH: f64 = 1_000_000_000.0 * 2_592_000_000.0;
const M: f64 = 1_000_000.0;
const GB: f64 = 1_000_000_000.0;
const HOUR_MS: f64 = 3_600_000.0;
const MINUTE_MS: f64 = 60_000.0;

/// The 29 billable constructs Vercel exposes, minus v0 seats. Order is the page order and is
/// the fixed order of the signed meter.catalog payload: never reorder without a version bump.
pub const CATALOG: &[Sku] = &[
    Sku { key: "cdn_additional_cpu", label: "CDN Requests - Additional CPU Duration",
          group: "Vercel Delivery Network", unit: "minutes", per: MINUTE_MS,
          source: "no edge CPU-duration gauge on the serving path",
          state: UsageState::Unmetered, counter: None, legacy: None, legacy_rate: None },
    Sku { key: "cdn_requests", label: "CDN Requests",
          group: "Vercel Delivery Network", unit: "M requests", per: M,
          source: "CloudState::record, one per admitted request event",
          state: UsageState::Billed, counter: Some("cdn_requests"), legacy: None, legacy_rate: None },
    Sku { key: "fast_data_transfer", label: "Fast Data Transfer",
          group: "Vercel Delivery Network", unit: "GB", per: GB,
          source: "edge_pipeline_inner buffered 200 responses and CDN cache hits",
          state: UsageState::Billed, counter: Some("fast_data_transfer_bytes"), legacy: None, legacy_rate: None },
    Sku { key: "workflow_events", label: "Workflow Events",
          group: "Vercel Functions", unit: "M events", per: M,
          source: "main.rs::wf_invoker step execution (usage_metering)",
          state: UsageState::Legacy, counter: None, legacy: Some("workflow_steps"),
          legacy_rate: Some("workflow_steps_per_million_cents") },
    Sku { key: "fluid_provisioned_memory", label: "Fluid Provisioned Memory",
          group: "Vercel Functions", unit: "GB-hrs", per: 1000.0,
          source: "fleet_function_stats memory_gb_hrs x1000",
          state: UsageState::Legacy, counter: None, legacy: Some("mem_gb_hr_milli"),
          legacy_rate: Some("mem_gb_hr_cents") },
    Sku { key: "fluid_active_cpu", label: "Fluid Active CPU",
          group: "Vercel Functions", unit: "hours", per: HOUR_MS,
          source: "fleet_function_stats active_cpu_ms",
          state: UsageState::Legacy, counter: None, legacy: Some("active_cpu_ms"),
          legacy_rate: Some("active_cpu_hr_cents") },
    Sku { key: "workflow_storage_retention", label: "Workflow Storage Retention",
          group: "Vercel Functions", unit: "GB-mos", per: BYTE_MS_PER_GB_MONTH,
          source: "usage_metering::sample_once workflow state byte-ms gauge",
          state: UsageState::Billed, counter: Some("workflow_storage_byte_ms"), legacy: None, legacy_rate: None },
    Sku { key: "function_invocations", label: "Function Invocations",
          group: "Vercel Functions", unit: "M invocations", per: M,
          source: "fleet_function_stats requests",
          state: UsageState::Legacy, counter: None, legacy: Some("requests"),
          legacy_rate: Some("requests_per_million_cents") },
    Sku { key: "fast_origin_transfer", label: "Fast Origin Transfer",
          group: "Vercel Functions", unit: "GB", per: GB,
          source: "fluid_gateway::proxy_function origin request bytes",
          state: UsageState::Billed, counter: Some("fast_origin_transfer_bytes"), legacy: None, legacy_rate: None },
    Sku { key: "workflow_storage_writes", label: "Workflow Storage Writes",
          group: "Vercel Functions", unit: "GB", per: GB,
          source: "world_queue::enqueue job-mirror bytes",
          state: UsageState::Billed, counter: Some("workflow_storage_writes_bytes"), legacy: None, legacy_rate: None },
    Sku { key: "isr_reads", label: "ISR Reads",
          group: "Content, Caching & Optimization", unit: "M reads", per: M,
          source: "admin::rc_get / hive_edge::RuntimeCache::get",
          state: UsageState::Billed, counter: Some("isr_reads"), legacy: None, legacy_rate: None },
    Sku { key: "image_transformations", label: "Image Optimization Transformation",
          group: "Content, Caching & Optimization", unit: "1K transformations", per: 1000.0,
          source: "fluid_gateway::serve_optimized_image cache-MISS encode",
          state: UsageState::Billed, counter: Some("image_transformations"), legacy: None, legacy_rate: None },
    Sku { key: "image_cache_writes", label: "Image Optimization Cache Writes",
          group: "Content, Caching & Optimization", unit: "1K writes", per: 1000.0,
          source: "fluid_gateway image cache rename landing",
          state: UsageState::Billed, counter: Some("image_cache_writes"), legacy: None, legacy_rate: None },
    Sku { key: "observability_events", label: "Observability Events",
          group: "Observability", unit: "M events", per: M,
          source: "CloudState::record / event_log::append",
          state: UsageState::Billed, counter: Some("observability_events"), legacy: None, legacy_rate: None },
    Sku { key: "web_analytics_events", label: "Web Analytics Events",
          group: "Observability", unit: "M events", per: M,
          source: "fluid_gateway::vercel_insights accepted beacons",
          state: UsageState::Billed, counter: Some("web_analytics_events"), legacy: None, legacy_rate: None },
    Sku { key: "build_cpu_minutes", label: "Build CPU Minutes",
          group: "Build & Deploy", unit: "hours", per: HOUR_MS,
          source: "git.rs::BuildCompletionGuard::drop wall-ms on the pinned 1-vCPU build lane (stated assumption)",
          state: UsageState::Billed, counter: Some("build_cpu_ms"), legacy: None, legacy_rate: None },
    Sku { key: "deployment_storage", label: "Deployment Storage",
          group: "Build & Deploy", unit: "GB-mos", per: BYTE_MS_PER_GB_MONTH,
          source: "usage_metering::sample_once deploy-root byte-ms gauge",
          state: UsageState::Billed, counter: Some("deployment_storage_byte_ms"), legacy: None, legacy_rate: None },
    Sku { key: "function_storage", label: "Function Storage",
          group: "Build & Deploy", unit: "GB-mos", per: BYTE_MS_PER_GB_MONTH,
          source: "usage_metering::sample_once build-cache / runtime-artifact byte-ms gauge",
          state: UsageState::Billed, counter: Some("function_storage_byte_ms"), legacy: None, legacy_rate: None },
    Sku { key: "snapshot_storage", label: "Snapshot Storage",
          group: "Build & Deploy", unit: "GB-mos", per: BYTE_MS_PER_GB_MONTH,
          source: "reflink-shared snapshots are not attributable per tenant (storage_broker)",
          state: UsageState::Unmetered, counter: None, legacy: None, legacy_rate: None },
    Sku { key: "sandbox_provisioned_memory", label: "Sandbox Provisioned Memory",
          group: "Sandbox", unit: "GB-hrs", per: 3_686_400_000.0,
          source: "sandboxes_platform::stop_sandbox memory_mb x measured duration",
          state: UsageState::Billed, counter: Some("sandbox_memory_mb_ms"), legacy: None, legacy_rate: None },
    Sku { key: "sandbox_data_transfer", label: "Sandbox Data Transfer",
          group: "Sandbox", unit: "GB", per: GB,
          source: "no sandbox network byte gauge; exec output bytes are capped, not network transfer",
          state: UsageState::Unmetered, counter: None, legacy: None, legacy_rate: None },
    Sku { key: "sandbox_active_cpu", label: "Sandbox Active CPU",
          group: "Sandbox", unit: "hours", per: HOUR_MS,
          source: "sandboxes_platform::stop_sandbox total_duration_ms",
          state: UsageState::Billed, counter: Some("sandbox_active_cpu_ms"), legacy: None, legacy_rate: None },
    Sku { key: "sandbox_creations", label: "Sandbox Creations",
          group: "Sandbox", unit: "creations", per: 1.0,
          source: "sandboxes_platform::create_sandbox_with_id",
          state: UsageState::Indexed, counter: Some("sandbox_creations"), legacy: None, legacy_rate: None },
    Sku { key: "queue_message_sends", label: "Queue Message Sends",
          group: "Queues", unit: "M messages", per: M,
          source: "queues_api::message_send and world_queue::enqueue",
          state: UsageState::Billed, counter: Some("queue_message_sends"), legacy: None, legacy_rate: None },
    Sku { key: "queue_notifications", label: "Queue Notifications",
          group: "Queues", unit: "M notifications", per: M,
          source: "no notification primitive; push POST is a delivery, not a notification",
          state: UsageState::Unmetered, counter: None, legacy: None, legacy_rate: None },
    Sku { key: "queue_message_receives", label: "Queue Message Receives",
          group: "Queues", unit: "M messages", per: M,
          source: "queues_api::messages_pull and queues::spawn_delivery_loop due batch",
          state: UsageState::Billed, counter: Some("queue_message_receives"), legacy: None, legacy_rate: None },
    Sku { key: "queue_message_deletes", label: "Queue Message Deletes",
          group: "Queues", unit: "M messages", per: M,
          source: "queues_api::messages_ack, push success and retention reaped",
          state: UsageState::Billed, counter: Some("queue_message_deletes"), legacy: None, legacy_rate: None },
    Sku { key: "queue_visibility_changes", label: "Queue Visibility Changes",
          group: "Queues", unit: "M changes", per: M,
          source: "no visibility-change primitive; lease mutations are counted as receives",
          state: UsageState::Unmetered, counter: None, legacy: None, legacy_rate: None },
    Sku { key: "pro_seats", label: "Pro",
          group: "Subscription Licenses", unit: "seats", per: 1.0,
          source: "billing::account(tenant).plan; a licence is bought or granted, never metered",
          state: UsageState::Subscription, counter: None, legacy: None, legacy_rate: None },
];

/// The FIXED index of every cumulative counter. UsageTotals.catalog and TenantUsage.catalog
/// are [u64; COUNTER_KEYS.len()] arrays in exactly this order; the signed meter.catalog
/// payload carries the same keys in the same order. Never reorder without a version bump.
pub const COUNTER_KEYS: [&'static str; 19] = [
    "cdn_requests",
    "fast_data_transfer_bytes",
    "workflow_storage_byte_ms",
    "fast_origin_transfer_bytes",
    "workflow_storage_writes_bytes",
    "isr_reads",
    "image_transformations",
    "image_cache_writes",
    "observability_events",
    "web_analytics_events",
    "build_cpu_ms",
    "deployment_storage_byte_ms",
    "function_storage_byte_ms",
    "sandbox_memory_mb_ms",
    "sandbox_active_cpu_ms",
    "sandbox_creations",
    "queue_message_sends",
    "queue_message_receives",
    "queue_message_deletes",
];

pub const COUNTER_COUNT: usize = COUNTER_KEYS.len();

pub fn counter_keys() -> Vec<&'static str> {
    COUNTER_KEYS.to_vec()
}

/// Index into the [u64; COUNTER_COUNT] catalog arrays. None for an unknown key: callers drop
/// the delta instead of growing an unbounded map or indexing out of bounds.
pub fn counter_index(key: &str) -> Option<usize> {
    COUNTER_KEYS.iter().position(|k| *k == key)
}

pub fn by_key(key: &str) -> Option<&'static Sku> {
    CATALOG.iter().find(|s| s.key == key)
}

/// Tenant-safe rows: no COGS, no other tenant data, no price (the page reads prices from
/// rate_card only).
pub fn public_rows() -> Vec<serde_json::Value> {
    CATALOG
        .iter()
        .map(|s| {
            serde_json::json!({
                "key": s.key,
                "label": s.label,
                "group": s.group,
                "unit": s.unit,
                "per": s.per,
                "state": s.state,
                "source": s.source,
                "counter": s.counter,
                "legacy": s.legacy,
                "legacy_rate": s.legacy_rate,
            })
        })
        .collect()
}

/// Process-global measurement hook, the same shape as hive_p2p::set_browser_meter. Installed
/// once by hive-cloud at boot; before install, or for a key that is not a real counter,
/// record is a compiled-in no-op. It never blocks, never does I/O and never panics.
pub type UsageSink = Arc<dyn Fn(&str, &'static str, u64) + Send + Sync>;
static SINK: OnceLock<UsageSink> = OnceLock::new();

pub fn set_sink(sink: UsageSink) -> Result<(), UsageSink> {
    SINK.set(sink)
}

/// Report one measured delta. key must be a catalog counter key; a typo or an unmetered
/// construct is dropped rather than admitted into the meter (no unbounded key growth). Tenant
/// attribution is the CALLER's job and must come from a stored record, never a request header.
pub fn record(tenant: &str, key: &'static str, delta: u64) {
    if delta == 0 || tenant.is_empty() {
        return;
    }
    if counter_index(key).is_none() {
        return;
    }
    if let Some(sink) = SINK.get() {
        sink(tenant, key, delta);
    }
}

/// Byte-count spelling of record, for the transfer and storage SKUs.
pub fn record_bytes(tenant: &str, key: &'static str, bytes: u64) {
    record(tenant, key, bytes);
}
