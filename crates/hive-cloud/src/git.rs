//! Deploy from a git repository with a live, Vercel-style **build log**.
//!
//! `start_build` creates a build record (state = building) and returns its id
//! immediately; a background task clones the repo, emits timestamped log lines
//! (region, machine config, cloning, install/build commands, ready), then
//! registers the routable deployment via the gateway. The dashboard polls
//! `GET /v1/builds/:id` to stream the logs as they appear.

use base64::Engine;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fluid_core::{
    DeployState, FunctionConfig, GitDeployRequest, GitSource, Manifest, PortSpec, Route,
    RouteTarget, ServiceProtocol,
};
use hive_core::now_ms;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use tokio::process::Command;
use uuid::Uuid;

use crate::state::CloudState;

#[derive(Clone, Serialize, serde::Deserialize)]
pub struct LogLine {
    pub ts_ms: u64,
    pub line: String,
}

#[derive(Clone, Serialize, serde::Deserialize)]
pub struct Build {
    pub id: String,
    pub project: String,
    pub repo_url: String,
    pub branch: String,
    pub commit: String,
    pub commit_message: String,
    pub state: DeployState,
    pub started_ms: u64,
    #[serde(default)]
    pub finished_ms: Option<u64>,
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    /// Set when a NEWER build of the same project started while this one was
    /// in flight — `run_build` checks it just before `deploy_full` and vetoes
    /// its own production flip, so the last flip is always the newest push,
    /// never merely the last-finishing build (latest-push-wins).
    #[serde(default)]
    pub superseded_by: Option<String>,
    /// Human-readable reason a build reached `Error`, structured (not just a
    /// buried log line) so a caller can answer "why did this fail" without
    /// re-deriving it by parsing `lines`. This field DID NOT EXIST before —
    /// any reader pulling `build["error"]` off the raw record always got the
    /// zero-value `""` no matter what actually failed, because there was
    /// nothing to clear: the key was never modeled. Witnessed live: 10
    /// consecutive `internet-structure` build failures, each with a full
    /// diagnostic message sitting in `lines` (e.g. "lost contact with remote
    /// build after 400 failed polls") and an empty top-level error every
    /// time. `#[serde(default)]` so pre-existing persisted records without
    /// this key still deserialize.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub lines: Vec<LogLine>,
}

/// Cap on how many builds the snapshot persists (newest by `started_ms`) —
/// bounds the state file while keeping every deployment a user can realistically
/// navigate to covered with its build logs.
const SNAPSHOT_BUILD_CAP: usize = 200;

/// Cap on how many builds are retained IN MEMORY. The snapshot was already
/// bounded; the live map was not, so a node that builds continuously kept every
/// build it had ever run — with its log lines — until it restarted. Set above
/// `SNAPSHOT_BUILD_CAP` so nothing that would have been persisted is dropped
/// from memory first (a build visible after a restart but not before would be
/// an absurd failure mode).
const MEMORY_BUILD_CAP: usize = 400;

/// Cap on the bytes retained for ONE log line. The build-output reader already
/// caps what it takes off the pipe (`hive_core::logcap`), but `log()` is also
/// called with internally-formatted messages (command echoes, error text that
/// embeds subprocess output) — a byte bound belongs at the store boundary too,
/// so no future caller can reintroduce an unbounded line.
const MAX_BUILD_LOG_LINE_BYTES: usize = 16 * 1024;

#[derive(Default)]
pub struct BuildStore {
    map: Mutex<HashMap<String, Build>>,
}

impl BuildStore {
    pub fn new() -> BuildStore {
        BuildStore {
            map: Mutex::new(HashMap::new()),
        }
    }
    pub fn get(&self, id: &str) -> Option<Build> {
        self.map.lock().get(id).cloned()
    }
    pub fn list(&self) -> Vec<Build> {
        self.map.lock().values().cloned().collect()
    }
    /// Persistable view (newest `SNAPSHOT_BUILD_CAP` builds). Builds were
    /// purely in-memory before this — every node restart silently erased all
    /// build logs, which is why /deployments/<id> showed "No build record
    /// found" for anything deployed before the last restart. DeployRecords
    /// survived (they ARE persisted), making the loss invisible until a user
    /// opened the build-logs panel.
    pub fn snapshot(&self) -> Vec<Build> {
        let mut v: Vec<Build> = self.map.lock().values().cloned().collect();
        v.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
        v.truncate(SNAPSHOT_BUILD_CAP);
        v
    }
    /// Boot-time restore. A build persisted mid-flight (`Queued`/`Building`)
    /// is dead — its process did not survive the restart — so it is finalized
    /// as `Error` with an explanatory log line rather than presenting as
    /// running forever.
    pub fn load(&self, builds: Vec<Build>) {
        let mut m = self.map.lock();
        for mut b in builds {
            if matches!(b.state, DeployState::Queued | DeployState::Building) {
                b.state = DeployState::Error;
                b.finished_ms.get_or_insert_with(now_ms);
                let msg = "build interrupted: node restarted while the build was in flight";
                b.lines.push(LogLine {
                    ts_ms: now_ms(),
                    line: msg.into(),
                });
                b.error.get_or_insert_with(|| msg.into());
            }
            m.entry(b.id.clone()).or_insert(b);
        }
    }
    fn insert(&self, b: Build) {
        let mut m = self.map.lock();
        m.insert(b.id.clone(), b);
        // Bound the live map, not just the persisted snapshot. Evict the oldest
        // FINISHED builds only — an in-flight build must never lose its record
        // out from under `log()`/`update()`, which would silently stop
        // collecting its output.
        if m.len() > MEMORY_BUILD_CAP {
            let mut finished: Vec<(u64, String)> = m
                .values()
                .filter(|b| b.finished_ms.is_some())
                .map(|b| (b.started_ms, b.id.clone()))
                .collect();
            let over = m.len() - MEMORY_BUILD_CAP;
            if !finished.is_empty() {
                finished.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                let mut freed_lines = 0usize;
                for (_, id) in finished.into_iter().take(over) {
                    if let Some(old) = m.remove(&id) {
                        freed_lines += old.lines.len();
                    }
                }
                tracing::debug!(
                    retained = m.len(),
                    cap = MEMORY_BUILD_CAP,
                    freed_lines,
                    "build store evicted oldest finished builds"
                );
            }
        }
    }
    fn log(&self, id: &str, line: impl Into<String>) {
        if let Some(b) = self.map.lock().get_mut(id) {
            let mut line: String = line.into();
            if line.len() > MAX_BUILD_LOG_LINE_BYTES {
                let dropped = line.len() - MAX_BUILD_LOG_LINE_BYTES;
                // Cut on a char boundary — `truncate` panics mid-codepoint.
                let mut cut = MAX_BUILD_LOG_LINE_BYTES;
                while cut > 0 && !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                line.truncate(cut);
                line.push_str(&format!("…[hive: {dropped} bytes dropped]"));
            }
            b.lines.push(LogLine {
                ts_ms: now_ms(),
                line,
            });
            // Cap per-build log retention: a chatty build could otherwise grow
            // an unbounded Vec<LogLine> that is then cloned into EVERY 120s
            // replicated platform snapshot (a real contributor to the fleet's
            // snapshot-churn heap pressure). Keep the most recent lines; a build
            // that needs more than this is already pathological.
            const MAX_BUILD_LOG_LINES: usize = 2_000;
            let len = b.lines.len();
            if len > MAX_BUILD_LOG_LINES {
                b.lines.drain(0..len - MAX_BUILD_LOG_LINES);
            }
        }
    }
    pub fn update(&self, id: &str, f: impl FnOnce(&mut Build)) {
        if let Some(b) = self.map.lock().get_mut(id) {
            f(b);
        }
    }
    /// Remove every build record for `project` — there was previously no
    /// removal path at all (unbounded retention, including full log lines
    /// that can carry commit-author/env/config content), so a deleted
    /// project's build history stayed fully queryable via `GET /v1/builds/:id`
    /// for as long as the process kept running. Returns the count removed.
    pub fn remove_for_project(&self, project: &str) -> usize {
        let mut m = self.map.lock();
        let before = m.len();
        m.retain(|_, b| b.project != project);
        before - m.len()
    }
}

// ---- Build cancellation ----------------------------------------------------
//
// A build's actual work (git clone, npm/pnpm/yarn install, the framework
// build command, a `podman build`) runs as real OS child processes spawned
// from `run_build`'s background task. Marking the `Build` record `Cancelled`
// alone does not stop any of that — the child keeps running to completion
// (or hanging forever) unless something actually signals it. This registry is
// the "something": every long-running command in the pipeline runs through
// `run_cancellable_output` (git clone chain) or is wrapped equivalently in
// `run_streamed` (install/build), each of which publishes the live process's
// GROUP id here before awaiting it. `cancel_build` then (1) flips a flag so no
// FURTHER step starts, (2) SIGKILLs that whole process group — not just one
// pid, since a shell driving `npm install` forks `npm`/`node` as children that
// inherit its group, and a single-pid kill would leave them running as
// orphans — (3) asks a MIRRORED build's real remote host to cancel too, and
// (4) aborts the Rust task driving the build so it can't proceed once its
// process is dead.

/// Where a MIRRORED build's real backing process actually lives: the "pure
/// remote placement" fanout branch in `run_build` dispatches the real build to
/// a peer and only mirrors its logs/state here (see `mirror_remote_build`).
/// Cancelling the coordinator's (mirror) build must also cancel the REAL
/// process, which runs on this target under its OWN, different build id.
#[derive(Clone)]
struct MirrorTarget {
    admin: Option<String>,
    iroh: Option<(String, String)>,
    target_bid: String,
}

#[derive(Default)]
struct BuildCancelSlot {
    /// Process-GROUP id of whatever child THIS build is currently blocked on.
    /// `None` between steps (no command in flight right now).
    pgid: Mutex<Option<u32>>,
    /// Set true the instant `cancel_build` runs — checked at step boundaries
    /// so a step that finishes just as the kill signal goes out never starts
    /// the next command.
    cancelled: std::sync::atomic::AtomicBool,
    /// Set while this build mirrors a remote fanned-out build.
    mirror: Mutex<Option<MirrorTarget>>,
    /// The task actually driving this build.
    task: Mutex<Option<tokio::task::AbortHandle>>,
}

/// Registry of in-flight builds' cancellation handles, keyed by build id.
/// Lives on `CloudState` (`cloud.build_cancels`) — deliberately NOT persisted
/// or gossiped: it describes only a live local OS process, which cannot
/// survive a restart (and a restarted node already finalizes any
/// Queued/Building record to `Error` on boot — see `BuildStore::load`).
#[derive(Default)]
pub struct BuildCancelRegistry {
    map: Mutex<HashMap<String, Arc<BuildCancelSlot>>>,
}

impl BuildCancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fresh slot for a build about to start — called
    /// synchronously in `start_build`, BEFORE the driving task is even
    /// spawned, so a cancel arriving in the first instant of a build's life
    /// (before any child has spawned) still has somewhere to record itself.
    fn register(&self, bid: &str) {
        self.map
            .lock()
            .insert(bid.to_string(), Arc::new(BuildCancelSlot::default()));
    }

    fn attach_task(&self, bid: &str, task: tokio::task::AbortHandle) {
        if let Some(slot) = self.map.lock().get(bid) {
            *slot.task.lock() = Some(task);
        }
    }

    fn slot(&self, bid: &str) -> Option<Arc<BuildCancelSlot>> {
        self.map.lock().get(bid).cloned()
    }

    /// Whether `bid` has been asked to cancel — checked cooperatively at step
    /// boundaries throughout `run_build` so a build already mid-cancel never
    /// starts a fresh, unkillable-until-spawned step.
    pub fn is_cancelled(&self, bid: &str) -> bool {
        self.map
            .lock()
            .get(bid)
            .is_some_and(|s| s.cancelled.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Publish (or clear, `None`) the OS process-GROUP id THIS build is
    /// currently blocked on. Called by every cancellable command wrapper
    /// right after spawn, and again (with `None`) once that command exits —
    /// so a later, unrelated step never targets a stale/reused pid.
    fn set_running(&self, bid: &str, pgid: Option<u32>) {
        if let Some(slot) = self.slot(bid) {
            *slot.pgid.lock() = pgid;
        }
    }

    fn set_mirror(&self, bid: &str, target: MirrorTarget) {
        if let Some(slot) = self.slot(bid) {
            *slot.mirror.lock() = Some(target);
        }
    }

    /// Drop the bookkeeping for a finished build — nothing can spawn under
    /// this id anymore once it's terminal.
    fn forget(&self, bid: &str) {
        self.map.lock().remove(bid);
    }

    /// Cancel an in-flight build: mark it cancelled, SIGKILL whatever OS
    /// process GROUP it's currently blocked on, ask its mirror target (if any)
    /// to cancel the REAL remote build, then give the Rust task driving it a
    /// brief grace window to notice its child died and unwind NORMALLY —
    /// running its own remaining cleanup (e.g. removing a first-deploy
    /// placeholder, `forget`ting this very slot) — before force-aborting it.
    /// A self-driven exit is strictly better than an abort: abort tears the
    /// task down mid-poll with NONE of its remaining cleanup ever running, so
    /// it's the fallback for a task truly stuck somewhere with no tracked
    /// child (a step this registry doesn't instrument), never the common case.
    /// Returns `false` when `bid` has no live slot (already terminal, or
    /// never existed) — the caller's own "not found" handling covers that.
    pub async fn cancel(&self, cloud: &Arc<CloudState>, bid: &str) -> bool {
        let slot = match self.slot(bid) {
            Some(s) => s,
            None => return false,
        };
        slot.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let pgid = *slot.pgid.lock();
        if let Some(pg) = pgid {
            // Negative pid = signal the whole process GROUP (see module doc).
            let _ = tokio::process::Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{pg}"))
                .output()
                .await;
        }
        let mirror = slot.mirror.lock().clone();
        if let Some(m) = mirror {
            let _ = cancel_remote_build(cloud, &m).await;
        }
        let task = slot.task.lock().clone();
        if let Some(task) = task {
            // SIGKILL delivery + the task noticing (its `child.wait()` return)
            // is normally low tens of ms; 1s at 50ms steps gives it ample room
            // without making a cancel request feel stuck.
            for _ in 0..20 {
                if task.is_finished() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if !task.is_finished() {
                task.abort();
            }
        }
        // The task's own tail (if it got to run) already called `forget` —
        // this is a no-op then, and the necessary cleanup for the abort path.
        self.forget(bid);
        true
    }
}

/// Ask a remote node to cancel a build it's actually running (the target of a
/// mirrored/fanned-out build) — same transport `fanout_remote`/
/// `mirror_remote_build` use. Best-effort: a failure here still leaves the
/// LOCAL kill (if any) and the cancelled-flag/task-abort in effect.
async fn cancel_remote_build(cloud: &Arc<CloudState>, m: &MirrorTarget) -> bool {
    let path = format!("/v1/builds/{}/cancel", m.target_bid);
    if let Some(admin) = &m.admin {
        return cloud
            .http
            .post(format!("{admin}{path}"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
    }
    if let Some((id, addr)) = &m.iroh {
        return crate::gossip::request_to(
            cloud,
            id,
            addr,
            hive_p2p::GOSSIP_POST,
            &path,
            &[],
            15,
        )
        .await
        .is_some();
    }
    false
}

/// Like `Command::output()`, but first drops `cmd` into its OWN process group
/// (`process_group(0)`: pgid == its own pid) and publishes that group into the
/// per-build cancel registry — so `cancel_build` can SIGKILL the whole tree,
/// not just this one pid, if the user cancels while THIS command is the one
/// hung. Cleared again once the command exits so a later, unrelated step
/// never targets a stale/reused pid. Used by the git clone/fetch/checkout
/// chain; `run_streamed` (install/build) does the equivalent inline since it
/// already manages its own child for log streaming.
async fn run_cancellable_output(
    cmd: &mut Command,
    cloud: &Arc<CloudState>,
    bid: &str,
) -> std::io::Result<std::process::Output> {
    use std::process::Stdio;
    cmd.process_group(0);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    cloud.build_cancels.set_running(bid, child.id());
    if cloud.build_cancels.is_cancelled(bid) {
        // A cancel landed in the tiny window between the caller's last
        // cooperative check and this spawn — kill it now rather than let it
        // run unkilled to completion.
        let _ = child.start_kill();
    }
    let out = child.wait_with_output().await;
    cloud.build_cancels.set_running(bid, None);
    out
}

/// Marker error: `run_build` returned `Err` because the user cancelled it, not
/// because a step genuinely failed. Lets the outer catch (in `start_build`'s
/// driving task) set `DeployState::Cancelled` instead of `Error` without
/// fragile string-matching on the message.
#[derive(Debug)]
struct BuildCancelled;
impl std::fmt::Display for BuildCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "build cancelled by user")
    }
}
impl std::error::Error for BuildCancelled {}

/// Sanitize a string for use in a container image tag ([a-z0-9._-] only).
pub(crate) fn sanitize_tag(s: &str) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out
        .trim_matches(|c| c == '-' || c == '.' || c == '_')
        .to_string();
    if out.is_empty() {
        "app".into()
    } else {
        out
    }
}

pub fn project_name_from_url(url: &str) -> String {
    // Container-image refs (`image://ns/name:tag`): the project is the image NAME —
    // drop the registry/namespace prefix AND the `:tag` (else "simplifi:latest"
    // slugs to the ugly "simplifi-latest" and every tag change forks the project).
    let last = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("project");
    let last = if url.starts_with("image://") {
        last.split(':').next().unwrap_or(last)
    } else {
        last
    };
    last.trim_end_matches(".git")
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Durable deployment root: `$HIVE_DATA/deploys`. The mock backend serves files
/// straight from a deployment's `root` for the deployment's whole life, so the
/// root must survive a host reboot for exactly as long as the (replicated)
/// deployment RECORD does. Under `/tmp` it did not: a reboot wiped the checkout
/// while the record persisted, and the node then 404'd DEPLOYMENT_NOT_FOUND for
/// a deployment it believed it had (witnessed live: dan.shadw.app, 2026-08-03).
/// Pre-change roots under [`legacy_deploy_root`] keep working untouched — records
/// carry absolute paths and the boot restore keys off `root` existing — until
/// their host reboots; the lookup/purge/GC paths below all fall back to the
/// legacy root so pre-change zip projects can still redeploy from retained
/// source and their stale dirs still get reaped.
pub(crate) fn deploy_root() -> PathBuf {
    crate::persist::data_dir().join("deploys")
}

/// The pre-durability deployment root (`$TMPDIR/hive-deploys`). Nothing writes
/// here anymore; it remains a read fallback (retained source for redeploys) and
/// a GC/purge target until every pre-change checkout has aged out or rebooted away.
pub(crate) fn legacy_deploy_root() -> PathBuf {
    std::env::temp_dir().join("hive-deploys")
}

/// A project's on-disk name component under the deploy root. A project name is
/// tenant-controlled text, so it is NEVER interpolated into a path verbatim —
/// `sanitize_tag` maps it to `[a-z0-9._-]` (the same discipline as `hive-vol-`
/// volume names), so a project called `..` or `a/b` can never escape the root.
/// Pre-sanitization checkouts used the raw name; readers that must still find
/// those take both prefixes (see `checkout_prefixes`).
fn checkout_tag(project: &str) -> String {
    sanitize_tag(project)
}

/// Dir-name prefixes a project's checkouts can carry: the sanitized component
/// (current), plus the raw project name (pre-sanitization dirs). Matching both
/// is what lets redeploy/GC/purge see checkouts written by older binaries.
pub(crate) fn checkout_prefixes(project: &str) -> Vec<String> {
    let tag = checkout_tag(project);
    let mut v = vec![format!("{tag}-")];
    let raw = format!("{project}-");
    if !v.contains(&raw) {
        v.push(raw);
    }
    v
}

/// The newest on-disk deploy checkout for a project (`<deploy_root>/<tag>-*`),
/// preferring a completed checkout with a package.json/Dockerfile. Lets a node that
/// holds the SOURCE derive the service graph even when it holds no deployment
/// RECORD (container deploys register the record on the coordinator, run on the
/// lease-owner node). Scans BOTH the durable root and the legacy /tmp root.
/// Returns None if no source is on disk.
pub(crate) fn newest_deploy_dir(project: &str) -> Option<PathBuf> {
    let prefixes = checkout_prefixes(project);
    let mut cands: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for base in [deploy_root(), legacy_deploy_root()] {
        let Ok(rd) = std::fs::read_dir(&base) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !prefixes.iter().any(|p| name.starts_with(p.as_str())) {
                continue;
            }
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            // Only real checkouts (has a package.json or a container build file).
            if !p.join("package.json").exists()
                && !p.join("Dockerfile").exists()
                && !p.join("Containerfile").exists()
            {
                continue;
            }
            if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                cands.push((m, p));
            }
        }
    }
    cands.sort_by_key(|(m, _)| *m);
    cands.pop().map(|(_, p)| p)
}

/// Whether `project` already has a deployment ANYWHERE in the fleet — the local
/// gateway OR a peer node (via gossiped `peer_deployments`). This distinguishes a
/// FIRST deploy from a REDEPLOY. Critical for remotely-placed projects: the
/// coordinator doesn't serve the host locally, so a local-only `serves_host` check
/// wrongly treats every redeploy as a first deploy — spawning a phantom "Building…"
/// placeholder deployment (git-less → classified Preview) and forcing `npm ci`.
fn project_has_deployment(cloud: &Arc<CloudState>, project: &str) -> bool {
    if cloud.gw.serves_host(&format!("{project}.localhost")) {
        return true;
    }
    if cloud.gw.list().iter().any(|d| d.project == project) {
        return true;
    }
    cloud
        .peer_deployments
        .read()
        .values()
        .flatten()
        .any(|d| d.project == project)
}

/// Self-management GC: reap stale clone/build working dirs under the deploy
/// roots. Each build clones a repo into `<root>/<tag>-<stamp>-<bid>` (and
/// `-building-<ms>-<bid>`); these are NOT removed after the build, so they
/// accumulate and exhaust disk over time. Remove any dir untouched for longer
/// than `max_age` — UNLESS it is the `root` of a LIVE deployment. The mock
/// backend serves files straight from `root`, and the restart restore keys off
/// `root` existing, so reaping an active root would take a deployment offline /
/// drop it on the next restart (this is the bug that dropped shoomoo). Sweeps
/// BOTH the durable root and the legacy /tmp root (pre-durability checkouts
/// still age out there). Best-effort; returns the number of dirs removed.
pub async fn gc_build_dirs(cloud: &Arc<CloudState>, max_age: Duration) -> usize {
    // Never reap a dir that currently backs a deployment (its `root`), canonicalized
    // so symlink/relative differences don't cause a false "not live".
    let live_roots: std::collections::HashSet<PathBuf> = cloud
        .gw
        .deployment_records()
        .iter()
        .map(|r| std::fs::canonicalize(&r.root).unwrap_or_else(|_| PathBuf::from(&r.root)))
        .collect();
    let now = std::time::SystemTime::now();
    let mut removed = 0usize;
    for root in [deploy_root(), legacy_deploy_root()] {
        let mut entries = match tokio::fs::read_dir(&root).await {
            Ok(d) => d,
            Err(_) => continue,
        };
        while let Ok(Some(e)) = entries.next_entry().await {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let canon = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if live_roots.contains(&canon) {
                continue; // backs a live deployment — keep it
            }
            let stale = e
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| now.duration_since(t).ok())
                .map(|age| age > max_age)
                .unwrap_or(false);
            if stale && tokio::fs::remove_dir_all(&p).await.is_ok() {
                removed += 1;
                tracing::info!(dir = %p.display(), "gc: reaped stale build dir");
            }
        }
    }
    removed
}

/// Releases the `Building…` placeholder's hold on the project's host alias even
/// when the build task never finishes.
///
/// `Drop` runs when a spawned task is ABORTED (the future is dropped) and when
/// it panics — the two cases that used to leak the placeholder forever, since
/// the removal lived at the end of the happy path. Removal is async and `Drop`
/// is not, so the guard hands the work to a detached task; outside a runtime
/// (process teardown) it degrades to the boot reconciler + reaper, which reap
/// the same shells.
struct PlaceholderGuard {
    cloud: Arc<CloudState>,
    id: Option<String>,
}

impl PlaceholderGuard {
    /// Take the id so the caller removes it inline on the happy path — after
    /// this the guard is inert.
    fn disarm(&mut self) -> Option<String> {
        self.id.take()
    }
}

impl Drop for PlaceholderGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(deployment = %id, "Building… placeholder left behind (no runtime to remove it); the boot reconciler will drop it");
            return;
        };
        let cloud = self.cloud.clone();
        handle.spawn(async move {
            cloud.gw.remove(&id).await;
            crate::persist::persist(&cloud);
            tracing::warn!(deployment = %id, "removed Building… placeholder after its build task ended without finishing (cancel/panic)");
        });
    }
}

/// Start a build; returns its id immediately. The build runs in the background.
pub fn start_build(cloud: Arc<CloudState>, req: GitDeployRequest) -> String {
    let id = format!("dpl-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let project = req
        .project
        .clone()
        .unwrap_or_else(|| project_name_from_url(&req.repo_url));
    // Latest-push-wins coalescing (bn-launch-sequencing-rebuild-before-deploy):
    // any in-flight build of the same project is now stale — mark it superseded
    // so it vetoes its own production flip just before `deploy_full`. Without
    // this, two rapid pushes race to the production alias and the LAST FINISH
    // wins regardless of commit freshness (an older commit's slow build could
    // regress production indefinitely).
    for b in cloud.builds.list() {
        if b.project == project && matches!(b.state, DeployState::Queued | DeployState::Building) {
            cloud.builds.update(&b.id, |old| {
                old.superseded_by = Some(id.clone());
                old.lines.push(LogLine {
                    ts_ms: now_ms(),
                    line: format!("superseded by newer build {id}"),
                });
            });
        }
    }
    cloud.builds.insert(Build {
        id: id.clone(),
        project: project.clone(),
        repo_url: req.repo_url.clone(),
        branch: req.branch.clone().unwrap_or_default(),
        commit: String::new(),
        commit_message: String::new(),
        state: DeployState::Building,
        started_ms: now_ms(),
        finished_ms: None,
        deployment_id: None,
        alias: None,
        superseded_by: None,
        error: None,
        lines: Vec::new(),
    });

    crate::webhooks::dispatch(
        &cloud.webhooks,
        &project,
        "deployment.created",
        serde_json::json!({ "id": id, "project": project, "repo": req.repo_url, "state": "building" }),
    );

    // Register cancellation bookkeeping BEFORE spawning the driving task, so a
    // cancel arriving in the first instant of this build's life (before any
    // child process even exists) still has somewhere to record itself — see
    // `BuildCancelRegistry`.
    cloud.build_cancels.register(&id);

    let bid = id.clone();
    let wh_project = project.clone();
    let cloud_for_registry = cloud.clone();
    let handle = tokio::spawn(async move {
        // Whether this is the project's FIRST deployment — captured BEFORE the
        // placeholder registers an alias (which would otherwise make it look like a
        // redeploy). Drives `npm ci` on the initial build (Task 1). Fleet-aware so a
        // redeploy of a remotely-placed project isn't mistaken for a first deploy.
        let first_deploy = !project_has_deployment(&cloud, &project);
        // First-deploy only: serve a "Building…" page at the domain immediately so
        // the URL resolves throughout the build (a slow Next.js build no longer
        // 404s). The real deployment supersedes it; we then remove the placeholder.
        // The placeholder is a RESERVATION (it holds the project's host alias),
        // so it is released by a Drop guard, not by a line at the end of the
        // happy path: a user cancel ABORTS this task, and an aborted future
        // never reaches its remaining statements — which is how a shell that
        // owns `<project>` outlived its build and made this node answer for a
        // deployment it does not have. Same discipline as `ColdStartGuard`.
        let mut placeholder = PlaceholderGuard {
            cloud: cloud.clone(),
            id: register_building_placeholder(&cloud, &project, &req, &bid).await,
        };
        let result = run_build(&cloud, &bid, req, project, first_deploy).await;
        if let Some(pid) = placeholder.disarm() {
            // The real deploy (or the build-failed page) has taken over the alias;
            // drop the placeholder so it doesn't linger. On a hard error this also
            // clears the "Building…" page (matching prior no-deployment behavior).
            let _ = cloud.gw.remove(&pid).await;
            crate::persist::persist(&cloud);
        }
        if let Err(e) = result {
            // A cancel already stamped the record + fired the kill (see
            // `BuildCancelRegistry::cancel`, called from the admin handler) —
            // distinguish that from a genuine build failure so we don't
            // overwrite `Cancelled` with `Error` nor fire a spurious
            // `deployment.error` webhook for a user-requested stop.
            let cancelled = e.downcast_ref::<BuildCancelled>().is_some()
                || cloud.build_cancels.is_cancelled(&bid);
            if cancelled {
                cloud.builds.log(&bid, "Build cancelled by user.".to_string());
                cloud.builds.update(&bid, |b| {
                    if !matches!(b.state, DeployState::Cancelled) {
                        b.state = DeployState::Cancelled;
                        b.finished_ms.get_or_insert_with(now_ms);
                    }
                });
            } else {
                let err_text = e.to_string();
                cloud.builds.log(&bid, format!("Error: {err_text}"));
                cloud.builds.update(&bid, |b| {
                    b.state = DeployState::Error;
                    b.finished_ms = Some(now_ms());
                    b.error = Some(err_text.clone());
                });
                crate::webhooks::dispatch(
                    &cloud.webhooks,
                    &wh_project,
                    "deployment.error",
                    serde_json::json!({ "id": bid, "project": wh_project, "error": e.to_string() }),
                );
            }
        }
        cloud.build_cancels.forget(&bid);
    });
    cloud_for_registry
        .build_cancels
        .attach_task(&id, handle.abort_handle());
    id
}

/// True when THIS node holds the retained source for `project` — a durable
/// `<tag>.src.zip` OR a prior build checkout — i.e. a zip redeploy can rebuild
/// here without re-fetching from a peer.
pub(crate) fn has_local_source(project: &str) -> bool {
    retained_source_path(project).is_some() || newest_deploy_dir(project).is_some()
}

/// The durable retained-source archive for a zip-uploaded project
/// (`<deploy_root>/<tag>.src.zip`). Writes always target this path.
fn retained_source_write_path(project: &str) -> PathBuf {
    deploy_root().join(format!("{}.src.zip", checkout_tag(project)))
}

/// Find a browser-handler file for a function that did NOT opt in via
/// fluid.json — the entry point for AUTOMATIC browser eligibility.
///
/// Fixed, function-scoped-then-generic names are probed, first existing match
/// wins: the explicit `<fn>.browser.{js,mjs,cjs}` / `browser.{js,mjs,cjs}`
/// convention first, then the function's OWN plausible entry files
/// (`<fn>.handler.*`, `<fn>.*`, then generic `handler.*` / `index.*` /
/// `main.*`) — so a function whose ordinary handler file exports
/// `module.exports` is served in browsers with zero config, not just one that
/// ships a dedicated `.browser.js`. What makes probing a function's own entry
/// safe (rather than "shipping the wrong code" from a long-running server) is
/// `bundle()`'s BUILD-TIME gate: the forbidden-surface scan drops any entry
/// that uses `require`/`import`/`process`/Node APIs, and `handler_export_present`
/// drops any entry that never assigns `module.exports`/`exports.handler` — a
/// server's entry hits both, so it is filtered, never bundled. A synthesized
/// candidate that fails to bundle is skipped SILENTLY (the function just serves
/// the normal fleet path); only an explicit fluid.json opt-in fails the build.
/// The `start_cmd` argv itself is still never parsed for an entry (it may be
/// `next start`/`npm start` with no JS file at all); the file names above are
/// probed on disk instead. The function name only builds a bare filename
/// (rejected unless `[a-z0-9._-]`), and `bundle()`'s `resolve_entry` re-checks
/// the final path stays inside the deployment root — no walk outside `build_dir`.
fn infer_browser_entry(build_dir: &Path, fn_name: &str) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    let safe_name = fn_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !fn_name.is_empty();
    if safe_name {
        for ext in ["js", "mjs", "cjs"] {
            candidates.push(format!("{fn_name}.browser.{ext}"));
        }
    }
    for ext in ["js", "mjs", "cjs"] {
        candidates.push(format!("browser.{ext}"));
    }
    // The function's own plausible handler files, most-specific first. Each is
    // still gated by bundle()'s forbidden-surface + handler-export checks, so a
    // matched-but-ineligible file (a server entry) is skipped, not shipped.
    if safe_name {
        for ext in ["js", "mjs", "cjs"] {
            candidates.push(format!("{fn_name}.handler.{ext}"));
        }
        for ext in ["js", "mjs", "cjs"] {
            candidates.push(format!("{fn_name}.{ext}"));
        }
    }
    for stem in ["handler", "index", "main"] {
        for ext in ["js", "mjs", "cjs"] {
            candidates.push(format!("{stem}.{ext}"));
        }
    }
    candidates
        .into_iter()
        .find(|c| build_dir.join(c).is_file())
}

/// Every function name a repo's `fluid.json` EXPLICITLY opted into browser
/// execution, read straight off the raw file.
///
/// Load-bearing because only ONE of `produce_manifest`'s five manifest shapes
/// (the plain-`fluid.json` branch) deserializes the file into a `Manifest` at
/// all: the prebuilt-image, docker-compose, Dockerfile, and FDI branches each
/// SYNTHESIZE `functions` from their own source, so `FunctionConfig::browser`
/// arrives at the bundling pass as `None` and the opt-in evaporates with no
/// error, no log, and no artifact — the deployment goes Ready and the picker
/// silently omits it. The build-contract rule is loud-or-honored, never
/// silently dropped, so `deploy_full` compares this against the manifest it
/// actually produced (see its "dropped browser opt-in" guard) and fails the
/// build naming each lost function. Parsed with a minimal local shape (not
/// `Manifest`) so a container-path `fluid.json` that carries only a `container`
/// block — or a malformed one, which those paths tolerate today — still yields
/// an empty list rather than becoming a new way to fail a build.
async fn fluid_json_browser_optins(build_dir: &Path) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Fn_ {
        #[serde(default)]
        name: String,
        #[serde(default)]
        browser: Option<serde_json::Value>,
    }
    #[derive(serde::Deserialize)]
    struct Wrap {
        #[serde(default)]
        functions: Vec<Fn_>,
    }
    let Ok(text) = tokio::fs::read_to_string(build_dir.join("fluid.json")).await else {
        return Vec::new();
    };
    serde_json::from_str::<Wrap>(&text)
        .map(|w| {
            w.functions
                .into_iter()
                .filter(|f| f.browser.as_ref().is_some_and(|v| !v.is_null()))
                .map(|f| f.name)
                .collect()
        })
        .unwrap_or_default()
}

