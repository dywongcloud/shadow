//! Cloudflare Queues REST surface — 1:1 with
//! `/accounts/{account_id}/queues` (this platform's tenant IS the account),
//! consumer CRUD, pull-consumer messages/pull + messages/ack, producer send,
//! and the realtime metrics endpoint. See `crate::queues` for the model +
//! store.
//!
//! Response envelope matches Cloudflare's own shape everywhere:
//! `{"success":bool,"errors":[...],"messages":[...],"result":...}`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::admin::tenant;
use crate::queues::{
    ConsumerSettings, ConsumerType, QueueSettings, MAX_MESSAGE_BODY_BYTES,
    PULL_BATCH_SIZE_DEFAULT, PULL_BATCH_SIZE_MAX, VISIBILITY_TIMEOUT_DEFAULT_MS,
};
use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route("/v1/queues", get(queues_list).post(queue_create))
        .route(
            "/v1/queues/:queue_id",
            get(queue_get).patch(queue_update).delete(queue_delete),
        )
        .route(
            "/v1/queues/:queue_id/consumers",
            get(consumers_list).post(consumer_create),
        )
        .route(
            "/v1/queues/:queue_id/consumers/:consumer_id",
            axum::routing::delete(consumer_delete),
        )
        .route("/v1/queues/:queue_id/messages", post(message_send))
        .route(
            "/v1/queues/:queue_id/messages/pull",
            post(messages_pull),
        )
        .route("/v1/queues/:queue_id/messages/ack", post(messages_ack))
        .route("/v1/queues/:queue_id/metrics", get(queue_metrics))
}

// ---- Cloudflare-shaped envelope helpers ----

fn ok_envelope(result: Value) -> Json<Value> {
    Json(json!({
        "success": true,
        "errors": [],
        "messages": [],
        "result": result,
    }))
}

fn err_envelope(status: StatusCode, code: u32, message: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({
            "success": false,
            "errors": [{"code": code, "message": message}],
            "messages": [],
            "result": null,
        })),
    )
}

/// Unknown id and foreign-tenant id both look identical — no existence leak
/// (the `browser_artifacts`/`browser_db` precedent).
fn not_found(what: &str) -> (StatusCode, Json<Value>) {
    err_envelope(StatusCode::NOT_FOUND, 10007, &format!("{what} not found"))
}

// ---- Queue CRUD ----

async fn queues_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
) -> Json<Value> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let queues: Vec<Value> = c
        .queues
        .list(&t)
        .into_iter()
        .map(|q| queue_view(&c, q))
        .collect();
    ok_envelope(json!(queues))
}

#[derive(Deserialize)]
struct QueueCreateReq {
    queue_name: String,
}

async fn queue_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(body): Json<QueueCreateReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let name = body.queue_name.trim();
    if name.is_empty() {
        return Err(err_envelope(
            StatusCode::BAD_REQUEST,
            10001,
            "queue_name is required",
        ));
    }
    // Cloudflare scopes queue_name uniqueness per-account — unlike this
    // platform's GLOBAL project-name rule, a different tenant's same-named
    // queue must NOT collide (queues-adversarial-edge-cases).
    if c.queues.find_by_name(&t, name).is_some() {
        return Err(err_envelope(
            StatusCode::CONFLICT,
            10002,
            &format!("a queue named \"{name}\" already exists"),
        ));
    }
    let queue = c.queues.create(&t, name);
    crate::persist::persist(&c);
    Ok(ok_envelope(queue_view(&c, queue)))
}

async fn queue_get(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let queue = c.queues.get(&t, &queue_id).ok_or_else(|| not_found("queue"))?;
    Ok(ok_envelope(queue_view(&c, queue)))
}

#[derive(Deserialize, Default)]
struct QueueUpdateReq {
    delivery_delay: Option<u32>,
    delivery_paused: Option<bool>,
    message_retention_period: Option<u32>,
}

async fn queue_update(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
    Json(body): Json<QueueUpdateReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let current = c.queues.get(&t, &queue_id).ok_or_else(|| not_found("queue"))?;
    let settings = QueueSettings {
        delivery_delay: body.delivery_delay.unwrap_or(current.settings.delivery_delay),
        delivery_paused: body
            .delivery_paused
            .unwrap_or(current.settings.delivery_paused),
        message_retention_period: body
            .message_retention_period
            .unwrap_or(current.settings.message_retention_period),
    };
    let queue = c
        .queues
        .update_settings(&t, &queue_id, settings)
        .ok_or_else(|| not_found("queue"))?;
    crate::persist::persist(&c);
    Ok(ok_envelope(queue_view(&c, queue)))
}

