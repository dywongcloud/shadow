//! Marketplace control-plane integration.
//!
//! Marketplace settlement is deliberately not a mesh capability: this module
//! accepts only authenticated service calls, verifies a configured settlement
//! authority, and then hands a normal deployment to `git`/`schedule`.  Nodes
//! never receive Marketplace credentials or a browser-provided placement choice.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{schedule, state::CloudState};

type HmacSha256 = Hmac<Sha256>;

const ADVERTISEMENT_TTL_MS: u64 = 60_000;
const INTENT_TTL_MS: u64 = 15 * 60_000;
const MARKETPLACE_CLOCK_SKEW_MS: u64 = 5 * 60_000;
const MARKETPLACE_NONCE_TTL_MS: u64 = 10 * 60_000;

#[derive(Serialize)]
struct ApiError {
    error: &'static str,
}

#[derive(Debug)]
struct MarketplaceError(StatusCode, &'static str);

impl IntoResponse for MarketplaceError {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, MarketplaceError>;

/// Replicated replay and idempotency facts.  These contain no credentials,
/// topology, or buyer transaction data beyond the immutable service records.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct MarketplaceSecuritySnapshot {
    #[serde(default)]
    nonces: BTreeMap<String, u64>,
    #[serde(default)]
    payment_intents: BTreeMap<String, PaymentIntent>,
    #[serde(default)]
    payment_idempotency: BTreeMap<String, String>,
    #[serde(default)]
    allocation_idempotency: BTreeMap<String, String>,
    #[serde(default)]
    advertisements: BTreeMap<String, InternalAdvertisement>,
}

#[derive(Default)]
pub struct MarketplaceSecurityStore(RwLock<MarketplaceSecuritySnapshot>);

impl MarketplaceSecurityStore {
    pub fn snapshot(&self) -> MarketplaceSecuritySnapshot {
        self.0.read().clone()
    }
    pub fn load(&self, snapshot: MarketplaceSecuritySnapshot) {
        *self.0.write() = snapshot;
    }
    fn consume_nonce(&self, nonce: String, expires_at_ms: u64, now: u64) -> bool {
        let mut value = self.0.write();
        value.nonces.retain(|_, expiry| *expiry > now);
        if value.nonces.contains_key(&nonce) {
            return false;
        }
        value.nonces.insert(nonce, expires_at_ms);
        true
    }
    fn put_advertisement(&self, advertisement: InternalAdvertisement) {
        let mut value = self.0.write();
        value
            .advertisements
            .retain(|_, item| item.expires_at_ms > hive_core::now_ms());
        value
            .advertisements
            .insert(advertisement.id.clone(), advertisement);
    }
    fn advertisement(&self, id: &str) -> Option<InternalAdvertisement> {
        self.0.read().advertisements.get(id).cloned()
    }
    fn intent_for_key(&self, key: &str) -> Option<PaymentIntent> {
        let value = self.0.read();
        value
            .payment_idempotency
            .get(key)
            .and_then(|id| value.payment_intents.get(id))
            .cloned()
    }
    fn put_intent_if_absent(&self, key: String, intent: PaymentIntent) -> PaymentIntent {
        let mut value = self.0.write();
        if let Some(existing_id) = value.payment_idempotency.get(&key) {
            if let Some(existing) = value.payment_intents.get(existing_id) {
                return existing.clone();
            }
        }
        value.payment_idempotency.insert(key, intent.id.clone());
        value
            .payment_intents
            .insert(intent.id.clone(), intent.clone());
        intent
    }
    fn intent(&self, id: &str) -> Option<PaymentIntent> {
        self.0.read().payment_intents.get(id).cloned()
    }
    fn update_intent(&self, intent: PaymentIntent) {
        self.0
            .write()
            .payment_intents
            .insert(intent.id.clone(), intent);
    }
    fn bind_allocation_key(&self, key: String, order_id: String) {
        self.0.write().allocation_idempotency.insert(key, order_id);
    }
    fn allocation_key(&self, key: &str) -> Option<String> {
        self.0.read().allocation_idempotency.get(key).cloned()
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct InternalAdvertisement {
    id: String,
    expires_at_ms: u64,
    node_ids: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct FeeSplit {
    gross_amount_atomic: String,
    provider_recipient: String,
    provider_amount_atomic: String,
    fee_recipient: String,
    fee_amount_atomic: String,
    fee_bps: u16,
}

#[derive(Clone, Serialize, Deserialize)]
struct PaymentIntent {
    id: String,
    order_reference: String,
    allocation: AllocationRequest,
    amount_atomic: String,
    expires_at_ms: u64,
    settlement: SettlementConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_split: Option<FeeSplit>,
    verification_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    transaction_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceRequirements {
    #[serde(default)]
    pub vcpu: u32,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub disk_gb: u64,
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Allocation {
    pub marketplace_order_id: String,
    pub tenant_id: String,
    pub resources: ResourceRequirements,
    pub approved_node_ids: Vec<String>,
    pub theo_amount: String,
    pub expires_at_ms: u64,
    pub contract_reference: String,
    pub advertisement_id: String,
    pub status: String,
    pub routed_build_id: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Default)]
pub struct AllocationStore(RwLock<BTreeMap<String, Allocation>>);

impl AllocationStore {
    pub fn snapshot(&self) -> Vec<Allocation> {
        self.0.read().values().cloned().collect()
    }
    pub fn load(&self, rows: Vec<Allocation>) {
        let mut values = self.0.write();
        values.clear();
        values.extend(
            rows.into_iter()
                .map(|row| (row.marketplace_order_id.clone(), row)),
        );
    }
    fn get(&self, id: &str) -> Option<Allocation> {
        self.0.read().get(id).cloned()
    }
    fn put_if_absent(&self, row: Allocation) -> Result<Allocation, Allocation> {
        let mut values = self.0.write();
        if let Some(old) = values.get(&row.marketplace_order_id) {
            return Err(old.clone());
        }
        values.insert(row.marketplace_order_id.clone(), row.clone());
        Ok(row)
    }
    fn update(&self, row: Allocation) {
        self.0.write().insert(row.marketplace_order_id.clone(), row);
    }
    /// Atomically claim an accepted allocation for one routing attempt.
    fn begin_route(&self, id: &str, now: u64) -> Result<Allocation, Allocation> {
        let mut values = self.0.write();
        let row = values
            .get_mut(id)
            .expect("caller checked allocation exists");
        if row.routed_build_id.is_some() || row.status == "routing" || row.status == "fulfilled" {
            return Err(row.clone());
        }
        row.status = "routing".into();
        row.updated_at_ms = now;
        Ok(row.clone())
    }
}

#[derive(Serialize)]
struct AdvertisementResponse {
    advertisement_id: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
    attestation: String,
    settlement: SettlementConfig,
    nodes: Vec<Value>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SettlementConfig {
    chain_id: String,
    token_contract: String,
    token_decimals: u8,
    confirmation_policy: u64,
    configuration_reference: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct AllocationRequest {
    marketplace_order_id: String,
    tenant_id: String,
    resources: ResourceRequirements,
    #[serde(default)]
    approved_node_ids: Vec<String>,
    theo_amount: String,
    expires_at_ms: u64,
    contract_reference: String,
    advertisement_id: String,
    #[serde(default)]
    advertisement_attestation: String,
}

#[derive(Deserialize)]
struct PaymentIntentRequest {
    allocation: AllocationRequest,
    idempotency_key: String,
    gross_amount_atomic: String,
}

#[derive(Deserialize)]
struct PaymentVerificationRequest {
    payment_intent_id: String,
    transaction_hash: String,
}

#[derive(Deserialize)]
struct MarketplaceAllocationRequest {
    payment_intent_id: String,
    allocation: AllocationRequest,
    idempotency_key: String,
    deployment: fluid_core::GitDeployRequest,
}

#[derive(Deserialize)]
struct RouteRequest {
    // The deployment payload is ordinary DevHub build input, but placement and
    // orchestration fields below are always overwritten by the backend.
    deployment: fluid_core::GitDeployRequest,
}

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route("/v1/marketplace/l0/advertisements", get(advertise_capacity))
        .route(
            "/v1/marketplace/settlement-config",
            get(get_settlement_config),
        )
        .route(
            "/v1/marketplace/payment-intents",
            post(create_payment_intent),
        )
        .route("/v1/marketplace/payments/verify", post(verify_payment))
        .route("/v1/marketplace/l0/allocations", post(submit_allocation))
}

/// DevHub operator visibility is intentionally a separate route from the
/// Marketplace service API: an operator never needs the Marketplace credential
/// merely to inspect what the control plane has accepted.
pub async fn operator_view(
    State(cloud): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::admin::require_operator(claims.as_ref().map(|claim| &claim.0))?;
    let eligible: Vec<Value> = cloud
        .registry
        .nodes()
        .iter()
        .filter(|node| eligible_node(&cloud, node))
        .map(|node| {
            json!({
                "node_id": node.id,
                "region": node.region,
                "backend": node.backend,
                "cpu_cores": node.cpu_cores,
                "memory_mb": node.mem_total_mb,
                "disk_free_gb": node.disk_free_gb,
                "gpu_count": node.gpu_count,
                "wasmer": node.wasm_runtime == Some(true),
                "bun": node.bun_runtime == Some(true)
            })
        })
        .collect();
    Ok(Json(json!({
        "eligible_nodes": eligible,
        "allocations": cloud.marketplace_allocations.snapshot()
    })))
}

fn marketplace_key() -> Result<Vec<u8>, (StatusCode, String)> {
    std::env::var("HIVE_MARKETPLACE_API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.into_bytes())
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "Marketplace integration is not configured".into(),
        ))
}

fn require_marketplace(headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let secret = marketplace_key()?;
    let supplied = headers
        .get("x-marketplace-key")
        .and_then(|value| value.to_str().ok())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "missing Marketplace authentication".into(),
        ))?;
    let mut expected =
        HmacSha256::new_from_slice(&secret).expect("HMAC accepts arbitrary key length");
    expected.update(b"hive-marketplace-v1");
    expected
        .verify_slice(&hex::decode(supplied).unwrap_or_default())
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                "invalid Marketplace authentication".into(),
            )
        })
}

