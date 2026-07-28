//! Read workflow RUN observability from an app's Vercel-WDK "world" store.
//!
//! Apps that use the WDK keep run/step/event state in a pluggable "world". The
//! common self-host adapter is `@open-workflow/world-redis`, which stores entities
//! in (Upstash) Redis as base64(CBOR(entity)) under a key prefix (default `owf`):
//!   * ZSET `<p>:runs`            score=createdAt, member=runId
//!   * STR  `<p>:run:<runId>`     base64(CBOR(WorkflowRun))
//!   * ZSET `<p>:steps:<runId>`   member=stepId
//!   * STR  `<p>:step:<runId>:<stepId>`  base64(CBOR(Step))
//!   * LIST `<p>:events:<runId>` + HASH `<p>:eventdata:<runId>` eventId->base64(CBOR)
//!
//! We read it directly over the Upstash REST API using the project's own env
//! (`WORKFLOW_REDIS_REST_URL`/`_TOKEN`, falling back to `UPSTASH_REDIS_REST_*`),
//! which the platform stores per-project. This MUST run on the node that holds
//! the project's decrypted env (its host) — `env_map` only decrypts locally — so
//! the coordinator proxies these reads to the hosting node (cross-region aware).
//!
//! ## Managed-world conformance (`@workflow/world` / world-vercel)
//!
//! Beyond reads, the lower half of this module is a WRITE layer that conforms to
//! the Vercel WDK `@workflow/world` contract at the interface + store-schema
//! level (the same level `world-redis`/`world-vercel`/`world-local` conform),
//! adapted to hive's iroh p2p multi-region infrastructure:
//!
//! | @workflow/world concept        | hive mapping                                   |
//! |--------------------------------|------------------------------------------------|
//! | events.create(run_cancelled)   | `run_op(cancel)` → append_event + run status   |
//! | recreateRunFromExisting        | `run_op(replay)` → new ULID run + run_created  |
//! | reenqueueRun                   | `run_op(reenqueue)` → enqueue_run              |
//! | wakeUpRun (cancel sleeps)      | `run_op(wakeup)` → complete_pending_waits+enq  |
//! | Queue.queue(__wkf_workflow_…)  | `enqueue_run` → owf:job HASH + owf:sched ZSET  |
//! | per-run write serialization    | `with_run_lock` (SET NX PX 15s + Lua unlock)   |
//! | ULID run ids (ts-valid)        | `ulid()` (48-bit ms + 80-bit rand, base32)     |
//! | base64(CBOR) entity/event enc  | `encode_blob` (serde_cbor; cbor-x round-trips) |
//! | status union / EventType enum  | upstream vocab (pending/running/completed/…)   |
//! | multi-region write routing     | host-routed over iroh, exactly like reads      |
//!
//! Storage lives in the PROJECT'S OWN world-redis store (the `owf:*` keys the
//! app runtime replays from), never a hive-private store — so a hive-issued
//! cancel/replay and the app's own runtime share one world state. The queue
//! trigger uses the app's own `owf:job`/`owf:sched` schema, which the app's
//! in-process world-redis dispatcher (50ms poll) delivers to
//! `/.well-known/workflow/v1/flow`, so a resumed run actually EXECUTES. What is
//! deliberately NOT reimplemented (Vercel-transport specifics, not part of the
//! World contract): the v4 frame protocol, OIDC auth, s3rf:/kvrf: ref
//! offloading, region-tagged ULIDs, VQS regional routing.

use std::sync::Arc;

use base64::Engine;
use ring::rand::SecureRandom;
use serde_json::{json, Value};

use crate::state::CloudState;

/// Resolved world connection for a project: (rest_url, token, key_prefix).
fn world_config(cloud: &Arc<CloudState>, project: &str) -> Option<(String, String, String)> {
    let env = cloud.projects.env_map(project);
    let get = |k: &str| env.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let url = get("WORKFLOW_REDIS_REST_URL").or_else(|| get("UPSTASH_REDIS_REST_URL"))?;
    let token = get("WORKFLOW_REDIS_REST_TOKEN").or_else(|| get("UPSTASH_REDIS_REST_TOKEN"))?;
    let prefix = get("WORKFLOW_REDIS_KEY_PREFIX").unwrap_or_else(|| "owf".to_string());
    Some((url.trim_end_matches('/').to_string(), token, prefix))
}

/// True if this project has a (readable) WDK world configured.
pub fn has_world(cloud: &Arc<CloudState>, project: &str) -> bool {
    world_config(cloud, project).is_some()
}

/// One Upstash REST command (`["GET","k"]` style) → its `result` value.
async fn cmd(cloud: &Arc<CloudState>, url: &str, token: &str, parts: &[&str]) -> Option<Value> {
    let resp = cloud
        .http
        .post(url)
        .bearer_auth(token)
        .json(&parts.to_vec())
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;
    resp.json::<Value>().await.ok()?.get("result").cloned()
}

/// A pipeline of commands → vector of `result` values (errors map to Null).
async fn pipeline(cloud: &Arc<CloudState>, url: &str, token: &str, cmds: Vec<Vec<String>>) -> Vec<Value> {
    let resp = cloud
        .http
        .post(format!("{url}/pipeline"))
        .bearer_auth(token)
        .json(&cmds)
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .await;
    let Some(resp) = resp.ok() else { return Vec::new() };
    let Some(v) = resp.json::<Value>().await.ok() else { return Vec::new() };
    v.as_array()
        .map(|a| a.iter().map(|e| e.get("result").cloned().unwrap_or(Value::Null)).collect())
        .unwrap_or_default()
}

