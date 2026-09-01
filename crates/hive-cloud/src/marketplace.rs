//! Marketplace L0 capacity allocation boundary.
//!
//! This is deliberately a narrow server-to-server API. Marketplace never gets
//! topology or credentials and DevHub never constructs or relays a buyer's
//! transaction. Settlement is accepted only after DevHub verifies its receipt.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use hmac::{Hmac, Mac};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sha3::Keccak256;
use uuid::Uuid;

use crate::{schedule, state::CloudState};

type HmacSha256 = Hmac<Sha256>;

const BASE_CHAIN_ID: u64 = 8453;
const THEO_TOKEN: &str = "0xebe516a20238f79dc20b07ead6768e08891ed309";
const FEE_BPS: u16 = 500;
const CONFIRMATIONS: u64 = 2;
const ADVERTISEMENT_TTL_MS: u64 = 60_000;
const INTENT_TTL_MS: u64 = 15 * 60_000;
const CLOCK_SKEW_MS: u64 = 5 * 60_000;
const NONCE_TTL_MS: u64 = 10 * 60_000;
const SETTLEMENT_SIGNATURE: &str =
    "Settlement(bytes32,address,address,uint256,address,uint256,address,uint256,uint256)";

#[derive(Serialize)]
struct ApiError {
    error: &'static str,
}

