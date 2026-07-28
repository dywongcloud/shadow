//! Managed serverless GPU inference — the platform half of the llama.cpp
//! pooling stack (the node half, `llama-rpc.service` on every GPU node, is
//! provisioned infrastructure: CUDA `rpc-server` on port 50052, peers-only via
//! hive-lockdown).
//!
//! A project opts in with a `fluid.json` top-level block
//! (`{"inference": {"model": "<gguf-url-or-hf-path>", "pool": true}}` →
//! [`crate::project_settings::InferenceSpec`], synced into the replicated
//! projects store by the deploy path). This module then:
//!
//!  * elects ONE coordinator GPU node per project — deterministically
//!    (FNV hash of the project over the sorted GPU-node roster), so every node
//!    computes the same assignment from the same gossiped registry with no new
//!    coordination protocol;
//!  * on that coordinator, downloads/caches the GGUF and runs a real
//!    `llama-server` (OpenAI-compatible HTTP API) bound to a deterministic
//!    per-project port in `50100..=50999` — a range hive-lockdown drops for
//!    non-peers, so the endpoint is fleet-internal only;
//!  * decides single-node vs pooled per the placement rule: if the model fits
//!    the coordinator's free VRAM it runs alone (no pooling overhead); only
//!    when it does NOT fit does it engage llama.cpp's RPC layer-distribution
//!    (`--rpc member:50052,...`) across enough same-region pool members to
//!    cover the need. Deliberately NO silent CPU fallback: a model that cannot
//!    fit even the pool's aggregate free VRAM parks the endpoint in a `failed`
//!    status with an honest error.
//!  * on the control-plane leader, injects `HIVE_INFERENCE_URL` into the
//!    project env (same precedent as DB env auto-injection) so app code —
//!    Next.js, Flask, Django, FastAPI, Fastify, anything — just reads one env
//!    var and speaks OpenAI-protocol to it.
//!
//! Reconcile-loop discipline matches `spawn_db_reconcile`: desired state is
//! derived fresh every tick from replicated stores; the node hosting a piece
//! of it converges its own local reality (spawn/kill children), self-healing
//! across restarts with no imperative cross-node RPC.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::state::CloudState;