fn api_error(status: StatusCode, error: &'static str) -> MarketplaceError {
    MarketplaceError(status, error)
}

/// Authenticate the complete request before parsing it.  The nonce is kept in
/// replicated durable state because reads may land on a different API node.
fn verify_marketplace_request(
    cloud: &Arc<CloudState>,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), MarketplaceError> {
    let secret = std::env::var("HIVE_MARKETPLACE_HMAC_SECRET")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| api_error(StatusCode::SERVICE_UNAVAILABLE, "marketplace_unavailable"))?;
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    let timestamp = header("x-marketplace-timestamp")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    let nonce = header("x-marketplace-nonce")
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    let signature = header("x-marketplace-signature")
        .and_then(|value| hex::decode(value).ok())
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    let now = hive_core::now_ms();
    if now.abs_diff(timestamp) > MARKETPLACE_CLOCK_SKEW_MS {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "marketplace_request_expired",
        ));
    }
    let digest = hex::encode(Sha256::digest(body));
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        path,
        timestamp,
        nonce,
        digest
    );
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts arbitrary key length");
    mac.update(canonical.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| api_error(StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    if !cloud.marketplace_security.consume_nonce(
        nonce.to_string(),
        now + MARKETPLACE_NONCE_TTL_MS,
        now,
    ) {
        return Err(api_error(
            StatusCode::CONFLICT,
            "marketplace_nonce_replayed",
        ));
    }
    crate::persist::persist(cloud);
    Ok(())
}

fn settlement_config() -> Result<SettlementConfig, (StatusCode, String)> {
    let required = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or((
                StatusCode::SERVICE_UNAVAILABLE,
                format!("{name} is not configured"),
            ))
    };
    Ok(SettlementConfig {
        // Reuse the existing THEO settlement settings; Marketplace must not
        // become a second source for contract, treasury, decimals, or chain.
        chain_id: required("THEO_CHAIN_ID")?,
        token_contract: required("THEO_TOKEN_ADDRESS")?,
        token_decimals: required("THEO_TOKEN_DECIMALS")?.parse().map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "THEO_TOKEN_DECIMALS is invalid".into(),
            )
        })?,
        confirmation_policy: required("THEO_REQUIRED_CONFIRMATIONS")?
            .parse()
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "THEO_REQUIRED_CONFIRMATIONS is invalid".into(),
                )
            })?,
        configuration_reference: std::env::var(
            "HIVE_MARKETPLACE_SETTLEMENT_CONFIGURATION_REFERENCE",
        )
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "theo-v1".into()),
    })
}

