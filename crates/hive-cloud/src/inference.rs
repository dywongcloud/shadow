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
//!    cover the need — ordered by real coordinator→member RTT, not free VRAM
//!    (same "region" spans link classes: the San Jose CVM↔GPU pairs are
//!    ~62-65ms apart while the GPU nodes are <2ms from each other, and one
//!    high-RTT member link turns an 11GB tensor upload into hours at TCP-cwnd
//!    speed). Deliberately NO silent CPU fallback: a model that cannot
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
/// total pool VRAM (stable, capacity-anchored). Within the region,
/// RENDEZVOUS (highest-random-weight) hashing, not modulo: a node dropping
/// out of the roster moves ONLY the endpoints that hashed onto that node,
/// and — critically — when the roster recovers (a health flap, a rolling
/// restart) every endpoint's winner is the SAME node as before, so routine
/// fleet rolls stop reassigning (and therefore killing) mid-load inference
/// servers. Live-witnessed with modulo: a rolling GPU-group restart shifted
/// every project's slot and the reconcile killed a 25-minutes-into-load 14B
/// endpoint mid tensor-upload.
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
    members.into_iter().max_by_key(|n| fnv(&format!("{project}|{}", n.name)))
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
    /// 0 while this node is the elected coordinator; else the moment the
    /// election first pointed elsewhere — drives the reassignment grace
    /// window (see the teardown split in `spawn_reconcile`).
    #[serde(skip_serializing_if = "u64_is_zero", default)]
    pub unassigned_since_ms: u64,
    /// The moment the CURRENT child entered its loading phase (spawn time);
    /// cleared to 0 the first time /health answers OK. Drives the load-timeout
    /// watchdog — 0 means "live or not loading", which is exactly the state
    /// the watchdog must never touch.
    #[serde(skip_serializing_if = "u64_is_zero", default)]
    pub load_started_ms: u64,
}

fn u64_is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Default)]
pub struct InferenceRuntime {
    /// project → (child, status). Children are this process's own — killed on
    /// process exit, respawned by the reconcile tick after a restart.
    servers: parking_lot::Mutex<HashMap<String, (Option<tokio::process::Child>, EndpointStatus)>>,
    /// member ip → (rtt_ms, measured_at_ms): TCP-connect-time probes of member
    /// rpc ports, in-process with a TTL — the fallback RTT signal for peers the
    /// registry hasn't measured yet (see `member_rtts`). Only ever populated on
    /// an elected coordinator, because only `reconcile_local` reads it.
    rtt_probe_cache: parking_lot::Mutex<HashMap<String, (u64, u64)>>,
}

impl InferenceRuntime {
    pub fn statuses(&self) -> Vec<EndpointStatus> {
        let mut v: Vec<_> = self.servers.lock().values().map(|(_, s)| s.clone()).collect();
        v.sort_by(|a, b| a.project.cmp(&b.project));
        v
    }
}

/// Transient-scope unit name for a project's inference server. Stable, so a
/// teardown/restart can stop the right scope by name (see `reconcile_local`).
fn scope_name(project: &str) -> String {
    let safe: String =
        project.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
    format!("hive-inference-{safe}")
}

/// The slice every inference scope runs under, so the whole class of inference
/// workloads is accountable (and capped) as one unit, separately from
/// `hive-node.service`.
fn inference_slice() -> String {
    std::env::var("HIVE_INFERENCE_SLICE").unwrap_or_else(|_| "hive-inference.slice".into())
}

/// Per-endpoint memory ceiling for the scope. Generous by default (a large
/// quantized model is legitimately many GB resident) but bounded, so one
/// endpoint cannot consume the host.
fn scope_memory_max() -> String {
    std::env::var("HIVE_INFERENCE_MEMORY_MAX").unwrap_or_else(|_| "48G".into())
}