/// VRAM safety margin: quantized weights get mmapped roughly at file size; KV
/// cache, CUDA context and fragmentation ride on top. 1.2x + 800MB tracks the
/// live-measured footprint of the small-model E2E within reason without
/// starving co-tenants.
fn vram_need_mb(model_bytes: u64) -> u64 {
    (model_bytes / (1024 * 1024)) * 12 / 10 + 800
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Deterministic per-project endpoint port. Collisions between two projects
/// hashing to the same slot are tolerated by the coordinator check (second
/// server fails to bind, surfaces as failed status) — with 900 slots this is
/// rare enough to keep the mapping stateless.
pub fn port_for(project: &str) -> u16 {
    50100 + (fnv(project) % 900) as u16
}

/// The GPU roster this module schedules over: healthy, GPU-carrying nodes,
/// sorted by name for cross-node determinism.
fn gpu_roster(cloud: &Arc<CloudState>) -> Vec<hive_edge::NodeInfo> {
    let mut v: Vec<_> =
        cloud.registry.nodes().into_iter().filter(|n| n.healthy && n.gpu_count > 0).collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Deterministic coordinator election: same inputs (gossiped registry) →
/// same answer on every node. Region choice: the region with the largest
/// total pool VRAM (stable, capacity-anchored); coordinator: project-hash
/// over that region's sorted members.
pub fn coordinator_for(cloud: &Arc<CloudState>, project: &str) -> Option<hive_edge::NodeInfo> {
    let roster = gpu_roster(cloud);
    if roster.is_empty() {
        return None;
    }
    let mut by_region: HashMap<String, (u64, Vec<hive_edge::NodeInfo>)> = HashMap::new();
    for n in roster {
        let e = by_region.entry(n.region.clone()).or_default();
        e.0 += n.gpu_vram_mb;
        e.1.push(n);
    }
    let (_, (_, members)) = by_region
        .into_iter()
        .max_by(|a, b| a.1 .0.cmp(&b.1 .0).then(b.0.cmp(&a.0)))
        .map(|(k, v)| (k.clone(), v))?;
    let idx = (fnv(project) % members.len() as u64) as usize;
    members.into_iter().nth(idx)
}

/// Resolve a model ref to a fetchable URL: pass-through for http(s), else
/// treat `org/repo/file.gguf` as a HuggingFace path.
pub fn model_url(model: &str) -> Option<String> {
    if model.starts_with("http://") || model.starts_with("https://") {
        return Some(model.to_string());
    }
    let parts: Vec<&str> = model.split('/').collect();
    if parts.len() >= 3 && model.ends_with(".gguf") {
        let (org, repo) = (parts[0], parts[1]);
        let file = parts[2..].join("/");
        return Some(format!("https://huggingface.co/{org}/{repo}/resolve/main/{file}"));
    }
    None
}

/// One managed endpoint's live status, as reported by its coordinator (and
/// mirrored into `/v1/inference` via gossip-free local read + leader listing).
#[derive(Clone, Debug, Serialize)]
pub struct EndpointStatus {
    pub project: String,
    pub model: String,
    pub coordinator: String,
    pub port: u16,
    pub pool: bool,
    pub rpc_members: Vec<String>,
    /// "starting" | "running" | "failed: <reason>"
    pub status: String,
    pub updated_ms: u64,
}

#[derive(Default)]
pub struct InferenceRuntime {
    /// project → (child, status). Children are this process's own — killed on
    /// process exit, respawned by the reconcile tick after a restart.
    servers: parking_lot::Mutex<HashMap<String, (Option<tokio::process::Child>, EndpointStatus)>>,
}

impl InferenceRuntime {
    pub fn statuses(&self) -> Vec<EndpointStatus> {
        let mut v: Vec<_> = self.servers.lock().values().map(|(_, s)| s.clone()).collect();
        v.sort_by(|a, b| a.project.cmp(&b.project));
        v
    }
}

fn llama_bin() -> String {
    std::env::var("HIVE_LLAMA_BIN").unwrap_or_else(|_| "/opt/llama/bin/llama-server".into())
}

fn models_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("HIVE_MODELS_DIR").unwrap_or_else(|_| "/root/models".into()),
    )
}

/// Download-and-cache the model file (content-addressed by ref hash so a
/// changed ref refetches). Returns (path, size_bytes).
async fn ensure_model(model: &str) -> Result<(std::path::PathBuf, u64), String> {
    let url = model_url(model).ok_or_else(|| format!("unresolvable model ref '{model}'"))?;
    let dir = models_dir();
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let path = dir.join(format!("hive-{:016x}.gguf", fnv(&url)));
    if let Ok(md) = tokio::fs::metadata(&path).await {
        if md.len() > 0 {
            return Ok((path, md.len()));
        }
    }
    tracing::info!(%url, ?path, "inference: downloading model");
    let tmp = path.with_extension("part");
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("model fetch {} -> {}", url, resp.status()));
    }
    let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        f.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);
    tokio::fs::rename(&tmp, &path).await.map_err(|e| e.to_string())?;
    let size = tokio::fs::metadata(&path).await.map_err(|e| e.to_string())?.len();
    Ok((path, size))
}

/// Placement (pool-aware): fits the coordinator alone → no members. Else, when
/// pooling is allowed, add same-region members by descending free VRAM until
/// covered. Else honest failure.
fn plan_members(
    need_mb: u64,
    coordinator: &hive_edge::NodeInfo,
    pool_allowed: bool,
    pools: &[crate::gpu_pool::GpuPoolRegion],
) -> Result<Vec<String>, String> {
    let region_pool = pools.iter().find(|p| p.region == coordinator.region);
    let coord_free = region_pool
        .and_then(|p| p.nodes.iter().find(|n| n.name == coordinator.name))
        .map(|n| n.vram_free_mb)
        .unwrap_or(coordinator.gpu_vram_mb);
    if need_mb <= coord_free {
        return Ok(Vec::new());
    }
    if !pool_allowed {
        return Err(format!(
            "model needs ~{need_mb}MB VRAM but coordinator {} has {coord_free}MB free and pooling is disabled (set inference.pool=true)",
            coordinator.name
        ));
    }
    let Some(rp) = region_pool else {
        return Err("no pool data for coordinator region".into());
    };
    let mut covered = coord_free;
    let mut members = Vec::new();
    let mut others: Vec<_> = rp.nodes.iter().filter(|n| n.name != coordinator.name).collect();
    others.sort_by(|a, b| b.vram_free_mb.cmp(&a.vram_free_mb));
    for n in others {
        if covered >= need_mb {
            break;
        }
        covered += n.vram_free_mb;
        members.push(n.name.clone());
    }
    if covered < need_mb {
        return Err(format!(
            "model needs ~{need_mb}MB VRAM but the whole {} pool only has {covered}MB free",
            rp.region
        ));
    }
    Ok(members)
}

