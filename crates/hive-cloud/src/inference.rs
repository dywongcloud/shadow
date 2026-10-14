//! Managed serverless GPU inference: the platform half of the llama.cpp pooling stack (the node
//! half, `llama-rpc.service` on every GPU node, is provisioned infrastructure: CUDA `rpc-server` on
//! port 50052, peers-only via hive-lockdown).
//!
//! A project opts in with a `fluid.json` top-level block (`{"inference": {"model":
//! "<gguf-url-or-hf-path>", "pool": true}}` -> [`crate::project_settings::InferenceSpec`], synced
//! into the replicated projects store by the deploy path). This module then:
//!
//! * elects ONE coordinator GPU node per project, deterministically (a rendezvous hash of the
//!   project over the sorted GPU-node roster), so every node computes the same assignment from the
//!   same gossiped registry with no new coordination protocol;
//! * on that coordinator, downloads and caches the GGUF and runs a real `llama-server`
//!   (OpenAI-compatible HTTP API) bound to a deterministic per-project port in `50100..=50999`, a
//!   range hive-lockdown drops for non-peers, so the endpoint is fleet-internal only;
//! * decides single-node versus pooled by the placement rule: a model that fits the coordinator's
//!   free VRAM runs alone (no pooling overhead); only when it does NOT fit does it engage
//!   llama.cpp's RPC layer distribution (`--rpc member:50052,...`) across enough same-region pool
//!   members to cover the need, ordered by real coordinator-to-member RTT and not by free VRAM
//!   ('same region' spans link classes, and one high-RTT member link turns a multi-GB tensor upload
//!   into hours at TCP-cwnd speed). Deliberately NO silent CPU fallback: a model that cannot fit
//!   even the pool's aggregate free VRAM parks the endpoint in a `failed` status with an honest
//!   error;
//! * on the control-plane leader, injects `HIVE_INFERENCE_URL` into the project env (the same
//!   precedent as DB env auto-injection), so app code just reads one env var and speaks the OpenAI
//!   protocol to it.
//!
//! The reconcile loop follows `spawn_db_reconcile`'s discipline: desired state is derived fresh
//! every tick from replicated stores, and the node hosting a piece of it converges its own local
//! reality (spawn/kill children), self-healing across restarts with no imperative cross-node RPC.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::state::CloudState;

/// VRAM safety margin: quantized weights get mmapped at roughly file size, with KV cache, CUDA
/// context and fragmentation on top; 1.2x + 800 MB tracks the measured footprint of a small model
/// without starving co-tenants.
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

/// Deterministic per-project endpoint port. Collisions between two projects hashing to the same
/// slot are tolerated by the coordinator check (the second server fails to bind and surfaces as a
/// failed status): with 900 slots this is rare enough to keep the mapping stateless.
pub fn port_for(project: &str) -> u16 {
    50100 + (fnv(project) % 900) as u16
}

/// The GPU roster this module schedules over: healthy, GPU-carrying nodes,
/// sorted by name for cross-node determinism.
fn gpu_roster(cloud: &Arc<CloudState>) -> Vec<hive_edge::NodeInfo> {
    let mut v: Vec<_> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.healthy && n.gpu_count > 0)
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Deterministic coordinator election: the same inputs (the gossiped registry) give the same answer
/// on every node. Region choice: the region with the largest total pool VRAM (stable,
/// capacity-anchored). Within the region, RENDEZVOUS (highest-random-weight) hashing, not modulo: a
/// node dropping out of the roster moves ONLY the endpoints that hashed onto that node, and when
/// the roster recovers (a health flap, a rolling restart) every endpoint's winner is the SAME node
/// as before, so routine fleet rolls never reassign (and therefore kill) mid-load inference
/// servers, which modulo hashing did.
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
    members
        .into_iter()
        .max_by_key(|n| fnv(&format!("{project}|{}", n.name)))
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
        return Some(format!(
            "https://huggingface.co/{org}/{repo}/resolve/main/{file}"
        ));
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
    /// 0 while this node is the elected coordinator; else the moment the election first pointed
    /// elsewhere, which drives the reassignment grace window (see the teardown split in
    /// `spawn_reconcile`).
    #[serde(skip_serializing_if = "u64_is_zero", default)]
    pub unassigned_since_ms: u64,
    /// The moment the CURRENT child entered its loading phase (spawn time); cleared to 0 the first
    /// time /health answers OK. Drives the load-timeout watchdog: 0 means 'live or not loading',
    /// which is exactly the state the watchdog must never touch.
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
    /// member ip -> (rtt_ms, measured_at_ms): TCP-connect-time probes of member rpc ports,
    /// in-process with a TTL, the fallback RTT signal for peers the registry has not measured yet
    /// (see `member_rtts`). Only ever populated on an elected coordinator, because only
    /// `reconcile_local` reads it.
    rtt_probe_cache: parking_lot::Mutex<HashMap<String, (u64, u64)>>,
    /// model URL -> its download's state (see [`model_state`]); shared by every project naming it.
    fetches: parking_lot::Mutex<HashMap<String, Fetch>>,
}

