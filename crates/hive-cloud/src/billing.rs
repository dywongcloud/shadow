//! Billing & compute credits — Hobby vs Pro, pay-as-you-go + prepaid credits.
//!
//! Stripe-ready: when `STRIPE_SECRET_KEY` is set the checkout endpoint creates a
//! real Stripe Checkout Session; otherwise a fully-working **mock checkout** is
//! used for local/dev. Every balance change is recorded in an append-only ledger
//! and the audit log, so billing is ACID-durable and historically auditable.
//! `STRIPE_API_BASE` points the two Stripe calls (open a session, read it back) at a stand-in
//! instead of `https://api.stripe.com` ([`stripe_api_base`]).

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

pub const MONTH_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Plan catalog. `included_cents` is the monthly compute allowance, the only field that costs real
/// money per active tenant (the rest are ceilings that decide how early a user hits a wall);
/// `overage` allows pay-as-you-go beyond it (Pro, Enterprise). The `max_*` fields are the enforced
/// quotas (0 = unlimited).
#[derive(Clone, Serialize)]
pub struct PlanSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub price_cents: u64,
    /// The Stripe recurring Price id of this plan's subscription (`price_...`), `None` for Hobby.
    /// Checkout uses it directly (`mode=subscription`): ad-hoc `price_data` only supports one-time
    /// payments.
    pub stripe_price_id: Option<&'static str>,
    pub included_cents: u64,
    pub overage: bool,
    /// Max projects for the tenant (0 = unlimited).
    pub max_projects: u32,
    /// Max team members/seats (0 = unlimited).
    pub max_members: u32,
    /// Max Sandboxes per project, 0 = unlimited (business-locking quota).
    pub max_sandboxes: u32,
    /// Max SIMULTANEOUSLY RUNNING Sandboxes per project, 0 = unlimited.
    pub max_running_sandboxes: u32,
    /// Max storage/drive mounts per sandbox, 0 = unlimited.
    pub max_sandbox_mounts: u32,
    /// Max env vars per sandbox, 0 = unlimited.
    pub max_sandbox_env_vars: u32,
    /// Max exposed ports per sandbox, 0 = unlimited.
    pub max_sandbox_ports: u32,
    /// Max total live bytes across a project's shadw drive (0 = unlimited), enforced only at the
    /// `PUT file` handler (`drive_api.rs`).
    pub max_drive_bytes: u64,
    pub features: &'static [&'static str],
}

pub const PLANS: &[PlanSpec] = &[
    PlanSpec {
        id: "hobby",
        name: "Hobby",
        price_cents: 0,
        stripe_price_id: None,
        included_cents: 500,
        overage: false,
        max_projects: 250,
        max_members: 3,
        max_sandboxes: 15,
        max_running_sandboxes: 3,
        max_sandbox_mounts: 3,
        max_sandbox_env_vars: 50,
        max_sandbox_ports: 4,
        max_drive_bytes: 1024 * 1024 * 1024,
        features: &[
            "$5 of included compute / month",
            "1 concurrent build",
            "Community support",
            "Hobby fair-use limits",
        ],
    },
    PlanSpec {
        id: "pro",
        name: "Pro",
        price_cents: 2000,
        stripe_price_id: Some("price_1TqK9jRj0yq8r94Ek8Gjgl7d"),
        included_cents: 2000,
        overage: true,
        max_projects: 1000,
        max_members: 25,
        max_sandboxes: 50,
        max_running_sandboxes: 10,
        max_sandbox_mounts: 5,
        max_sandbox_env_vars: 100,
        max_sandbox_ports: 8,
        max_drive_bytes: 25 * 1024 * 1024 * 1024,
        features: &[
            "$20 of included compute / month",
            "Pay-as-you-go beyond included",
            "Unlimited concurrent builds",
            "Email support & higher limits",
        ],
    },
    PlanSpec {
        id: "enterprise",
        name: "Enterprise",
        price_cents: 100000,
        stripe_price_id: Some("price_1TqKCGRj0yq8r94E4B8VhCC3"),
        included_cents: 100000,
        overage: true,
        max_projects: 0,
        max_members: 0,
        max_sandboxes: 0,
        max_running_sandboxes: 0,
        max_sandbox_mounts: 0,
        max_sandbox_env_vars: 0,
        max_sandbox_ports: 0,
        max_drive_bytes: 0,
        features: &[
            "$1,000 of included compute / month",
            "Automatic multi-region fail-over",
            "1-hour function runtime (3600s)",
            "Team / Org SSO (SAML & OIDC)",
            "100k concurrency · priority support",
        ],
    },
];

/// Per-plan CEILING on one function's resources, `(max_vcpus, max_memory_mib)`, or `None` for no
/// plan-specific cap (the fleet-wide `HIVE_CONTAINER_*_MAX` clamp still applies). Capacity policy:
/// an enterprise function may request at most 2 vCPU / 4 GiB (default sizing is unchanged) since
/// enterprise deployments are the fleet's biggest consumers; every other plan, legacy tenants
/// included, keeps its sizing.
pub fn plan_resource_ceiling(plan: &str) -> Option<(u32, u32)> {
    match plan.trim().to_ascii_lowercase().as_str() {
        "enterprise" => Some((2, 4096)),
        _ => None,
    }
}

/// The plan whose id is exactly `id`, or `None`. A REQUEST that names a plan (checkout, operator
/// grant, tier lock) must go through this, never through [`plan_spec`]: that one falls back to
/// Hobby so a stored legacy plan string still prices, which made an unknown requested plan look
/// like a valid free one and get stored verbatim as the tenant's plan.
pub fn known_plan(id: &str) -> Option<&'static PlanSpec> {
    PLANS.iter().find(|p| p.id == id)
}

/// The accepted plan ids, for a refusal that names them.
pub fn plan_ids() -> String {
    PLANS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")
}

/// The plan priced for `id`: Hobby for an id no plan carries (a stored legacy value). For a
/// plan named by a request use [`known_plan`].
pub fn plan_spec(id: &str) -> &'static PlanSpec {
    known_plan(id).unwrap_or(&PLANS[0])
}

/// Max projects on a plan (0 = unlimited).
pub fn plan_max_projects(plan: &str) -> u32 {
    plan_spec(plan).max_projects
}
/// Max team seats on a plan (0 = unlimited).
pub fn plan_max_members(plan: &str) -> u32 {
    plan_spec(plan).max_members
}
/// Max Sandboxes per project on a plan (0 = unlimited).
pub fn plan_max_sandboxes(plan: &str) -> u32 {
    plan_spec(plan).max_sandboxes
}
/// Max simultaneously RUNNING Sandboxes per project on a plan (0 = unlimited).
pub fn plan_max_running_sandboxes(plan: &str) -> u32 {
    plan_spec(plan).max_running_sandboxes
}
/// Max storage/drive mounts per sandbox on a plan (0 = unlimited).
pub fn plan_max_sandbox_mounts(plan: &str) -> u32 {
    plan_spec(plan).max_sandbox_mounts
}
/// Max env vars per sandbox on a plan (0 = unlimited).
pub fn plan_max_sandbox_env_vars(plan: &str) -> u32 {
    plan_spec(plan).max_sandbox_env_vars
}
/// Max exposed ports per sandbox on a plan (0 = unlimited).
pub fn plan_max_sandbox_ports(plan: &str) -> u32 {
    plan_spec(plan).max_sandbox_ports
}
/// Max total live shadw drive bytes for a project on a plan (0 = unlimited).
pub fn plan_max_drive_bytes(plan: &str) -> u64 {
    plan_spec(plan).max_drive_bytes
}

/// The usage rate card — the SINGLE source of truth for metered pricing (the UI
/// fetches this; the meter loop charges with it). Cents per unit.
#[derive(Clone, Copy, Serialize)]
pub struct RateCard {
    /// Active (non-idle) CPU, cents per CPU-hour.
    pub active_cpu_hr_cents: f64,
    /// Provisioned memory, cents per GB-hour.
    pub mem_gb_hr_cents: f64,
    /// Edge requests, cents per million.
    pub requests_per_million_cents: f64,
    /// WAF-blocked requests, cents per million.
    pub waf_per_million_cents: f64,
    /// Serverless GPU instance-time, cents per GPU-hour (Tesla T4 class).
    pub gpu_hr_cents: f64,
    /// Requests served by a donated browser node, cents per million. 0.0 by design: only
    /// platform-consumed resources are billed, and an owner's own browser serving their own app
    /// consumes none. A real rate-card field, so pricing it later is a config change.
    pub browser_per_million_cents: f64,
}