async fn queue_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.queues.get(&t, &queue_id).is_none() {
        return Err(not_found("queue"));
    }
    c.queues.delete(&t, &queue_id);
    crate::persist::persist(&c);
    Ok(ok_envelope(json!({"id": queue_id})))
}

fn queue_view(c: &Arc<CloudState>, q: crate::queues::Queue) -> Value {
    let consumers = c.queues.list_consumers(&q.queue_id);
    json!({
        "queue_id": q.queue_id,
        "queue_name": q.queue_name,
        "created_on": q.created_on,
        "modified_on": q.modified_on,
        "consumers_total_count": consumers.len(),
        "producers_total_count": 0,
        "consumers": consumers.iter().map(consumer_view).collect::<Vec<_>>(),
        "producers": Value::Array(vec![]),
        "settings": {
            "delivery_delay": q.settings.delivery_delay,
            "delivery_paused": q.settings.delivery_paused,
            "message_retention_period": q.settings.message_retention_period,
        },
    })
}

fn consumer_view(c: &crate::queues::Consumer) -> Value {
    json!({
        "consumer_id": c.consumer_id,
        "queue_id": c.queue_id,
        "type": c.kind,
        "script_name": c.script_name,
        "dead_letter_queue": c.dead_letter_queue,
        "created_on": c.created_on,
        "settings": {
            "batch_size": c.settings.batch_size,
            "max_batch_timeout": c.settings.max_batch_timeout,
            "max_retries": c.settings.max_retries,
            "max_concurrency": c.settings.max_concurrency,
            "retry_delay": c.settings.retry_delay,
            "visibility_timeout_ms": c.settings.visibility_timeout_ms,
        },
    })
}

// ---- Consumer CRUD ----

async fn consumers_list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.queues.get(&t, &queue_id).is_none() {
        return Err(not_found("queue"));
    }
    let consumers: Vec<Value> = c
        .queues
        .list_consumers(&queue_id)
        .iter()
        .map(consumer_view)
        .collect();
    Ok(ok_envelope(json!(consumers)))
}

#[derive(Deserialize)]
struct ConsumerCreateReq {
    #[serde(rename = "type", default = "default_consumer_type")]
    kind: ConsumerType,
    #[serde(default)]
    script_name: String,
    #[serde(default)]
    dead_letter_queue: Option<String>,
    #[serde(default)]
    settings: ConsumerSettings,
}
fn default_consumer_type() -> ConsumerType {
    ConsumerType::Worker
}

async fn consumer_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
    Json(body): Json<ConsumerCreateReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.queues.get(&t, &queue_id).is_none() {
        return Err(not_found("queue"));
    }
    // A dead_letter_queue must exist AND belong to the same tenant — matches
    // Cloudflare's own consumer-create validation.
    if let Some(dlq_name) = &body.dead_letter_queue {
        let dlq = c
            .queues
            .find_by_name(&t, dlq_name)
            .ok_or_else(|| err_envelope(
                StatusCode::BAD_REQUEST,
                10003,
                &format!("dead_letter_queue \"{dlq_name}\" does not exist"),
            ))?;
        let consumer = c.queues.create_consumer(
            &queue_id,
            body.kind,
            body.settings,
            body.script_name,
            Some(dlq.queue_id),
        );
        crate::persist::persist(&c);
        return Ok(ok_envelope(consumer_view(&consumer)));
    }
    let consumer =
        c.queues
            .create_consumer(&queue_id, body.kind, body.settings, body.script_name, None);
    crate::persist::persist(&c);
    Ok(ok_envelope(consumer_view(&consumer)))
}