fn fee_split(gross_amount_atomic: &str) -> Option<FeeSplit> {
    let fee_bps = std::env::var("HIVE_MARKETPLACE_FEE_BPS")
        .ok()?
        .parse::<u16>()
        .ok()?;
    let provider_recipient = std::env::var("HIVE_MARKETPLACE_PROVIDER_RECIPIENT").ok()?;
    let fee_recipient = std::env::var("HIVE_MARKETPLACE_FEE_RECIPIENT").ok()?;
    // Fee splitting needs the separately deployed contract that enforces and
    // emits both recipient legs. Configuration alone cannot make a plain
    // ERC-20 transfer safe to settle.
    std::env::var("HIVE_MARKETPLACE_FEE_SPLIT_CONTRACT").ok()?;
    if fee_bps > 10_000 || provider_recipient.is_empty() || fee_recipient.is_empty() {
        return None;
    }
    let gross = gross_amount_atomic.parse::<u128>().ok()?;
    let fee = gross.checked_mul(fee_bps as u128)?.checked_div(10_000)?;
    Some(FeeSplit {
        gross_amount_atomic: gross.to_string(),
        provider_recipient,
        provider_amount_atomic: gross.checked_sub(fee)?.to_string(),
        fee_recipient,
        fee_amount_atomic: fee.to_string(),
        fee_bps,
    })
}

async fn get_settlement_config(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
) -> ApiResult<Value> {
    verify_marketplace_request(
        &cloud,
        "GET",
        "/v1/marketplace/settlement-config",
        &headers,
        &[],
    )?;
    let config = settlement_config()
        .map_err(|_| api_error(StatusCode::SERVICE_UNAVAILABLE, "settlement_unavailable"))?;
    Ok(Json(json!({
        "chain_id": config.chain_id,
        "token_contract": config.token_contract,
        "token_decimals": config.token_decimals,
        "confirmation_policy": config.confirmation_policy,
        "configuration_reference": config.configuration_reference,
    })))
}

async fn advertise_capacity(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
) -> ApiResult<Value> {
    verify_marketplace_request(
        &cloud,
        "GET",
        "/v1/marketplace/l0/advertisements",
        &headers,
        &[],
    )?;
    let now = hive_core::now_ms();
    let eligible: Vec<_> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|node| eligible_node(&cloud, node))
        .collect();
    let internal = InternalAdvertisement {
        id: format!("adv_{}", Uuid::new_v4()),
        expires_at_ms: now + ADVERTISEMENT_TTL_MS,
        node_ids: eligible.iter().map(|node| node.id.clone()).collect(),
    };
    cloud
        .marketplace_security
        .put_advertisement(internal.clone());
    crate::persist::persist(&cloud);
    // The public service contract intentionally aggregates capacity. Node
    // identities, addresses, relays, and connection state never leave DevHub.
    Ok(Json(json!({
        "advertisement_id": internal.id,
        "issued_at": now,
        "expires_at": internal.expires_at_ms,
        "capacity": {
            "available_units": internal.node_ids.len(),
            "gpu_units": eligible.iter().filter(|node| node.gpu_count > 0).count(),
        }
    })))
}