/// The retained-source archive to READ: the durable location first, then the
/// pre-durability /tmp locations (sanitized and raw-name) so a zip project
/// deployed by an older binary can still redeploy after the upgrade.
fn retained_source_path(project: &str) -> Option<PathBuf> {
    let tag = checkout_tag(project);
    [
        retained_source_write_path(project),
        legacy_deploy_root().join(format!("{tag}.src.zip")),
        legacy_deploy_root().join(format!("{project}.src.zip")),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Redeploy a project whose retained source lives on a SPECIFIC host node (not this
/// coordinator). Dispatch the rebuild to that node over the existing fanout transport
/// (HTTP admin or iroh mesh); the target, receiving an `upload://` request with no
/// archive, rebuilds from ITS OWN retained source. The remote build is mirrored into a
/// local build record so the dashboard shows a single build. Returns the build id.
pub(crate) fn redeploy_on_host(
    cloud: Arc<CloudState>,
    project: String,
    req: GitDeployRequest,
    host: crate::schedule::Target,
) -> String {
    let id = format!("dpl-{}", &Uuid::new_v4().simple().to_string()[..10]);
    cloud.builds.insert(Build {
        id: id.clone(),
        project: project.clone(),
        repo_url: req.repo_url.clone(),
        branch: req.branch.clone().unwrap_or_default(),
        commit: String::new(),
        commit_message: String::new(),
        state: DeployState::Building,
        started_ms: now_ms(),
        finished_ms: None,
        deployment_id: None,
        alias: None,
        superseded_by: None,
        error: None,
        lines: Vec::new(),
    });
    cloud.build_cancels.register(&id);
    let bid = id.clone();
    let cloud_for_registry = cloud.clone();
    let handle = tokio::spawn(async move {
        cloud.builds.log(
            &bid,
            format!("Redeploy: dispatching to host node {}", host.node),
        );
        // Single pinned host = the deploy's one-and-only (primary) target — never
        // a secondary replica of a multi-region fanout. `fanout_remote` publishes
        // it as this build's cancel mirror target as soon as dispatch succeeds.
        let ok = fanout_remote(
            &cloud,
            &bid,
            &req,
            &project,
            std::slice::from_ref(&host),
            true,
        )
        .await;
        let cancelled = !ok && cloud.build_cancels.is_cancelled(&bid);
        cloud.builds.update(&bid, |b| {
            b.state = if ok {
                DeployState::Ready
            } else if cancelled {
                DeployState::Cancelled
            } else {
                DeployState::Error
            };
            b.finished_ms = Some(now_ms());
        });
        crate::persist::persist(&cloud);
        cloud.build_cancels.forget(&bid);
    });
    cloud_for_registry
        .build_cancels
        .attach_task(&id, handle.abort_handle());
    id
}

async fn run_build(
    cloud: &Arc<CloudState>,
    bid: &str,
    mut req: GitDeployRequest,
    project: String,
    first_deploy: bool,
) -> anyhow::Result<()> {
    let region = &cloud.region;
    let region_label = region_label(region);
    let log = |s: String| cloud.builds.log(bid, s);

    // Persist any env vars supplied with the deploy (e.g. from the New Project
    // screen) onto the project BEFORE building, so they're available to BOTH the
    // build commands (via env_map below) and the runtime (function configs).
    if let Some(env) = &req.env {
        for (k, v) in env {
            if k.trim().is_empty() {
                continue;
            }
            cloud.projects.put_env(
                &project,
                crate::project_settings::EnvVar {
                    key: k.trim().to_string(),
                    value: v.clone(),
                    target: "all".into(),
                    sensitive: false,
                    updated_ms: 0,
                },
            );
        }
        if !env.is_empty() {
            log(format!(
                "Set {} environment variable(s) for the project.",
                env.iter().filter(|(k, _)| !k.trim().is_empty()).count()
            ));
            crate::persist::persist(cloud);
        }
    }

    // On a fanout deploy, adopt the coordinator's forwarded BuildConfig +
    // FunctionSettings so this target builds with the user's configured framework/
    // commands + compute tier (not just auto-detect). Direct user deploys omit
    // these (the coordinator reads its own store).
    if let Some(bc) = req.build_config.as_ref().and_then(|v| {
        serde_json::from_value::<crate::project_settings::BuildConfig>(v.clone()).ok()
    }) {
        cloud.projects.set_build(&project, bc);
    }
    if let Some(fs) = req.function_settings.as_ref().and_then(|v| {
        serde_json::from_value::<crate::project_settings::FunctionSettings>(v.clone()).ok()
    }) {
        cloud.projects.set_functions(&project, fs);
    }

    // ---- Placement scheduler / fanout (coordinator only) -------------------
    // Unless this is already a per-target deploy (`no_fanout`), decide WHERE this
    // project should be HOSTED from its configured regions + live mesh state, and
    // place it there rather than always building on this (the coordinator) node —
    // which is the resource-poor local Mac. See `schedule::place`.
    if !req.no_fanout {
        let regions = cloud.projects.get(&project).functions.regions;
        // is_container is unknown until the repo is built (Dockerfile detection),
        // so default to firecracker placement here; a container is re-homed to a
        // podman-capable node after the build (see the capability re-dispatch below).
        // EXCEPT a prebuilt-image deploy is known to be a container up front → place
        // it on a container-capable node immediately (no post-build re-home needed).
        // Lease-holder-sticky for redeploys: an existing container project's live
        // lease pins the redeploy to its current serving node (see
        // schedule::place_for_project); no lease / new project → normal policy.
        //
        // Stateful/single-writer fanout guard: a prebuilt-image deploy is a known
        // container up front (per the comment above), and per the existing
        // container-lease single-owner model (`lease.rs`) every container is
        // treated as stateful for fanout-placement purposes — see
        // `schedule::place`'s `stateful` doc for why an un-synced multi-region
        // fanout would silently diverge a container's per-node volume. A
        // Dockerfile/compose-detected container isn't known yet at this point
        // (see comment above); once its manifest (and real protocol) exists it
        // is covered post-build instead — by the multi-region-tail guard below
        // when THIS node hosts the build, and by the `fanout_secondary`
        // stateful-replica guard (each dispatched sub-build declines to host if
        // it turns out stateful and non-primary) on the pure-remote fanout
        // branch, which never reaches the tail here and whose `no_fanout`
        // sub-builds skip both of this coordinator's gates.
        let known_container = req.image_ref.is_some();
        let needs_gpu = cloud.projects.get(&project).functions.gpu;
        let targets = crate::schedule::place_for_project(
            cloud,
            &project,
            &regions,
            known_container,
            known_container,
            needs_gpu,
        );
        // A GPU deployment must NEVER fall through to this node when placement
        // found no GPU-capable target. Everywhere else an empty `targets` means
        // "host locally", which is the right default — but for a GPU request that
        // silently lands the deployment on a GPU-less host, and every cold start
        // then dies with `unresolvable CDI devices nvidia.com/gpu=all`. Witnessed
        // live: a gpu project was placed on the CPU-only leader (its GPU nodes had
        // been marked unhealthy) and served 503 DEPLOYMENT_CIRCUIT_OPEN instead of
        // reporting the real reason. Fail loudly here, naming the actual cause.
        if needs_gpu && targets.is_empty() {
            let gpu_nodes = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| n.gpu_count > 0)
                .count();
            let msg = format!(
                // Assembled from single-line pieces on purpose: a `\`-continued
                // Rust string literal keeps its source indentation, which shipped
                // as a run of spaces mid-sentence in a message users actually read.
                "{} {} {}",
                format_args!(
                    "this project requests a serverless GPU, but no healthy GPU-capable node is currently reachable ({gpu_nodes} GPU node(s) known to the mesh)."
                ),
                "The deploy was NOT placed on a CPU node, because the GPU passthrough would fail there.",
                "Check GPU node health, or turn off Serverless GPU in Function Settings to deploy on ordinary compute."
            );
            log(msg.clone());
            tracing::warn!(project = %project, gpu_nodes, "deploy refused: gpu requested, no GPU-capable target");
            return Err(anyhow::anyhow!(msg));
        }
        // #3: surface the auto-chosen region(s) in Function Settings — when a
        // project has none configured (new project), persist where the scheduler
        // placed it so the dashboard shows that region pre-selected/checked.
        if regions.is_empty() && !targets.is_empty() {
            let node_region: std::collections::HashMap<String, String> = cloud
                .registry
                .nodes()
                .into_iter()
                .map(|n| (n.name, n.region))
                .collect();
            let mut placed: Vec<String> = targets
                .iter()
                .filter_map(|t| node_region.get(&t.node).cloned())
                .filter(|r| !r.is_empty())
                .collect();
            placed.sort();
            placed.dedup();
            if !placed.is_empty() {
                let mut fs = cloud.projects.get(&project).functions;
                fs.regions = placed.clone();
                cloud.projects.set_functions(&project, fs);
                crate::persist::persist(cloud);
                log(format!(
                    "Default region(s) set in Function Settings: {}",
                    placed.join(", ")
                ));
            }
        }
        // A target is LOCAL only when it has neither an HTTP admin nor an iroh route
        // (iroh targets are remote FC nodes reached over the mesh).
        let local_selected = targets
            .iter()
            .any(|t| t.admin.is_none() && t.iroh.is_none());
        let remote: Vec<crate::schedule::Target> = targets
            .iter()
            .filter(|t| t.admin.is_some() || t.iroh.is_some())
            .cloned()
            .collect();

        if !targets.is_empty() && !local_selected {
            // Pure remote placement: do NOT build/host locally. Dispatch to the
            // chosen region node(s), mirror their build into this build record,
            // then remove the project from any other node that still hosts it.
            let names: Vec<String> = targets.iter().map(|t| t.node.clone()).collect();
            log(format!(
                "Placement: region-aware scheduler → {}",
                names.join(", ")
            ));
            // `primary_first: true` — on this branch NO local target was selected,
            // so `remote` IS the entire placement in region order: remote[0] (the
            // first requested region's node, matching the region `schedule::place`'s
            // stateful guard would itself constrain to) is the designated primary;
            // every other target is dispatched as a `fanout_secondary` replica.
            // That flag is what closes the stateful-guard hole on this pure-remote
            // path: a first-time Dockerfile/compose deploy's container-ness is
            // UNKNOWN here (see `known_container` above), so this placement gate
            // ran with stateful=false, and each dispatched sub-build carries
            // `no_fanout: true` — skipping BOTH its own initial placement gate AND
            // the post-build multi-region tail gate. Without the flag, a stateful
            // service whose 2+ selected regions exclude the coordinator's own
            // region would build independently in every region with the guard
            // consulted nowhere (the exact split-brain it exists to prevent). With
            // it, each secondary re-evaluates statefulness AFTER its own build and
            // declines to host if stateful (see the fanout-replica guard in
            // `run_build`), collapsing the deploy to the primary region only —
            // while a STATELESS multi-region fanout proceeds on every target
            // exactly as before.
            let ok = fanout_remote(cloud, bid, &req, &project, &remote, true).await;
            // Atomic promotion (Vercel convention): only relocate — i.e. remove the
            // project from nodes that still host the PREVIOUS deployment — once the
            // new placement actually built & is serving. A FAILED build must never
            // take down the currently-serving deployment; the old one keeps serving
            // and the user just sees a failed build. (This is the bug that dropped a
            // healthy project when a relocating redeploy errored.)
            let cancelled = !ok && cloud.build_cancels.is_cancelled(bid);
            if ok {
                cleanup_non_targets(cloud, &project, &names).await;
            } else if cancelled {
                log("Build cancelled by user — keeping the existing deployment in place.".into());
            } else {
                log(
                    "Build failed — keeping the existing deployment in place (no relocation)."
                        .into(),
                );
            }
            cloud.builds.update(bid, |b| {
                b.state = if ok {
                    DeployState::Ready
                } else if cancelled {
                    DeployState::Cancelled
                } else {
                    DeployState::Error
                };
                b.finished_ms = Some(now_ms());
            });
            crate::persist::persist(cloud);
            cloud.build_cancels.forget(bid);
            return Ok(());
        }
        if local_selected && !remote.is_empty() {
            let names: Vec<String> = remote.iter().map(|t| t.node.clone()).collect();
            log(format!(
                "Placement: hosting here + replicating to {} (multi-region)",
                names.join(", ")
            ));
        }
        // local_selected (host here, fanout extras at the tail) OR no eligible
        // target (empty → host locally as a safe fallback): fall through.
    }

    log(format!("Running build in {region_label} - {region}"));
    log("Build machine configuration: 4 cores, 8 GB".into());
    tokio::time::sleep(Duration::from_millis(350)).await;

    // Acquire the source: extract an UPLOADED ZIP, or `git clone` a repo.
    //
    // The checkout dir carries the BUILD ID, not just the millisecond stamp:
    // two concurrent builds of the same project used to collide on
    // `<project>-<now_ms()>` (both wake from the synchronized 350ms sleep above
    // on the same timer tick), extracting + installing into ONE shared dir —
    // two racing `unzip -o` processes then kill one build with "cannot create
    // …: No such file or directory" (exit 50) and the loser never reaches
    // ready (witnessed live 3x). The project component is `sanitize_tag`'d: a
    // tenant-controlled name is never a path component verbatim.
    let stamp = now_ms();
    let dir = deploy_root().join(format!("{}-{}-{}", checkout_tag(&project), stamp, bid));
    tokio::fs::create_dir_all(deploy_root()).await?;
    let branch = req.branch.clone().unwrap_or_default();

    let (commit, full_sha, commit_message) = if let Some(image) = req.image_ref.clone() {
        // Prebuilt OCI image: NO source to clone/extract or build. Make an empty build
        // dir so the shared tail (env injection, deploy_full) is unchanged; the image
        // is pulled + turned into a container manifest in `produce_manifest`.
        tokio::fs::create_dir_all(&dir).await?;
        log(format!("Deploying prebuilt image: {image}"));
        (
            "image".to_string(),
            String::new(),
            format!("Image: {image}"),
        )
    } else if let Some(zip_b64) = req.zip_b64.take() {
        // Drag-drop / zip upload: decode + extract instead of cloning, and synthesize
        // git-ish metadata so the rest of the pipeline is unchanged. `take()` drops the
        // base64 off the request so it's never logged/persisted past this point.
        let name = req.repo_url.trim_start_matches("upload://").to_string();
        log(format!(
            "Extracting uploaded archive: {}",
            if name.is_empty() {
                "archive.zip"
            } else {
                &name
            }
        ));
        let t0 = now_ms();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(zip_b64.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid upload encoding: {e}"))?;
        let files = extract_zip_into(&bytes, &dir).await?;
        log(format!(
            "Extracted {files} file(s) in {}ms",
            now_ms().saturating_sub(t0)
        ));
        // Retain the ORIGINAL archive durably so a later Redeploy can rebuild this
        // zip-uploaded project from source. Stored as a FILE beside the checkouts —
        // `gc_build_dirs` only reaps directories, so this survives build-dir GC and,
        // living under the durable deploy root, host reboots; the redeploy path
        // re-extracts it. Written via tmp+rename: two concurrent builds of the same
        // project (or a concurrent redeploy re-extracting it) otherwise truncate/
        // rewrite this shared file mid-read — a torn zip fails the OTHER build.
        let retained = retained_source_write_path(&project);
        let retained_tmp = deploy_root().join(format!("{}.{}.tmp", retained.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), bid));
        match tokio::fs::write(&retained_tmp, &bytes).await {
            Ok(()) => {
                if let Err(e) = tokio::fs::rename(&retained_tmp, &retained).await {
                    log(format!(
                        "(note) could not retain source archive for future redeploys: {e}"
                    ));
                }
            }
            Err(e) => log(format!(
                "(note) could not retain source archive for future redeploys: {e}"
            )),
        }
        (
            "upload".to_string(),
            String::new(),
            format!(
                "Uploaded {}",
                if name.is_empty() {
                    "archive.zip".into()
                } else {
                    name
                }
            ),
        )
    } else if req.repo_url.starts_with("upload://") {
        // REDEPLOY of a zip-uploaded project: no git remote to clone and the request
        // carries no fresh archive. Rebuild from RETAINED source on THIS node — the
        // durable `<tag>.src.zip` if present (clean original; `retained_source_path`
        // also falls back to the pre-durability /tmp locations), else a copy of the
        // prior build's on-disk checkout. The redeploy handler pins the rebuild to the
        // node that holds this source, so a missing source here is a real error.
        let name = req.repo_url.trim_start_matches("upload://").to_string();
        let retained = retained_source_path(&project);
        let t0 = now_ms();
        if let Some(retained) = retained {
            log(format!(
                "Redeploy: re-extracting retained source archive ({})",
                if name.is_empty() {
                    "archive.zip"
                } else {
                    &name
                }
            ));
            let bytes = tokio::fs::read(&retained).await?;
            let files = extract_zip_into(&bytes, &dir).await?;
            log(format!(
                "Extracted {files} file(s) in {}ms",
                now_ms().saturating_sub(t0)
            ));
        } else if let Some(src) = newest_deploy_dir(&project) {
            log("Redeploy: reusing retained source from the prior build".to_string());
            let (s, d) = (src.clone(), dir.clone());
            tokio::task::spawn_blocking(move || copy_dir_into(&s, &d))
                .await
                .map_err(|e| anyhow::anyhow!("source copy task failed: {e}"))??;
            log(format!(
                "Prepared source in {}ms",
                now_ms().saturating_sub(t0)
            ));
        } else {
            anyhow::bail!(
                "no retained source found for this uploaded project — re-upload the archive to deploy again"
            );
        }
        (
            "upload".to_string(),
            String::new(),
            format!(
                "Redeploy of {}",
                if name.is_empty() {
                    "uploaded archive".into()
                } else {
                    name
                }
            ),
        )
    } else {
        // A PR opened from a FORK has its branch/commit only on the fork's own
        // remote — the base repo (`req.repo_url`) never has it. `git_webhook` sets
        // `head_repo_url` for exactly that case; every other deploy path (push,
        // manual import, redeploy, non-fork PR) leaves it None and clones
        // `req.repo_url` as before. Project ownership/matching and every displayed
        // field (`Build.repo_url`, commit-status reporting below) stay on the BASE
        // repo — only the actual clone/fetch source changes here.
        let clone_source_url = req
            .head_repo_url
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| req.repo_url.clone());
        let short_repo = clone_source_url
            .trim_start_matches("https://")
            .trim_end_matches(".git");
        let pinned_commit = req.commit.clone().filter(|s| !s.is_empty());
        log(format!(
            "Cloning {short_repo} (Branch: {}, Commit: {})",
            if branch.is_empty() { "main" } else { &branch },
            pinned_commit
                .as_deref()
                .map(|s| s.chars().take(7).collect::<String>())
                .unwrap_or_else(|| "HEAD".into())
        ));
        let t0 = now_ms();
        // Credential for a PRIVATE repo: the deploy request's `git_token` — either
        // attached server-side by the dashboard from the user's connected GitHub
        // (interactive deploys), or a freshly minted GitHub App installation
        // token attached by `admin::git_webhook` (webhook-triggered deploys have
        // no user session, see `github_app_auth`) — else a node-level
        // `GITHUB_TOKEN`. It is injected ONLY into a local clone URL as
        // `x-access-token` basic auth — `req.repo_url` stays tokenless everywhere it
        // is logged/persisted, and the token is scrubbed out of any clone stderr.
        // Applied to `clone_source_url` (the fork, when this is a fork PR): the same
        // installation/token resolution as the base repo is reused verbatim (no new
        // auth flow) — it may not grant access to an arbitrary fork, but a public
        // fork still clones fine anonymously, and a rejected token falls back to the
        // anonymous retry below exactly as it does for the base repo today.
        let git_token: Option<String> = req
            .git_token
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()));
        let tokenized_url = git_token.as_ref().and_then(|t| {
            clone_source_url
                .strip_prefix("https://github.com/")
                .map(|rest| format!("https://x-access-token:{t}@github.com/{rest}"))
        });

        // Run `git clone <url>` into `dir`; returns (success, token-scrubbed stderr).
        // Never let git open an interactive credential prompt (GIT_TERMINAL_PROMPT=0 /
        // GIT_ASKPASS): this daemon has no controlling terminal, so a rejected/absent
        // credential must fail cleanly instead of crashing on /dev/tty.
        //
        // When `req.commit` names an exact SHA (a webhook-triggered deploy), a plain
        // `--depth 1 --branch` clone is not enough — under a rapid double-push the
        // branch tip may already have moved past the commit GitHub notified us
        // about, and a shallow branch clone simply does not contain that older
        // commit. Pin to it instead: `git init` + `git fetch --depth 1 <sha>` fetches
        // exactly that commit directly (GitHub, and any PAT/GitHub-App authenticated
        // host, serve an arbitrary reachable SHA on request — this is the same trick
        // GitHub Actions' own checkout action uses), then `checkout FETCH_HEAD` pins
        // to it, staying just as cheap as the branch-tip shallow clone in the common
        // case. If the remote rejects a direct SHA fetch (older/self-hosted git
        // servers without `uploadpack.allowReachableSHA1InWant`), fall back to a full
        // (unshallowed) clone of the branch followed by `checkout <sha>`, which is
        // guaranteed to contain the commit as long as it is reachable from that
        // branch. When `req.commit` is None (manual deploy, redeploy, import — no
        // specific commit to pin to), behavior is EXACTLY the prior shallow
        // branch-tip clone.
        let run_clone = |clone_url: String| {
            let (dir, branch, token, commit, ccloud, cbid) = (
                dir.clone(),
                branch.clone(),
                git_token.clone(),
                pinned_commit.clone(),
                cloud.clone(),
                bid.to_string(),
            );
            async move {
                let scrub = |raw: &[u8]| {
                    let mut s = String::from_utf8_lossy(raw).trim().to_string();
                    if let Some(t) = &token {
                        s = s.replace(t.as_str(), "***");
                    }
                    s
                };
                if let Some(sha) = commit {
                    // Fast path: fetch the exact commit directly into a fresh repo.
                    if tokio::fs::create_dir_all(&dir).await.is_ok() {
                        let mut init = Command::new("git");
                        init.env("GIT_TERMINAL_PROMPT", "0")
                            .env("GIT_ASKPASS", "/bin/echo");
                        init.arg("init").arg("-q").arg(&dir);
                        let init_ok = run_cancellable_output(&mut init, &ccloud, &cbid)
                            .await
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if init_ok {
                            let mut remote = Command::new("git");
                            remote
                                .env("GIT_TERMINAL_PROMPT", "0")
                                .env("GIT_ASKPASS", "/bin/echo");
                            remote
                                .arg("-C")
                                .arg(&dir)
                                .arg("remote")
                                .arg("add")
                                .arg("origin")
                                .arg(&clone_url);
                            let _ = run_cancellable_output(&mut remote, &ccloud, &cbid).await;

                            let mut fetch = Command::new("git");
                            fetch
                                .env("GIT_TERMINAL_PROMPT", "0")
                                .env("GIT_ASKPASS", "/bin/echo");
                            fetch
                                .arg("-C")
                                .arg(&dir)
                                .arg("fetch")
                                .arg("--depth")
                                .arg("1")
                                .arg("origin")
                                .arg(&sha);
                            match run_cancellable_output(&mut fetch, &ccloud, &cbid).await {
                                Ok(out) if out.status.success() => {
                                    let mut checkout = Command::new("git");
                                    checkout
                                        .env("GIT_TERMINAL_PROMPT", "0")
                                        .env("GIT_ASKPASS", "/bin/echo")
                                        .arg("-C")
                                        .arg(&dir)
                                        .arg("checkout")
                                        .arg("-q")
                                        .arg("FETCH_HEAD");
                                    match run_cancellable_output(&mut checkout, &ccloud, &cbid).await {
                                        Ok(cout) if cout.status.success() => {
                                            return (true, scrub(&cout.stderr));
                                        }
                                        Ok(cout) => {
                                            tracing::debug!(
                                                stderr = %scrub(&cout.stderr),
                                                "checkout FETCH_HEAD failed after SHA fetch, falling back to full clone + checkout"
                                            );
                                        }
                                        Err(e) => {
                                            tracing::debug!(error = %e, "checkout FETCH_HEAD failed after SHA fetch, falling back to full clone + checkout");
                                        }
                                    }
                                }
                                Ok(out) => {
                                    tracing::debug!(
                                        stderr = %scrub(&out.stderr),
                                        "git fetch by SHA failed, falling back to full clone + checkout"
                                    );
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "git fetch by SHA failed, falling back to full clone + checkout");
                                }
                            }
                        }
                    }
                    // Fallback: full (unshallowed) clone of the branch, then pin via checkout.
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    let mut cmd = Command::new("git");
                    cmd.env("GIT_TERMINAL_PROMPT", "0")
                        .env("GIT_ASKPASS", "/bin/echo");
                    cmd.arg("clone");
                    if !branch.is_empty() {
                        cmd.arg("--branch").arg(&branch);
                    }
                    cmd.arg(&clone_url).arg(&dir);
                    match run_cancellable_output(&mut cmd, &ccloud, &cbid).await {
                        Ok(out) if out.status.success() => {
                            let mut checkout = Command::new("git");
                            checkout
                                .env("GIT_TERMINAL_PROMPT", "0")
                                .env("GIT_ASKPASS", "/bin/echo")
                                .arg("-C")
                                .arg(&dir)
                                .arg("checkout")
                                .arg("-q")
                                .arg(&sha);
                            match run_cancellable_output(&mut checkout, &ccloud, &cbid).await {
                                Ok(cout) => (cout.status.success(), scrub(&cout.stderr)),
                                Err(e) => (false, format!("{e}")),
                            }
                        }
                        Ok(out) => (false, scrub(&out.stderr)),
                        Err(e) => (false, format!("{e}")),
                    }
                } else {
                    // No specific commit to pin to: exactly the prior behavior — a
                    // shallow clone of the branch tip.
                    let mut cmd = Command::new("git");
                    cmd.env("GIT_TERMINAL_PROMPT", "0")
                        .env("GIT_ASKPASS", "/bin/echo");
                    cmd.arg("clone").arg("--depth").arg("1");
                    if !branch.is_empty() {
                        cmd.arg("--branch").arg(&branch);
                    }
                    cmd.arg(&clone_url).arg(&dir);
                    match run_cancellable_output(&mut cmd, &ccloud, &cbid).await {
                        Ok(out) => (out.status.success(), scrub(&out.stderr)),
                        Err(e) => (false, format!("{e}")),
                    }
                }
            }
        };
        let auth_failed = |s: &str| {
            s.contains("could not read Username")
                || s.contains("Authentication failed")
                || s.contains("terminal prompts disabled")
                || s.contains("invalid username or password")
                // GitHub returns this (not a generic 401) when a token lacks access to a
                // repo — including private repos, which is exactly the fork-PR case where
                // the surrounding retry-anonymously fallback must still trigger.
                || s.contains("Repository not found")
        };

        let first_url = tokenized_url
            .clone()
            .unwrap_or_else(|| clone_source_url.clone());
        let used_token = tokenized_url.is_some();
        let (mut ok, mut stderr) = run_clone(first_url).await;
        // A stored token that is expired / lacks access must never break a repo that
        // would clone fine anonymously (public repos, token rotation): retry once
        // without the credential before surfacing an error.
        if !ok && used_token && auth_failed(&stderr) {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            log("Retrying clone without the stored GitHub credential…".into());
            let (ok2, stderr2) = run_clone(clone_source_url.clone()).await;
            ok = ok2;
            stderr = stderr2;
        }
        if !ok && cloud.build_cancels.is_cancelled(bid) {
            return Err(BuildCancelled.into());
        }
        anyhow::ensure!(
            ok,
            "{}",
            if auth_failed(&stderr) {
                if used_token {
                    format!(
                        "git clone failed: GitHub rejected the stored credential — it may be expired \
                         or lack access to this repository. Reconnect GitHub and redeploy. ({stderr})"
                    )
                } else {
                    format!(
                        "git clone failed: this repository is private or inaccessible over anonymous \
                         HTTPS. Connect GitHub on the Integrations page (or set GITHUB_TOKEN on the node) \
                         and redeploy — private repositories need a credential. ({stderr})"
                    )
                }
            } else {
                format!("git clone failed: {stderr}")
            }
        );
        // The token has done its job; drop it so no build/deploy record, gossip frame,
        // or displayed field constructed from `req` beyond this point can retain it.
        req.git_token = None;
        log(format!(
            "Cloning completed: {}ms",
            now_ms().saturating_sub(t0)
        ));
        let commit = run_git(&dir, &["rev-parse", "--short", "HEAD"])
            .await
            .unwrap_or_default();
        // Full SHA for GitHub commit-status reporting (the statuses API needs it).
        let full_sha = run_git(&dir, &["rev-parse", "HEAD"])
            .await
            .unwrap_or_else(|| commit.clone());
        let commit_message = run_git(&dir, &["log", "-1", "--pretty=%s"])
            .await
            .unwrap_or_default();
        // Best-effort "pending" check on the commit (no-op without GITHUB_TOKEN).
        {
            let (repo, sha) = (req.repo_url.clone(), full_sha.clone());
            tokio::spawn(async move {
                report_github_status(&repo, &sha, "pending", "", "Build in progress…").await;
            });
        }
        (commit, full_sha, commit_message)
    };
    let actual_branch = if branch.is_empty() {
        run_git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap_or_else(|| "main".into())
    } else {
        branch.clone()
    };
    cloud.builds.update(bid, |b| {
        b.commit = commit.clone();
        b.commit_message = commit_message.clone();
        b.branch = actual_branch.clone();
    });

    // Build from a subdirectory for monorepo templates (e.g. `examples/nextjs`).
    // Use the request's root_dir, falling back to the project's persisted one so
    // redeploys keep building the right subdirectory.
    let persisted_root = cloud.projects.root_dir_of(&project);
    let effective_root = req
        .root_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| Some(persisted_root.clone()).filter(|s| !s.trim().is_empty() && s != "./"));
    let build_dir = match effective_root
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(root) => {
            log(format!("Root directory: {root}"));
            dir.join(root)
        }
        None => dir.clone(),
    };
    anyhow::ensure!(
        build_dir.exists(),
        "root directory '{}' not found in repo",
        effective_root.unwrap_or_default()
    );

    // ---- Ignored Build Step (vercel.json `ignoreCommand`) ----
    // Vercel semantics: run the command in the project root; exit 0 => skip this
    // build entirely (no new deployment — the prior one keeps serving), non-zero
    // => continue. Lets a repo short-circuit commits that don't need a rebuild.
    if let Some(vc) = fluid_build::load_vercel_config(&build_dir) {
        if let Some(cmd) = vc
            .ignore_command
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            log(format!("Running Ignored Build Step: {cmd}"));
            if let Ok(st) = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .current_dir(&build_dir)
                .status()
                .await
            {
                if st.success() {
                    log(
                        "Ignored Build Step exited 0 — skipping this build (no changes to deploy)."
                            .into(),
                    );
                    cloud.builds.update(bid, |b| {
                        b.state = DeployState::Ready;
                        b.finished_ms = Some(now_ms());
                    });
                    return Ok(());
                }
                log(format!(
                    "Ignored Build Step exited {} — continuing build.",
                    st.code().unwrap_or(-1)
                ));
            }
        }
        // devCommand / bunVersion are recorded for parity but not executed: the
        // platform has no local dev server and manages the runtime itself.
        if let Some(dc) = vc
            .dev_command
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            log(format!(
                "vercel.json devCommand: {dc} (informational — not executed)"
            ));
        }
        if let Some(bv) = vc
            .bun_version
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            log(format!("vercel.json bunVersion: {bv} (informational)"));
        }
    }

    // Produce the deployment manifest. A build failure must NOT abort the
    // deploy — Vercel still records the deployment/project — so on error we fall
    // back to a "build failed" page and keep going (build state ends as Error).
    let mut build_failed = false;
    let mut manifest = match produce_manifest(
        cloud,
        bid,
        &dir,
        &build_dir,
        &project,
        &commit,
        first_deploy,
        req.use_cache,
        req.image_ref.as_deref(),
        req.image_port,
        req.image_protocol,
        req.image_memory.as_deref(),
        req.image_cpus.as_deref(),
        req.image_pids.unwrap_or(0),
        req.image_ports.clone(),
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            build_failed = true;
            log(format!("Build failed: {e}"));
            log(
                "Keeping the deployment — serving a build-status page so the project still exists."
                    .into(),
            );
            let index = build_dir.join("index.html");
            let _ = tokio::fs::create_dir_all(&build_dir).await;
            let _ = tokio::fs::write(
                &index,
                build_failed_page(&project, &commit, &e.to_string(), &req.repo_url),
            )
            .await;
            static_manifest(&project, ".")
        }
    };
    manifest.project = project.clone();

    // Map framework build features (redirects/rewrites/middleware/edge) onto the
    // deployment so the gateway honors them and the UI can surface them.
    if !build_failed {
        let feats = fluid_build::detect_features(&build_dir);
        if !feats.is_empty() {
            manifest.redirects = feats
                .redirects
                .iter()
                .map(|r| fluid_core::Redirect {
                    source: r.source.clone(),
                    destination: r.destination.clone(),
                    status: r.status,
                    has: vec![],
                    missing: vec![],
                })
                .collect();
            manifest.rewrites = feats
                .rewrites
                .iter()
                .map(|r| fluid_core::Rewrite {
                    source: r.source.clone(),
                    destination: r.destination.clone(),
                    has: vec![],
                    missing: vec![],
                })
                .collect();
            if let Some(mw) = &feats.middleware {
                manifest.middleware = Some(fluid_core::Middleware {
                    matcher: mw.matcher.clone(),
                    runtime: mw.runtime.clone(),
                });
            }
            // Mark edge-runtime functions so the service graph / overview can show them.
            if !feats.edge_functions.is_empty() {
                for f in manifest.functions.iter_mut() {
                    f.runtime = "edge".into();
                }
            }
            log(format!(
                "Mapped framework features: {} redirect(s), {} rewrite(s), middleware: {}, {} edge fn(s).",
                manifest.redirects.len(),
                manifest.rewrites.len(),
                manifest.middleware.is_some(),
                feats.edge_functions.len(),
            ));
        }
    }

    // Sync the fluid.json top-level `inference` block into project settings —
    // presence creates/updates the managed llama.cpp endpoint (see
    // inference.rs), absence tears it down. Parsed from the RAW file so it
    // works identically across every manifest-shape path (FDI, explicit
    // fluid.json, container) rather than only the paths that deserialize the
    // whole file into `Manifest`.
    {
        #[derive(serde::Deserialize)]
        struct InfWrap {
            #[serde(default)]
            inference: Option<crate::project_settings::InferenceSpec>,
        }
        let spec = match tokio::fs::read_to_string(build_dir.join("fluid.json")).await {
            Ok(txt) => serde_json::from_str::<InfWrap>(&txt)
                .map(|w| w.inference)
                .unwrap_or(None),
            Err(_) => None,
        };
        let current = cloud.projects.get(&manifest.project).inference;
        if current != spec {
            if let Some(s) = &spec {
                log(format!(
                    "Managed inference requested: model {} (pool: {}).",
                    s.model, s.pool
                ));
            } else if current.is_some() {
                log("Managed inference removed (no inference block in fluid.json).".into());
            }
            cloud.projects.set_inference(&manifest.project, spec);
        }
    }

    // Sync/merge the browser-replicated database opt-in (bn-storages-page-
    // browser-db-wiring). Unlike `inference` above, `browser_db` already lives
    // IN `fluid_core::Manifest` (parsed straight off fluid.json by
    // `Manifest::from_json`, the explicit-fluid.json branch of
    // `produce_manifest`), so `manifest.browser_db` here already reflects an
    // explicit repo-authored block for every manifest-shape path that goes
    // through it. Two directions, never fighting each other:
    //   * fluid.json declared a block -> mirror it into project settings (the
    //     `inference` read-side precedent) so the Storages page shows what's
    //     actually deployed even for a hand-edited fluid.json.
    //   * fluid.json declared NONE -> apply the dashboard-managed settings
    //     spec instead (the `FunctionSettings::gpu` OR precedent, applied to
    //     an `Option`: an explicit fluid.json block always wins over the
    //     UI-managed one). This is what lets the Storages page's "Deploy a
    //     replicated SQLite database" flow take effect with no git push.
    {
        let current = cloud.projects.get(&manifest.project).browser_db;
        if manifest.browser_db.is_some() {
            if current != manifest.browser_db {
                cloud
                    .projects
                    .set_browser_db(&manifest.project, manifest.browser_db.clone());
            }
        } else if let Some(settings_spec) = current {
            log(
                "Browser-replicated database: applying the dashboard-managed browser_db config (fluid.json declares none).".into(),
            );
            manifest.browser_db = Some(settings_spec);
        }
    }

    // Inject project env vars + function settings.
    let env = cloud.projects.env_map(&manifest.project);
    let fsettings = cloud.projects.get(&manifest.project).functions;
    if !env.is_empty() {
        log(format!("Loaded {} environment variable(s).", env.len()));
    }
    for f in manifest.functions.iter_mut() {
        for (k, v) in &env {
            f.env.insert(k.clone(), v.clone());
        }
        f.max_duration_secs = fsettings.default_max_duration_secs;
        f.vcpus = fsettings.vcpus.max(1);
        f.memory_mib = fsettings.memory_mib;
        // Serverless GPU: the project-level toggle marks every function; a
        // per-function fluid.json `gpu: true` (already parsed into the manifest)
        // is preserved — settings can only turn GPU ON, never strip a
        // function's own declared need.
        f.gpu = f.gpu || fsettings.gpu;
        // Fluid Compute (the project's `fluid_enabled` toggle): when ON (default),
        // one warm instance serves MANY concurrent requests (in-instance
        // concurrency — ideal for I/O-bound work like LLM/DB calls that sit idle
        // waiting) and at least one instance is kept warm to avoid cold starts.
        // When OFF, fall back to classic one-request-per-instance + scale-to-zero.
        if fsettings.fluid_enabled {
            f.max_concurrency = f.max_concurrency.max(10);
            f.min_instances = f.min_instances.max(1);
        } else {
            f.max_concurrency = 1;
            f.min_instances = 0;
        }
    }

    // ---- Merge vercel.json routing/headers/crons/images + per-function config ----
    // Applied AFTER project defaults so vercel.json per-function overrides win,
    // and its redirects/rewrites are evaluated before framework-derived ones.
    if !build_failed {
        if let Some(vc) = fluid_build::load_vercel_config(&build_dir) {
            apply_vercel_config(&mut manifest, &vc, &|s| log(s));
        }
    }

    // Ensure static deployments always have something to serve at "/". Some
    // repos (plain static, monorepos) have no index.html — generate a landing
    // page so the deployed URL returns 200 instead of 404.
    if manifest.functions.is_empty() {
        let static_dir = manifest.static_dir.clone().unwrap_or_else(|| ".".into());
        let base = if static_dir == "." {
            build_dir.clone()
        } else {
            build_dir.join(&static_dir)
        };
        let index = base.join("index.html");
        if !index.exists() {
            let _ = tokio::fs::create_dir_all(&base).await;
            let html = landing_page(&project, &commit, &commit_message, &req.repo_url);
            let _ = tokio::fs::write(&index, html).await;
            log("No index.html found — generated a default landing page.".into());
        }
    }

    log("Uploading build outputs…".into());
    log(format!(
        "Functions: {}, Static assets prepared.",
        manifest.functions.len()
    ));

    // ---- Classify production vs preview (Vercel's model) ----
    // A project's production branch is recorded on its first deploy (the imported
    // branch). Pushes to it are PRODUCTION; every other branch / PR is a PREVIEW.
    // An explicit target on the request (webhook PR events) overrides the branch.
    let mut prod_branch = cloud.projects.production_branch_of(&project);
    if prod_branch.is_empty() && !actual_branch.is_empty() {
        prod_branch = actual_branch.clone();
        cloud.projects.set_production_branch(&project, &prod_branch);
        log(format!("Production branch set to '{prod_branch}'."));
    }
    let is_production = match req.target.as_deref().map(str::trim) {
        Some("preview") => false,
        Some("production") => true,
        // No explicit target -> classify from the branch.
        _ => !actual_branch.is_empty() && actual_branch == prod_branch,
    };
    log(format!(
        "Target: {} (branch '{}' vs production branch '{}')",
        if is_production {
            "Production"
        } else {
            "Preview"
        },
        actual_branch,
        prod_branch
    ));

    // Runtime Cache wiring: expose the regional data cache to this deployment's
    // function cells via env. Scope isolates production vs preview per Vercel.
    // Cells reach the loopback admin endpoint (HIVE_RUNTIME_CACHE_URL override
    // for non-standard admin ports / isolated backends).
    {
        let rc_url = std::env::var("HIVE_RUNTIME_CACHE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8786/v1/runtime-cache".to_string());
        let rc_scope = format!(
            "{}:{}",
            project,
            if is_production {
                "production"
            } else {
                "preview"
            }
        );
        for f in manifest.functions.iter_mut() {
            f.env
                .insert("HIVE_RUNTIME_CACHE_URL".into(), rc_url.clone());
            f.env
                .insert("HIVE_RUNTIME_CACHE_SCOPE".into(), rc_scope.clone());
        }
    }

    // Register the routable deployment.
    let git = GitSource {
        repo_url: req.repo_url.clone(),
        branch: actual_branch,
        commit: commit.clone(),
        commit_message: commit_message.clone(),
    };
    // CONTAINER deployments (`__container__`/podman) now run on ANY node — the
    // Firecracker backend runs them via podman ON THE HOST (outside the microVM),
    // just like the mock backend. So no re-home is needed: the container builds
    // (podman build, below) and serves on whichever node it was placed on.
    let is_container = manifest.functions.iter().any(|f| f.runtime == "container");
    // Captured here (before `manifest` moves into `deploy_full` below) for the
    // multi-region-fanout stateful guard in the "Multi-region tail" block further
    // down. Either signal marks a single-writer service that must not be silently
    // fanned out to independent, non-synchronized regional replicas: `is_container`
    // (every container gets its own durable per-node volume — see
    // `container_volume_cfg` — matching the existing single-owner lease model in
    // `lease.rs`), or `FunctionConfig::needs_raw_proxy()` (gRPC/TCP/UDP — a
    // Postgres or Minecraft-style stateful protocol even when NOT containerized).
    let is_stateful = is_container || manifest.functions.iter().any(|f| f.needs_raw_proxy());

    // ---- Stateful fanout-replica guard (the remote sub-build side) ---------
    // The coordinator's two placement gates cannot cover the pure-remote fanout
    // path for a first-time Dockerfile/compose deploy: container-ness (and thus
    // statefulness) is unknowable before the build runs, so the initial gate ran
    // with stateful=false, and this sub-build was dispatched with
    // `no_fanout: true` — which skips both that gate and the post-build
    // multi-region tail gate on THIS node. `fanout_secondary` is the
    // coordinator's signal that this build is a NON-PRIMARY member of a
    // multi-target fanout; now that the build has run, `is_stateful` is finally
    // known, so this is the first (and only) point where "stateful replica in a
    // multi-region fanout" is detectable at all on this path. Decline to host:
    // every container gets a fresh, independent, per-node volume with no
    // data-sync or leader election (see `schedule::place`'s `stateful` doc), so
    // hosting here alongside the primary would silently fork state per region
    // (split-brain Postgres, diverging game-world saves). Collapse to the
    // primary region with a logged explanation instead of hard-failing — the
    // same degrade-with-a-warning convention as `schedule::place`'s
    // explicit-region stateful constraint (and the log line is mirrored into
    // the coordinator's build record, so the user sees exactly why only one
    // region hosts). Stateless multi-region fanout is untouched: `is_stateful`
    // is false for it, so every secondary proceeds to host as before.
    if is_stateful && req.fanout_secondary {
        log(
            "Stateful service detected (container volume / raw single-writer protocol) on a \
             secondary fanout target: declining to host an independent regional replica — no \
             data-sync or leader-election exists between fanout replicas, so hosting here would \
             silently fork this service's state per region (split-brain). The deploy is served \
             from the primary region only; to run this region instead, select it as the single \
             region in Function Settings."
                .into(),
        );
        cloud.builds.update(bid, |b| {
            b.state = DeployState::Ready;
            b.finished_ms = Some(now_ms());
        });
        return Ok(());
    }

    // Build-time bytecode-cache warm-up: precompile the server's bytecode INTO
    // the artifact so a fresh microVM's first hit skips parse/compile. Dispatches
    // per the SINGLE resolved runtime (`hive_core::Runtime`, replacing what used
    // to be four independent copies of argv-basename sniffing) — Node gets the
    // opaque `NODE_COMPILE_CACHE` V8 cache (source untouched); Bun gets a
    // STRUCTURALLY DIFFERENT build-time bundle+bytecode step that REWRITES the
    // function's start_cmd to point at the cached bundle (see
    // `warmup_bun_bytecode`'s doc comment for why the two mechanisms can't share
    // one code path). Best-effort either way. Must happen BEFORE deliver_build so
    // the cache/bundle is packed into the image.
    if !build_failed && !is_container {
        if let Some(node_fn) = manifest.functions.iter().find(|f| {
            hive_core::Runtime::resolve(&f.runtime, &f.start_cmd) == hive_core::Runtime::Node
        }) {
            let start_cmd = node_fn.start_cmd.clone();
            let warm_env = cloud.projects.env_map(&project);
            warmup_node_cache(&build_dir, &start_cmd, &warm_env, cloud, bid).await;
        }
        if let Some(idx) = manifest.functions.iter().position(|f| {
            hive_core::Runtime::resolve(&f.runtime, &f.start_cmd) == hive_core::Runtime::Bun
        }) {
            let start_cmd = manifest.functions[idx].start_cmd.clone();
            let rewritten = warmup_bun_bytecode(&build_dir, &start_cmd, cloud, bid).await;
            manifest.functions[idx].start_cmd = rewritten;
        }
    }

    // Browser-executable artifacts (browser-function-artifact-build-contract):
    // every function that opted in via fluid.json `functions[].browser` is
    // bundled into ONE deterministic QuickJS-compatible source, persisted
    // content-addressed on THIS node, and stamped onto the manifest as a
    // digest-only descriptor — the only thing the replicated deployment state
    // ever carries. An opted-in function that is NOT browser-eligible
    // (container/python/go runtime, TypeScript entry, Node/Bun/Deno API use,
    // unresolved host ops) FAILS THE BUILD loudly here: dropping the opt-in
    // silently would leave the function serving on the fleet path while
    // donors believe they are serving it — the exact pretend-every-function-
    // can-run-in-a-browser state this contract exists to remove. Deliberately
    // NOT packed into the deliver_build ext4: the artifact executes in
    // donors' browsers, not in the microVM, and carries no env/secrets.
    // AUTOMATIC browser eligibility (no fluid.json opt-in required): any JS/Bun
    // function whose own handler file exports `module.exports` is served in
    // browsers automatically — `infer_browser_entry` probes the `.browser.js`
    // convention AND the function's ordinary entry files (`<fn>.js`,
    // `handler.js`, `index.js`, …). "Automatic" no longer means "only a
    // dedicated .browser.js": it means "we found a handler-SHAPED entry that
    // survives the build gate". The `start_cmd` argv is still never parsed for
    // an entry (it may be `next start`/`npm start` with no JS file), and a
    // long-running SERVER entry is not shipped by accident — bundle()'s
    // forbidden-surface scan (require/import/process/Node APIs) and its new
    // handler-export check filter it out. Container/python/go/command runtimes
    // are excluded by construction. Crucially, a SYNTHESIZED policy that then
    // fails to bundle is SKIPPED silently (the function just serves the normal
    // fleet path) — only an EXPLICIT fluid.json opt-in still fails the build
    // loudly, because only there did the tenant assert the function IS
    // browser-eligible. Either way the DECISION is recorded on the function
    // (`browser_ineligible_reason`) so "not listed in the picker" always has a
    // sentence behind it instead of being indistinguishable from silence.
    if !build_failed {
        // A tenant-authored `browser_ineligible_reason` is meaningless input —
        // this field is a build VERDICT. Clear it before evaluating so a
        // fluid.json can never inject a fake (or falsely reassuring) reason,
        // the same server-derived discipline admission capabilities follow.
        for f in manifest.functions.iter_mut() {
            f.browser_ineligible_reason = None;
        }
        // DROPPED-OPT-IN GUARD. `produce_manifest` deserializes fluid.json into
        // a `Manifest` on exactly ONE of its five branches; the prebuilt-image,
        // compose, Dockerfile, and FDI branches synthesize `functions`
        // themselves and never carry `functions[].browser` through. Without
        // this check an explicit opt-in on any of those repos vanishes with no
        // error and no artifact, and the deployment goes Ready looking exactly
        // like one that never opted in — the reported "I have an opted-in
        // function yet it doesn't work". The contract has no warn-and-drop
        // branch, so name every lost function and fail before the record is
        // registered (the prior deployment keeps serving, same as any other
        // rejected opt-in).
        let declared = fluid_json_browser_optins(&build_dir).await;
        let dropped: Vec<String> = declared
            .into_iter()
            .filter(|name| {
                !manifest
                    .functions
                    .iter()
                    .any(|f| &f.name == name && f.browser.is_some())
            })
            .collect();
        if !dropped.is_empty() {
            let msg = format!(
                "Browser opt-in rejected — the deployment was NOT registered. fluid.json declares \
                 `functions[].browser` for {}, but this project builds through the {} path, which \
                 constructs its own function list and cannot carry a per-function browser opt-in. \
                 The functions this build produced are [{}]. Remove the `browser` block (a \
                 container/compose/prebuilt-image service can never run in a donor's browser), or \
                 deploy this function from a plain fluid.json project with a JS/Bun `start_cmd`.",
                dropped
                    .iter()
                    .map(|n| {
                        // A `functions[]` entry with no `name` still counts as a
                        // dropped opt-in (it can never match a synthesized
                        // function), but `""` reads as a bug in the message.
                        if n.is_empty() {
                            "an unnamed function entry".to_string()
                        } else {
                            format!("{n:?}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                if is_container { "container" } else { "framework-detected" },
                manifest
                    .functions
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            log(msg.clone());
            tracing::warn!(project = %project, dropped = ?dropped, "browser opt-in dropped by the manifest path");
            return Err(anyhow::anyhow!(msg));
        }
    }
    let mut auto_browser: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !build_failed {
        for f in manifest.functions.iter_mut() {
            if f.browser.is_some() {
                continue; // explicit opt-in — leave it exactly as authored
            }
            let runtime = hive_core::Runtime::resolve(&f.runtime, &f.start_cmd);
            if !matches!(
                runtime,
                hive_core::Runtime::Node | hive_core::Runtime::Bun
            ) {
                // container/python/go/command — never browser-eligible. Recorded
                // rather than skipped: this is the single most common reason a
                // ready deployment is missing from the picker.
                f.browser_ineligible_reason = Some(format!(
                    "runs as {} — only plain JS/Bun request→response handlers can run in a browser",
                    runtime.as_str()
                ));
                continue;
            }
            if let Some(entry) = infer_browser_entry(&build_dir, &f.name) {
                log(format!(
                    "Browser artifact ({}): auto-detected handler {entry:?} — serving in browsers \
                     automatically (no fluid.json browser opt-in needed).",
                    f.name
                ));
                f.browser = Some(fluid_core::BrowserPolicy {
                    entry,
                    ..Default::default()
                });
                auto_browser.insert(f.name.clone());
            } else {
                f.browser_ineligible_reason = Some(format!(
                    "no browser handler file found — looked for {}.browser.js, browser.js, {}.js, \
                     handler.js, index.js and main.js (.mjs/.cjs too) in the deployment root; add \
                     one that assigns its handler to module.exports",
                    f.name, f.name
                ));
            }
        }
    }

    let mut browser_bundles: Vec<(String, fluid_core::BrowserArtifact)> = Vec::new();
    if !build_failed {
        for f in manifest.functions.iter_mut() {
            if f.browser.is_none() {
                continue;
            }
            let synthesized = auto_browser.contains(&f.name);
            let bundled = match crate::browser_artifacts::bundle(&build_dir, f) {
                Ok(bundled) => bundled,
                Err(reason) if synthesized => {
                    // Auto mode never fails the build: the tenant did not opt
                    // in, so an ineligible auto-candidate simply serves the
                    // normal fleet path. Un-stamp the synthesized policy so it
                    // never rides the manifest.
                    log(format!(
                        "Browser artifact ({}): auto-detected handler is not browser-eligible, \
                         serving the normal fleet path instead — {reason}",
                        f.name
                    ));
                    tracing::info!(project = %project, function = %f.name, %reason, "auto browser artifact skipped (ineligible)");
                    f.browser = None;
                    // The verdict is kept even though the policy is not: this
                    // is the exact sentence the run-node picker shows for a
                    // ready-but-unlisted deployment.
                    f.browser_ineligible_reason = Some(reason);
                    continue;
                }
                Err(reason) => {
                    let msg = format!(
                        "Browser opt-in rejected — the deployment was NOT registered and the \
                         function stays browser-ineligible: {reason}"
                    );
                    log(msg.clone());
                    tracing::warn!(project = %project, function = %f.name, %reason, "browser artifact bundle rejected");
                    return Err(anyhow::anyhow!(msg));
                }
            };
            for note in &bundled.notes {
                log(format!("Browser artifact ({}): {note}", f.name));
            }
            if let Err(e) = crate::browser_artifacts::persist(&bundled.source, &bundled.descriptor).await {
                let msg = format!(
                    "Browser artifact ({}): could not persist to the content-addressed store: {e}",
                    f.name
                );
                log(msg.clone());
                return Err(anyhow::anyhow!(msg));
            }
            log(format!(
                "Browser artifact ({}): bundled {} bytes, source {}, policy {} (mode {:?}, {} ms, {} MiB, ops {:?}).",
                f.name,
                bundled.descriptor.source_bytes,
                &bundled.descriptor.source_digest[..12],
                &bundled.descriptor.policy_digest[..12],
                bundled.descriptor.mode,
                bundled.descriptor.timeout_ms,
                bundled.descriptor.memory_bytes / (1024 * 1024),
                bundled.descriptor.allowed_ops,
            ));
            browser_bundles.push((f.name.clone(), bundled.descriptor.clone()));
            f.browser_artifact = Some(bundled.descriptor);
            f.browser_ineligible_reason = None; // eligible: descriptor XOR reason
        }
    }

    // For an isolated backend (Firecracker), a serving microVM cannot see the
    // host build dir the mock backend serves from. Pack the build output into a
    // per-deployment artifact the cell mounts at /build, and point this
    // deployment's function pool at that image. No-op for the same-host mock.
    // SKIP for containers: they serve from the OCI image (podman), not a microVM
    // data disk, so there's nothing to deliver.
    if !build_failed && !is_container && cloud.gw.backend_name() == "firecracker" {
        let image = format!("dpl-{}", sanitize_tag(bid));
        match cloud.gw.deliver_build(&image, &build_dir).await {
            Ok(()) => {
                log("Delivered build output to a microVM image (Firecracker).".into());
                manifest.image = Some(image);
            }
            Err(e) => log(format!(
                "WARN: could not deliver build to microVM ({e}); serving may fail."
            )),
        }
    }

    // ---- Public raw-port allocation (TCP/UDP/gRPC ingress) -----------------
    // A raw-protocol service has no Host header to route on, so the shared
    // 80/443 gateway can't reach it — allocate (or, on a redeploy, RE-USE: the
    // claim is keyed by project/function/container-port/protocol, not by
    // deployment id) one public port per declared raw PortSpec, and stamp it
    // into the manifest BEFORE the record is registered/persisted so the
    // allocation rides the deployment record fleet-wide. Placed after the
    // stateful fanout-replica guard above so a declining secondary never
    // claims ports it will not serve. Allocation failure (range exhausted /
    // claim not persistable) degrades with a logged warning rather than
    // failing the whole deploy — HTTP-family routes still work.
    match crate::raw_ports::allocate_raw_ports_coordinated(cloud, &project, &mut manifest).await {
        Ok(ports) if !ports.is_empty() => log(format!(
            "Allocated public raw port(s): {} (stable across redeploys).",
            ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
        )),
        Ok(_) => {}
        Err(e) => log(format!(
            "WARN: could not allocate public raw port(s): {e} — raw TCP/UDP ingress is unavailable for this deployment."
        )),
    }

    // Capture vercel.json crons before the manifest is moved into the gateway —
    // they're registered (production only) after the deployment is live.
    let cron_specs = manifest.crons.clone();

    // Build-inversion veto (bn-launch-sequencing-rebuild-before-deploy): the
    // newer build marked this one superseded at `start_build`; honor it just
    // before the production flip so the newest push's record is the one that
    // lands, never merely the last-finishing build.
    if let Some(newer) = cloud.builds.get(bid).and_then(|b| b.superseded_by) {
        return Err(anyhow::anyhow!(
            "build superseded by newer build {newer} for project {project}; skipping the production flip"
        ));
    }

    // Tenant = the project's team; tags the deployment + every cell it spawns so
    // compute is partitioned and quota'd per team (same resolver billing/audit use).
    let tenant = cloud.projects.team_of(&manifest.project);
    let info = cloud.gw.deploy_full(
        build_dir.to_string_lossy().into_owned(),
        manifest,
        req.creator.clone().unwrap_or_else(|| "you".into()),
        Some(git),
        is_production,
        if build_failed {
            DeployState::Error
        } else {
            DeployState::Ready
        },
        tenant,
    );

    // Record deployment ownership of each browser artifact now that the
    // deployment id exists (`deploy_full` mints it). The bytes are already
    // persisted; ownership is bookkeeping — the GC keep-set derives from live
    // deployment records, so a failed write here is a WARN, never fatal.
    for (function, descriptor) in &browser_bundles {
        if let Err(e) = crate::browser_artifacts::add_owner(&descriptor.policy_digest, &info.id.0).await {
            log(format!(
                "WARN: browser artifact ({function}) ownership was not recorded ({e}); the GC keep-set still protects it."
            ));
        }
    }

    // Register `vercel.json` crons against the PRODUCTION deployment (Vercel only
    // runs crons in production). Replaces this project's prior config-sourced jobs
    // so redeploys don't accumulate duplicates; manual jobs are untouched. Crons
    // hit the project's production alias, so they always target current prod.
    if !build_failed && is_production {
        let jobs: Vec<hive_edge::CronJob> = cron_specs
            .iter()
            .enumerate()
            .map(|(i, c)| hive_edge::CronJob {
                id: format!("vc-{}-{}", sanitize_tag(&project), i),
                name: format!("vercel.json {}", c.path),
                // Vercel uses 5-field expressions; the scheduler is 6-field (with
                // seconds) — prepend a 0-second field when needed.
                schedule: to_six_field_cron(&c.schedule),
                deployment: project.clone(),
                path: c.path.clone(),
                enabled: true,
                last_run_ms: None,
                next_run_ms: None,
                runs: 0,
                source: "vercel.json".into(),
                tenant: crate::admin::norm(&cloud.projects.team_of(&project)).to_string(),
            })
            .collect();
        let n = cloud.cron.set_source_jobs(&project, "vercel.json", jobs);
        if !cron_specs.is_empty() {
            log(format!("Registered {n} cron job(s) from vercel.json."));
        }
    }

    // Ingest any Vercel WDK manifest the app emitted (`.well-known/workflow/v1/
    // manifest.json`) so its workflows + step graphs appear in the Workflows tab
    // and render on the canvas. Best-effort: a non-WDK app simply has none.
    let ingested = ingest_workflow_manifest(cloud, &info.project, &build_dir).await;
    if ingested > 0 {
        log(format!(
            "Detected Vercel WDK: registered {ingested} workflow(s) for the Workflows tab."
        ));
    }

    // Managed World auto-wiring: any project detected to use the Vercel
    // Workflow SDK (JS/TS via the .well-known manifest just ingested above, or
    // Python via a vercel.json experimentalServices __wkf_* worker) gets BOTH
    // halves of hive's own native World -- Queue (this dispatcher) and Storage
    // (a real provisioned Redis, same provision() path as a database created
    // from the dashboard) -- wired in by DEFAULT, unless it already brought
    // its own Upstash/Redis world config (BYO opt-out) or sets fluid.json
    // `{"workflow":{"world":"external"}}`. Persisted via put_env (same
    // mechanism/timing as every other project env var -- takes effect
    // starting the NEXT deploy, since this build's function manifest is
    // already finalized by this point in the pipeline). Idempotent across
    // redeploys: once Storage is provisioned, apply_db_egress sets
    // UPSTASH_REDIS_REST_URL, which workflow_world_opted_out treats as BYO on
    // every later deploy, so this block runs at most once per project.
    // Deliberately does NOT set WORKFLOW_TARGET_WORLD: no
    // @open-workflow/world-hive-equivalent package is published for tenant
    // apps to import yet, and forcing that env var without a resolvable
    // module would break the app rather than help it -- what's wired now
    // makes both backing services ready the moment a compatible World
    // package is present, and world.rs's dashboard reader already falls back
    // to UPSTASH_REDIS_REST_URL/_TOKEN so the Workflows tab lights up
    // immediately once Storage finishes provisioning, independent of the
    // World package.
    {
        let py_wdk = crate::world_queue::vercel_json_declares_workflow_worker(&build_dir);
        if (ingested > 0 || py_wdk)
            && !crate::world_queue::workflow_world_opted_out(cloud, &info.project, &build_dir)
        {
            let queue_url = std::env::var("HIVE_QUEUE_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:8786".to_string());
            let team = crate::admin::norm(&cloud.projects.team_of(&info.project)).to_string();
            if let Ok(queue_token) = crate::auth::issue(
                "world-queue-client",
                &team,
                "service",
                false,
                365 * 24 * 3600,
            ) {
                cloud.projects.put_env(
                    &info.project,
                    crate::project_settings::EnvVar {
                        key: "HIVE_QUEUE_ENDPOINT".into(),
                        value: queue_url,
                        target: "all".into(),
                        sensitive: false,
                        updated_ms: now_ms(),
                    },
                );
                cloud.projects.put_env(
                    &info.project,
                    crate::project_settings::EnvVar {
                        key: "HIVE_QUEUE_TOKEN".into(),
                        value: queue_token,
                        target: "all".into(),
                        sensitive: true,
                        updated_ms: now_ms(),
                    },
                );
            }
            let req = crate::databases::ProvisionReq {
                name: "workflow-storage".into(),
                project: info.project.clone(),
                team,
                kind: crate::databases::DbKind::Redis,
                region: Some(cloud.region.clone()),
                provider: None,
                replicas: Vec::new(),
            };
            let cloud_ready = cloud.clone();
            crate::databases::provision(
                cloud.databases.clone(),
                cloud.region.clone(),
                req,
                cloud.db_domain.clone(),
                cloud.node_name.clone(),
                cloud.api_base(),
                move |d| {
                    if matches!(d.status, crate::databases::DbStatus::Ready) {
                        crate::admin::apply_db_egress(&cloud_ready, &d);
                    }
                    crate::persist::persist(&cloud_ready);
                },
            );
            log(format!(
                "Detected Vercel Workflow SDK ({}): auto-wired hive's managed World -- Queue (HIVE_QUEUE_ENDPOINT/_TOKEN) now, Storage (a provisioned Redis, UPSTASH_REDIS_REST_URL/_TOKEN) finishing in the background -- both active from the next deploy.",
                if ingested > 0 { "JS/TS" } else { "Python" }
            ));
        }
    }

    if build_failed {
        log(format!(
            "Deployment created (build failed). Aliased to {}",
            info.alias
        ));
    } else {
        log(format!("Deployment ready. Aliased to {}", info.alias));
    }
    cloud.builds.update(bid, |b| {
        b.state = if build_failed {
            DeployState::Error
        } else {
            DeployState::Ready
        };
        b.finished_ms = Some(now_ms());
        b.deployment_id = Some(info.id.to_string());
        b.alias = Some(info.alias.clone());
    });

    // Issue #2: derive the intelligent service graph ASYNC, off the deploy path. It
    // reads the checked-out repo (kept for live deployments), detects the framework,
    // scans consumed deps / monorepo packages / bundled front+back / databases, and
    // records env var NAMES only. Never blocks the deploy; best-effort. Runs even for
    // a FAILED build — the source tree is present, so the graph is still derivable.
    {
        let cloud2 = cloud.clone();
        let bd = build_dir.clone();
        let proj = info.project.clone();
        let dep_id = info.id.to_string();
        tokio::spawn(async move {
            let fw = fluid_build::detect(&bd);
            let (fw_slug, fw_name) = (fw.slug.to_string(), fw.name.to_string());
            let is_container = bd.join("Dockerfile").exists()
                || bd.join("Containerfile").exists()
                || crate::compose::compose_file(&bd).is_some();
            let bd2 = bd.clone();
            let scan = match tokio::task::spawn_blocking(move || {
                crate::svcgraph::scan_dir(&bd2, &fw_slug, &fw_name, is_container)
            })
            .await
            {
                Ok(s) => s,
                Err(_) => return,
            };
            let env_keys: Vec<String> = cloud2
                .projects
                .get_masked(&proj)
                .env
                .iter()
                .map(|e| e.key.clone())
                .collect();
            let graph = crate::svcgraph::build_graph(&proj, &dep_id, &scan, &env_keys);
            let n = graph.nodes.len();
            cloud2.svcgraph.put(graph);
            crate::persist::persist(&cloud2);
            tracing::info!(project = %proj, deployment = %dep_id, nodes = n, "service graph computed");
        });
    }

    let ev = cloud.event(
        &cloud.region,
        "DEPLOY",
        &info.alias,
        "/",
        200,
        "deploy",
        &format!("git {}", req.repo_url),
    );
    cloud.record(ev);
    cloud.audit.record(
        &cloud.projects.team_of(&info.project),
        &req.creator.clone().unwrap_or_else(|| "you".into()),
        if build_failed {
            "create_failed"
        } else {
            "create"
        },
        "deployment",
        &info.id.to_string(),
        &format!("{} → {}", info.project, info.alias),
    );
    crate::persist::persist(cloud);
    // Best-effort final GitHub commit status (success/failure). No-op without a
    // GITHUB_TOKEN; points the check at the live deployment URL.
    {
        let (repo, sha) = (req.repo_url.clone(), full_sha.clone());
        let url = cloud.deploy_url(&info.alias);
        let (state, desc) = if build_failed {
            ("failure", "Build failed")
        } else if is_production {
            ("success", "Production deployment ready")
        } else {
            ("success", "Preview deployment ready")
        };
        let (state, desc) = (state.to_string(), desc.to_string());
        tokio::spawn(async move {
            report_github_status(&repo, &sha, &state, &url, &desc).await;
        });
    }
    crate::webhooks::dispatch(
        &cloud.webhooks,
        &info.project,
        if is_production {
            "deployment.promoted"
        } else {
            "deployment.ready"
        },
        serde_json::json!({
            "id": info.id.to_string(),
            "project": info.project,
            "url": cloud.deploy_url(&info.alias),
            "state": "ready",
            "production": is_production,
            "target": if is_production { "production" } else { "preview" },
            "commit": commit,
        }),
    );

    // Multi-region tail: this node was a selected target AND hosted the build, so
    // also replicate the deploy to any OTHER selected region node(s), then drop
    // the project from nodes that are no longer targets. Only on a clean build.
    if !req.no_fanout && !build_failed {
        let regions = cloud.projects.get(&project).functions.regions;
        // `stateful = is_stateful` (captured above, before `manifest` moved into
        // `deploy_full`): this is THE fanout hazard site — a container built and
        // hosted here would otherwise be replicated to every other selected region
        // with a brand-new, independent, non-synced volume. See
        // `schedule::place`'s `stateful` doc.
        let needs_gpu = cloud.projects.get(&project).functions.gpu;
        let targets = crate::schedule::place(cloud, &regions, false, is_stateful, needs_gpu);
        if targets
            .iter()
            .any(|t| t.admin.is_none() && t.iroh.is_none())
        {
            let remote: Vec<crate::schedule::Target> = targets
                .iter()
                .filter(|t| t.admin.is_some() || t.iroh.is_some())
                .cloned()
                .collect();
            if !remote.is_empty() {
                // `primary_first: false` — THIS node hosted the build (it is the
                // primary), so every remote here is an extra-region secondary.
                // Defense-in-depth: for a stateful deploy `place` above already
                // constrained the targets to one region, so remotes only exist
                // for stateless deploys, where the secondary flag is inert.
                let _ = fanout_remote(cloud, bid, &req, &project, &remote, false).await;
            }
            let names: Vec<String> = targets.iter().map(|t| t.node.clone()).collect();
            cleanup_non_targets(cloud, &project, &names).await;
        }
    }
    Ok(())
}

/// Dispatch a per-target deploy to each remote target's admin and MIRROR its
/// build into this coordinator build record (so the dashboard's existing build
/// page streams the real, remote build log). Returns true if every target ended
/// in a Ready state. Each dispatched deploy carries `no_fanout:true` so the
/// target just builds + hosts (no recursion), the project's current env (so the
/// target has it even on a redeploy), and the owning team header.
///
/// `primary_first`: whether `remote[0]` is the deploy's designated PRIMARY
/// host (true on the pure-remote placement branch, where the target list IS
/// the whole placement, and on a single-host pinned redeploy) or the primary
/// already lives elsewhere (false on the multi-region tail, where THIS
/// coordinator hosted the build and every remote is an extra region). Every
/// non-primary target gets `fanout_secondary: true` stamped on its request —
/// the signal `run_build`'s stateful fanout-replica guard needs, since a
/// `no_fanout` sub-build skips both coordinator-side placement gates and
/// otherwise has no way to know it is one of N>1 independent regions.
async fn fanout_remote(
    cloud: &Arc<CloudState>,
    bid: &str,
    req: &GitDeployRequest,
    project: &str,
    remote: &[crate::schedule::Target],
    primary_first: bool,
) -> bool {
    let log = |s: String| cloud.builds.log(bid, s);
    let team = cloud.projects.team_of(project);
    let env = cloud.projects.env_map(project);
    // Forward the user's configured build settings + compute tier so the target
    // builds identically to the coordinator (not just from auto-detect).
    let build_config = cloud
        .projects
        .get_if_set(project)
        .map(|s| s.build)
        .and_then(|b| serde_json::to_value(b).ok());
    let function_settings = serde_json::to_value(cloud.projects.get(project).functions).ok();
    let mut all_ok = true;
    for (idx, t) in remote.iter().enumerate() {
        let mut dreq = req.clone();
        dreq.no_fanout = true;
        // Everyone but the designated primary is a secondary replica — see this
        // fn's doc + the stateful fanout-replica guard in `run_build`.
        dreq.fanout_secondary = !(primary_first && idx == 0);
        dreq.project = Some(project.to_string());
        dreq.env = Some(env.clone()); // carry env so a redeploy isn't env-less on the target
        dreq.build_config = build_config.clone();
        dreq.function_settings = function_settings.clone();
        let route = if t.admin.is_some() { "http" } else { "iroh" };
        log(format!("→ {}: dispatching deploy (via {route})", t.node));
        // Dispatch over HTTP admin (preferred) OR the iroh mesh (NAT'd coordinator →
        // FC nodes, the SSH tunnels are gone). Both return `{ "build_id": ... }`.
        // Each transport is attempted in turn rather than one being chosen and
        // its failure ending the deploy. HTTP admin is preferred (cheaper, and
        // it carries the same signed delegation token), iroh is the fallback —
        // and vice versa when only iroh exists. `attempt_failures` collects the
        // REAL reason from each so a total failure names what actually went
        // wrong; the previous code discarded it and logged a bare
        // "iroh dispatch failed", which said nothing about whether the peer was
        // unreachable, the request timed out, or the response was unparseable.
        let mut attempt_failures: Vec<String> = Vec::new();
        let resp_json: Option<serde_json::Value> = if let Some(admin) = &t.admin {
            // `x-hive-team` alone is only trusted by the target's `tenant()`
            // resolver in UNENFORCED/dev mode — under JWT enforcement (every
            // production node) a request with no Authorization bearer resolves
            // to ANON_TENANT regardless of this header, silently losing the
            // project's real team on the target (or, prior to the UNTAGGED_TENANT
            // fix, stamping it into the target's own personal namespace). Attach
            // the SAME short-lived signed delegation token the iroh path uses
            // (`mesh_team_qs`) as a real Bearer credential so this HTTP fanout
            // authenticates identically to a normal request.
            let mut rb = cloud
                .http
                .post(format!("{admin}/v1/git/deploy"))
                .header("x-hive-team", team.clone());
            if crate::auth::enforced() {
                if let Ok(tok) = crate::auth::issue("mesh-internal", &team, "service", false, 60) {
                    rb = rb.bearer_auth(tok);
                }
            }
            match rb.json(&dreq).timeout(Duration::from_secs(15)).send().await {
                Ok(r) => r.json::<serde_json::Value>().await.ok(),
                Err(e) => {
                    attempt_failures.push(format!("http: {e}"));
                    None
                }
            }
        } else {
            None
        };
        // Fall back to the mesh when HTTP produced nothing (never attempted, or
        // attempted and failed). This is the path that matters on nodes whose
        // 8786/8787 is firewalled off — healthy over iroh, unreachable over HTTP.
        let resp_json: Option<serde_json::Value> = match resp_json {
            Some(v) => Some(v),
            None if t.iroh.is_some() => {
                let (id, addr) = t.iroh.as_ref().expect("checked is_some");
                if t.admin.is_some() {
                    log(format!("→ {}: HTTP dispatch failed, retrying via iroh", t.node));
                }
                let body = serde_json::to_vec(&dreq).unwrap_or_default();
                let path = format!("/v1/git/deploy?{}", crate::admin::mesh_team_qs(&team));
                match crate::gossip::request_to(
                    cloud,
                    id,
                    addr,
                    hive_p2p::GOSSIP_POST,
                    &path,
                    &body,
                    20,
                )
                .await
                {
                    Some(b) => match serde_json::from_slice(&b) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            attempt_failures.push(format!("iroh: unparseable reply ({e})"));
                            None
                        }
                    },
                    None => {
                        attempt_failures
                            .push("iroh: no reply (peer unreachable over the mesh, or timed out after 20s)".into());
                        None
                    }
                }
            }
            None => None,
        };
        let Some(resp_json) = resp_json else {
            if attempt_failures.is_empty() {
                // Neither transport was even available: the node advertised no
                // admin URL and no iroh address, so there was nothing to try.
                attempt_failures.push("no dispatch route (node advertises neither an admin URL nor an iroh address)".into());
            }
            log(format!(
                "✗ {}: deploy dispatch failed — {}",
                t.node,
                attempt_failures.join("; ")
            ));
            all_ok = false;
            continue;
        };
        let resp_json = Some(resp_json);
        let target_bid = resp_json
            .as_ref()
            .and_then(|v| v.get("build_id").and_then(|x| x.as_str()).map(String::from));
        let Some(target_bid) = target_bid else {
            let err = resp_json
                .as_ref()
                .and_then(|v| v.get("error").and_then(|x| x.as_str()))
                .unwrap_or("no build id returned");
            log(format!("✗ {}: {err}", t.node));
            all_ok = false;
            continue;
        };
        // This target is the build's designated PRIMARY (see the doc above) —
        // publish it as the cancel mirror so `cancel_build` on THIS (mirror)
        // build id reaches the REAL process, which runs on `t`, not here.
        // Secondaries are extras a cancel doesn't need to chase.
        if !dreq.fanout_secondary {
            cloud.build_cancels.set_mirror(
                bid,
                MirrorTarget {
                    admin: t.admin.clone(),
                    iroh: t.iroh.clone(),
                    target_bid: target_bid.clone(),
                },
            );
        }
        let ok = mirror_remote_build(cloud, bid, t, &target_bid, &t.node).await;
        if !ok {
            all_ok = false;
        } else if let Some(admin) = &t.admin {
            // Sync the host's auto-detected Build settings back to THIS coordinator
            // so the dashboard (which reads settings here) shows the framework +
            // commands that were actually used (Issue #3). Only fill fields the
            // user hasn't explicitly set, so manual overrides are never clobbered.
            // HTTP targets only (non-critical; iroh targets skip it).
            if let Some(v) = cloud
                .http
                .get(format!("{admin}/v1/projects/{project}/settings"))
                .header("x-hive-team", team.clone())
                .timeout(Duration::from_secs(8))
                .send()
                .await
                .ok()
            {
                if let Ok(s) = v.json::<serde_json::Value>().await {
                    if let Some(rb) = s.get("build") {
                        if let Ok(remote_bc) = serde_json::from_value::<
                            crate::project_settings::BuildConfig,
                        >(rb.clone())
                        {
                            let mut cur = cloud
                                .projects
                                .get_if_set(project)
                                .map(|s| s.build)
                                .unwrap_or_default();
                            let mut changed = false;
                            for (cf, rf) in [
                                (&mut cur.framework, &remote_bc.framework),
                                (&mut cur.install_command, &remote_bc.install_command),
                                (&mut cur.build_command, &remote_bc.build_command),
                                (&mut cur.output_dir, &remote_bc.output_dir),
                            ] {
                                if cf.trim().is_empty() && !rf.trim().is_empty() {
                                    *cf = rf.clone();
                                    changed = true;
                                }
                            }
                            if changed {
                                cloud.projects.set_build(project, cur);
                                crate::persist::persist(cloud);
                            }
                        }
                    }
                }
            }
        }
    }
    all_ok
}

/// Poll a remote node's `/v1/builds/{id}` and stream NEW log lines into this
/// build record (prefixed with the node name) until it reaches a terminal state
/// or times out. On success, copies the remote deployment's id + alias onto this
/// build record so the dashboard shows the live URL. Returns true iff Ready.
async fn mirror_remote_build(
    cloud: &Arc<CloudState>,
    bid: &str,
    target: &crate::schedule::Target,
    target_bid: &str,
    node: &str,
) -> bool {
    let mut mirrored = 0usize;
    let mut polls_failed = 0usize;
    let deadline = now_ms() + 10 * 60 * 1000; // 10 min cap
    // AUTH FOR THE POLL, not just the dispatch. `/v1/builds/:id` is
    // team-scoped (`admin::build_owned_by`) and this poll carried NEITHER
    // `?team=` nor `?tok=`, so on the RECEIVING node `team_claims`/
    // `team_headers` (gossip.rs) derived nothing, `build_get` computed the
    // anonymous tenant, `build_owned_by` never matched the build's real
    // team, and every poll 404'd — INCLUDING every poll against a target
    // build that had already finished. Verified live: 400/400 polls failed
    // for a target build that reached `ready` in under 3 seconds, on a
    // reachable node, every single time. This is not a mesh-health symptom;
    // it silently broke remote-build status mirroring for every
    // fanout-placed deployment on the whole fleet, always burning the full
    // 10-minute deadline regardless of how fast the actual remote build was.
    // `mesh_team_qs` is the SAME delegation-token minting already used for
    // every other mesh-internal proxied read (`fetch_from_host` and
    // friends) — the coordinator knows the real owning team from its own
    // local build record, so it can assert it the same way.
    let team = cloud
        .builds
        .get(bid)
        .map(|b| cloud.projects.team_of(&b.project))
        .unwrap_or_default();
    let team_qs = crate::admin::mesh_team_qs(&team);
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        // `cancel_build` already fired the kill (local process group or, for a
        // primary target, a direct remote cancel dispatch — see
        // `BuildCancelRegistry::cancel`) and stamped the coordinator's own
        // record `Cancelled` BEFORE calling it — this loop just needs to stop
        // polling promptly instead of riding out the full 10-minute deadline.
        if cloud.build_cancels.is_cancelled(bid) {
            cloud
                .builds
                .log(bid, format!("{node}: mirror stopped (build cancelled)"));
            return false;
        }
        // Poll the target's build over the SAME transport we dispatched on.
        let v: Option<serde_json::Value> = if let Some(admin) = &target.admin {
            let sep = if team_qs.is_empty() { "" } else { "?" };
            match cloud
                .http
                .get(format!("{admin}/v1/builds/{target_bid}{sep}{team_qs}"))
                .timeout(Duration::from_secs(8))
                .send()
                .await
                .ok()
            {
                Some(r) => r.json::<serde_json::Value>().await.ok(),
                None => None,
            }
        } else if let Some((id, addr)) = &target.iroh {
            // 20s, matching the DISPATCH timeout above. This poll used to get 8s,
            // which is a shorter budget than the request that successfully placed
            // the deploy in the first place — an iroh dial that has to fall back
            // through a relay can exceed it, and then every single poll returns
            // None while the remote build has actually already finished.
            let sep = if team_qs.is_empty() { "" } else { "?" };
            crate::gossip::request_to(
                cloud,
                id,
                addr,
                hive_p2p::GOSSIP_GET,
                &format!("/v1/builds/{target_bid}{sep}{team_qs}"),
                &[],
                20,
            )
            .await
            .and_then(|b| serde_json::from_slice(&b).ok())
        } else {
            None
        };
        let Some(v) = v else {
            // A failed poll used to be SILENT (request_to just returns None), so a
            // build stuck at "Building" for a deploy that had really succeeded gave
            // the next reader nothing to go on — it took reading the TARGET's own
            // build record to discover it was `ready` all along. Log the first
            // failure and then every ~30s so the transport, not the app, is
            // implicated immediately.
            polls_failed += 1;
            if polls_failed == 1 || polls_failed % 20 == 0 {
                tracing::warn!(
                    node = %node, build = %bid, target_build = %target_bid,
                    transport = if target.admin.is_some() { "http" } else { "iroh" },
                    polls_failed,
                    "cannot read the remote build state — mirrored deploy status will stall"
                );
            }
            if now_ms() > deadline {
                cloud.builds.log(bid, format!("✗ {node}: lost contact with remote build after {polls_failed} failed polls"));
                return false;
            }
            continue;
        };
        polls_failed = 0;
        // Stream any log lines we haven't mirrored yet.
        if let Some(lines) = v.get("lines").and_then(|x| x.as_array()) {
            for line in lines.iter().skip(mirrored) {
                if let Some(text) = line.get("line").and_then(|x| x.as_str()) {
                    cloud.builds.log(bid, format!("[{node}] {text}"));
                }
            }
            mirrored = lines.len();
        }
        let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("");
        if state.eq_ignore_ascii_case("ready") {
            if let Some(dep) = v.get("deployment_id").and_then(|x| x.as_str()) {
                let dep = dep.to_string();
                let alias = v.get("alias").and_then(|x| x.as_str()).map(String::from);
                cloud.builds.update(bid, |b| {
                    b.deployment_id = Some(dep.clone());
                    if let Some(a) = &alias {
                        b.alias = Some(a.clone());
                    }
                });
            }
            cloud.builds.log(bid, format!("✓ {node}: deployment ready"));
            return true;
        }
        if state.eq_ignore_ascii_case("error") {
            // Link the FAILED remote build to its deployment too (the remote
            // sets deployment_id on both success and failure): without this,
            // the coordinator's mirror record was unlinked on failure, so
            // /v1/deployments/:id/build could never answer locally for a
            // failed fanout deploy and the detail page showed no logs at all —
            // exactly when the user most needs them.
            if let Some(dep) = v.get("deployment_id").and_then(|x| x.as_str()) {
                let dep = dep.to_string();
                cloud.builds.update(bid, |b| {
                    b.deployment_id = Some(dep.clone());
                });
            }
            cloud.builds.log(bid, format!("✗ {node}: build failed"));
            return false;
        }
        if now_ms() > deadline {
            cloud
                .builds
                .log(bid, format!("✗ {node}: remote build timed out"));
            return false;
        }
    }
}

/// After placing a project on its target node(s), tell every OTHER node that
/// still hosts it to delete it — so changing regions RELOCATES the deployment
/// rather than leaving stale copies. Best-effort; never fails the deploy.
async fn cleanup_non_targets(cloud: &Arc<CloudState>, project: &str, target_names: &[String]) {
    // SELF cleanup first: peer_routes only ever lists PEERS, so a stale copy on
    // THIS (coordinator) node would otherwise survive a relocation — wasting disk,
    // keeping it a lease candidate, and inflating the serving count. If this node
    // isn't a chosen target but still hosts the project, drop the local
    // DEPLOYMENTS (gateway) — but keep the project SETTINGS, since the coordinator
    // holds the authoritative env/build config for future redeploys.
    let me = cloud.node_name.clone();
    if !target_names.iter().any(|t| t == &me) {
        let hosts_locally = cloud.gw.served_hosts().iter().any(|h| {
            let sub = h.split('.').next().unwrap_or(h);
            sub == project || sub.starts_with(&format!("{project}-"))
        });
        if hosts_locally {
            let removed = cloud.gw.remove_project(project).await;
            tracing::info!(
                project,
                removed = removed.len(),
                "relocation: removed stale local copy from coordinator"
            );
            crate::persist::persist(cloud);
        }
    }
    // Relocation cleanup: BROADCAST a single-hop delete to every healthy peer
    // EXCEPT the chosen targets, over HTTP admin OR the iroh mesh. (Previously
    // derived a "hosting" set from gossiped routes and dispatched HTTP-only —
    // sparse post-restart gossip + empty node_admins for FC nodes meant stale
    // copies silently survived relocations.) The receiving arm is team-checked
    // and idempotent, so non-hosting peers are a cheap no-op.
    let team = cloud.projects.team_of(project);
    let peers: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.healthy)
        .map(|n| n.name)
        .collect();
    for node in peers {
        if target_names.iter().any(|t| t == &node) {
            continue; // keep the chosen targets
        }
        crate::admin::dispatch_project_delete(cloud, &node, project, &team).await;
    }
}