async fn consumer_delete(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((queue_id, consumer_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.queues.get(&t, &queue_id).is_none() {
        return Err(not_found("queue"));
    }
    if !c.queues.delete_consumer(&queue_id, &consumer_id) {
        return Err(not_found("consumer"));
    }
    crate::persist::persist(&c);
    Ok(ok_envelope(json!({"id": consumer_id})))
}

// ---- Producer send ----

#[derive(Deserialize)]
struct MessageSendReq {
    body: Value,
    /// Per-message override. When absent, the queue's own configured
    /// `delivery_delay` applies (Cloudflare's own default-delay semantics —
    /// a queue-level delay is meaningless if every send silently ignores it).
    delay_seconds: Option<u32>,
}

async fn message_send(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
    Json(req): Json<MessageSendReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    // `delivery_paused` gates DELIVERY (checked by the push-consumer loop and
    // the pull-consumer's messages/pull below), not producer sends — the
    // field name is specifically "delivery_paused", not "send_paused", and a
    // producer must be able to keep enqueueing while delivery is held.
    let queue = c.queues.get(&t, &queue_id).ok_or_else(|| not_found("queue"))?;
    let body_str = req.body.to_string();
    if body_str.len() > MAX_MESSAGE_BODY_BYTES {
        return Err(err_envelope(
            StatusCode::PAYLOAD_TOO_LARGE,
            10004,
            &format!("message body exceeds the {MAX_MESSAGE_BODY_BYTES}-byte limit"),
        ));
    }
    let delay = req.delay_seconds.unwrap_or(queue.settings.delivery_delay);
    let id = c
        .queues
        .send(&queue_id, body_str, Value::Null, delay)
        .map_err(|e| err_envelope(StatusCode::BAD_REQUEST, 10005, &e.to_string()))?;
    Ok(ok_envelope(json!({"id": id})))
}

// ---- Pull consumer: pull / ack ----

#[derive(Deserialize, Default)]
struct PullReq {
    batch_size: Option<u32>,
    visibility_timeout_ms: Option<u64>,
}

async fn messages_pull(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
    Json(req): Json<PullReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    let queue = c.queues.get(&t, &queue_id).ok_or_else(|| not_found("queue"))?;
    let batch_size = req
        .batch_size
        .unwrap_or(PULL_BATCH_SIZE_DEFAULT)
        .clamp(1, PULL_BATCH_SIZE_MAX);
    let visibility_timeout_ms = req
        .visibility_timeout_ms
        .unwrap_or(VISIBILITY_TIMEOUT_DEFAULT_MS)
        .clamp(1000, crate::queues::VISIBILITY_TIMEOUT_MAX_MS);
    // A paused queue holds delivery: a pull returns an honestly-empty batch
    // rather than leasing messages a paused consumer isn't meant to see yet.
    let leased = if queue.settings.delivery_paused {
        Vec::new()
    } else {
        c.queues
            .pull(&queue_id, batch_size, visibility_timeout_ms)
            .await
    };
    let (backlog_count, _, _) = c.queues.backlog_stats(&queue_id).await;
    let messages: Vec<Value> = leased
        .into_iter()
        .map(|m| {
            json!({
                "body": m.body,
                "id": m.id,
                "timestamp_ms": m.timestamp_ms,
                "attempts": m.attempts,
                "metadata": m.metadata,
                "lease_id": m.lease.map(|(id, _)| id).unwrap_or_default(),
            })
        })
        .collect();
    Ok(ok_envelope(json!({
        "message_backlog_count": backlog_count,
        "messages": messages,
    })))
}

#[derive(Deserialize)]
struct Ack {
    lease_id: String,
}
#[derive(Deserialize)]
struct Retry {
    lease_id: String,
    #[serde(default)]
    delay_seconds: u32,
}
#[derive(Deserialize, Default)]
struct AckReq {
    #[serde(default)]
    acks: Vec<Ack>,
    #[serde(default)]
    retries: Vec<Retry>,
}

async fn messages_ack(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
    Json(req): Json<AckReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.queues.get(&t, &queue_id).is_none() {
        return Err(not_found("queue"));
    }
    let mut acked = 0usize;
    let mut retried = 0usize;
    for a in &req.acks {
        if c.queues.ack(&queue_id, &a.lease_id) {
            acked += 1;
        }
    }
    for r in &req.retries {
        if c.queues.retry(&queue_id, &r.lease_id, r.delay_seconds) {
            retried += 1;
        }
    }
    Ok(ok_envelope(json!({"acked": acked, "retried": retried})))
}

// ---- Metrics ----

async fn queue_metrics(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(queue_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = tenant(&c, &headers, claims.as_ref().map(|e| &e.0));
    if c.queues.get(&t, &queue_id).is_none() {
        return Err(not_found("queue"));
    }
    let (count, bytes, oldest) = c.queues.backlog_stats(&queue_id).await;
    Ok(ok_envelope(json!({
        "backlog_count": count,
        "backlog_bytes": bytes,
        "oldest_message_timestamp_ms": oldest,
    })))
}