async fn create_payment_intent(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<PaymentIntent> {
    verify_marketplace_request(
        &cloud,
        "POST",
        "/v1/marketplace/payment-intents",
        &headers,
        &body,
    )?;
    let request: PaymentIntentRequest = serde_json::from_slice(&body)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if request.idempotency_key.is_empty() || request.idempotency_key.len() > 128 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_idempotency_key",
        ));
    }
    if let Some(intent) = cloud
        .marketplace_security
        .intent_for_key(&request.idempotency_key)
    {
        return Ok(Json(intent));
    }
    let advertisement = cloud
        .marketplace_security
        .advertisement(&request.allocation.advertisement_id)
        .filter(|advertisement| advertisement.expires_at_ms > hive_core::now_ms())
        .ok_or_else(|| api_error(StatusCode::CONFLICT, "advertisement_unavailable"))?;
    if request.allocation.marketplace_order_id.trim().is_empty()
        || request.allocation.tenant_id.trim().is_empty()
        || request.allocation.expires_at_ms <= hive_core::now_ms()
        || request.allocation.expires_at_ms > advertisement.expires_at_ms
        || !request.allocation.approved_node_ids.is_empty()
    {
        return Err(api_error(StatusCode::BAD_REQUEST, "invalid_allocation"));
    }
    let settlement = settlement_config()
        .map_err(|_| api_error(StatusCode::SERVICE_UNAVAILABLE, "settlement_unavailable"))?;
    let split = fee_split(&request.gross_amount_atomic)
        .ok_or_else(|| api_error(StatusCode::SERVICE_UNAVAILABLE, "settlement_unavailable"))?;
    let intent = PaymentIntent {
        id: format!("pi_{}", Uuid::new_v4().simple()),
        order_reference: request.allocation.marketplace_order_id.clone(),
        allocation: request.allocation,
        amount_atomic: split.gross_amount_atomic.clone(),
        expires_at_ms: hive_core::now_ms() + INTENT_TTL_MS,
        settlement,
        fee_split: Some(split),
        verification_status: "pending".into(),
        transaction_hash: None,
    };
    let intent = cloud
        .marketplace_security
        .put_intent_if_absent(request.idempotency_key, intent);
    crate::persist::persist(&cloud);
    Ok(Json(intent))
}

async fn verify_payment(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Value> {
    verify_marketplace_request(
        &cloud,
        "POST",
        "/v1/marketplace/payments/verify",
        &headers,
        &body,
    )?;
    let request: PaymentVerificationRequest = serde_json::from_slice(&body)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let mut intent = cloud
        .marketplace_security
        .intent(&request.payment_intent_id)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "payment_intent_not_found"))?;
    if intent.expires_at_ms <= hive_core::now_ms() {
        intent.verification_status = "failed".into();
    } else if intent
        .transaction_hash
        .as_deref()
        .is_some_and(|hash| hash != request.transaction_hash)
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            "payment_intent_already_bound",
        ));
    } else if !valid_transaction_hash(&request.transaction_hash) {
        intent.verification_status = "failed".into();
    } else {
        intent.transaction_hash = Some(request.transaction_hash);
        intent.verification_status = verify_payment_chain(&cloud, &intent).await;
    }
    cloud.marketplace_security.update_intent(intent.clone());
    crate::persist::persist(&cloud);
    Ok(Json(json!({
        "payment_intent_id": intent.id,
        "status": intent.verification_status,
        "expires_at": intent.expires_at_ms,
    })))
}