/// Best-effort GitHub Commit Status report (Vercel-style "shadw — Deployment
/// ready" check on the commit/PR). No-op unless `GITHUB_TOKEN` is set in the
/// node's environment and the repo is on github.com. All failures are swallowed
/// so deploys never depend on GitHub being reachable.
async fn report_github_status(
    repo_url: &str,
    sha: &str,
    state: &str,
    target_url: &str,
    description: &str,
) {
    let token = match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return,
    };
    if sha.is_empty() {
        return;
    }
    let Some((owner, repo)) = parse_owner_repo(repo_url) else {
        return;
    };
    let url = format!("https://api.github.com/repos/{owner}/{repo}/statuses/{sha}");
    let mut body = serde_json::json!({
        "state": state, // pending | success | failure | error
        "description": description,
        "context": "shadw",
    });
    if !target_url.is_empty() {
        body["target_url"] = serde_json::Value::String(target_url.to_string());
    }
    let client = reqwest::Client::new();
    let _ = client
        .post(&url)
        .header(reqwest::header::USER_AGENT, "shadw")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .json(&body)
        .send()
        .await;
}

/// Parse `owner/repo` from a github.com URL (https or ssh form). Returns None for
/// non-github or malformed URLs.
fn parse_owner_repo(repo_url: &str) -> Option<(String, String)> {
    let s = repo_url.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let tail = s.rsplit("github.com").next()?;
    let tail = tail.trim_start_matches(['/', ':']);
    let mut parts = tail.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

fn region_label(region: &str) -> String {
    match region {
        "iad1" => "Washington, D.C., USA (East)",
        "sfo1" => "San Francisco, USA (West)",
        "fra1" => "Frankfurt, Germany",
        "hnd1" => "Tokyo, Japan",
        other => other,
    }
    .to_string()
}

/// Produce the deployment manifest from a cloned repo: Dockerfile (podman),
/// explicit `fluid.json`, or Framework-Defined Infrastructure. Any error here is
/// recoverable by the caller (the deployment is still created with a fallback).
async fn produce_manifest(
    cloud: &Arc<CloudState>,
    bid: &str,
    repo_root: &Path,
    dir: &Path,
    project: &str,
    commit: &str,
    first_deploy: bool,
    use_cache: bool,
    image_ref: Option<&str>,
    image_port: Option<u16>,
    image_protocol: Option<ServiceProtocol>,
    image_memory: Option<&str>,
    image_cpus: Option<&str>,
    image_pids: u32,
    image_ports: Option<Vec<fluid_core::PortSpec>>,
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);
    // Prebuilt OCI image (Docker Hub / Quay / any registry): pull it, auto-detect its
    // port, and run it as a container with an automatic persistent volume + env. No
    // Dockerfile build, no framework detection.
    if let Some(image) = image_ref {
        // Same override format/semantics as the Dockerfile-build path's fluid.json
        // `container` block below — parsed here since an image deploy has no
        // repo/fluid.json of its own to read.
        let mem_mib = image_memory.map(parse_mem_mib).unwrap_or(0);
        let cpus = image_cpus.map(parse_cpus_quota).unwrap_or(0.0);
        return image_container_manifest(
            cloud,
            bid,
            project,
            image,
            image_port,
            image_protocol,
            mem_mib,
            cpus,
            image_pids,
            image_ports,
        )
        .await;
    }
    // docker-compose / compose.yaml: a multi-service container deployment in ONE
    // project namespace. Takes precedence over a lone Dockerfile (it expresses the
    // full topology). Single-Dockerfile projects are unaffected.
    if let Some(compose_path) = crate::compose::compose_file(dir) {
        return build_compose_manifest(cloud, bid, dir, project, commit, &compose_path).await;
    }
    if let Some(dockerfile) = container_build_file(dir) {
        let fname = dockerfile
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Dockerfile")
            .to_string();
        log(format!("Detected {fname} — building container image."));
        let safe_project = sanitize_tag(project);
        let image = format!("hive-{}-{}", safe_project, &commit[..commit.len().min(7)]);
        // Railway-style overrides from an optional `fluid.json` { "container": {...} }:
        // explicit listen port + wire protocol (http/grpc/tcp/…). Falls back to the
        // Dockerfile `EXPOSE`/`ENV PORT` (then 8080) and protocol "http".
        let ovr = match tokio::fs::read_to_string(dir.join("fluid.json")).await {
            Ok(s) => parse_container_override(&s),
            Err(_) => ContainerOverride::default(),
        };
        let exposed = match ovr.port {
            Some(p) => p,
            None => parse_expose(&dockerfile).await.unwrap_or(8080),
        };
        let protocol = ovr.protocol.unwrap_or_else(|| "http".to_string());
        // Memory/cpus/pids ceilings: an explicit fluid.json `container.memory`/
        // `cpus`/`pids`, else 0/0.0 = "use the node's generous default" (never
        // the old hardcoded 512m that OOM-killed real monolith containers).
        // These are baked into the manifest.
        let mem_mib = ovr.memory.as_deref().map(parse_mem_mib).unwrap_or(0);
        let cpus = ovr.cpus.as_deref().map(parse_cpus_quota).unwrap_or(0.0);
        let pids = ovr.pids.unwrap_or(0);
        let t1 = now_ms();
        // Single-image build, no compose networking involved (unlike
        // `build_compose_manifest`, which stays on podman — see that
        // function's own comment) — safe to use Apple's `container` on
        // macOS. The image this produces is later RUN via the same OS
        // default (`podman_run_container`/mock.rs's `__container__` path),
        // so build and run always share one image store.
        let apple = hive_backend::container_cli::is_apple_default();
        let bin = hive_backend::container_cli::bin(apple);
        let mut build = Command::new(bin);
        // `-f` so the detected file (Dockerfile OR Containerfile) is used explicitly,
        // regardless of the builder's own default-file precedence.
        build
            .arg("build")
            .arg("-t")
            .arg(&image)
            .arg("-f")
            .arg(&dockerfile);
        // Resolve the container CLI on both Linux Firecracker hosts (/usr/bin,
        // podman) and macOS (/opt/homebrew/bin, either CLI) regardless of the
        // service's inherited PATH.
        let base_path = std::env::var("PATH").unwrap_or_default();
        build.env(
            "PATH",
            format!("/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:{base_path}"),
        );
        // Pass project env as --build-arg so a Dockerfile `ARG`/`ENV` can use them.
        for (k, v) in cloud.projects.env_map(project) {
            build.arg("--build-arg").arg(format!("{k}={v}"));
        }
        let out = build.arg(".").current_dir(dir).output().await?;
        for line in String::from_utf8_lossy(&out.stderr)
            .lines()
            .chain(String::from_utf8_lossy(&out.stdout).lines())
            .filter(|l| !l.trim().is_empty())
            .take(40)
        {
            log(format!("  {line}"));
        }
        anyhow::ensure!(out.status.success(), "{bin} build failed");
        log(format!(
            "Image built: {image} ({}ms), port {exposed} ({protocol})",
            now_ms().saturating_sub(t1)
        ));
        Ok(container_manifest(
            project, &image, exposed, &protocol, mem_mib, cpus, pids,
        ))
    } else if let Ok(s) = tokio::fs::read_to_string(dir.join("fluid.json")).await {
        let mut m = Manifest::from_json(&s)?;
        if m.project.is_empty() {
            m.project = project.to_string();
        }
        log("Detected fluid.json — using project configuration.".into());
        Ok(m)
    } else {
        build_via_fdi(cloud, bid, repo_root, dir, project, first_deploy, use_cache).await
    }
}