pub const RATE_CARD: RateCard = RateCard {
    active_cpu_hr_cents: 12.8,
    mem_gb_hr_cents: 1.06,
    requests_per_million_cents: 200.0,
    waf_per_million_cents: 60.0,
    gpu_hr_cents: 60.0,
    browser_per_million_cents: 0.0,
};

/// Cumulative usage counters for a tenant (as measured from live metrics).
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub active_cpu_ms: u64,
    /// Provisioned memory GB-hours ×1000 (milli, to keep it an integer).
    pub mem_gb_hr_milli: u64,
    pub requests: u64,
    pub blocked: u64,
    /// GPU instance wall-time, ms: the GPU is held for the instance's whole life, so wall-time, not
    /// active-CPU, is the honest unit. `serde(default)` keeps pre-GPU snapshots loading.
    #[serde(default)]
    pub gpu_ms: u64,
    /// Requests served by a donated browser node, counted apart from `requests` so they are visible
    /// ahead of any pricing decision and never folded into the billed fleet-request count.
    #[serde(default)]
    pub browser_requests: u64,
}

impl RateCard {
    /// Cost in (fractional) cents for a usage delta.
    pub fn cost_cents(&self, u: &UsageTotals) -> f64 {
        (u.active_cpu_ms as f64 / 3_600_000.0) * self.active_cpu_hr_cents
            + (u.mem_gb_hr_milli as f64 / 1000.0) * self.mem_gb_hr_cents
            + (u.requests as f64 / 1_000_000.0) * self.requests_per_million_cents
            + (u.blocked as f64 / 1_000_000.0) * self.waf_per_million_cents
            + (u.gpu_ms as f64 / 3_600_000.0) * self.gpu_hr_cents
            + (u.browser_requests as f64 / 1_000_000.0) * self.browser_per_million_cents
    }
}