struct MarketplaceError(axum::http::StatusCode, &'static str);
impl IntoResponse for MarketplaceError {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}
type ApiResult<T> = Result<Json<T>, MarketplaceError>;

fn error(status: axum::http::StatusCode, value: &'static str) -> MarketplaceError {
    MarketplaceError(status, value)
}

/// Replicated, durable facts required for round-robin API requests.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct MarketplaceSecuritySnapshot {
    #[serde(default)]
    nonces: BTreeMap<String, u64>,
    #[serde(default)]
    deployments: BTreeMap<String, ListedDeployment>,
    #[serde(default)]
    payment_intents: BTreeMap<String, PaymentIntent>,
    #[serde(default)]
    payment_idempotency: BTreeMap<String, String>,
    #[serde(default)]
    allocation_idempotency: BTreeMap<String, String>,
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
    fn consume_nonce(&self, key: String, now: u64) -> bool {
        let mut state = self.0.write();
        state.nonces.retain(|_, expires| *expires > now);
        if state.nonces.contains_key(&key) {
            return false;
        }
        state.nonces.insert(key, now + NONCE_TTL_MS);
        true
    }
    fn replace_deployments(&self, listed: Vec<ListedDeployment>) {
        let mut state = self.0.write();
        state.deployments.retain(|_, entry| entry.expires_at_ms > hive_core::now_ms());
        for entry in listed {
            state.deployments.insert(entry.deployment_id.clone(), entry);
        }
    }
    fn deployment(&self, id: &str) -> Option<ListedDeployment> {
        self.0.read().deployments.get(id).cloned()
    }
    fn intent_for_key(&self, key: &str) -> Option<PaymentIntent> {
        let state = self.0.read();
        state
            .payment_idempotency
            .get(key)
            .and_then(|id| state.payment_intents.get(id))
            .cloned()
    }
    fn put_intent(&self, key: String, intent: PaymentIntent) -> PaymentIntent {
        let mut state = self.0.write();
        if let Some(id) = state.payment_idempotency.get(&key) {
            if let Some(old) = state.payment_intents.get(id) {
                return old.clone();
            }
        }
        state.payment_idempotency.insert(key, intent.payment_intent_id.clone());
        state
            .payment_intents
            .insert(intent.payment_intent_id.clone(), intent.clone());
        intent
    }
    fn intent(&self, id: &str) -> Option<PaymentIntent> {
        self.0.read().payment_intents.get(id).cloned()
    }
    fn update_intent(&self, intent: PaymentIntent) {
        self.0.write().payment_intents.insert(intent.payment_intent_id.clone(), intent);
    }
    fn allocation_for_key(&self, key: &str) -> Option<String> {
        self.0.read().allocation_idempotency.get(key).cloned()
    }
    fn bind_allocation_key(&self, key: String, id: String) {
        self.0.write().allocation_idempotency.insert(key, id);
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ListedDeployment {
    deployment_id: String,
    provider_id: String,
    canonical_node_id: String,
    provider_recipient: String,
    region: String,
    runtime: String,
    capabilities: Capabilities,
    expires_at_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Capabilities {
    vcpu: u32,
    ram_mib: u64,
    gpu: Gpu,
    storage_gib: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Gpu {
    model: Option<String>,
    count: u32,
}

/// The intentionally narrow Marketplace L0 advertisement contract.
///
/// Internal listings retain the additional scheduling data needed for
/// settlement and placement. This projection prevents that data from becoming
/// part of the Marketplace response by accident.
#[derive(Serialize)]
struct MarketplaceDeploymentsResponse {
    data: Vec<MarketplaceDeployment>,
}

#[derive(Serialize)]
struct MarketplaceDeployment {
    deployment_id: String,
    provider_id: String,
    canonical_node_id: String,
    provider_recipient: String,
    region: String,
    runtime: String,
    capabilities: MarketplaceCapabilities,
    availability: MarketplaceAvailability,
    health: &'static str,
    issued_at: String,
    expires_at: String,
    revoked_at: Option<String>,
    configuration_reference: &'static str,
}

#[derive(Serialize)]
struct MarketplaceCapabilities {
    vcpu: u32,
    ram_mib: u64,
    storage_gib: u64,
}

#[derive(Serialize)]
struct MarketplaceAvailability {
    available: bool,
    capacity_units: u8,
}

impl MarketplaceDeployment {
    fn from_listed(entry: ListedDeployment, issued_at_ms: u64) -> Self {
        Self {
            deployment_id: entry.deployment_id,
            provider_id: entry.provider_id,
            canonical_node_id: entry.canonical_node_id,
            provider_recipient: entry.provider_recipient,
            region: entry.region,
            runtime: entry.runtime,
            capabilities: MarketplaceCapabilities {
                vcpu: entry.capabilities.vcpu,
                ram_mib: entry.capabilities.ram_mib,
                storage_gib: entry.capabilities.storage_gib,
            },
            availability: MarketplaceAvailability { available: true, capacity_units: 1 },
            health: "healthy",
            issued_at: iso_millis_timestamp(issued_at_ms),
            expires_at: iso_millis_timestamp(entry.expires_at_ms),
            revoked_at: None,
            configuration_reference: "l0-config-v1",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SettlementConfig {
    currency: String,
    chain_id: u64,
    token_contract: String,
    token_decimals: u8,
    fee_recipient: String,
    fee_bps: u16,
    settlement_contract: String,
    settlement_event_signature: String,
    confirmation_policy: ConfirmationPolicy,
    configuration_reference: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct ConfirmationPolicy {
    minimum_confirmations: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct PaymentIntent {
    payment_intent_id: String,
    expires_at_ms: u64,
    order_reference: String,
    gross_amount_atomic: String,
    buyer_address: String,
    deployment: ListedDeployment,
    provider_amount_atomic: String,
    fee_amount_atomic: String,
    settlement: SettlementConfig,
    verification_status: String,
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
        let mut state = self.0.write();
        state.clear();
        state.extend(rows.into_iter().map(|row| (row.marketplace_order_id.clone(), row)));
    }
    fn get(&self, id: &str) -> Option<Allocation> {
        self.0.read().get(id).cloned()
    }
    fn put_if_absent(&self, allocation: Allocation) -> Result<Allocation, Allocation> {
        let mut state = self.0.write();
        if let Some(old) = state.get(&allocation.marketplace_order_id) {
            return Err(old.clone());
        }
        state.insert(allocation.marketplace_order_id.clone(), allocation.clone());
        Ok(allocation)
    }
    fn update(&self, allocation: Allocation) {
        self.0.write().insert(allocation.marketplace_order_id.clone(), allocation);
    }
}

#[derive(Deserialize)]
struct PaymentIntentRequest {
    deployment_id: String,
    provider_id: String,
    canonical_node_id: String,
    order_reference: String,
    gross_amount_atomic: String,
    buyer_address: String,
}
#[derive(Deserialize)]
struct PaymentVerificationRequest {
    payment_intent_id: String,
    transaction_hash: String,
}
#[derive(Deserialize)]
struct AllocationRequest {
    payment_intent_id: String,
    tenant_id: String,
    resources: ResourceRequirements,
    deployment: fluid_core::GitDeployRequest,
}

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route("/v1/marketplace/l0/deployments", get(list_deployments))
        .route("/v1/marketplace/settlement-config", get(get_settlement_config))
        .route("/v1/marketplace/payment-intents", post(create_payment_intent))
        .route("/v1/marketplace/payments/verify", post(verify_payment))
        .route("/v1/marketplace/l0/allocations", post(submit_allocation))
}

/// Operator-only visibility intentionally remains separate from the
/// Marketplace service API and exposes no Marketplace credentials.
pub async fn operator_view(
    State(cloud): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    crate::admin::require_operator(claims.as_ref().map(|claim| &claim.0))?;
    Ok(Json(json!({
        "eligible_deployments": listed_deployments(&cloud).into_iter().map(|entry| json!({
            "deployment_id": entry.deployment_id,
            "provider_id": entry.provider_id,
            "canonical_node_id": entry.canonical_node_id,
            "region": entry.region
        })).collect::<Vec<_>>(),
        "allocations": cloud.marketplace_allocations.snapshot()
    })))
}

fn required_env(name: &str) -> Result<String, MarketplaceError> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| error(axum::http::StatusCode::SERVICE_UNAVAILABLE, "marketplace_unavailable"))
}

fn normalized_address(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        && value[2..].bytes().any(|byte| byte != b'0'))
    .then(|| value.to_ascii_lowercase())
}

fn settlement_config() -> Result<SettlementConfig, MarketplaceError> {
    // A mainnet address is not a test switch. This explicit operator gate keeps
    // the endpoint unavailable until the named deployment has been audited and
    // approved for this exact configuration version.
    if std::env::var("HIVE_MARKETPLACE_ATOMIC_SPLIT_AUDITED").ok().as_deref() != Some("1") {
        return Err(error(axum::http::StatusCode::SERVICE_UNAVAILABLE, "settlement_unavailable"));
    }
    let settlement_contract = normalized_address(&required_env("HIVE_MARKETPLACE_ATOMIC_SPLIT_CONTRACT")?)
        .ok_or_else(|| error(axum::http::StatusCode::SERVICE_UNAVAILABLE, "settlement_unavailable"))?;
    let fee_recipient = normalized_address(&required_env("HIVE_MARKETPLACE_FEE_RECIPIENT")?)
        .ok_or_else(|| error(axum::http::StatusCode::SERVICE_UNAVAILABLE, "settlement_unavailable"))?;
    let reference = required_env("HIVE_MARKETPLACE_SETTLEMENT_CONFIGURATION_REFERENCE")?;
    if reference != "base-theo-atomic-split-v1" {
        return Err(error(axum::http::StatusCode::SERVICE_UNAVAILABLE, "settlement_unavailable"));
    }
    Ok(SettlementConfig {
        currency: "THEO".into(),
        chain_id: BASE_CHAIN_ID,
        token_contract: THEO_TOKEN.into(),
        token_decimals: 18,
        fee_recipient,
        fee_bps: FEE_BPS,
        settlement_contract,
        settlement_event_signature: SETTLEMENT_SIGNATURE.into(),
        confirmation_policy: ConfirmationPolicy { minimum_confirmations: CONFIRMATIONS },
        configuration_reference: reference,
    })
}

fn provider_registry() -> BTreeMap<String, String> {
    // This is operator-owned registry data, never Marketplace input. Every
    // advertised provider must have a verified recipient entry.
    let configured = std::env::var("HIVE_MARKETPLACE_PROVIDER_RECIPIENTS").unwrap_or_default();
    configured
        .split(',')
        .filter_map(|entry| {
            let (provider, recipient) = entry.split_once('=')?;
            normalized_address(recipient).map(|recipient| (provider.trim().to_owned(), recipient))
        })
        .collect()
}

fn hmac_secret(key_id: &str) -> Option<String> {
    std::env::var("HIVE_MARKETPLACE_HMAC_KEYS")
        .ok()?
        .split(',')
        .filter_map(|entry| entry.split_once(':'))
        .find_map(|(id, secret)| (id == key_id && !secret.is_empty()).then(|| secret.to_owned()))
}

fn verify_marketplace_request(
    cloud: &Arc<CloudState>,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), MarketplaceError> {
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    let key_id = header("x-marketplace-key-id")
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| error(axum::http::StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    let secret = hmac_secret(key_id)
        .ok_or_else(|| error(axum::http::StatusCode::UNAUTHORIZED, "invalid_marketplace_key"))?;
    let timestamp = header("x-marketplace-timestamp")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| error(axum::http::StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    let nonce = header("x-marketplace-nonce")
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| error(axum::http::StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    let digest = hex::encode(Sha256::digest(body));
    if header("x-marketplace-content-sha256")
        .map(|value| value.eq_ignore_ascii_case(&digest))
        != Some(true)
    {
        return Err(error(axum::http::StatusCode::UNAUTHORIZED, "invalid_marketplace_digest"));
    }
    let signature = header("x-marketplace-signature")
        .and_then(|value| hex::decode(value).ok())
        .ok_or_else(|| error(axum::http::StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    let now = hive_core::now_ms();
    if now.abs_diff(timestamp) > CLOCK_SKEW_MS {
        return Err(error(axum::http::StatusCode::UNAUTHORIZED, "marketplace_request_expired"));
    }
    let canonical = format!("{}\n{}\n{}\n{}\n{}", method, path, timestamp, nonce, digest);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC supports all key sizes");
    mac.update(canonical.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| error(axum::http::StatusCode::UNAUTHORIZED, "invalid_marketplace_signature"))?;
    if !cloud.marketplace_security.consume_nonce(format!("{key_id}:{nonce}"), now) {
        return Err(error(axum::http::StatusCode::CONFLICT, "marketplace_nonce_replayed"));
    }
    crate::persist::persist(cloud);
    Ok(())
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, MarketplaceError> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(ToOwned::to_owned)
        .ok_or_else(|| error(axum::http::StatusCode::BAD_REQUEST, "missing_idempotency_key"))
}

fn listed_deployments(cloud: &CloudState) -> Vec<ListedDeployment> {
    let recipients = provider_registry();
    let expires_at_ms = hive_core::now_ms() + ADVERTISEMENT_TTL_MS;
    cloud.registry.nodes().into_iter().filter_map(|node| {
        let provider_id = node.provider.clone()?.trim().to_owned();
        let provider_recipient = recipients.get(&provider_id)?.clone();
        eligible_node(cloud, &node).then(|| ListedDeployment {
            deployment_id: format!("dep_{}", &hex::encode(Sha256::digest(format!("{provider_id}:{}", node.id)))[..24]),
            provider_id,
            canonical_node_id: node.id,
            provider_recipient,
            region: node.region,
            runtime: node.backend,
            capabilities: Capabilities {
                vcpu: node.cpu_cores,
                ram_mib: node.mem_total_mb,
                gpu: Gpu { model: node.gpu_model, count: node.gpu_count },
                storage_gib: node.disk_total_gb,
            },
            expires_at_ms,
        })
    }).collect()
}

async fn list_deployments(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
) -> ApiResult<MarketplaceDeploymentsResponse> {
    verify_marketplace_request(&cloud, "GET", "/v1/marketplace/l0/deployments", &headers, &[])?;
    let now = hive_core::now_ms();
    let listed = listed_deployments(&cloud);
    cloud.marketplace_security.replace_deployments(listed.clone());
    crate::persist::persist(&cloud);
    Ok(Json(MarketplaceDeploymentsResponse {
        data: listed
            .into_iter()
            .map(|entry| MarketplaceDeployment::from_listed(entry, now))
            .collect(),
    }))
}

async fn get_settlement_config(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
) -> ApiResult<Value> {
    verify_marketplace_request(&cloud, "GET", "/v1/marketplace/settlement-config", &headers, &[])?;
    Ok(Json(json!({"settlement": settlement_config()?})))
}

async fn create_payment_intent(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Value> {
    verify_marketplace_request(&cloud, "POST", "/v1/marketplace/payment-intents", &headers, &body)?;
    let key = idempotency_key(&headers)?;
    if let Some(intent) = cloud.marketplace_security.intent_for_key(&key) {
        return Ok(Json(payment_intent_response(&intent)));
    }
    let request: PaymentIntentRequest = serde_json::from_slice(&body)
        .map_err(|_| error(axum::http::StatusCode::BAD_REQUEST, "invalid_request"))?;
    let deployment = cloud.marketplace_security.deployment(&request.deployment_id)
        .filter(|entry| entry.expires_at_ms > hive_core::now_ms())
        .filter(|entry| entry.provider_id == request.provider_id && entry.canonical_node_id == request.canonical_node_id)
        .ok_or_else(|| error(axum::http::StatusCode::CONFLICT, "deployment_unavailable"))?;
    let buyer_address = normalized_address(&request.buyer_address)
        .ok_or_else(|| error(axum::http::StatusCode::BAD_REQUEST, "invalid_buyer_address"))?;
    if !valid_order_reference(&request.order_reference) {
        return Err(error(axum::http::StatusCode::BAD_REQUEST, "invalid_order_reference"));
    }
    let gross = canonical_atomic(&request.gross_amount_atomic)
        .ok_or_else(|| error(axum::http::StatusCode::BAD_REQUEST, "invalid_amount"))?;
    // 500 / 10_000 is exactly 1 / 20. Decimal long division keeps the full
    // EVM uint256 range instead of narrowing an atomic amount to u128.
    let fee = decimal_divide_small(&gross, 20);
    let provider_amount = decimal_subtract(&gross, &fee)
        .expect("fee is floor(gross / 20) and can never exceed gross");
    let intent = PaymentIntent {
        payment_intent_id: format!("pi_{}", Uuid::new_v4().simple()),
        expires_at_ms: hive_core::now_ms() + INTENT_TTL_MS,
        order_reference: request.order_reference.to_ascii_lowercase(),
        gross_amount_atomic: gross,
        buyer_address,
        deployment,
        provider_amount_atomic: provider_amount,
        fee_amount_atomic: fee,
        settlement: settlement_config()?,
        verification_status: "pending".into(),
        transaction_hash: None,
    };
    let intent = cloud.marketplace_security.put_intent(key, intent);
    crate::persist::persist(&cloud);
    Ok(Json(payment_intent_response(&intent)))
}

fn payment_intent_response(intent: &PaymentIntent) -> Value {
    json!({"payment_intent_id": intent.payment_intent_id,
        "expires_at": iso_timestamp(intent.expires_at_ms),
        "amount_atomic": intent.gross_amount_atomic, "order_reference": intent.order_reference,
        "settlement": intent.settlement})
}

async fn verify_payment(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Value> {
    verify_marketplace_request(&cloud, "POST", "/v1/marketplace/payments/verify", &headers, &body)?;
    let request: PaymentVerificationRequest = serde_json::from_slice(&body)
        .map_err(|_| error(axum::http::StatusCode::BAD_REQUEST, "invalid_request"))?;
    let mut intent = cloud.marketplace_security.intent(&request.payment_intent_id)
        .ok_or_else(|| error(axum::http::StatusCode::NOT_FOUND, "payment_intent_not_found"))?;
    if intent.transaction_hash.as_deref().is_some_and(|old| old != request.transaction_hash) {
        return Err(error(axum::http::StatusCode::CONFLICT, "payment_intent_already_bound"));
    }
    if intent.verification_status != "verified" {
        intent.transaction_hash = Some(request.transaction_hash);
        intent.verification_status = verify_payment_chain(&cloud, &intent).await;
        cloud.marketplace_security.update_intent(intent.clone());
        crate::persist::persist(&cloud);
    }
    Ok(Json(json!({"payment_intent_id": intent.payment_intent_id, "status": intent.verification_status})))
}

async fn verify_payment_chain(cloud: &CloudState, intent: &PaymentIntent) -> String {
    if intent.expires_at_ms <= hive_core::now_ms() || !valid_transaction_hash(intent.transaction_hash.as_deref().unwrap_or_default()) {
        return "failed".into();
    }
    let Ok(rpc_url) = required_env("THEO_RPC_URL") else { return "failed".into() };
    if !rpc_url.starts_with("https://") { return "failed".into() }
    let chain = marketplace_rpc(cloud, &rpc_url, "eth_chainId", json!([])).await.and_then(|value| hex_u64(&value));
    if chain != Some(BASE_CHAIN_ID) { return "failed".into() }
    let Some(receipt) = marketplace_rpc(cloud, &rpc_url, "eth_getTransactionReceipt", json!([intent.transaction_hash])).await else { return "pending".into() };
    if receipt.is_null() { return "pending".into() }
    if receipt.get("status").and_then(Value::as_str) != Some("0x1") { return "failed".into() }
    let Some(block) = receipt.get("blockNumber").and_then(Value::as_str).and_then(hex_u64_str) else { return "failed".into() };
    let Some(tip) = marketplace_rpc(cloud, &rpc_url, "eth_blockNumber", json!([])).await.and_then(|value| value.as_str().and_then(hex_u64_str)) else { return "pending".into() };
    if tip.saturating_sub(block).saturating_add(1) < CONFIRMATIONS { return "awaiting_confirmations".into() }
    if receipt.get("logs").and_then(Value::as_array).is_some_and(|logs| logs.iter().any(|log| valid_settlement_log(log, intent))) {
        "verified".into()
    } else {
        "failed".into()
    }
}
async fn marketplace_rpc(cloud: &CloudState, url: &str, method: &str, params: Value) -> Option<Value> {
    cloud.http.post(url).json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send().await.ok()?.json::<Value>().await.ok()?.get("result").cloned()
}

fn valid_settlement_log(log: &Value, intent: &PaymentIntent) -> bool {
    if log.get("address").and_then(Value::as_str).and_then(normalized_address).as_deref()
        != Some(intent.settlement.settlement_contract.as_str()) { return false }
    let expected_topic = format!("0x{}", hex::encode(Keccak256::digest(SETTLEMENT_SIGNATURE.as_bytes())));
    if log.get("topics").and_then(Value::as_array).and_then(|topics| topics.first())
        .and_then(Value::as_str).is_none_or(|topic| !topic.eq_ignore_ascii_case(&expected_topic)) { return false }
    let Some(bytes) = log.get("data").and_then(Value::as_str).and_then(decode_hex) else { return false };
    if bytes.len() != 32 * 9 { return false }
    word(&bytes, 0).as_deref() == Some(intent.order_reference.as_str())
        && address_word(&bytes, 1).as_deref() == Some(intent.buyer_address.as_str())
        && address_word(&bytes, 2).as_deref() == Some(THEO_TOKEN)
        && uint_word(&bytes, 3).as_deref() == Some(intent.gross_amount_atomic.as_str())
        && address_word(&bytes, 4).as_deref() == Some(intent.deployment.provider_recipient.as_str())
        && uint_word(&bytes, 5).as_deref() == Some(intent.provider_amount_atomic.as_str())
        && address_word(&bytes, 6).as_deref() == Some(intent.settlement.fee_recipient.as_str())
        && uint_word(&bytes, 7).as_deref() == Some(intent.fee_amount_atomic.as_str())
        && uint_word(&bytes, 8).as_deref() == Some(FEE_BPS.to_string().as_str())
}

async fn submit_allocation(
    State(cloud): State<Arc<CloudState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Value> {
    verify_marketplace_request(&cloud, "POST", "/v1/marketplace/l0/allocations", &headers, &body)?;
    let key = idempotency_key(&headers)?;
    if let Some(id) = cloud.marketplace_security.allocation_for_key(&key) {
        if let Some(allocation) = cloud.marketplace_allocations.get(&id) {
            return Ok(Json(json!({"allocation_id": id, "status": allocation.status})));
        }
        return Err(error(axum::http::StatusCode::CONFLICT, "allocation_idempotency_conflict"));
    }
    let request: AllocationRequest = serde_json::from_slice(&body)
        .map_err(|_| error(axum::http::StatusCode::BAD_REQUEST, "invalid_request"))?;
    let intent = cloud.marketplace_security.intent(&request.payment_intent_id)
        .ok_or_else(|| error(axum::http::StatusCode::NOT_FOUND, "payment_intent_not_found"))?;
    if intent.verification_status != "verified" {
        return Err(error(axum::http::StatusCode::CONFLICT, "settlement_not_verified"));
    }
    // Immediately before scheduling, derive the current listing from the
    // authoritative registry again; no stale listing can substitute a node.
    let current = listed_deployments(&cloud).into_iter()
        .find(|item| item.deployment_id == intent.deployment.deployment_id
            && item.provider_id == intent.deployment.provider_id
            && item.canonical_node_id == intent.deployment.canonical_node_id
            && item.provider_recipient == intent.deployment.provider_recipient);
    let Some(current) = current else {
        return Err(error(axum::http::StatusCode::CONFLICT, "deployment_unavailable"));
    };
    let approved = std::collections::HashSet::from([current.canonical_node_id.clone()]);
    let regions = request.resources.region.iter().cloned().collect::<Vec<_>>();
    if schedule::place(&cloud, &regions, false, false, request.resources.gpu,
        schedule::InterpreterNeeds::default(), false, false, Some(&approved)).is_empty() {
        return Err(error(axum::http::StatusCode::CONFLICT, "capacity_unavailable"));
    }
    let now = hive_core::now_ms();
    let allocation = Allocation {
        marketplace_order_id: intent.order_reference.clone(), tenant_id: request.tenant_id.clone(),
        resources: request.resources.clone(), approved_node_ids: vec![current.canonical_node_id],
        theo_amount: intent.gross_amount_atomic.clone(), expires_at_ms: intent.expires_at_ms,
        contract_reference: intent.payment_intent_id.clone(), advertisement_id: intent.deployment.deployment_id.clone(),
        status: "submitted".into(), routed_build_id: None, created_at_ms: now, updated_at_ms: now,
    };
    let allocation = match cloud.marketplace_allocations.put_if_absent(allocation) {
        Ok(row) => row, Err(old) => return Ok(Json(json!({"allocation_id": old.marketplace_order_id, "status": old.status}))),
    };
    cloud.marketplace_security.bind_allocation_key(key, allocation.marketplace_order_id.clone());
    crate::persist::persist(&cloud);
    let mut deployment = request.deployment;
    deployment.no_fanout = false;
    deployment.fanout_secondary = false;
    deployment.project_incarnation = None;
    deployment.marketplace_placement = Some(fluid_core::MarketplacePlacementSnapshot {
        contract_version: 1, policy_version: 1, marketplace_order_id: allocation.marketplace_order_id.clone(),
        buyer_tenant_id: allocation.tenant_id.clone(), retrieved_at_ms: now,
        approved_node_ids: allocation.approved_node_ids.clone(),
        policy: json!({"source":"marketplace-verified-settlement","payment_intent_id": intent.payment_intent_id}),
    });
    match crate::admin::start_named_deploy(&cloud, &allocation.tenant_id, deployment, None).await {
        Ok(result) => {
            let mut allocation = allocation;
            allocation.status = "scheduled".into();
            allocation.routed_build_id = result["build_id"].as_str().map(ToOwned::to_owned);
            allocation.updated_at_ms = hive_core::now_ms();
            cloud.marketplace_allocations.update(allocation.clone());
            crate::persist::persist(&cloud);
            Ok(Json(json!({"allocation_id": allocation.marketplace_order_id, "status": allocation.status, "build_id": allocation.routed_build_id})))
        }
        Err(_) => {
            let mut allocation = allocation;
            allocation.status = "failed".into();
            allocation.updated_at_ms = hive_core::now_ms();
            cloud.marketplace_allocations.update(allocation.clone());
            crate::persist::persist(&cloud);
            Err(error(axum::http::StatusCode::CONFLICT, "provisioning_failed"))
        }
    }
}

fn eligible_node(cloud: &CloudState, node: &hive_edge::NodeInfo) -> bool {
    let connected = node.name == cloud.node_name || cloud.node_admins.read().contains_key(&node.name)
        || (node.peer_id.is_some() && node.iroh_addr.is_some());
    node.healthy && connected
        && (node.backend == "firecracker" || (node.backend == "litebox" && schedule::runtime_artifact_capable(node, true)))
        && node.mem_total_mb >= 1024 && (node.disk_free_gb == 0 || node.disk_free_gb >= 20)
}
fn valid_order_reference(value: &str) -> bool {
    value.len() == 66 && value.starts_with("0x") && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn iso_timestamp(timestamp_ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms as i64)
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}
fn iso_millis_timestamp(timestamp_ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms as i64)
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}
fn valid_transaction_hash(value: &str) -> bool { valid_order_reference(value) }
fn hex_u64(value: &Value) -> Option<u64> { value.as_str().and_then(hex_u64_str) }
fn hex_u64_str(value: &str) -> Option<u64> { u64::from_str_radix(value.strip_prefix("0x")?, 16).ok() }
fn decode_hex(value: &str) -> Option<Vec<u8>> { hex::decode(value.strip_prefix("0x")?).ok() }
fn word(bytes: &[u8], index: usize) -> Option<String> {
    bytes.get(index * 32..(index + 1) * 32).map(|word| format!("0x{}", hex::encode(word)))
}
fn address_word(bytes: &[u8], index: usize) -> Option<String> {
    bytes.get(index * 32 + 12..(index + 1) * 32).map(|word| format!("0x{}", hex::encode(word)))
}
fn uint_word(bytes: &[u8], index: usize) -> Option<String> {
    let word = bytes.get(index * 32..(index + 1) * 32)?;
    // uint256 to decimal, without a lossy host-integer conversion.
    let mut digits = vec![0u8];
    for byte in word {
        let mut carry = *byte as u16;
        for digit in &mut digits {
            let value = (*digit as u16) * 256 + carry;
            *digit = (value % 10) as u8;
            carry = value / 10;
        }
        while carry > 0 {
            digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    Some(digits.into_iter().rev().map(|digit| (b'0' + digit) as char).collect())
}
fn canonical_atomic(value: &str) -> Option<String> {
    const MAX_UINT256: &str =
        "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.trim_start_matches('0').to_owned())
        .filter(|value| !value.is_empty() && (value.len() < MAX_UINT256.len()
            || (value.len() == MAX_UINT256.len() && value.as_str() <= MAX_UINT256)))
}
fn decimal_divide_small(value: &str, divisor: u8) -> String {
    let mut remainder = 0u16;
    let mut output = String::new();
    for byte in value.bytes() {
        let quotient = (remainder * 10 + (byte - b'0') as u16) / divisor as u16;
        remainder = (remainder * 10 + (byte - b'0') as u16) % divisor as u16;
        if !output.is_empty() || quotient != 0 {
            output.push((b'0' + quotient as u8) as char);
        }
    }
    if output.is_empty() { "0".into() } else { output }
}
fn decimal_subtract(value: &str, subtrahend: &str) -> Option<String> {
    let mut result = Vec::with_capacity(value.len());
    let mut borrow = 0i16;
    let mut left = value.bytes().rev();
    let mut right = subtrahend.bytes().rev();
    loop {
        let Some(a) = left.next() else { break };
        let b = right.next().map(|byte| (byte - b'0') as i16).unwrap_or(0);
        let mut digit = (a - b'0') as i16 - b - borrow;
        borrow = if digit < 0 { digit += 10; 1 } else { 0 };
        result.push((b'0' + digit as u8) as char);
    }
    (borrow == 0).then(|| {
        let output: String = result.into_iter().rev().collect();
        let output = output.trim_start_matches('0');
        if output.is_empty() { "0".into() } else { output.to_owned() }
    })
}