/// Does a package.json in `dir` reference workspace-protocol deps (`workspace:*`)
/// — i.e. it's a package inside a monorepo workspace that must be installed from
/// the workspace root?
async fn uses_workspace_protocol(dir: &Path) -> bool {
    let Ok(s) = tokio::fs::read_to_string(dir.join("package.json")).await else {
        return false;
    };
    s.contains("\"workspace:") || s.contains("workspace:*")
}

/// Is `root` a workspace root (pnpm-workspace.yaml or a `workspaces` field)?
async fn is_workspace_root(root: &Path) -> bool {
    if root.join("pnpm-workspace.yaml").exists() {
        return true;
    }
    if let Ok(s) = tokio::fs::read_to_string(root.join("package.json")).await {
        return s.contains("\"workspaces\"");
    }
    false
}

/// Whether to use `npm ci` (a clean, lockfile-exact install) instead of
/// `npm install`. Restricted to npm projects that have a committed
/// `package-lock.json` (yarn/pnpm have their own lockfiles), and only when this
/// is the project's first deployment (Task 1) or the build cache was explicitly
/// disabled on a redeploy (Task 2). All other builds use `npm install` + cache.
fn should_use_npm_ci(
    pm: &str,
    has_package_lock: bool,
    first_deploy: bool,
    use_cache: bool,
) -> bool {
    pm == "npm" && has_package_lock && (first_deploy || !use_cache)
}

/// Ensure a `yarn`/`pnpm` command's binary exists before running it (Issue #2).
/// yarn/pnpm are provisioned by corepack — but corepack itself is missing from
/// some distro Node packages, so a bare `corepack enable` is a no-op and the
/// command fails with "yarn: command not found". This self-heals: install
/// corepack via npm if absent, enable the shims (which respect the project's
/// `packageManager` field → correct yarn/pnpm version, classic or berry), then
/// run the command. npm always ships with Node. npm/bun/other commands pass
/// through unchanged.
fn corepack_wrap(cmd: &str) -> String {
    match cmd.split_whitespace().next().unwrap_or("") {
        "yarn" | "pnpm" => format!(
            "(command -v corepack >/dev/null 2>&1 || npm i -g corepack >/dev/null 2>&1); corepack enable >/dev/null 2>&1; {cmd}"
        ),
        _ => cmd.to_string(),
    }
}

/// Framework-Defined Infrastructure: detect the framework, run its real install
/// + build commands (streamed), then normalize the output into a Manifest —
/// either static assets or a serverless server. This is the executor that turns
/// a source repo into the Build Output API contract (`fluid-build`).
async fn build_via_fdi(
    cloud: &Arc<CloudState>,
    bid: &str,
    repo_root: &Path,
    dir: &Path,
    project: &str,
    first_deploy: bool,
    use_cache: bool,
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);

    // Build-config overrides ONLY when the user explicitly set a non-empty value
    // (defaults are empty — see BuildConfig::default). Detection drives the rest.
    let bc = cloud.projects.get_if_set(project).map(|s| s.build);
    let pick = |f: fn(&crate::project_settings::BuildConfig) -> &String| {
        bc.as_ref().map(f).filter(|s| !s.trim().is_empty()).cloned()
    };
    // `vercel.json` (loaded from the project root) takes precedence over Project
    // Settings, which in turn override framework auto-detection — exactly Vercel's
    // resolution order. A non-empty vercel.json value wins; otherwise fall back.
    let vc = fluid_build::load_vercel_config(dir);
    if vc.is_some() {
        log("Detected vercel.json — applying configuration overrides.".into());
    }
    let vc_pick = |sel: fn(&fluid_build::VercelConfig) -> Option<&String>| {
        vc.as_ref()
            .and_then(sel)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let inst = vc_pick(|c| c.install_command.as_ref()).or_else(|| pick(|b| &b.install_command));
    let bld = vc_pick(|c| c.build_command.as_ref()).or_else(|| pick(|b| &b.build_command));
    let outd = vc_pick(|c| c.output_directory.as_ref()).or_else(|| pick(|b| &b.output_dir));
    // An explicit framework choice (vercel.json, else project settings) overrides
    // auto-detection.
    let fwo = vc_pick(|c| c.framework.as_ref()).or_else(|| pick(|b| &b.framework));

    // Explicit RUNTIME resolution — orthogonal to package-manager detection below.
    // Precedence: vercel.json's native `runtime` field > vercel.json's `bunVersion`
    // presence (Vercel's own Bun-beta selector) > Project Settings' explicit
    // override > None (defer to `detect_start_cmd`/`adapter_manifest` inferring
    // from the framework's own start command — today's behavior, unchanged, so
    // every existing Node deployment and every Bun-as-package-manager-only
    // project — a `bun.lock` with no explicit runtime choice — keeps running on
    // Node exactly as before; a bun.lock alone must NEVER force the runtime).
    let runtime_override: Option<hive_core::Runtime> = vc_pick(|c| c.runtime.as_ref())
        .and_then(|s| hive_core::Runtime::from_config_str(&s))
        .or_else(|| {
            vc.as_ref()
                .and_then(|c| c.bun_version.as_ref())
                .map(|_| hive_core::Runtime::Bun)
        })
        .or_else(|| {
            bc.as_ref()
                .map(|b| b.runtime.as_str())
                .filter(|s| !s.trim().is_empty())
                .and_then(hive_core::Runtime::from_config_str)
        });
    if let Some(rt) = runtime_override {
        log(format!(
            "Explicit runtime: {} (vercel.json/project settings).",
            rt.as_str()
        ));
    }

    let plan = fluid_build::plan_build(
        dir,
        fwo.as_deref(),
        inst.as_deref(),
        bld.as_deref(),
        outd.as_deref(),
    );

    // Persist the auto-detected framework back into Project Settings so the
    // dashboard's Build settings reflect what was actually built (Issue #3). Only
    // fill in a field the user hasn't explicitly set, so we never clobber overrides.
    {
        let cur = bc.clone().unwrap_or_default();
        if cur.framework.trim().is_empty() {
            let mut nb = cur;
            nb.framework = plan.framework.slug.to_string();
            if nb.install_command.trim().is_empty() {
                nb.install_command = plan.install_command.clone();
            }
            if nb.build_command.trim().is_empty() {
                nb.build_command = plan.build_command.clone();
            }
            if nb.output_dir.trim().is_empty() {
                nb.output_dir = plan.output_dir.clone();
            }
            cloud.projects.set_build(project, nb);
            log(format!(
                "Auto-detected framework: {} (saved to Build settings).",
                plan.framework.name
            ));
        }
    }

    // Generic "Static" framework = publish the files as-is: no install, no build.
    // This honors the user's explicit choice and, crucially, never runs `npm
    // install` — so repos with a build-y `postinstall` (e.g. jor1k's `./compile`)
    // deploy as plain static sites instead of failing the install. Serve the
    // configured output dir, else the repo root (where index.html lives).
    if plan.framework.slug == "static" {
        let sd = outd.clone().unwrap_or_else(|| ".".to_string());
        log(format!(
            "Static project (framework override) — skipping install/build; serving \"{sd}\" as-is."
        ));
        return Ok(static_manifest(project, &sd));
    }

    // Monorepo detection: only when THIS package actually uses `workspace:*` deps
    // (which resolve only from a root install). A standalone example that merely
    // lives inside a workspace repo (e.g. vercel/vercel's examples/* have normal
    // deps) is installed in place — installing at the root would match no project.
    let root_is_workspace = is_workspace_root(repo_root).await;
    let is_monorepo = dir != repo_root && uses_workspace_protocol(dir).await && root_is_workspace;
    let install_dir = if is_monorepo { repo_root } else { dir };
    // A standalone app that merely LIVES inside a workspace repo it is NOT a member
    // of (e.g. vercel/vercel's `examples/*`): the workspace package manager
    // (pnpm/yarn) walks UP to the repo root and installs the workspace WITHOUT this
    // subdir's deps — so its framework binary (`vite`, …) is never installed and the
    // build dies with "command not found". Force npm here: npm doesn't traverse up
    // to a pnpm/yarn workspace, so it installs THIS dir's package.json in place.
    let foreign_subdir = !is_monorepo && dir != repo_root && root_is_workspace;
    let pm_detection = fluid_build::detect_package_manager(install_dir);
    let pm = if foreign_subdir {
        "npm"
    } else {
        pm_detection.manager
    };

    log(format!(
        "Detected framework: {} — primitive: {:?}, package manager: {} ({:?}){}",
        plan.framework.name,
        plan.framework.primitive,
        pm,
        if foreign_subdir {
            fluid_build::PackageManagerSource::Default
        } else {
            pm_detection.source
        },
        if is_monorepo {
            " (workspace monorepo — installing at root)"
        } else {
            ""
        }
    ));
    // Package-manager choice never forces the RUNTIME (see `runtime_override`
    // above) — a bun.lock/packageManager:"bun" alone only picks `bun install`.
    // A conflicting lockfile (e.g. both bun.lock and pnpm-lock.yaml committed)
    // is never silently dropped — always logged, deterministic winner.
    if !foreign_subdir {
        if let Some(w) = &pm_detection.conflict_warning {
            log(format!("WARN: {w}"));
        }
    }
    if let Some(nd) = preferred_node_bin() {
        log(format!("Node runtime: {nd}"));
    }

    let has_pkg = install_dir.join("package.json").exists();
    // Build cache key (lockfile + package manager) for restore/save.
    let cache_key = compute_cache_key(install_dir, pm).await;

    // For a pnpm workspace, scope the install to just this package + its deps
    // (`--filter`) so we don't install the entire monorepo.
    let rel = dir
        .strip_prefix(repo_root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let pnpm_filter = if is_monorepo && !rel.is_empty() {
        format!(" --filter \"{{./{rel}}}...\"")
    } else {
        String::new()
    };

    // `npm ci` is the clean, lockfile-exact install. We use it ONLY for an npm
    // project with a committed package-lock.json (never yarn/pnpm — those have
    // their own lockfiles), and ONLY when:
    //   • this is the project's FIRST deployment (Task 1 — clean initial build), or
    //   • the redeploy explicitly disabled the build cache (Task 2 — fresh install).
    // Every other build uses `npm install` + the warm node_modules cache (fast).
    // `npm ci` wipes node_modules, so it never benefits from a restored cache.
    let use_npm_ci = should_use_npm_ci(
        pm,
        install_dir.join("package-lock.json").exists(),
        first_deploy,
        use_cache,
    );
    // Install command: honor an explicit override, else the right command for the
    // detected package manager (pnpm/yarn via corepack so the binary is present).
    let install_cmd: String = corepack_wrap(&inst.unwrap_or_else(|| match pm {
        "pnpm" => format!("pnpm install --no-frozen-lockfile{pnpm_filter}"),
        "yarn" => "yarn install --network-timeout 600000".into(),
        "bun" => "bun install".into(),
        _ if use_npm_ci => "npm ci --no-audit --no-fund".into(),
        _ => "npm install --no-audit --no-fund".into(),
    }));
    if use_npm_ci {
        log(format!(
            "Using `npm ci` (package-lock.json present, {}).",
            if first_deploy {
                "first deployment"
            } else {
                "build cache disabled"
            }
        ));
    }
    // Build command. Framework presets give a RAW binary invocation, e.g.
    // "vite build" / "next build". For npm that resolves via the project's
    // node_modules/.bin (which run_streamed puts on PATH). But for a pnpm/yarn
    // WORKSPACE install (e.g. the vercel/vercel monorepo's examples), the binary
    // is NOT linked into the package's local .bin, so a raw `vite build` dies with
    // "vite: command not found" (exit 127). Run framework build commands through
    // the package manager's `exec` so it resolves the bin from the hoisted/virtual
    // store. A `npm run …` style command is just re-pointed to the active PM.
    let build_cmd = corepack_wrap(&{
        let bc = plan.build_command.clone();
        let first = bc.split_whitespace().next().unwrap_or("");
        let is_pm_invocation = matches!(
            first,
            "npm" | "pnpm" | "yarn" | "bun" | "npx" | "bunx" | "corepack"
        );
        if first == "npm" && pm != "npm" {
            bc.replacen("npm", pm, 1)
        } else if !bc.trim().is_empty() && !is_pm_invocation {
            match pm {
                "pnpm" => format!("pnpm exec {bc}"),
                "yarn" => format!("yarn exec {bc}"),
                // When the RUNTIME (not just the package manager) is explicitly
                // Bun, add `--bun` so the framework's own internally-spawned
                // Node-shebang child processes (e.g. Next.js's build workers)
                // also run under Bun instead of silently falling back to Node —
                // this is exactly Vercel's documented `bun run --bun next build`
                // requirement. Plain `bunx {bc}` (no `--bun`) only resolves the
                // LOCAL binary through Bun's package-runner; it does not affect
                // what runtime that binary's own children use. Orthogonal to
                // package-manager choice: a bun.lock with no explicit runtime
                // override keeps today's plain `bunx {bc}`.
                "bun" if runtime_override == Some(hive_core::Runtime::Bun) => {
                    format!("bunx --bun {bc}")
                }
                "bun" => format!("bunx {bc}"),
                // npm: the raw binary resolves via node_modules/.bin on PATH.
                _ => bc,
            }
        } else {
            bc
        }
    });

    // 0) Restore cached dependencies (local, else pull from a mesh peer). Skipped
    // when the cache is disabled (redeploy opt-out) or when `npm ci` will wipe
    // node_modules anyway — a restore would just be thrown away.
    if use_cache && !use_npm_ci {
        if let Some(k) = &cache_key {
            restore_cache(cloud, bid, install_dir, k).await;
        }
    } else if !use_cache {
        log("Build cache disabled — installing dependencies fresh.".into());
    }
    // Project env vars (already persisted at build start) — injected into both
    // the install and build steps so framework builds can read them.
    let proj_env = cloud.projects.env_map(project);
    // 1) Install dependencies at the install dir (root for monorepos). With a
    // restored node_modules this is a fast verify; otherwise a clean install.
    if has_pkg && !install_cmd.trim().is_empty() {
        log(format!(
            "Running \"{}\"{}",
            install_cmd,
            if is_monorepo { " (workspace root)" } else { "" }
        ));
        run_streamed(install_dir, &install_cmd, cloud, bid, &proj_env)
            .await
            .map_err(|e| anyhow::anyhow!("install command failed: {e}"))?;
    }
    // 1.5) SvelteKit: the default `@sveltejs/adapter-auto` only emits output for
    // managed hosts (Vercel/Netlify/Cloudflare/…); on a self-hosted node it
    // produces NO runnable server, so the function never binds its port. Swap it
    // for `@sveltejs/adapter-node`, which emits a standalone `build/` server that
    // listens on $HOST:$PORT — run later with `node build`. Done after install
    // (node_modules present) and before the build.
    let is_sveltekit = plan.framework.slug == "sveltekit";
    if is_sveltekit {
        let cfg = dir.join("svelte.config.js");
        if let Ok(src) = tokio::fs::read_to_string(&cfg).await {
            if !src.contains("@sveltejs/adapter-node") && src.contains("@sveltejs/adapter-auto") {
                // Pin adapter-node to the installed SvelteKit major (kit 1.x →
                // adapter-node 1.x; kit ≥2 → latest), or modern adapter-node's
                // `@sveltejs/kit@^2` peer dep clashes with an old kit and `npm`
                // aborts (ERESOLVE). `--legacy-peer-deps` tolerates prereleases.
                let kit_major =
                    tokio::fs::read_to_string(dir.join("node_modules/@sveltejs/kit/package.json"))
                        .await
                        .ok()
                        .and_then(|s| s.split("\"version\"").nth(1).map(|x| x.to_string()))
                        .and_then(|x| x.split('"').nth(1).map(|v| v.to_string()))
                        .and_then(|ver| ver.split('.').next().map(|m| m.to_string()));
                let spec = match kit_major.as_deref() {
                    Some("1") => "@sveltejs/adapter-node@^1",
                    _ => "@sveltejs/adapter-node",
                };
                log(format!("SvelteKit detected with adapter-auto — switching to {spec} so it serves on a self-hosted node."));
                let add = corepack_wrap(&match pm {
                    "pnpm" => format!("pnpm add -D --config.strict-peer-dependencies=false {spec}"),
                    "yarn" => format!("yarn add -D {spec}"),
                    "bun" => format!("bun add -d {spec}"),
                    _ => format!("npm install -D --no-audit --no-fund --legacy-peer-deps {spec}"),
                });
                run_streamed(dir, &add, cloud, bid, &proj_env)
                    .await
                    .map_err(|e| anyhow::anyhow!("installing adapter-node failed: {e}"))?;
                let patched = src.replace("@sveltejs/adapter-auto", "@sveltejs/adapter-node");
                let _ = tokio::fs::write(&cfg, patched).await;
            }
        }
    }
    // 1.6) OpenNext: its default AWS wrapper emits a Lambda-style handler, which
    // won't bind a port on a self-hosted node. Force the `node` wrapper+converter
    // so `.open-next/server-functions/default/index.mjs` boots a standalone HTTP
    // server on $PORT (run later with `node …index.mjs`) — giving it Fluid compute.
    // Only synthesized when the user hasn't supplied their own open-next config.
    if plan.framework.slug == "opennext" {
        let has_cfg = [
            "open-next.config.ts",
            "open-next.config.js",
            "open-next.config.mjs",
        ]
        .iter()
        .any(|f| dir.join(f).exists());
        if !has_cfg {
            let cfg = "// Auto-generated by OpenEdge: run OpenNext's server function as a\n\
                       // standalone Node HTTP server on $PORT so it runs on Fluid compute.\n\
                       export default {\n  default: {\n    override: {\n      wrapper: \"node\",\n      converter: \"node\",\n    },\n  },\n};\n";
            let _ = tokio::fs::write(dir.join("open-next.config.ts"), cfg).await;
            log("OpenNext detected — generated open-next.config.ts (node wrapper) so the server runs on $PORT under Fluid.".into());
        } else {
            log("OpenNext detected — using your open-next.config (ensure the default server uses the `node` wrapper to serve on $PORT).".into());
        }
    }
    // 2) Build in the project directory.
    if has_pkg && !build_cmd.trim().is_empty() {
        log(format!("Running \"{}\"", build_cmd));
        run_streamed(dir, &build_cmd, cloud, bid, &proj_env)
            .await
            .map_err(|e| anyhow::anyhow!("build command failed: {e}"))?;
    }
    // 3) Save the warm cache for the next build (and for peers to pull).
    if let Some(k) = &cache_key {
        save_cache(cloud, bid, install_dir, k).await;
    }

    let has_bo = fluid_build::has_build_output(dir);
    if has_bo {
        log("Build Output API detected (.vercel/output).".into());
    }

    use fluid_build::Primitive;
    match plan.framework.primitive {
        Primitive::Static => {
            let sd = if has_bo {
                ".vercel/output/static".to_string()
            } else {
                plan.output_dir.clone()
            };
            log(format!("Serving static assets from \"{sd}\"."));
            Ok(static_manifest(project, &sd))
        }
        Primitive::Serverless | Primitive::Hybrid => {
            // Next.js DEPLOYMENT ADAPTERS (OpenNext / vinext): the build emits a
            // Node HTTP server + a separate immutable-assets dir. Run the server as
            // the Fluid `api` function and serve assets from the CDN, falling
            // through to the function on a miss (the CDN→origin model). This is what
            // gives OpenNext/vinext apps Fluid compute (warm pool + concurrency).
            if let Some(m) =
                adapter_manifest(project, plan.framework.slug, dir, runtime_override).await
            {
                log(format!(
                    "{} adapter: server `{}`, assets from \"{}\" (CDN→origin fallthrough).",
                    plan.framework.name,
                    m.functions
                        .first()
                        .map(|f| f.start_cmd.join(" "))
                        .unwrap_or_default(),
                    m.static_dir.clone().unwrap_or_default(),
                ));
                return Ok(m);
            }
            // Node-server model: the framework was just built, so its production
            // server (`next start`, `node build`, …) will boot and listen on
            // $PORT in the build dir. The gateway proxies to it. SvelteKit (built
            // with adapter-node above) runs its standalone server via `node build`
            // (Node's directory-resolution finds build/index.js automatically —
            // left untouched for zero regression risk). Under an explicit Bun
            // runtime, point directly at the file instead of relying on Bun
            // matching Node's directory-resolution semantics (unverified).
            let start = if is_sveltekit && dir.join("build/index.js").exists() {
                if runtime_override == Some(hive_core::Runtime::Bun) {
                    vec![
                        "bun".to_string(),
                        "run".to_string(),
                        "build/index.js".to_string(),
                    ]
                } else {
                    vec!["node".to_string(), "build".to_string()]
                }
            } else {
                detect_start_cmd(dir, runtime_override).await
            };
            log(format!(
                "Provisioning serverless server: `{}`.",
                start.join(" ")
            ));
            // Per-route bundle splitting (Issue: SHADOW_NEXT_PER_ROUTE) — DISCOVERY
            // ONLY for now. When enabled and this is a Next.js app, classify each
            // route from the .next manifests + file traces and write a per-route
            // build manifest. The serve path is UNCHANGED (still `next start`); the
            // runtime dispatcher consumes this manifest separately and falls back.
            let mut route_policies: Vec<fluid_core::RoutePolicy> = Vec::new();
            if std::env::var("SHADOW_NEXT_PER_ROUTE")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false)
            {
                let next_dir = dir.join(".next");
                if next_dir.exists() {
                    let prm = fluid_build::per_route::discover(&next_dir);
                    log(format!(
                        "per-route: classified {} route(s) — {} per-route-eligible (Node), {} on next-start fallback (static/edge/middleware).",
                        prm.routes.len(), prm.eligible_count(), prm.fallback_count()
                    ));
                    // Map build-time classification -> runtime policy (#16), persisted
                    // into the manifest so the serve path can apply route-type-aware
                    // caching/retry. The `next start` fallback still serves every
                    // route; this only enriches responses, it doesn't change routing.
                    route_policies = prm
                        .routes
                        .iter()
                        .map(|r| fluid_core::RoutePolicy {
                            pattern: r.route.clone(),
                            class: fluid_core::RouteClass::from_name(r.kind.class_name()),
                            revalidate: r.revalidate,
                        })
                        .collect();
                    if let Ok(js) = serde_json::to_string_pretty(&prm) {
                        let _ = tokio::fs::write(dir.join("shadow-per-route.json"), js).await;
                        log(format!("per-route: wrote shadow-per-route.json + {} route policies into the manifest (cache/retry wiring). Serving still uses `next start` (fallback).", route_policies.len()));
                    }
                }
            }
            let mut m = function_manifest(project, start, runtime_override);
            m.route_policies = route_policies;
            // after()/waitUntil runtime support for Node-runtime deployments: drop
            // the platform shim into the build dir and inject it via NODE_OPTIONS
            // so Next.js `after()` (which funnels into the platform waitUntil at
            // globalThis[Symbol.for('@next/request-context')].get().waitUntil) keeps
            // the instance warm for background work via the existing
            // x-fluid-wait-until-ms keep-alive convention. Skipped for Bun (doesn't
            // honor NODE_OPTIONS --require); harmless no-op if after() is never used.
            if runtime_override != Some(hive_core::Runtime::Bun) {
                install_after_shim(dir, &mut m).await;
            }
            Ok(m)
        }
    }
}