/// A generated invoice for one billing period (subscription + metered usage).
#[derive(Clone, Serialize, Deserialize)]
pub struct Invoice {
    pub id: String,
    pub number: String,
    pub tenant: String,
    pub plan: String,
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    pub lines: Vec<InvoiceLine>,
    pub subtotal_cents: i64,
    pub total_cents: i64,
    /// "paid" | "due" | "draft"
    pub status: String,
    pub created_ms: u64,
    /// Tamper-evident commitment over this SAME period's raw ledger entries: `Some` only once
    /// `account()`'s rollover computed one, never on a draft invoice. `serde(default)` loads older
    /// invoices with `None`.
    #[serde(default)]
    pub ledger_checkpoint: Option<LedgerCheckpoint>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InvoiceLine {
    pub description: String,
    pub amount_cents: i64,
}

/// Tamper-evident commitment over one tenant-period's raw `LedgerEntry` rows: a plain SHA-256 hash
/// chain for tamper-evidence and fast replay verification. NOT zero-knowledge: this platform's
/// billing has no external untrusted verifier to convince. See
/// `BillingStore::compute_ledger_checkpoint`.
#[derive(Clone, Serialize, Deserialize)]
pub struct LedgerCheckpoint {
    pub tenant: String,
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    /// Number of raw `LedgerEntry` rows folded into `commitment` — a fast
    /// "does this still look complete" check without re-hashing.
    pub entry_count: u64,
    /// hex(SHA-256) chain root over every entry in
    /// `[period_start_ms, period_end_ms)`, ordered `(ts_ms, id)` ASC.
    pub commitment: String,
    /// True once the raw entries this commitment covers have actually been
    /// pruned from both the in-memory ledger and `billing_ledger`.
    pub pruned: bool,
    pub created_ms: u64,
}

/// Max function runtime (seconds) allowed on a plan. Enterprise unlocks 1 hour.
pub fn plan_max_duration_secs(plan: &str) -> u64 {
    match plan {
        "enterprise" => 3600,
        "pro" => 800,
        _ => 300,
    }
}

/// Automatic multi-region fail-over is an Enterprise capability.
pub fn plan_allows_failover(plan: &str) -> bool {
    plan == "enterprise"
}

/// Optional team/org SSO is an Enterprise capability.
pub fn plan_allows_sso(plan: &str) -> bool {
    plan == "enterprise"
}

/// SAML SSO configuration is Enterprise-only (org identity federation).
pub fn plan_allows_saml(plan: &str) -> bool {
    plan == "enterprise"
}
/// SCIM 2.0 directory sync is Enterprise-only.
pub fn plan_allows_scim(plan: &str) -> bool {
    plan == "enterprise"
}
/// Custom SIEM log streaming (audit → Datadog/Splunk/HTTP) is Enterprise-only.
pub fn plan_allows_siem(plan: &str) -> bool {
    plan == "enterprise"
}
/// Account-level IP blocking is Enterprise-only.
pub fn plan_allows_ip_blocking(plan: &str) -> bool {
    plan == "enterprise"
}
/// Org-wide Conformance is Enterprise-only.
pub fn plan_allows_conformance(plan: &str) -> bool {
    plan == "enterprise"
}
/// Deployment password / access protection is available from Pro up.
pub fn plan_allows_deploy_protection(plan: &str) -> bool {
    matches!(plan, "pro" | "enterprise")
}
/// Microfrontends composition is available from Pro up.
pub fn plan_allows_microfrontends(plan: &str) -> bool {
    matches!(plan, "pro" | "enterprise")
}
/// Request-log history older than 7 days is available from Pro up.
pub fn plan_allows_extended_logs(plan: &str) -> bool {
    matches!(plan, "pro" | "enterprise")
}
/// How far back `/v1/logs` may read, per plan. This is the SERVER-SIDE gate —
/// the UI offering a 30-day option is presentation, the refusal lives here.
pub fn plan_max_log_age_ms(plan: &str) -> u64 {
    if plan_allows_extended_logs(plan) {
        30 * crate::event_log::DAY_MS
    } else {
        7 * crate::event_log::DAY_MS
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BillingAccount {
    pub tenant: String,
    pub plan: String,
    pub status: String,
    pub included_cents: u64,
    pub used_cents: u64,
    /// Prepaid credit balance (pay-as-you-go top-ups), in cents.
    pub balance_cents: i64,
    #[serde(default)]
    pub stripe_customer: String,
    /// The Stripe Subscription id (`sub_...`) backing a paid plan, so a
    /// `customer.subscription.deleted`/`.updated` webhook can find its tenant without a separate
    /// index.
    #[serde(default)]
    pub stripe_subscription: String,
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    pub updated_ms: u64,
    /// Operator-set tier FLOOR: while `Some(plan)`, no automatic path (the free-plan checkout
    /// shortcut, Stripe `customer.subscription.deleted`, a period rollover) may move this tenant
    /// below that plan. A one-shot grant is not durable (a free-plan checkout once reverted only
    /// the billing half of an `enterprise` grant, leaving the account quota-locked while `c.teams`
    /// still read `enterprise`), so only an explicit operator call sets or clears it.
    #[serde(default)]
    pub tier_locked_plan: Option<String>,
    /// One-off addon charges (e.g. "Dedicated IPv4 — myapp") for the CURRENT period, folded into
    /// `build_invoice`'s output beside the subscription and usage lines and cleared on rollover
    /// right after the closing invoice, which still includes them, is built.
    #[serde(default)]
    pub addon_lines: Vec<InvoiceLine>,
}

impl BillingAccount {
    fn default_for(tenant: &str) -> Self {
        let now = now_ms();
        let p = plan_spec("hobby");
        BillingAccount {
            tenant: tenant.to_string(),
            plan: "hobby".into(),
            status: "active".into(),
            included_cents: p.included_cents,
            used_cents: 0,
            balance_cents: 0,
            stripe_customer: String::new(),
            stripe_subscription: String::new(),
            period_start_ms: now,
            period_end_ms: now + MONTH_MS,
            updated_ms: now,
            tier_locked_plan: None,
            addon_lines: Vec::new(),
        }
    }
    pub fn remaining_included(&self) -> u64 {
        self.included_cents.saturating_sub(self.used_cents)
    }
}

/// Rank a plan id so a tier floor can be compared. Unknown ids rank lowest,
/// matching `plan_spec`'s fail-toward-hobby behaviour.
pub fn plan_rank(plan: &str) -> u8 {
    match plan {
        "enterprise" => 3,
        "pro" => 2,
        "hobby" => 1,
        _ => 0,
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: String,
    pub tenant: String,
    pub ts_ms: u64,
    /// "credit" | "charge" | "plan_change" | "renewal"
    pub kind: String,
    pub amount_cents: i64,
    pub balance_after_cents: i64,
    pub note: String,
}

/// A pending checkout (mock or Stripe) awaiting confirmation.
#[derive(Clone, Serialize, Deserialize)]
pub struct Checkout {
    pub id: String,
    pub tenant: String,
    /// "plan" | "credits" | "addon"
    pub kind: String,
    pub plan: String,
    pub amount_cents: u64,
    /// The addon SKU (e.g. "dedicated_ipv4"). Empty unless `kind == "addon"`.
    #[serde(default)]
    pub sku: String,
    /// The addon's target resource id (a project name, for "dedicated_ipv4").
    /// Empty unless `kind == "addon"`.
    #[serde(default)]
    pub target: String,
    /// The REAL Stripe Checkout Session id (`cs_...`); empty for a mock checkout. Confirmation
    /// verifies actual payment with Stripe through it instead of trusting a client's say-so.
    #[serde(default)]
    pub stripe_session_id: String,
    pub created_ms: u64,
}

/// Per-tenant meter cursor: the last cumulative usage seen + the sub-cent remainder
/// carried between ticks so no usage is lost to rounding.
#[derive(Default)]
struct MeterState {
    last: UsageTotals,
    frac_cents: f64,
}

/// One tenant's meter cursor as it crosses the persistence boundary
/// (`meters_snapshot`/`meters_load`). It is the ONLY thing standing between a restart and a
/// re-bill: `meter_usage` charges `current - last`, and `last` once lived only in memory, so the
/// first tick after a restart re-charged the ENTIRE cumulative fleet total.
#[derive(Clone, Serialize, Deserialize)]
pub struct MeterWatermark {
    pub tenant: String,
    #[serde(default)]
    pub last: UsageTotals,
    /// Sub-cent remainder carried to the next tick, persisted with the watermark (dropping it loses
    /// up to 1 cent of accrued usage per tenant per restart).
    #[serde(default)]
    pub frac_cents: f64,
}

/// How long an unconfirmed checkout survives: Stripe expires a Checkout Session after 24 h, so an
/// older local record can never be confirmed. Needed because checkouts persist; the map used to be
/// cleared by every restart.
pub const CHECKOUT_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

/// What stands behind a checkout completing: [`BillingStore::confirm_checkout`] makes every caller name
/// it, so a new completion path cannot apply a purchase without saying what paid for it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Payment {
    /// Stripe vouches for it: the handler verified the session's payment with Stripe, or the delivery is
    /// a signature-verified Stripe webhook.
    Stripe,
    /// Nothing but the caller's say-so (a mock checkout): completes an amount of zero, or any amount on a
    /// node where [`mock_checkout_allowed`].
    Unproven,
}

/// Why [`BillingStore::confirm_checkout`] applied nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unconfirmed {
    /// No such open checkout: never opened, already completed, past [`CHECKOUT_RETENTION_MS`], or held by
    /// another node.
    Unknown,
    /// A payment is due and nothing proves it was made. The checkout stays open.
    Unpaid,
}

/// Largest amount, in cents, one credit, charge or checkout may carry ($10 billion). Amounts arrive
/// as `u64` in request bodies and live as `i64`: a `u64` above `i64::MAX` cast `as i64` is NEGATIVE,
/// so a debit of one used to CREDIT the account (`POST /v1/billing/charge`). Every entry point
/// bounds its amount by this before it reaches a balance, and the balance updates saturate, so
/// neither one hostile amount nor a long run of maximal ones can wrap a balance.
pub const MAX_AMOUNT_CENTS: u64 = 1_000_000_000_000;

/// Why [`BillingStore::charge`] debited nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChargeError {
    /// The amount is above [`MAX_AMOUNT_CENTS`].
    OutOfRange,
    /// Hobby (no overage) with neither allowance nor prepaid credits left for it.
    Insufficient,
}

#[derive(Default)]
pub struct BillingStore {
    accounts: RwLock<HashMap<String, BillingAccount>>,
    ledger: RwLock<Vec<LedgerEntry>>,
    checkouts: RwLock<HashMap<String, Checkout>>,
    meters: RwLock<HashMap<String, MeterState>>,
    invoices: RwLock<HashMap<String, Vec<Invoice>>>,
    /// Set when this process restored a snapshot with NO meters field (a pre-upgrade node's, or
    /// none): the watermarks are UNKNOWN, not zero, so a tenant's first reading is adopted as its
    /// baseline and charged nothing, like a counter decrease (`meter_usage`); charging from zero
    /// would re-bill the fleet's cumulative usage. `false` for a store that never loaded anything:
    /// a first reading is then genuinely first usage.
    meters_baseline_unknown: std::sync::atomic::AtomicBool,
}

impl BillingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or lazily create a Hobby) account for a tenant.
    ///
    /// A call after the period elapsed ROLLS it: finalizes the closing invoice (ledger checkpoint
    /// from the OLD `period_start_ms`), resets the counters and re-derives the allowance from the
    /// EFFECTIVE plan, never below an operator tier floor. The rollover holds the `accounts` write
    /// guard, so it may call only `usage_cost_cents_since` and `compute_ledger_checkpoint`, never
    /// `usage_cost_cents_for`, whose read lock deadlocks against it (a real leader hang once the
    /// relational mirror called `account()` per tenant per tick).
    pub fn account(&self, tenant: &str) -> BillingAccount {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let mut m = self.accounts.write();
        let acc = m
            .entry(tenant.to_string())
            .or_insert_with(|| BillingAccount::default_for(tenant));
        let now = now_ms();
        let rolled = if now >= acc.period_end_ms {
            let mut closing = build_invoice(
                acc,
                self.usage_cost_cents_since(tenant, acc.period_start_ms),
                "paid",
            );
            closing.ledger_checkpoint =
                Some(self.compute_ledger_checkpoint(tenant, acc.period_start_ms, now));
            acc.period_start_ms = now;
            acc.period_end_ms = now + MONTH_MS;
            acc.used_cents = 0;
            let effective = match acc.tier_locked_plan.as_deref() {
                Some(f) if plan_rank(f) > plan_rank(&acc.plan) => {
                    acc.plan = f.to_string();
                    f
                }
                _ => acc.plan.as_str(),
            };
            acc.included_cents = plan_spec(effective).included_cents;
            acc.updated_ms = now;
            acc.addon_lines.clear();
            Some(closing)
        } else {
            None
        };
        let out = acc.clone();
        drop(m);
        if let Some(inv) = rolled {
            self.invoices
                .write()
                .entry(out.tenant.clone())
                .or_default()
                .push(inv);
            self.ledger_push(
                &out.tenant,
                "renewal",
                0,
                out.balance_cents,
                "Monthly period renewed; invoice finalized",
            );
        }
        out
    }

    /// Sum of usage (non-plan) charges recorded THIS period, in cents; takes `self.accounts.read()`
    /// itself, so call it ONLY when no lock on `self.accounts` is held on this thread
    /// (parking_lot's RwLock is not re-entrant). `account()`'s rollover uses
    /// `usage_cost_cents_since`.
    fn usage_cost_cents_for(&self, tenant: &str) -> i64 {
        let acc_start = self
            .accounts
            .read()
            .get(tenant)
            .map(|a| a.period_start_ms)
            .unwrap_or(0);
        self.usage_cost_cents_since(tenant, acc_start)
    }

    /// Sum of usage (non-plan) charges recorded since `period_start_ms`, in cents (usage ledger
    /// entries are negative amounts, so the sum is negated). Touches only `self.ledger`, never
    /// `self.accounts`: safe to call while the caller already holds `self.accounts`'s write lock.
    fn usage_cost_cents_since(&self, tenant: &str, period_start_ms: u64) -> i64 {
        self.ledger
            .read()
            .iter()
            .filter(|e| e.tenant == tenant && e.kind == "usage" && e.ts_ms >= period_start_ms)
            .map(|e| -e.amount_cents)
            .sum()
    }

    /// Computes (never mutates) a tamper-evident SHA-256 hash-chain commitment over a tenant's raw
    /// ledger entries in `[period_start_ms, period_end_ms)`; touches only `self.ledger`, so it is
    /// safe under `self.accounts`'s write lock.
    ///
    /// Entries are sorted `(ts_ms, id)` first (the ledger is push-appended across tenants
    /// concurrently, and two computations of one period must agree); `0x1F` delimits fields
    /// unambiguously; the genesis hash is domain-separated by version + tenant + period start, so
    /// no two empty chains collide, and leaves a `v2` escape hatch.
    pub(crate) fn compute_ledger_checkpoint(
        &self,
        tenant: &str,
        period_start_ms: u64,
        period_end_ms: u64,
    ) -> LedgerCheckpoint {
        const SEP: u8 = 0x1F;
        let mut entries: Vec<LedgerEntry> = self
            .ledger
            .read()
            .iter()
            .filter(|e| e.tenant == tenant && e.ts_ms >= period_start_ms && e.ts_ms < period_end_ms)
            .cloned()
            .collect();
        entries.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms).then_with(|| a.id.cmp(&b.id)));

        let mut chain = Sha256::new();
        chain.update(b"hive-billing-ledger-checkpoint-v1");
        chain.update([SEP]);
        chain.update(tenant.as_bytes());
        chain.update([SEP]);
        chain.update(period_start_ms.to_string().as_bytes());
        let mut chain = chain.finalize();

        for e in &entries {
            let mut leaf = Sha256::new();
            leaf.update(e.id.as_bytes());
            leaf.update([SEP]);
            leaf.update(e.tenant.as_bytes());
            leaf.update([SEP]);
            leaf.update(e.ts_ms.to_string().as_bytes());
            leaf.update([SEP]);
            leaf.update(e.kind.as_bytes());
            leaf.update([SEP]);
            leaf.update(e.amount_cents.to_string().as_bytes());
            leaf.update([SEP]);
            leaf.update(e.balance_after_cents.to_string().as_bytes());
            leaf.update([SEP]);
            leaf.update(e.note.as_bytes());
            let leaf = leaf.finalize();

            let mut next = Sha256::new();
            next.update(chain);
            next.update([SEP]);
            next.update(leaf);
            chain = next.finalize();
        }

        LedgerCheckpoint {
            tenant: tenant.to_string(),
            period_start_ms,
            period_end_ms,
            entry_count: entries.len() as u64,
            commitment: hex::encode(chain),
            pruned: false,
            created_ms: now_ms(),
        }
    }

    /// Remove raw entries for tenant/period from the IN-MEMORY ledger only —
    /// the relational side is pruned separately via
    /// `relational::prune_billing_ledger`. Returns the count removed.
    pub(crate) fn prune_ledger_range(
        &self,
        tenant: &str,
        period_start_ms: u64,
        period_end_ms: u64,
    ) -> usize {
        let mut g = self.ledger.write();
        let before = g.len();
        g.retain(|e| {
            !(e.tenant == tenant && e.ts_ms >= period_start_ms && e.ts_ms < period_end_ms)
        });
        before - g.len()
    }

    /// Marks the in-memory `Invoice` whose checkpoint covers `[period_start_ms, period_end_ms)` as
    /// pruned, so a restart's `invoices_load` does not re-attempt the completed relational delete
    /// every tick.
    pub(crate) fn mark_checkpoint_pruned(&self, tenant: &str, period_start_ms: u64) {
        if let Some(invs) = self.invoices.write().get_mut(tenant) {
            for inv in invs.iter_mut() {
                if let Some(cp) = inv.ledger_checkpoint.as_mut() {
                    if cp.period_start_ms == period_start_ms {
                        cp.pruned = true;
                    }
                }
            }
        }
    }

    /// Record metered compute usage (a fact — always accrues, never rejected). Uses
    /// the monthly included allowance first, then prepaid balance (may go negative;
    /// the create-time gate handles Hobby lock-out separately).
    fn record_usage(&self, tenant: &str, cents: u64, note: &str) {
        if cents == 0 {
            return;
        }
        let bal_after;
        {
            let mut m = self.accounts.write();
            let acc = m
                .entry(tenant.to_string())
                .or_insert_with(|| BillingAccount::default_for(tenant));
            let included_left = acc.included_cents.saturating_sub(acc.used_cents);
            let from_included = cents.min(included_left);
            acc.used_cents += from_included;
            let rest = cents - from_included;
            if rest > 0 {
                acc.balance_cents -= rest as i64;
            }
            acc.updated_ms = now_ms();
            bal_after = acc.balance_cents;
        }
        self.ledger_push(tenant, "usage", -(cents as i64), bal_after, note);
    }

    /// Feeds the latest cumulative usage totals for a tenant and charges the delta since the last
    /// reading, carrying the sub-cent remainder; returns cents charged.
    ///
    /// The counters are CUMULATIVE and summed across the fleet (`fleet_function_stats`), so `cur <
    /// last` is a counter RESET (a node restarted), not usage: it re-baselines, charging nothing.
    /// Charging `cur` re-billed the ENTIRE fleet's usage on every node restart (a tenant hit -$55
    /// against a $5 allowance during a rolling upgrade and tripped the `can_deploy` credit lock);
    /// under-counting is the right direction to err. A tenant with no cursor in a process booted
    /// without persisted meters is treated the same (`meters_baseline_unknown`).
    pub fn meter_usage(&self, tenant: &str, current: UsageTotals) -> u64 {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let (delta, carry) = {
            let mut meters = self.meters.write();
            let seeding = self
                .meters_baseline_unknown
                .load(std::sync::atomic::Ordering::Relaxed)
                && !meters.contains_key(tenant);
            let st = meters.entry(tenant.to_string()).or_default();
            let sub = |cur: u64, last: u64| if cur >= last { cur - last } else { 0 };
            let delta = if seeding {
                tracing::info!(
                    tenant,
                    "billing meter: no persisted watermark for this tenant — adopting the live \
                     counters as its baseline and charging nothing this tick"
                );
                UsageTotals::default()
            } else {
                UsageTotals {
                    active_cpu_ms: sub(current.active_cpu_ms, st.last.active_cpu_ms),
                    mem_gb_hr_milli: sub(current.mem_gb_hr_milli, st.last.mem_gb_hr_milli),
                    requests: sub(current.requests, st.last.requests),
                    blocked: sub(current.blocked, st.last.blocked),
                    gpu_ms: sub(current.gpu_ms, st.last.gpu_ms),
                    browser_requests: sub(current.browser_requests, st.last.browser_requests),
                }
            };
            st.last = current;
            (delta, st.frac_cents)
        };
        let cost = RATE_CARD.cost_cents(&delta) + carry;
        let whole = cost.floor().max(0.0) as u64;
        {
            let mut meters = self.meters.write();
            if let Some(st) = meters.get_mut(tenant) {
                st.frac_cents = cost - whole as f64;
            }
        }
        if whole > 0 {
            self.record_usage(tenant, whole, "Compute usage (metered)");
        }
        whole
    }

    /// The current (in-progress) draft invoice for a tenant.
    pub fn current_invoice(&self, tenant: &str) -> Invoice {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let acc = self.account(tenant);
        build_invoice(&acc, self.usage_cost_cents_for(tenant), "draft")
    }

    /// Finalized invoices for a tenant (newest first) plus the current draft.
    pub fn invoices(&self, tenant: &str) -> Vec<Invoice> {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let mut v = self
            .invoices
            .read()
            .get(tenant)
            .cloned()
            .unwrap_or_default();
        v.push(self.current_invoice(tenant));
        v.sort_by(|a, b| b.period_start_ms.cmp(&a.period_start_ms));
        v
    }

    /// Finalized (`status == "paid"`) invoices for a tenant as stored, WITHOUT the in-progress
    /// draft that `invoices()` appends; `self.invoices` only holds what `account()`'s rollover
    /// pushed, so it is finalized-only by construction, and the relational mirror
    /// (`relational::upsert_billing`) must NEVER persist a draft.
    pub fn finalized_invoices(&self, tenant: &str) -> Vec<Invoice> {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        self.invoices
            .read()
            .get(tenant)
            .cloned()
            .unwrap_or_default()
    }

    fn ledger_push(
        &self,
        tenant: &str,
        kind: &str,
        amount: i64,
        balance_after: i64,
        note: &str,
    ) -> LedgerEntry {
        let e = LedgerEntry {
            id: format!("led_{}", &Uuid::new_v4().simple().to_string()[..12]),
            tenant: tenant.to_string(),
            ts_ms: now_ms(),
            kind: kind.to_string(),
            amount_cents: amount,
            balance_after_cents: balance_after,
            note: note.to_string(),
        };
        self.ledger.write().push(e.clone());
        e
    }

    /// Sets (or clears, with `None`) a tenant's operator tier FLOOR and raises the live plan to it
    /// at once (through `set_plan`), so the lock and the served tier cannot disagree. Operator-only
    /// by construction: the only caller is the admin grant path.
    pub fn set_tier_lock(&self, tenant: &str, floor: Option<&str>) -> BillingAccount {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        {
            let mut m = self.accounts.write();
            let acc = m
                .entry(tenant.to_string())
                .or_insert_with(|| BillingAccount::default_for(tenant));
            acc.tier_locked_plan = floor.map(|s| s.to_string());
            acc.updated_ms = now_ms();
        }
        match floor {
            Some(f) if plan_rank(f) > plan_rank(&self.account(tenant).plan) => {
                self.set_plan(tenant, f)
            }
            _ => self.account(tenant),
        }
    }

    /// Sets a tenant's plan, never below its operator tier floor: every automatic downgrade path
    /// (free-plan checkout shortcut, `customer.subscription.deleted`, checkout confirmation)
    /// funnels through here, so the floor is enforced at this ONE chokepoint and `set_tier_lock` is
    /// the only way past it.
    pub fn set_plan(&self, tenant: &str, plan: &str) -> BillingAccount {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let locked = self
            .accounts
            .read()
            .get(tenant)
            .and_then(|a| a.tier_locked_plan.clone());
        let mut plan = plan;
        if let Some(floor) = locked.as_deref() {
            if plan_rank(plan) < plan_rank(floor) {
                tracing::warn!(
                    tenant,
                    attempted = plan,
                    floor,
                    "refusing tier downgrade below operator-set floor"
                );
                plan = floor;
            }
        }
        let spec = plan_spec(plan);
        {
            let mut m = self.accounts.write();
            let acc = m
                .entry(tenant.to_string())
                .or_insert_with(|| BillingAccount::default_for(tenant));
            acc.plan = spec.id.to_string();
            acc.included_cents = spec.included_cents;
            acc.updated_ms = now_ms();
        }
        let bal = self.account(tenant).balance_cents;
        self.ledger_push(
            tenant,
            "plan_change",
            spec.price_cents as i64,
            bal,
            &format!("Switched to {} plan", spec.name),
        );
        self.account(tenant)
    }

    pub fn add_credits(&self, tenant: &str, cents: u64, note: &str) -> BillingAccount {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let credit = i64::try_from(cents).unwrap_or(i64::MAX);
        let bal;
        {
            let mut m = self.accounts.write();
            let acc = m
                .entry(tenant.to_string())
                .or_insert_with(|| BillingAccount::default_for(tenant));
            acc.balance_cents = acc.balance_cents.saturating_add(credit);
            acc.updated_ms = now_ms();
            bal = acc.balance_cents;
        }
        self.ledger_push(tenant, "credit", credit, bal, note);
        self.account(tenant)
    }

    /// Records a one-off addon charge (e.g. dedicated-IPv4 provisioning) as an `InvoiceLine` on
    /// `BillingAccount::addon_lines`, which `build_invoice` folds into the live draft and the
    /// finalized invoice. A ledger entry is logged for visibility only: `balance_after_cents` is
    /// unaffected, since an addon is billed on the invoice, not drawn from prepaid balance.
    pub fn record_addon_charge(&self, tenant: &str, description: &str, amount_cents: i64) {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let bal;
        {
            let mut m = self.accounts.write();
            let acc = m
                .entry(tenant.to_string())
                .or_insert_with(|| BillingAccount::default_for(tenant));
            acc.addon_lines.push(InvoiceLine {
                description: description.to_string(),
                amount_cents,
            });
            acc.updated_ms = now_ms();
            bal = acc.balance_cents;
        }
        self.ledger_push(tenant, "addon_charge", amount_cents, bal, description);
    }

    /// Charge compute usage: consume the monthly included allowance first, then
    /// prepaid balance. On Hobby (no overage) a charge that would go negative is
    /// rejected ([`ChargeError::Insufficient`]); an amount above [`MAX_AMOUNT_CENTS`] is refused
    /// before it touches the account ([`ChargeError::OutOfRange`]): `cents` comes straight from a
    /// tenant's request body and, unbounded, `as i64` turned one above `i64::MAX` into a CREDIT.
    pub fn charge(
        &self,
        tenant: &str,
        cents: u64,
        note: &str,
    ) -> Result<BillingAccount, ChargeError> {
        if cents > MAX_AMOUNT_CENTS {
            return Err(ChargeError::OutOfRange);
        }
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let bal_after;
        {
            let mut m = self.accounts.write();
            let acc = m
                .entry(tenant.to_string())
                .or_insert_with(|| BillingAccount::default_for(tenant));
            let spec = plan_spec(&acc.plan);
            let included_left = acc.included_cents.saturating_sub(acc.used_cents);
            let from_included = cents.min(included_left);
            let owed = (cents - from_included) as i64;
            if owed > 0 && acc.balance_cents < owed && !spec.overage {
                return Err(ChargeError::Insufficient);
            }
            acc.used_cents += from_included;
            acc.balance_cents = acc.balance_cents.saturating_sub(owed);
            acc.updated_ms = now_ms();
            bal_after = acc.balance_cents;
        }
        self.ledger_push(tenant, "charge", -(cents as i64), bal_after, note);
        Ok(self.account(tenant))
    }

    /// Business lock: can this tenant start a new deployment? Pro/Enterprise always
    /// can (pay-as-you-go). Hobby is blocked once the monthly included compute is
    /// exhausted AND there's no prepaid balance — the usage→limit→lock tie-in.
    pub fn can_deploy(&self, tenant: &str) -> Result<(), String> {
        let acc = self.account(tenant);
        let spec = plan_spec(&acc.plan);
        if spec.overage {
            return Ok(());
        }
        if acc.remaining_included() == 0 && acc.balance_cents <= 0 {
            return Err(
                "Monthly included compute exhausted — upgrade to Pro or add credits to deploy."
                    .into(),
            );
        }
        Ok(())
    }

    pub fn ledger(&self, tenant: &str) -> Vec<LedgerEntry> {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let mut v: Vec<LedgerEntry> = self
            .ledger
            .read()
            .iter()
            .filter(|e| e.tenant == tenant)
            .cloned()
            .collect();
        v.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
        v
    }

    /// Checkouts currently open (not yet confirmed) for a tenant, for the relational mirror's
    /// `billing_checkouts`.
    pub fn checkouts_for_tenant(&self, tenant: &str) -> Vec<Checkout> {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        self.checkouts
            .read()
            .values()
            .filter(|c| c.tenant == tenant)
            .cloned()
            .collect()
    }

    pub fn open_checkout(
        &self,
        tenant: &str,
        kind: &str,
        plan: &str,
        amount_cents: u64,
    ) -> Checkout {
        self.open_checkout_full(tenant, kind, plan, amount_cents, "", "")
    }

    /// Same as [`Self::open_checkout`], carrying an addon SKU + target
    /// (project id) — the fields `kind == "addon"` needs to resolve pricing
    /// and dispatch provisioning on confirmation. Empty for "plan"/"credits".
    pub fn open_checkout_full(
        &self,
        tenant: &str,
        kind: &str,
        plan: &str,
        amount_cents: u64,
        sku: &str,
        target: &str,
    ) -> Checkout {
        let c = Checkout {
            id: format!("co_{}", &Uuid::new_v4().simple().to_string()[..18]),
            tenant: tenant.to_string(),
            kind: kind.to_string(),
            plan: plan.to_string(),
            amount_cents,
            sku: sku.to_string(),
            target: target.to_string(),
            stripe_session_id: String::new(),
            created_ms: now_ms(),
        };
        self.checkouts.write().insert(c.id.clone(), c.clone());
        c
    }

    /// Record the REAL Stripe Checkout Session id for a just-created local
    /// checkout (called right after the Stripe API call succeeds).
    pub fn attach_stripe_session(&self, checkout_id: &str, stripe_session_id: &str) {
        if let Some(co) = self.checkouts.write().get_mut(checkout_id) {
            co.stripe_session_id = stripe_session_id.to_string();
        }
    }

    /// Persists the Stripe customer/subscription ids backing a tenant's paid plan; an empty string
    /// is a no-op, so an event whose payload omits an id never overwrites a known one.
    pub fn set_stripe_ids(&self, tenant: &str, customer: &str, subscription: &str) {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let mut m = self.accounts.write();
        let acc = m
            .entry(tenant.to_string())
            .or_insert_with(|| BillingAccount::default_for(tenant));
        if !customer.is_empty() {
            acc.stripe_customer = customer.to_string();
        }
        if !subscription.is_empty() {
            acc.stripe_subscription = subscription.to_string();
        }
        acc.updated_ms = now_ms();
    }

    /// Find the tenant whose account is backed by a given Stripe subscription
    /// id — used by the `customer.subscription.deleted`/`.updated` webhook,
    /// which identifies the subscription but not our tenant slug directly.
    pub fn tenant_for_subscription(&self, subscription_id: &str) -> Option<String> {
        self.accounts
            .read()
            .values()
            .find(|a| a.stripe_subscription == subscription_id)
            .map(|a| a.tenant.clone())
    }

    pub fn get_checkout(&self, id: &str) -> Option<Checkout> {
        self.checkouts.read().get(id).cloned()
    }

    /// Drops an open checkout the payment provider never accepted. Nothing was charged, so unlike
    /// [`Self::confirm_checkout`] nothing is applied.
    pub fn discard_checkout(&self, id: &str) {
        self.checkouts.write().remove(id);
    }

    /// Drops a tenant's billing account (its team was deleted). The LEDGER is left intact on
    /// purpose: it is the financial record and must outlive the account.
    pub fn remove_account(&self, tenant: &str) -> Option<BillingAccount> {
        self.accounts.write().remove(tenant)
    }

    /// Completes a checkout (idempotent: it consumes the session) once `payment` stands behind it.
    /// A CREDIT top-up is applied here; a PLAN checkout deliberately is NOT: the caller applies it
    /// through `admin::apply_plan_everywhere`, which writes BOTH the billing account and the team
    /// record and audits it (this used to call `set_plan` itself, leaving a stale team record and no
    /// `plan_change` audit entry), and an addon is provisioned by the caller too. One writer only.
    ///
    /// This is the ONE place a purchase becomes applied, so it is where "paid for" is decided: an
    /// [`Payment::Unproven`] checkout with an amount due completes only where
    /// [`mock_checkout_allowed`], whoever the caller is and whatever state a persisted or replicated
    /// record is in. It is refused ([`Unconfirmed::Unpaid`]) without being consumed.
    pub fn confirm_checkout(
        &self,
        id: &str,
        payment: Payment,
    ) -> Result<(Checkout, BillingAccount), Unconfirmed> {
        let c = {
            let mut open = self.checkouts.write();
            let due = open.get(id).ok_or(Unconfirmed::Unknown)?;
            if payment == Payment::Unproven && due.amount_cents > 0 && !mock_checkout_allowed() {
                return Err(Unconfirmed::Unpaid);
            }
            open.remove(id).ok_or(Unconfirmed::Unknown)?
        };
        let acc = match c.kind.as_str() {
            "credits" => self.add_credits(&c.tenant, c.amount_cents, "Credit top-up (checkout)"),
            _ => self.account(&c.tenant),
        };
        Ok((c, acc))
    }

    /// Persistence comes in FOUR pairs: `snapshot`/`load` (accounts + ledger),
    /// `invoices_snapshot`/`invoices_load`, `meters_snapshot`/`meters_load` and
    /// `checkouts_snapshot`/`checkouts_load`. Every field of `BillingStore` is covered by exactly
    /// one: a field added without a pair silently dies at the next restart or failover.
    pub fn snapshot(&self) -> (Vec<BillingAccount>, Vec<LedgerEntry>) {
        (
            self.accounts.read().values().cloned().collect(),
            self.ledger.read().clone(),
        )
    }
    pub fn load(&self, accounts: Vec<BillingAccount>, ledger: Vec<LedgerEntry>) {
        *self.accounts.write() = accounts
            .into_iter()
            .map(|a| (a.tenant.clone(), a))
            .collect();
        *self.ledger.write() = ledger;
    }
    /// Per-tenant meter watermarks for persistence, tenant-sorted so the bytes are deterministic
    /// for byte-compare change gates.
    ///
    /// A SEPARATE pair from `snapshot()`/`load()` on purpose: that tuple is the wire shape
    /// `store_sync`'s `billing` entry encodes, and a node on the previous binary decodes it as
    /// `(Map, Vec)`, which fails on a 3-element array, so widening it would stop billing
    /// replicating to every not-yet-upgraded node for the length of a rollout.
    pub fn meters_snapshot(&self) -> Vec<MeterWatermark> {
        let m = self.meters.read();
        let mut v: Vec<MeterWatermark> = m
            .iter()
            .map(|(tenant, st)| MeterWatermark {
                tenant: tenant.clone(),
                last: st.last,
                frac_cents: st.frac_cents,
            })
            .collect();
        v.sort_by(|a, b| a.tenant.cmp(&b.tenant));
        v
    }

    /// Restores the meter watermarks. `None` (no meters field in the snapshot) means UNKNOWN, not
    /// zero (`meters_baseline_unknown`); `Some(vec![])` is a real empty set, so a tenant absent
    /// from a PRESENT list is new and bills from zero.
    pub fn meters_load(&self, meters: Option<Vec<MeterWatermark>>) {
        let Some(list) = meters else {
            self.meters_baseline_unknown
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                "billing meters: snapshot carried no watermarks — treating every tenant's meter \
                 baseline as UNKNOWN (first reading re-baselines, charges nothing) rather than zero"
            );
            return;
        };
        let n = list.len();
        *self.meters.write() = list
            .into_iter()
            .map(|w| {
                (
                    w.tenant,
                    MeterState {
                        last: w.last,
                        frac_cents: w.frac_cents,
                    },
                )
            })
            .collect();
        self.meters_baseline_unknown
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if n > 0 {
            tracing::info!(tenants = n, "billing meters: restored usage watermarks");
        }
    }

    /// Open (unconfirmed) checkouts across all tenants, for persistence.
    /// Age-filtered: a session past [`CHECKOUT_RETENTION_MS`] can no longer be
    /// confirmed at Stripe, so persisting it would only grow the map forever.
    pub fn checkouts_snapshot(&self) -> Vec<Checkout> {
        let now = now_ms();
        let mut v: Vec<Checkout> = self
            .checkouts
            .read()
            .values()
            .filter(|c| now.saturating_sub(c.created_ms) < CHECKOUT_RETENTION_MS)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Restore open checkouts. The retention bound is enforced HERE as well as
    /// in `checkouts_snapshot`, so no on-disk file (hand-edited, or written by
    /// a build with a different bound) can reload an unbounded set.
    pub fn checkouts_load(&self, checkouts: Vec<Checkout>) {
        let now = now_ms();
        *self.checkouts.write() = checkouts
            .into_iter()
            .filter(|c| now.saturating_sub(c.created_ms) < CHECKOUT_RETENTION_MS)
            .map(|c| (c.id.clone(), c))
            .collect();
    }

    /// Finalized invoices across all tenants (for persistence).
    pub fn invoices_snapshot(&self) -> Vec<Invoice> {
        self.invoices.read().values().flatten().cloned().collect()
    }
    pub fn invoices_load(&self, invoices: Vec<Invoice>) {
        let mut m: HashMap<String, Vec<Invoice>> = HashMap::new();
        for inv in invoices {
            m.entry(inv.tenant.clone()).or_default().push(inv);
        }
        *self.invoices.write() = m;
    }
    pub fn all_accounts(&self) -> Vec<BillingAccount> {
        self.accounts.read().values().cloned().collect()
    }
}