fn valid_transaction_hash(hash: &str) -> bool {
    hash.len() == 66
        && hash.starts_with("0x")
        && hash[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// This verifies chain identity, successful inclusion and confirmation count
/// locally. `verified` is deliberately withheld until the fee-split contract
/// provides its canonical event ABI: inspecting generic calldata cannot prove
/// both recipient legs or the order reference.
async fn verify_payment_chain(cloud: &CloudState, intent: &PaymentIntent) -> String {
    let Some(rpc_url) = std::env::var("THEO_RPC_URL")
        .ok()
        .filter(|url| url.starts_with("https://"))
    else {
        return "failed".into();
    };
    let Some(tx_hash) = intent.transaction_hash.as_deref() else {
        return "pending".into();
    };
    let rpc = |method, params| async {
        cloud
            .http
            .post(&rpc_url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()?
            .get("result")
            .cloned()
    };
    let Some(chain) = rpc("eth_chainId", json!([]))
        .await
        .and_then(|value| value.as_str().map(str::to_owned))
    else {
        return "pending".into();
    };
    let expected = intent
        .settlement
        .chain_id
        .parse::<u64>()
        .ok()
        .map(|id| format!("0x{id:x}"));
    if Some(chain.to_ascii_lowercase()) != expected.map(|id| id.to_ascii_lowercase()) {
        return "failed".into();
    }
    let Some(receipt) = rpc("eth_getTransactionReceipt", json!([tx_hash])).await else {
        return "pending".into();
    };
    if receipt.is_null() {
        return "pending".into();
    }
    if receipt.get("status").and_then(Value::as_str) != Some("0x1") {
        return "failed".into();
    }
    let Some(block) = receipt
        .get("blockNumber")
        .and_then(Value::as_str)
        .and_then(|value| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok())
    else {
        return "failed".into();
    };
    let Some(tip) = rpc("eth_blockNumber", json!([])).await.and_then(|value| {
        value
            .as_str()
            .and_then(|value| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok())
    }) else {
        return "pending".into();
    };
    if tip.saturating_sub(block).saturating_add(1) < intent.settlement.confirmation_policy {
        return "awaiting_confirmations".into();
    }
    // Never manufacture a verified result without the contract's event
    // semantics. The contract integration supplies this verifier in a follow-up.
    "failed".into()
}

async fn submit_allocation(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Value> {
    verify_marketplace_request(
        &cloud,
        "POST",
        "/v1/marketplace/l0/allocations",
        &headers,
        &body,
    )?;
    let request: MarketplaceAllocationRequest = serde_json::from_slice(&body)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if request.idempotency_key.is_empty() || request.idempotency_key.len() > 128 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_idempotency_key",
        ));
    }
    if let Some(order_id) = cloud
        .marketplace_security
        .allocation_key(&request.idempotency_key)
    {
        let allocation = cloud
            .marketplace_allocations
            .get(&order_id)
            .ok_or_else(|| api_error(StatusCode::CONFLICT, "allocation_idempotency_conflict"))?;
        return Ok(Json(
            json!({"allocation_id": allocation.marketplace_order_id, "status": allocation.status}),
        ));
    }
    let intent = cloud
        .marketplace_security
        .intent(&request.payment_intent_id)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "payment_intent_not_found"))?;
    if intent.verification_status != "verified" {
        return Ok(Json(
            json!({"payment_intent_id": intent.id, "status": intent.verification_status}),
        ));
    }
    if serde_json::to_vec(&intent.allocation).ok() != serde_json::to_vec(&request.allocation).ok() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "allocation_does_not_match_payment_intent",
        ));
    }
    let advertisement = cloud
        .marketplace_security
        .advertisement(&request.allocation.advertisement_id)
        .filter(|advertisement| advertisement.expires_at_ms > hive_core::now_ms());
    let Some(advertisement) = advertisement else {
        return Ok(Json(json!({"status": "capacity_no_longer_available"})));
    };
    if request.allocation.marketplace_order_id.trim().is_empty()
        || request.allocation.tenant_id.trim().is_empty()
        || request.allocation.expires_at_ms <= hive_core::now_ms()
        || request.allocation.expires_at_ms > advertisement.expires_at_ms
        || !request.allocation.approved_node_ids.is_empty()
    {
        return Ok(Json(json!({"status": "capacity_no_longer_available"})));
    }
    let approved: HashSet<String> = advertisement.node_ids.iter().cloned().collect();
    let regions: Vec<String> = request
        .allocation
        .resources
        .region
        .iter()
        .cloned()
        .collect();
    if schedule::place(
        &cloud,
        &regions,
        false,
        false,
        request.allocation.resources.gpu,
        schedule::InterpreterNeeds::default(),
        false,
        false,
        Some(&approved),
    )
    .is_empty()
    {
        return Ok(Json(json!({"status": "capacity_no_longer_available"})));
    }
    let now = hive_core::now_ms();
    let allocation = Allocation {
        marketplace_order_id: request.allocation.marketplace_order_id.clone(),
        tenant_id: request.allocation.tenant_id.clone(),
        resources: request.allocation.resources.clone(),
        approved_node_ids: advertisement.node_ids.clone(),
        theo_amount: intent.amount_atomic.clone(),
        expires_at_ms: request.allocation.expires_at_ms,
        contract_reference: intent.id.clone(),
        advertisement_id: request.allocation.advertisement_id.clone(),
        status: "submitted".into(),
        routed_build_id: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let allocation = match cloud.marketplace_allocations.put_if_absent(allocation) {
        Ok(value) => value,
        Err(existing) => {
            return Ok(Json(
                json!({"allocation_id": existing.marketplace_order_id, "status": existing.status}),
            ))
        }
    };
    cloud.marketplace_security.bind_allocation_key(
        request.idempotency_key,
        allocation.marketplace_order_id.clone(),
    );
    crate::persist::persist(&cloud);
    let mut deployment = request.deployment;
    deployment.no_fanout = false;
    deployment.fanout_secondary = false;
    deployment.project_incarnation = None;
    deployment.marketplace_placement = Some(fluid_core::MarketplacePlacementSnapshot {
        contract_version: 1,
        policy_version: 1,
        marketplace_order_id: allocation.marketplace_order_id.clone(),
        buyer_tenant_id: allocation.tenant_id.clone(),
        retrieved_at_ms: now,
        approved_node_ids: allocation.approved_node_ids.clone(),
        policy: json!({"source":"marketplace-payment-intent","payment_intent_id":intent.id}),
    });
    match crate::admin::start_named_deploy(&cloud, &allocation.tenant_id, deployment, None).await {
        Ok(result) => {
            let mut stored = allocation;
            stored.status = "scheduled".into();
            stored.routed_build_id = result["build_id"].as_str().map(str::to_owned);
            stored.updated_at_ms = hive_core::now_ms();
            cloud.marketplace_allocations.update(stored.clone());
            crate::persist::persist(&cloud);
            Ok(Json(
                json!({"allocation_id": stored.marketplace_order_id, "status": "scheduled", "build_id": stored.routed_build_id}),
            ))
        }
        Err(_) => {
            let mut stored = allocation;
            stored.status = "failed".into();
            stored.updated_at_ms = hive_core::now_ms();
            cloud.marketplace_allocations.update(stored.clone());
            crate::persist::persist(&cloud);
            Ok(Json(
                json!({"allocation_id": stored.marketplace_order_id, "status": "failed"}),
            ))
        }
    }
}