impl InferenceRuntime {
    pub fn statuses(&self) -> Vec<EndpointStatus> {
        let mut v: Vec<_> = self
            .servers
            .lock()
            .values()
            .map(|(_, s)| s.clone())
            .collect();
        v.sort_by(|a, b| a.project.cmp(&b.project));
        v
    }
}

/// Transient-scope unit name for a project's inference server. Stable, so a
/// teardown/restart can stop the right scope by name (see `reconcile_local`).
fn scope_name(project: &str) -> String {
    let safe: String = project
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("hive-inference-{safe}")
}

/// The slice every inference scope runs under, so the whole class of inference workloads is
/// accountable (and capped) as one unit, separately from `hive-node.service`.
fn inference_slice() -> String {
    std::env::var("HIVE_INFERENCE_SLICE").unwrap_or_else(|_| "hive-inference.slice".into())
}

/// Per-endpoint memory ceiling for the scope. Generous by default (a large quantized model is
/// legitimately many GB resident) but bounded, so one endpoint cannot consume the host.
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

/// Domain separator for the model-blob content identity (AGENTS.md: a NEW content identity mints
/// BLAKE3 via a domain-separated hash of the bytes).
const MODEL_BLOB_DOMAIN: &[u8] = b"hive-inference-model-blob-v1\0";

/// Largest model this node will download in one `ensure_model` call. `fluid.json`'s
/// `inference.model` is tenant input reaching a real network fetch with no bound otherwise;
/// `HIVE_INFERENCE_MAX_MODEL_BYTES` overrides (default 200 GiB, generous for the largest quantized
/// models this stack targets).
fn max_model_bytes() -> u64 {
    std::env::var("HIVE_INFERENCE_MAX_MODEL_BYTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(200 * 1024 * 1024 * 1024)
}

/// A small pointer from a model ref's cheap lookup key to the content-addressed blob it currently
/// resolves to: `fnv(url)` is never collision-resistant (a deliberate collision is brute-forceable
/// in a 64-bit space), so the pointer's own stored `url` is compared verbatim before trusting it —
/// a colliding ref just misses the cache and re-downloads under its own content hash, it can never
/// make one tenant's cached bytes serve under a different tenant's model ref.
#[derive(serde::Serialize, serde::Deserialize)]
struct ModelRef {
    url: String,
    file: String,
}

/// Longest ONE model download may take end to end (`HIVE_INFERENCE_MODEL_TOTAL_SECS`, default 2 h).
fn model_total_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("HIVE_INFERENCE_MODEL_TOTAL_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(2 * 3600),
    )
}

/// Longest the transfer may go without receiving a single byte (`HIVE_INFERENCE_MODEL_STALL_SECS`,
/// default 30 s); also bounds the connect-and-headers phase. A trickle that delivers a byte now and
/// then never trips it: [`model_min_bytes_per_sec`] is what ends that.
fn model_stall_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("HIVE_INFERENCE_MODEL_STALL_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(30),
    )
}

/// The slowest average a download may sustain over [`model_rate_window`]
/// (`HIVE_INFERENCE_MODEL_MIN_BYTES_PER_SEC`, default 64 KiB/s; 0 disables the floor). A slow drip
/// (a byte every few seconds) delivers data often enough that the stall timeout never fires, and
/// without a rate floor it would hold one of the node's few download slots for the whole total
/// timeout (2 h). Every real model transfer runs orders of magnitude above this.
fn model_min_bytes_per_sec() -> u64 {
    std::env::var("HIVE_INFERENCE_MODEL_MIN_BYTES_PER_SEC")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(64 * 1024)
}