/// Build an invoice from an account's current period: subscription fee + metered
/// compute usage. `usage_cents` is the total usage recorded this period.
fn build_invoice(acc: &BillingAccount, usage_cents: i64, status: &str) -> Invoice {
    let spec = plan_spec(&acc.plan);
    let mut lines = vec![InvoiceLine {
        description: format!("{} plan — monthly subscription", spec.name),
        amount_cents: spec.price_cents as i64,
    }];
    if usage_cents > 0 {
        let included = spec.included_cents as i64;
        let covered = usage_cents.min(included);
        lines.push(InvoiceLine {
            description: "Compute usage (metered)".into(),
            amount_cents: usage_cents,
        });
        if covered > 0 {
            lines.push(InvoiceLine {
                description: format!(
                    "Included allowance credit (${:.2})",
                    included as f64 / 100.0
                ),
                amount_cents: -covered,
            });
        }
    }
    lines.extend(acc.addon_lines.iter().cloned());
    let subtotal: i64 = lines.iter().map(|l| l.amount_cents).sum();
    let num = format!(
        "INV-{}",
        &Uuid::new_v4().simple().to_string()[..8].to_uppercase()
    );
    Invoice {
        id: format!("in_{}", &Uuid::new_v4().simple().to_string()[..18]),
        number: num,
        tenant: acc.tenant.clone(),
        plan: acc.plan.clone(),
        period_start_ms: acc.period_start_ms,
        period_end_ms: acc.period_end_ms,
        lines,
        subtotal_cents: subtotal,
        total_cents: subtotal,
        status: status.to_string(),
        created_ms: now_ms(),
        ledger_checkpoint: None,
    }
}