/// Whether `systemd-run` is actually usable here — a node without systemd (or
/// a container) falls back to a plain child rather than failing to launch.
async fn which_systemd_run() -> bool {
    tokio::process::Command::new("systemd-run")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
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

/// A member link slower than this is a last resort: llama.cpp's tensor upload
/// runs one TCP stream per member, so throughput is cwnd-limited by RTT — the
/// live incident loaded 11GB at ~1.4MB/s over a 65ms "same-region" link.
fn max_member_rtt_ms() -> u64 {
    std::env::var("HIVE_INFERENCE_MAX_MEMBER_RTT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(20)
}

fn load_timeout_secs() -> u64 {
    std::env::var("HIVE_INFERENCE_LOAD_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(3600)
}

/// The parked status a load-timeout abort leaves behind. Matched EXACTLY by
/// the relaunch guard in `reconcile_local`, so an endpoint that timed out
/// loading is never respawned into another hours-long doomed transfer.
const LOAD_TIMEOUT_STATUS: &str =
    "failed: model load exceeded HIVE_INFERENCE_LOAD_TIMEOUT_SECS (rtt-limited member link?)";

const RTT_PROBE_CACHE_TTL_MS: u64 = 300_000;
/// Sentinel for "probe failed / no address": sorts last and always reads as
/// over-threshold, so such a member is only ever a last resort.
const RTT_UNKNOWN: u64 = u64::MAX;

/// Coordinator→member RTT for every same-region pool candidate, keyed by node
/// name. Primary signal: the registry's `latency_ms` — each node's OWN gossip
/// probes measure it, and since `reconcile_local` runs only on the elected
/// coordinator, the local registry vantage IS the coordinator→candidate RTT.
/// `latency_ms == 0` on a non-self peer means "never probed by us", so that
/// hole (and only that hole) falls back to a TCP connect-time probe of the
/// member's rpc port, cached in-process with a TTL.
async fn member_rtts(
    cloud: &Arc<CloudState>,
    coordinator: &hive_edge::NodeInfo,
    pools: &[crate::gpu_pool::GpuPoolRegion],
) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let Some(rp) = pools.iter().find(|p| p.region == coordinator.region) else {
        return out;
    };
    let registry = cloud.registry.nodes();
    for cand in rp.nodes.iter().filter(|n| n.name != coordinator.name) {
        let reg = registry.iter().find(|n| n.name == cand.name);
        if let Some(ms) = reg.map(|n| n.latency_ms).filter(|ms| *ms > 0) {
            out.insert(cand.name.clone(), ms);
            continue;
        }
        let Some(ip) = reg.and_then(|n| n.public_ip.clone()) else {
            out.insert(cand.name.clone(), RTT_UNKNOWN);
            continue;
        };
        let now = hive_core::now_ms();
        let cached = cloud
            .inference
            .rtt_probe_cache
            .lock()
            .get(&ip)
            .filter(|(_, at)| now.saturating_sub(*at) < RTT_PROBE_CACHE_TTL_MS)
            .map(|(ms, _)| *ms);
        let ms = match cached {
            Some(ms) => ms,
            None => {
                let t0 = std::time::Instant::now();
                let ms = match tokio::time::timeout(
                    Duration::from_secs(2),
                    tokio::net::TcpStream::connect((ip.as_str(), 50052u16)),
                )
                .await
                {
                    // .max(1): a sub-ms LAN connect must not read as the
                    // "unmeasured" 0 the registry path treats as a hole.
                    Ok(Ok(_)) => (t0.elapsed().as_millis() as u64).max(1),
                    _ => RTT_UNKNOWN,
                };
                cloud.inference.rtt_probe_cache.lock().insert(ip, (ms, now));
                ms
            }
        };
        out.insert(cand.name.clone(), ms);
    }
    out
}

/// Placement (pool-aware): fits the coordinator alone → no members. Else, when
/// pooling is allowed, add same-region members until covered — free VRAM is
/// the ADMISSION bar (a member only counts for what it can hold), but real
/// coordinator→member RTT decides the ORDER, ascending: "same region" spans
/// link classes (see module docs), and preferring big-VRAM over near-RTT is
/// how a 14B load streamed for hours at cwnd-limited speed. A member over
/// `max_rtt_ms` is used ONLY when the pool cannot cover the model without it,
/// and loudly. Else honest failure.
fn plan_members(
    need_mb: u64,
    coordinator: &hive_edge::NodeInfo,
    pool_allowed: bool,
    pools: &[crate::gpu_pool::GpuPoolRegion],
    rtts: &HashMap<String, u64>,
    max_rtt_ms: u64,
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
    let rtt_of = |name: &str| rtts.get(name).copied().unwrap_or(RTT_UNKNOWN);
    let mut others: Vec<_> = rp.nodes.iter().filter(|n| n.name != coordinator.name).collect();
    // Free VRAM only breaks RTT ties; the name tiebreak keeps the plan
    // deterministic across ticks when both figures match.
    others.sort_by(|a, b| {
        rtt_of(&a.name)
            .cmp(&rtt_of(&b.name))
            .then(b.vram_free_mb.cmp(&a.vram_free_mb))
            .then(a.name.cmp(&b.name))
    });
    // Stable partition preserves the RTT order within each tier.
    let (near, far): (Vec<_>, Vec<_>) = others.into_iter().partition(|n| rtt_of(&n.name) <= max_rtt_ms);
    let mut covered = coord_free;
    let mut members = Vec::new();
    for n in near.iter().chain(far.iter()) {
        if covered >= need_mb {
            break;
        }
        let rtt = rtt_of(&n.name);
        if rtt == RTT_UNKNOWN {
            tracing::warn!(
                member = %n.name,
                "inference: pooling over a member with UNMEASURABLE RTT (no registry latency, rpc-port probe failed) — required for VRAM coverage; model load may be slow or fail"
            );
        } else if rtt > max_rtt_ms {
            tracing::warn!(
                member = %n.name,
                rtt_ms = rtt,
                max_rtt_ms,
                "inference: pooling over a HIGH-RTT member link (exceeds HIVE_INFERENCE_MAX_MEMBER_RTT_MS) — the pool cannot cover the model without it; expect a slow, TCP-cwnd-limited model load"
            );
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
                        st.load_started_ms = 0;
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
        if !healthy {
            // Transfer-progress watchdog: /health answers 503 for the entire
            // load, so a load that has been "running" past the deadline is not
            // progress, it is an rtt-limited (or wedged) tensor upload that
            // would otherwise hold the endpoint in "starting" forever —
            // live-witnessed as an 11GB upload at ~1.4MB/s over a 65ms member
            // link. Abort to failed; the LIVE-child rule is untouched because
            // load_started_ms is 0 once /health has ever answered OK.
            let deadline_ms = load_timeout_secs().saturating_mul(1000);
            let overdue = {
                let servers = cloud.inference.servers.lock();
                servers.get(project).is_some_and(|(_, st)| {
                    st.load_started_ms > 0
                        && hive_core::now_ms().saturating_sub(st.load_started_ms) > deadline_ms
                })
            };
            if overdue {
                {
                    let mut servers = cloud.inference.servers.lock();
                    if let Some((child, st)) = servers.get_mut(project) {
                        if let Some(mut c) = child.take() {
                            let _ = c.start_kill();
                        }
                        st.status = LOAD_TIMEOUT_STATUS.into();
                        st.load_started_ms = 0;
                        st.updated_ms = hive_core::now_ms();
                    }
                }
                let _ = tokio::process::Command::new("systemctl")
                    .args(["stop", &format!("{}.scope", scope_name(project))])
                    .output()
                    .await;
                tracing::warn!(
                    project,
                    port,
                    timeout_secs = load_timeout_secs(),
                    "inference: aborted model load — exceeded HIVE_INFERENCE_LOAD_TIMEOUT_SECS (rtt-limited member link?); endpoint parked failed, no relaunch until the spec changes or the node restarts"
                );
                return;
            }
        }
        let mut servers = cloud.inference.servers.lock();
        if let Some((_, st)) = servers.get_mut(project) {
            if healthy {
                // LIVE from here on: the watchdog must never judge a serving
                // child by its long-ago load start (health flaps re-show
                // "starting" but must never re-arm the abort).
                st.load_started_ms = 0;
            }
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

    // A load-timeout abort PARKS: relaunching the identical spec would start
    // another equally-doomed hours-long transfer on the next tick, turning the
    // watchdog into a retry loop. The park lifts when the operator changes the
    // model (spec differs) or the node restarts (in-process state clears).
    {
        let servers = cloud.inference.servers.lock();
        if let Some((child, st)) = servers.get(project) {
            if child.is_none() && st.status == LOAD_TIMEOUT_STATUS && st.model == spec.model {
                return;
            }
        }
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
                unassigned_since_ms: 0,
                load_started_ms: 0,
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
    let mut pools = crate::gpu_pool::snapshot(cloud).await;
    // Admission requires a dialable rpc address: plan_members counts a
    // member's VRAM toward coverage, but the --rpc list below can only name
    // members with a public IP — an address-less candidate admitted for its
    // VRAM would let the plan claim coverage the launched server can never
    // reach, failing the load with no honest placement error. Drop them
    // BEFORE planning so a genuine shortfall parks the endpoint as failed.
    let addressable: std::collections::HashSet<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.public_ip.is_some())
        .map(|n| n.name)
        .collect();
    for p in &mut pools {
        p.nodes.retain(|n| n.name == me.name || addressable.contains(&n.name));
    }
    let rtts = member_rtts(cloud, &me, &pools).await;
    let members = match plan_members(need, &me, spec.pool, &pools, &rtts, max_member_rtt_ms()) {
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
    // reassign semantics stay real. Both shapes are swept: a bare child and a
    // transient scope (the scope is stopped by name, which kills its members).
    let _ = tokio::process::Command::new("pkill")
        .args(["-f", &format!("llama-server.*--alias {project}")])
        .output()
        .await;
    let _ = tokio::process::Command::new("systemctl")
        .args(["stop", &format!("{}.scope", scope_name(project))])
        .output()
        .await;

    // Run the server in its OWN cgroup, never as a plain child of hive-cloud.
    // A plain child lands in hive-node.service's memory cgroup, so a model's
    // resident footprint (15.7GB for a 14B Q8) competes with the platform
    // process against the SERVICE's MemoryMax — live-witnessed on
    // fc-sanjose-cvm-2 as `Memory cgroup out of memory: Killed process ...
    // (hive-cloud) ... oom_memcg=/system.slice/hive-node.service`: the kernel
    // killed the NODE's platform process, not the inference server. A transient
    // scope under a dedicated slice accounts inference memory separately, so an
    // over-large model fails only its own endpoint (honest `failed` status)
    // and can never take hive-node down with it.
    let use_scope = std::env::var("HIVE_INFERENCE_SCOPE").map(|v| v != "0").unwrap_or(true)
        && which_systemd_run().await;
    let mut cmd = if use_scope {
        let mut c = tokio::process::Command::new("systemd-run");
        c.arg("--scope")
            .arg("--collect")
            .arg(format!("--unit={}", scope_name(project)))
            .arg(format!("--slice={}", inference_slice()))
            .arg(format!("--property=MemoryMax={}", scope_memory_max()))
            .arg("--property=MemorySwapMax=0")
            .arg("--quiet")
            .arg(llama_bin());
        c
    } else {
        tokio::process::Command::new(llama_bin())
    };
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
            if let Some((c, st)) = cloud.inference.servers.lock().get_mut(project) {
                *c = Some(child);
                st.load_started_ms = hive_core::now_ms();
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
                // Teardown split, deliberately asymmetric:
                //  * spec REMOVED (project gone from `desired`) → kill now —
                //    a deleted project's endpoint must not linger;
                //  * assignment MOVED (still desired, elected elsewhere) →
                //    grace window before killing. Rendezvous hashing already
                //    makes reassignment rare, but a coordinator that dropped
                //    out of the roster long enough to lose its endpoints
                //    shouldn't slaughter a mid-load server the moment it
                //    rejoins and briefly computes a partial roster.
                let assigned: std::collections::HashSet<&String> =
                    mine.iter().map(|(p, _)| p).collect();
                let specd: std::collections::HashSet<&String> =
                    desired.iter().map(|(p, _)| p).collect();
                let grace_ms = std::env::var("HIVE_INFERENCE_REASSIGN_GRACE_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(600)
                    * 1000;
                let now = hive_core::now_ms();
                let mut servers = cloud.inference.servers.lock();
                let mut kill: Vec<String> = Vec::new();
                for (p, (_, st)) in servers.iter_mut() {
                    if assigned.contains(p) {
                        st.unassigned_since_ms = 0;
                        continue;
                    }
                    if !specd.contains(p) {
                        kill.push(p.clone());
                        continue;
                    }
                    if st.unassigned_since_ms == 0 {
                        st.unassigned_since_ms = now;
                        tracing::info!(project = %p, "inference: assignment moved elsewhere — holding through grace window");
                    } else if now.saturating_sub(st.unassigned_since_ms) > grace_ms {
                        kill.push(p.clone());
                    }
                }
                for p in kill {
                    if let Some((child, _)) = servers.remove(&p) {
                        if let Some(mut c) = child {
                            let _ = c.start_kill();
                        }
                        // A transient scope OUTLIVES the process that spawned
                        // it (that is the point — inference memory is accounted
                        // separately from hive-node.service), so killing the
                        // spawn handle is not enough: stop the unit by name or
                        // the server keeps serving a project that moved away.
                        let unit = format!("{}.scope", scope_name(&p));
                        tokio::spawn(async move {
                            let _ = tokio::process::Command::new("systemctl").args(["stop", &unit]).output().await;
                        });
                        tracing::info!(project = %p, "inference: stopped endpoint (no longer desired/assigned here)");
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
