//! Billing & compute credits — Hobby vs Pro, pay-as-you-go + prepaid credits.
//!
//! Stripe-ready: when `STRIPE_SECRET_KEY` is set the checkout endpoint creates a
//! real Stripe Checkout Session; otherwise a fully-working **mock checkout** is
//! used for local/dev. Every balance change is recorded in an append-only ledger
//! and the audit log, so billing is ACID-durable and historically auditable.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub const MONTH_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Plan catalog. `included_cents` is the monthly compute allowance; `overage`
/// allows pay-as-you-go beyond it (Pro only). `max_projects`/`max_members` are the
/// enforced quotas (0 = unlimited) — the "business locking" gates.
#[derive(Clone, Serialize)]
pub struct PlanSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub price_cents: u64,
    /// The real Stripe recurring Price id for this plan's subscription
    /// (`price_...`), or `None` for a plan with no paid subscription (Hobby).
    /// Checkout uses this directly (`mode=subscription`) instead of ad-hoc
    /// `price_data` — required for a REAL recurring charge; ad-hoc price_data
    /// only supports one-time payments.
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
        max_projects: 100,
        max_members: 1,
        max_sandboxes: 5,
        max_running_sandboxes: 1,
        max_sandbox_mounts: 1,
        max_sandbox_env_vars: 20,
        max_sandbox_ports: 2,
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
        // $1,000/mo flat — matches the real Stripe price exactly (was 50000 /
        // $500, a stale mock value from before real pricing existed).
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
        features: &[
            "$1,000 of included compute / month",
            "Automatic multi-region fail-over",
            "1-hour function runtime (3600s)",
            "Team / Org SSO (SAML & OIDC)",
            "100k concurrency · priority support",
        ],
    },
];

pub fn plan_spec(id: &str) -> &'static PlanSpec {
    PLANS.iter().find(|p| p.id == id).unwrap_or(&PLANS[0])
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
    /// Requests served by a donated browser node, cents per million. 0.0 by
    /// design (bn-impl-billing-metering): Cloudflare/Salad precedent is to
    /// bill only platform-consumed resources, and an owner's own browser
    /// serving their own app consumes none. Kept as a real rate-card field
    /// (not omitted) so pricing it later is a config change, not a schema
    /// migration.
    pub browser_per_million_cents: f64,
}

pub const RATE_CARD: RateCard = RateCard {
    active_cpu_hr_cents: 12.8,         // $0.128 / active-CPU-hr
    mem_gb_hr_cents: 1.06,             // $0.0106 / GB-hr
    requests_per_million_cents: 200.0, // $2.00 / M requests
    waf_per_million_cents: 60.0,       // $0.60 / M blocked
    gpu_hr_cents: 60.0,                // $0.60 / GPU-hr (T4 class)
    browser_per_million_cents: 0.0,    // uncharged today, see field doc
};