pub fn stripe_configured() -> bool {
    std::env::var("STRIPE_SECRET_KEY")
        .map(|k| !k.is_empty())
        .unwrap_or(false)
}

/// Whether a checkout may complete with no payment behind it (the local "mock" checkout of a
/// developer's machine): only on a node with NO Stripe key and NO identity layer
/// (`auth::enforced()` false). With a Stripe key a checkout completes only through a verified Stripe
/// session, and on an enforced platform without one billing is simply not set up: a paid checkout is
/// refused, never mocked. One predicate for what may complete ([`BillingStore::confirm_checkout`])
/// and what may open (`admin::billing_checkout`).
pub fn mock_checkout_allowed() -> bool {
    !stripe_configured() && !crate::auth::enforced()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_plan_prices_and_stripe_price_ids_are_wired() {
        // REGRESSION TEST: Pro and Enterprise must carry the REAL Stripe price
        // ids (not ad-hoc price_data) and their amounts must match what those
        // real prices actually charge — a mismatch here would mean the
        // dashboard displays one number while Stripe charges another.
        let pro = plan_spec("pro");
        assert_eq!(pro.stripe_price_id, Some("price_1TqK9jRj0yq8r94Ek8Gjgl7d"));
        assert_eq!(
            pro.price_cents, 2000,
            "Pro must be $20/mo, matching the real Stripe price"
        );

        let ent = plan_spec("enterprise");
        assert_eq!(ent.stripe_price_id, Some("price_1TqKCGRj0yq8r94E4B8VhCC3"));
        assert_eq!(
            ent.price_cents, 100_000,
            "Enterprise must be $1,000/mo, matching the real Stripe price"
        );

        // Hobby has no paid subscription — must never reference a Stripe price.
        assert_eq!(plan_spec("hobby").stripe_price_id, None);
    }

    #[test]
    fn webhook_signature_verifies_a_real_hmac_and_rejects_tampering() {
        // Independent HMAC-SHA256 computation (not reusing the crate's own
        // `hmac_sha256_hex` helper) so this test can't pass merely because
        // both sides share the same bug.
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let secret = "whsec_test_fixture_secret";
        let payload = br#"{"id":"evt_test","type":"checkout.session.completed"}"#;
        let ts = (now_ms() / 1000) as i64;
        let mut signed = format!("{ts}.").into_bytes();
        signed.extend_from_slice(payload);
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&signed);
        let sig_hex: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let header = format!("t={ts},v1={sig_hex}");
        assert!(
            verify_webhook_signature(payload, &header, secret),
            "a genuinely valid signature must verify"
        );

        // Wrong secret.
        assert!(!verify_webhook_signature(
            payload,
            &header,
            "whsec_wrong_secret"
        ));
        // Tampered payload (the classic webhook-forgery attempt).
        assert!(!verify_webhook_signature(b"{}", &header, secret));
        // Tampered signature.
        let bad_header =
            format!("t={ts},v1=0000000000000000000000000000000000000000000000000000000000000000");
        assert!(!verify_webhook_signature(payload, &bad_header, secret));
        // Stale timestamp (older than Stripe's 5-minute tolerance) — replay defense.
        let stale_header = format!("t={},v1={sig_hex}", ts - 301);
        assert!(
            !verify_webhook_signature(payload, &stale_header, secret),
            "a >5-minute-old signature must be rejected"
        );
        // Missing v1 entirely.
        assert!(!verify_webhook_signature(
            payload,
            &format!("t={ts}"),
            secret
        ));
        // Garbage header.
        assert!(!verify_webhook_signature(
            payload,
            "not-a-real-header",
            secret
        ));
    }

    #[test]
    fn checkout_confirmation_requires_a_stripe_session_id_to_carry_payment_state() {
        // REGRESSION TEST for a real, confirmed payment-integrity gap: the
        // original `confirm_checkout` applied a plan/credits unconditionally
        // for ANY checkout id — a caller could open a checkout and immediately
        // confirm it without ever paying, since nothing checked with Stripe.
        // The fix moved verification into the `billing_confirm` HANDLER (it
        // needs the shared `reqwest::Client` + tenant-ownership context this
        // store-level test doesn't have), gated on `Checkout.stripe_session_id`
        // being non-empty. This test locks in the STORE-level contract the fix
        // depends on: a checkout opened while Stripe is configured must carry
        // a real session id once `attach_stripe_session` runs, and the field
        // must default empty (mock mode) so the handler knows to skip
        // Stripe verification there instead of failing closed on nothing to verify.
        let s = BillingStore::new();
        let co = s.open_checkout("acme", "plan", "pro", 2000);
        assert!(
            co.stripe_session_id.is_empty(),
            "a freshly-opened checkout has no Stripe session yet"
        );
        s.attach_stripe_session(&co.id, "cs_test_realsession123");
        let refreshed = s.get_checkout(&co.id).unwrap();
        assert_eq!(refreshed.stripe_session_id, "cs_test_realsession123");
    }

    #[test]
    fn stripe_ids_persist_and_resolve_subscription_to_tenant() {
        let s = BillingStore::new();
        s.set_plan("acme", "pro");
        s.set_stripe_ids("acme", "cus_realcustomer", "sub_realsubscription");
        let acc = s.account("acme");
        assert_eq!(acc.stripe_customer, "cus_realcustomer");
        assert_eq!(acc.stripe_subscription, "sub_realsubscription");
        assert_eq!(
            s.tenant_for_subscription("sub_realsubscription").as_deref(),
            Some("acme")
        );
        assert_eq!(s.tenant_for_subscription("sub_doesnotexist"), None);

        // An empty id in a later call must NOT clobber a known one (a webhook
        // payload that omits a field must not erase state a previous, more
        // complete event already recorded).
        s.set_stripe_ids("acme", "", "");
        assert_eq!(s.account("acme").stripe_customer, "cus_realcustomer");
    }

    #[test]
    fn checkout_confirmation_is_scoped_to_a_single_tenant() {
        // Complements the tenant-ownership check enforced in the
        // `billing_confirm` HANDLER (which this store-level test can't reach
        // directly) by confirming the data the handler checks against is
        // actually present and correct: `Checkout.tenant` is exactly the
        // opening tenant, never inferable/overridable from the confirm call.
        let s = BillingStore::new();
        let co = s.open_checkout("victim-team", "plan", "enterprise", 100_000);
        assert_eq!(co.tenant, "victim-team");
        let fetched = s.get_checkout(&co.id).unwrap();
        assert_eq!(
            fetched.tenant, "victim-team",
            "the handler's tenant-ownership check depends on this being immutable"
        );
    }

    #[test]
    fn rate_card_cost_matches_ui() {
        // 1 CPU-hr = $0.128 = 12.8¢; 1 GB-hr = 1.06¢; 1M req = $2 = 200¢.
        let u = UsageTotals {
            active_cpu_ms: 3_600_000,
            mem_gb_hr_milli: 1000,
            requests: 1_000_000,
            blocked: 0,
            gpu_ms: 0,
            browser_requests: 0,
        };
        let c = RATE_CARD.cost_cents(&u);
        assert!((c - (12.8 + 1.06 + 200.0)).abs() < 1e-6, "got {c}");
    }

    #[test]
    fn meter_charges_delta_and_carries_fraction() {
        let s = BillingStore::new();
        s.set_plan("acme", "pro"); // overage plan so balance can go negative
                                   // First reading: 0.5¢ worth (below a whole cent) → nothing charged yet.
        let half = UsageTotals {
            active_cpu_ms: (3_600_000.0 * 0.5 / 12.8) as u64,
            ..Default::default()
        };
        let c1 = s.meter_usage("acme", half);
        assert_eq!(c1, 0, "sub-cent usage should not charge yet");
        // Cumulative grows to ~1.5¢ → the crossed whole cent (1¢) is charged.
        let onefive = UsageTotals {
            active_cpu_ms: (3_600_000.0 * 1.5 / 12.8) as u64,
            ..Default::default()
        };
        let c2 = s.meter_usage("acme", onefive);
        assert!(c2 >= 1, "should charge the crossed whole cent, got {c2}");
        // The usage is recorded in the ledger as a negative "usage" entry.
        assert!(s
            .ledger("acme")
            .iter()
            .any(|e| e.kind == "usage" && e.amount_cents < 0));
    }

    #[test]
    fn meter_handles_counter_reset() {
        let s = BillingStore::new();
        s.set_plan("acme", "pro");
        let big = UsageTotals {
            active_cpu_ms: 3_600_000 * 10,
            ..Default::default()
        }; // 128¢
        s.meter_usage("acme", big);
        // Node restarts → counter drops to a small value; must not underflow/panic and
        // should charge the fresh (small) amount, not a huge delta.
        let small = UsageTotals {
            active_cpu_ms: 3_600_000,
            ..Default::default()
        }; // 12.8¢
        let c = s.meter_usage("acme", small);
        assert!(
            c <= 13,
            "reset should charge only the fresh amount, got {c}"
        );
    }

    #[test]
    fn hobby_deploy_gate_locks_when_exhausted() {
        let s = BillingStore::new();
        // Fresh hobby account can deploy.
        assert!(s.can_deploy("h").is_ok());
        // Consume just past the $5 (500¢) included allowance (small overage).
        let over = UsageTotals {
            active_cpu_ms: (3_600_000.0 * 520.0 / 12.8) as u64,
            ..Default::default()
        };
        s.meter_usage("h", over);
        // Now included is gone and balance is negative → deploys are locked.
        assert!(s.can_deploy("h").is_err());
        // Topping up enough to clear the overage unlocks it.
        s.add_credits("h", 500, "top up");
        assert!(s.can_deploy("h").is_ok());
    }

    #[test]
    fn invoice_has_subscription_and_usage_lines() {
        let s = BillingStore::new();
        s.set_plan("acme", "pro");
        let u = UsageTotals {
            active_cpu_ms: 3_600_000 * 5,
            ..Default::default()
        }; // 64¢
        s.meter_usage("acme", u);
        let inv = s.current_invoice("acme");
        assert_eq!(inv.status, "draft");
        assert!(inv
            .lines
            .iter()
            .any(|l| l.description.contains("subscription")));
        assert!(inv
            .lines
            .iter()
            .any(|l| l.description.contains("Compute usage")));
        // Pro subscription is $20 = 2000¢; total is subtotal of all lines.
        assert!(inv.total_cents >= 2000);
    }

    #[test]
    fn plan_quotas_are_wired() {
        // Assert the SHAPE (bounded, and strictly below Pro), not the exact
        // numbers: pinning literals here made every deliberate limit change look
        // like a regression, which is the opposite of what this test is for.
        // 0 means unlimited, so an enterprise check is an equality on 0.
        assert!(plan_max_members("hobby") > 0);
        assert!(plan_max_members("hobby") < plan_max_members("pro"));
        assert_eq!(plan_max_members("enterprise"), 0); // unlimited
        assert!(plan_max_projects("hobby") > 0);
        assert!(plan_max_projects("hobby") < plan_max_projects("pro"));
        assert_eq!(plan_max_projects("enterprise"), 0);
        // Hobby must never gain overage billing — it is the no-payment tier.
        assert!(!plan_spec("hobby").overage);
        assert!(plan_spec("pro").overage);
    }
}