async fn health_ok(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    match reqwest::Client::new().get(&url).timeout(Duration::from_secs(3)).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Reconcile ONE endpoint on its coordinator (this node). Ensures model +
/// llama-server child; updates status in the runtime map.
async fn reconcile_local(cloud: &Arc<CloudState>, project: &str, spec: &crate::project_settings::InferenceSpec) {
    let port = port_for(project);
    // A LIVE child is never relaunched, healthy or not: llama-server answers
    // /health with 503 for the entire multi-minute model load (tensor upload
    // to RPC members dominates for pooled models), and treating that as dead
    // spawned a doomed duplicate every tick — the duplicate failed to bind
    // the port held by the loading child and exited, while overwriting (and
    // orphaning) the real child's handle. Live-witnessed on the first pooled
    // 14B endpoint. Only an EXITED child triggers a relaunch; health merely
    // flips the reported status.
    let child_running = {
        let mut servers = cloud.inference.servers.lock();
        if let Some((child, st)) = servers.get_mut(project) {
            if let Some(c) = child {
                match c.try_wait() {
                    Ok(None) => true,
                    _ => {
                        *child = None;
                        st.status = "starting".into();
                        false
                    }
                }
            } else {
                false
            }
        } else {
            false
        }
    };
    if child_running {
        let healthy = health_ok(port).await;
        let mut servers = cloud.inference.servers.lock();
        if let Some((_, st)) = servers.get_mut(project) {
            let new_status = if healthy { "running" } else { "starting" };
            if st.status != new_status {
                st.status = new_status.into();
                st.updated_ms = hive_core::now_ms();
                if healthy {
                    tracing::info!(project, port, "inference: endpoint healthy");
                }
            }
        }
        return;
    }

    let set_status = |cloud: &Arc<CloudState>, status: String, members: &[String]| {
        let mut servers = cloud.inference.servers.lock();
        let e = servers.entry(project.to_string()).or_insert_with(|| {
            (None, EndpointStatus {
                project: project.to_string(),
                model: spec.model.clone(),
                coordinator: cloud.node_name.clone(),
                port,
                pool: spec.pool,
                rpc_members: Vec::new(),
                status: "starting".into(),
                updated_ms: hive_core::now_ms(),
            })
        });
        e.1.model = spec.model.clone();
        e.1.pool = spec.pool;
        e.1.rpc_members = members.to_vec();
        e.1.status = status;
        e.1.updated_ms = hive_core::now_ms();
    };

    let (model_path, model_bytes) = match ensure_model(&spec.model).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(project, error = %e, "inference: model unavailable");
            set_status(cloud, format!("failed: {e}"), &[]);
            return;
        }
    };
    let need = vram_need_mb(model_bytes);
    let me = cloud.registry.nodes().into_iter().find(|n| n.is_self);
    let Some(me) = me else { return };
    let pools = crate::gpu_pool::snapshot(cloud).await;
    let members = match plan_members(need, &me, spec.pool, &pools) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(project, error = %e, "inference: placement failed");
            set_status(cloud, format!("failed: {e}"), &[]);
            return;
        }
    };
    // Member names → ip:50052 rpc endpoints (public IPs are peer-allowed
    // through hive-lockdown; verified reachable node-to-node).
    let member_addrs: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| members.contains(&n.name))
        .filter_map(|n| n.public_ip.map(|ip| format!("{ip}:50052")))
        .collect();

    // Kill any UNTRACKED llama-server for this project first (a survivor of a
    // previous hive-node process, or an orphan of the pre-fix overwrite bug) —
    // it holds the port, and a tracked child must own the endpoint so kill/
    // reassign semantics stay real.
    let _ = tokio::process::Command::new("pkill")
        .args(["-f", &format!("llama-server.*--alias {project}")])
        .output()
        .await;

    let mut cmd = tokio::process::Command::new(llama_bin());
    cmd.arg("-m")
        .arg(&model_path)
        .arg("--host")
        .arg("0.0.0.0")
        .arg("--port")
        .arg(port.to_string())
        .arg("--gpu-layers")
        .arg("99")
        .arg("--alias")
        .arg(project)
        // Both dirs: the CUDA runtime libs live in lib/, but the build's own
        // libggml*.so land in bin/ — the proven-working invocation from the
        // node bring-up used exactly this pair.
        .env("LD_LIBRARY_PATH", "/opt/llama/bin:/opt/llama/lib");
    if !member_addrs.is_empty() {
        cmd.arg("--rpc").arg(member_addrs.join(","));
    }
    let log_dir = std::path::PathBuf::from("/var/lib/hive/inference");
    let _ = tokio::fs::create_dir_all(&log_dir).await;
    let log = std::fs::File::create(log_dir.join(format!("{project}.log"))).ok();
    if let Some(l) = log {
        let l2 = l.try_clone().ok();
        cmd.stdout(std::process::Stdio::from(l));
        if let Some(l2) = l2 {
            cmd.stderr(std::process::Stdio::from(l2));
        }
    }
    match cmd.spawn() {
        Ok(child) => {
            tracing::info!(project, port, members = ?member_addrs, model = %spec.model, "inference: launched llama-server");
            set_status(cloud, "starting".into(), &members);
            if let Some((c, _)) = cloud.inference.servers.lock().get_mut(project) {
                *c = Some(child);
            }
        }
        Err(e) => {
            set_status(cloud, format!("failed: spawn {e}"), &members);
        }
    }
}