/// Decode a base64(CBOR) blob into JSON (unwrapping cbor-x tags like Date).
///
/// Every failure path logs instead of silently returning `None` — the real
/// `@workflow/world` spec has `SPEC_VERSION_SUPPORTS_COMPRESSION` (a
/// gzip/zstd wrapper this fn has zero awareness of), so a future dispatcher
/// version writing compressed payloads would otherwise make every read
/// through here silently vanish rather than surface as a diagnosable error —
/// the exact "decrypt returns ciphertext instead of failing" shape that cost
/// real debugging time elsewhere in this codebase (see the at-rest-secret
/// supersession incident). Distinguishes "this looks like a known
/// compression format we don't handle" from genuine corruption so the two
/// don't get confused when triaging a future occurrence.
fn decode_blob(v: &Value) -> Option<Value> {
    let b64 = v.as_str().or_else(|| {
        tracing::warn!(value = %v, "decode_blob: stored value is not a string (expected base64)");
        None
    })?;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, len = b64.len(), "decode_blob: base64 decode failed");
            return None;
        }
    };
    match serde_cbor::from_slice::<serde_cbor::Value>(&bytes) {
        Ok(cv) => Some(cbor_to_json(cv)),
        Err(e) => {
            let looks_gzip = bytes.starts_with(&[0x1f, 0x8b]);
            let looks_zstd = bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]);
            if looks_gzip || looks_zstd {
                tracing::warn!(
                    format = if looks_gzip { "gzip" } else { "zstd" },
                    bytes = bytes.len(),
                    "decode_blob: payload looks compressed (SPEC_VERSION_SUPPORTS_COMPRESSION?) — decompression is NOT implemented, this event/run is unreadable until it is"
                );
            } else {
                tracing::warn!(error = %e, bytes = bytes.len(), first_bytes = ?&bytes[..bytes.len().min(8)], "decode_blob: CBOR decode failed (corrupt or unrecognized format)");
            }
            None
        }
    }
}

/// serde_cbor::Value → serde_json::Value. Tags are unwrapped to their inner value
/// (cbor-x encodes Date as a tag over an epoch number); map keys are stringified.
fn cbor_to_json(v: serde_cbor::Value) -> Value {
    use serde_cbor::Value as C;
    match v {
        C::Null => Value::Null,
        C::Bool(b) => Value::Bool(b),
        C::Integer(i) => json!(i as i64),
        C::Float(f) => json!(f),
        C::Bytes(b) => Value::String(base64::engine::general_purpose::STANDARD.encode(b)),
        C::Text(s) => Value::String(s),
        C::Array(a) => Value::Array(a.into_iter().map(cbor_to_json).collect()),
        C::Map(m) => {
            let mut o = serde_json::Map::new();
            for (k, val) in m {
                let key = match k {
                    C::Text(s) => s,
                    C::Integer(i) => i.to_string(),
                    _ => continue,
                };
                o.insert(key, cbor_to_json(val));
            }
            Value::Object(o)
        }
        C::Tag(_, inner) => cbor_to_json(*inner),
        _ => Value::Null,
    }
}

/// List the most recent workflow runs for a project (newest first), each tagged
/// with `project`. Returns None if no world is configured / unreachable.
pub async fn list_runs(cloud: &Arc<CloudState>, project: &str, limit: usize) -> Option<Vec<Value>> {
    let (url, token, p) = world_config(cloud, project)?;
    let hi = limit.saturating_sub(1).to_string();
    let ids = cmd(cloud, &url, &token, &[&format!("ZRANGE"), &format!("{p}:runs"), "0", &hi, "REV"]).await?;
    let ids: Vec<String> = ids.as_array()?.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    if ids.is_empty() {
        return Some(Vec::new());
    }
    let gets: Vec<Vec<String>> = ids.iter().map(|id| vec!["GET".into(), format!("{p}:run:{id}")]).collect();
    let blobs = pipeline(cloud, &url, &token, gets).await;
    let mut out = Vec::new();
    for b in blobs {
        if let Some(mut run) = decode_blob(&b) {
            enrich_run(&mut run, project);
            out.push(run);
        }
    }
    Some(out)
}

/// Add the platform's internal WorkflowRun fields (id/name/status/started_ms/…)
/// ALONGSIDE the native WDK fields (runId/workflowName/startedAt/…). This keeps
/// the runs API backward-compatible: older dashboards read the internal shape,
/// the new runs/Gantt UI reads the WDK shape — neither crashes on a missing field.
fn enrich_run(run: &mut Value, project: &str) {
    let Some(o) = run.as_object_mut() else { return };
    let wf = o.get("workflowName").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let id = o.get("runId").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let started = secs_to_ms(o.get("startedAt").or_else(|| o.get("createdAt")));
    let finished = secs_to_ms(o.get("completedAt"));
    let status = map_status(o.get("status").and_then(|x| x.as_str()).unwrap_or(""));
    o.insert("project".into(), json!(project));
    o.insert("id".into(), json!(id));
    o.insert("def_id".into(), json!(wf));
    o.insert("name".into(), json!(clean_name(&wf)));
    o.insert("status".into(), json!(status));
    o.insert("started_ms".into(), json!(started.unwrap_or(0)));
    o.insert("finished_ms".into(), finished.map(|m| json!(m)).unwrap_or(Value::Null));
    o.entry("steps").or_insert_with(|| json!([]));
}

/// Map WDK run status → the platform's internal RunStatus vocabulary.
fn map_status(s: &str) -> &'static str {
    match s.to_ascii_lowercase().as_str() {
        "completed" | "succeeded" => "succeeded",
        "running" => "running",
        "failed" | "error" => "failed",
        "cancelled" => "cancelled",
        _ => "pending",
    }
}

/// epoch-seconds (float) → ms.
fn secs_to_ms(v: Option<&Value>) -> Option<u64> {
    let f = v?.as_f64()?;
    if f <= 0.0 {
        return None;
    }
    Some(if f > 1e12 { f as u64 } else { (f * 1000.0) as u64 })
}

/// "workflow//./app/workflows/session//sessionWorkflow" → "sessionWorkflow".
fn clean_name(n: &str) -> String {
    n.split('/').filter(|s| !s.is_empty()).last().unwrap_or(n).to_string()
}