/// Where the Stripe API lives: `https://api.stripe.com`, or `STRIPE_API_BASE` for a stand-in (a
/// stripe-mock process, an egress proxy, a witness). The secret key rides every request as basic
/// auth, so only `https://`, or `http://` to a loopback host, is honoured; anything else is logged
/// and ignored, and the real API is used.
fn stripe_api_base() -> String {
    const DEFAULT: &str = "https://api.stripe.com";
    let Ok(raw) = std::env::var("STRIPE_API_BASE") else {
        return DEFAULT.to_string();
    };
    let base = raw.trim().trim_end_matches('/');
    if base.is_empty() {
        return DEFAULT.to_string();
    }
    let honoured = reqwest::Url::parse(base).is_ok_and(|url| match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    });
    if honoured {
        base.to_string()
    } else {
        tracing::warn!(
            "STRIPE_API_BASE must be https:// (or http:// to a loopback host): ignoring it, using {DEFAULT}"
        );
        DEFAULT.to_string()
    }
}

/// Creates a real Stripe Checkout Session (best-effort). `price_id` is `Some` for a plan upgrade (a
/// recurring subscription, `mode=subscription`) and `None` for a one-time purchase (a credit
/// top-up, `mode=payment` with ad-hoc `price_data`). `checkout_id` is OUR `Checkout.id`, stamped
/// into the session `metadata` (and the SUBSCRIPTION's, since `customer.subscription.*` webhooks
/// carry only that) so webhooks can find the local record. Returns `(hosted_checkout_url,
/// stripe_session_id)`; the session id is stored via `attach_stripe_session` so `billing_confirm`
/// verifies real payment before applying anything.
pub async fn stripe_checkout(
    http: &reqwest::Client,
    price_id: Option<&str>,
    amount_cents: u64,
    product_name: &str,
    success_url: &str,
    cancel_url: &str,
    checkout_id: &str,
) -> anyhow::Result<(String, String)> {
    let key = std::env::var("STRIPE_SECRET_KEY")?;
    let mut params: Vec<(String, String)> = vec![
        ("success_url".into(), success_url.to_string()),
        ("cancel_url".into(), cancel_url.to_string()),
        ("metadata[checkout_id]".into(), checkout_id.to_string()),
        ("line_items[0][quantity]".into(), "1".into()),
    ];
    match price_id {
        Some(pid) => {
            params.push(("mode".into(), "subscription".into()));
            params.push(("line_items[0][price]".into(), pid.to_string()));
            params.push((
                "subscription_data[metadata][checkout_id]".into(),
                checkout_id.to_string(),
            ));
        }
        None => {
            params.push(("mode".into(), "payment".into()));
            params.push(("line_items[0][price_data][currency]".into(), "usd".into()));
            params.push((
                "line_items[0][price_data][unit_amount]".into(),
                amount_cents.to_string(),
            ));
            params.push((
                "line_items[0][price_data][product_data][name]".into(),
                product_name.to_string(),
            ));
        }
    }
    let resp = http
        .post(format!("{}/v1/checkout/sessions", stripe_api_base()))
        .basic_auth(&key, Some(""))
        .form(&params)
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow::anyhow!("stripe: no checkout url ({v})"))?;
    let session_id = v
        .get("id")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow::anyhow!("stripe: no session id ({v})"))?;
    Ok((url.to_string(), session_id.to_string()))
}

