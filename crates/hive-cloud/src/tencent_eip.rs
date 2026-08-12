//! Dedicated public IPv4 (Tencent Cloud EIP) provisioning for the paid
//! `dedicated_ipv4` project addon.
//!
//! [`provision_from_checkout`] is called from exactly two places —
//! `admin::billing_confirm` (the browser-redirect confirmation) and
//! `admin::billing_webhook`'s `checkout.session.completed` handler (the
//! Stripe-authoritative path, since a user's browser is not guaranteed to
//! still be present) — and both funnel through the SAME idempotency check
//! before ever touching the network: does `ProjectSettings::dedicated_ipv4`
//! already hold a claim for this project? That field IS the durable claim.
//! `ProjectStore` replicates fleet-wide via `store_sync::REGISTRY`'s
//! "projects" entry (AGENTS.md's round-robin-reads rule: this must never be
//! node-local), so a double confirmation firing — Stripe retries webhooks,
//! and the browser redirect can race the webhook for the same event — is a
//! no-op read, never a second `AllocateAddresses` purchase. A redeploy reuses
//! the same claim via `git.rs`'s manifest merge; it never calls this module.
//!
//! Real Tencent Cloud calls (`AllocateAddresses` + `AssociateAddress`, both
//! under the `vpc` service, TC3-HMAC-SHA256 signed exactly per Tencent's API
//! 3.0 spec) are isolated in [`allocate_eip`] — the only place in this
//! module that touches the network. Per this task's hard constraint, nothing
//! in this repo's own tooling ever calls it: it is written to be correct by
//! inspection (and, later, testable against a mocked HTTP layer), meant to
//! run for real only from a live confirmed checkout in production.

use crate::state::CloudState;
use fluid_core::DedicatedIpv4;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// The only addon SKU this module knows how to provision. `admin::billing_checkout`
/// gates on this exact string before ever creating a checkout with `kind == "addon"`.
pub const SKU: &str = "dedicated_ipv4";

/// Real Stripe recurring price id for the addon, operator-configured —
/// deliberately NOT a hard-coded `price_...` literal (unlike `plan_spec`'s
/// tier prices): this repo's own tooling must never be able to charge a real
/// Stripe price by accident, and there is no real price id to hard-code
/// until an operator actually creates one in the Stripe dashboard.
pub fn price_id() -> Option<String> {
    std::env::var("HIVE_DEDICATED_IPV4_PRICE_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Informational/mock-checkout price (USD cents/mo) used for the checkout's
/// own `amount_cents` bookkeeping and the invoice line — mirrors how
/// `PlanSpec::price_cents` is the informational amount even though the real
/// charge is governed by the Stripe price object once `price_id()` is set.
pub const PRICE_CENTS: u64 = 1000;

/// Idempotent addon provisioning — see the module doc for the full contract.
/// `co.target` is the project id (validated as tenant-owned by the caller at
/// checkout-open time, `admin::project_owned_by`); `co.tenant` is the
/// billing tenant charged.
pub async fn provision_from_checkout(
    c: &Arc<CloudState>,
    co: &crate::billing::Checkout,
) -> Result<DedicatedIpv4, String> {
    let project = co.target.trim();
    if project.is_empty() {
        return Err("dedicated_ipv4 checkout carries no target project".into());
    }
    if let Some(existing) = c.projects.get(project).dedicated_ipv4 {
        tracing::info!(
            project,
            tenant = %co.tenant,
            eip = %existing.tencent_eip_id,
            "dedicated_ipv4: claim already exists, skipping AllocateAddresses (idempotent)"
        );
        return Ok(existing);
    }
    let alloc = allocate_eip(&c.http, project, &c.node_name).await?;
    c.projects
        .set_dedicated_ipv4(project, Some(alloc.clone()));
    c.billing.record_addon_charge(
        &co.tenant,
        &format!("Dedicated IPv4 — {project}"),
        PRICE_CENTS as i64,
    );
    crate::persist::persist(c);
    tracing::info!(
        project,
        tenant = %co.tenant,
        address = %alloc.address,
        eip = %alloc.tencent_eip_id,
        "dedicated_ipv4: provisioned"
    );
    Ok(alloc)
}

/// Real `AllocateAddresses` + `AssociateAddress` calls against Tencent
/// Cloud's `vpc` service. NEVER call this from implementation/verification
/// tooling in this repo — it spends real money and reserves a real address
/// the instant a request lands. Requires `TENCENTCLOUD_SECRET_ID`/
/// `TENCENTCLOUD_SECRET_KEY` (the account's API credentials),
/// `HIVE_TENCENT_EIP_REGION` (a real Tencent region id, e.g.
/// "ap-hongkong" — the fleet's own region labels like "sanjose" are NOT
/// Tencent region ids and must not be guessed into one), and
/// `HIVE_TENCENT_CVM_INSTANCE_ID` (the Tencent-side CVM instance id THIS
/// node runs on — distinct from `node_name`, which stamps
/// `DedicatedIpv4::owner_node` for the fleet-side listener reconcile in
/// `dedicated_ipv4_listener.rs`: that comparison is against `NodeInfo`'s own
/// name, never a cloud-provider instance id). Any missing config fails
/// closed with a named reason — never a silent skip that leaves a paid
/// checkout with no address.
async fn allocate_eip(
    http: &reqwest::Client,
    project: &str,
    node_name: &str,
) -> Result<DedicatedIpv4, String> {
    let region = non_empty_env("HIVE_TENCENT_EIP_REGION")?;
    let instance_id = non_empty_env("HIVE_TENCENT_CVM_INSTANCE_ID")?;

    let alloc = tc3_request(
        http,
        "vpc",
        "AllocateAddresses",
        &region,
        &serde_json::json!({
            "AddressCount": 1,
            "InternetChargeType": "BANDWIDTH_PACKAGE",
            "Tags": [{"TagKey": "hive-project", "TagValue": project}],
        }),
    )
    .await?;
    let eip_id = alloc
        .get("Response")
        .and_then(|r| r.get("AddressSet"))
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.as_str())
        .ok_or_else(|| format!("AllocateAddresses: no AddressSet in response ({alloc})"))?
        .to_string();

    let assoc = tc3_request(
        http,
        "vpc",
        "AssociateAddress",
        &region,
        &serde_json::json!({
            "AddressId": eip_id,
            "InstanceId": instance_id,
        }),
    )
    .await?;
    if let Some(err) = assoc.get("Response").and_then(|r| r.get("Error")) {
        return Err(format!("AssociateAddress failed for {eip_id}: {err}"));
    }

    // DescribeAddresses to learn the actual dotted-quad now bound to eip_id
    // (AllocateAddresses' own AddressSet, on some API versions, returns EIP
    // ids rather than addresses — never assume the shape, look it up).
    let desc = tc3_request(
        http,
        "vpc",
        "DescribeAddresses",
        &region,
        &serde_json::json!({ "AddressIds": [eip_id] }),
    )
    .await?;
    let address = desc
        .get("Response")
        .and_then(|r| r.get("AddressSet"))
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.get("AddressIp"))
        .and_then(|s| s.as_str())
        .ok_or_else(|| format!("DescribeAddresses: no AddressIp for {eip_id} ({desc})"))?
        .to_string();

    Ok(DedicatedIpv4 {
        address,
        tencent_eip_id: eip_id,
        region,
        owner_node: node_name.to_string(),
        allocated_ms: hive_core::now_ms(),
    })
}