fn attestation_secret() -> Result<Vec<u8>, (StatusCode, String)> {
    std::env::var("HIVE_MARKETPLACE_ADVERTISEMENT_SECRET")
        .ok()
        .filter(|value| !value.is_empty())
        .map(String::into_bytes)
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "HIVE_MARKETPLACE_ADVERTISEMENT_SECRET is not configured".into(),
        ))
}

#[derive(Serialize, Deserialize)]
struct Attestation {
    id: String,
    expires_at_ms: u64,
    node_ids: Vec<String>,
}

fn sign_attestation(value: &Attestation) -> Result<String, (StatusCode, String)> {
    let payload = serde_json::to_vec(value).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "attestation serialization failed".into(),
        )
    })?;
    let mut mac = HmacSha256::new_from_slice(&attestation_secret()?)
        .expect("HMAC accepts arbitrary key length");
    mac.update(&payload);
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    ))
}

fn verify_attestation(id: &str, token: &str) -> Result<Attestation, (StatusCode, String)> {
    let (payload, tag) = token.split_once('.').ok_or((
        StatusCode::BAD_REQUEST,
        "malformed advertisement attestation".into(),
    ))?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "malformed advertisement attestation".into(),
        )
    })?;
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "malformed advertisement attestation".into(),
        )
    })?;
    let mut mac = HmacSha256::new_from_slice(&attestation_secret()?)
        .expect("HMAC accepts arbitrary key length");
    mac.update(&payload);
    mac.verify_slice(&tag).map_err(|_| {
        (
            StatusCode::FORBIDDEN,
            "invalid advertisement attestation".into(),
        )
    })?;
    let value: Attestation = serde_json::from_slice(&payload).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "malformed advertisement attestation".into(),
        )
    })?;
    if value.id != id || value.expires_at_ms <= hive_core::now_ms() {
        return Err((
            StatusCode::CONFLICT,
            "advertisement is expired or does not match".into(),
        ));
    }
    Ok(value)
}

fn eligible_node(cloud: &CloudState, node: &hive_edge::NodeInfo) -> bool {
    let connected = node.name == cloud.node_name
        || cloud.node_admins.read().contains_key(&node.name)
        || (node.peer_id.is_some() && node.iroh_addr.is_some());
    node.healthy
        && connected
        && (node.backend == "firecracker"
            || (node.backend == "litebox" && schedule::runtime_artifact_capable(node, true)))
        && node.mem_total_mb >= 1024
        && (node.disk_free_gb == 0 || node.disk_free_gb >= 20)
}

async fn advertise_nodes(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
) -> Result<Json<AdvertisementResponse>, (StatusCode, String)> {
    require_marketplace(&headers)?;
    let settlement = settlement_config()?;
    let now = hive_core::now_ms();
    let nodes: Vec<Value> = cloud.registry.nodes().iter().filter(|node| eligible_node(&cloud, node)).map(|node| {
        let runtimes: Vec<&str> = [(node.wasm_runtime == Some(true)).then_some("wasmer"), (node.bun_runtime == Some(true)).then_some("bun")]
            .into_iter().flatten().collect();
        json!({"node_id": node.id, "region": node.region, "capabilities": {
            "backend": node.backend, "cpu_cores": node.cpu_cores, "memory_mb": node.mem_total_mb,
            "gpu_count": node.gpu_count, "gpu_model": node.gpu_model, "gpu_vram_mb": node.gpu_vram_mb
        }, "available_capacity": {"disk_free_gb": node.disk_free_gb, "gpu_free_mb": node.gpu_free_mb},
        "supported_runtimes": runtimes, "pricing": marketplace_pricing()})
    }).collect();
    let attested = Attestation {
        id: format!("adv_{}", Uuid::new_v4()),
        expires_at_ms: now + ADVERTISEMENT_TTL_MS,
        node_ids: nodes
            .iter()
            .filter_map(|node| node["node_id"].as_str().map(str::to_string))
            .collect(),
    };
    let attestation = sign_attestation(&attested)?;
    cloud.audit.record(
        "marketplace",
        "marketplace-service",
        "issue",
        "marketplace_advertisement",
        &attested.id,
        &format!(
            "nodes={} expires_at_ms={}",
            attested.node_ids.len(),
            attested.expires_at_ms
        ),
    );
    Ok(Json(AdvertisementResponse {
        advertisement_id: attested.id,
        issued_at_ms: now,
        expires_at_ms: attested.expires_at_ms,
        attestation,
        settlement,
        nodes,
    }))
}

fn marketplace_pricing() -> Value {
    json!({"currency": "THEO", "terms_reference": std::env::var("HIVE_MARKETPLACE_PRICING_TERMS").unwrap_or_default()})
}

