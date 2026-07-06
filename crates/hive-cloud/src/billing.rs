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
        price_cents: 50000,
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
}

pub const RATE_CARD: RateCard = RateCard {
    active_cpu_hr_cents: 12.8,          // $0.128 / active-CPU-hr
    mem_gb_hr_cents: 1.06,             // $0.0106 / GB-hr
    requests_per_million_cents: 200.0, // $2.00 / M requests
    waf_per_million_cents: 60.0,       // $0.60 / M blocked
};

/// Cumulative usage counters for a tenant (as measured from live metrics).
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub active_cpu_ms: u64,
    /// Provisioned memory GB-hours ×1000 (milli, to keep it an integer).
    pub mem_gb_hr_milli: u64,
    pub requests: u64,
    pub blocked: u64,
}

impl RateCard {
    /// Cost in (fractional) cents for a usage delta.
    pub fn cost_cents(&self, u: &UsageTotals) -> f64 {
        (u.active_cpu_ms as f64 / 3_600_000.0) * self.active_cpu_hr_cents
            + (u.mem_gb_hr_milli as f64 / 1000.0) * self.mem_gb_hr_cents
            + (u.requests as f64 / 1_000_000.0) * self.requests_per_million_cents
            + (u.blocked as f64 / 1_000_000.0) * self.waf_per_million_cents
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
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    pub updated_ms: u64,
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
            period_start_ms: now,
            period_end_ms: now + MONTH_MS,
            updated_ms: now,
        }
    }
    pub fn remaining_included(&self) -> u64 {
        self.included_cents.saturating_sub(self.used_cents)
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
        let tenant = if tenant.is_empty() { "personal" } else { tenant };
        let mut m = self.accounts.write();
        let acc = m.entry(tenant.to_string()).or_insert_with(|| BillingAccount::default_for(tenant));
        // Roll the monthly period (renew included allowance) if elapsed — and
        // FINALIZE the closing period's invoice before resetting the counters.
        let now = now_ms();
        let rolled = if now >= acc.period_end_ms {
            let closing = build_invoice(acc, self.usage_cost_cents_for(tenant), "paid");
            acc.period_start_ms = now;
            acc.period_end_ms = now + MONTH_MS;
            acc.used_cents = 0;
            acc.included_cents = plan_spec(&acc.plan).included_cents;
            acc.updated_ms = now;
            Some(closing)
        } else {
            None
        };
        let out = acc.clone();
        drop(m);
        if let Some(inv) = rolled {
            self.invoices.write().entry(out.tenant.clone()).or_default().push(inv);
            self.ledger_push(&out.tenant, "renewal", 0, out.balance_cents, "Monthly period renewed; invoice finalized");
        }
        out
    }

    /// Sum of usage (non-plan) charges recorded THIS period, in cents.
    fn usage_cost_cents_for(&self, tenant: &str) -> i64 {
        let acc_start = self.accounts.read().get(tenant).map(|a| a.period_start_ms).unwrap_or(0);
        self.ledger
            .read()
            .iter()
            .filter(|e| e.tenant == tenant && e.kind == "usage" && e.ts_ms >= acc_start)
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
            let acc = m.entry(tenant.to_string()).or_insert_with(|| BillingAccount::default_for(tenant));
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
        let tenant = if tenant.is_empty() { "personal" } else { tenant };
        let (delta, carry) = {
            let mut meters = self.meters.write();
            let st = meters.entry(tenant.to_string()).or_default();
            let sub = |cur: u64, last: u64| if cur >= last { cur - last } else { cur };
            let delta = UsageTotals {
                active_cpu_ms: sub(current.active_cpu_ms, st.last.active_cpu_ms),
                mem_gb_hr_milli: sub(current.mem_gb_hr_milli, st.last.mem_gb_hr_milli),
                requests: sub(current.requests, st.last.requests),
                blocked: sub(current.blocked, st.last.blocked),
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
        let tenant = if tenant.is_empty() { "personal" } else { tenant };
        let acc = self.account(tenant);
        build_invoice(&acc, self.usage_cost_cents_for(tenant), "draft")
    }

    /// Finalized invoices for a tenant (newest first) plus the current draft.
    pub fn invoices(&self, tenant: &str) -> Vec<Invoice> {
        let tenant = if tenant.is_empty() { "personal" } else { tenant };
        let mut v = self.invoices.read().get(tenant).cloned().unwrap_or_default();
        v.push(self.current_invoice(tenant));
        v.sort_by(|a, b| b.period_start_ms.cmp(&a.period_start_ms));
        v
    }

    fn ledger_push(&self, tenant: &str, kind: &str, amount: i64, balance_after: i64, note: &str) -> LedgerEntry {
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

    pub fn set_plan(&self, tenant: &str, plan: &str) -> BillingAccount {
        let tenant = if tenant.is_empty() { "personal" } else { tenant };
        let spec = plan_spec(plan);
        {
            let mut m = self.accounts.write();
            let acc = m.entry(tenant.to_string()).or_insert_with(|| BillingAccount::default_for(tenant));
            acc.plan = spec.id.to_string();
            acc.included_cents = spec.included_cents;
            acc.updated_ms = now_ms();
        }
        let bal = self.account(tenant).balance_cents;
        self.ledger_push(tenant, "plan_change", spec.price_cents as i64, bal, &format!("Switched to {} plan", spec.name));
        self.account(tenant)
    }

    pub fn add_credits(&self, tenant: &str, cents: u64, note: &str) -> BillingAccount {
        let tenant = if tenant.is_empty() { "personal" } else { tenant };
        let bal;
        {
            let mut m = self.accounts.write();
            let acc = m.entry(tenant.to_string()).or_insert_with(|| BillingAccount::default_for(tenant));
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
        let tenant = if tenant.is_empty() { "personal" } else { tenant };
        let bal_after;
        {
            let mut m = self.accounts.write();
            let acc = m.entry(tenant.to_string()).or_insert_with(|| BillingAccount::default_for(tenant));
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
                "Monthly included compute exhausted — upgrade to Pro or add credits to deploy.".into(),
            );
        }
        Ok(())
    }

    pub fn ledger(&self, tenant: &str) -> Vec<LedgerEntry> {
        let tenant = if tenant.is_empty() { "personal" } else { tenant };
        let mut v: Vec<LedgerEntry> = self.ledger.read().iter().filter(|e| e.tenant == tenant).cloned().collect();
        v.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
        v
    }

    pub fn open_checkout(&self, tenant: &str, kind: &str, plan: &str, amount_cents: u64) -> Checkout {
        let c = Checkout {
            id: format!("cs_{}", &Uuid::new_v4().simple().to_string()[..18]),
            tenant: tenant.to_string(),
            kind: kind.to_string(),
            plan: plan.to_string(),
            amount_cents,
            created_ms: now_ms(),
        };
        self.checkouts.write().insert(c.id.clone(), c.clone());
        c
    }

    pub fn get_checkout(&self, id: &str) -> Option<Checkout> {
        self.checkouts.read().get(id).cloned()
    }

    /// Complete a checkout: apply the plan change or credit top-up. Idempotent
    /// (consumes the session).
    pub fn confirm_checkout(&self, id: &str) -> Option<(Checkout, BillingAccount)> {
        let c = self.checkouts.write().remove(id)?;
        let acc = if c.kind == "credits" {
            self.add_credits(&c.tenant, c.amount_cents, "Credit top-up (checkout)")
        } else {
            self.set_plan(&c.tenant, &c.plan)
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
        *self.accounts.write() = accounts.into_iter().map(|a| (a.tenant.clone(), a)).collect();
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
                description: format!("Included allowance credit (${:.2})", included as f64 / 100.0),
                amount_cents: -covered,
            });
        }
    }
    let subtotal: i64 = lines.iter().map(|l| l.amount_cents).sum();
    let num = format!("INV-{}", &Uuid::new_v4().simple().to_string()[..8].to_uppercase());
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
    std::env::var("STRIPE_SECRET_KEY").map(|k| !k.is_empty()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_card_cost_matches_ui() {
        // 1 CPU-hr = $0.128 = 12.8¢; 1 GB-hr = 1.06¢; 1M req = $2 = 200¢.
        let u = UsageTotals { active_cpu_ms: 3_600_000, mem_gb_hr_milli: 1000, requests: 1_000_000, blocked: 0 };
        let c = RATE_CARD.cost_cents(&u);
        assert!((c - (12.8 + 1.06 + 200.0)).abs() < 1e-6, "got {c}");
    }

    #[test]
    fn meter_charges_delta_and_carries_fraction() {
        let s = BillingStore::new();
        s.set_plan("acme", "pro"); // overage plan so balance can go negative
        // First reading: 0.5¢ worth (below a whole cent) → nothing charged yet.
        let half = UsageTotals { active_cpu_ms: (3_600_000.0 * 0.5 / 12.8) as u64, ..Default::default() };
        let c1 = s.meter_usage("acme", half);
        assert_eq!(c1, 0, "sub-cent usage should not charge yet");
        // Cumulative grows to ~1.5¢ → the crossed whole cent (1¢) is charged.
        let onefive = UsageTotals { active_cpu_ms: (3_600_000.0 * 1.5 / 12.8) as u64, ..Default::default() };
        let c2 = s.meter_usage("acme", onefive);
        assert!(c2 >= 1, "should charge the crossed whole cent, got {c2}");
        // The usage is recorded in the ledger as a negative "usage" entry.
        assert!(s.ledger("acme").iter().any(|e| e.kind == "usage" && e.amount_cents < 0));
    }

    #[test]
    fn meter_handles_counter_reset() {
        let s = BillingStore::new();
        s.set_plan("acme", "pro");
        let big = UsageTotals { active_cpu_ms: 3_600_000 * 10, ..Default::default() }; // 128¢
        s.meter_usage("acme", big);
        // Node restarts → counter drops to a small value; must not underflow/panic and
        // should charge the fresh (small) amount, not a huge delta.
        let small = UsageTotals { active_cpu_ms: 3_600_000, ..Default::default() }; // 12.8¢
        let c = s.meter_usage("acme", small);
        assert!(c <= 13, "reset should charge only the fresh amount, got {c}");
    }

    #[test]
    fn hobby_deploy_gate_locks_when_exhausted() {
        let s = BillingStore::new();
        // Fresh hobby account can deploy.
        assert!(s.can_deploy("h").is_ok());
        // Consume just past the $5 (500¢) included allowance (small overage).
        let over = UsageTotals { active_cpu_ms: (3_600_000.0 * 520.0 / 12.8) as u64, ..Default::default() };
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
        let u = UsageTotals { active_cpu_ms: 3_600_000 * 5, ..Default::default() }; // 64¢
        s.meter_usage("acme", u);
        let inv = s.current_invoice("acme");
        assert_eq!(inv.status, "draft");
        assert!(inv.lines.iter().any(|l| l.description.contains("subscription")));
        assert!(inv.lines.iter().any(|l| l.description.contains("Compute usage")));
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

/// Create a real Stripe Checkout Session (best-effort). Returns the hosted URL.
pub async fn stripe_checkout(
    http: &reqwest::Client,
    amount_cents: u64,
    product_name: &str,
    success_url: &str,
    cancel_url: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("STRIPE_SECRET_KEY")?;
    let params = [
        ("mode", "payment".to_string()),
        ("success_url", success_url.to_string()),
        ("cancel_url", cancel_url.to_string()),
        ("line_items[0][quantity]", "1".to_string()),
        ("line_items[0][price_data][currency]", "usd".to_string()),
        ("line_items[0][price_data][unit_amount]", amount_cents.to_string()),
        ("line_items[0][price_data][product_data][name]", product_name.to_string()),
    ];
    let resp = http
        .post("https://api.stripe.com/v1/checkout/sessions")
        .basic_auth(key, Some(""))
        .form(&params)
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    v.get("url")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("stripe: no checkout url ({})", v))
}