/// The Node `--require` preload that provides `after()`/`waitUntil` support. Read
/// at compile time from the crate's assets and written into each Node deployment.
const AFTER_SHIM_JS: &str = include_str!("../assets/after-shim.cjs");
const AFTER_SHIM_FILE: &str = ".hive-after-shim.cjs";

/// Write the after()-support shim into the build dir and wire it into the first
/// function's serve env (NODE_OPTIONS=--require + HIVE_AFTER_MAX_MS = maxDuration).
/// A relative `--require ./<file>` path so it resolves against the function's CWD
/// on every backend (the FC microVM relocates the workdir to /build).
async fn install_after_shim(dir: &Path, m: &mut Manifest) {
    if tokio::fs::write(dir.join(AFTER_SHIM_FILE), AFTER_SHIM_JS)
        .await
        .is_err()
    {
        return; // best-effort: a shim write failure must never fail the deploy
    }
    let Some(f) = m.functions.first_mut() else {
        return;
    };
    let max_ms = (f.max_duration_secs.max(1) as u64).saturating_mul(1000);
    // Append to any NODE_OPTIONS the project already set, don't clobber it.
    let require = format!("--require ./{AFTER_SHIM_FILE}");
    f.env
        .entry("NODE_OPTIONS".to_string())
        .and_modify(|v| {
            if !v.contains(AFTER_SHIM_FILE) {
                v.push(' ');
                v.push_str(&require);
            }
        })
        .or_insert(require);
    f.env
        .entry("HIVE_AFTER_MAX_MS".to_string())
        .or_insert_with(|| max_ms.to_string());
}

fn static_manifest(project: &str, static_dir: &str) -> Manifest {
    Manifest {
        project: project.to_string(),
        static_dir: Some(if static_dir.is_empty() {
            ".".into()
        } else {
            static_dir.to_string()
        }),
        functions: vec![],
        routes: vec![Route {
            pattern: "/".into(),
            target: RouteTarget::Static,
        }],
        ..Default::default()
    }
}

/// The command that boots the built app's production server.
async fn detect_start_cmd(dir: &Path, runtime: Option<hive_core::Runtime>) -> Vec<String> {
    let bun = runtime == Some(hive_core::Runtime::Bun);
    if let Ok(pkg) = tokio::fs::read_to_string(dir.join("package.json")).await {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
            if v.get("scripts").and_then(|s| s.get("start")).is_some() {
                // Bun's own script-runner honors package.json#scripts.start
                // identically to `npm start` (package.json-script INDIRECTION —
                // the script's own text is executed as a shell command), so
                // `--bun` is required here: a script like `"start": "node
                // server.js"` (extremely common — it's what `npm init`/most
                // hand-written package.jsons emit) would otherwise spawn REAL
                // node, silently defeating the Bun runtime choice. Verified
                // live: `bun run start` on such a script reported
                // `process.versions.bun === null`; `bun run --bun start`
                // reported the real Bun version. This does NOT apply to a
                // direct-file invocation (`bun server.js`, no script-name
                // indirection) below — there the top-level process is already
                // Bun, so nothing to force.
                return if bun {
                    vec!["bun".into(), "run".into(), "--bun".into(), "start".into()]
                } else {
                    vec!["npm".into(), "start".into()]
                };
            }
        }
    }
    for entry in ["server.js", "index.js", "app.py", "main.py", "server.py"] {
        if dir.join(entry).exists() {
            if entry.ends_with(".py") {
                return vec!["python3".into(), entry.into()];
            }
            let runner = if bun { "bun" } else { "node" };
            return vec![runner.into(), entry.into()];
        }
    }
    if bun {
        vec!["bun".into(), "run".into(), "--bun".into(), "start".into()]
    } else {
        vec!["npm".into(), "start".into()]
    }
}