/// Cumulative usage counters for a tenant (as measured from live metrics).
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub active_cpu_ms: u64,
    /// Provisioned memory GB-hours ×1000 (milli, to keep it an integer).
    pub mem_gb_hr_milli: u64,
    pub requests: u64,
    pub blocked: u64,
    /// GPU instance wall-time, ms (a GPU function's fluid_ms — the GPU is held
    /// for the instance's whole life, so wall-time not active-CPU is the honest
    /// unit). `serde(default)` keeps persisted pre-GPU snapshots loading.
    #[serde(default)]
    pub gpu_ms: u64,
    /// Requests served by a donated browser node — counted separately from
    /// `requests` so it's visible in the UI/quota ahead of any pricing
    /// decision, never silently folded into the billed fleet-request count.
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
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InvoiceLine {
    pub description: String,
    pub amount_cents: i64,
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

// ---- Enterprise feature gating (see [`crate::enterprise`]) ----------------
// SAML SSO, SCIM directory sync, custom SIEM audit streaming, account-level IP
// blocking, and Conformance are Enterprise-only. Deployment password protection
// and microfrontends are available from Pro up (Vercel parity).

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

#[derive(Clone, Serialize, Deserialize)]
pub struct BillingAccount {
    pub tenant: String,
    pub plan: String,
    pub status: String, // "active"
    pub included_cents: u64,
    pub used_cents: u64,
    /// Prepaid credit balance (pay-as-you-go top-ups), in cents.
    pub balance_cents: i64,
    #[serde(default)]
    pub stripe_customer: String,
    /// The active Stripe Subscription id (`sub_...`) backing a paid plan, if
    /// any — set from the checkout/webhook verification response. Lets a
    /// `customer.subscription.deleted`/`.updated` webhook find the right
    /// tenant to downgrade/sync without a separate index.
    #[serde(default)]
    pub stripe_subscription: String,
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    pub updated_ms: u64,
    /// Operator-set tier FLOOR. While `Some(plan)`, no automatic path may move
    /// this tenant below that plan — not the free-plan checkout shortcut, not a
    /// Stripe `customer.subscription.deleted`, not a period rollover.
    ///
    /// A one-shot grant is not durable, and that cost real money and real
    /// downtime: the platform owner was granted `enterprise` on 2026-07-18, a
    /// user-initiated free-plan checkout silently reverted only the BILLING
    /// half to `hobby` on 2026-07-25, and the account sat quota-locked
    /// (`can_deploy` denied, balance -42030 cents) while `c.teams` still read
    /// `enterprise`. Only an explicit operator call sets or clears this, so a
    /// downgrade event can no longer undo a grant.
    #[serde(default)]
    pub tier_locked_plan: Option<String>,
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
    /// "plan" | "credits"
    pub kind: String,
    pub plan: String,
    pub amount_cents: u64,
    /// The REAL Stripe Checkout Session id (`cs_...`), set once the real
    /// session is created. Empty for a mock (no-Stripe) checkout. Lets
    /// confirmation verify actual payment with Stripe instead of trusting an
    /// unauthenticated client's say-so that they paid.
    #[serde(default)]
    pub stripe_session_id: String,
    pub created_ms: u64,
}

/// Per-tenant meter cursor: the last cumulative usage seen + the sub-cent remainder
/// carried between ticks so no usage is lost to rounding. In-memory (rebuilt on
/// restart from live metrics; charges already recorded persist in the account).
#[derive(Default)]
struct MeterState {
    last: UsageTotals,
    frac_cents: f64,
}

#[derive(Default)]
pub struct BillingStore {
    accounts: RwLock<HashMap<String, BillingAccount>>,
    ledger: RwLock<Vec<LedgerEntry>>,
    checkouts: RwLock<HashMap<String, Checkout>>,
    meters: RwLock<HashMap<String, MeterState>>,
    invoices: RwLock<HashMap<String, Vec<Invoice>>>,
}

impl BillingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or lazily create a Hobby) account for a tenant.
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
        // Roll the monthly period (renew included allowance) if elapsed — and
        // FINALIZE the closing period's invoice before resetting the counters.
        let now = now_ms();
        let rolled = if now >= acc.period_end_ms {
            // MUST use usage_cost_cents_since (ledger-only, no `self.accounts`
            // touch) here, never usage_cost_cents_for -- `m` (accounts' write
            // guard) is still held at this point, and usage_cost_cents_for
            // internally does `self.accounts.read()`, which deadlocks against
            // parking_lot::RwLock's non-reentrant write-then-read-same-thread
            // pattern. Live-witnessed in production: this branch only fires
            // once a tenant's monthly period actually elapses, so it went
            // unhit for a long time -- reproduced as a real hang on the
            // control-plane leader once account() started being called more
            // frequently (relational mirror loop, one call per tenant per
            // tick) raised the odds of landing on a tenant mid-rollover.
            let closing = build_invoice(
                acc,
                self.usage_cost_cents_since(tenant, acc.period_start_ms),
                "paid",
            );
            acc.period_start_ms = now;
            acc.period_end_ms = now + MONTH_MS;
            acc.used_cents = 0;
            // Re-derive the allowance from the EFFECTIVE plan, i.e. never below
            // an operator tier floor. Without this a locked tenant whose stored
            // plan was downgraded before the lock existed would have the lower
            // allowance re-applied every month, re-arming `can_deploy`'s block.
            let effective = match acc.tier_locked_plan.as_deref() {
                Some(f) if plan_rank(f) > plan_rank(&acc.plan) => {
                    acc.plan = f.to_string();
                    f
                }
                _ => acc.plan.as_str(),
            };
            acc.included_cents = plan_spec(effective).included_cents;
            acc.updated_ms = now;
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

    /// Sum of usage (non-plan) charges recorded THIS period, in cents. Looks up
    /// the tenant's period_start_ms itself via `self.accounts.read()` -- ONLY
    /// safe to call when the caller does NOT already hold a lock (read or
    /// write) on `self.accounts` on this thread/task. `account()`'s rollover
    /// path holds `self.accounts`'s write lock across this computation and
    /// must use `usage_cost_cents_since` instead (see its call site).
    fn usage_cost_cents_for(&self, tenant: &str) -> i64 {
        let acc_start = self
            .accounts
            .read()
            .get(tenant)
            .map(|a| a.period_start_ms)
            .unwrap_or(0);
        self.usage_cost_cents_since(tenant, acc_start)
    }

    /// Sum of usage (non-plan) charges recorded since `period_start_ms`, in
    /// cents. Touches only `self.ledger`, never `self.accounts` -- safe to
    /// call while the caller already holds `self.accounts`'s write lock.
    fn usage_cost_cents_since(&self, tenant: &str, period_start_ms: u64) -> i64 {
        self.ledger
            .read()
            .iter()
            .filter(|e| e.tenant == tenant && e.kind == "usage" && e.ts_ms >= period_start_ms)
            .map(|e| -e.amount_cents) // usage entries are negative amounts
            .sum()
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

    /// Feed the latest cumulative usage totals for a tenant; charges the delta since
    /// the last reading (delta·rate), carrying the sub-cent remainder so nothing is
    /// lost. Handles counter resets (pool recycle / node restart) by charging the
    /// fresh value. This is the metering→billing pipeline. Returns cents charged.
    pub fn meter_usage(&self, tenant: &str, current: UsageTotals) -> u64 {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        let (delta, carry) = {
            let mut meters = self.meters.write();
            let st = meters.entry(tenant.to_string()).or_default();
            // These are CUMULATIVE counters summed across the whole fleet
            // (`fleet_function_stats`), so `cur < last` means a counter RESET,
            // not usage — a single node restarting drops its contribution and
            // with it the fleet total.
            //
            // Charging `cur` in that case (the previous behaviour) re-bills the
            // ENTIRE fleet's accumulated usage as if it were brand new, every
            // time any one node restarts. A rolling fleet upgrade therefore
            // bills the whole fleet over and over: witnessed as a tenant landing
            // at -$55 against a $5 allowance during a session that restarted all
            // 12 nodes several times, which then tripped the can_deploy credit
            // lock and refused deploys.
            //
            // Treat a decrease as a re-baseline: charge nothing for this tick and
            // adopt the new floor. That can under-count the usage accrued between
            // the last sample and the restart, which is the correct direction to
            // err for billing — never charge a customer for a counter reset.
            let sub = |cur: u64, last: u64| if cur >= last { cur - last } else { 0 };
            let delta = UsageTotals {
                active_cpu_ms: sub(current.active_cpu_ms, st.last.active_cpu_ms),
                mem_gb_hr_milli: sub(current.mem_gb_hr_milli, st.last.mem_gb_hr_milli),
                requests: sub(current.requests, st.last.requests),
                blocked: sub(current.blocked, st.last.blocked),
                gpu_ms: sub(current.gpu_ms, st.last.gpu_ms),
                browser_requests: sub(current.browser_requests, st.last.browser_requests),
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

    /// Finalized (`status == "paid"`) invoices for a tenant, exactly as
    /// stored — WITHOUT the current in-progress period's draft (unlike
    /// `invoices()`, which appends it for the read API). `self.invoices`
    /// only ever holds invoices `account()`'s period-rollover pushed via
    /// `build_invoice(.., "paid")`, so this is already finalized-only by
    /// construction; used by the relational mirror (`relational::upsert_billing`),
    /// which must NEVER persist a draft (transient/computed, not a stored fact).
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

    /// Set (or clear, with `None`) a tenant's operator tier FLOOR, and raise the
    /// live plan to it immediately so the lock and the served tier can never
    /// disagree. Operator-only by construction: the only caller is the admin
    /// grant path, so no user-reachable route can lift or lower it.
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
        // Raising to the floor goes through set_plan so the ledger records the
        // change and `included_cents` is re-derived from the plan spec.
        match floor {
            Some(f) if plan_rank(f) > plan_rank(&self.account(tenant).plan) => {
                self.set_plan(tenant, f)
            }
            _ => self.account(tenant),
        }
    }

    pub fn set_plan(&self, tenant: &str, plan: &str) -> BillingAccount {
        let tenant = if tenant.is_empty() {
            "personal"
        } else {
            tenant
        };
        // Honour the operator-set tier FLOOR. Every automatic downgrade path in
        // the platform (free-plan checkout shortcut, Stripe
        // `customer.subscription.deleted`, checkout confirmation) funnels
        // through here, so enforcing it at this ONE chokepoint is what makes
        // the lock unbypassable rather than something each caller must
        // remember. `set_tier_lock` is the only way past it.
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
        let bal;
        {
            let mut m = self.accounts.write();
            let acc = m
                .entry(tenant.to_string())
                .or_insert_with(|| BillingAccount::default_for(tenant));
            acc.balance_cents += cents as i64;
            acc.updated_ms = now_ms();
            bal = acc.balance_cents;
        }
        self.ledger_push(tenant, "credit", cents as i64, bal, note);
        self.account(tenant)
    }

    /// Charge compute usage: consume the monthly included allowance first, then
    /// prepaid balance. On Hobby (no overage) a charge that would go negative is
    /// rejected. Returns Ok(account) or Err(reason).
    pub fn charge(&self, tenant: &str, cents: u64, note: &str) -> Result<BillingAccount, String> {
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
            let mut remaining = cents;
            let included_left = acc.included_cents.saturating_sub(acc.used_cents);
            let from_included = remaining.min(included_left);
            acc.used_cents += from_included;
            remaining -= from_included;
            if remaining > 0 {
                if acc.balance_cents >= remaining as i64 {
                    acc.balance_cents -= remaining as i64;
                } else if spec.overage {
                    // Pay-as-you-go: allow the balance to go negative (invoiced).
                    acc.balance_cents -= remaining as i64;
                } else {
                    return Err("Insufficient credits — upgrade to Pro or add credits.".into());
                }
            }
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

    /// Checkouts currently open (not yet confirmed — `confirm_checkout`
    /// removes a checkout from the live map once applied) for a tenant. Used
    /// by the relational mirror to persist a tenant's in-flight checkout
    /// state (`billing_checkouts`, previously not mirrored at all).
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
        let c = Checkout {
            id: format!("co_{}", &Uuid::new_v4().simple().to_string()[..18]),
            tenant: tenant.to_string(),
            kind: kind.to_string(),
            plan: plan.to_string(),
            amount_cents,
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

    /// Persist the Stripe customer/subscription ids backing a tenant's paid
    /// plan (from a verified checkout or a `checkout.session.completed`
    /// webhook). Empty strings are no-ops (never overwrite a known id with
    /// nothing just because one event's payload omitted it).
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

    /// Drop a tenant's billing account entirely (used when its team is deleted),
    /// so a removed tenant cannot leave a stray tier/quota row behind. The
    /// LEDGER is deliberately left intact: it is the financial record of what
    /// actually happened and must survive the account it describes.
    pub fn remove_account(&self, tenant: &str) -> Option<BillingAccount> {
        self.accounts.write().remove(tenant)
    }

    /// Complete a checkout: apply the plan change or credit top-up. Idempotent
    /// (consumes the session).
    /// Complete a checkout. A CREDIT top-up is applied here (it touches only the
    /// balance). A PLAN checkout deliberately does NOT write the plan here: the
    /// caller applies it via `admin::apply_plan_everywhere`, which writes BOTH
    /// the billing account and the team record and emits the audit entry.
    ///
    /// This used to call `set_plan` itself, which made it a SECOND, silent plan
    /// writer behind the caller's back — a tier could change with no audit
    /// record and with the team record left stale, which is exactly the split
    /// (`teams: enterprise` / `billing: hobby`, no `plan_change` in the audit
    /// log) witnessed in production. One writer only.
    pub fn confirm_checkout(&self, id: &str) -> Option<(Checkout, BillingAccount)> {
        let c = self.checkouts.write().remove(id)?;
        let acc = if c.kind == "credits" {
            self.add_credits(&c.tenant, c.amount_cents, "Credit top-up (checkout)")
        } else {
            self.account(&c.tenant)
        };
        Some((c, acc))
    }

    // --- persistence ---
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
    }
}

pub fn stripe_configured() -> bool {
    std::env::var("STRIPE_SECRET_KEY")
        .map(|k| !k.is_empty())
        .unwrap_or(false)
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
        assert_eq!(plan_max_members("hobby"), 1);
        assert_eq!(plan_max_members("enterprise"), 0); // unlimited
        assert!(plan_max_projects("hobby") > 0);
        assert_eq!(plan_max_projects("enterprise"), 0);
    }
}

/// Create a real Stripe Checkout Session (best-effort). `price_id` is `Some` for
/// a plan upgrade — a REAL recurring subscription against that Stripe Price
/// (`mode=subscription`); `None` for a one-time purchase (credit top-ups —
/// `mode=payment` with ad-hoc `price_data`, since there's no fixed Price object
/// for an arbitrary top-up amount). `checkout_id` is OUR internal `Checkout.id`,
/// stamped into the session's `metadata` so the webhook handler (which only
/// ever hears from Stripe, never from our own client) can find the matching
/// local record. Returns `(hosted_checkout_url, stripe_session_id)` — the
/// session id is stored via `BillingStore::attach_stripe_session` so
/// `billing_confirm` can later verify real payment before applying anything.
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
            // Subscriptions need the metadata on the SUBSCRIPTION object too
            // (not just the checkout session) — that's what the
            // `customer.subscription.*` webhooks carry, and the session's own
            // metadata isn't copied onto it automatically.
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
        .post("https://api.stripe.com/v1/checkout/sessions")
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

/// Real payment/subscription status of a Stripe Checkout Session, fetched
/// directly from Stripe. `paid` is true when the session actually completed
/// (`payment_status == "paid"` for a one-time charge, or `status ==
/// "complete"` for a $0-due-now subscription setup) — this is the
/// authoritative check `billing_confirm` uses instead of trusting an
/// unauthenticated client's redirect-back as proof of payment.
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
            "https://api.stripe.com/v1/checkout/sessions/{session_id}"
        ))
        .basic_auth(&key, Some(""))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    if v.get("error").is_some() {
        anyhow::bail!("stripe: session lookup failed ({v})");
    }
    let paid = v.get("payment_status").and_then(|s| s.as_str()) == Some("paid")
        || v.get("status").and_then(|s| s.as_str()) == Some("complete");
    Ok(StripeSessionStatus {
        paid,
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

/// Verify a Stripe webhook delivery's `Stripe-Signature` header
/// (`t=<unix_ts>,v1=<hex_hmac_sha256>[,v1=<hex>...]`) against the RAW request
/// body using `STRIPE_WEBHOOK_SECRET`. Matches Stripe's documented scheme:
/// HMAC-SHA256 of `"{timestamp}.{raw_body}"`, constant-time compared against
/// each `v1` value (Stripe may send more than one during a signing-secret
/// rotation), with a 5-minute replay-tolerance window on the timestamp.
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