/// The reconcile loop. Every node ticks; each node acts only on the slice it
/// owns (coordinator role for spawn/kill, leader role for env injection).
pub fn spawn_reconcile(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(
        std::env::var("HIVE_INFERENCE_RECONCILE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30),
    );
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            crate::supervise::beat("inference-reconcile");
            let desired: Vec<(String, crate::project_settings::InferenceSpec)> = cloud
                .projects
                .snapshot()
                .into_iter()
                .filter_map(|(p, s)| s.inference.map(|i| (p, i)))
                .collect();

            // Coordinator slice: converge local children to the desired set.
            let mut mine: Vec<(String, crate::project_settings::InferenceSpec)> = Vec::new();
            for (p, spec) in &desired {
                if coordinator_for(&cloud, p).map(|n| n.name) == Some(cloud.node_name.clone()) {
                    mine.push((p.clone(), spec.clone()));
                }
            }
            {
                // Kill servers no longer desired here (spec removed or
                // coordinator moved elsewhere on roster change).
                let keep: std::collections::HashSet<&String> = mine.iter().map(|(p, _)| p).collect();
                let mut servers = cloud.inference.servers.lock();
                let stale: Vec<String> =
                    servers.keys().filter(|k| !keep.contains(k)).cloned().collect();
                for p in stale {
                    if let Some((Some(mut child), _)) = servers.remove(&p) {
                        let _ = child.start_kill();
                        tracing::info!(project = %p, "inference: stopped endpoint (no longer assigned here)");
                    }
                }
            }
            for (p, spec) in &mine {
                reconcile_local(&cloud, p, spec).await;
            }

            // Leader slice: env injection for every inference project.
            if cloud.is_control_plane_leader() {
                for (p, _spec) in &desired {
                    let Some(coord) = coordinator_for(&cloud, p) else { continue };
                    let Some(ip) = coord.public_ip.clone() else { continue };
                    let url = format!("http://{ip}:{}/v1", port_for(p));
                    let current = cloud.projects.env_map(p).get("HIVE_INFERENCE_URL").cloned();
                    if current.as_deref() != Some(url.as_str()) {
                        cloud.projects.put_env(p, crate::project_settings::EnvVar {
                            key: "HIVE_INFERENCE_URL".into(),
                            value: url.clone(),
                            target: "all".into(),
                            sensitive: false,
                            updated_ms: hive_core::now_ms(),
                        });
                        tracing::info!(project = %p, %url, "inference: injected HIVE_INFERENCE_URL");
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}