/// The window the rate floor is measured over (`HIVE_INFERENCE_MODEL_RATE_WINDOW_SECS`, default 30 s,
/// never under 1 s): a download is ended when a whole window passes under the floor, so a trickle
/// holds its slot for one window, not for the total timeout.
fn model_rate_window() -> Duration {
    Duration::from_secs(
        std::env::var("HIVE_INFERENCE_MODEL_RATE_WINDOW_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(30)
            .max(1),
    )
}

/// How long a FAILED download of a model URL is remembered before the next reconcile tick retries
/// it (`HIVE_INFERENCE_MODEL_RETRY_SECS`, default 300 s): a dead or hostile URL must not be hammered
/// every tick.
fn model_retry_cooldown_ms() -> u64 {
    std::env::var("HIVE_INFERENCE_MODEL_RETRY_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(300)
        * 1000
}

/// Free space a download must always leave on the models volume
/// (`HIVE_INFERENCE_MODEL_DISK_RESERVE_BYTES`, default 2 GiB): a model that would fill the disk
/// fails its own endpoint, it never takes the node's other writers (state, logs, builds) with it.
fn disk_reserve_bytes() -> u64 {
    std::env::var("HIVE_INFERENCE_MODEL_DISK_RESERVE_BYTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024)
}

/// Free bytes on the volume holding `dir` (the mount whose path is the longest prefix of it), or
/// `None` when no mount covers it (a probe that cannot answer must not block a download).
fn free_bytes_on(dir: &std::path::Path) -> Option<u64> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// Most model downloads this node runs at once (`HIVE_INFERENCE_MODEL_DOWNLOADS_MAX`, default 2,
/// never under 1): every project's `fluid.json` can name its own multi-GB model, and each running
/// download holds a connection, a partial file and a claim on the disk.
fn max_concurrent_downloads() -> usize {
    std::env::var("HIVE_INFERENCE_MODEL_DOWNLOADS_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(2)
        .max(1)
}

/// One permit per running download, node-wide ([`max_concurrent_downloads`]). A URL that finds none
/// is simply tried again on the next reconcile tick ([`model_state`]); nothing queues.
static DOWNLOAD_SLOTS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(max_concurrent_downloads())));

/// Most downloads one TENANT may have running at once (`HIVE_INFERENCE_MODEL_DOWNLOADS_PER_TENANT`,
/// default 1, never under 1): the node's few slots are shared, and a tenant whose projects name
/// several slow hosts must not be able to hold every one of them.
fn max_downloads_per_tenant() -> usize {
    std::env::var("HIVE_INFERENCE_MODEL_DOWNLOADS_PER_TENANT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1)
}

/// How many downloads each tenant has running; an untagged project counts under the fail-closed
/// `UNTAGGED_TENANT`, so all of them together share one place.
static TENANT_DOWNLOADS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, usize>>> =
    std::sync::LazyLock::new(Default::default);

/// One running download counted against its tenant ([`max_downloads_per_tenant`]); dropped on every
/// exit of the download task.
struct TenantSlot {
    tenant: String,
}

impl TenantSlot {
    /// Counts a download for `tenant`, or `None` when it already has its share running.
    fn acquire(tenant: &str) -> Option<TenantSlot> {
        let mut running = TENANT_DOWNLOADS.lock().unwrap_or_else(|e| e.into_inner());
        let n = running.entry(tenant.to_string()).or_insert(0);
        if *n >= max_downloads_per_tenant() {
            return None;
        }
        *n += 1;
        Some(TenantSlot {
            tenant: tenant.to_string(),
        })
    }
}

impl Drop for TenantSlot {
    fn drop(&mut self) {
        let mut running = TENANT_DOWNLOADS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = running.get_mut(&self.tenant) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                running.remove(&self.tenant);
            }
        }
    }
}

/// The bytes every download in flight still EXPECTS to write (its declared length less what has
/// landed): the disk reserve is shared by all of them, so each admission and each periodic re-check
/// asks whether the volume can hold ALL of it and still keep the reserve, never just its own file.
static DISK_CLAIMED: std::sync::Mutex<u64> = std::sync::Mutex::new(0);

/// One download's claim on the disk: dropped on every exit, so the ledger never leaks a claim.
struct DiskClaim {
    remaining: u64,
}

impl DiskClaim {
    /// Admits a download of `declared` bytes (0 when the server declared no length) when the volume
    /// at `dir` keeps `reserve` free once everything already in flight and this download have
    /// written what they still expect. The free space is read INSIDE the ledger lock, so it is
    /// consistent with the claims it is compared against. A volume that cannot be probed admits.
    fn admit(dir: &std::path::Path, declared: u64, reserve: u64) -> Option<DiskClaim> {
        let mut claimed = DISK_CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
        if free_bytes_on(dir)
            .is_some_and(|free| free < claimed.saturating_add(declared).saturating_add(reserve))
        {
            return None;
        }
        *claimed = claimed.saturating_add(declared);
        Some(DiskClaim {
            remaining: declared,
        })
    }

    /// `n` bytes reached the disk: they are no longer expected, they are written (the free space
    /// already reflects them), so they leave the claim.
    fn landed(&mut self, n: u64) {
        let take = n.min(self.remaining);
        self.remaining -= take;
        let mut claimed = DISK_CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
        *claimed = claimed.saturating_sub(take);
    }

    /// Whether the volume still holds everything in flight (this download's remainder included)
    /// and keeps `reserve`.
    fn still_fits(&self, dir: &std::path::Path, reserve: u64) -> bool {
        let claimed = DISK_CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
        !free_bytes_on(dir).is_some_and(|free| free < claimed.saturating_add(reserve))
    }
}

impl Drop for DiskClaim {
    fn drop(&mut self) {
        let mut claimed = DISK_CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
        *claimed = claimed.saturating_sub(self.remaining);
    }
}

/// A partial download that deletes itself on EVERY exit — an error, a timeout, a cap, or the future
/// being dropped mid-transfer (the load watchdog and a shutdown both cancel it). Only the rename
/// that publishes the finished blob disarms it.
struct PartFile(Option<std::path::PathBuf>);

impl Drop for PartFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Deletes what nothing references any more, on every download: partial downloads untouched for 15
/// minutes (a live one is rewritten continually), the blobs of the retired `fnv(url)`-named cache
/// (`hive-<16 hex>.gguf`, never read again once the cache is content-addressed) and content blobs
/// (`<64 hex>.gguf`) that no `*.ref.json` pointer names (the previous blob of a ref whose content
/// changed, a blob whose pointer write failed) — all only past an hour/15 min of age so a download
/// that has just published is never raced.
async fn sweep_orphans(dir: &std::path::Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let mut files: Vec<(String, std::time::Duration)> = Vec::new();
    let mut referenced = std::collections::HashSet::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let age = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .unwrap_or_default();
        if name.ends_with(".ref.json") {
            if let Ok(raw) = tokio::fs::read(entry.path()).await {
                if let Ok(pointer) = serde_json::from_slice::<ModelRef>(&raw) {
                    referenced.insert(pointer.file);
                }
            }
        }
        files.push((name, age));
    }
    for (name, age) in files {
        let stem = name.strip_suffix(".gguf");
        let orphan = if name.starts_with("hive-") && name.ends_with(".part") {
            age > Duration::from_secs(15 * 60)
        } else if stem
            .and_then(|s| s.strip_prefix("hive-"))
            .is_some_and(|s| is_hex(s, 16))
        {
            age > Duration::from_secs(3600)
        } else if stem.is_some_and(|s| is_hex(s, 64)) {
            !referenced.contains(&name) && age > Duration::from_secs(3600)
        } else {
            false
        };
        if orphan && tokio::fs::remove_file(dir.join(&name)).await.is_ok() {
            tracing::info!(file = %name, "inference: removed an orphaned model cache file");
        }
    }
}