/// Full detail for one run: the run + its steps (for the Gantt) + its events.
pub async fn run_detail(cloud: &Arc<CloudState>, project: &str, run_id: &str) -> Option<Value> {
    let (url, token, p) = world_config(cloud, project)?;
    let run = cmd(cloud, &url, &token, &["GET", &format!("{p}:run:{run_id}")]).await.and_then(|v| decode_blob(&v));
    // Steps (ordered by the index ZSET).
    let step_ids = cmd(cloud, &url, &token, &["ZRANGE", &format!("{p}:steps:{run_id}"), "0", "-1"]).await;
    let step_ids: Vec<String> = step_ids
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()))
        .unwrap_or_default();
    let mut steps = Vec::new();
    if !step_ids.is_empty() {
        let gets: Vec<Vec<String>> = step_ids.iter().map(|s| vec!["GET".into(), format!("{p}:step:{run_id}:{s}")]).collect();
        for b in pipeline(cloud, &url, &token, gets).await {
            if let Some(step) = decode_blob(&b) {
                steps.push(step);
            }
        }
    }
    // Events (append-ordered list + hash of payloads).
    let ev_ids = cmd(cloud, &url, &token, &["LRANGE", &format!("{p}:events:{run_id}"), "0", "-1"]).await;
    let ev_ids: Vec<String> = ev_ids
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()))
        .unwrap_or_default();
    let mut events = Vec::new();
    if !ev_ids.is_empty() {
        let mut hargs = vec!["HMGET".to_string(), format!("{p}:eventdata:{run_id}")];
        hargs.extend(ev_ids.iter().cloned());
        let hargs_ref: Vec<&str> = hargs.iter().map(|s| s.as_str()).collect();
        if let Some(arr) = cmd(cloud, &url, &token, &hargs_ref).await.and_then(|v| v.as_array().cloned()) {
            for b in arr {
                if let Some(ev) = decode_blob(&b) {
                    events.push(ev);
                }
            }
        }
    }
    Some(json!({ "run": run, "steps": steps, "events": events, "project": project }))
}

// ===========================================================================
// Managed-world WRITE layer — conformant with @workflow/world (Vercel WDK).
//
// The console's run operations (the upstream 3-dots menu: Cancel, Replay,
// Re-enqueue, Cancel-Active-Sleeps/Wake) mutate world state. Because an app's
// runtime replays EXCLUSIVELY from its own world (the `owf:*` keys in the
// project's Upstash store), hive writes the SAME records/events the app reads —
// the identical shapes `@open-workflow/world-redis` produces — rather than a
// hive-private store. Effects mirror `@workflow/core` runtime/runs.ts:
//   * cancelRun          → run_cancelled event + run.status=cancelled (+cleanup)
//   * reenqueueRun       → enqueue {runId} on __wkf_workflow_<name> (no writes)
//   * wakeUpRun          → complete pending waits (wait_completed events) + enqueue
//   * recreateRun/replay → new run cloned from input + run_created + enqueue
// Enqueue = the app's own world-redis queue schema (owf:job:<id> + owf:sched
// ZSET), which the app's in-process dispatcher (50ms poll) delivers to its
// /.well-known/workflow/v1/flow endpoint — so a resumed run actually EXECUTES.
// All writes take the per-run lock `<p>:lock:run:<runId>` (SET NX PX + Lua
// compare-delete) so they never interleave with the app runtime's own writes.
// Every op runs on the project's HOST node (env decrypts locally); the
// coordinator routes ops over the iroh mesh exactly like world reads.
// ===========================================================================