async fn create_allocation(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    Json(request): Json<AllocationRequest>,
) -> Result<Json<Allocation>, (StatusCode, String)> {
    require_marketplace(&headers)?;
    let _config = settlement_config()?;
    let attestation = verify_attestation(
        &request.advertisement_id,
        &request.advertisement_attestation,
    )?;
    validate_allocation_request(&request, &attestation)?;
    verify_settlement(
        &cloud,
        &request.marketplace_order_id,
        &request.contract_reference,
        &request.theo_amount,
    )
    .await?;
    let now = hive_core::now_ms();
    let tenant_id = request.tenant_id.clone();
    let contract_reference = request.contract_reference.clone();
    let row = Allocation {
        marketplace_order_id: request.marketplace_order_id,
        tenant_id,
        resources: request.resources,
        approved_node_ids: request.approved_node_ids,
        theo_amount: request.theo_amount,
        expires_at_ms: request.expires_at_ms,
        contract_reference,
        advertisement_id: request.advertisement_id,
        status: "accepted".into(),
        routed_build_id: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    match cloud.marketplace_allocations.put_if_absent(row) {
        Ok(row) => {
            crate::persist::persist(&cloud);
            cloud.audit.record(
                &row.tenant_id,
                "marketplace-service",
                "accept",
                "marketplace_allocation",
                &row.marketplace_order_id,
                "settlement verified",
            );
            Ok(Json(row))
        }
        Err(existing) => {
            if existing.tenant_id == request.tenant_id
                && existing.contract_reference == request.contract_reference
            {
                Ok(Json(existing))
            } else {
                Err((
                    StatusCode::CONFLICT,
                    "marketplace order id is already bound to a different allocation".into(),
                ))
            }
        }
    }
}

fn validate_allocation_request(
    request: &AllocationRequest,
    attestation: &Attestation,
) -> Result<(), (StatusCode, String)> {
    if request.marketplace_order_id.trim().is_empty()
        || request.tenant_id.trim().is_empty()
        || request.contract_reference.trim().is_empty()
        || request.theo_amount.trim().is_empty()
        || request.expires_at_ms <= hive_core::now_ms()
        || request.approved_node_ids.is_empty()
        || request.expires_at_ms > attestation.expires_at_ms
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "allocation has missing or expired required fields".into(),
        ));
    }
    let approved: HashSet<_> = request.approved_node_ids.iter().collect();
    if approved.len() != request.approved_node_ids.len()
        || !approved.iter().all(|id| attestation.node_ids.contains(*id))
    {
        return Err((
            StatusCode::FORBIDDEN,
            "approved nodes are not a unique subset of the live advertisement".into(),
        ));
    }
    Ok(())
}

async fn verify_settlement(
    cloud: &CloudState,
    order_id: &str,
    contract_reference: &str,
    amount: &str,
) -> Result<(), (StatusCode, String)> {
    let url = std::env::var("HIVE_MARKETPLACE_SETTLEMENT_VERIFY_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "HIVE_MARKETPLACE_SETTLEMENT_VERIFY_URL is not configured".into(),
        ))?;
    let key = std::env::var("HIVE_MARKETPLACE_SETTLEMENT_API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "HIVE_MARKETPLACE_SETTLEMENT_API_KEY is not configured".into(),
        ))?;
    let config = settlement_config()?;
    let treasury = std::env::var("THEO_TREASURY_ADDRESS").unwrap_or_default();
    let response = cloud.http.post(url).header("x-marketplace-settlement-key", key).json(&json!({
        "order_id": order_id, "contract_reference": contract_reference, "amount": amount,
        "chain_id": config.chain_id, "token_contract": config.token_contract, "treasury": treasury,
        "decimals": config.token_decimals, "required_confirmations": config.confirmation_policy
    })).send().await.map_err(|_| (StatusCode::BAD_GATEWAY, "settlement verifier is unavailable".into()))?;
    let verified: Value = response.json().await.map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            "settlement verifier returned invalid data".into(),
        )
    })?;
    if !verified["confirmed"].as_bool().unwrap_or(false)
        || verified["order_id"].as_str() != Some(order_id)
        || verified["contract_reference"].as_str() != Some(contract_reference)
        || verified["amount"].as_str() != Some(amount)
        || verified["chain_id"].as_str() != Some(&config.chain_id)
        || verified["token_contract"].as_str() != Some(&config.token_contract)
        || verified["treasury"].as_str() != Some(treasury.as_str())
        || verified["decimals"].as_u64() != Some(config.token_decimals as u64)
        || verified["confirmations"].as_u64().unwrap_or(0) < config.confirmation_policy
    {
        return Err((
            StatusCode::CONFLICT,
            "settlement is not confirmed for this allocation".into(),
        ));
    }
    Ok(())
}

async fn list_allocations(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<Allocation>>, (StatusCode, String)> {
    require_marketplace(&headers)?;
    Ok(Json(cloud.marketplace_allocations.snapshot()))
}
async fn get_allocation(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(order_id): Path<String>,
) -> Result<Json<Allocation>, (StatusCode, String)> {
    require_marketplace(&headers)?;
    cloud
        .marketplace_allocations
        .get(&order_id)
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "allocation not found".into()))
}