/// Whether a Checkout Session's money has actually been taken: Stripe's `payment_status` is
/// `"paid"`. `status == "complete"` is NOT enough: a delayed payment method (a bank debit)
/// completes the session while `payment_status` is still `"unpaid"`, and funds that have not
/// cleared can still fail. `"no_payment_required"` is refused as well: this platform creates no
/// zero-amount session (a free plan is applied directly and never reaches Stripe), so it could only
/// mean a trial or coupon nobody configured. Used by both ways a purchase is confirmed (the
/// `billing_confirm` read-back and the signed webhook).
pub fn session_paid(session: &serde_json::Value) -> bool {
    session.get("payment_status").and_then(|s| s.as_str()) == Some("paid")
}

/// Real payment/subscription status of a Stripe Checkout Session, fetched from Stripe: `paid` is
/// [`session_paid`]. The authoritative check `billing_confirm` uses instead of a client's
/// redirect-back.
pub struct StripeSessionStatus {
    pub paid: bool,
    pub customer: Option<String>,
    pub subscription: Option<String>,
}

pub async fn stripe_verify_session(
    http: &reqwest::Client,
    session_id: &str,
) -> anyhow::Result<StripeSessionStatus> {
    let key = std::env::var("STRIPE_SECRET_KEY")?;
    let resp = http
        .get(format!(
            "{}/v1/checkout/sessions/{session_id}",
            stripe_api_base()
        ))
        .basic_auth(&key, Some(""))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    if v.get("error").is_some() {
        anyhow::bail!("stripe: session lookup failed ({v})");
    }
    Ok(StripeSessionStatus {
        paid: session_paid(&v),
        customer: v
            .get("customer")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
        subscription: v
            .get("subscription")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
    })
}