/// Find a STABLE Node 20–24 bin dir, preferring it over an unstable system node
/// (e.g. Homebrew's node v26 canary) so framework builds (SvelteKit, etc.) don't
/// fail engine checks. Looks at nvm-installed versions first, then `node@NN` kegs.
pub fn preferred_node_bin() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let nvm = PathBuf::from(&home).join(".nvm/versions/node");
    let mut best: Option<(u32, PathBuf)> = None;
    if let Ok(rd) = std::fs::read_dir(&nvm) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(major) = name
                .trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|s| s.parse::<u32>().ok())
            {
                if (20..=24).contains(&major) {
                    let bin = e.path().join("bin");
                    if bin.join("node").exists()
                        && best.as_ref().map(|(m, _)| major > *m).unwrap_or(true)
                    {
                        best = Some((major, bin));
                    }
                }
            }
        }
    }
    if let Some((_, bin)) = best {
        return Some(bin.to_string_lossy().into_owned());
    }
    for major in [24u32, 22, 20] {
        for base in ["/opt/homebrew/opt", "/usr/local/opt"] {
            let bin = PathBuf::from(format!("{base}/node@{major}/bin"));
            if bin.join("node").exists() {
                return Some(bin.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Node BINARY to use for the build-time compile-cache warm-up. The produced V8
/// bytecode cache is keyed by the exact Node/V8 version + CPU arch — if the warm-up
/// Node differs from the RUNTIME Node, the runtime silently ignores the cache and
/// recompiles (the "cross-platform silent-miss"). On Firecracker the runtime is the
/// microVM's baked-in Node, which is NOT the build host's Node, so each FC host sets
/// `HIVE_WARMUP_NODE` to a copy of that exact binary. Elsewhere (Mac/mock backend,
/// where build host == runtime) we fall back to the pinned stable Node directory.
pub fn warmup_node_bin() -> Option<String> {
    if let Ok(p) = std::env::var("HIVE_WARMUP_NODE") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(p);
        }
        let cand = pb.join("node");
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    preferred_node_bin().map(|d| PathBuf::from(d).join("node").to_string_lossy().into_owned())
}

/// Find a `bun` binary on common install locations (Homebrew, Bun's own
/// installer default `~/.bun/bin`, system paths).
fn which_bun() -> Option<String> {
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        let cand = PathBuf::from(dir).join("bun");
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let cand = PathBuf::from(&home).join(".bun/bin/bun");
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

/// Bun BINARY for the build-time bytecode warm-up. Like V8's compile cache, Bun's
/// bytecode format is tied to a specific Bun build — mismatched builds mean the
/// runtime silently ignores the cache (safe, just no speedup). `HIVE_WARMUP_BUN`
/// pins the exact RUNTIME Bun (set on Firecracker hosts to a copy of the microVM's
/// baked-in Bun), mirroring `HIVE_WARMUP_NODE`/`warmup_node_bin`. Falls back to
/// whatever `bun` the build host has (correct when build host == runtime host,
/// e.g. the Mac/mock backend).
pub fn warmup_bun_bin() -> Option<String> {
    if let Ok(p) = std::env::var("HIVE_WARMUP_BUN") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(p);
        }
        let cand = pb.join("bun");
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    which_bun()
}

async fn bun_version(bun_bin: &str) -> Option<String> {
    let out = Command::new(bun_bin).arg("--version").output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Extract an uploaded ZIP (raw bytes) into `dir` via the system `unzip`. Strips
/// macOS cruft (`__MACOSX`, `.DS_Store`) and, when the archive is a single top-level
/// directory (the common "zip a folder" / GitHub "Download ZIP" shape), flattens it
/// so the project root (package.json, etc.) lands directly at `dir`. Returns the
/// resulting file count. `unzip` itself refuses absolute/`..` (zip-slip) paths.
async fn extract_zip_into(bytes: &[u8], dir: &Path) -> anyhow::Result<u64> {
    tokio::fs::create_dir_all(dir).await?;
    // Sibling temp file, never inside the build (so it isn't counted as a build
    // output). Append-based name, NOT `Path::with_extension`: a dotted checkout
    // name (a sanitized project tag may contain `.`) would be truncated to its
    // first label — "foo.bar-…" → "foo.upload.zip" — colliding across projects.
    // The per-build-unique `dir` makes this path unique per build.
    let tmp = dir.with_file_name(format!(
        "{}.upload.zip",
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    tokio::fs::write(&tmp, bytes).await?;
    // unzip exit 0 = ok, 1 = warning (e.g. it skipped an unsafe path) — both acceptable.
    let out = Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(&tmp)
        .arg("-d")
        .arg(dir)
        .output()
        .await?;
    let _ = tokio::fs::remove_file(&tmp).await;
    let code = out.status.code().unwrap_or(-1);
    anyhow::ensure!(
        code == 0 || code == 1,
        "could not unpack archive (unzip {code}): {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let _ = tokio::fs::remove_dir_all(dir.join("__MACOSX")).await;
    // Collect top-level entries (dropping .DS_Store) to detect a single wrapper dir.
    let mut top: Vec<PathBuf> = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(e) = rd.next_entry().await? {
        if e.file_name() == ".DS_Store" {
            let _ = tokio::fs::remove_file(e.path()).await;
            continue;
        }
        top.push(e.path());
    }
    if top.len() == 1 && top[0].is_dir() {
        let inner = top[0].clone();
        let mut rd2 = tokio::fs::read_dir(&inner).await?;
        while let Some(e) = rd2.next_entry().await? {
            let _ = tokio::fs::rename(e.path(), dir.join(e.file_name())).await;
        }
        let _ = tokio::fs::remove_dir_all(&inner).await;
    }
    Ok(dir_stats(dir).await.0)
}

/// Recursively copy the CONTENTS of `src` into `dst` (created if missing). Used by a
/// zip-upload REDEPLOY that has no re-fetchable remote: it rebuilds from the retained
/// on-disk checkout of the prior build. Symlinks are recreated best-effort; this is a
/// local same-filesystem copy, so cost is bounded by the checkout size.
fn copy_dir_into(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir_into(&from, &to)?;
        } else if ft.is_symlink() {
            if let Ok(target) = std::fs::read_link(&from) {
                let _ = std::os::unix::fs::symlink(target, &to);
            }
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// (file count, total bytes) under `dir`, recursively — to report what the warm-up
/// captured into the compile cache.
async fn dir_stats(dir: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&d).await else {
            continue;
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            match e.file_type().await {
                Ok(ft) if ft.is_dir() => stack.push(e.path()),
                Ok(_) => {
                    files += 1;
                    if let Ok(m) = e.metadata().await {
                        bytes += m.len();
                    }
                }
                _ => {}
            }
        }
    }
    (files, bytes)
}

/// Build-time V8 compile-cache warm-up (Node cold-start contract). Boots the server
/// once under `NODE_COMPILE_CACHE=<root>/.hive-compile-cache` so V8 bytecode for the
/// hot modules (framework + server bundle) is written INTO the artifact; the runtime
/// cell points `NODE_COMPILE_CACHE` at the same dir (workdir == delivered root), so
/// even a fresh microVM's first hit skips parse/compile. A `--require` preload flushes
/// the cache and exits cleanly after a short boot window (no signal needed; works for
/// `node x` and `npm start`). Best-effort: any failure is a perf no-op, never breaks a
/// deploy. Uses the pinned build Node — the runtime cell must run the SAME Node
/// major+arch or V8 silently ignores the cache (safe, just no speedup).
async fn warmup_node_cache(
    build_dir: &Path,
    start_cmd: &[String],
    proj_env: &std::collections::BTreeMap<String, String>,
    cloud: &Arc<CloudState>,
    bid: &str,
) {
    use std::process::Stdio;
    let log = |m: String| cloud.builds.log(bid, m);
    if std::env::var("HIVE_COMPILE_CACHE")
        .map(|v| v == "0" || v == "false")
        .unwrap_or(false)
    {
        return;
    }
    // Callers only invoke this for a function whose resolved runtime is Node
    // (see the `hive_core::Runtime::resolve(...) == Runtime::Node` filter at the
    // call site) — no redundant argv re-check here, since re-deriving it from
    // argv alone would disagree with an EXPLICIT `runtime: "node"` config paired
    // with an unusual custom launcher script.
    let cache_dir = build_dir.join(".hive-compile-cache");
    if tokio::fs::create_dir_all(&cache_dir).await.is_err() {
        return;
    }
    // Preload: after HIVE_WARMUP_MS, flush the compile cache + exit(0) so bytecode
    // persists. Self-terminating, so it works regardless of the server's own signal
    // handling and needs no external kill. `flushCompileCache` exists on Node >=22.8;
    // on older/unsupported Node it's a no-op and the cache dir stays empty (handled).
    let preload = build_dir.join(".hive-warmup-preload.cjs");
    let preload_js = "const m=require('module');const ms=parseInt(process.env.HIVE_WARMUP_MS||'5000',10);setTimeout(()=>{try{m.flushCompileCache&&m.flushCompileCache();}catch(e){}process.exit(0);},ms);";
    if tokio::fs::write(&preload, preload_js).await.is_err() {
        return;
    }
    // PATH mirrors run_streamed: project-local .bin, then the pinned stable Node.
    let local_bin = build_dir.join("node_modules/.bin");
    let mut prefix = local_bin.to_string_lossy().into_owned();
    // Pin the warm-up to the RUNTIME Node (HIVE_WARMUP_NODE on FC nodes = the microVM's
    // baked Node) so the bytecode cache is valid at runtime, not silently re-compiled.
    let warm_node = warmup_node_bin();
    if let Some(dir) = warm_node.as_deref().and_then(|nb| Path::new(nb).parent()) {
        prefix.push(':');
        prefix.push_str(&dir.to_string_lossy());
    }
    let path = format!(
        "{prefix}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}",
        std::env::var("PATH").unwrap_or_default()
    );
    let cmd_str = start_cmd.join(" ");
    // The V8 compile cache is keyed by each module's ABSOLUTE path. On Firecracker the
    // runtime relocates the artifact to a FIXED workdir (the microVM mounts the data
    // disk at /build), so the warm-up must compile with those SAME paths or the runtime
    // silently re-compiles (the cross-platform/-path silent-miss). When
    // HIVE_WARMUP_BUILD_PATH is set (FC nodes → /build), run the warm-up in a private
    // mount namespace with build_dir bind-mounted there so module paths match. On the
    // Mac/mock backend build_dir IS the run dir, so no remap is needed (env unset).
    let remap = std::env::var("HIVE_WARMUP_BUILD_PATH")
        .ok()
        .filter(|p| p.starts_with('/'));
    let remap = remap
        .as_deref()
        .map(|t| t.trim_end_matches('/').to_string());
    let (cc_dir, preload_arg): (String, String) = match &remap {
        Some(t) => (
            format!("{t}/.hive-compile-cache"),
            format!("{t}/.hive-warmup-preload.cjs"),
        ),
        None => (
            cache_dir.to_string_lossy().into_owned(),
            preload.to_string_lossy().into_owned(),
        ),
    };
    log(format!(
        "Compile-cache: warming V8 bytecode (booting `{cmd_str}`)…"
    ));
    // `exec` so the child IS the server process (not a wrapping shell); PORT=0 binds an
    // ephemeral port (compilation happens during boot, before/around the listen).
    let mut command = match &remap {
        Some(t) => {
            let mut c = Command::new("unshare");
            c.arg("-m").arg("sh").arg("-c").arg(format!(
                "mkdir -p {t} && mount --bind {b} {t} && cd {t} && exec {cmd_str}",
                b = build_dir.to_string_lossy()
            ));
            c
        }
        None => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c")
                .arg(format!("exec {cmd_str}"))
                .current_dir(build_dir);
            c
        }
    };
    let home: &Path = remap.as_deref().map(Path::new).unwrap_or(build_dir);
    let child = command
        .env("PATH", &path)
        .env("HOME", home)
        .env("PORT", "0")
        .env("NODE_COMPILE_CACHE", &cc_dir)
        .env("NODE_OPTIONS", format!("--require {preload_arg}"))
        .env("HIVE_WARMUP_MS", "5000")
        .envs(proj_env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut c) => {
            // A launcher (`npm start`, `next start`) forks the real server as a CHILD that
            // compiles + flushes its bytecode slightly AFTER the launcher exits. Reading the
            // cache the instant the launcher exits ships only the launcher's modules and
            // SILENTLY MISSES the server's (the real compile cost). So poll the cache until
            // it stops growing — i.e. all descendants have finished flushing — with a hard
            // cap so a hung boot can't stall the deploy. The preload self-exits each Node
            // process ~HIVE_WARMUP_MS after its modules are compiled (sync require completes
            // first), so the cache converges quickly.
            let start = tokio::time::Instant::now();
            let mut last = 0u64;
            let mut stable = 0u32;
            loop {
                tokio::time::sleep(Duration::from_millis(800)).await;
                let exited = matches!(c.try_wait(), Ok(Some(_)));
                let (_f, bytes) = dir_stats(&cache_dir).await;
                if bytes > 0 && bytes == last {
                    stable += 1
                } else {
                    stable = 0
                }
                last = bytes;
                let elapsed = start.elapsed();
                // Done when: the cache captured something and held steady ~2.4s; OR the
                // launcher exited having produced nothing after a grace window (non-Node /
                // old Node — nothing to wait for); OR the 45s hard cap.
                if (stable >= 3 && bytes > 0)
                    || (exited && bytes == 0 && elapsed > Duration::from_secs(8))
                    || elapsed > Duration::from_secs(45)
                {
                    break;
                }
            }
            let _ = c.start_kill();
        }
        Err(e) => log(format!(
            "WARN: compile-cache warm-up could not start ({e}); deploy continues uncached."
        )),
    }
    let _ = tokio::fs::remove_file(&preload).await;
    let (files, bytes) = dir_stats(&cache_dir).await;
    if files > 0 {
        log(format!(
            "Compile-cache: precompiled {files} module(s), {} KB → shipped in artifact (warm-up Node: {}).",
            bytes / 1024,
            warm_node.as_deref().unwrap_or("system")
        ));
    } else {
        let _ = tokio::fs::remove_dir_all(&cache_dir).await;
        log("Compile-cache: no bytecode captured (runtime Node may be <22.1, or the server didn't boot) — app still starts, just uncached.".into());
    }
}

/// Build-time Bun bytecode cache — the Bun-native equivalent of
/// `warmup_node_cache`, but STRUCTURALLY DIFFERENT, not a copy-paste: V8's
/// `NODE_COMPILE_CACHE` is an opaque, process-wide cache directory layered on
/// UNMODIFIED source (any `require()` gets cached, no rewrite needed). Bun uses
/// JavaScriptCore, which has no equivalent hook — its bytecode cache
/// (`bun build --bytecode`) is a build-time BUNDLING step that emits a NEW file
/// plus a `.jsc` bytecode sidecar (and, here, a `.map` external source map); `bun
/// run <bundled-file>` auto-loads both with ZERO runtime env var. So on success
/// this function REWRITES start_cmd to point at the bundled output — callers
/// MUST use the returned Vec, not the one they passed in.
///
/// Only applies when start_cmd resolves to a real, existing entry FILE (`bun
/// <file>` / `bun run <file>`) under `build_dir` — a plain Node-style
/// server.js/index.js, or an OpenNext/vinext adapter's single bundled server
/// file both qualify. Does NOT apply to a framework CLI invocation (`bunx --bun
/// next start`, `bun run --bun next start`): a CLI wrapper is not a statically
/// resolvable module graph Bun can bundle ahead of time — this is a REAL, proven
/// Bun limitation (proven empirically: `bun build` on a CLI entry either fails
/// or bundles the wrong thing), unlike Node's compile cache, which works
/// regardless of how the process was launched. Detected explicitly and
/// downgraded safely: start_cmd is returned UNCHANGED and a clear reason is
/// logged; the app still runs correctly on Bun, just without the bytecode
/// speedup for that one framework combination.
///
/// Best-effort: any failure (missing `bun` binary, bundling error, unexpected
/// output shape) is a perf no-op, never breaks a deploy — returns the ORIGINAL
/// start_cmd unchanged.
///
/// Pure entry-detection: is `start_cmd` a bundleable `bun <file>` / `bun run
/// <file>` invocation? Returns the file argument if so, `None` for anything
/// else (CLI wrappers like `bunx --bun next start`, non-Bun commands, etc.).
/// Extracted so the decision is unit-testable without a `CloudState`.
fn bun_bundle_entry(start_cmd: &[String]) -> Option<String> {
    let is_bun = |s: &str| Path::new(s).file_name().and_then(|f| f.to_str()) == Some("bun");
    match start_cmd {
        [first, file] if is_bun(first) => Some(file.clone()),
        [first, run, file] if is_bun(first) && run == "run" => Some(file.clone()),
        _ => None,
    }
}

async fn warmup_bun_bytecode(
    build_dir: &Path,
    start_cmd: &[String],
    cloud: &Arc<CloudState>,
    bid: &str,
) -> Vec<String> {
    let log = |m: String| cloud.builds.log(bid, m);
    let original = start_cmd.to_vec();
    if std::env::var("HIVE_COMPILE_CACHE")
        .map(|v| v == "0" || v == "false")
        .unwrap_or(false)
    {
        return original;
    }
    let Some(entry_arg) = bun_bundle_entry(start_cmd) else {
        log(
            "Bytecode-cache: skipped — this start command runs Bun's own CLI wrapper \
             (e.g. `bunx --bun next start`), not a single bundleable entry file. Bun's \
             ahead-of-time bytecode cache needs a statically resolvable module graph, \
             which a framework CLI is not (a real, documented Bun limitation — the app \
             still runs correctly on Bun, just without this speedup)."
                .into(),
        );
        return original;
    };
    let entry_path = build_dir.join(&entry_arg);
    if !entry_path.is_file() {
        log(format!(
            "Bytecode-cache: skipped — entry `{entry_arg}` not found under the build output."
        ));
        return original;
    }
    let Some(bun_bin) = warmup_bun_bin() else {
        log("Bytecode-cache: skipped — no `bun` binary available on this build host.".into());
        return original;
    };
    let outdir = build_dir.join(".hive-bun-bytecode");
    if tokio::fs::create_dir_all(&outdir).await.is_err() {
        return original;
    }
    log(format!(
        "Bytecode-cache: bundling `{entry_arg}` with Bun's ahead-of-time bytecode cache…"
    ));
    let out = Command::new(&bun_bin)
        .arg("build")
        .arg("--bytecode")
        .arg("--sourcemap=external")
        .arg("--target=bun")
        .arg(format!("--outdir={}", outdir.to_string_lossy()))
        .arg(&entry_path)
        .current_dir(build_dir)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let stem = Path::new(&entry_arg)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("server");
            let bundled = outdir.join(format!("{stem}.js"));
            if !bundled.is_file() {
                log("Bytecode-cache: bun build reported success but the expected output file was missing — using the original entry uncached.".into());
                return original;
            }
            let rel = bundled
                .strip_prefix(build_dir)
                .unwrap_or(&bundled)
                .to_string_lossy()
                .into_owned();
            let ver = bun_version(&bun_bin).await.unwrap_or_else(|| "?".into());
            log(format!("Bytecode-cache: bundled + precompiled `{entry_arg}` -> `{rel}` (bun {ver}, with external source map)."));
            vec!["bun".to_string(), "run".to_string(), rel]
        }
        Ok(o) => {
            let stderr: String = String::from_utf8_lossy(&o.stderr)
                .trim()
                .chars()
                .take(300)
                .collect();
            log(format!("Bytecode-cache: bun build failed ({stderr}); using the original entry uncached — app still starts normally."));
            original
        }
        Err(e) => {
            log(format!("Bytecode-cache: could not run `bun build` ({e}); using the original entry uncached."));
            original
        }
    }
}

/// Run a shell command in `dir`, streaming stdout+stderr into the build log.
/// `env` is the project's environment variables, injected into the build so
/// install/build steps (e.g. Next.js reading NEXT_PUBLIC_*, Vite VITE_*) see them.
/// Ingest a Vercel WDK manifest (`.well-known/workflow/v1/manifest.json`) emitted
/// by a built app: register each workflow — with its React-Flow `graph` — in the
/// engine so it shows up in the Workflows tab/table and renders on the canvas.
/// Returns the number registered. Best-effort (a non-WDK app simply has none).
async fn ingest_workflow_manifest(cloud: &Arc<CloudState>, project: &str, dir: &Path) -> usize {
    let Some(path) = find_workflow_manifest(dir) else {
        return 0;
    };
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return 0;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
        return 0;
    };
    let Some(workflows) = manifest.get("workflows").and_then(|v| v.as_object()) else {
        return 0;
    };
    let mut count = 0usize;
    for defs in workflows.values() {
        let Some(defs) = defs.as_object() else {
            continue;
        };
        for (name, wf) in defs {
            let id = wf
                .get("workflowId")
                .and_then(|v| v.as_str())
                .unwrap_or(name)
                .to_string();
            let graph = wf.get("graph").cloned();
            // Steps for the table = graph nodes minus the synthetic start/end markers.
            let steps: Vec<hive_edge::WorkflowStep> = graph
                .as_ref()
                .and_then(|g| g.get("nodes"))
                .and_then(|n| n.as_array())
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter(|node| {
                            let kind = node
                                .get("data")
                                .and_then(|d| d.get("nodeKind"))
                                .and_then(|k| k.as_str())
                                .unwrap_or("");
                            kind != "workflow_start" && kind != "workflow_end"
                        })
                        .map(|node| {
                            let label = node
                                .get("data")
                                .and_then(|d| d.get("label"))
                                .and_then(|l| l.as_str())
                                .or_else(|| node.get("id").and_then(|i| i.as_str()))
                                .unwrap_or("step")
                                .to_string();
                            hive_edge::WorkflowStep {
                                name: label,
                                deployment: project.to_string(),
                                path: String::new(),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            cloud.workflows.define(hive_edge::WorkflowDef {
                id,
                name: name.clone(),
                project: project.to_string(),
                steps,
                graph,
            });
            count += 1;
        }
    }
    count
}

/// Locate a WDK `manifest.json` under a build dir (bounded walk; skips vendored /
/// build-output trees that would duplicate it). Prefers the canonical relative
/// path at each level.
fn find_workflow_manifest(dir: &Path) -> Option<std::path::PathBuf> {
    fn walk(dir: &Path, depth: usize) -> Option<std::path::PathBuf> {
        let direct = dir.join(".well-known/workflow/v1/manifest.json");
        if direct.is_file() {
            return Some(direct);
        }
        if depth >= 6 {
            return None;
        }
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(
                    name,
                    "node_modules"
                        | ".git"
                        | ".next"
                        | "dist"
                        | "build"
                        | ".svelte-kit"
                        | ".vercel"
                ) {
                    continue;
                }
                if let Some(hit) = walk(&p, depth + 1) {
                    return Some(hit);
                }
            }
        }
        None
    }
    walk(dir, 0)
}

async fn run_streamed(
    dir: &Path,
    command: &str,
    cloud: &Arc<CloudState>,
    bid: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<()> {
    use std::process::Stdio;
    use tokio::io::BufReader;

    // Put the project's local CLIs (node_modules/.bin) first, then a STABLE Node
    // 20–24, then system paths. This ensures `node`/`npm` are a supported version
    // (not Homebrew's node 26 canary) so engine-gated frameworks build.
    let local_bin = dir.join("node_modules/.bin");
    let mut prefix = local_bin.to_string_lossy().into_owned();
    if let Some(nd) = preferred_node_bin() {
        prefix.push(':');
        prefix.push_str(&nd);
    }
    let path = format!(
        "{prefix}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{}",
        std::env::var("PATH").unwrap_or_default()
    );
    // Non-login shell (`-c`, not `-lc`): a login shell re-runs macOS `path_helper`
    // / profile scripts which reorder PATH and shove Homebrew's node (v26 canary)
    // back in front of our chosen stable Node 20–24. `-c` preserves our PATH.
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .env("PATH", &path)
        .env("HOME", dir)
        .env("CI", "1")
        .env("NEXT_TELEMETRY_DISABLED", "1")
        // Never fail a build on EBADENGINE — warn-only — and quiet npm noise.
        .env("npm_config_engine_strict", "false")
        .env("npm_config_fund", "false")
        .env("npm_config_audit", "false")
        // Project env vars — injected so the build can read them (last so they win).
        .envs(env)
        // Own process group (pgid == this pid): `npm install`/`npm run build`
        // fork `npm`/`node`/framework-worker children that inherit this shell's
        // group, so `cancel_build`'s group-kill (`kill -KILL -<pgid>`) reaches
        // the whole tree, not just this one `/bin/sh`.
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    cloud.build_cancels.set_running(bid, child.id());
    if cloud.build_cancels.is_cancelled(bid) {
        // A cancel landed in the tiny window between the caller's last
        // cooperative check and this spawn — kill it now.
        let _ = child.start_kill();
    }

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    // Redact every decrypted project env VALUE out of build output before it's
    // persisted to the (team-readable, indefinitely-retained) build log. The
    // build process runs the repo's OWN install/build commands with these
    // values injected — a build script that echoes env (verbose npm/yarn
    // dumps, a framework build failure printing `process.env`, or a
    // one-line `"prebuild": "env"` in a PR from anyone with push access) used
    // to leak every real secret in cleartext. Mirrors the identical
    // `sandboxes_platform.rs` exec-output redaction (`secret_values`/
    // `redact_secrets`) — same utility, same "redact every value, not just
    // ones flagged sensitive" conservative default.
    let secret_values: Vec<String> = env.values().filter(|v| !v.is_empty()).cloned().collect();
    // BOUNDED capture. `BufReader::lines()`/`next_line()` splits only on `\n`
    // and has no length limit, so a build that writes without newlines — a
    // `\r`-only progress bar (npm/pnpm/pip/curl/`podman pull`), a single-line
    // source map, a binary accidentally sent to stdout — grew ONE String until
    // the node died, taking every other tenant on it. Worse here than at the
    // other capture sites: each line is then `redact_secrets`'d (a second full
    // copy) and `format!`'d (a third), so an N-byte line cost ~3N. The build
    // command is tenant-supplied, which makes an unbounded read a
    // multi-tenant availability hole, not just a leak.
    //
    // `read_capped_line` still DRAINS the pipe (the child must never block on
    // us) but retains at most MAX_LOG_LINE_BYTES, and reports what it dropped.
    let (c1, b1, s1) = (cloud.clone(), bid.to_string(), secret_values.clone());
    let t1 = tokio::spawn(async move {
        let mut r = BufReader::new(stdout);
        while let Ok(Some(l)) =
            hive_core::logcap::read_capped_line(&mut r, hive_core::MAX_LOG_LINE_BYTES).await
        {
            c1.builds.log(
                &b1,
                format!("  {}", crate::sandboxes::redact_secrets(&l.text, &s1)),
            );
        }
    });
    let (c2, b2, s2) = (cloud.clone(), bid.to_string(), secret_values);
    let t2 = tokio::spawn(async move {
        let mut r = BufReader::new(stderr);
        while let Ok(Some(l)) =
            hive_core::logcap::read_capped_line(&mut r, hive_core::MAX_LOG_LINE_BYTES).await
        {
            c2.builds.log(
                &b2,
                format!("  {}", crate::sandboxes::redact_secrets(&l.text, &s2)),
            );
        }
    });
    let status = child.wait().await;
    cloud.build_cancels.set_running(bid, None);
    let status = status?;
    let _ = tokio::join!(t1, t2);
    if !status.success() && cloud.build_cancels.is_cancelled(bid) {
        return Err(BuildCancelled.into());
    }
    anyhow::ensure!(status.success(), "exited with {status}");
    Ok(())
}

// ---- Build cache (content-addressed, P2P, fault-tolerant) ----
//
// Dependencies are the slow part of a build. We cache `node_modules` (+ framework
// caches) as a tarball keyed by a content hash of the lockfile + package manager,
// stored under `$HIVE_DATA/build-cache/<key>.tar`. Restore before install; save
// after a successful build. On a LOCAL miss we pull the blob from a mesh peer
// (`GET /v1/buildcache/:key`) — the P2P paradigm: any node that has built these
// deps can serve them to the others. Every step is best-effort: a cache error
// (missing, corrupt, peer down) never fails the build — it just falls back to a
// clean install.

pub fn cache_root() -> PathBuf {
    crate::persist::data_dir().join("build-cache")
}

/// Content hash of the lockfile + package manager → cache key. None if there's
/// nothing installable to key on.
async fn compute_cache_key(install_dir: &Path, pm: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pm.as_bytes());
    let mut found = false;
    for name in [
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lockb",
        "package-lock.json",
        "package.json",
    ] {
        if let Ok(bytes) = tokio::fs::read(install_dir.join(name)).await {
            hasher.update(name.as_bytes());
            hasher.update(&bytes);
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    let digest = hasher.finalize();
    Some(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

// ---- #22 build-artifact integrity + authenticity --------------------------
//
// A node pulls `node_modules` tarballs from mesh peers (`try_peer_fetch`). Without
// verification, a corrupted byte stream — or a malicious peer — could inject
// arbitrary content into a build (a supply-chain hole). We protect every pull with
// two checks, both using primitives already in the tree (`sha2`, `hmac`):
//   * a SHA-256 CONTENT digest (`x-hive-artifact-sha256`) catches CORRUPTION —
//     always enforced when the header is present (works with no shared secret).
//   * an HMAC-SHA256 SIGNATURE (`x-hive-artifact-sig`) over the bytes, keyed by a
//     fleet-shared secret, catches FORGERY — a peer without the secret can't mint a
//     valid signature. Enforced whenever a secret is configured on this node.

pub const ARTIFACT_SHA_HEADER: &str = "x-hive-artifact-sha256";
pub const ARTIFACT_SIG_HEADER: &str = "x-hive-artifact-sig";

/// Fleet-shared secret for artifact signatures (#22). Reuses `HIVE_JWT_SECRET`
/// (already distributed fleet-wide) unless `HIVE_ARTIFACT_SECRET` overrides it.
/// `None` => signing disabled (dev / single-node), like the JWT dev-open default.
pub fn artifact_secret() -> Option<String> {
    if let Some(s) = std::env::var("HIVE_ARTIFACT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return Some(s);
    }
    // Fall back to a key DERIVED from HIVE_JWT_SECRET (a single HMAC call
    // with a purpose-specific label — HKDF-Expand in spirit) rather than the
    // raw JWT secret itself. Reusing the exact same symmetric key for two
    // different HMAC purposes (session-token signing and mesh build-artifact
    // integrity signing) violates "one key, one purpose": compromise of one
    // subsystem's key material compromises the other. Set
    // HIVE_ARTIFACT_SECRET explicitly in production for full independence.
    let root = std::env::var("HIVE_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(artifact_sig(&root, b"hive-artifact-signing-v1"))
}

/// Lowercase hex SHA-256 of `bytes` (#22 content digest).
pub fn artifact_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex HMAC-SHA256(secret, bytes) (#22 authenticity signature).
pub fn artifact_sig(secret: &str, bytes: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key len");
    mac.update(bytes);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Constant-time verify of an HMAC artifact signature (#22).
pub fn artifact_sig_valid(secret: &str, bytes: &[u8], sig_hex: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let Some(expected) = hex_decode(sig_hex) else {
        return false;
    };
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key len");
    mac.update(bytes);
    mac.verify_slice(&expected).is_ok() // constant-time comparison
}

/// Verify a pulled artifact against the peer's integrity/authenticity headers (#22).
/// Returns Ok(()) if the bytes are trustworthy, Err(reason) otherwise. `sha`/`sig`
/// are the header values the peer sent (None if absent).
fn verify_pulled_artifact(
    bytes: &[u8],
    sha: Option<&str>,
    sig: Option<&str>,
) -> Result<(), String> {
    // Content digest: if present, it MUST match (corruption guard).
    if let Some(sha) = sha {
        if !artifact_sha256(bytes).eq_ignore_ascii_case(sha) {
            return Err("content sha256 mismatch (corrupted artifact)".into());
        }
    }
    // Authenticity: if THIS node has a secret configured, a valid signature is
    // REQUIRED — a peer that can't sign is not trusted to supply build inputs.
    if let Some(secret) = artifact_secret() {
        match sig {
            Some(sig) if artifact_sig_valid(&secret, bytes, sig) => {}
            Some(_) => return Err("artifact signature invalid (untrusted/forged)".into()),
            None => return Err("artifact signature missing (peer can't authenticate)".into()),
        }
    }
    Ok(())
}

/// Try to fetch a cache blob from a mesh peer; write it to `dest`. Returns true on
/// success. Best-effort: any error is swallowed. Verifies integrity + authenticity
/// of the pulled bytes (#22) before accepting — a failed check rejects that peer's
/// copy and tries the next, never writing untrusted bytes to the cache.
async fn try_peer_fetch(cloud: &Arc<CloudState>, key: &str, dest: &Path) -> bool {
    let peers = cloud.peers.read().clone();
    for peer in peers {
        let url = format!("{}/v1/buildcache/{}", peer.trim_end_matches('/'), key);
        if let Ok(resp) = cloud
            .http
            .get(&url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
        {
            if resp.status().is_success() {
                let sha = resp
                    .headers()
                    .get(ARTIFACT_SHA_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                let sig = resp
                    .headers()
                    .get(ARTIFACT_SIG_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                if let Ok(bytes) = resp.bytes().await {
                    if let Err(reason) =
                        verify_pulled_artifact(&bytes, sha.as_deref(), sig.as_deref())
                    {
                        tracing::warn!(peer = %peer, key = %key, %reason, "rejected untrusted build artifact (#22)");
                        continue; // never write unverified bytes; try the next peer
                    }
                    if tokio::fs::create_dir_all(cache_root()).await.is_ok()
                        && tokio::fs::write(dest, &bytes).await.is_ok()
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Restore node_modules from the cache (local, else peer). Returns true if restored.
async fn restore_cache(cloud: &Arc<CloudState>, bid: &str, install_dir: &Path, key: &str) -> bool {
    let tar = cache_root().join(format!("{key}.tar"));
    if !tar.exists() && try_peer_fetch(cloud, key, &tar).await {
        cloud
            .builds
            .log(bid, format!("Pulled build cache from peer (key {key})."));
    }
    if !tar.exists() {
        cloud.builds.log(
            bid,
            "No build cache for these dependencies — installing fresh.",
        );
        return false;
    }
    let out = Command::new("tar")
        .arg("-xf")
        .arg(&tar)
        .current_dir(install_dir)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            cloud
                .builds
                .log(bid, format!("Restored build cache (key {key})."));
            true
        }
        _ => {
            // Corrupt/incompatible archive → drop it and install clean.
            let _ = tokio::fs::remove_file(&tar).await;
            cloud.builds.log(
                bid,
                "Build cache was unreadable — discarded; installing fresh.",
            );
            false
        }
    }
}

/// Save node_modules (+ framework cache if present) to the content-addressed cache.
/// Best-effort, atomic (write temp + rename). The temp name carries the build id:
/// the cache key is a CONTENT hash, so two concurrent builds with the same
/// lockfile (same project redeployed twice, or two projects with identical deps)
/// share a key — an un-suffixed `<key>.tar.tmp` had both `tar -cf` writers
/// interleaving one file, and the loser's rename could then install a torn
/// archive as the cached artifact.
async fn save_cache(cloud: &Arc<CloudState>, bid: &str, install_dir: &Path, key: &str) {
    if !install_dir.join("node_modules").exists() {
        return;
    }
    let dir = cache_root();
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return;
    }
    let tmp = dir.join(format!("{key}.{bid}.tar.tmp"));
    let final_ = dir.join(format!("{key}.tar"));
    // Include the framework's incremental cache too when it lives under the
    // install dir (e.g. node_modules/.cache); .next/cache lives in node_modules
    // for many setups but we keep this list conservative for portability.
    let mut args: Vec<String> = vec![
        "-cf".into(),
        tmp.to_string_lossy().into_owned(),
        "node_modules".into(),
    ];
    if install_dir.join(".next/cache").exists() {
        args.push(".next/cache".into());
    }
    let out = Command::new("tar")
        .args(&args)
        .current_dir(install_dir)
        .output()
        .await;
    if let Ok(o) = out {
        if o.status.success() && tokio::fs::rename(&tmp, &final_).await.is_ok() {
            cloud.builds.log(bid, "Saved build cache for next time.");
            return;
        }
    }
    let _ = tokio::fs::remove_file(&tmp).await;
}

/// Locate a repo's container build file: a `Dockerfile` or its identical twin
/// `Containerfile` (the vendor-neutral OCI/Buildah/Podman name — byte-for-byte the
/// same format and instructions). Returns the path to whichever exists, preferring
/// `Dockerfile` when both are present (the more common name; either builds the same).
fn container_build_file(dir: &Path) -> Option<PathBuf> {
    ["Dockerfile", "Containerfile"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.exists())
}

/// Parse the container's listen port from a Dockerfile: prefer `EXPOSE`, else
/// `ENV PORT=`, else None (caller defaults).
async fn parse_expose(path: &Path) -> Option<u16> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let mut env_port = None;
    for line in content.lines() {
        let l = line.trim();
        if let Some(rest) = l
            .strip_prefix("EXPOSE ")
            .or_else(|| l.strip_prefix("expose "))
        {
            if let Some(p) = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.split('/').next())
            {
                if let Ok(n) = p.parse::<u16>() {
                    return Some(n);
                }
            }
        }
        let lu = l.to_uppercase();
        if lu.starts_with("ENV PORT=") || lu.starts_with("ENV PORT ") {
            if let Some(p) = l.split(|c| c == '=' || c == ' ').last() {
                if let Ok(n) = p.trim().parse::<u16>() {
                    env_port = Some(n);
                }
            }
        }
    }
    env_port
}

/// A container deployment: the function "process" is `podman run`. The app is
/// told to listen on `internal` (via PORT env) and we publish the cell's $PORT →
/// that internal port, so the gateway proxies to 127.0.0.1:$PORT.
/// Merge a parsed `vercel.json` into the deployment manifest: routing
/// (redirects/rewrites/headers, prepended so vercel.json wins), cleanUrls,
/// trailingSlash, images, crons, and per-function overrides (matched by glob).
fn apply_vercel_config(m: &mut Manifest, vc: &fluid_build::VercelConfig, log: &dyn Fn(String)) {
    use fluid_core::{
        redirect_status, CondValue, CronSpec, Header, HeaderRule, ImagesConfig, LocalPattern,
        Redirect, RemotePattern, Rewrite, RuleCondition,
    };

    let conv_conds = |cs: &[fluid_build::VercelCondition]| -> Vec<RuleCondition> {
        cs.iter()
            .map(|c| RuleCondition {
                kind: c.kind.clone(),
                key: c.key.clone(),
                value: c.value.as_ref().map(|v| match v {
                    fluid_build::ConditionValue::Text(t) => CondValue::Text(t.clone()),
                    fluid_build::ConditionValue::Expr { pre, suf } => CondValue::Expr {
                        pre: pre.clone(),
                        suf: suf.clone(),
                    },
                }),
            })
            .collect()
    };

    // Redirects (vercel.json first → highest precedence).
    if !vc.redirects.is_empty() {
        let mut conv: Vec<Redirect> = vc
            .redirects
            .iter()
            .map(|r| Redirect {
                source: r.source.clone(),
                destination: r.destination.clone(),
                status: redirect_status(r.permanent, r.status_code),
                has: conv_conds(&r.has),
                missing: conv_conds(&r.missing),
            })
            .collect();
        conv.append(&mut m.redirects);
        m.redirects = conv;
    }

    // Rewrites (vercel.json first).
    if !vc.rewrites.is_empty() {
        let mut conv: Vec<Rewrite> = vc
            .rewrites
            .iter()
            .map(|r| Rewrite {
                source: r.source.clone(),
                destination: r.destination.clone(),
                has: conv_conds(&r.has),
                missing: conv_conds(&r.missing),
            })
            .collect();
        conv.append(&mut m.rewrites);
        m.rewrites = conv;
    }

    // Response headers.
    if !vc.headers.is_empty() {
        m.headers = vc
            .headers
            .iter()
            .map(|h| HeaderRule {
                source: h.source.clone(),
                headers: h
                    .headers
                    .iter()
                    .map(|x| Header {
                        key: x.key.clone(),
                        value: x.value.clone(),
                    })
                    .collect(),
                has: conv_conds(&h.has),
                missing: conv_conds(&h.missing),
            })
            .collect();
    }

    if let Some(cu) = vc.clean_urls {
        m.clean_urls = cu;
    }
    if vc.trailing_slash.is_some() {
        m.trailing_slash = vc.trailing_slash;
    }

    if let Some(img) = &vc.images {
        m.images = Some(ImagesConfig {
            sizes: img.sizes.clone(),
            qualities: img.qualities.clone(),
            formats: img.formats.clone(),
            minimum_cache_ttl: img.minimum_cache_ttl,
            domains: img.domains.clone(),
            remote_patterns: img
                .remote_patterns
                .iter()
                .map(|p| RemotePattern {
                    protocol: p.protocol.clone(),
                    hostname: p.hostname.clone(),
                    port: p.port.clone(),
                    pathname: p.pathname.clone(),
                    search: p.search.clone(),
                })
                .collect(),
            local_patterns: img
                .local_patterns
                .iter()
                .map(|p| LocalPattern {
                    pathname: p.pathname.clone(),
                    search: p.search.clone(),
                })
                .collect(),
            dangerously_allow_svg: img.dangerously_allow_svg,
            content_security_policy: img.content_security_policy.clone(),
            content_disposition_type: img.content_disposition_type.clone(),
        });
    }

    if !vc.crons.is_empty() {
        m.crons = vc
            .crons
            .iter()
            .map(|c| CronSpec {
                path: c.path.clone(),
                schedule: c.schedule.clone(),
            })
            .collect();
    }

    // Per-function overrides (glob → matched functions).
    for (glob, fnc) in &vc.functions {
        for f in m.functions.iter_mut() {
            if glob_match(glob, &f.name) {
                if let Some(d) = fnc.max_duration {
                    f.max_duration_secs = d;
                }
                if let Some(mem) = fnc.memory {
                    f.memory_mib = mem;
                    // Vercel scales CPU with memory; >2 GB ⇒ Performance tier.
                    f.vcpus = if mem > 2048 { 2 } else { 1 };
                }
                if !fnc.regions.is_empty() {
                    f.regions = fnc.regions.clone();
                }
                if let Some(inc) = &fnc.include_files {
                    f.include_files = Some(inc.clone());
                }
                if let Some(exc) = &fnc.exclude_files {
                    f.exclude_files = Some(exc.clone());
                }
                if let Some(rt) = &fnc.runtime {
                    f.runtime = rt.clone();
                }
            }
        }
    }

    // Project-level regions apply to any function without its own preference.
    if !vc.regions.is_empty() {
        for f in m.functions.iter_mut() {
            if f.regions.is_empty() {
                f.regions = vc.regions.clone();
            }
        }
    }

    log(format!(
        "vercel.json merged: {} redirect(s), {} rewrite(s), {} header rule(s), cleanUrls={}, trailingSlash={:?}, {} cron(s), images={}.",
        m.redirects.len(),
        m.rewrites.len(),
        m.headers.len(),
        m.clean_urls,
        m.trailing_slash,
        m.crons.len(),
        m.images.is_some(),
    ));
}

/// Convert a Vercel 5-field cron (`min hour dom mon dow`) to the scheduler's
/// 6-field form (`sec min hour dom mon dow`) by prepending a 0-second field.
/// Already-6-field expressions pass through unchanged.
fn to_six_field_cron(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 {
        format!("0 {}", expr.trim())
    } else {
        expr.trim().to_string()
    }
}

/// Glob match for `vercel.json` `functions` keys against a function name.
/// Supports `*` (within a path segment) and `**` (across segments). Because our
/// function names are extension-less (e.g. `api/hello`), a trailing file
/// extension on the pattern (e.g. `api/*.js`) is also tried with the extension
/// stripped.
fn glob_match(pattern: &str, name: &str) -> bool {
    if wild(pattern.as_bytes(), name.as_bytes()) {
        return true;
    }
    if let Some(dot) = pattern.rfind('.') {
        if !pattern[dot..].contains('/') {
            return wild(pattern[..dot].as_bytes(), name.as_bytes());
        }
    }
    false
}

/// Recursive wildcard matcher. `*` matches any run NOT crossing `/`; `**`
/// matches any run including `/`. Recursion is bounded by pattern length.
fn wild(p: &[u8], s: &[u8]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    if p[0] == b'*' {
        let dbl = p.len() > 1 && p[1] == b'*';
        let rest = if dbl { &p[2..] } else { &p[1..] };
        let mut i = 0;
        loop {
            if wild(rest, &s[i..]) {
                return true;
            }
            if i >= s.len() {
                return false;
            }
            // A single `*` may not consume `/`.
            if !dbl && s[i] == b'/' {
                return false;
            }
            i += 1;
        }
    } else if !s.is_empty() && p[0] == s[0] {
        wild(&p[1..], &s[1..])
    } else {
        false
    }
}

/// The mount point for the automatic per-container persistent volume (env-tunable).
fn container_volume_path() -> String {
    std::env::var("HIVE_CONTAINER_VOLUME_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/data".to_string())
}

/// The run-config JSON (container `start_cmd[3]`) that attaches an automatic
/// persistent volume: a host-backed named volume (≥1 GB) keyed STABLY per project
/// (+ optional service suffix for compose), so a container's data survives instance
/// restarts and redeploys. Merged into the compose network cfg when present.
fn container_volume_cfg(project: &str, service: Option<&str>) -> String {
    let mut name = format!("hive-vol-{}", sanitize_tag(project));
    if let Some(svc) = service.filter(|s| !s.is_empty()) {
        name.push('-');
        name.push_str(&sanitize_tag(svc));
    }
    // TENANT/PROJECT NETWORK ISOLATION: standalone containers also get their own
    // per-project DNS-less podman network (same deterministic subnet scheme as
    // compose). Without this they land on podman's shared default bridge, where
    // ANY tenant's container can reach ANY other's by bridge IP. No static `ip`
    // is pinned (unlike compose) so scale-out instances coexist; the backend
    // assigns dynamic addresses within the project subnet.
    let (net, subnet, gw) = project_net(project);
    serde_json::json!({
        "vol": name,
        "volpath": container_volume_path(),
        "net": net,
        "subnet": subnet,
        "gw": gw,
    })
    .to_string()
}

/// Deterministic per-project podman network (name, /24 subnet, gateway) in the
/// 10.128-191/16 space — identical scheme to the compose path so a project keeps
/// one network regardless of how it's deployed. Shared with the databases module
/// so a managed DB container joins the SAME network the app containers use (and
/// is reachable by its network-alias).
pub(crate) fn project_net(project: &str) -> (String, String, String) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    project.hash(&mut h);
    let v = h.finish();
    let (o2, o3) = (128 + (v % 64) as u8, ((v >> 6) % 256) as u8);
    (
        format!("hive-net-{}", sanitize_tag(project)),
        format!("10.{o2}.{o3}.0/24"),
        format!("10.{o2}.{o3}.1"),
    )
}

fn container_manifest(
    project: &str,
    image: &str,
    internal: u16,
    protocol: &str,
    memory_mib: u32,
    cpus: f64,
    pids: u32,
) -> Manifest {
    // `protocol` is a free-form string here (Railway-style `fluid.json` override for
    // the Dockerfile-build path, or an already-resolved wire string from an image
    // deploy) — parse it into the typed enum, falling back to the http default for
    // anything unrecognized rather than rejecting the build outright (this synthesis
    // step has never validated the string; `FunctionConfig`'s own JSON deserialize
    // boundary is where a genuinely malformed value is rejected hard).
    let proto: ServiceProtocol = protocol.parse().unwrap_or_default();
    Manifest {
        project: project.to_string(),
        static_dir: None,
        functions: vec![FunctionConfig {
            name: "web".into(),
            runtime: "container".into(),
            // Structured marker the backend recognizes: run this image as a
            // detached container, mapping the cell $PORT -> internal port.
            // start_cmd[3] = run-config JSON (here: the automatic persistent volume).
            start_cmd: vec![
                "__container__".into(),
                image.to_string(),
                internal.to_string(),
                container_volume_cfg(project, None),
            ],
            env: Default::default(),
            vcpus: 1,
            // 0/0.0 = use the node's generous, env-tunable container defaults (a
            // real value here comes from fluid.json `container.memory`/`cpus`/
            // `pids` or an `ImageDeployReq`'s equivalents). NOT the old hardcoded
            // 512m, which OOM-killed monolith containers.
            memory_mib,
            cpus,
            pids,
            max_concurrency: 20,
            min_instances: 1,
            max_instances: 5,
            idle_ttl_secs: 120,
            max_duration_secs: 300,
            protocol: proto,
            // Bridge the single port this function actually runs into the
            // forward-looking multi-port list (see `PortSpec::from_legacy_port`'s
            // doc comment — this call site is exactly the bridging it names) so a
            // stored deployment record always carries a recoverable port+protocol,
            // e.g. for `image_port_spec_for_project_fleet` on redeploy.
            ports: PortSpec::from_legacy_port(Some(internal), proto),
            ..Default::default()
        }],
        // A raw-protocol (tcp/udp/grpc) deployment has no HTTP request to
        // route — its gateway leg would tunnel-frame into what
        // `FunctionLaunch::raw_proxy` now serves as a pure byte splice
        // (garbage either way), and it's reached through its allocated raw
        // public port instead (`raw_ports`/`raw_proxy`), never `/`. Skip
        // creating the meaningless HTTP route rather than advertising an
        // endpoint that was never really there.
        routes: if proto.needs_raw_proxy() {
            Vec::new()
        } else {
            vec![Route {
                pattern: "/".into(),
                target: RouteTarget::Function("web".into()),
            }]
        },
        ..Default::default()
    }
}

/// The PATH podman needs on both Linux FC hosts (/usr/bin) and macOS (/opt/homebrew).
pub(crate) fn podman_path_env() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    format!("/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:{base}")
}

/// Fully qualify a container-image reference for podman. Linux podman ENFORCES
/// short-name resolution (`user/img:tag` fails with "short-name resolution
/// enforced" unless registries.conf lists search registries — macOS podman-machine
/// resolves docker.io implicitly, which is why short names work there). A ref whose
/// first path component isn't a registry host (no `.`/`:`, not `localhost`) gets
/// the Docker Hub prefix; single-component names also get the `library/` namespace.
pub(crate) fn qualify_image_ref(image: &str) -> String {
    let image = image.trim();
    match image.split_once('/') {
        // No slash: an official Docker Hub image (`nginx`, `redis:7`) — the `:` here
        // is a TAG, never a host:port (that only occurs before a `/`).
        None => format!("docker.io/library/{image}"),
        Some((first, _)) => {
            // Slash present: `first` is a registry host iff it looks like one
            // (dots, a port, or `localhost`); otherwise it's a Hub namespace.
            let is_registry_host =
                first.contains('.') || first.contains(':') || first == "localhost";
            if is_registry_host {
                image.to_string()
            } else {
                format!("docker.io/{image}") // user/img[:tag]
            }
        }
    }
}

/// Deploy a PRE-BUILT OCI image from any registry (Docker Hub / Quay / arbitrary):
/// `podman pull` it on this (target) node, auto-detect its listening port from the
/// image's `ExposedPorts` (unless overridden), and synthesize a container manifest
/// with the automatic persistent volume. Project env is injected by the caller.
///
/// Podman-only, even on macOS (unlike the single-Dockerfile build path above,
/// which does use Apple's `container` there): verified live that Apple's
/// `container image inspect` does not expose the OCI `Config.ExposedPorts`
/// field at all (podman: `{"80/tcp":{}}` for nginx; the identical field is
/// simply absent from `container`'s inspect output for the same image) — so
/// port auto-detection, a real shipped feature for registry-image deploys
/// without an explicit override, would silently stop working. See
/// `hive_backend::container_cli`'s module doc for the general policy.
async fn image_container_manifest(
    cloud: &Arc<CloudState>,
    bid: &str,
    project: &str,
    image: &str,
    port_override: Option<u16>,
    protocol_override: Option<ServiceProtocol>,
    memory_mib: u32,
    cpus: f64,
    pids: u32,
    ports_override: Option<Vec<PortSpec>>,
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);
    let path = podman_path_env();
    // Fully qualify short names (`user/img` → `docker.io/user/img`) — Linux podman
    // rejects unqualified refs ("short-name resolution enforced").
    let qualified = qualify_image_ref(image);
    let image = qualified.as_str();
    // Pull the image (fail the build with the registry error if it can't be fetched —
    // e.g. not found / private registry needing auth).
    log(format!("Pulling image {image} …"));
    let t0 = now_ms();
    let out = Command::new("podman")
        .args(["pull", image])
        .env("PATH", &path)
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("pull failed");
        anyhow::bail!("podman pull {image} failed: {}", msg.trim());
    }
    log(format!(
        "Pulled {image} in {}ms",
        now_ms().saturating_sub(t0)
    ));

    // Port + protocol: explicit values win outright. Otherwise auto-detect from the
    // image's own `ExposedPorts` (falling back to 8080/http when nothing is exposed
    // either) — `detect_image_port` now surfaces the actual detected protocol too,
    // so a UDP-only image (e.g. Minecraft Bedrock, 19132/udp, no TCP port at all) no
    // longer gets forced through the http default it can't actually speak. An
    // explicit port WITHOUT an explicit protocol assumes http (the pre-existing
    // behavior for an overridden port) rather than cross-referencing the detected
    // port's protocol, which could belong to a DIFFERENT exposed port than the one
    // the caller just asked for.
    let (port, protocol) = match (port_override, protocol_override) {
        (Some(p), Some(proto)) => (p, proto),
        (Some(p), None) => (p, ServiceProtocol::Http),
        (None, protocol_override) => match detect_image_port(&path, image).await {
            Some(spec) => (
                spec.container_port,
                protocol_override.unwrap_or(spec.protocol),
            ),
            None => (8080, protocol_override.unwrap_or_default()),
        },
    };
    log(format!(
        "Container port {port}/{protocol}{}. Attaching persistent volume (≥1 GB) at {}.",
        if port_override.is_some() || protocol_override.is_some() {
            " (configured)"
        } else {
            " (from image ExposedPorts)"
        },
        container_volume_path(),
    ));
    let mut manifest = container_manifest(
        project,
        image,
        port,
        protocol.as_str(),
        memory_mib,
        cpus,
        pids,
    );
    // A full multi-port declaration REPLACES the single-port ports list built
    // above (the first entry is still the primary — `start_cmd[2]`/`protocol`
    // above already reflect it, since callers pass the primary as `port`/
    // `protocol_override` too). Only raw-protocol specs get a public
    // allocation (`raw_ports::allocate_raw_ports_coordinated`); an http/https
    // secondary entry here just documents an extra exposed port with no
    // public ingress, same as a compose service's un-exposed ports.
    if let Some(ports) = ports_override.filter(|p| !p.is_empty()) {
        log(format!(
            "Declared {} additional port(s): {}.",
            ports.len().saturating_sub(1),
            ports
                .iter()
                .map(|p| format!("{}/{}", p.container_port, p.protocol))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if let Some(f) = manifest.functions.first_mut() {
            // Keep `protocol` (drives needs_raw_proxy()/routing for the whole
            // function) in sync with the declared PRIMARY entry, in case a
            // caller's primary protocol here disagrees with the single
            // `protocol_override`/detected value used above.
            if let Some(primary) = ports.first() {
                f.protocol = primary.protocol;
            }
            f.ports = ports;
        }
    }
    Ok(manifest)
}

/// Auto-detect a container's listening port + protocol from the image's
/// `Config.ExposedPorts` (`podman image inspect`). See [`parse_exposed_ports`] for
/// the selection order (prefers TCP, falls back to UDP-only images).
async fn detect_image_port(path_env: &str, image: &str) -> Option<PortSpec> {
    let out = Command::new("podman")
        .args([
            "image",
            "inspect",
            image,
            "--format",
            "{{json .Config.ExposedPorts}}",
        ])
        .env("PATH", path_env)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_exposed_ports(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `podman image inspect --format '{{json .Config.ExposedPorts}}'` output
/// (e.g. `{"8080/tcp":{},"9090/tcp":{}}`) → the lowest exposed TCP port (protocol
/// `Http`, matching the pre-existing default for the common case). When the image
/// exposes NO tcp port at all, falls back to the lowest exposed UDP port instead
/// (protocol `Udp`) rather than discarding it — a UDP-only service (e.g. Minecraft
/// Bedrock, `19132/udp`, no TCP port) is otherwise impossible to auto-detect at all.
/// `null` / empty / unparseable → `None`.
fn parse_exposed_ports(json: &str) -> Option<PortSpec> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let obj = v.as_object()?;
    // Keys look like "8080/tcp" / "19132/udp".
    let mut tcp_ports: Vec<u16> = Vec::new();
    let mut udp_ports: Vec<u16> = Vec::new();
    for k in obj.keys() {
        let (num, proto) = k.split_once('/').unwrap_or((k.as_str(), "tcp"));
        let Ok(port) = num.parse::<u16>() else {
            continue;
        };
        if proto.eq_ignore_ascii_case("udp") {
            udp_ports.push(port);
        } else {
            tcp_ports.push(port);
        }
    }
    tcp_ports.sort_unstable();
    if let Some(p) = tcp_ports.first() {
        return Some(PortSpec::single(*p, ServiceProtocol::Http));
    }
    udp_ports.sort_unstable();
    udp_ports
        .first()
        .map(|p| PortSpec::single(*p, ServiceProtocol::Udp))
}

/// Build a MULTI-service container manifest from a docker-compose / compose.yaml.
/// Each service becomes a `__container__` `FunctionConfig`; all share a per-project
/// podman network (encoded as `start_cmd[3]`, with the service name as the alias in
/// `start_cmd[4]`) so they reach each other by name. `min_instances=1` keeps every
/// service warm so internal ones (db/redis) actually run. The PRIMARY public service
/// always gets the `/` route; every other service stays internal-only UNLESS it opts
/// in via `x-shadw-expose` (`ParsedService::expose` — see `compose.rs`), in which case
/// it gets its own `/<service-name>` route plus a raw TCP/UDP proxy target
/// (`FunctionConfig::ports`). Every service's `FunctionConfig::protocol` reflects its
/// parsed (or expose-overridden) `ParsedService::protocol` — no service silently
/// defaults to `http` just because the compose port suffix went unparsed.
async fn build_compose_manifest(
    cloud: &Arc<CloudState>,
    bid: &str,
    dir: &Path,
    project: &str,
    commit: &str,
    compose_path: &Path,
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);
    let text = tokio::fs::read_to_string(compose_path).await?;
    let services = crate::compose::parse_compose(&text)
        .map_err(|e| anyhow::anyhow!("compose parse failed: {e}"))?;
    let fname = compose_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("compose.yaml");
    let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
    log(format!(
        "Detected {fname} — {} service(s): {}",
        services.len(),
        names.join(", ")
    ));

    let safe_project = sanitize_tag(project);
    let net = format!("hive-net-{safe_project}");
    let short = &commit[..commit.len().min(7)];
    let primary = crate::compose::primary_service(&services).map(|s| s.name.clone());
    let proj_env = cloud.projects.env_map(project);

    // Per-project /24 (deterministic, in the 10.128-191/16 space to avoid the common
    // 10.0/10.89 podman ranges) for the services' shared DNS-LESS network: each
    // service gets a STATIC IP and every service's `/etc/hosts` maps all sibling
    // names → IPs. This gives Compose name resolution WITHOUT podman's aardvark-dns,
    // which would otherwise collide with the node's Seer DNS on :53.
    let (o2, o3) = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        project.hash(&mut h);
        let v = h.finish();
        (128 + (v % 64) as u8, ((v >> 6) % 256) as u8)
    };
    let subnet = format!("10.{o2}.{o3}.0/24");
    let gw = format!("10.{o2}.{o3}.1");
    let svc_ip = |i: usize| format!("10.{o2}.{o3}.{}", 11 + i);
    let hosts: Vec<String> = services
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}:{}", s.name, svc_ip(i)))
        .collect();

    let mut functions = Vec::new();
    let mut routes = Vec::new();
    for (idx, svc) in services.iter().enumerate() {
        let image = if let Some(b) = &svc.build {
            let ctx = dir.join(&b.context);
            let image = format!(
                "hive-{}-{}-{}",
                safe_project,
                sanitize_tag(&svc.name),
                short
            );
            log(format!(
                "Building service '{}' from {} …",
                svc.name, b.context
            ));
            let mut build = Command::new("podman");
            build.arg("build").arg("-t").arg(&image);
            if let Some(df) = &b.dockerfile {
                build.arg("-f").arg(ctx.join(df));
            }
            let base_path = std::env::var("PATH").unwrap_or_default();
            build.env(
                "PATH",
                format!(
                    "/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:{base_path}"
                ),
            );
            for (k, v) in &proj_env {
                build.arg("--build-arg").arg(format!("{k}={v}"));
            }
            let out = build.arg(".").current_dir(&ctx).output().await?;
            for line in String::from_utf8_lossy(&out.stderr)
                .lines()
                .chain(String::from_utf8_lossy(&out.stdout).lines())
                .filter(|l| !l.trim().is_empty())
                .take(25)
            {
                log(format!("  [{}] {line}", svc.name));
            }
            anyhow::ensure!(
                out.status.success(),
                "podman build failed for service '{}'",
                svc.name
            );
            image
        } else if let Some(img) = &svc.image {
            log(format!("Service '{}' uses prebuilt image {img}", svc.name));
            img.clone()
        } else {
            anyhow::bail!(
                "compose service '{}' has neither `build` nor `image`",
                svc.name
            );
        };

        // Service env wins; project env fills gaps.
        let mut env = svc.env.clone();
        for (k, v) in &proj_env {
            env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        // start_cmd[3] = JSON network config the backend uses to join this service to
        // the shared DNS-less podman network with a static IP + sibling host entries.
        let netcfg = serde_json::json!({
            "net": net,
            "subnet": subnet,
            "gw": gw,
            "ip": svc_ip(idx),
            "alias": svc.name,
            "hosts": hosts,
            // Automatic per-service persistent volume (keyed by project+service).
            "vol": format!("hive-vol-{}-{}", sanitize_tag(project), sanitize_tag(&svc.name)),
            "volpath": container_volume_path(),
        })
        .to_string();
        let is_primary = Some(&svc.name) == primary.as_ref();
        // The `x-shadw-expose` opt-in may override the protocol/port a NON-PRIMARY
        // service is reached on externally; absent an override, both fall back to
        // the service's own parsed values. This is the single source of truth for
        // `FunctionConfig::protocol` (drives cross-node raw-vs-HTTP proxying for
        // every service, not just exposed ones) — `start_cmd[2]` (the container's
        // REAL internal listen port) is never affected by the override.
        let resolved_protocol = svc.expose.protocol.unwrap_or(svc.protocol);
        // The raw TCP splice (`mesh_raw::resolve`'s TCP branch) always connects
        // to the leased instance's PUBLISHED endpoint, which is derived from
        // `start_cmd[2]` (= `svc.port`, the container's real listen port) —
        // never from the PortSpec's `container_port`. An `x-shadw-expose` port
        // override that DIFFERS from `svc.port` would therefore silently
        // allocate/advertise a public port keyed to a container_port the
        // splice never actually dials, so a client connecting expecting that
        // stated port to be the real one would land on `svc.port` instead
        // with no error. Refuse to honor a differing override — fall back to
        // `svc.port` (the address the splice really uses) and say so loudly,
        // rather than letting the two silently diverge.
        let resolved_expose_port = match svc.expose.port {
            Some(p) if p != svc.port => {
                log(format!(
                    "WARN: service '{}' declares x-shadw-expose port {p}, but its raw TCP splice always targets the real listen port {} — using {} for the public allocation instead of the mismatched override.",
                    svc.name, svc.port, svc.port
                ));
                svc.port
            }
            Some(p) => p,
            None => svc.port,
        };
        functions.push(FunctionConfig {
            name: svc.name.clone(),
            runtime: "container".into(),
            // ["__container__", image, port, <netcfg-json>] — single container deploys
            // omit the 4th arg (no network); compose deploys carry the shared-net cfg.
            start_cmd: vec!["__container__".into(), image, svc.port.to_string(), netcfg],
            env,
            vcpus: 1,
            memory_mib: 512,
            max_concurrency: 20,
            min_instances: 1, // keep every service warm so internal deps actually run
            max_instances: 3,
            idle_ttl_secs: 300,
            max_duration_secs: 300,
            protocol: resolved_protocol,
            // Only an explicit `x-shadw-expose` opt-in publishes a raw external proxy
            // target for a NON-PRIMARY service — the primary is already reachable via
            // its `/` route, and every unopted-in service stays internal-only
            // (unchanged default behavior: a private DB sidecar never becomes public
            // just because it declares `ports:`).
            ports: if svc.expose.enabled {
                vec![PortSpec::single(resolved_expose_port, resolved_protocol)]
            } else {
                Vec::new()
            },
            ..Default::default()
        });
        if is_primary && !resolved_protocol.needs_raw_proxy() {
            routes.push(Route {
                pattern: "/".into(),
                target: RouteTarget::Function(svc.name.clone()),
            });
        } else if is_primary {
            log(format!(
                "Primary service '{}' is a raw {} service — no HTTP '/' route created (reachable through its allocated raw public port instead).",
                svc.name, resolved_protocol
            ));
        } else if svc.expose.enabled && !resolved_protocol.needs_raw_proxy() {
            // Explicit opt-in external route for a secondary service (e.g. a
            // standalone Postgres/Redis meant to be reachable from outside) —
            // namespaced under its own service name so it never collides with the
            // primary's `/`.
            routes.push(Route {
                pattern: format!("/{}", sanitize_tag(&svc.name)),
                target: RouteTarget::Function(svc.name.clone()),
            });
        }
    }
    log(format!(
        "Compose deployment ready — public entrypoint: {} (services share network {net})",
        primary.as_deref().unwrap_or("(none)")
    ));
    Ok(Manifest {
        project: project.to_string(),
        functions,
        routes,
        ..Default::default()
    })
}

/// Railway-style per-service overrides for a CONTAINER project, read from an optional
/// `fluid.json` `container` block (e.g. `{ "container": { "port": 50051, "protocol":
/// "grpc" } }`). Lets a Dockerfile/Containerfile project pin its listen port and
/// declare its wire protocol without changing the image. All fields optional.
#[derive(Debug, Default, serde::Deserialize)]
struct ContainerOverride {
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    protocol: Option<String>,
    /// Memory ceiling for the container, e.g. "4g", "2048m", "1.5g", or "512".
    /// Overrides the node's generous default; lets a heavy monolith request more.
    #[serde(default)]
    memory: Option<String>,
    /// CPU quota for the container (podman `--cpus`), e.g. "4", "2.0", "0.5".
    /// Overrides the node's generous default. Clamped to a fleet-wide ceiling in
    /// `ContainerLimits::for_container` (`HIVE_CONTAINER_CPUS_MAX`).
    #[serde(default)]
    cpus: Option<String>,
    /// Max-PIDs ceiling for the container's cgroup (podman `--pids-limit`) — a
    /// fork-bomb guard. Overrides the node's default. Clamped to a fleet-wide
    /// ceiling (`HIVE_CONTAINER_PIDS_MAX`).
    #[serde(default)]
    pids: Option<u32>,
}

/// Parse a human memory string ("4g", "2048m", "1.5g", "512") into MiB. Returns 0
/// (→ use the node's default) for empty/unparseable input.
fn parse_mem_mib(s: &str) -> u32 {
    let t = s.trim().to_lowercase();
    if t.is_empty() {
        return 0;
    }
    let (num, mult) = if let Some(n) = t.strip_suffix('g') {
        (n, 1024.0)
    } else if let Some(n) = t.strip_suffix("gb") {
        (n, 1024.0)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 1.0)
    } else if let Some(n) = t.strip_suffix("mb") {
        (n, 1.0)
    } else {
        (t.as_str(), 1.0) // bare number = MiB
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * mult).round() as u32)
        .unwrap_or(0)
}

/// Parse a CPU quota string ("4", "2.0", "0.5") into a fractional vCPU count.
/// Returns 0.0 (→ use the node's default) for empty/unparseable/non-positive
/// input.
fn parse_cpus_quota(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    t.parse::<f64>().ok().filter(|v| *v > 0.0).unwrap_or(0.0)
}

/// Parse the `container` override block from a repo's `fluid.json` text, if any.
/// Tolerant: a fluid.json without a `container` key (or invalid) yields defaults.
fn parse_container_override(fluid_json: &str) -> ContainerOverride {
    #[derive(serde::Deserialize)]
    struct Wrap {
        #[serde(default)]
        container: ContainerOverride,
    }
    serde_json::from_str::<Wrap>(fluid_json)
        .map(|w| w.container)
        .unwrap_or_default()
}

/// Build a manifest for a Next.js deployment adapter (OpenNext / vinext): the
/// framework's Node HTTP server runs as the Fluid `api` function while immutable
/// assets are served from the CDN, falling through to the server on a miss. Returns
/// `None` for non-adapter frameworks (caller uses the generic node-server path).
async fn adapter_manifest(
    project: &str,
    slug: &str,
    dir: &Path,
    runtime: Option<hive_core::Runtime>,
) -> Option<Manifest> {
    let bun = runtime == Some(hive_core::Runtime::Bun);
    // (start command, relative assets dir) per adapter. Both adapters emit a
    // single bundled server file — exactly the shape `bun run <file>` needs (and
    // the shape `warmup_bun_bytecode` can bytecode-cache), so under an explicit
    // Bun runtime we run that same file directly through Bun instead of Node.
    let (start_cmd, assets_dir): (Vec<String>, &str) = match slug {
        "opennext" => {
            let server = ".open-next/server-functions/default/index.mjs";
            if !dir.join(server).exists() {
                return None; // build didn't produce the expected server function
            }
            let runner = if bun { "bun" } else { "node" };
            (vec![runner.into(), server.into()], ".open-next/assets")
        }
        "vinext" => {
            // Prefer running Nitro's node server directly; else `vinext start`
            // (both honor $PORT). `--no-install` avoids a runtime network fetch.
            // `--bun` on the CLI-fallback path: the `vinext` CLI internally
            // boots a Nitro server, which may itself invoke `node` for
            // sub-tasks — same "prefer Bun for shebang-invoked children"
            // reasoning as the package.json#scripts.start fix above, and
            // documented as valid `bunx` usage (`bunx --bun vite dev …`).
            let start = if dir.join(".output/server/index.mjs").exists() {
                let runner = if bun { "bun" } else { "node" };
                vec![runner.into(), ".output/server/index.mjs".into()]
            } else if bun {
                vec![
                    "bunx".into(),
                    "--bun".into(),
                    "--no-install".into(),
                    "vinext".into(),
                    "start".into(),
                ]
            } else {
                vec![
                    "npx".into(),
                    "--no-install".into(),
                    "vinext".into(),
                    "start".into(),
                ]
            };
            (start, ".output/public")
        }
        _ => return None,
    };

    let func = FunctionConfig {
        name: "api".into(),
        runtime: runtime
            .map(|r| r.as_str().to_string())
            .unwrap_or_else(|| "auto".into()),
        start_cmd,
        env: Default::default(),
        vcpus: 1,
        memory_mib: 512,
        max_concurrency: 10,
        min_instances: 1,
        max_instances: 5,
        idle_ttl_secs: 60,
        max_duration_secs: 300,
        ..Default::default()
    };

    // Serve immutable assets from the CDN when they exist, with a fallthrough to the
    // origin function; otherwise route everything to the function (the server serves
    // its own assets).
    if dir.join(assets_dir).exists() {
        Some(Manifest {
            project: project.to_string(),
            static_dir: Some(assets_dir.to_string()),
            functions: vec![func],
            routes: vec![Route {
                pattern: "/".into(),
                target: RouteTarget::Static,
            }],
            origin_function: Some("api".into()),
            ..Default::default()
        })
    } else {
        Some(Manifest {
            project: project.to_string(),
            static_dir: None,
            functions: vec![func],
            routes: vec![Route {
                pattern: "/".into(),
                target: RouteTarget::Function("api".into()),
            }],
            ..Default::default()
        })
    }
}

fn function_manifest(
    project: &str,
    start_cmd: Vec<String>,
    runtime: Option<hive_core::Runtime>,
) -> Manifest {
    Manifest {
        project: project.to_string(),
        static_dir: None,
        functions: vec![FunctionConfig {
            name: "api".into(),
            runtime: runtime
                .map(|r| r.as_str().to_string())
                .unwrap_or_else(|| "auto".into()),
            start_cmd,
            env: Default::default(),
            vcpus: 1,
            memory_mib: 512,
            max_concurrency: 10,
            min_instances: 1,
            max_instances: 5,
            idle_ttl_secs: 60,
            max_duration_secs: 300,
            ..Default::default()
        }],
        routes: vec![Route {
            pattern: "/".into(),
            target: RouteTarget::Function("api".into()),
        }],
        ..Default::default()
    }
}

/// A minimal "Deployed on OpenEdge" landing page for static deploys with no index.
fn landing_page(project: &str, commit: &str, msg: &str, repo: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{project} · OpenEdge</title>
  <style>
    :root {{ color-scheme: light dark; }}
    * {{ box-sizing: border-box; }}
    body {{ margin:0; min-height:100vh; display:grid; place-items:center;
      font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
      background:#fafafa; color:#111; }}
    @media (prefers-color-scheme: dark) {{ body {{ background:#0a0a0a; color:#ededed; }} .card {{ background:#111 !important; border-color:#222 !important; }} .muted {{ color:#888 !important; }} }}
    .card {{ background:#fff; border:1px solid #ebebeb; border-radius:16px; padding:40px 44px; max-width:520px; box-shadow:0 1px 2px rgba(0,0,0,.04); }}
    .tri {{ width:46px; height:40px; }}
    h1 {{ font-size:24px; margin:18px 0 6px; letter-spacing:-0.02em; }}
    .muted {{ color:#666; font-size:14px; line-height:1.6; }}
    code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size:12px;
      background:rgba(127,127,127,.12); padding:2px 6px; border-radius:6px; }}
    .row {{ margin-top:18px; display:flex; gap:8px; flex-wrap:wrap; align-items:center; }}
    .badge {{ font-size:12px; border:1px solid #ebebeb; border-radius:999px; padding:2px 10px; }}
  </style>
</head>
<body>
  <div class="card">
    <svg class="tri" viewBox="0 0 24 22" aria-hidden><path d="M12 0 L24 22 L0 22 Z" fill="currentColor"/></svg>
    <h1>{project}</h1>
    <p class="muted">Deployed on <strong>OpenEdge</strong> — your unified, self-hosted cloud.</p>
    <div class="row">
      <span class="badge">● Ready</span>
      <span class="badge">commit <code>{commit}</code></span>
    </div>
    <p class="muted" style="margin-top:16px">{msg}</p>
    <p class="muted" style="margin-top:6px">Source: <code>{repo}</code></p>
  </div>
</body>
</html>"#
    )
}

/// A "build failed" status page so a failed deployment still serves something
/// (the project/deployment is created either way, like Vercel).
fn build_failed_page(project: &str, commit: &str, err: &str, repo: &str) -> String {
    let safe_err = err.replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{project} · Build failed · OpenEdge</title>
  <style>
    :root {{ color-scheme: light dark; }}
    * {{ box-sizing: border-box; }}
    body {{ margin:0; min-height:100vh; display:grid; place-items:center;
      font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
      background:#fafafa; color:#111; }}
    @media (prefers-color-scheme: dark) {{ body {{ background:#0a0a0a; color:#ededed; }} .card {{ background:#111 !important; border-color:#222 !important; }} .muted {{ color:#888 !important; }} pre {{ background:#000 !important; }} }}
    .card {{ background:#fff; border:1px solid #ebebeb; border-radius:16px; padding:36px 40px; max-width:560px; }}
    h1 {{ font-size:22px; margin:14px 0 6px; letter-spacing:-0.02em; }}
    .muted {{ color:#666; font-size:14px; line-height:1.6; }}
    .dot {{ display:inline-block; width:9px; height:9px; border-radius:999px; background:#f5454f; margin-right:7px; }}
    pre {{ background:#f6f6f6; border-radius:8px; padding:12px; overflow:auto; font-size:12px;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }}
  </style>
</head>
<body>
  <div class="card">
    <h1><span class="dot"></span>Build failed</h1>
    <p class="muted">The latest deployment of <strong>{project}</strong> ({commit}) did not build successfully, but the project was still created. Fix the error and redeploy.</p>
    <pre>{safe_err}</pre>
    <p class="muted">Source: {repo}</p>
  </div>
</body>
</html>"#
    )
}

/// A self-refreshing "Building…" placeholder, served at the project's domain the
/// moment a FIRST deploy starts — so the URL always resolves instead of 404'ing
/// for the whole build. It reloads every few seconds and flips to the real app
/// automatically once the deployment is ready.
fn building_page(project: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <meta http-equiv="refresh" content="3" />
  <title>{project} · Building · OpenEdge</title>
  <style>
    :root {{ color-scheme: light dark; }}
    * {{ box-sizing: border-box; }}
    body {{ margin:0; min-height:100vh; display:grid; place-items:center;
      font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
      background:#fafafa; color:#111; }}
    @media (prefers-color-scheme: dark) {{ body {{ background:#0a0a0a; color:#ededed; }} .card {{ background:#111 !important; border-color:#222 !important; }} .muted {{ color:#888 !important; }} }}
    .card {{ background:#fff; border:1px solid #ebebeb; border-radius:16px; padding:36px 40px; max-width:520px; text-align:center; }}
    h1 {{ font-size:22px; margin:18px 0 6px; letter-spacing:-0.02em; }}
    .muted {{ color:#666; font-size:14px; line-height:1.6; }}
    .spinner {{ width:34px; height:34px; border-radius:999px; border:3px solid #ddd; border-top-color:#111;
      animation:spin .8s linear infinite; margin:0 auto; }}
    @media (prefers-color-scheme: dark) {{ .spinner {{ border-color:#333; border-top-color:#ededed; }} }}
    @keyframes spin {{ to {{ transform:rotate(360deg); }} }}
  </style>
</head>
<body>
  <div class="card">
    <div class="spinner"></div>
    <h1>Building {project}…</h1>
    <p class="muted">Your deployment is building. This page refreshes automatically and will load your app as soon as it's ready.</p>
  </div>
</body>
</html>"#
    )
}

/// First-deploy only: register the project's host immediately with a "Building…"
/// page, so the domain resolves during the build. Returns the placeholder
/// deployment id (removed once the real deployment is live). A redeploy returns
/// None — the current version stays live until the new build is ready.
/// `bid` uniquifies the scratch dir: two concurrent first deploys of the same
/// project (both pass `project_has_deployment` before either registers) must not
/// point two deployment records at ONE shared placeholder root.
async fn register_building_placeholder(
    cloud: &Arc<CloudState>,
    project: &str,
    req: &GitDeployRequest,
    bid: &str,
) -> Option<String> {
    if project_has_deployment(cloud, project) {
        // Redeploy (the project already has a live deployment somewhere in the
        // fleet) — keep the live version serving until the new build is ready, and
        // never spawn a phantom placeholder. The local-only `serves_host` check used
        // to miss remotely-placed projects, producing a bogus "Building…" Preview
        // deployment row on every redeploy.
        return None;
    }
    let dir = deploy_root().join(format!(
        "{}-building-{}-{}",
        checkout_tag(project),
        now_ms(),
        bid
    ));
    tokio::fs::create_dir_all(&dir).await.ok()?;
    tokio::fs::write(dir.join("index.html"), building_page(project))
        .await
        .ok()?;
    let info = cloud.gw.deploy_full(
        dir.to_string_lossy().into_owned(),
        static_manifest(project, "."),
        req.creator.clone().unwrap_or_else(|| "you".into()),
        None,
        false, // not production — superseded by the real deploy when it's ready
        DeployState::Building,
        cloud.projects.team_of(project),
    );
    crate::persist::persist(cloud);
    Some(info.id.to_string())
}

/// The infix `register_building_placeholder` stamps into a placeholder's root
/// dir (`<tag>-building-<ms>-<bid>`). It is the only durable marker that tells a
/// placeholder apart from a real deployment after a restart, so both the boot
/// reconciler and the reaper below key off it.
pub(crate) const PLACEHOLDER_ROOT_INFIX: &str = "-building-";

/// Is this deployment root a `Building…` placeholder's scratch dir, and which
/// build minted it?
///
/// Returns the build id parsed out of the dir name, or `None` when the root is
/// not a placeholder at all.
pub(crate) fn placeholder_build_id(root: &str) -> Option<&str> {
    let name = std::path::Path::new(root).file_name()?.to_str()?;
    let (_, tail) = name.rsplit_once(PLACEHOLDER_ROOT_INFIX)?;
    // tail = `<ms>-<bid>`; the build id is everything after the first `-`.
    let (_ms, bid) = tail.split_once('-')?;
    (!bid.is_empty()).then_some(bid)
}

/// True for a persisted record that is a placeholder shell rather than a real
/// deployment: placeholder root, no git source, no functions.
pub(crate) fn is_placeholder_record(rec: &fluid_core::DeployRecord) -> bool {
    placeholder_build_id(&rec.root).is_some()
        && rec.git.is_none()
        && rec.manifest.functions.is_empty()
}

/// Reap `Building…` placeholders whose build is over.
///
/// The placeholder is removed exactly once, on the happy path of the task
/// `start_build` spawned — so ANY way that task fails to reach its last line
/// leaks it permanently: a user cancel (`BuildCancelRegistry` aborts the task
/// handle, and an aborted future never runs the removal), a panic, or the
/// process dying mid-build. A leaked placeholder is not inert: `deploy_full`
/// hands a project's FIRST deployment the production alias, so the shell owns
/// `<project>` on that node forever, `persist::restore` then reconciles it from
/// `Building` to a permanent `Error`, and the node both serves it and publishes
/// a DNS affinity record for it. Witnessed on `archive-zip.shadw.app`
/// (2026-08-05): fc-sanjose answered the project host from a 2-day-old
/// placeholder while fc-sanjose-gpu-1 held the Ready deployment.
///
/// The reap condition is "the build that minted it is no longer in flight",
/// which is decidable from local state alone and is exactly the condition the
/// removal call was standing in for. An unknown build id counts as over: build
/// records do not outlive a restart, and a placeholder for a build nobody
/// remembers can never be superseded by it.
pub async fn reap_orphan_placeholders(cloud: &Arc<CloudState>) -> usize {
    let stale: Vec<(String, String)> = cloud
        .gw
        .deployment_records()
        .into_iter()
        .filter(|rec| rec.state != DeployState::Ready && is_placeholder_record(rec))
        .filter(|rec| {
            placeholder_build_id(&rec.root).is_some_and(|bid| {
                !cloud
                    .builds
                    .get(bid)
                    .is_some_and(|b| matches!(b.state, DeployState::Queued | DeployState::Building))
            })
        })
        .map(|rec| (rec.id, rec.project))
        .collect();
    if stale.is_empty() {
        return 0;
    }
    for (id, project) in &stale {
        cloud.gw.remove(id).await;
        tracing::warn!(
            deployment = %id,
            project = %project,
            "reaped orphaned Building… placeholder (its build is no longer in flight)"
        );
    }
    crate::persist::persist(cloud);
    stale.len()
}

async fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

// ===========================================================================
// Webhook-less git auto-deploy: commit-polling reconciler.
//
// The GitHub webhook (`admin::git_webhook`) is the ONLY event-driven auto-deploy
// trigger, and it only ever fires if a hook was actually installed on the repo.
// A project imported as a plain public URL (the common "paste a repo URL" flow)
// or whose owner never completed the GitHub OAuth/App connection gets NEITHER a
// webhook NOR the Actions-workflow fallback (`git_ci == None`), so GitHub never
// notifies us of a push and no `git push` ever deploys — with zero visible error
// (no failed delivery, because no webhook object exists).
//
// This reconciler closes that gap WITHOUT any credential or owner action: it
// polls each git-sourced project's tracked-branch HEAD with `git ls-remote` and
// starts the SAME build the webhook would, whenever HEAD has advanced past the
// deployed commit. It is the credential-free build-past for the chronically-dead
// GitHub connection — for a public repo it needs nothing at all.
// ===========================================================================

/// Spawn the leader-only git commit-poll reconciler. See the module comment
/// above. Cheap when nothing changed (one `ls-remote` per git project per tick,
/// short-circuited by an in-memory SHA cache).
pub fn spawn_git_poll_reconcile(cloud: Arc<CloudState>) {
    crate::supervise::spawn_supervised("git-poll-reconcile", move || {
        let cloud = cloud.clone();
        async move {
            // Let the initial gossip / deployment-record sync settle so projects have
            // a real deployed-commit baseline before the first poll (else a cold
            // leader would treat every project as "unknown" at once).
            tokio::time::sleep(std::time::Duration::from_secs(45)).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(90));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                crate::supervise::beat("git-poll-reconcile");
                // LEADER ONLY: exactly one node polls + deploys, mirroring every other
                // reconciler's control-plane gate — otherwise each node would start the
                // same build for the same push.
                if !cloud.is_control_plane_leader() {
                    continue;
                }
                git_poll_cycle(&cloud).await;
            }
        }
    });
}

/// One poll pass over every git-sourced project. Never panics: a per-project
/// failure (unreachable remote, missing branch, auth failure) is logged at debug
/// and skipped so it can't wedge the other projects or the loop.
async fn git_poll_cycle(cloud: &Arc<CloudState>) {
    let projects: Vec<String> = cloud.projects.snapshot().into_keys().collect();
    let mut n_git = 0u32; // git-sourced projects actually polled this cycle
    let mut n_deployed = 0u32; // projects whose HEAD advanced -> build started
    for project in projects {
        // Fleet-aware source; skip non-git (zip `upload://`, image `image://`)
        // and never-deployed / unbound projects.
        let Some(src) = crate::admin::git_for_project_fleet(cloud, &project) else {
            continue;
        };
        if !src.is_real_git() {
            continue;
        }
        n_git += 1;
        // Tracked branch: the project's production branch, else the deployment's
        // own branch. Empty => nothing to poll.
        let branch = {
            let pb = cloud.projects.production_branch_of(&project);
            if pb.is_empty() {
                src.branch.clone()
            } else {
                pb
            }
        };
        if branch.is_empty() {
            continue;
        }

        // Token for a PRIVATE github repo (public repos need none): the same
        // resolution `git_webhook` uses — a GitHub App installation token first,
        // else a node-wide GITHUB_TOKEN. Carried into both the `ls-remote` read
        // and the deploy request's clone.
        let token = resolve_git_poll_token(&src.repo_url).await;

        let head = match git_ls_remote_head(&src.repo_url, &branch, token.as_deref()).await {
            Some(h) if !h.is_empty() => h,
            // Branch not found / remote unreachable / auth failure: skip this
            // cycle, retry next tick. Never treated as "no commit -> deploy".
            _ => continue,
        };

        // Baseline: the in-memory last-seen SHA if we've observed this project
        // before, else the currently-deployed commit. This is what makes the
        // FIRST poll after a boot deploy shoomoo-style undeployed pushes (deployed
        // != HEAD) while leaving already-current projects alone (deployed == HEAD).
        let baseline = cloud
            .git_poll_seen
            .read()
            .get(&project)
            .cloned()
            .unwrap_or_else(|| src.commit.clone());

        if commit_eq(&head, &baseline) {
            // Up to date: record HEAD so subsequent cycles are a cheap compare.
            cloud.git_poll_seen.write().insert(project.clone(), head);
            continue;
        }

        // A genuine advance. Don't stack on a build for this exact commit that's
        // already in flight LOCALLY (best-effort — a build placed on a peer node
        // isn't in this leader's list; the SHA-dedup below is the real guard).
        let already_building = cloud.builds.list().iter().any(|b| {
            b.project == project
                && matches!(b.state, DeployState::Queued | DeployState::Building)
                && commit_eq(&b.commit, &head)
        });
        // Record BEFORE enqueue so a slow build can't be re-enqueued next tick and
        // a real webhook + this poller can't double-fire (whoever deploys HEAD
        // first, the other sees deployed == HEAD and skips).
        cloud
            .git_poll_seen
            .write()
            .insert(project.clone(), head.clone());
        if already_building {
            continue;
        }

        let root_dir = Some(cloud.projects.root_dir_of(&project)).filter(|s| !s.is_empty());
        let req = GitDeployRequest {
            repo_url: src.repo_url.clone(),
            branch: Some(branch.clone()).filter(|b| !b.is_empty()),
            // Pin the EXACT polled SHA (same race protection as the webhook path).
            commit: Some(head.clone()),
            head_repo_url: None, // polling only ever tracks the project's own repo
            project: Some(project.clone()),
            creator: Some("git-poll".into()),
            production: true, // legacy field; classification uses target/branch
            target: None,     // branch-classified: production branch => production
            use_cache: true,
            root_dir,
            env: None,        // env comes from the project store on a push redeploy
            no_fanout: false, // coordinator deploy: schedule + fanout to placement
            fanout_secondary: false,
            build_config: None,
            function_settings: None,
            redeploy: false,
            zip_b64: None,
            image_ref: None,
            image_port: None,
            image_protocol: None,
            image_memory: None,
            image_cpus: None,
            image_pids: None,
            image_ports: None,
            git_token: token,
        };
        let build_id = start_build(cloud.clone(), req);
        let ev = cloud.event(
            &cloud.region,
            "DEPLOY",
            &format!("{project}.localhost"),
            "/",
            200,
            "git-poll",
            &format!(
                "git-poll {} {} @ {}",
                src.repo_url,
                branch,
                head.chars().take(7).collect::<String>()
            ),
        );
        cloud.record(ev);
        tracing::info!(
            project = %project,
            repo = %src.repo_url,
            branch = %branch,
            commit = %head,
            build = %build_id,
            "git_poll: tracked branch advanced past the deployed commit — auto-deploy started (no webhook installed)"
        );
        n_deployed += 1;
    }
    tracing::debug!(
        git_projects = n_git,
        deployed = n_deployed,
        "git_poll: cycle complete"
    );
}

/// True when two commit strings refer to the same commit, tolerant of one being
/// an abbreviated prefix of the other (a deployment record may store a short
/// SHA while `ls-remote` returns the full 40-char one). Empty never matches.
fn commit_eq(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    short.len() >= 7 && long.starts_with(short)
}

/// `git ls-remote <url> refs/heads/<branch>` → the branch HEAD SHA. Host-agnostic
/// (github/gitlab/bitbucket/self-hosted) and free of GitHub's REST rate limit.
/// Bounded by a hard timeout so a hung remote can't wedge the poll cycle, and it
/// never prompts for credentials (a private repo without a token just fails and
/// is skipped). Returns None on any failure or an absent branch.
async fn git_ls_remote_head(repo_url: &str, branch: &str, token: Option<&str>) -> Option<String> {
    let url = git_poll_authed_url(repo_url, token);
    let fut = Command::new("git")
        .arg("-c")
        .arg("credential.helper=") // ignore any global credential helper
        .arg("ls-remote")
        .arg(&url)
        .arg(format!("refs/heads/{branch}"))
        .env("GIT_TERMINAL_PROMPT", "0") // never block on an interactive prompt
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), fut)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Output line: "<sha>\trefs/heads/<branch>".
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .filter(|s| s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_string())
}

/// Inject a github token into an https github URL for a PRIVATE-repo read. Only
/// `https://github.com/...` URLs are touched, so a token is never leaked to
/// another forge; every other URL (public github, gitlab, self-hosted, ssh)
/// passes through unchanged.
fn git_poll_authed_url(repo_url: &str, token: Option<&str>) -> String {
    match token {
        Some(t) if !t.is_empty() && repo_url.starts_with("https://github.com/") => {
            repo_url.replacen("https://", &format!("https://x-access-token:{t}@"), 1)
        }
        _ => repo_url.to_string(),
    }
}

/// Token for polling/cloning a PRIVATE github repo: a GitHub App installation
/// token (minted server-side, no user session) first, else a node-wide
/// `GITHUB_TOKEN`. `None` for a public repo (or a non-github host) — nothing is
/// needed there. Mirrors `git_webhook`'s own resolution so the two auto-deploy
/// paths authenticate identically.
async fn resolve_git_poll_token(repo_url: &str) -> Option<String> {
    if !repo_url.starts_with("https://github.com/") {
        return None;
    }
    if crate::github_app_auth::configured() {
        let path = repo_url
            .trim_start_matches("https://github.com/")
            .trim_end_matches(".git");
        if let Some((owner, repo)) = path.split_once('/') {
            if let Ok(Some(tok)) =
                crate::github_app_auth::installation_token_for_repo(owner, repo).await
            {
                return Some(tok);
            }
        }
    }
    std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_refs_are_fully_qualified_for_linux_podman() {
        // Short names get docker.io (Linux podman enforces qualification).
        assert_eq!(
            qualify_image_ref("fruitbox12/simplifi:latest"),
            "docker.io/fruitbox12/simplifi:latest"
        );
        assert_eq!(qualify_image_ref("nginx"), "docker.io/library/nginx");
        assert_eq!(qualify_image_ref("redis:7"), "docker.io/library/redis:7");
        // Already-qualified refs are untouched.
        assert_eq!(
            qualify_image_ref("quay.io/org/img:tag"),
            "quay.io/org/img:tag"
        );
        assert_eq!(qualify_image_ref("ghcr.io/owner/app"), "ghcr.io/owner/app");
        assert_eq!(qualify_image_ref("localhost/hive-x"), "localhost/hive-x");
        assert_eq!(qualify_image_ref("registry:5000/x"), "registry:5000/x");
    }

    #[test]
    fn exposed_ports_prefers_tcp_falls_back_to_udp() {
        assert_eq!(
            parse_exposed_ports(r#"{"8080/tcp":{}}"#),
            Some(PortSpec::single(8080, ServiceProtocol::Http))
        );
        // Lowest tcp wins when multiple are exposed.
        assert_eq!(
            parse_exposed_ports(r#"{"9090/tcp":{},"3000/tcp":{}}"#),
            Some(PortSpec::single(3000, ServiceProtocol::Http))
        );
        // A tcp port present alongside udp ones still wins (matches prior precedence).
        assert_eq!(
            parse_exposed_ports(r#"{"8080/tcp":{},"19132/udp":{}}"#),
            Some(PortSpec::single(8080, ServiceProtocol::Http))
        );
        // UDP-only image (no tcp port at all, e.g. Minecraft Bedrock 19132/udp) is no
        // longer discarded — the lowest udp port is surfaced with protocol=udp instead
        // of forcing a blind fallback to the 8080/http default.
        assert_eq!(
            parse_exposed_ports(r#"{"53/udp":{}}"#),
            Some(PortSpec::single(53, ServiceProtocol::Udp))
        );
        assert_eq!(
            parse_exposed_ports(r#"{"19133/udp":{},"19132/udp":{}}"#),
            Some(PortSpec::single(19132, ServiceProtocol::Udp))
        ); // lowest udp
        assert_eq!(parse_exposed_ports("null"), None);
        assert_eq!(parse_exposed_ports(""), None);
    }

    #[test]
    fn image_manifest_has_container_fn_and_volume() {
        // The prebuilt-image manifest runs the image as a container with a stable,
        // per-project persistent volume encoded in start_cmd[3].
        let m = container_manifest(
            "my-proj",
            "fruitbox12/simplifi:latest",
            8080,
            "http",
            0,
            0.0,
            0,
        );
        let f = &m.functions[0];
        assert_eq!(f.runtime, "container");
        assert_eq!(f.start_cmd[0], "__container__");
        assert_eq!(f.start_cmd[1], "fruitbox12/simplifi:latest");
        assert_eq!(f.start_cmd[2], "8080");
        let cfg: serde_json::Value = serde_json::from_str(&f.start_cmd[3]).unwrap();
        assert_eq!(cfg["vol"], "hive-vol-my-proj");
        assert!(cfg["volpath"].as_str().unwrap().starts_with('/'));
    }

    #[tokio::test]
    async fn adapter_manifest_opennext_hybrid() {
        let dir = std::env::temp_dir().join(format!("hive-on-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Non-adapter framework → None (generic node-server path).
        assert!(adapter_manifest("p", "nextjs", &dir, None).await.is_none());
        // No server function yet → None even for opennext.
        std::fs::create_dir_all(&dir).unwrap();
        assert!(adapter_manifest("p", "opennext", &dir, None)
            .await
            .is_none());
        // Full OpenNext output → hybrid manifest (assets + origin fallthrough).
        std::fs::create_dir_all(dir.join(".open-next/server-functions/default")).unwrap();
        std::fs::write(
            dir.join(".open-next/server-functions/default/index.mjs"),
            "//server",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join(".open-next/assets")).unwrap();
        let m = adapter_manifest("p", "opennext", &dir, None)
            .await
            .expect("opennext manifest");
        assert_eq!(m.static_dir.as_deref(), Some(".open-next/assets"));
        assert_eq!(m.origin_function.as_deref(), Some("api"));
        assert_eq!(
            m.functions[0].start_cmd,
            vec!["node", ".open-next/server-functions/default/index.mjs"]
        );
        assert_eq!(m.functions[0].runtime, "auto");
        assert!(matches!(m.routes[0].target, RouteTarget::Static));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn adapter_manifest_opennext_bun_runtime() {
        let dir = std::env::temp_dir().join(format!("hive-on-bun-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".open-next/server-functions/default")).unwrap();
        std::fs::write(
            dir.join(".open-next/server-functions/default/index.mjs"),
            "//server",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join(".open-next/assets")).unwrap();
        let m = adapter_manifest("p", "opennext", &dir, Some(hive_core::Runtime::Bun))
            .await
            .expect("opennext manifest");
        assert_eq!(
            m.functions[0].start_cmd,
            vec!["bun", ".open-next/server-functions/default/index.mjs"]
        );
        assert_eq!(m.functions[0].runtime, "bun");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn adapter_manifest_vinext_node_server() {
        let dir = std::env::temp_dir().join(format!("hive-vi-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".output/server")).unwrap();
        std::fs::write(dir.join(".output/server/index.mjs"), "//nitro").unwrap();
        std::fs::create_dir_all(dir.join(".output/public")).unwrap();
        let m = adapter_manifest("p", "vinext", &dir, None)
            .await
            .expect("vinext manifest");
        assert_eq!(m.static_dir.as_deref(), Some(".output/public"));
        assert_eq!(m.origin_function.as_deref(), Some("api"));
        assert_eq!(
            m.functions[0].start_cmd,
            vec!["node", ".output/server/index.mjs"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn adapter_manifest_vinext_bun_runtime_no_prebuilt_server() {
        let dir = std::env::temp_dir().join(format!("hive-vi-bun-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".output/public")).unwrap();
        let m = adapter_manifest("p", "vinext", &dir, Some(hive_core::Runtime::Bun))
            .await
            .expect("vinext manifest");
        assert_eq!(
            m.functions[0].start_cmd,
            vec!["bunx", "--bun", "--no-install", "vinext", "start"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bun_next_ssr_example_fixture_resolves_end_to_end() {
        // Runs the REAL detection + adapter pipeline against the checked-in,
        // manually-verified `examples/bun-next-ssr` fixture (its
        // `.open-next/server-functions/default/index.mjs` was confirmed by hand
        // to actually boot and serve requests under `bun run`) — not a
        // synthetic temp dir. Proves the full chain: framework auto-detection
        // (`fluid_build::detect`) picks the opennext adapter, `vercel.json`'s
        // native `runtime` field resolves to Bun, and `adapter_manifest` emits
        // a `bun` start_cmd pointing at that exact file.
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bun-next-ssr");
        assert!(dir.exists(), "fixture missing: {}", dir.display());

        let framework = fluid_build::detect(&dir);
        assert_eq!(framework.slug, "opennext");

        let vc = fluid_build::load_vercel_config(&dir).expect("vercel.json must parse");
        let runtime = vc
            .runtime
            .as_deref()
            .and_then(hive_core::Runtime::from_config_str);
        assert_eq!(runtime, Some(hive_core::Runtime::Bun));

        let m = adapter_manifest("bun-next-ssr", framework.slug, &dir, runtime)
            .await
            .expect("opennext manifest");
        assert_eq!(
            m.functions[0].start_cmd,
            vec!["bun", ".open-next/server-functions/default/index.mjs"]
        );
        assert_eq!(m.functions[0].runtime, "bun");
        assert_eq!(m.static_dir.as_deref(), Some(".open-next/assets"));
    }

    #[tokio::test]
    async fn bun_next_middleware_example_fixture_resolves_bun_start_and_framework() {
        // `examples/bun-next-middleware` is a REAL Next.js app (real `next`/
        // `react`/`react-dom` deps, real `bun.lock`) with a `middleware.js`.
        // Manually verified live end-to-end under the EXACT commands this
        // platform generates: `bunx --bun next build` (real production build,
        // compiled the middleware bundle) then `bun run --bun start` (== the
        // `["bun","run","--bun","start"]` this test asserts) — the middleware
        // executed correctly (`x-middleware-ran: true` on every response) and
        // the page rendered correctly. Next.js sandboxes middleware in its own
        // Edge Runtime regardless of the host process, so middleware behavior
        // is identical under Node and Bun by construction — this fixture and
        // its live verification are the proof, not an assumption.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/bun-next-middleware");
        assert!(dir.exists(), "fixture missing: {}", dir.display());

        let framework = fluid_build::detect(&dir);
        assert_eq!(
            framework.slug, "nextjs",
            "must detect as plain Next.js (no OpenNext/vinext adapter here)"
        );
        assert_eq!(framework.build_command, "next build");

        let vc = fluid_build::load_vercel_config(&dir).expect("vercel.json must parse");
        let runtime = vc
            .runtime
            .as_deref()
            .and_then(hive_core::Runtime::from_config_str);
        assert_eq!(runtime, Some(hive_core::Runtime::Bun));

        let pm = fluid_build::detect_package_manager(&dir);
        assert_eq!(pm.manager, "bun");

        // The exact command the real deploy pipeline would run to serve this
        // app (package.json has `"start": "next start"`).
        let start = detect_start_cmd(&dir, runtime).await;
        assert_eq!(start, vec!["bun", "run", "--bun", "start"]);
    }

    #[tokio::test]
    async fn bun_monorepo_example_fixture_detected_as_monorepo_with_bun_pm() {
        // `examples/bun-monorepo` is a REAL Bun workspace (root
        // `"workspaces": ["packages/*"]` + root `bun.lock`, `packages/api`
        // depends on `packages/shared` via `workspace:*`). Manually verified
        // live: a real `bun install` at the root correctly symlinked
        // `packages/api/node_modules/@acme/shared -> ../../../shared`, and
        // `packages/api`'s server (run via the platform's exact
        // `bun run --bun start` invocation) correctly imported and called
        // into the shared package under genuine Bun. This test proves the
        // PLATFORM's OWN monorepo-detection functions recognize this shape.
        let repo_root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bun-monorepo");
        let api_dir = repo_root.join("packages/api");
        assert!(api_dir.exists(), "fixture missing: {}", api_dir.display());

        assert!(
            is_workspace_root(&repo_root).await,
            "root package.json has a \"workspaces\" field"
        );
        assert!(
            uses_workspace_protocol(&api_dir).await,
            "packages/api depends on shared via workspace:*"
        );
        let is_monorepo = api_dir != repo_root
            && uses_workspace_protocol(&api_dir).await
            && is_workspace_root(&repo_root).await;
        assert!(
            is_monorepo,
            "must be recognized as a monorepo member, matching build_via_fdi's own condition"
        );

        // Package-manager detection at the ROOT (where a monorepo installs)
        // must pick bun via the root bun.lock.
        let pm = fluid_build::detect_package_manager(&repo_root);
        assert_eq!(pm.manager, "bun");
        assert_eq!(pm.source, fluid_build::PackageManagerSource::BunLock);

        // The subdirectory's own vercel.json still resolves the runtime
        // independently of the monorepo-root package-manager detection —
        // runtime resolution and monorepo/install-dir resolution are
        // orthogonal concerns, exactly like runtime vs. package manager.
        let vc =
            fluid_build::load_vercel_config(&api_dir).expect("packages/api/vercel.json must parse");
        assert_eq!(
            vc.runtime
                .as_deref()
                .and_then(hive_core::Runtime::from_config_str),
            Some(hive_core::Runtime::Bun)
        );

        let start = detect_start_cmd(&api_dir, Some(hive_core::Runtime::Bun)).await;
        assert_eq!(start, vec!["bun", "run", "--bun", "start"]);
    }

    #[test]
    fn container_build_file_detects_dockerfile_and_containerfile() {
        let dir = std::env::temp_dir().join(format!("hive-cf-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Neither → None (falls through to fluid.json / framework detection).
        assert!(container_build_file(&dir).is_none());

        // Containerfile alone IS detected (the Task-2 fix — was ignored before).
        std::fs::write(dir.join("Containerfile"), "FROM scratch\nEXPOSE 9000\n").unwrap();
        let f = container_build_file(&dir).expect("Containerfile detected");
        assert_eq!(f.file_name().unwrap(), "Containerfile");

        // When both exist, Dockerfile takes priority (deterministic).
        std::fs::write(dir.join("Dockerfile"), "FROM scratch\nEXPOSE 8080\n").unwrap();
        let f = container_build_file(&dir).expect("Dockerfile preferred");
        assert_eq!(f.file_name().unwrap(), "Dockerfile");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn container_override_parses_railway_style_config() {
        // Full override: explicit port + protocol.
        let o = parse_container_override(r#"{"container":{"port":50051,"protocol":"grpc"}}"#);
        assert_eq!(o.port, Some(50051));
        assert_eq!(o.protocol.as_deref(), Some("grpc"));
        // fluid.json without a `container` block → all defaults (no override).
        let o2 = parse_container_override(r#"{"project":"x","functions":[]}"#);
        assert_eq!(o2.port, None);
        assert_eq!(o2.protocol, None);
        // Tolerant: invalid JSON yields defaults rather than erroring the build.
        let o3 = parse_container_override("definitely not json");
        assert_eq!(o3.port, None);
        assert_eq!(o3.protocol, None);
    }

    #[test]
    fn container_manifest_carries_protocol() {
        let m = container_manifest("proj", "img:tag", 50051, "grpc", 0, 0.0, 0);
        assert_eq!(m.functions.len(), 1);
        assert_eq!(m.functions[0].protocol_or_http(), "grpc");
        assert!(m.functions[0].needs_raw_proxy());
        assert_eq!(
            &m.functions[0].start_cmd[..3],
            &["__container__", "img:tag", "50051"]
        );
        // start_cmd[3] now carries the automatic persistent-volume run-config.
        let cfg: serde_json::Value = serde_json::from_str(&m.functions[0].start_cmd[3]).unwrap();
        assert_eq!(cfg["vol"], "hive-vol-proj");
        assert_eq!(
            m.functions[0].memory_mib, 0,
            "0 = use the node's generous default (not 512m)"
        );
        assert_eq!(
            m.functions[0].cpus, 0.0,
            "0.0 = use the node's generous default"
        );
        assert_eq!(
            m.functions[0].pids, 0,
            "0 = use the node's generous default"
        );
    }

    #[test]
    fn container_memory_override_parses_and_bakes_into_manifest() {
        // fluid.json { container: { memory: "4g" } } → 4096 MiB on the manifest.
        let o = parse_container_override(r#"{"container":{"memory":"4g"}}"#);
        assert_eq!(o.memory.as_deref(), Some("4g"));
        assert_eq!(parse_mem_mib("4g"), 4096);
        assert_eq!(parse_mem_mib("2048m"), 2048);
        assert_eq!(parse_mem_mib("1.5g"), 1536);
        assert_eq!(parse_mem_mib("512"), 512);
        assert_eq!(parse_mem_mib(""), 0);
        assert_eq!(parse_mem_mib("garbage"), 0);
        let m = container_manifest("proj", "img:tag", 8080, "http", 4096, 0.0, 0);
        assert_eq!(m.functions[0].memory_mib, 4096);
    }

    #[test]
    fn container_cpus_and_pids_override_parses_and_bakes_into_manifest() {
        // fluid.json { container: { cpus: "4", pids: 2048 } } → baked onto the
        // manifest verbatim (clamping happens later, at `ContainerLimits::
        // for_container` consumption time — same split as memory above).
        let o = parse_container_override(r#"{"container":{"cpus":"4","pids":2048}}"#);
        assert_eq!(o.cpus.as_deref(), Some("4"));
        assert_eq!(o.pids, Some(2048));
        assert_eq!(parse_cpus_quota("4"), 4.0);
        assert_eq!(parse_cpus_quota("2.0"), 2.0);
        assert_eq!(parse_cpus_quota("0.5"), 0.5);
        assert_eq!(parse_cpus_quota(""), 0.0);
        assert_eq!(parse_cpus_quota("garbage"), 0.0);
        assert_eq!(
            parse_cpus_quota("-1"),
            0.0,
            "a non-positive quota must not sneak through as a real override"
        );
        assert_eq!(
            parse_cpus_quota("0"),
            0.0,
            "zero must not sneak through as a real override"
        );
        let m = container_manifest("proj", "img:tag", 8080, "http", 0, 4.0, 2048);
        assert_eq!(m.functions[0].cpus, 4.0);
        assert_eq!(m.functions[0].pids, 2048);
    }

    #[tokio::test]
    async fn parse_expose_reads_containerfile_port() {
        let dir = std::env::temp_dir().join(format!("hive-cf-port-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Containerfile"),
            "FROM scratch\nEXPOSE 50051/tcp\n",
        )
        .unwrap();
        let cf = container_build_file(&dir).unwrap();
        assert_eq!(parse_expose(&cf).await, Some(50051));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_sha256_detects_corruption() {
        let a = b"tarball-bytes-v1";
        let d = artifact_sha256(a);
        assert_eq!(d.len(), 64, "sha256 hex is 64 chars");
        assert_eq!(artifact_sha256(a), d, "deterministic");
        assert_ne!(
            artifact_sha256(b"tarball-bytes-v2"),
            d,
            "different bytes -> different digest"
        );
    }

    #[test]
    fn artifact_hmac_sign_verify_and_tamper() {
        let secret = "fleet-shared-secret";
        let bytes = b"node_modules tar payload";
        let sig = artifact_sig(secret, bytes);
        assert!(
            artifact_sig_valid(secret, bytes, &sig),
            "valid signature verifies"
        );
        // Tampered payload fails.
        assert!(!artifact_sig_valid(
            secret,
            b"node_modules tar payloaX",
            &sig
        ));
        // Wrong secret (a peer without the fleet secret) can't forge.
        assert!(!artifact_sig_valid("other-secret", bytes, &sig));
        // Garbage / odd-length hex is rejected, not panicked.
        assert!(!artifact_sig_valid(secret, bytes, "zz"));
        assert!(!artifact_sig_valid(secret, bytes, "abc"));
    }

    // One test owns the process env vars (tests share a process; mutating env in
    // parallel would race), exercising both the no-secret and secret-set regimes.
    #[test]
    fn verify_pulled_artifact_integrity_and_authenticity() {
        let bytes = b"the artifact";
        let good_sha = artifact_sha256(bytes);

        // --- No secret configured (dev / single node) ---
        std::env::remove_var("HIVE_ARTIFACT_SECRET");
        std::env::remove_var("HIVE_JWT_SECRET");
        assert!(
            verify_pulled_artifact(bytes, Some(&good_sha), None).is_ok(),
            "good digest accepted"
        );
        assert!(
            verify_pulled_artifact(bytes, Some("deadbeef"), None).is_err(),
            "corruption rejected"
        );
        assert!(
            verify_pulled_artifact(bytes, None, None).is_ok(),
            "legacy peer accepted in dev"
        );

        // --- Fleet secret configured (production) ---
        std::env::set_var("HIVE_ARTIFACT_SECRET", "s3cret");
        let sig = artifact_sig("s3cret", bytes);
        assert!(
            verify_pulled_artifact(bytes, Some(&good_sha), Some(&sig)).is_ok(),
            "valid sha+sig accepted"
        );
        assert!(
            verify_pulled_artifact(bytes, Some(&good_sha), None).is_err(),
            "missing sig rejected when secret set"
        );
        let forged = artifact_sig("wrong", bytes);
        assert!(
            verify_pulled_artifact(bytes, Some(&good_sha), Some(&forged)).is_err(),
            "forged sig rejected"
        );
        std::env::remove_var("HIVE_ARTIFACT_SECRET");

        // REGRESSION: key separation (#73) — with no HIVE_ARTIFACT_SECRET but
        // HIVE_JWT_SECRET set, artifact_secret() must return a DERIVED key,
        // never the raw JWT secret verbatim (reusing one symmetric key for two
        // unrelated HMAC purposes means compromising one compromises both).
        std::env::set_var("HIVE_JWT_SECRET", "jwt-root-secret");
        let derived = artifact_secret().expect("must derive a fallback key from HIVE_JWT_SECRET");
        assert_ne!(
            derived, "jwt-root-secret",
            "must never reuse the raw JWT secret verbatim"
        );
        // Deterministic (same root -> same derived key every time).
        assert_eq!(artifact_secret().as_deref(), Some(derived.as_str()));
        std::env::remove_var("HIVE_JWT_SECRET");
        assert!(
            artifact_secret().is_none(),
            "no secret configured at all -> None"
        );
    }

    #[test]
    fn glob_match_function_keys() {
        // within-segment * and extension stripping for our extension-less names
        assert!(glob_match("api/*.js", "api/hello"));
        assert!(glob_match("api/*", "api/hello"));
        assert!(!glob_match("api/*.js", "api/sub/hello")); // * doesn't cross '/'
        assert!(glob_match("api/**/*.ts", "api/sub/hello"));
        assert!(glob_match("api/**/*", "api/a/b/c"));
        assert!(glob_match("api/test.js", "api/test"));
        assert!(glob_match("src/pages/**/*", "src/pages/isr/x"));
        assert!(!glob_match("api/users", "api/posts"));
    }

    #[test]
    fn apply_vercel_config_merges() {
        use fluid_build::VercelConfig;
        let vc = VercelConfig::from_json(
            r#"{
              "redirects": [{ "source": "/old", "destination": "/new", "permanent": false }],
              "headers": [{ "source": "/(.*)", "headers": [{ "key": "X-A", "value": "1" }] }],
              "cleanUrls": true,
              "trailingSlash": false,
              "crons": [{ "path": "/api/cron", "schedule": "0 0 * * *" }],
              "functions": { "api/*": { "maxDuration": 45, "memory": 3009 } }
            }"#,
        )
        .unwrap();
        let mut m = Manifest {
            functions: vec![FunctionConfig {
                name: "api/hello".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        apply_vercel_config(&mut m, &vc, &|_| {});
        assert_eq!(m.redirects.len(), 1);
        assert_eq!(m.redirects[0].status, 307); // permanent:false
        assert_eq!(m.headers.len(), 1);
        assert!(m.clean_urls);
        assert_eq!(m.trailing_slash, Some(false));
        assert_eq!(m.crons.len(), 1);
        assert_eq!(m.functions[0].max_duration_secs, 45);
        assert_eq!(m.functions[0].memory_mib, 3009);
        assert_eq!(m.functions[0].vcpus, 2);
    }

    #[test]
    fn project_name_from_url_sanitizes() {
        assert_eq!(
            project_name_from_url("https://github.com/vercel/next.js.git"),
            "next-js"
        );
        assert_eq!(
            project_name_from_url("https://github.com/Owner/My_Repo"),
            "my-repo"
        );
        assert_eq!(
            project_name_from_url("git@github.com:acme/cool-app.git"),
            "cool-app"
        );
        assert_eq!(project_name_from_url("https://example.com/a/b/"), "b");
    }

    #[test]
    fn npm_ci_only_first_deploy_or_cache_disabled_with_package_lock() {
        // First deploy + package-lock.json (npm) -> npm ci (Task 1).
        assert!(should_use_npm_ci("npm", true, true, true));
        // Redeploy (not first) with cache enabled -> npm install (warm cache).
        assert!(!should_use_npm_ci("npm", true, false, true));
        // Redeploy with cache DISABLED + package-lock.json -> npm ci (Task 2).
        assert!(should_use_npm_ci("npm", true, false, false));
        // No package-lock.json -> never npm ci (it would hard-fail).
        assert!(!should_use_npm_ci("npm", false, true, true));
        assert!(!should_use_npm_ci("npm", false, false, false));
        // Non-npm package managers never use npm ci, regardless of flags.
        assert!(!should_use_npm_ci("yarn", true, true, false));
        assert!(!should_use_npm_ci("pnpm", true, true, false));
        assert!(!should_use_npm_ci("bun", true, false, false));
    }

    #[test]
    fn sanitize_tag_is_docker_safe() {
        assert_eq!(sanitize_tag("My App!!"), "my-app");
        assert_eq!(sanitize_tag("---weird///name---"), "weird-name");
        assert_eq!(sanitize_tag(""), "app");
        // Only [a-z0-9._-] survive.
        assert!(sanitize_tag("Foo/Bar:Baz")
            .chars()
            .all(|c| c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || c == '.'
                || c == '_'
                || c == '-'));
    }

    #[test]
    fn preferred_node_bin_never_panics() {
        // May be Some or None depending on the host; must not panic and, if Some,
        // must point at an existing `node`.
        if let Some(dir) = preferred_node_bin() {
            assert!(std::path::Path::new(&dir).join("node").exists());
        }
    }

    #[test]
    fn warmup_bun_bin_never_panics_and_points_at_a_real_binary() {
        // Real host lookup (Homebrew/~/.bun/bin/system) — may be Some or None
        // depending on the host, must not panic, and if Some must be executable.
        if let Some(bin) = warmup_bun_bin() {
            assert!(
                std::path::Path::new(&bin).is_file(),
                "warmup_bun_bin returned a non-existent path: {bin}"
            );
        }
    }

    #[tokio::test]
    async fn bun_version_reads_a_real_version_string() {
        // This build host has a real `bun` installed — exercise the actual
        // subprocess call (no mocking) and confirm it parses a real version.
        let Some(bin) = which_bun() else { return }; // skip on a host with no bun
        let v = bun_version(&bin)
            .await
            .expect("bun --version must succeed on a real bun binary");
        assert!(
            v.chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false),
            "unexpected bun --version output: {v}"
        );
    }

    #[test]
    fn bun_bundle_entry_accepts_plain_bun_invocations() {
        assert_eq!(
            bun_bundle_entry(&["bun".into(), "server.js".into()]),
            Some("server.js".into())
        );
        assert_eq!(
            bun_bundle_entry(&["bun".into(), "run".into(), "server.js".into()]),
            Some("server.js".into())
        );
        assert_eq!(
            bun_bundle_entry(&["/usr/local/bin/bun".into(), "index.mjs".into()]),
            Some("index.mjs".into()),
            "must match on basename, not just literal \"bun\""
        );
    }

    #[test]
    fn bun_bundle_entry_rejects_cli_wrapper_invocations() {
        // Vercel's own documented Next.js+Bun form — a CLI wrapper, not a
        // bundleable entry file. Must be explicitly rejected, not guessed at.
        assert_eq!(
            bun_bundle_entry(&[
                "bun".into(),
                "run".into(),
                "--bun".into(),
                "next".into(),
                "start".into()
            ]),
            None
        );
        assert_eq!(
            bun_bundle_entry(&["bunx".into(), "--bun".into(), "next".into(), "start".into()]),
            None
        );
        assert_eq!(bun_bundle_entry(&["node".into(), "server.js".into()]), None);
        assert_eq!(bun_bundle_entry(&[]), None);
        // `detect_start_cmd`'s own package.json#scripts.start shape
        // (`["bun","run","--bun","start"]`) — a script-NAME, not a file path —
        // must also be rejected (there is nothing to bundle; "start" doesn't
        // resolve to a file on disk).
        assert_eq!(
            bun_bundle_entry(&["bun".into(), "run".into(), "--bun".into(), "start".into()]),
            None
        );
    }

    #[tokio::test]
    async fn detect_start_cmd_regression_scripts_start_forces_real_bun_not_node() {
        // REGRESSION TEST for a real bug found via live verification: a
        // `package.json` with `"scripts": {"start": "node server.js"}` (the
        // single most common real-world shape — it's what `npm init`/most
        // hand-authored package.jsons emit) previously produced
        // `["bun","run","start"]` under an explicit Bun runtime. That
        // literally re-executes the script's own text ("node server.js") —
        // Bun's script-runner treats package.json scripts as shell commands,
        // so it spawned REAL Node, silently defeating the Bun runtime choice.
        // Verified live: `bun run start` on such a script reported
        // `process.versions.bun === null`; `bun run --bun start` reported the
        // real Bun version. `detect_start_cmd` must always include `--bun`.
        let dir = std::env::temp_dir().join(format!(
            "hive-detect-start-cmd-bun-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &dir.join("package.json"),
            r#"{"scripts":{"start":"node server.js"}}"#,
        )
        .unwrap();
        let cmd = detect_start_cmd(&dir, Some(hive_core::Runtime::Bun)).await;
        assert_eq!(cmd, vec!["bun", "run", "--bun", "start"]);
        // The Node path must stay byte-for-byte identical to before.
        let cmd_node = detect_start_cmd(&dir, None).await;
        assert_eq!(cmd_node, vec!["npm", "start"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bun_express_api_example_fixture_gets_the_bun_flag() {
        // Runs the REAL `detect_start_cmd` against the checked-in
        // `examples/bun-express-api` fixture — the exact repo shape that
        // exposed the `--bun` bug above (manually verified live both ways).
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bun-express-api");
        assert!(dir.exists(), "fixture missing: {}", dir.display());
        let cmd = detect_start_cmd(&dir, Some(hive_core::Runtime::Bun)).await;
        assert_eq!(cmd, vec!["bun", "run", "--bun", "start"]);
    }

    #[tokio::test]
    async fn bun_hono_api_example_fixture_resolves_bun_start() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bun-hono-api");
        assert!(dir.exists(), "fixture missing: {}", dir.display());
        let cmd = detect_start_cmd(&dir, Some(hive_core::Runtime::Bun)).await;
        assert_eq!(cmd, vec!["bun", "run", "--bun", "start"]);
    }

    #[tokio::test]
    async fn warmup_bun_bytecode_real_bundle_and_bytecode_cache() {
        // Real end-to-end exercise of the actual Bun mechanism this function
        // wraps (no mocks): bundle a real entry file with `bun build --bytecode`
        // and confirm the .jsc bytecode sidecar + .map source map land exactly
        // where `warmup_bun_bytecode`'s own success path expects them, using the
        // SAME `bun` binary resolution (`warmup_bun_bin`) the real code path uses.
        let Some(bun_bin) = warmup_bun_bin() else {
            return;
        }; // skip on a host with no bun
        let dir =
            std::env::temp_dir().join(format!("hive-bun-bytecode-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("server.js"),
            "Bun.serve({port: Number(process.env.PORT||0), fetch(){return new Response('ok')}});",
        )
        .unwrap();
        let outdir = dir.join(".hive-bun-bytecode");
        let out = tokio::process::Command::new(&bun_bin)
            .arg("build")
            .arg("--bytecode")
            .arg("--sourcemap=external")
            .arg("--target=bun")
            .arg(format!("--outdir={}", outdir.to_string_lossy()))
            .arg(dir.join("server.js"))
            .current_dir(&dir)
            .output()
            .await
            .expect("bun build must run");
        assert!(
            out.status.success(),
            "bun build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(outdir.join("server.js").is_file(), "bundled entry missing");
        assert!(
            outdir.join("server.js.jsc").is_file(),
            "bytecode sidecar missing — bytecode cache did not activate"
        );
        assert!(
            outdir.join("server.js.map").is_file(),
            "external source map missing"
        );
        // `bun run` on the bundled file must actually execute correctly (the whole
        // point — a cache that produces an unrunnable artifact is worse than none).
        let run = tokio::process::Command::new(&bun_bin)
            .arg("run")
            .arg(outdir.join("server.js"))
            .env("PORT", "0")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = run {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            // Still alive (didn't crash on boot) — good enough signal without a
            // full HTTP round trip; kill it immediately after.
            let alive = child.try_wait().ok().flatten().is_none();
            let _ = child.start_kill();
            assert!(
                alive,
                "bundled+bytecode-cached server crashed immediately on boot"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cache_key_is_deterministic_and_content_sensitive() {
        let base = std::env::temp_dir().join(format!("oe-cachekey-{}", now_ms()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        tokio::fs::write(base.join("package-lock.json"), b"{\"v\":1}")
            .await
            .unwrap();

        let k1 = compute_cache_key(&base, "npm").await;
        let k2 = compute_cache_key(&base, "npm").await;
        assert!(k1.is_some());
        assert_eq!(k1, k2, "same lockfile+pm must yield the same key");

        // Different package manager → different key.
        let k_pnpm = compute_cache_key(&base, "pnpm").await;
        assert_ne!(k1, k_pnpm);

        // Changed lockfile → different key.
        tokio::fs::write(base.join("package-lock.json"), b"{\"v\":2}")
            .await
            .unwrap();
        let k3 = compute_cache_key(&base, "npm").await;
        assert_ne!(k1, k3, "changed lockfile must change the key");

        // No lockfile/package.json → None.
        let empty = std::env::temp_dir().join(format!("oe-cachekey-empty-{}", now_ms()));
        tokio::fs::create_dir_all(&empty).await.unwrap();
        assert_eq!(compute_cache_key(&empty, "npm").await, None);

        let _ = tokio::fs::remove_dir_all(&base).await;
        let _ = tokio::fs::remove_dir_all(&empty).await;
    }

    #[test]
    fn build_store_insert_log_update() {
        let store = BuildStore::new();
        store.insert(Build {
            id: "dpl-test".into(),
            project: "demo".into(),
            repo_url: "https://github.com/a/b".into(),
            branch: "main".into(),
            commit: String::new(),
            commit_message: String::new(),
            state: DeployState::Building,
            started_ms: now_ms(),
            finished_ms: None,
            deployment_id: None,
            alias: None,
            superseded_by: None,
            error: None,
            lines: Vec::new(),
        });
        store.log("dpl-test", "building…");
        store.update("dpl-test", |b| {
            b.state = DeployState::Ready;
            b.finished_ms = Some(now_ms());
        });
        let b = store.get("dpl-test").expect("build exists");
        assert!(matches!(b.state, DeployState::Ready));
        assert_eq!(b.lines.len(), 1);
        assert_eq!(b.lines[0].line, "building…");
        assert!(b.finished_ms.is_some());
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn build_log_redaction_matches_run_streamed_wiring() {
        // REGRESSION TEST for a real, confirmed vulnerability: `run_streamed`
        // used to inject decrypted project env vars into the build process
        // and persist stdout/stderr verbatim — a build script that echoes env
        // (verbose install-tool dumps, or a one-line `"prebuild": "env"`)
        // leaked every real secret in cleartext into the team-readable build
        // log. Exercises the EXACT expression `run_streamed` now runs on every
        // line: collect `env.values()` into the redaction list, then
        // `sandboxes::redact_secrets`.
        let mut env: std::collections::BTreeMap<String, String> = Default::default();
        env.insert(
            "DATABASE_URL".into(),
            "postgres://u:sup3rSecr3t@host/db".into(),
        );
        env.insert("STRIPE_SECRET_KEY".into(), "sk_live_abc123xyz".into());
        env.insert("NODE_ENV".into(), "production".into());

        let secret_values: Vec<String> = env.values().filter(|v| !v.is_empty()).cloned().collect();

        let line = "prebuild: DATABASE_URL=postgres://u:sup3rSecr3t@host/db STRIPE_SECRET_KEY=sk_live_abc123xyz NODE_ENV=production npm WARN deprecated left-pad@1.0.0";
        let redacted = crate::sandboxes::redact_secrets(line, &secret_values);

        assert!(
            !redacted.contains("sup3rSecr3t"),
            "DB password must be redacted: {redacted}"
        );
        assert!(
            !redacted.contains("sk_live_abc123xyz"),
            "Stripe key must be redacted: {redacted}"
        );
        // Conservative-by-design: EVERY injected env value gets redacted, not
        // just ones that look like secrets (matching sandboxes_platform.rs's
        // identical "redact every value" convention) — so NODE_ENV's value
        // disappears too, a deliberate false-positive tradeoff.
        assert!(
            !redacted.contains("=production"),
            "every injected value is redacted, including non-secret-looking ones: {redacted}"
        );
        assert!(
            redacted.contains("[REDACTED]"),
            "must show a redaction marker: {redacted}"
        );
        // Content that was NEVER an injected env value (ordinary build-tool
        // chatter) must survive untouched.
        assert!(
            redacted.contains("npm WARN deprecated left-pad@1.0.0"),
            "unrelated log content must survive: {redacted}"
        );
    }
}