/// base64(CBOR(value)) — the on-wire encoding every `owf:*` entity/event uses
/// (cbor-x on the app side round-trips it; serde_cbor encodes JSON numbers/
/// strings/maps identically, and `z.coerce.date()` on the app accepts our
/// epoch-ms number timestamps).
fn encode_blob(v: &Value) -> String {
    let bytes = serde_cbor::to_vec(v).unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A Crockford-base32 ULID (26 chars): 48-bit big-endian ms timestamp + 80 bits
/// of randomness. The app validates recreated run ids as real ULIDs whose
/// embedded timestamp is within [-24h, +5min], so replay MUST mint a genuine
/// ULID (a uuid would be rejected).
fn ulid() -> String {
    const ENC: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let ts = hive_core::now_ms();
    let mut rnd = [0u8; 10];
    let _ = ring::rand::SystemRandom::new().fill(&mut rnd).map_err(|_| {
        // extremely unlikely; fall back to time-derived bytes so we never panic
        for (i, b) in rnd.iter_mut().enumerate() {
            *b = ((ts >> (i * 5)) & 0xff) as u8;
        }
    });
    // 128-bit value: [48-bit ts][80-bit random]
    let mut bytes = [0u8; 16];
    bytes[0] = ((ts >> 40) & 0xff) as u8;
    bytes[1] = ((ts >> 32) & 0xff) as u8;
    bytes[2] = ((ts >> 24) & 0xff) as u8;
    bytes[3] = ((ts >> 16) & 0xff) as u8;
    bytes[4] = ((ts >> 8) & 0xff) as u8;
    bytes[5] = (ts & 0xff) as u8;
    bytes[6..16].copy_from_slice(&rnd);
    // 128 bits -> 26 base32 chars, MSB-first (5 bits each; the top char only
    // has 3 significant bits since 26*5 = 130 > 128).
    let val = u128::from_be_bytes(bytes);
    let mut out = [0u8; 26];
    for (oi, i) in (0..26).rev().enumerate() {
        let shift: u32 = (i as u32) * 5;
        let idx = ((val >> shift) & 0x1f) as usize;
        out[oi] = ENC[idx];
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Acquire the app's per-run lock (`SET <p>:lock:run:<id> <tok> NX PX 15000`).
/// Returns the token to release with, or None if held (contention with the
/// app runtime — the caller retries a few times).
async fn world_lock(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run_id: &str) -> Option<String> {
    let key = format!("{p}:lock:run:{run_id}");
    let lock_tok = format!("hive-{}", ulid());
    let res = cmd(cloud, url, token, &["SET", &key, &lock_tok, "NX", "PX", "15000"]).await?;
    // Upstash returns "OK" on success, null when NX fails.
    if res.as_str() == Some("OK") { Some(lock_tok) } else { None }
}

/// Release the lock only if we still hold it (Lua compare-and-delete — never
/// delete a lock the app runtime re-acquired after ours expired).
async fn world_unlock(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run_id: &str, lock_tok: &str) {
    let key = format!("{p}:lock:run:{run_id}");
    let script = "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end";
    let _ = cmd(cloud, url, token, &["EVAL", script, "1", &key, lock_tok]).await;
}

/// Take the run lock, retrying briefly if the app runtime holds it.
async fn with_run_lock(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run_id: &str) -> Option<String> {
    for _ in 0..8 {
        if let Some(t) = world_lock(cloud, url, token, p, run_id).await {
            return Some(t);
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    None
}

/// Append one event: HSET the payload + RPUSH the id onto the run's event list
/// + RPUSH the correlation index (exactly `@open-workflow/world-redis`
/// `entities.js` appendEvent).
async fn append_event(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run_id: &str, event: &Value) {
    let event_id = event.get("eventId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let blob = encode_blob(event);
    let _ = cmd(cloud, url, token, &["HSET", &format!("{p}:eventdata:{run_id}"), &event_id, &blob]).await;
    let _ = cmd(cloud, url, token, &["RPUSH", &format!("{p}:events:{run_id}"), &event_id]).await;
    if let Some(cid) = event.get("correlationId").and_then(|v| v.as_str()) {
        if !cid.is_empty() {
            let _ = cmd(cloud, url, token, &["RPUSH", &format!("{p}:corr:{cid}"), &format!("{run_id}|{event_id}")]).await;
        }
    }
}

/// Build one event record with a fresh id + created timestamp.
fn make_event(run_id: &str, event_type: &str, correlation_id: Option<&str>, event_data: Value, spec_version: i64) -> Value {
    let now = hive_core::now_ms();
    let mut e = json!({
        "eventId": format!("evnt_{}", ulid()),
        "runId": run_id,
        "eventType": event_type,
        "createdAt": now,
        "specVersion": spec_version,
        "eventData": event_data,
    });
    if let Some(cid) = correlation_id {
        e.as_object_mut().unwrap().insert("correlationId".into(), json!(cid));
    }
    e
}

/// Read one run record (decoded JSON) or None.
async fn get_run(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run_id: &str) -> Option<Value> {
    cmd(cloud, url, token, &["GET", &format!("{p}:run:{run_id}")]).await.and_then(|v| decode_blob(&v))
}

/// Persist a run record + maintain the `<p>:runs` and per-status ZSET indexes.
async fn put_run(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run: &Value, prev_status: Option<&str>) {
    let run_id = run.get("runId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let created = run.get("createdAt").and_then(|v| v.as_f64()).unwrap_or(hive_core::now_ms() as f64);
    let status = run.get("status").and_then(|v| v.as_str()).unwrap_or("pending").to_string();
    let _ = cmd(cloud, url, token, &["SET", &format!("{p}:run:{run_id}"), &encode_blob(run)]).await;
    let score = format!("{}", created as i64);
    let _ = cmd(cloud, url, token, &["ZADD", &format!("{p}:runs"), &score, &run_id]).await;
    if prev_status != Some(status.as_str()) {
        if let Some(prev) = prev_status {
            let _ = cmd(cloud, url, token, &["ZREM", &format!("{p}:runs:status:{prev}"), &run_id]).await;
        }
        let _ = cmd(cloud, url, token, &["ZADD", &format!("{p}:runs:status:{status}"), &score, &run_id]).await;
    }
}

/// Enqueue `{runId}` on the app's own world-redis queue (job HASH + sched ZSET)
/// so the app's in-process dispatcher delivers it to `/.well-known/workflow/v1/
/// flow` and the run resumes/executes — the storage-level trigger of
/// conformance-spec §4.3. `route` = "flow" for workflow topics.
///
/// The stored `body` shape is dictated by the REAL consumer, not us: the real
/// `@open-workflow/world-redis` dispatcher's HTTP handler ignores the request
/// body entirely (`dispatchOne` POSTs an EMPTY body — the `?msg=` query param
/// is the lookup key) and reads the job it stores server-side via
/// `readJob(msgId)`, then `decodeBlob(job.body)`, then reads `message.runId`
/// DIRECTLY off the decoded value — see the vendored 0.1.2 source,
/// `createQueueHandler`: `const message = decodeBlob(job.body); const runId =
/// message?.runId ?? message?.workflowRunId;`. That flows straight into
/// `@workflow/core`'s runtime, which parses it against
/// `WorkflowInvokePayloadSchema = z.object({ runId: z.string(), ... })` — a
/// FLAT top-level `runId`, never `{payload:{runId}}`. Live-witnessed the exact
/// failure this produces: a manually delivered job with the old nested shape
/// hit a real app-level 500, `Zod: "runId" expected string, received
/// undefined` — the request reached the app fine (proving the gateway/tunnel
/// layer, fixed earlier this session via the iroh_addr staleness fix, was
/// never the remaining problem); the STORED MESSAGE SHAPE was wrong the whole
/// time, so no run could ever have progressed past this call even on a
/// perfectly healthy tunnel — which is the real, deeper root cause of
/// [`reconcile_orphan_jobs`]'s endless orphan/reschedule/poison churn.
async fn enqueue_run(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run_id: &str, workflow_name: &str) {
    let queue_name = format!("__wkf_workflow_{}", clean_name(workflow_name));
    let msg_id = format!("msg_{}", ulid());
    let body = json!({ "runId": run_id });
    let body_b64 = encode_blob(&body);
    // owf:job:<id> HASH { queueName, body(base64 CBOR), attempt, route }
    let _ = cmd(cloud, url, token, &[
        "HSET", &format!("{p}:job:{msg_id}"),
        "queueName", &queue_name,
        "body", &body_b64,
        "attempt", "1",
        "route", "flow",
    ]).await;
    // owf:sched ZSET score=runAt(now) member=msgId
    let now = format!("{}", hive_core::now_ms());
    let _ = cmd(cloud, url, token, &["ZADD", &format!("{p}:sched"), &now, &msg_id]).await;
}

/// The four run operations, each returning the updated/ new run's id + a small
/// summary the console renders. `op` ∈ cancel|reenqueue|wakeup|replay.
pub async fn run_op(cloud: &Arc<CloudState>, project: &str, run_id: &str, op: &str, cancel_reason: Option<&str>) -> Result<Value, String> {
    let (url, token, p) = world_config(cloud, project).ok_or_else(|| "no world configured for project".to_string())?;
    let run = get_run(cloud, &url, &token, &p, run_id).await.ok_or_else(|| "run not found".to_string())?;
    let spec_version = run.get("specVersion").and_then(|v| v.as_i64()).unwrap_or(2);
    let workflow_name = run.get("workflowName").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let status = run.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();

    match op {
        "cancel" => {
            let lock = with_run_lock(cloud, &url, &token, &p, run_id).await.ok_or_else(|| "run is busy (locked by the runtime); retry".to_string())?;
            let terminal = matches!(status.as_str(), "completed" | "failed" | "cancelled");
            if !terminal {
                let now = hive_core::now_ms();
                let mut updated = run.clone();
                if let Some(o) = updated.as_object_mut() {
                    o.insert("status".into(), json!("cancelled"));
                    o.insert("completedAt".into(), json!(now));
                    o.insert("updatedAt".into(), json!(now));
                    o.remove("output");
                    o.remove("error");
                }
                let data = cancel_reason.map(|r| json!({ "cancelReason": r })).unwrap_or(json!({}));
                append_event(cloud, &url, &token, &p, run_id, &make_event(run_id, "run_cancelled", None, data, spec_version)).await;
                put_run(cloud, &url, &token, &p, &updated, Some(&status)).await;
                terminal_cleanup(cloud, &url, &token, &p, run_id).await;
            }
            world_unlock(cloud, &url, &token, &p, run_id, &lock).await;
            Ok(json!({ "runId": run_id, "status": "cancelled", "op": "cancel", "alreadyTerminal": terminal }))
        }
        "reenqueue" => {
            enqueue_run(cloud, &url, &token, &p, run_id, &workflow_name).await;
            Ok(json!({ "runId": run_id, "op": "reenqueue", "enqueued": true }))
        }
        "wakeup" => {
            let lock = with_run_lock(cloud, &url, &token, &p, run_id).await.ok_or_else(|| "run is busy (locked by the runtime); retry".to_string())?;
            let stopped = complete_pending_waits(cloud, &url, &token, &p, run_id, spec_version).await;
            world_unlock(cloud, &url, &token, &p, run_id, &lock).await;
            if stopped > 0 {
                enqueue_run(cloud, &url, &token, &p, run_id, &workflow_name).await;
            }
            Ok(json!({ "runId": run_id, "op": "wakeup", "stoppedCount": stopped }))
        }
        "replay" => {
            // Clone the run from its ORIGINAL input bytes into a brand-new run.
            let input = run.get("input").cloned().unwrap_or(Value::Null);
            let deployment_id = run.get("deploymentId").and_then(|v| v.as_str()).unwrap_or("dpl_redis_local").to_string();
            let execution_context = run.get("executionContext").cloned().unwrap_or(json!({}));
            let new_id = format!("wrun_{}", ulid());
            let now = hive_core::now_ms();
            let new_run = json!({
                "runId": new_id,
                "status": "pending",
                "deploymentId": deployment_id,
                "workflowName": workflow_name,
                "specVersion": spec_version,
                "executionContext": execution_context,
                "input": input,
                "attributes": {},
                "createdAt": now,
                "updatedAt": now,
                "replayedFromRunId": run_id,
            });
            let lock = with_run_lock(cloud, &url, &token, &p, &new_id).await.ok_or_else(|| "could not lock the new run".to_string())?;
            append_event(cloud, &url, &token, &p, &new_id,
                &make_event(&new_id, "run_created", None, json!({
                    "deploymentId": deployment_id, "workflowName": workflow_name, "input": new_run["input"],
                    "executionContext": execution_context,
                }), spec_version)).await;
            put_run(cloud, &url, &token, &p, &new_run, None).await;
            world_unlock(cloud, &url, &token, &p, &new_id, &lock).await;
            enqueue_run(cloud, &url, &token, &p, &new_id, &workflow_name).await;
            Ok(json!({ "runId": new_id, "op": "replay", "replayedFromRunId": run_id }))
        }
        _ => Err(format!("unknown run op '{op}'")),
    }
}

/// Complete every pending wait (a `wait_created` with no matching
/// `wait_completed` by correlationId) — the "cancel active sleeps" effect. Reads
/// the event log, writes a `wait_completed` per pending wait + flips the wait
/// entity to completed. Returns how many were stopped.
async fn complete_pending_waits(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run_id: &str, spec_version: i64) -> u64 {
    let ev_ids = cmd(cloud, url, token, &["LRANGE", &format!("{p}:events:{run_id}"), "0", "-1"]).await;
    let ev_ids: Vec<String> = ev_ids
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()))
        .unwrap_or_default();
    if ev_ids.is_empty() {
        return 0;
    }
    let mut hargs = vec!["HMGET".to_string(), format!("{p}:eventdata:{run_id}")];
    hargs.extend(ev_ids.iter().cloned());
    let hargs_ref: Vec<&str> = hargs.iter().map(|s| s.as_str()).collect();
    let mut created: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(arr) = cmd(cloud, url, token, &hargs_ref).await.and_then(|v| v.as_array().cloned()) {
        for b in arr {
            if let Some(ev) = decode_blob(&b) {
                let et = ev.get("eventType").and_then(|x| x.as_str()).unwrap_or("");
                let cid = ev.get("correlationId").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if cid.is_empty() { continue; }
                match et {
                    "wait_created" => { created.insert(cid, ev); }
                    "wait_completed" => { completed.insert(cid); }
                    _ => {}
                }
            }
        }
    }
    let mut stopped = 0u64;
    for (cid, ev) in created {
        if completed.contains(&cid) { continue; }
        let resume_at = ev.get("eventData").and_then(|d| d.get("resumeAt")).cloned().unwrap_or(json!(hive_core::now_ms()));
        append_event(cloud, url, token, p, run_id,
            &make_event(run_id, "wait_completed", Some(&cid), json!({ "resumeAt": resume_at }), spec_version)).await;
        // Flip the wait entity to completed (best-effort; the event is the source of truth for replay).
        if let Some(w) = cmd(cloud, url, token, &["GET", &format!("{p}:wait:{run_id}:{cid}")]).await.and_then(|v| decode_blob(&v)) {
            let mut w2 = w;
            if let Some(o) = w2.as_object_mut() {
                o.insert("status".into(), json!("completed"));
                o.insert("completedAt".into(), json!(hive_core::now_ms()));
                o.insert("updatedAt".into(), json!(hive_core::now_ms()));
            }
            let _ = cmd(cloud, url, token, &["SET", &format!("{p}:wait:{run_id}:{cid}"), &encode_blob(&w2)]).await;
        }
        stopped += 1;
    }
    stopped
}

/// Terminal cleanup on cancel: drop the run's hooks (+ token claims) and waits,
/// matching world-redis `terminalCleanup`. Best-effort.
async fn terminal_cleanup(cloud: &Arc<CloudState>, url: &str, token: &str, p: &str, run_id: &str) {
    // hooks for this run
    if let Some(ids) = cmd(cloud, url, token, &["ZRANGE", &format!("{p}:hooks:run:{run_id}"), "0", "-1"]).await.and_then(|v| v.as_array().cloned()) {
        for h in ids {
            if let Some(hid) = h.as_str() {
                if let Some(hook) = cmd(cloud, url, token, &["GET", &format!("{p}:hook:{hid}")]).await.and_then(|v| decode_blob(&v)) {
                    if let Some(tok) = hook.get("token").and_then(|t| t.as_str()) {
                        let sha = sha256_hex(tok);
                        let _ = cmd(cloud, url, token, &["DEL", &format!("{p}:hooktoken:{sha}")]).await;
                    }
                }
                let _ = cmd(cloud, url, token, &["DEL", &format!("{p}:hook:{hid}")]).await;
                let _ = cmd(cloud, url, token, &["ZREM", &format!("{p}:hooks"), hid]).await;
            }
        }
        let _ = cmd(cloud, url, token, &["DEL", &format!("{p}:hooks:run:{run_id}")]).await;
    }
    // waits for this run
    if let Some(ids) = cmd(cloud, url, token, &["ZRANGE", &format!("{p}:waits:{run_id}"), "0", "-1"]).await.and_then(|v| v.as_array().cloned()) {
        for w in ids {
            if let Some(cid) = w.as_str() {
                let _ = cmd(cloud, url, token, &["DEL", &format!("{p}:wait:{run_id}:{cid}")]).await;
            }
        }
        let _ = cmd(cloud, url, token, &["DEL", &format!("{p}:waits:{run_id}")]).await;
    }
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// List workflow "hooks" for a project (newest first), each tagged with `project`.
///
/// A hook is a `hook_created` event; its live invocation count is the number of
/// `hook_received` events in the SAME run whose `correlationId` matches the hook.
/// When `run_id` is Some we scan only that run's events; otherwise we scan the
/// most recent `limit` runs (capped to bound the per-run event fan-out). Reads use
/// the same LIST-index + HASH-payload pattern as [`run_detail`]. Returns None when
/// no world is configured; Some(empty) when a world exists but has no hooks.
pub async fn list_hooks(cloud: &Arc<CloudState>, project: &str, run_id: Option<&str>, limit: usize) -> Option<Vec<Value>> {
    let (url, token, p) = world_config(cloud, project)?;
    // Which runs to scan: the explicit one, else the most recent `limit` (bounded
    // to ~50 so a project with thousands of runs can't trigger a huge fan-out).
    let run_ids: Vec<String> = if let Some(rid) = run_id {
        vec![rid.to_string()]
    } else {
        let cap = limit.clamp(1, 50);
        let hi = cap.saturating_sub(1).to_string();
        let ids = cmd(cloud, &url, &token, &["ZRANGE", &format!("{p}:runs"), "0", &hi, "REV"]).await?;
        ids.as_array()?.iter().filter_map(|x| x.as_str().map(String::from)).collect()
    };
    let mut out: Vec<Value> = Vec::new();
    for rid in run_ids {
        // Events for this run (append-ordered LIST + HASH of payloads) — mirror `run_detail`.
        let ev_ids = cmd(cloud, &url, &token, &["LRANGE", &format!("{p}:events:{rid}"), "0", "-1"]).await;
        let ev_ids: Vec<String> = ev_ids
            .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()))
            .unwrap_or_default();
        if ev_ids.is_empty() {
            continue;
        }
        let mut hargs = vec!["HMGET".to_string(), format!("{p}:eventdata:{rid}")];
        hargs.extend(ev_ids.iter().cloned());
        let hargs_ref: Vec<&str> = hargs.iter().map(|s| s.as_str()).collect();
        let mut events: Vec<Value> = Vec::new();
        if let Some(arr) = cmd(cloud, &url, &token, &hargs_ref).await.and_then(|v| v.as_array().cloned()) {
            for b in arr {
                if let Some(ev) = decode_blob(&b) {
                    events.push(ev);
                }
            }
        }
        if events.is_empty() {
            continue;
        }
        let etype = |e: &Value| e.get("eventType").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let corr = |e: &Value| e.get("correlationId").and_then(|x| x.as_str()).unwrap_or("").to_string();
        // Count invocations (`hook_received`) per hookId within this run.
        let mut invocations: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for ev in &events {
            if etype(ev) == "hook_received" {
                let cid = corr(ev);
                if !cid.is_empty() {
                    *invocations.entry(cid).or_insert(0) += 1;
                }
            }
        }
        // One hook row per `hook_created` event; fold in dispose/conflict lifecycle.
        for ev in &events {
            if etype(ev) != "hook_created" {
                continue;
            }
            let data = ev.get("eventData").cloned().unwrap_or(Value::Null);
            let dget = |k: &str| data.get(k).cloned();
            let hook_id = {
                let c = corr(ev);
                if !c.is_empty() {
                    c
                } else {
                    dget("hookId").and_then(|x| x.as_str().map(String::from)).unwrap_or_default()
                }
            };
            let token_val = dget("token").filter(|v| v.is_string()).unwrap_or(Value::Null);
            let metadata = dget("metadata").unwrap_or(Value::Null);
            let is_webhook = dget("isWebhook")
                .and_then(|x| x.as_bool())
                .or_else(|| metadata.get("isWebhook").and_then(|x| x.as_bool()))
                .unwrap_or(false);
            let created_at = ev.get("createdAt").cloned().unwrap_or(Value::Null);
            let created_ms = secs_to_ms(ev.get("createdAt")).unwrap_or(0);
            let inv_count = invocations.get(&hook_id).copied().unwrap_or(0);
            // Optional lifecycle state from later `hook_disposed` / `hook_conflict`.
            let matches_hook = |e: &Value| !hook_id.is_empty() && corr(e) == hook_id;
            let disposed = events.iter().any(|e| etype(e) == "hook_disposed" && matches_hook(e));
            let conflicted = events.iter().any(|e| etype(e) == "hook_conflict" && matches_hook(e));
            let status = if conflicted {
                "conflict"
            } else if disposed {
                "disposed"
            } else {
                "active"
            };
            out.push(json!({
                "hookId": hook_id,
                "runId": rid,
                "token": token_val,
                "isWebhook": is_webhook,
                "metadata": metadata,
                "createdAt": created_at,
                "created_ms": created_ms,
                "invocations": inv_count,
                "status": status,
                "project": project,
            }));
        }
    }
    // Newest-first by created_ms.
    out.sort_by(|a, b| {
        let am = a.get("created_ms").and_then(|x| x.as_u64()).unwrap_or(0);
        let bm = b.get("created_ms").and_then(|x| x.as_u64()).unwrap_or(0);
        bm.cmp(&am)
    });
    Some(out)
}

/// Reschedule ORPHANED queue jobs: an `<p>:job:<msgId>` hash whose id is
/// absent from the `<p>:sched` ZSET is a job some dispatcher CLAIMED (ZREM)
/// and then failed to deliver without rescheduling — witnessed live when a
/// client-side response-encoding mismatch 404'd every delivery and
/// `@open-workflow/world-redis` 0.1.2's dispatchOne dropped the claim on any
/// non-OK response (its comment assumes the handler wrapper reschedules, but
/// the wrapper never RAN on a 404). The stranded run then sits `pending`
/// forever. Delivery is idempotent (the handler treats a missing/terminal job
/// as a no-op and caps attempts), so blanket rescheduling is safe. Jobs
/// younger than the grace window are skipped: a just-claimed job legitimately
/// lives outside the ZSET for the duration of its 307 trampoline chain.
/// Deliver an orphaned workflow queue job to the app's `flow` endpoint FROM THE
/// HOST NODE, bypassing the app's own in-cell dispatcher.
///
/// WHY this exists: the `@open-workflow/world-redis` dispatcher runs INSIDE the
/// app cell and delivers each job by POSTing to the app's own PUBLIC url
/// (`https://<app>/.well-known/workflow/v1/flow`). A cell cannot reliably reach
/// its own public host — the per-cell NAT hairpin (cell → node's own public IP)
/// is dropped — so that in-cell POST fails `RUNTIME_TUNNEL_FAILED`/`NOT_RETRYABLE`,
/// the dispatcher drops the job, and the run stalls forever (witnessed live on
/// shoomoo: 0×200 / N×502 from inside the cell while the SAME POST from a host
/// node returns 200). A host-origin POST reaches the local edge without the
/// hairpin, so the host CAN deliver what the cell can't. Delivery is idempotent
/// (the flow handler keys on the msg id), so this racing a lucky in-cell attempt
/// is safe. Returns true only on a 2xx ack. Single bounded attempt — the 60s
/// reconcile cycle IS the retry cadence, so one slow/dead cell can't wedge the
/// pass.
async fn deliver_orphan_job(cloud: &Arc<CloudState>, project: &str, msg_id: &str) -> bool {
    let domain = cloud.apps_domain.trim().trim_start_matches('.');
    if domain.is_empty() {
        return false;
    }
    let url = format!("https://{project}.{domain}/.well-known/workflow/v1/flow?msg={msg_id}");
    // A single app can spread across several cells that cold-start under load, so
    // any one delivery attempt can transiently 502 (RUNTIME_TUNNEL_FAILED) while
    // a sibling instance is warming — the SAME endpoint returns 200 moments later.
    // A few spaced attempts catch a warm instance within that flap window instead
    // of dropping the job back to the (broken) in-cell dispatcher for another 60s.
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1200 * attempt as u64)).await;
        }
        match cloud
            .http
            .post(&url)
            // Matches the real dispatcher's own request exactly (world-redis
            // 0.1.2 `dispatchOne`): `?msg=` present means the handler looks the
            // job up server-side via `readJob(msgId)` and never reads the
            // request body at all, so an empty body is correct, not a
            // placeholder — sending `{}` worked identically only by accident.
            .header("content-type", "application/json")
            .body("")
            .timeout(std::time::Duration::from_secs(12))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return true,
            // A 404 means the app has no such job to run (already consumed, or its
            // run is gone) — not deliverable, and retrying won't change that.
            Ok(r) if r.status().as_u16() == 404 => return false,
            _ => continue,
        }
    }
    false
}

