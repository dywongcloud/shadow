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
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

use crate::{schedule, state::CloudState};

type HmacSha256 = Hmac<Sha256>;

const ADVERTISEMENT_TTL_MS: u64 = 60_000;

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

#[derive(Clone, Serialize)]
struct SettlementConfig {
    chain_id: String,
    token_contract: String,
    treasury: String,
    decimals: u8,
    required_confirmations: u64,
}

#[derive(Deserialize)]
struct AllocationRequest {
    marketplace_order_id: String,
    tenant_id: String,
    resources: ResourceRequirements,
    approved_node_ids: Vec<String>,
    theo_amount: String,
    expires_at_ms: u64,
    contract_reference: String,
    advertisement_id: String,
    advertisement_attestation: String,
}

#[derive(Deserialize)]
struct RouteRequest {
    // The deployment payload is ordinary DevHub build input, but placement and
    // orchestration fields below are always overwritten by the backend.
    deployment: fluid_core::GitDeployRequest,
}

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route("/v1/marketplace/nodes", get(advertise_nodes))
        .route(
            "/v1/marketplace/allocations",
            post(create_allocation).get(list_allocations),
        )
        .route("/v1/marketplace/allocations/:order_id", get(get_allocation))
        .route(
            "/v1/marketplace/allocations/:order_id/route",
            post(route_allocation),
        )
        .route(
            "/v1/marketplace/allocations/:order_id/fulfill",
            post(fulfill_allocation),
        )
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
        treasury: required("THEO_TREASURY_ADDRESS")?,
        decimals: required("THEO_TOKEN_DECIMALS")?.parse().map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "THEO_TOKEN_DECIMALS is invalid".into(),
            )
        })?,
        required_confirmations: required("THEO_REQUIRED_CONFIRMATIONS")?
            .parse()
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "THEO_REQUIRED_CONFIRMATIONS is invalid".into(),
                )
            })?,
    })
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
    let response = cloud.http.post(url).header("x-marketplace-settlement-key", key).json(&json!({
        "order_id": order_id, "contract_reference": contract_reference, "amount": amount,
        "chain_id": config.chain_id, "token_contract": config.token_contract, "treasury": config.treasury,
        "decimals": config.decimals, "required_confirmations": config.required_confirmations
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
        || verified["treasury"].as_str() != Some(&config.treasury)
        || verified["decimals"].as_u64() != Some(config.decimals as u64)
        || verified["confirmations"].as_u64().unwrap_or(0) < config.required_confirmations
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