/// Verifies a Stripe webhook's `Stripe-Signature` header (`t=<unix_ts>,v1=<hex_hmac_sha256>[,...]`)
/// against the RAW body with `STRIPE_WEBHOOK_SECRET`: HMAC-SHA256 of `"{timestamp}.{raw_body}"`,
/// constant-time compared against each `v1` (several during a secret rotation), with a 5-minute
/// replay window.
pub fn verify_webhook_signature(payload: &[u8], sig_header: &str, secret: &str) -> bool {
    let mut timestamp: Option<i64> = None;
    let mut v1_sigs: Vec<&str> = Vec::new();
    for part in sig_header.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            match k.trim() {
                "t" => timestamp = v.trim().parse().ok(),
                "v1" => v1_sigs.push(v.trim()),
                _ => {}
            }
        }
    }
    let Some(ts) = timestamp else { return false };
    let now_s = (now_ms() / 1000) as i64;
    if (now_s - ts).abs() > 300 {
        return false;
    }
    if v1_sigs.is_empty() {
        return false;
    }
    let mut signed = Vec::with_capacity(payload.len() + 20);
    signed.extend_from_slice(ts.to_string().as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(payload);
    let expected = crate::admin::hmac_sha256_hex(secret.as_bytes(), &signed);
    v1_sigs.iter().any(|s| crate::admin::ct_eq(s, &expected))
}
