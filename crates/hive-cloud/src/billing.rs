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
/// allows pay-as-you-go beyond it (Pro only).
#[derive(Clone, Serialize)]
pub struct PlanSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub price_cents: u64,
    pub included_cents: u64,
    pub overage: bool,
    pub features: &'static [&'static str],
}

pub const PLANS: &[PlanSpec] = &[
    PlanSpec {
        id: "hobby",
        name: "Hobby",
        price_cents: 0,
        included_cents: 500,
        overage: false,
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

#[derive(Default)]
pub struct BillingStore {
    accounts: RwLock<HashMap<String, BillingAccount>>,
    ledger: RwLock<Vec<LedgerEntry>>,
    checkouts: RwLock<HashMap<String, Checkout>>,
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
        // Roll the monthly period (renew included allowance) if elapsed.
        let now = now_ms();
        if now >= acc.period_end_ms {
            acc.period_start_ms = now;
            acc.period_end_ms = now + MONTH_MS;
            acc.used_cents = 0;
            acc.included_cents = plan_spec(&acc.plan).included_cents;
            acc.updated_ms = now;
        }
        acc.clone()
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
    pub fn all_accounts(&self) -> Vec<BillingAccount> {
        self.accounts.read().values().cloned().collect()
    }
}

pub fn stripe_configured() -> bool {
    std::env::var("STRIPE_SECRET_KEY").map(|k| !k.is_empty()).unwrap_or(false)
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