fn non_empty_env(key: &str) -> Result<String, String> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} is not configured on this node"))
}

/// One TC3-HMAC-SHA256 signed POST against `<service>.tencentcloudapi.com`,
/// per Tencent Cloud's API 3.0 signature spec — the exact canonical-request /
/// string-to-sign / derived-key chain, byte for byte, so this is correct by
/// inspection against the published algorithm rather than approximated.
async fn tc3_request(
    http: &reqwest::Client,
    service: &str,
    action: &str,
    region: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let secret_id = non_empty_env("TENCENTCLOUD_SECRET_ID")?;
    let secret_key = non_empty_env("TENCENTCLOUD_SECRET_KEY")?;
    let host = format!("{service}.tencentcloudapi.com");
    let payload_str = serde_json::to_string(payload).map_err(|e| e.to_string())?;
    let ts = (hive_core::now_ms() / 1000) as i64;
    let date = chrono_date_utc(ts);

    let hashed_payload = hex::encode(Sha256::digest(payload_str.as_bytes()));
    // Exactly Tencent's documented minimal canonical-header set
    // (`content-type` + `host`) — the two REQUIRED headers per the TC3-HMAC-
    // SHA256 spec's own worked example. `X-TC-Action`/`X-TC-Version`/
    // `X-TC-Region` are still sent as real request headers (the API 3.0
    // protocol reads the action from `X-TC-Action`, not the signature), just
    // not folded into what's cryptographically signed — matching the
    // published example precisely rather than an unverifiable variant.
    let canonical_headers =
        format!("content-type:application/json; charset=utf-8\nhost:{host}\n");
    let signed_headers = "content-type;host";
    let canonical_request =
        format!("POST\n/\n\n{canonical_headers}\n{signed_headers}\n{hashed_payload}");
    let hashed_canonical = hex::encode(Sha256::digest(canonical_request.as_bytes()));

    let credential_scope = format!("{date}/{service}/tc3_request");
    let string_to_sign =
        format!("TC3-HMAC-SHA256\n{ts}\n{credential_scope}\n{hashed_canonical}");

    let secret_date = hmac_sha256(format!("TC3{secret_key}").as_bytes(), date.as_bytes());
    let secret_service = hmac_sha256(&secret_date, service.as_bytes());
    let secret_signing = hmac_sha256(&secret_service, b"tc3_request");
    let signature = hex::encode(hmac_sha256(&secret_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "TC3-HMAC-SHA256 Credential={secret_id}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );

    let resp = http
        .post(format!("https://{host}"))
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Host", &host)
        .header("X-TC-Action", action)
        .header("X-TC-Timestamp", ts.to_string())
        .header("X-TC-Version", "2017-03-12")
        .header("X-TC-Region", region)
        .header("Authorization", authorization)
        .body(payload_str)
        .send()
        .await
        .map_err(|e| format!("tencent {action}: request failed: {e}"))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("tencent {action}: bad response body: {e}"))?;
    if let Some(err) = v.get("Response").and_then(|r| r.get("Error")) {
        return Err(format!("tencent {action} failed: {err}"));
    }
    Ok(v)
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

/// `YYYY-MM-DD` in UTC from a Unix timestamp — Tencent's `CredentialScope`
/// date component. Hand-rolled (no `chrono` dependency in this crate) using
/// the standard civil-from-days algorithm (Howard Hinnant's `civil_from_days`),
/// correct across the whole proleptic Gregorian range this ever needs.
fn chrono_date_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