async fn route_allocation(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(order_id): Path<String>,
    Json(mut request): Json<RouteRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_marketplace(&headers)?;
    let allocation = cloud
        .marketplace_allocations
        .get(&order_id)
        .ok_or((StatusCode::NOT_FOUND, "allocation not found".into()))?;
    if allocation.expires_at_ms <= hive_core::now_ms() || allocation.status == "revoked" {
        return Err((
            StatusCode::CONFLICT,
            "allocation is expired or revoked".into(),
        ));
    }
    verify_settlement(
        &cloud,
        &allocation.marketplace_order_id,
        &allocation.contract_reference,
        &allocation.theo_amount,
    )
    .await?;
    let approved: HashSet<String> = allocation.approved_node_ids.iter().cloned().collect();
    let regions: Vec<String> = allocation.resources.region.iter().cloned().collect();
    let targets = schedule::place(
        &cloud,
        &regions,
        false,
        false,
        allocation.resources.gpu,
        schedule::InterpreterNeeds::default(),
        false,
        false,
        Some(&approved),
    );
    if targets.is_empty() {
        cloud.audit.record(
            &allocation.tenant_id,
            "marketplace-service",
            "reject",
            "marketplace_route",
            &order_id,
            "no approved node currently eligible",
        );
        return Err((
            StatusCode::CONFLICT,
            "no advertised approved node is currently eligible".into(),
        ));
    }
    if let Some(build_id) = allocation.routed_build_id.clone() {
        return Ok(Json(
            json!({"order_id": order_id, "build_id": build_id, "status": allocation.status}),
        ));
    }
    let mut allocation = match cloud
        .marketplace_allocations
        .begin_route(&order_id, hive_core::now_ms())
    {
        Ok(row) => row,
        Err(existing) if let Some(build_id) = existing.routed_build_id.clone() => {
            return Ok(Json(
                json!({"order_id": order_id, "build_id": build_id, "status": existing.status}),
            ));
        }
        Err(_) => {
            return Err((
                StatusCode::CONFLICT,
                "allocation routing is already in progress".into(),
            ))
        }
    };
    crate::persist::persist(&cloud);
    // Marketplace cannot submit a per-target/internal deployment. Those flags
    // skip `git.rs`'s scheduler and would bypass the approved-node allowlist.
    request.deployment.no_fanout = false;
    request.deployment.fanout_secondary = false;
    request.deployment.project_incarnation = None;
    request.deployment.marketplace_placement = Some(fluid_core::MarketplacePlacementSnapshot {
        contract_version: 1,
        policy_version: 1,
        marketplace_order_id: allocation.marketplace_order_id.clone(),
        buyer_tenant_id: allocation.tenant_id.clone(),
        retrieved_at_ms: hive_core::now_ms(),
        approved_node_ids: allocation.approved_node_ids.clone(),
        policy: json!({"source":"hive-marketplace-allocation","contract_reference":allocation.contract_reference,"expires_at_ms":allocation.expires_at_ms}),
    });
    let result =
        crate::admin::start_named_deploy(&cloud, &allocation.tenant_id, request.deployment, None)
            .await;
    let result = match result {
        Ok(value) => value,
        Err(error) => {
            allocation.status = "accepted".into();
            allocation.updated_at_ms = hive_core::now_ms();
            cloud.marketplace_allocations.update(allocation);
            crate::persist::persist(&cloud);
            return Err(error);
        }
    };
    let build_id = result["build_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "deployment admission returned no build id".into(),
        ))?
        .to_string();
    allocation.status = "routed".into();
    allocation.routed_build_id = Some(build_id.clone());
    allocation.updated_at_ms = hive_core::now_ms();
    cloud.marketplace_allocations.update(allocation.clone());
    crate::persist::persist(&cloud);
    cloud.audit.record(
        &allocation.tenant_id,
        "marketplace-service",
        "route",
        "marketplace_allocation",
        &order_id,
        &format!("build_id={build_id}; existing scheduler dispatch selected approved targets"),
    );
    Ok(result)
}

async fn fulfill_allocation(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    Path(order_id): Path<String>,
) -> Result<Json<Allocation>, (StatusCode, String)> {
    require_marketplace(&headers)?;
    let mut row = cloud
        .marketplace_allocations
        .get(&order_id)
        .ok_or((StatusCode::NOT_FOUND, "allocation not found".into()))?;
    if row.status == "fulfilled" {
        return Ok(Json(row));
    }
    if row.status != "routed" || row.expires_at_ms <= hive_core::now_ms() || row.status == "revoked"
    {
        return Err((
            StatusCode::CONFLICT,
            "only an active routed allocation may be fulfilled".into(),
        ));
    }
    verify_settlement(
        &cloud,
        &row.marketplace_order_id,
        &row.contract_reference,
        &row.theo_amount,
    )
    .await?;
    row.status = "fulfilled".into();
    row.updated_at_ms = hive_core::now_ms();
    cloud.marketplace_allocations.update(row.clone());
    crate::persist::persist(&cloud);
    cloud.audit.record(
        &row.tenant_id,
        "marketplace-service",
        "fulfill",
        "marketplace_allocation",
        &order_id,
        "settlement re-verified server-side",
    );
    Ok(Json(row))
}