/// The cached blob for `model` when a pointer for exactly this URL names a non-empty file (no
/// network); `Err` only for a ref that resolves to no URL.
async fn cached_model(model: &str) -> Result<Option<(std::path::PathBuf, u64)>, String> {
    let url = model_url(model).ok_or_else(|| format!("unresolvable model ref '{model}'"))?;
    let dir = models_dir();
    let ref_path = dir.join(format!("hive-{:016x}.ref.json", fnv(&url)));
    if let Ok(raw) = tokio::fs::read(&ref_path).await {
        if let Ok(r) = serde_json::from_slice::<ModelRef>(&raw) {
            if r.url == url {
                let path = dir.join(&r.file);
                if let Ok(md) = tokio::fs::metadata(&path).await {
                    if md.len() > 0 {
                        return Ok(Some((path, md.len())));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Download-and-cache the model file. The URL is tenant input (`fluid.json` `inference.model`) that
/// reaches a real fetch, so it goes through the same `hive_outbound` guard as every other
/// tenant-URL fetcher (public addresses only, redirects re-validated per hop — HuggingFace's CDN
/// redirects legitimately) and a size cap; a failure is one fixed, generic reason, never the
/// upstream status or body (no oracle for an internal address). The transfer is bounded in TIME
/// (a stall timeout between chunks and a total timeout) and in DISK (the models volume must keep a
/// reserve SHARED by every download in flight — a [`DiskClaim`] — checked up front against the
/// declared length and again as bytes land), and the partial file is removed on every outcome. The stored blob is named by the BLAKE3 content hash of its OWN
/// bytes (a NEW content identity, hashed incrementally as it downloads so a multi-GB model is never
/// held in memory at once) rather than a hash of the URL: the old `fnv(url)` filename let two
/// different URLs that happened to collide on 64 bits silently share a cache slot. Returns
/// (path, size_bytes).
async fn ensure_model(model: &str) -> Result<(std::path::PathBuf, u64), String> {
    if let Some(hit) = cached_model(model).await? {
        return Ok(hit);
    }
    let url = model_url(model).ok_or_else(|| format!("unresolvable model ref '{model}'"))?;
    let dir = models_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    sweep_orphans(&dir).await;

    let ref_path = dir.join(format!("hive-{:016x}.ref.json", fnv(&url)));
    let target = hive_outbound::validate_fetch_url(&url)
        .map_err(|_| "model URL refused by the outbound policy".to_string())?;
    tracing::info!(%url, "inference: downloading model");
    let (total, stall) = (model_total_timeout(), model_stall_timeout());
    let started = std::time::Instant::now();
    let mut resp =
        tokio::time::timeout(stall, hive_outbound::following_client().get(target).send())
            .await
            .map_err(|_| "model fetch timed out".to_string())?
            .map_err(|_| "model fetch failed".to_string())?;
    if !resp.status().is_success() {
        return Err("model fetch failed".to_string());
    }
    let max_bytes = max_model_bytes();
    let reserve = disk_reserve_bytes();
    let declared = resp.content_length();
    if declared.is_some_and(|len| len > max_bytes) {
        return Err("model exceeds the maximum download size".to_string());
    }
    // The reserve is shared by every download in flight: this one is admitted only if the volume can
    // hold what the others still expect to write, this file, and the reserve, all at once.
    let Some(mut claim) = DiskClaim::admit(&dir, declared.unwrap_or(0), reserve) else {
        return Err("insufficient disk space for the model".to_string());
    };
    // Unique per download: two projects naming the same URL must never write one partial file.
    let tmp = dir.join(format!(
        "hive-{:016x}-{}.part",
        fnv(&url),
        uuid::Uuid::new_v4().simple()
    ));
    let mut part = PartFile(Some(tmp.clone()));
    let mut f = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| e.to_string())?;
    use tokio::io::AsyncWriteExt;
    let mut hasher = blake3::Hasher::new();
    hasher.update(MODEL_BLOB_DOMAIN);
    let mut written: u64 = 0;
    let mut disk_checked_at: u64 = 0;
    let (min_rate, rate_window) = (model_min_bytes_per_sec(), model_rate_window());
    let mut window_started = std::time::Instant::now();
    let mut window_bytes: u64 = 0;
    loop {
        let left = total.saturating_sub(started.elapsed());
        let next = match tokio::time::timeout(stall.min(left), resp.chunk()).await {
            Ok(r) => r.map_err(|_| "model fetch failed".to_string())?,
            Err(_) if left <= stall => {
                return Err("model download exceeded the time limit".to_string())
            }
            Err(_) => return Err("model download stalled".to_string()),
        };
        let Some(chunk) = next else { break };
        written += chunk.len() as u64;
        if written > max_bytes {
            return Err("model exceeds the maximum download size".to_string());
        }
        window_bytes += chunk.len() as u64;
        if window_started.elapsed() >= rate_window {
            if window_bytes < min_rate.saturating_mul(rate_window.as_secs()) {
                return Err("model download too slow".to_string());
            }
            window_started = std::time::Instant::now();
            window_bytes = 0;
        }
        if written - disk_checked_at >= 256 * 1024 * 1024 {
            disk_checked_at = written;
            if !claim.still_fits(&dir, reserve) {
                return Err("insufficient disk space for the model".to_string());
            }
        }
        hasher.update(&chunk);
        f.write_all(&chunk).await.map_err(|e| e.to_string())?;
        claim.landed(chunk.len() as u64);
    }
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);

    let digest = hasher.finalize().to_hex().to_string();
    let file_name = format!("{digest}.gguf");
    let path = dir.join(&file_name);
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| e.to_string())?;
    part.0 = None;
    let size = tokio::fs::metadata(&path)
        .await
        .map_err(|e| e.to_string())?
        .len();
    let pointer = ModelRef {
        url: url.clone(),
        file: file_name,
    };
    if let Ok(bytes) = serde_json::to_vec(&pointer) {
        if let Err(e) = tokio::fs::write(&ref_path, &bytes).await {
            tracing::warn!(error = %e, ?ref_path, "inference: could not persist model-ref pointer (next call will re-download and re-verify)");
        }
    }
    Ok((path, size))
}

/// Where a model URL's download stands, shared by every project naming that URL.
enum Fetch {
    /// `since_ms` bounds how long a `Running` entry is believed: a task that died without recording
    /// its outcome must not park the URL forever.
    Running {
        since_ms: u64,
    },
    Failed {
        error: String,
        at_ms: u64,
    },
}

/// What the reconcile loop learns about a project's model.
enum ModelState {
    Ready(std::path::PathBuf, u64),
    Downloading,
    Failed(String),
}

/// The model for `model`, WITHOUT ever blocking the reconcile loop on a download: a cached blob is
/// returned at once; otherwise the download runs in its own task (bounded by `ensure_model`'s
/// timeouts) and the loop moves on to every other project, polling again next tick. The loop used
/// to await the transfer inline, so one slow-drip URL stalled every tenant's endpoint on the
/// coordinator. A failed URL is remembered for [`model_retry_cooldown_ms`] before it is retried, a URL
/// two projects share is fetched once, at most [`max_concurrent_downloads`] run at a time and a
/// tenant has at most [`max_downloads_per_tenant`] of them.
async fn model_state(cloud: &Arc<CloudState>, project: &str, model: &str) -> ModelState {
    match cached_model(model).await {
        Ok(Some((path, bytes))) => return ModelState::Ready(path, bytes),
        Err(e) => return ModelState::Failed(e),
        Ok(None) => {}
    }
    let Some(url) = model_url(model) else {
        return ModelState::Failed(format!("unresolvable model ref '{model}'"));
    };
    let slots = {
        let mut fetches = cloud.inference.fetches.lock();
        let now = hive_core::now_ms();
        match fetches.get(&url) {
            Some(Fetch::Running { since_ms })
                if now.saturating_sub(*since_ms)
                    < model_total_timeout().as_millis() as u64 + 300_000 =>
            {
                return ModelState::Downloading;
            }
            Some(Fetch::Failed { error, at_ms })
                if now.saturating_sub(*at_ms) < model_retry_cooldown_ms() =>
            {
                return ModelState::Failed(error.clone());
            }
            _ => {}
        }
        // At most `max_concurrent_downloads` run node-wide; a URL that finds no free slot is not
        // recorded and is simply tried again next tick, so nothing queues and a `Running` entry
        // always names a transfer that really started.
        // One download per tenant at a time, then a node-wide slot: a tenant whose projects name
        // several slow hosts holds one place, never the node's whole share.
        let tenant = crate::admin::record_tenant(&cloud.projects.team_of(project)).to_string();
        let Some(tenant_slot) = TenantSlot::acquire(&tenant) else {
            return ModelState::Downloading;
        };
        let Ok(slot) = DOWNLOAD_SLOTS.clone().try_acquire_owned() else {
            return ModelState::Downloading;
        };
        fetches.insert(url.clone(), Fetch::Running { since_ms: now });
        (slot, tenant_slot)
    };
    let (cloud, model) = (cloud.clone(), model.to_string());
    tokio::spawn(async move {
        let _slots = slots;
        let outcome = ensure_model(&model).await;
        let mut fetches = cloud.inference.fetches.lock();
        match outcome {
            Ok(_) => {
                fetches.remove(&url);
            }
            Err(error) => {
                tracing::warn!(%url, %error, "inference: model download failed");
                fetches.insert(
                    url,
                    Fetch::Failed {
                        error,
                        at_ms: hive_core::now_ms(),
                    },
                );
            }
        }
    });
    ModelState::Downloading
}

/// A member link slower than this (`HIVE_INFERENCE_MAX_MEMBER_RTT_MS`, default 20 ms) is a last
/// resort: llama.cpp's tensor upload runs one TCP stream per member, so throughput is cwnd-limited
/// by RTT, and a multi-GB model over a high-RTT 'same-region' link loads at a crawl.
fn max_member_rtt_ms() -> u64 {
    std::env::var("HIVE_INFERENCE_MAX_MEMBER_RTT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
}

fn load_timeout_secs() -> u64 {
    std::env::var("HIVE_INFERENCE_LOAD_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600)
}

/// The parked status a load-timeout abort leaves behind, matched EXACTLY by the relaunch guard in
/// `reconcile_local`. An abort PARKS because relaunching the identical spec would start another
/// equally doomed hours-long transfer on the next tick, turning the watchdog into a retry loop; the
/// park lifts when the operator changes the model (the spec differs) or the node restarts
/// (in-process state clears).
const LOAD_TIMEOUT_STATUS: &str =
    "failed: model load exceeded HIVE_INFERENCE_LOAD_TIMEOUT_SECS (rtt-limited member link?)";

const RTT_PROBE_CACHE_TTL_MS: u64 = 300_000;
/// Sentinel for "probe failed / no address": sorts last and always reads as
/// over-threshold, so such a member is only ever a last resort.
const RTT_UNKNOWN: u64 = u64::MAX;

/// Coordinator-to-member RTT for every same-region pool candidate, keyed by node name. Primary
/// signal: the registry's `latency_ms`, which each node's OWN gossip probes measure; since
/// `reconcile_local` runs only on the elected coordinator, the local registry vantage IS the
/// coordinator-to-candidate RTT. `latency_ms == 0` or `NodeRegistry::RESTORED_LATENCY_MS` on a
/// non-self peer means 'unmeasured by us' (the latter was restored from indirect gossip), so either
/// hole falls back to a TCP connect-time probe of the member's rpc port, cached in-process with a
/// TTL and floored at 1 ms so a sub-ms LAN connect never reads as the 'unmeasured' 0. `u64::MAX` is
/// withdrawn and likewise never a measurement.
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
        if let Some(ms) = reg.map(|n| n.latency_ms).filter(|ms| {
            *ms > 0
                && *ms != hive_edge::region::NodeRegistry::RESTORED_LATENCY_MS
                && *ms != u64::MAX
        }) {
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

/// Placement (pool-aware): a model that fits the coordinator alone needs no members. Otherwise,
/// when pooling is allowed, same-region members are added until the need is covered: free VRAM is
/// the ADMISSION bar (a member only counts for what it can hold), but real coordinator-to-member
/// RTT decides the ORDER, ascending, because 'same region' spans link classes (see the module doc)
/// and preferring big-VRAM over near-RTT streams a large load at cwnd-limited speed for hours. Free
/// VRAM only breaks RTT ties, with the name as the last tiebreak so the plan is deterministic
/// across ticks. Members over `max_rtt_ms` are partitioned behind the near ones (a stable
/// partition, so RTT order holds within each tier) and used ONLY when the pool cannot cover the
/// model without them, and loudly. Else an honest failure.
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
    let mut others: Vec<_> = rp
        .nodes
        .iter()
        .filter(|n| n.name != coordinator.name)
        .collect();
    others.sort_by(|a, b| {
        rtt_of(&a.name)
            .cmp(&rtt_of(&b.name))
            .then(b.vram_free_mb.cmp(&a.vram_free_mb))
            .then(a.name.cmp(&b.name))
    });
    let (near, far): (Vec<_>, Vec<_>) = others
        .into_iter()
        .partition(|n| rtt_of(&n.name) <= max_rtt_ms);
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
    match reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Reconcile ONE endpoint on its coordinator (this node). Ensures model +
/// llama-server child; updates status in the runtime map.
/// A LIVE child is never relaunched, healthy or not: llama-server answers /health with 503 for the
/// entire multi-minute model load (tensor upload to RPC members dominates for pooled models), and
/// treating that as dead would spawn a doomed duplicate every tick that fails to bind the port the
/// loading child holds and orphans the real child's handle. Only an EXITED child triggers a
/// relaunch; health merely flips the reported status. A transfer-progress watchdog covers the load
/// itself: one still 'running' past `HIVE_INFERENCE_LOAD_TIMEOUT_SECS` (default 3600) is an
/// RTT-limited or wedged tensor upload that would hold the endpoint in `starting` forever, so it
/// aborts to `failed` and parks (see `LOAD_TIMEOUT_STATUS`); `load_started_ms` is 0 once /health
/// has ever answered OK, so health flaps re-show `starting` but never re-arm the abort.
///
/// Pool admission requires a dialable rpc address: `plan_members` counts a member's VRAM toward
/// coverage, but the `--rpc` list can only name members with a public IP (`ip:50052`, peer-allowed
/// through hive-lockdown), so address-less candidates are dropped BEFORE planning, otherwise the
/// plan could claim coverage the launched server can never reach and fail the load with no honest
/// placement error.
///
/// Any UNTRACKED llama-server for this project (a survivor of a previous hive-node process) is
/// killed first, because it holds the port and a tracked child must own the endpoint so
/// kill/reassign semantics stay real; both shapes are swept, a bare child and a transient scope
/// (stopping the scope by name kills its members). The server runs in its OWN cgroup (a transient
/// `systemd-run --scope` under a dedicated slice; `HIVE_INFERENCE_SCOPE=0` or no systemd means a
/// plain child), never as a plain child of hive-cloud: a plain child lands in hive-node.service's
/// memory cgroup, so a model's resident footprint competes with the platform process against the
/// SERVICE's MemoryMax and the kernel OOM-kills the node's platform process rather than the
/// inference server. A scope accounts inference memory separately, so an over-large model fails
/// only its own endpoint (an honest `failed` status) and can never take hive-node down with it.
/// `LD_LIBRARY_PATH` names both library dirs: the CUDA runtime libs live in lib/, the build's own
/// libggml*.so in bin/.
async fn reconcile_local(
    cloud: &Arc<CloudState>,
    project: &str,
    spec: &crate::project_settings::InferenceSpec,
) {
    let port = port_for(project);
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
            (
                None,
                EndpointStatus {
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
                },
            )
        });
        e.1.model = spec.model.clone();
        e.1.pool = spec.pool;
        e.1.rpc_members = members.to_vec();
        e.1.status = status;
        e.1.updated_ms = hive_core::now_ms();
    };

    // Never awaits a download inline: this loop serves every project on the coordinator.
    let (model_path, model_bytes) = match model_state(cloud, project, &spec.model).await {
        ModelState::Ready(path, bytes) => (path, bytes),
        ModelState::Downloading => {
            set_status(cloud, "starting".into(), &[]);
            return;
        }
        ModelState::Failed(e) => {
            tracing::warn!(project, error = %e, "inference: model unavailable");
            set_status(cloud, format!("failed: {e}"), &[]);
            return;
        }
    };
    let need = vram_need_mb(model_bytes);
    let me = cloud.registry.nodes().into_iter().find(|n| n.is_self);
    let Some(me) = me else { return };
    let mut pools = crate::gpu_pool::snapshot(cloud).await;
    let addressable: std::collections::HashSet<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.public_ip.is_some())
        .map(|n| n.name)
        .collect();
    for p in &mut pools {
        p.nodes
            .retain(|n| n.name == me.name || addressable.contains(&n.name));
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
    let member_addrs: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| members.contains(&n.name))
        .filter_map(|n| n.public_ip.map(|ip| format!("{ip}:50052")))
        .collect();

    let _ = tokio::process::Command::new("pkill")
        .args(["-f", &format!("llama-server.*--alias {project}")])
        .output()
        .await;
    let _ = tokio::process::Command::new("systemctl")
        .args(["stop", &format!("{}.scope", scope_name(project))])
        .output()
        .await;

    let use_scope = std::env::var("HIVE_INFERENCE_SCOPE")
        .map(|v| v != "0")
        .unwrap_or(true)
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
/// Teardown is deliberately asymmetric: a REMOVED spec (project gone from `desired`) kills at once,
/// so a deleted project's endpoint does not linger, while a MOVED assignment (still desired,
/// elected elsewhere) gets a grace window (`HIVE_INFERENCE_REASSIGN_GRACE_SECS`, default 600)
/// before the kill, so a coordinator that dropped out of the roster and briefly computes a partial
/// roster on rejoin does not slaughter a mid-load server (rendezvous hashing already makes
/// reassignment rare). A transient scope OUTLIVES the process that spawned it (that is the point:
/// inference memory is accounted separately from `hive-node.service`), so a kill also stops the
/// unit by name; otherwise the server keeps serving a project that moved away.
pub fn spawn_reconcile(cloud: Arc<CloudState>) {
    let interval = Duration::from_secs(
        std::env::var("HIVE_INFERENCE_RECONCILE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
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

            let mut mine: Vec<(String, crate::project_settings::InferenceSpec)> = Vec::new();
            for (p, spec) in &desired {
                if coordinator_for(&cloud, p).map(|n| n.name) == Some(cloud.node_name.clone()) {
                    mine.push((p.clone(), spec.clone()));
                }
            }
            {
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
                        let unit = format!("{}.scope", scope_name(&p));
                        tokio::spawn(async move {
                            let _ = tokio::process::Command::new("systemctl")
                                .args(["stop", &unit])
                                .output()
                                .await;
                        });
                        tracing::info!(project = %p, "inference: stopped endpoint (no longer desired/assigned here)");
                    }
                }
            }
            for (p, spec) in &mine {
                reconcile_local(&cloud, p, spec).await;
            }

            if crate::leadership::may_act(&cloud, crate::leadership::Job::InferenceEnv) {
                for (p, _spec) in &desired {
                    let Some(coord) = coordinator_for(&cloud, p) else {
                        continue;
                    };
                    let Some(ip) = coord.public_ip.clone() else {
                        continue;
                    };
                    let url = format!("http://{ip}:{}/v1", port_for(p));
                    let current = cloud.projects.env_map(p).get("HIVE_INFERENCE_URL").cloned();
                    if current.as_deref() != Some(url.as_str()) {
                        cloud.projects.put_env(
                            p,
                            crate::project_settings::EnvVar {
                                key: "HIVE_INFERENCE_URL".into(),
                                value: url.clone(),
                                target: "all".into(),
                                scope: "runtime".into(),
                                sensitive: false,
                                updated_ms: hive_core::now_ms(),
                            },
                        );
                        tracing::info!(project = %p, %url, "inference: injected HIVE_INFERENCE_URL");
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}