pub async fn reconcile_orphan_jobs(cloud: &Arc<CloudState>) -> (usize, usize) {
    const GRACE_MS: u64 = 120_000;
    /// Reschedules before a persistently-undeliverable job is GC'd as poison.
    const MAX_RECONCILES: i64 = 5;
    // Ids seen orphaned on the PREVIOUS pass, per project. A job legitimately
    // leaves the sched ZSET for the whole time a delivery is IN FLIGHT
    // (dispatchOne ZREM-claims, then the handler wrapper re-schedules or
    // deletes) — a single-pass check re-added those claims and DOUBLE-fired
    // deliveries (witnessed: the same msg ids "rescheduled" 17-18x/20min).
    // Only an id orphaned on TWO consecutive passes (>=60s apart) is treated
    // as genuinely dropped.
    static PREV_ORPHANS: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>> = std::sync::OnceLock::new();
    let mut scanned = 0usize;
    let mut rescheduled = 0usize;
    let mut delivered = 0usize;
    let projects: Vec<String> = cloud
        .projects
        .snapshot()
        .into_iter()
        .map(|(name, _)| name)
        .filter(|p| has_world(cloud, p))
        .collect();
    for project in projects {
        let Some((url, token, p)) = world_config(cloud, &project) else { continue };
        // Only the project's HOST node reconciles: every node CAN read the
        // world (env is gossiped), and duplicate ZADDs are idempotent, but one
        // writer keeps the reconcile log attributable and halves REST load.
        if cloud.gw.git_for_project(&project).is_none() {
            continue;
        }
        let sched: std::collections::HashSet<String> = match cmd(cloud, &url, &token, &["ZRANGE", &format!("{p}:sched"), "0", "-1"]).await {
            Some(Value::Array(a)) => a.into_iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => continue, // world unreachable — do not guess
        };
        let keys = match cmd(cloud, &url, &token, &["SCAN", "0", "MATCH", &format!("{p}:job:*"), "COUNT", "1000"]).await {
            Some(Value::Array(a)) if a.len() == 2 => match &a[1] {
                Value::Array(ks) => ks.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>(),
                _ => continue,
            },
            _ => continue,
        };
        let now = hive_core::now_ms();
        let mut cur: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut confirmed: Vec<String> = Vec::new();
        {
            let prev_map = PREV_ORPHANS.get_or_init(Default::default);
            let prev = prev_map.lock().get(&project).cloned().unwrap_or_default();
            for key in keys {
                scanned += 1;
                let Some(msg_id) = key.strip_prefix(&format!("{p}:job:")) else { continue };
                // Real queue jobs only: `<p>:job:idem:*` are plain-string
                // idempotency markers, not job hashes — never schedulable.
                if !msg_id.starts_with("msg_") {
                    continue;
                }
                if sched.contains(msg_id) {
                    continue;
                }
                // msg_<ULID>: the ULID's leading 10 chars carry the creation
                // time — skip fresh jobs still inside a redirect-chain claim.
                if let Some(ts) = ulid_ms(msg_id.trim_start_matches("msg_")) {
                    if now.saturating_sub(ts) < GRACE_MS {
                        continue;
                    }
                }
                cur.insert(msg_id.to_string());
                if prev.contains(msg_id) {
                    confirmed.push(msg_id.to_string());
                }
            }
            prev_map.lock().insert(project.clone(), cur);
        }
        for msg_id in confirmed {
            // Terminal-run GC: a job whose run already finished will never be
            // consumed by a delivery (the handler no-ops without deleting) —
            // delete it instead of rescheduling forever.
            if let Some(body) = cmd(cloud, &url, &token, &["HGET", &format!("{p}:job:{msg_id}"), "body"]).await {
                if let Some(decoded) = decode_blob(&body) {
                    let run_id = decoded
                        .get("runId")
                        .or_else(|| decoded.get("payload").and_then(|p| p.get("runId")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !run_id.is_empty() {
                        if let Some(run) = get_run(cloud, &url, &token, &p, run_id).await {
                            let status = run.get("status").and_then(|v| v.as_str()).unwrap_or("");
                            if matches!(status, "completed" | "failed" | "cancelled") {
                                let _ = cmd(cloud, &url, &token, &["DEL", &format!("{p}:job:{msg_id}")]).await;
                                tracing::info!(project = %project, msg_id = %msg_id, status, "world reconcile: GC'd job for terminal run");
                                continue;
                            }
                        }
                    }
                }
            }
            // HOST-DELIVER FIRST: the app's in-cell dispatcher can't reach its
            // own public url (NAT hairpin), so deliver the job from here — the
            // host CAN reach the flow endpoint. On success the flow handler
            // consumes the job; nothing to reschedule. Only if the host can't
            // deliver either (cell genuinely down) do we fall through to the
            // reschedule/poison path so the app dispatcher still gets a shot and
            // a permanently-dead job still gets GC'd.
            if deliver_orphan_job(cloud, &project, &msg_id).await {
                delivered += 1;
                tracing::info!(project = %project, msg_id = %msg_id, "world reconcile: delivered orphaned queue job from host (bypassing broken in-cell dispatch)");
                continue;
            }
            // Poison cap: a job this loop keeps having to rescue is not being
            // consumed by deliveries at all — stop churning it.
            let n = match cmd(cloud, &url, &token, &["HINCRBY", &format!("{p}:job:{msg_id}"), "reconcile_n", "1"]).await {
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
                Some(Value::String(s)) => s.parse().unwrap_or(0),
                _ => 0,
            };
            if n > MAX_RECONCILES {
                let _ = cmd(cloud, &url, &token, &["DEL", &format!("{p}:job:{msg_id}")]).await;
                tracing::warn!(project = %project, msg_id = %msg_id, reconciles = n, "world reconcile: GC'd poison job (never consumed after repeated reschedules)");
                continue;
            }
            let score = format!("{now}");
            if cmd(cloud, &url, &token, &["ZADD", &format!("{p}:sched"), &score, &msg_id]).await.is_some() {
                rescheduled += 1;
                tracing::info!(project = %project, msg_id = %msg_id, "world reconcile: rescheduled orphaned queue job");
            }
        }
    }
    (scanned, rescheduled + delivered)
}

/// Crockford-base32 ULID timestamp (first 10 chars → ms since epoch).
fn ulid_ms(ulid: &str) -> Option<u64> {
    const ALPHA: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let ts = ulid.get(..10)?;
    let mut v: u64 = 0;
    for ch in ts.bytes() {
        let d = ALPHA.iter().position(|&a| a == ch.to_ascii_uppercase())? as u64;
        v = v.checked_mul(32)?.checked_add(d)?;
    }
    Some(v)
}

/// 60s background loop around [`reconcile_orphan_jobs`]. Supervised: this loop
/// is the one whose silent death was indistinguishable from "queue is clean"
/// (it logs only when it fixes something), which is exactly what supervision's
/// heartbeat + restart visibility exists to disambiguate.
pub fn spawn_world_reconcile(cloud: Arc<CloudState>) {
    crate::supervise::spawn_supervised("world-reconcile", move || {
        let cloud = cloud.clone();
        async move {
            // Let gossip/env settle after boot before the first pass.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            loop {
                crate::supervise::beat("world-reconcile");
                let (scanned, fixed) = reconcile_orphan_jobs(&cloud).await;
                if fixed > 0 {
                    tracing::warn!(scanned, fixed, "world reconcile: recovered orphaned workflow queue jobs");
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        }
    });
}
