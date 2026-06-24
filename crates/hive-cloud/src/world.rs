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

use std::sync::Arc;

use base64::Engine;
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
fn decode_blob(v: &Value) -> Option<Value> {
    let b64 = v.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let cv: serde_cbor::Value = serde_cbor::from_slice(&bytes).ok()?;
    Some(cbor_to_json(cv))
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
