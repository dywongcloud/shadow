#![allow(
    dead_code,
    reason = "The optional build-cache and runtime-warmup paths remain compiled for capability validation, but are not enabled by the current deployment flow."
)]

//! Deploy from a git repository with a live, Vercel-style **build log**.
//!
//! `start_build` creates a build record (state = building) and returns its id
//! immediately; a background task clones the repo, emits timestamped log lines
//! (region, machine config, cloning, install/build commands, ready), then
//! registers the routable deployment via the gateway. The dashboard polls
//! `GET /v1/builds/:id` to stream the logs as they appear.

use anyhow::Context as _;
use base64::Engine;
use futures::StreamExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fluid_core::{
    DeployState, FunctionConfig, GitDeployRequest, GitSource, Manifest, PortSpec,
    ProjectIncarnation, Route, RouteTarget, ServiceProtocol,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_incarnation: Option<ProjectIncarnation>,
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

/// Fanout capability boundary for paired host-static/runtime-artifact semantics.
/// Its location deliberately cannot match the legacy mesh prefix arm.
const RUNTIME_ARTIFACT_FANOUT_PATH: &str = "/v1/runtime-artifact/v1/git/deploy";

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
    /// Remove build records proved to belong to one deleted incarnation. Legacy
    /// records carry no identity and are retained rather than guessed from a
    /// project-name prefix.
    pub fn remove_for_incarnation(
        &self,
        project: &str,
        incarnation: ProjectIncarnation,
    ) -> Vec<Build> {
        let mut map = self.map.lock();
        let ids: Vec<String> = map
            .values()
            .filter(|build| {
                build.project == project && build.project_incarnation == Some(incarnation)
            })
            .map(|build| build.id.clone())
            .collect();
        ids.into_iter().filter_map(|id| map.remove(&id)).collect()
    }

    pub fn ids_for_incarnation(
        &self,
        project: &str,
        incarnation: ProjectIncarnation,
    ) -> Vec<String> {
        self.map
            .lock()
            .values()
            .filter(|build| {
                build.project == project && build.project_incarnation == Some(incarnation)
            })
            .map(|build| build.id.clone())
            .collect()
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

struct BuildCancelSlot {
    project: String,
    incarnation: ProjectIncarnation,
    /// Process-GROUP id of whatever child THIS build is currently blocked on.
    /// `None` between steps (no command in flight right now).
    pgid: Mutex<Option<u32>>,
    /// Set true the instant cancellation begins — checked at step boundaries.
    cancelled: std::sync::atomic::AtomicBool,
    /// Set while this build mirrors a remote fanned-out build.
    mirror: Mutex<Option<MirrorTarget>>,
    /// The task actually driving this build.
    task: Mutex<Option<tokio::task::AbortHandle>>,
    /// Completion is released only by `BuildCompletionGuard::drop`, including
    /// task abort and panic. Deletion waits on this before touching checkouts.
    completed: std::sync::atomic::AtomicBool,
    completion: tokio::sync::Notify,
}

impl BuildCancelSlot {
    fn new(project: String, incarnation: ProjectIncarnation) -> Self {
        Self {
            project,
            incarnation,
            pgid: Mutex::new(None),
            cancelled: std::sync::atomic::AtomicBool::new(false),
            mirror: Mutex::new(None),
            task: Mutex::new(None),
            completed: std::sync::atomic::AtomicBool::new(false),
            completion: tokio::sync::Notify::new(),
        }
    }
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

    /// Register a fresh slot before the driving task is spawned. Returning the
    /// exact Arc lets the task's Drop guard remove only its own slot, never a
    /// later replacement that reused the same build id.
    fn register(
        &self,
        bid: &str,
        project: &str,
        incarnation: ProjectIncarnation,
    ) -> Arc<BuildCancelSlot> {
        let slot = Arc::new(BuildCancelSlot::new(project.to_string(), incarnation));
        self.map.lock().insert(bid.to_string(), slot.clone());
        slot
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

    fn complete(&self, bid: &str, slot: &Arc<BuildCancelSlot>) {
        slot.completed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        slot.completion.notify_waiters();
        let mut map = self.map.lock();
        if map
            .get(bid)
            .is_some_and(|registered| Arc::ptr_eq(registered, slot))
        {
            map.remove(bid);
        }
    }

    async fn wait_completed(slot: &Arc<BuildCancelSlot>, deadline: tokio::time::Instant) -> bool {
        loop {
            if slot.completed.load(std::sync::atomic::Ordering::SeqCst) {
                return true;
            }
            let notified = slot.completion.notified();
            if slot.completed.load(std::sync::atomic::Ordering::SeqCst) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return slot.completed.load(std::sync::atomic::Ordering::SeqCst);
            }
        }
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
        // Completion is owned by the task's Drop guard. Never erase the slot
        // here: deletion may already hold this Arc and must observe the task and
        // every checkout guard unwind before reclaiming source.
        let _ =
            Self::wait_completed(&slot, tokio::time::Instant::now() + Duration::from_secs(2)).await;
        true
    }

    /// Cancel every active build owned by one exact incarnation and wait for
    /// their Drop-owned completion. The returned residual ids are observable
    /// cleanup failures; callers must not reclaim source while it is non-empty.
    pub async fn cancel_project_and_drain(
        &self,
        cloud: &Arc<CloudState>,
        project: &str,
        incarnation: ProjectIncarnation,
        timeout: Duration,
    ) -> Result<Vec<String>, Vec<String>> {
        let slots: Vec<(String, Arc<BuildCancelSlot>)> = self
            .map
            .lock()
            .iter()
            .filter(|(_, slot)| slot.project == project && slot.incarnation == incarnation)
            .map(|(id, slot)| (id.clone(), slot.clone()))
            .collect();
        if slots.is_empty() {
            return Ok(Vec::new());
        }
        for (id, _) in &slots {
            let _ = self.cancel(cloud, id).await;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut residual = Vec::new();
        for (id, slot) in &slots {
            if !Self::wait_completed(slot, deadline).await {
                residual.push(id.clone());
            }
        }
        if residual.is_empty() {
            Ok(slots.into_iter().map(|(id, _)| id).collect())
        } else {
            Err(residual)
        }
    }
}

struct BuildCompletionGuard {
    cloud: Arc<CloudState>,
    bid: String,
    slot: Arc<BuildCancelSlot>,
}

impl Drop for BuildCompletionGuard {
    fn drop(&mut self) {
        self.cloud.build_cancels.complete(&self.bid, &self.slot);
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
        return crate::gossip::request_to(cloud, id, addr, hive_p2p::GOSSIP_POST, &path, &[], 15)
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

/// Feed a GitHub token to a git subprocess without ever putting it on the
/// command line, in an environment variable, or into `.git/config`. Standard
/// `git credential.helper` mechanism: the helper is a FIXED shell one-liner
/// that reads a pre-formatted `username=...\npassword=...\n` credential
/// response off an inherited file descriptor. The descriptor backs a 0600 temp
/// file that is `unlink()`-ed the instant after it opens — the fd stays valid
/// and readable, but the path is gone immediately, so no other process can
/// reach it by name, and root-owns it by construction (this daemon runs as
/// root) — with FD_CLOEXEC explicitly cleared so it survives into git's
/// `exec()` (`std::fs::File` sets FD_CLOEXEC by default; git must not lose the
/// fd at the very step that needs it). `-c credential.helper=` is applied
/// FIRST to clear any inherited helper — a base image's `osxkeychain`/
/// `libsecret`/`store` entry would otherwise be consulted ahead of ours and
/// could leak an unrelated stored credential into a tenant's build (witnessed
/// directly while prototyping this fix: `git credential fill` silently filled
/// in a real, unrelated macOS-keychain-stored PAT before a custom helper ever
/// ran, because nothing had cleared the existing helper chain first) — and the
/// replacement is scoped to `credential.https://github.com.helper`, never the
/// blanket `credential.helper` slot, so it is offered only for github.com
/// HTTPS, matching every existing token resolver (`resolve_git_poll_token`,
/// `git_webhook`'s installation-token minting), which are github.com-only.
/// Every caller MUST keep the returned `File` alive until `Command::spawn`
/// returns: `fork()` copies the parent's fd table before `spawn()` returns, so
/// the child already holds its own reference by then and the parent's copy
/// (dropped when the `File` goes out of scope — including on cancellation,
/// this repo's Drop-releases-on-abort discipline) can close right after.
fn credential_feed(token: &str) -> anyhow::Result<(std::fs::File, [String; 4])> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let path = std::env::temp_dir().join(format!(
        ".hive-git-cred-{}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open credential feed {}", path.display()))?;
    // Unlink immediately: the fd stays valid and readable, but the path is gone
    // the instant after open — never a file with a persistent path.
    let _ = std::fs::remove_file(&path);
    write!(file, "username=x-access-token\npassword={token}\n").context("write credential feed")?;
    file.flush().context("flush credential feed")?;
    file.seek(SeekFrom::Start(0))
        .context("rewind credential feed")?;
    let raw = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    anyhow::ensure!(
        flags >= 0,
        "fcntl(F_GETFD) on credential feed failed: {}",
        std::io::Error::last_os_error()
    );
    anyhow::ensure!(
        unsafe { libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == 0,
        "fcntl(F_SETFD) on credential feed failed: {}",
        std::io::Error::last_os_error()
    );
    Ok((
        file,
        [
            "-c".to_string(),
            "credential.helper=".to_string(),
            "-c".to_string(),
            format!("credential.https://github.com.helper=!f() {{ cat <&{raw}; }}; f"),
        ],
    ))
}

/// Apply credential config to a git subprocess: always clears any inherited
/// `credential.helper` first (see `credential_feed`'s doc — this alone closes
/// the "ambient stored credential leaks into a tenant build" surface even for
/// an anonymous/public clone), then wires the fixed FD-backed helper when a
/// token is present. Returns the open `File` the caller must hold alive until
/// the command has actually been spawned.
fn apply_credential(
    cmd: &mut Command,
    token: Option<&str>,
) -> anyhow::Result<Option<std::fs::File>> {
    match token {
        Some(t) if !t.is_empty() => {
            let (file, args) = credential_feed(t)?;
            cmd.args(args);
            Ok(Some(file))
        }
        _ => {
            cmd.arg("-c").arg("credential.helper=");
            Ok(None)
        }
    }
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
    if out.is_empty() { "app".into() } else { out }
}

pub(crate) fn project_volume_name(
    project: &str,
    incarnation: ProjectIncarnation,
    service: Option<&str>,
) -> String {
    let mut name = format!(
        "hive-vol-{}-{}",
        sanitize_tag(project),
        incarnation.path_component()
    );
    if let Some(service) = service.filter(|service| !service.is_empty()) {
        name.push('-');
        name.push_str(&sanitize_tag(service));
    }
    name
}

/// Extract only current-incarnation volume names that the server-authored
/// container run configuration will actually hand to podman. A stored legacy
/// or malformed configuration is not cleanup authority: callers either reject
/// registration or retain it as an observable cleanup residual.
pub(crate) fn project_volume_names(
    project: &str,
    incarnation: ProjectIncarnation,
    functions: &[fluid_core::FunctionConfig],
) -> Result<std::collections::BTreeSet<String>, String> {
    let base = project_volume_name(project, incarnation, None);
    let mut names = std::collections::BTreeSet::new();
    for function in functions
        .iter()
        .filter(|function| function.runtime == "container")
    {
        if function.start_cmd.first().map(String::as_str) != Some("__container__") {
            return Err(format!(
                "container function {:?} has no platform container marker",
                function.name
            ));
        }
        let config = function.start_cmd.get(3).ok_or_else(|| {
            format!(
                "container function {:?} has no platform run configuration",
                function.name
            )
        })?;
        let config: serde_json::Value = serde_json::from_str(config).map_err(|error| {
            format!(
                "container function {:?} has malformed platform run configuration: {error}",
                function.name
            )
        })?;
        let volume = config
            .get("vol")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "container function {:?} has no platform volume name",
                    function.name
                )
            })?;
        let service = project_volume_name(project, incarnation, Some(&function.name));
        if volume != base && volume != service {
            return Err(format!(
                "container function {:?} volume is not owned by project incarnation {}",
                function.name, incarnation
            ));
        }
        names.insert(volume.to_string());
    }
    Ok(names)
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

/// Checkout directories currently being created or read by a build. The GC
/// and source-selector share this one lock: either a build owns a directory or
/// the reaper owns its deletion, never both. Counts keep nested/reentrant reads
/// honest without making the path registry a set with premature release.
#[derive(Default)]
struct ActiveCheckoutOwners {
    total: usize,
    by_incarnation: HashMap<(String, ProjectIncarnation), usize>,
}

static ACTIVE_CHECKOUTS: std::sync::OnceLock<Mutex<HashMap<PathBuf, ActiveCheckoutOwners>>> =
    std::sync::OnceLock::new();

fn active_checkouts() -> &'static Mutex<HashMap<PathBuf, ActiveCheckoutOwners>> {
    ACTIVE_CHECKOUTS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct ActiveCheckoutGuard {
    path: PathBuf,
    project: String,
    incarnation: ProjectIncarnation,
}

impl ActiveCheckoutGuard {
    fn new(path: PathBuf, project: &str, incarnation: ProjectIncarnation) -> Self {
        let mut active = active_checkouts().lock();
        let owners = active.entry(path.clone()).or_default();
        owners.total += 1;
        *owners
            .by_incarnation
            .entry((project.to_string(), incarnation))
            .or_default() += 1;
        drop(active);
        Self {
            path,
            project: project.to_string(),
            incarnation,
        }
    }

    fn register_locked(
        active: &mut HashMap<PathBuf, ActiveCheckoutOwners>,
        path: PathBuf,
        project: &str,
        incarnation: ProjectIncarnation,
    ) -> Self {
        let owners = active.entry(path.clone()).or_default();
        owners.total += 1;
        *owners
            .by_incarnation
            .entry((project.to_string(), incarnation))
            .or_default() += 1;
        Self {
            path,
            project: project.to_string(),
            incarnation,
        }
    }
}

impl Drop for ActiveCheckoutGuard {
    fn drop(&mut self) {
        let mut active = active_checkouts().lock();
        let Some(owners) = active.get_mut(&self.path) else {
            return;
        };
        owners.total = owners.total.saturating_sub(1);
        let owner = (self.project.clone(), self.incarnation);
        if let Some(count) = owners.by_incarnation.get_mut(&owner) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                owners.by_incarnation.remove(&owner);
            }
        }
        if owners.total == 0 {
            active.remove(&self.path);
        }
    }
}

pub(crate) fn active_checkout_paths(
    project: &str,
    incarnation: ProjectIncarnation,
) -> Vec<PathBuf> {
    let owner = (project.to_string(), incarnation);
    active_checkouts()
        .lock()
        .iter()
        .filter(|(_, owners)| owners.by_incarnation.contains_key(&owner))
        .map(|(path, _)| path.clone())
        .collect()
}

/// Wait until every checkout reader/writer for one deleted incarnation has
/// released its Drop-owned reservation. The bounded error names the exact paths
/// that remain owned, so deletion can retain and report them rather than race a
/// reader or claim complete cleanup.
pub(crate) async fn wait_for_checkout_drain(
    project: &str,
    incarnation: ProjectIncarnation,
    timeout: Duration,
) -> Result<(), Vec<PathBuf>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let active = active_checkout_paths(project, incarnation);
        if active.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(active);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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

fn checkout_matches_source_id(name: &str, prefixes: &[String], id: &str) -> bool {
    if !valid_source_build_id(id) {
        return false;
    }
    let suffix = format!("-{id}");
    let Some(stem) = name.strip_suffix(&suffix) else {
        return false;
    };
    prefixes.iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|stamp| {
            !stamp.is_empty() && stamp.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

/// `newest_deploy_dir` restricted to checkouts belonging to specific
/// BUILD ids: dirs are named `{tag}-{ms}-{build_id}`. An empty allowlist is
/// never an upload-redeploy wildcard; absent provenance fails closed.
pub(crate) fn newest_deploy_dir_for_ids(project: &str, ids: &[String]) -> Option<PathBuf> {
    newest_deploy_dir_for_ids_inner(project, ids)
}

fn newest_deploy_dir_for_ids_inner(project: &str, ids: &[String]) -> Option<PathBuf> {
    if ids.is_empty() {
        return None;
    }
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
            if !ids
                .iter()
                .any(|id| checkout_matches_source_id(&name, &prefixes, id))
            {
                continue;
            }
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
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

/// Select and pin retained source under the same lock the reaper takes before
/// deletion. The selector can therefore never hand a path to `copy_dir_into`
/// after GC has decided to remove it, nor can GC claim it after selection.
fn acquire_deploy_dir_for_ids(
    project: &str,
    incarnation: ProjectIncarnation,
    ids: &[String],
) -> Option<(PathBuf, ActiveCheckoutGuard)> {
    let mut active = active_checkouts().lock();
    let path = newest_deploy_dir_for_ids_inner(project, ids)?;
    let guard =
        ActiveCheckoutGuard::register_locked(&mut active, path.clone(), project, incarnation);
    Some((path, guard))
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

fn checkout_paths_overlap(left: &Path, right: &Path) -> bool {
    if left == right || left.starts_with(right) || right.starts_with(left) {
        return true;
    }
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right || left.starts_with(&right) || right.starts_with(&left)
}

/// Claim and remove one stale candidate under the same lock used to register
/// active checkouts. A completed build registers its deployment before its
/// guard drops, so checking active owners first and current deployment roots
/// second leaves no gap where neither can protect the directory.
fn reap_build_dir_if_unprotected(cloud: &Arc<CloudState>, path: &Path) -> bool {
    let active = active_checkouts().lock();
    if active
        .keys()
        .any(|active_path| checkout_paths_overlap(path, active_path))
    {
        return false;
    }
    if cloud
        .gw
        .deployment_records()
        .iter()
        .any(|record| checkout_paths_overlap(path, Path::new(&record.root)))
    {
        return false;
    }
    std::fs::remove_dir_all(path).is_ok()
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
            let stale = e
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| now.duration_since(t).ok())
                .map(|age| age > max_age)
                .unwrap_or(false);
            if !stale {
                continue;
            }
            let candidate = p.clone();
            let cloud = cloud.clone();
            let reaped = tokio::task::spawn_blocking(move || {
                reap_build_dir_if_unprotected(&cloud, &candidate)
            })
            .await
            .unwrap_or(false);
            if reaped {
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
    incarnation: ProjectIncarnation,
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
        let incarnation = self.incarnation;
        handle.spawn(async move {
            cloud.gw.remove_exact(&id, incarnation).await;
            crate::persist::persist(&cloud);
            tracing::warn!(deployment = %id, "removed Building… placeholder after its build task ended without finishing (cancel/panic)");
        });
    }
}

/// Start a build and return its id after its lifecycle reservations exist.
/// `expected_incarnation` is authority passed separately by the authenticated
/// fanout receiver; the identically-named request field is never trusted here.
pub async fn start_build(
    cloud: Arc<CloudState>,
    mut req: GitDeployRequest,
    expected_incarnation: Option<ProjectIncarnation>,
    admission_team: Option<String>,
) -> anyhow::Result<String> {
    // Reject a credential-bearing or structurally-unsafe source BEFORE any of
    // it can reach a `Build` row, a webhook payload, a log line, or process
    // argv — everything below this line persists/emits `req.repo_url`.
    validate_deploy_source(&req)?;
    let id = format!("dpl-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let project = req
        .project
        .clone()
        .unwrap_or_else(|| project_name_from_url(&req.repo_url));
    req.project_incarnation = None;
    // Non-Git sources carry no Git credential need; strip it here so it can
    // never ride a fanout/mirror payload for a source that never clones.
    if req.image_ref.is_some()
        || req.zip_b64.is_some()
        || req.repo_url.starts_with("upload://")
        || req.repo_url.starts_with("image://")
    {
        req.git_token = None;
    }

    let _lifecycle = crate::project_settings::lifecycle_write(&project).await;
    let legacy_deployment_ids = admission_team
        .as_deref()
        .map(|team| crate::admin::legacy_deployment_ids_for_owner(&cloud, &project, team))
        .unwrap_or_default();
    let incarnation = match expected_incarnation {
        Some(expected) => cloud
            .projects
            .ensure_incarnation_exact(&project, expected)?,
        None => cloud.projects.ensure_incarnation(&project)?,
    };
    if let Some(team) = admission_team.as_deref() {
        cloud.projects.set_team_exact(&project, incarnation, team)?;
    }
    if let Some(root) = req
        .root_dir
        .as_deref()
        .map(str::trim)
        .filter(|root| !root.is_empty())
    {
        cloud
            .projects
            .set_root_dir_exact(&project, incarnation, root)?;
    }
    crate::admin::adopt_authorized_legacy_deployments(
        &cloud,
        &project,
        incarnation,
        legacy_deployment_ids,
    )?;
    cloud.git_index.set_project_repo(&project, &req.repo_url);
    req.project_incarnation = Some(incarnation);

    // Latest-push-wins is incarnation-scoped: a new same-name project cannot
    // supersede work retained from a deleted predecessor.
    for b in cloud.builds.list() {
        if b.project == project
            && b.project_incarnation == Some(incarnation)
            && matches!(b.state, DeployState::Queued | DeployState::Building)
        {
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
        project_incarnation: Some(incarnation),
        repo_url: req.repo_url.clone(),
        branch: req.branch.clone().unwrap_or_default(),
        commit: req.commit.clone().unwrap_or_default(),
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
    crate::persist::persist(&cloud);
    crate::webhooks::dispatch(
        &cloud.webhooks,
        &project,
        "deployment.created",
        serde_json::json!({ "id": id, "project": project, "repo": req.repo_url, "state": "building" }),
    );

    let slot = cloud.build_cancels.register(&id, &project, incarnation);
    let completion = BuildCompletionGuard {
        cloud: cloud.clone(),
        bid: id.clone(),
        slot,
    };
    let bid = id.clone();
    let wh_project = project.clone();
    let cloud_for_registry = cloud.clone();
    let handle = tokio::spawn(async move {
        let _completion = completion;
        let first_deploy = !project_has_deployment(&cloud, &project);
        let mut placeholder = PlaceholderGuard {
            cloud: cloud.clone(),
            id: register_building_placeholder(&cloud, &project, incarnation, &req, &bid).await,
            incarnation,
        };
        let result = run_build(&cloud, &bid, req, project, incarnation, first_deploy).await;
        if let Some(pid) = placeholder.disarm() {
            let _ = cloud.gw.remove_exact(&pid, incarnation).await;
            crate::persist::persist(&cloud);
        }
        if let Err(e) = result {
            let cancelled = e.downcast_ref::<BuildCancelled>().is_some()
                || cloud.build_cancels.is_cancelled(&bid);
            if cancelled {
                cloud
                    .builds
                    .log(&bid, "Build cancelled by user.".to_string());
                cloud.builds.update(&bid, |b| {
                    if !matches!(b.state, DeployState::Cancelled) {
                        b.state = DeployState::Cancelled;
                        b.finished_ms.get_or_insert_with(now_ms);
                    }
                });
                crate::persist::persist(&cloud);
            } else {
                let err_text = e.to_string();
                cloud.builds.log(&bid, format!("Error: {err_text}"));
                cloud.builds.update(&bid, |b| {
                    b.state = DeployState::Error;
                    b.finished_ms = Some(now_ms());
                    b.error = Some(err_text.clone());
                });
                crate::persist::persist(&cloud);
                crate::webhooks::dispatch(
                    &cloud.webhooks,
                    &wh_project,
                    "deployment.error",
                    serde_json::json!({ "id": bid, "project": wh_project, "error": e.to_string() }),
                );
            }
        }
    });
    cloud_for_registry
        .build_cancels
        .attach_task(&id, handle.abort_handle());
    Ok(id)
}

/// True when THIS node holds retained source that can rebuild one of `ids`.
/// `ids` are platform-issued BUILD ids stamped into upload GitSource.commit;
/// empty/legacy provenance fails closed instead of selecting a different lane.
pub(crate) fn has_local_source_for_ids(project: &str, ids: &[String]) -> bool {
    retained_source_path_for_ids(project, ids).is_some()
        || newest_deploy_dir_for_ids(project, ids).is_some()
}

fn valid_source_build_id(id: &str) -> bool {
    id.strip_prefix("dpl-").is_some_and(|suffix| {
        suffix.len() == 10
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(crate) fn upload_source_build_id(source: &fluid_core::GitSource) -> Option<String> {
    (source.repo_url.starts_with("upload://") && valid_source_build_id(&source.commit))
        .then(|| source.commit.clone())
}

/// Durable retained source for one upload lineage. The build id is platform-
/// issued and validated before it becomes a path component; previews and
/// production uploads therefore never overwrite one project-wide archive.
fn retained_source_write_path(project: &str, build_id: &str) -> PathBuf {
    debug_assert!(valid_source_build_id(build_id));
    deploy_root().join(format!("{}-{build_id}.src.zip", checkout_tag(project)))
}

fn retained_source_path_for_ids(project: &str, ids: &[String]) -> Option<PathBuf> {
    ids.iter()
        .filter(|id| valid_source_build_id(id))
        .map(|id| retained_source_write_path(project, id))
        .find(|path| path.is_file())
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
    candidates.into_iter().find(|c| build_dir.join(c).is_file())
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

/// Redeploy a project whose retained source lives on a SPECIFIC host node (not this
/// coordinator). Dispatch the rebuild to that node over the existing fanout transport
/// (HTTP admin or iroh mesh); the target, receiving an `upload://` request with no
/// archive, rebuilds from ITS OWN retained source. The remote build is mirrored into a
/// local build record so the dashboard shows a single build. Returns the build id.
pub(crate) async fn redeploy_on_host(
    cloud: Arc<CloudState>,
    project: String,
    mut req: GitDeployRequest,
    host: crate::schedule::Target,
    expected_incarnation: Option<ProjectIncarnation>,
) -> anyhow::Result<String> {
    // Same gate as `start_build` — every `cloud.builds.insert` site validates
    // first. This path's `repo_url` normally comes from an already-validated
    // stored project record, but validating again is cheap and keeps the
    // invariant absolute rather than trust-dependent on the caller.
    validate_deploy_source(&req)?;
    // Same non-Git-source token strip as `start_build` — this path exists
    // specifically for retained upload/image redeploys, which never clone.
    if req.image_ref.is_some()
        || req.zip_b64.is_some()
        || req.repo_url.starts_with("upload://")
        || req.repo_url.starts_with("image://")
    {
        req.git_token = None;
    }
    let id = format!("dpl-{}", &Uuid::new_v4().simple().to_string()[..10]);
    req.project_incarnation = None;
    let _lifecycle = crate::project_settings::lifecycle_write(&project).await;
    let incarnation = match expected_incarnation {
        Some(expected) => cloud
            .projects
            .ensure_incarnation_exact(&project, expected)?,
        None => cloud.projects.ensure_incarnation(&project)?,
    };
    req.project_incarnation = Some(incarnation);
    cloud.builds.insert(Build {
        id: id.clone(),
        project: project.clone(),
        project_incarnation: Some(incarnation),
        repo_url: req.repo_url.clone(),
        branch: req.branch.clone().unwrap_or_default(),
        commit: req.commit.clone().unwrap_or_default(),
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
    crate::persist::persist(&cloud);
    let slot = cloud.build_cancels.register(&id, &project, incarnation);
    let completion = BuildCompletionGuard {
        cloud: cloud.clone(),
        bid: id.clone(),
        slot,
    };
    let bid = id.clone();
    let cloud_for_registry = cloud.clone();
    let handle = tokio::spawn(async move {
        let _completion = completion;
        cloud.builds.log(
            &bid,
            format!("Redeploy: dispatching to host node {}", host.node),
        );
        let ok = fanout_remote(
            &cloud,
            &bid,
            &req,
            &project,
            incarnation,
            std::slice::from_ref(&host),
            true,
        )
        .await;
        let promotable = ok.promotable();
        let cancelled = (!promotable && ok.cancelled())
            || (!promotable && cloud.build_cancels.is_cancelled(&bid));
        if !promotable && !cancelled && ok.build_failed() == 0 {
            let unreachable = ok.unreachable().join(", ");
            cloud.builds.log(
                &bid,
                if unreachable.is_empty() {
                    "✗ the pinned host declined to run this deploy as a stateful single-writer service — nothing was built there and the existing deployment is untouched."
                        .to_string()
                } else {
                    format!(
                        "✗ could not reach {unreachable} to run this deploy — nothing was built there, so the existing deployment is untouched. This is a fleet-reachability fault, not a build failure; retry once the node is back."
                    )
                },
            );
        }
        cloud.builds.update(&bid, |b| {
            b.state = if promotable {
                DeployState::Ready
            } else if cancelled {
                DeployState::Cancelled
            } else {
                DeployState::Error
            };
            b.finished_ms = Some(now_ms());
        });
        crate::persist::persist(&cloud);
    });
    cloud_for_registry
        .build_cancels
        .attach_task(&id, handle.abort_handle());
    Ok(id)
}

async fn resolve_checkout_dir(
    checkout: &Path,
    configured_root: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let checkout = tokio::fs::canonicalize(checkout).await.map_err(|error| {
        anyhow::anyhow!(
            "could not resolve deployment checkout '{}': {error}",
            checkout.display()
        )
    })?;
    let Some(configured_root) = configured_root
        .map(str::trim)
        .filter(|root| !root.is_empty())
    else {
        return Ok(checkout);
    };
    anyhow::ensure!(
        configured_root.len() <= 4096 && !configured_root.chars().any(char::is_control),
        "root directory {:?} contains control characters or exceeds 4096 bytes",
        configured_root
    );

    let mut relative = PathBuf::new();
    for component in Path::new(configured_root).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => relative.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!(
                    "root directory {:?} must be a relative path inside the repository",
                    configured_root
                );
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Ok(checkout);
    }

    let mut cursor = checkout.clone();
    for component in relative.components() {
        cursor.push(component.as_os_str());
        let metadata = tokio::fs::symlink_metadata(&cursor)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "root directory {:?} was not found in the repository: {error}",
                    configured_root
                )
            })?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "root directory {:?} traverses a symbolic link; choose a real repository directory",
            configured_root
        );
    }

    let candidate = tokio::fs::canonicalize(&cursor).await.map_err(|error| {
        anyhow::anyhow!(
            "could not resolve root directory {:?}: {error}",
            configured_root
        )
    })?;
    let metadata = tokio::fs::metadata(&candidate).await?;
    anyhow::ensure!(
        metadata.is_dir(),
        "root directory {:?} is not a directory",
        configured_root
    );
    anyhow::ensure!(
        candidate.starts_with(&checkout),
        "root directory {:?} escapes the repository checkout",
        configured_root
    );
    Ok(candidate)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildTrustLane {
    Production,
    PreviewBase,
    PreviewFork,
}

impl BuildTrustLane {
    fn environment(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::PreviewBase | Self::PreviewFork => "preview",
        }
    }

    fn cache_label(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::PreviewBase => "preview-base",
            Self::PreviewFork => "preview-fork",
        }
    }

    fn is_production(self) -> bool {
        self == Self::Production
    }

    fn is_fork(self) -> bool {
        self == Self::PreviewFork
    }
}

#[derive(Clone, Debug)]
struct BuildTrustContext {
    lane: BuildTrustLane,
    canonical_repo: String,
    actual_repo: String,
}

fn repository_identity(raw: &str) -> anyhow::Result<String> {
    let raw = raw.trim();
    anyhow::ensure!(!raw.is_empty(), "repository URL is empty");
    anyhow::ensure!(
        !raw.chars().any(char::is_control),
        "repository URL contains control characters"
    );
    // A value starting with '-' could be parsed as a git OPTION instead of a
    // positional URL/pathspec by any downstream `git` invocation that forwards
    // it verbatim (clone/fetch/ls-remote all take the url as a bare arg) —
    // reject it here, once, rather than trusting every call site to guard
    // argv order.
    anyhow::ensure!(
        !raw.starts_with('-'),
        "repository URL may not begin with '-'"
    );
    if let Some(rest) = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
    {
        let authority = rest.split('/').next().unwrap_or_default();
        anyhow::ensure!(
            !authority.contains('@'),
            "repository URLs must not contain embedded credentials; connect GitHub or use the server-side git credential"
        );
        anyhow::ensure!(!authority.is_empty(), "repository URL has no host");
        let path = rest.splitn(2, '/').nth(1).unwrap_or_default();
        anyhow::ensure!(
            !path.trim_matches('/').is_empty(),
            "repository URL has no path"
        );
    }
    Ok(raw
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(raw.trim_end_matches('/'))
        .to_string())
}

/// An OCI reference may contain at most one `@`, and only ever as a content
/// digest pin (`@sha256:<hex>`, or another registered digest algorithm) — that
/// is the ONLY place `@` is valid in OCI reference grammar. A crafted string
/// shaped like HTTP userinfo (`user:token@registry/...`) is not valid OCI
/// syntax, but nothing downstream re-validates it before it reaches
/// `podman pull`'s argv and every log line that names the image verbatim
/// (`image_container_manifest`) — reject it here, before either happens.
fn reject_credential_bearing_image_ref(image: &str) -> anyhow::Result<()> {
    let image = image.trim();
    anyhow::ensure!(!image.is_empty(), "image reference is empty");
    anyhow::ensure!(
        !image.chars().any(char::is_control),
        "image reference contains control characters"
    );
    anyhow::ensure!(
        !image.starts_with('-'),
        "image reference may not begin with '-'"
    );
    let mut parts = image.splitn(3, '@');
    let _reference = parts.next().unwrap_or_default();
    if let Some(digest) = parts.next() {
        anyhow::ensure!(
            parts.next().is_none(),
            "image reference contains more than one '@'; credential-bearing image references are rejected"
        );
        let valid_digest = digest.split_once(':').is_some_and(|(algo, hex)| {
            !algo.is_empty()
                && algo
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'.' | b'-'))
                && hex.len() >= 32
                && hex.bytes().all(|b| b.is_ascii_hexdigit())
        });
        anyhow::ensure!(
            valid_digest,
            "image reference '@' suffix is not a valid content digest; credential-bearing image references are rejected"
        );
    }
    Ok(())
}

/// Reject a credential-bearing or structurally-unsafe deploy source before it
/// can reach a `Build` row, a webhook payload, a log line, or process argv.
/// Callers MUST run this before `cloud.builds.insert` — the two insertion
/// sites in this file (`start_build`, `redeploy_on_host`) both call it first.
fn validate_deploy_source(req: &GitDeployRequest) -> anyhow::Result<()> {
    repository_identity(&req.repo_url).context("repository URL")?;
    if let Some(head) = req.head_repo_url.as_deref().filter(|s| !s.is_empty()) {
        repository_identity(head).context("fork repository URL")?;
    }
    if let Some(image) = req.image_ref.as_deref() {
        reject_credential_bearing_image_ref(image).context("image reference")?;
    }
    Ok(())
}

fn resolve_build_trust(
    cloud: &Arc<CloudState>,
    req: &GitDeployRequest,
    project: &str,
    incarnation: ProjectIncarnation,
) -> anyhow::Result<BuildTrustContext> {
    let canonical_repo = repository_identity(&req.repo_url)?;
    let actual_repo = match req.head_repo_url.as_deref().map(str::trim) {
        Some(url) if !url.is_empty() => repository_identity(url)?,
        _ => canonical_repo.clone(),
    };
    let source_is_fork = !same_repository(&actual_repo, &canonical_repo);
    let requested_target = req.target.as_deref().map(str::trim);
    anyhow::ensure!(
        !(source_is_fork && requested_target == Some("production")),
        "a fork source cannot deploy directly to production; build it as a protected preview first"
    );

    let branch = req.branch.as_deref().unwrap_or("").trim();
    let production_branch = cloud
        .projects
        .get_exact(project, incarnation)?
        .production_branch;
    if requested_target == Some("production") && !production_branch.is_empty() && !branch.is_empty()
    {
        anyhow::ensure!(
            branch == production_branch,
            "production target contradicts the server-owned production branch: branch {branch:?} is not {production_branch:?}"
        );
    }

    let lane = if source_is_fork {
        BuildTrustLane::PreviewFork
    } else {
        match requested_target {
            Some("preview") => BuildTrustLane::PreviewBase,
            Some("production") => BuildTrustLane::Production,
            _ if !production_branch.is_empty() && !branch.is_empty() => {
                if branch == production_branch {
                    BuildTrustLane::Production
                } else {
                    BuildTrustLane::PreviewBase
                }
            }
            // First source deploy establishes its branch as production. A
            // branch-less request for an existing project cannot prove that it
            // targets production and therefore fails toward preview.
            _ if production_branch.is_empty() => BuildTrustLane::Production,
            _ => BuildTrustLane::PreviewBase,
        }
    };

    Ok(BuildTrustContext {
        lane,
        canonical_repo,
        actual_repo,
    })
}

/// Extract the only Marketplace policy input Hive is allowed to consume:
/// authoritative node registry identifiers. Policy retrieval, Clerk
/// authentication, tenant derivation, and schema validation happen in DevHub's
/// server-only route; Hive must neither accept identity material nor refetch.
///
/// This defensive re-check protects CLI/internal paths from treating an
/// incomplete Marketplace marker as an ordinary deployment. It intentionally
/// does not relax to a local-node fallback: an absent eligible approved node is
/// a placement refusal.
fn marketplace_approved_nodes(
    req: &GitDeployRequest,
) -> anyhow::Result<Option<std::collections::HashSet<String>>> {
    let Some(snapshot) = req.marketplace_placement.as_ref() else {
        return Ok(None);
    };
    anyhow::ensure!(
        snapshot.contract_version == 1 && snapshot.policy_version > 0,
        "MARKETPLACE_POLICY_INVALID: unsupported Marketplace policy version"
    );
    anyhow::ensure!(
        !snapshot.marketplace_order_id.trim().is_empty()
            && !snapshot.buyer_tenant_id.trim().is_empty(),
        "MARKETPLACE_POLICY_INVALID: Marketplace order and buyer tenant are required"
    );
    anyhow::ensure!(
        snapshot.policy.is_object(),
        "MARKETPLACE_POLICY_INVALID: Marketplace policy snapshot is malformed"
    );
    let approved: std::collections::HashSet<String> = snapshot
        .approved_node_ids
        .iter()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
        .cloned()
        .collect();
    anyhow::ensure!(
        !approved.is_empty() && approved.len() == snapshot.approved_node_ids.len(),
        "MARKETPLACE_POLICY_INVALID: approved_node_ids must be a non-empty unique list of node ids"
    );
    Ok(Some(approved))
}

async fn run_build(
    cloud: &Arc<CloudState>,
    bid: &str,
    mut req: GitDeployRequest,
    project: String,
    incarnation: ProjectIncarnation,
    first_deploy: bool,
) -> anyhow::Result<()> {
    cloud.projects.get_exact(&project, incarnation)?;
    // A Marketplace policy is a deployment-scoped, immutable authorization
    // snapshot. Do not refetch it here: the Clerk JWT belongs exclusively to
    // DevHub's server-side consumer. This process uses only the validated,
    // safe node-id allowlist copied into the request before the build began.
    let marketplace_approved_nodes = marketplace_approved_nodes(&req)?;
    let region = &cloud.region;
    let region_label = region_label(region);
    let log = |s: String| cloud.builds.log(bid, s);

    // This is the first operation in the build driver. It runs before fanout,
    // source acquisition, Git metadata reads, ignoreCommand, install, build,
    // warmup, or cache access, and is recomputed independently on every target.
    // The request's target is only an assertion checked against server state;
    // fork provenance and the stored production branch remain authoritative.
    let trust = resolve_build_trust(cloud, &req, &project, incarnation)?;
    req.target = Some(trust.lane.environment().to_string());
    if trust.lane.is_production() {
        let branch = req.branch.as_deref().unwrap_or("").trim();
        if cloud
            .projects
            .get_exact(&project, incarnation)?
            .production_branch
            .is_empty()
            && !branch.is_empty()
        {
            cloud
                .projects
                .set_production_branch_exact(&project, incarnation, branch)?;
            log(format!("Production branch set to '{branch}'."));
        }
    }

    // Persist env supplied by a DIRECT deploy as runtime-only and only for the
    // selected environment. A fanout sub-build receives an already-filtered
    // ephemeral runtime map from its coordinator; writing that map back would
    // widen its provenance. A fork request is never allowed to mutate durable
    // project env, even when it landed on the coordinator directly.
    if let Some(env) = &req.env {
        if !req.no_fanout && !trust.lane.is_fork() {
            for (k, v) in env {
                if k.trim().is_empty() {
                    continue;
                }
                cloud.projects.put_env_exact(
                    &project,
                    incarnation,
                    crate::project_settings::EnvVar {
                        key: k.trim().to_string(),
                        value: v.clone(),
                        target: trust.lane.environment().into(),
                        scope: "runtime".into(),
                        sensitive: false,
                        updated_ms: 0,
                    },
                )?;
            }
            if !env.is_empty() {
                log(format!(
                    "Set {} runtime environment variable(s) for {}.",
                    env.iter().filter(|(k, _)| !k.trim().is_empty()).count(),
                    trust.lane.environment(),
                ));
                crate::persist::persist(cloud);
            }
        } else if trust.lane.is_fork() && !env.is_empty() {
            log("Fork preview: request environment was not persisted.".into());
        }
    }

    // On a fanout deploy, adopt the coordinator's forwarded BuildConfig +
    // FunctionSettings so this target builds with the user's configured framework/
    // commands + compute tier (not just auto-detect). Direct user deploys omit
    // these (the coordinator reads its own store).
    if let Some(value) = req.build_config.as_ref() {
        let build_config = serde_json::from_value::<crate::project_settings::BuildConfig>(
            value.clone(),
        )
        .map_err(|error| {
            fluid_build::BuildContractError::new(
                fluid_build::BuildContractErrorCode::InvalidForwardedSettings,
                "adopt forwarded BuildConfig",
                error.to_string(),
            )
        })?;
        cloud
            .projects
            .set_build_exact(&project, incarnation, build_config)?;
    }
    if let Some(value) = req.function_settings.as_ref() {
        let function_settings =
            serde_json::from_value::<crate::project_settings::FunctionSettings>(value.clone())
                .map_err(|error| {
                    fluid_build::BuildContractError::new(
                        fluid_build::BuildContractErrorCode::InvalidForwardedSettings,
                        "adopt forwarded FunctionSettings",
                        error.to_string(),
                    )
                })?;
        cloud
            .projects
            .set_functions_exact(&project, incarnation, function_settings)?;
    }

    // ---- Placement scheduler / fanout (coordinator only) -------------------
    // Unless this is already a per-target deploy (`no_fanout`), decide WHERE this
    // project should be HOSTED from its configured regions + live mesh state, and
    // place it there rather than always building on this (the coordinator) node —
    // which is the resource-poor local Mac. See `schedule::place`.
    if !req.no_fanout {
        let placement_settings = cloud.projects.get_exact(&project, incarnation)?;
        let regions = placement_settings.functions.regions.clone();
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
        let needs_gpu = placement_settings.functions.gpu;
        // Pre-build placement, so there is no manifest yet — the only Wasmer
        // signal available this early is an explicit runtime in Project
        // Settings. A project that declares `runtime: "wasmer"` ONLY in a
        // vercel.json inside the repo is not knowable until after checkout, and
        // is caught by the post-build fanout gate and the cold-start refusal
        // instead. Partial knowledge used honestly: it can only ever REMOVE
        // incapable nodes, never add one.
        let known_wasm = hive_core::Runtime::from_config_str(&placement_settings.build.runtime)
            == Some(hive_core::Runtime::Wasmer);
        let known_bun = hive_core::Runtime::from_config_str(&placement_settings.build.runtime)
            == Some(hive_core::Runtime::Bun);
        let needs_build_isolation = !known_container;
        let targets = crate::schedule::place_for_project(
            cloud,
            &project,
            &regions,
            known_container,
            known_container,
            needs_gpu,
            crate::schedule::InterpreterNeeds {
                wasm: known_wasm,
                bun: known_bun,
            },
            !known_container,
            needs_build_isolation,
            marketplace_approved_nodes.as_ref(),
        );
        let build_isolation_nodes = cloud
            .registry
            .nodes()
            .into_iter()
            .filter(|n| n.build_isolation_protocol == Some(1))
            .count();
        if needs_build_isolation && targets.is_empty() && build_isolation_nodes == 0 {
            // Refuse EARLY only when project settings already prove a
            // repository-controlled command will run (an explicit
            // install/build command). Everything else proceeds to checkout
            // and planning — platform-only work that executes no repository
            // code — and the command chokepoints below refuse with this same
            // message the moment a plan would actually execute anything.
            // That keeps a pure-static zero-command repo deployable on a
            // builder-less fleet without weakening the isolation boundary.
            let explicit_commands = !placement_settings.build.install_command.trim().is_empty()
                || !placement_settings.build.build_command.trim().is_empty();
            if explicit_commands {
                let msg = "BUILD_ISOLATION_UNAVAILABLE: this source deployment requires an isolated build executor, but no node advertises build-isolation protocol v1. No repository-controlled command was run on the host.".to_string();
                log(msg.clone());
                tracing::warn!(project = %project, "deploy refused: no isolated builder capability");
                return Err(anyhow::anyhow!(msg));
            }
            log("No isolated builder capability on this fleet — only zero-command static deploys can succeed; any repository command will be refused.".into());
        }
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
        if req.marketplace_placement.is_some() && targets.is_empty() {
            let msg = "MARKETPLACE_PLACEMENT_UNAVAILABLE: no approved Marketplace node is currently healthy, reachable, and capable. The deployment was not placed outside buyer-authorized nodes.".to_string();
            log(msg.clone());
            tracing::warn!(project = %project, "deploy refused: no eligible Marketplace-approved node");
            return Err(anyhow::anyhow!(msg));
        }
        // Same refusal for the wasm runtime, and for the identical reason the GPU
        // arm above exists. An empty `targets` otherwise means "host locally",
        // which is the right default for an ordinary deployment but puts a
        // Wasmer function on a node with no `wasmer` binary — every cold start
        // then fails NODE_RUNTIME_MISSING forever. `schedule::wasm_capable` is a
        // HARD filter precisely so this case is reachable; without a loud
        // refusal here the filter just produced an empty set that the caller
        // quietly ignored, and the capability gate's own doc comment promised a
        // guarantee the code did not keep.
        if known_wasm && targets.is_empty() {
            let capable_nodes = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| n.wasm_runtime == Some(true))
                .count();
            let msg = format!(
                "{} {} {}",
                format_args!(
                    "this project declares runtime \"wasmer\", but no healthy node currently advertises a wasmer runtime ({capable_nodes} wasm-capable node(s) known to the mesh)."
                ),
                "The deploy was NOT placed on a node without it, because every cold start there would fail with NODE_RUNTIME_MISSING.",
                "Bake wasmer into a node's guest image (hive_wasmer_in_rootfs) or install the wasmer CLI on a mock/litebox node, then redeploy."
            );
            log(msg.clone());
            tracing::warn!(project = %project, capable_nodes, "deploy refused: wasmer requested, no wasm-capable target");
            return Err(anyhow::anyhow!(msg));
        }
        if needs_build_isolation && targets.is_empty() {
            // Same deferred-refusal rule as the fleet-wide gate above: only an
            // explicit command setting proves a repository-controlled command
            // will run. Everything else proceeds locally — the command
            // chokepoints (`require_build_session`) refuse the moment the
            // resolved plan would actually execute anything, so a
            // zero-command static repo still deploys on a builder-less node.
            let explicit_commands = !placement_settings.build.install_command.trim().is_empty()
                || !placement_settings.build.build_command.trim().is_empty();
            if explicit_commands {
                let msg = format!(
                    "BUILD_ISOLATION_UNAVAILABLE: this source deployment requires an isolated build executor, but no healthy reachable placement satisfying the request advertises build-isolation protocol v1 ({build_isolation_nodes} capable node(s) known to the mesh). No repository-controlled command was run on the host."
                );
                log(msg.clone());
                tracing::warn!(project = %project, build_isolation_nodes, "deploy refused: isolated builder unavailable");
                return Err(anyhow::anyhow!(msg));
            }
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
                let mut fs = cloud.projects.get_exact(&project, incarnation)?.functions;
                fs.regions = placed.clone();
                cloud
                    .projects
                    .set_functions_exact(&project, incarnation, fs)?;
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
            let ok = fanout_remote(cloud, bid, &req, &project, incarnation, &remote, true).await;
            // Atomic promotion (Vercel convention): only relocate — i.e. remove the
            // project from nodes that still host the PREVIOUS deployment — once the
            // new placement actually built & is serving. A FAILED build must never
            // take down the currently-serving deployment; the old one keeps serving
            // and the user just sees a failed build. (This is the bug that dropped a
            // healthy project when a relocating redeploy errored.)
            // PROMOTION POLICY (see `FanoutOutcome::promotable`): at least one
            // target genuinely Ready promotes the deployment. A target we could
            // never REACH degrades capacity; it does not veto the regions that
            // are demonstrably serving. Before this, every outcome folded into
            // one bool, so an unreachable node failed the whole deploy even
            // when two regions had built and gone live.
            let promotable = ok.promotable();
            let unreachable = ok.unreachable();
            let cancelled = (!promotable && ok.cancelled())
                || (!promotable && cloud.build_cancels.is_cancelled(bid));
            // Honest wording. "Keeping the existing deployment in place" is a
            // claim that something is still serving, and it is FALSE for a
            // project's first-ever deploy attempt — there is nothing to keep.
            // Witnessed live: internet-structure failed every attempt (0
            // deployment records, ever) and every failure log line still said
            // "keeping the existing deployment in place", which reads as "your
            // site is fine" to an operator watching a genuinely-down project.
            let had_prior_deployment =
                cloud.gw.deployment_records().iter().any(|r| {
                    r.project.eq_ignore_ascii_case(&project) && r.state == DeployState::Ready
                }) || cloud.peer_deployments.read().values().flatten().any(|d| {
                    d.project.eq_ignore_ascii_case(&project) && d.state == DeployState::Ready
                });
            if promotable {
                // DEGRADED, not failed: name every target the deploy could not
                // be delivered to, so the operator sees reduced replication
                // instead of silence, and so a later reconcile can repair it.
                if !unreachable.is_empty() {
                    log(format!(
                        "⚠ Deployed to {} of {} target(s). Could not reach: {} — those regions ran \
                         nothing and are DEGRADED, not failed; replication is repaired when they \
                         return. Serving continues from the healthy region(s).",
                        ok.ready(),
                        ok.per_target.len(),
                        unreachable.join(", ")
                    ));
                }
                // OFF the critical path: the new placement is provably serving
                // at this point, so the user's build must not stay "Building"
                // while stale copies are reaped from unrelated peers. Every
                // delete is idempotent and independently retried, and nothing
                // below reads its result.
                //
                // Unreachable targets are EXCLUDED from the relocation set's
                // "keep" list only in the sense that they were never reached —
                // `names` still lists every intended target, so a node that is
                // simply unreachable right now is NOT reaped as a stale copy.
                // Relocation reaping is PRODUCTION-lane semantics: only a
                // production build defines a new authoritative placement whose
                // non-targets hold "stale copies". A preview build must never
                // reap anything (its placement says nothing about where
                // production lives), and an UNRESOLVABLE environment (no
                // explicit target, production_branch unknown here) skips the
                // reap — stale copies linger until the next classified
                // production build, which is retention, never correctness.
                if request_is_production(cloud, &req, &project) == Some(true) {
                    let c2 = cloud.clone();
                    let p2 = project.clone();
                    let n2 = names.clone();
                    tokio::spawn(async move {
                        cleanup_non_targets(&c2, &p2, &n2).await;
                    });
                }
            } else if cancelled {
                log(if had_prior_deployment {
                    "Build cancelled by user — keeping the existing deployment in place.".into()
                } else {
                    "Build cancelled by user.".to_string()
                });
            } else if ok.build_failed() == 0 && !unreachable.is_empty() {
                // NOTHING ran anywhere: every target was unreachable. Still a
                // failed deploy (there is no new version serving), but it must
                // NOT be reported as a build failure — the application was
                // never executed, so nothing is known about whether it builds.
                // Saying "Build failed" here sent users to debug an app that
                // was never run.
                log(format!(
                    "✗ Could not reach any target to run this deploy ({}). Nothing was built, so {} \
                     This is a fleet-reachability fault, not a build failure — retry when the \
                     node(s) are back.",
                    unreachable.join(", "),
                    if had_prior_deployment {
                        "the existing deployment is untouched and still serving."
                    } else {
                        "this project still has nothing serving."
                    }
                ));
            } else if ok.build_failed() == 0 && ok.declined() > 0 {
                // Every reachable target deliberately declined to host (stateful
                // single-writer guard) and none failed. Nothing is serving the new
                // version, but no application fault occurred — reporting "Build
                // failed" here would send the user to debug an app that built fine.
                log(format!(
                    "✗ No target hosted this deploy: {} target(s) declined as a stateful \
                     single-writer service. Nothing was built incorrectly — this is a placement \
                     outcome, not a build failure.",
                    ok.declined()
                ));
            } else {
                log(if had_prior_deployment {
                    "Build failed — keeping the existing deployment in place (no relocation)."
                        .to_string()
                } else {
                    "Build failed — this project has no prior successful deployment, so nothing \
                     is currently serving."
                        .to_string()
                });
            }
            cloud.builds.update(bid, |b| {
                b.state = if promotable {
                    DeployState::Ready
                } else if cancelled {
                    DeployState::Cancelled
                } else {
                    DeployState::Error
                };
                b.finished_ms = Some(now_ms());
            });
            crate::persist::persist(cloud);
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
    // (No artificial pause here. A fixed 350ms sleep used to sit between these
    // two cosmetic log lines and the real work; nothing synchronizes on it —
    // the checkout-collision it was once entangled with is solved by the
    // build-id-suffixed dir names below.)

    // Acquire the source: extract an uploaded ZIP through a descriptor-relative
    // no-follow bounded importer, or `git clone` a repo.
    //
    // The checkout dir carries the BUILD ID, not just the millisecond stamp:
    // two concurrent builds of the same project used to collide on
    // `<project>-<now_ms()>` (both wake from the synchronized 350ms sleep above
    // on the same timer tick), extracting + installing into ONE shared dir —
    // two racing extraction processes then kill one build with "cannot create
    // …: No such file or directory" (exit 50) and the loser never reaches
    // ready (witnessed live 3x). The project component is `sanitize_tag`'d: a
    // tenant-controlled name is never a path component verbatim.
    let stamp = now_ms();
    let dir = deploy_root().join(format!("{}-{}-{}", checkout_tag(&project), stamp, bid));
    // Register before the path can become visible to the reaper. Drop releases
    // it on every return, panic, task abort, and client cancellation; successful
    // builds have already registered their deployment root before that release.
    let _active_checkout = ActiveCheckoutGuard::new(dir.clone(), &project, incarnation);
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
        // Retain the ORIGINAL archive under this build's immutable lineage id so
        // a later Redeploy can rebuild exactly the selected production/preview
        // source. A project-wide archive let a newer preview overwrite the bytes
        // a production redeploy would consume. Stored as a FILE beside checkouts —
        // `gc_build_dirs` only reaps directories, so it survives build-dir GC and
        // host reboots. Written via tmp+rename so readers never observe a torn zip.
        let retained = retained_source_write_path(&project, bid);
        let retained_tmp = deploy_root().join(format!(
            "{}.{}.tmp",
            retained
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            bid
        ));
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
            bid.to_string(),
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
        // carries no fresh archive. Rebuild only from source carrying the selected
        // deployment's immutable build-lineage id: its per-build retained archive,
        // else that exact prior checkout. Legacy records with no lineage fail closed
        // and ask for re-upload rather than guessing across production/preview lanes.
        let name = req.repo_url.trim_start_matches("upload://").to_string();
        let mut source_build_ids = req
            .source_deployment_ids
            .iter()
            .filter(|id| valid_source_build_id(id))
            .cloned()
            .collect::<Vec<_>>();
        source_build_ids.sort();
        source_build_ids.dedup();
        anyhow::ensure!(
            source_build_ids.len() == 1,
            "uploaded source has no unique build-lineage id — re-upload the archive to deploy again"
        );
        let source_build_id = source_build_ids.remove(0);
        let retained =
            retained_source_path_for_ids(&project, std::slice::from_ref(&source_build_id));
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
        } else if let Some((src, _source_checkout)) = acquire_deploy_dir_for_ids(
            &project,
            incarnation,
            std::slice::from_ref(&source_build_id),
        ) {
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
            source_build_id,
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
        // `GITHUB_TOKEN`. Fed to the git process ONLY through `credential_feed`'s
        // fixed FD-backed helper — never embedded in a clone URL — so
        // `clone_source_url`/`req.repo_url` stay tokenless everywhere: argv,
        // `.git/config`, logs, the Build row, webhooks, and fanout. Applied when
        // cloning `clone_source_url` (the fork, when this is a fork PR): the same
        // installation/token resolution as the base repo is reused verbatim (no new
        // auth flow) — it may not grant access to an arbitrary fork, but a public
        // fork still clones fine anonymously, and a rejected token falls back to the
        // anonymous retry below exactly as it does for the base repo today.
        let git_token: Option<String> = req
            .git_token
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()))
            .filter(|_| clone_source_url.starts_with("https://github.com/"));

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
        let run_clone = |use_token: bool| {
            let (dir, branch, token, commit, ccloud, cbid, clone_url) = (
                dir.clone(),
                branch.clone(),
                if use_token { git_token.clone() } else { None },
                pinned_commit.clone(),
                cloud.clone(),
                bid.to_string(),
                clone_source_url.clone(),
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
                        init.arg("-c").arg("credential.helper=");
                        init.env("GIT_TERMINAL_PROMPT", "0")
                            .env("GIT_ASKPASS", "/bin/echo");
                        init.arg("init").arg("-q").arg(&dir);
                        let init_ok = run_cancellable_output(&mut init, &ccloud, &cbid)
                            .await
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if init_ok {
                            let mut remote = Command::new("git");
                            remote.arg("-c").arg("credential.helper=");
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
                            let _cred = match apply_credential(&mut fetch, token.as_deref()) {
                                Ok(cred) => cred,
                                Err(e) => return (false, format!("credential setup failed: {e}")),
                            };
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
                                        .arg("-c")
                                        .arg("credential.helper=")
                                        .env("GIT_TERMINAL_PROMPT", "0")
                                        .env("GIT_ASKPASS", "/bin/echo")
                                        .arg("-C")
                                        .arg(&dir)
                                        .arg("checkout")
                                        .arg("-q")
                                        .arg("FETCH_HEAD");
                                    match run_cancellable_output(&mut checkout, &ccloud, &cbid)
                                        .await
                                    {
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
                    let _cred = match apply_credential(&mut cmd, token.as_deref()) {
                        Ok(cred) => cred,
                        Err(e) => return (false, format!("credential setup failed: {e}")),
                    };
                    cmd.arg("clone");
                    if !branch.is_empty() {
                        cmd.arg("--branch").arg(&branch);
                    }
                    cmd.arg(&clone_url).arg(&dir);
                    match run_cancellable_output(&mut cmd, &ccloud, &cbid).await {
                        Ok(out) if out.status.success() => {
                            let mut checkout = Command::new("git");
                            checkout
                                .arg("-c")
                                .arg("credential.helper=")
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
                    let _cred = match apply_credential(&mut cmd, token.as_deref()) {
                        Ok(cred) => cred,
                        Err(e) => return (false, format!("credential setup failed: {e}")),
                    };
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

        let used_token = git_token.is_some();
        let (mut ok, mut stderr) = run_clone(used_token).await;
        // A stored token that is expired / lacks access must never break a repo that
        // would clone fine anonymously (public repos, token rotation): retry once
        // without the credential before surfacing an error.
        if !ok && used_token && auth_failed(&stderr) {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            log("Retrying clone without the stored GitHub credential…".into());
            let (ok2, stderr2) = run_clone(false).await;
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
        // `.git/config`'s origin URL was ALWAYS `clone_source_url` (tokenless) —
        // the credential only ever lived in the FD-backed helper, never in a
        // clone URL — so there is nothing to scrub here anymore.
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

    // Verify the pre-command trust decision against the branch Git resolved. A
    // branch-less first deploy can only learn its default branch after clone; it
    // may establish that branch, but it may not silently change a pre-existing
    // production branch.
    let mut prod_branch = cloud
        .projects
        .get_exact(&project, incarnation)?
        .production_branch;
    if trust.lane.is_production() && prod_branch.is_empty() && !actual_branch.is_empty() {
        prod_branch = actual_branch.clone();
        cloud
            .projects
            .set_production_branch_exact(&project, incarnation, &prod_branch)?;
        log(format!("Production branch set to '{prod_branch}'."));
    }
    anyhow::ensure!(
        !trust.lane.is_production()
            || prod_branch.is_empty()
            || actual_branch.is_empty()
            || actual_branch == prod_branch,
        "resolved branch {actual_branch:?} contradicts the server-owned production branch {prod_branch:?}"
    );
    let is_production = trust.lane.is_production();
    let allow_all_environment = !trust.lane.is_fork();
    let build_env = cloud.projects.env_map_for_execution_exact(
        &project,
        incarnation,
        trust.lane.environment(),
        crate::project_settings::EnvExecutionScope::Build,
        allow_all_environment,
    )?;
    let stored_runtime_env = cloud.projects.env_map_for_execution_exact(
        &project,
        incarnation,
        trust.lane.environment(),
        crate::project_settings::EnvExecutionScope::Runtime,
        allow_all_environment,
    )?;
    let runtime_env = if req.no_fanout {
        // Coordinator-filtered, ephemeral runtime values. Build variables are
        // always re-selected locally by explicit environment + build scope and
        // can never hitchhike in this compatibility map.
        req.env.clone().unwrap_or(stored_runtime_env)
    } else {
        stored_runtime_env
    };
    if trust.lane.is_fork() {
        log(format!(
            "Fork preview: only explicitly preview-scoped variables are eligible ({} build, {} runtime); all-environment and production values are withheld.",
            build_env.len(),
            runtime_env.len(),
        ));
    }
    let build_cache_enabled = req.use_cache;

    // Build from a subdirectory for monorepo templates (e.g. `examples/nextjs`).
    // This is the one chokepoint for request, persisted, webhook, redeploy and
    // fanout roots: old stored rows are untrusted input too, not a reason to
    // bypass the checkout boundary.
    let persisted_root = cloud
        .projects
        .get_exact(&project, incarnation)?
        .build
        .root_dir;
    let effective_root = req
        .root_dir
        .as_deref()
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(str::to_string)
        .or_else(|| {
            Some(persisted_root.trim())
                .filter(|root| !root.is_empty())
                .map(str::to_string)
        });
    let checkout_dir = resolve_checkout_dir(&dir, None).await?;
    let (build_dir, workspace_member) = if let Some(root) = effective_root.as_deref() {
        log(format!("Root directory: {root}"));
        let selected = resolve_checkout_dir(&checkout_dir, Some(root)).await?;
        let member = if selected == checkout_dir {
            false
        } else {
            crate::app_discovery::is_member(&checkout_dir, &selected).await?
        };
        (selected, member)
    } else if req.image_ref.is_some() {
        (checkout_dir.clone(), false)
    } else if let Some(selection) = crate::app_discovery::select(&checkout_dir).await? {
        let relative = if selection.relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            selection.relative.to_string_lossy().replace('\\', "/")
        };
        log(format!(
            "Auto-detected workspace app: {relative} ({}; evidence: {}; decision: {}).",
            selection.workspace_source,
            selection.evidence.join(", "),
            selection.decision_digest,
        ));
        let selected = if selection.relative.as_os_str().is_empty() {
            checkout_dir.clone()
        } else {
            resolve_checkout_dir(&checkout_dir, Some(&relative)).await?
        };
        (selected, !selection.relative.as_os_str().is_empty())
    } else {
        (checkout_dir.clone(), false)
    };
    let vercel_config = fluid_build::load_vercel_config_checked(&build_dir).map_err(|error| {
        fluid_build::BuildContractError::invalid_metadata("load selected-app vercel.json", error)
    })?;
    // Package/workspace metadata and outputDirectory are interpreted only after
    // application selection, and before the executor receives the checkout.
    let mut fdi_preparation = if req.image_ref.is_none()
        && tokio::fs::read_to_string(build_dir.join("fluid.json"))
            .await
            .is_err()
    {
        Some(
            prepare_fdi(
                cloud,
                &checkout_dir,
                &build_dir,
                &project,
                vercel_config.as_ref(),
                workspace_member,
            )
            .await?,
        )
    } else {
        None
    };
    if fdi_preparation.is_none() {
        if let Some(output) = vercel_config
            .as_ref()
            .and_then(|config| config.output_directory.as_deref())
            .filter(|output| !output.trim().is_empty())
        {
            fluid_build::OutputDirectory::parse(output)?;
        }
    }
    let mut isolated = if req.image_ref.is_none() {
        // Capability-tolerant: a node with NO build executor installed gets
        // `None` (only zero-command static deploys can proceed — every
        // command chokepoint refuses on None). A node whose executor EXISTS
        // but fails to begin still errors loudly: that is a broken builder,
        // not a missing capability, and silently downgrading it to
        // static-only semantics would mask the fault.
        match crate::build_executor::get() {
            Ok(_) => Some(IsolatedBuild::begin(&checkout_dir).await?),
            Err(error) => {
                log(format!(
                    "No isolated build executor on this node ({error}); repository commands are disabled for this build — only a zero-command static plan can succeed."
                ));
                None
            }
        }
    } else {
        None
    };

    if let Some(warning) = fdi_preparation
        .as_ref()
        .and_then(|preparation| preparation.package_manager.conflict_warning.as_ref())
    {
        log(format!("WARN: {warning}"));
    }

    // ---- Ignored Build Step (vercel.json `ignoreCommand`) ----
    // Vercel semantics: run the command in the project root; exit 0 => skip this
    // build entirely (no new deployment — the prior one keeps serving), non-zero
    // => continue. Lets a repo short-circuit commits that don't need a rebuild.
    if let Some(vc) = vercel_config.as_ref() {
        if let Some(cmd) = vc
            .ignore_command
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            log(format!("Running Ignored Build Step: {cmd}"));
            match run_ignored_command(
                isolated.as_mut().ok_or_else(|| {
                    anyhow::anyhow!(
                        "BUILD_ISOLATION_UNAVAILABLE: vercel.json ignoreCommand is a repository-controlled command and requires an isolated build executor. No repository-controlled command was run on the host."
                    )
                })?,
                &build_dir,
                &cmd,
                cloud,
                bid,
                &build_env,
            )
            .await
            {
                Ok(true) => {
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
                Ok(false) => log("Ignored Build Step exited non-zero — continuing build.".into()),
                Err(error) => return Err(error),
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
        isolated.as_mut(),
        &checkout_dir,
        &build_dir,
        &project,
        incarnation,
        &commit,
        first_deploy,
        build_cache_enabled,
        &build_env,
        &trust,
        vercel_config.as_ref(),
        fdi_preparation.take(),
        workspace_member,
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
        Err(error) if error.downcast_ref::<BuildCancelled>().is_some() => return Err(error),
        Err(error)
            if error
                .downcast_ref::<fluid_build::BuildContractError>()
                .is_some() =>
        {
            return Err(error);
        }
        Err(error) if build_executor_platform_fault(&error) => return Err(error),
        Err(e) => {
            build_failed = true;
            log(format!("Build failed: {e}"));
            log(
                "Keeping the deployment — serving a build-status page so the project still exists."
                    .into(),
            );
            drop(isolated.take());
            tokio::fs::remove_dir_all(&build_dir)
                .await
                .context("clearing failed build output")?;
            tokio::fs::create_dir(&build_dir)
                .await
                .context("creating failed build output")?;
            tokio::fs::write(
                build_dir.join("index.html"),
                build_failed_page(&project, &commit, &e.to_string(), &req.repo_url),
            )
            .await
            .context("writing failed build page")?;
            static_manifest(&project, ".")
        }
    };
    manifest.project = project.clone();

    // Build Output carries its own compiled feature contract. Never let a second
    // framework heuristic pass overwrite it; vercel.json's explicit overlay below
    // remains the only higher-precedence source.
    if !build_failed && !fluid_build::has_build_output(&build_dir) {
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
        let current = cloud
            .projects
            .get_exact(&manifest.project, incarnation)?
            .inference;
        if current != spec {
            if let Some(s) = &spec {
                log(format!(
                    "Managed inference requested: model {} (pool: {}).",
                    s.model, s.pool
                ));
            } else if current.is_some() {
                log("Managed inference removed (no inference block in fluid.json).".into());
            }
            cloud
                .projects
                .set_inference_exact(&manifest.project, incarnation, spec)?;
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
        let current = cloud
            .projects
            .get_exact(&manifest.project, incarnation)?
            .browser_db;
        if manifest.browser_db.is_some() {
            if current != manifest.browser_db {
                cloud.projects.set_browser_db_exact(
                    &manifest.project,
                    incarnation,
                    manifest.browser_db.clone(),
                )?;
            }
        } else if let Some(settings_spec) = current {
            log(
                "Browser-replicated database: applying the dashboard-managed browser_db config (fluid.json declares none).".into(),
            );
            manifest.browser_db = Some(settings_spec);
        }
    }

    // Stamp the framework slug onto the manifest so the deployment record
    // (and the dashboard's project grid) carries the real framework for its
    // logo — the build already detected it into BuildConfig.framework
    // (auto-detect saves it there), empty stays empty (UI falls back).
    if manifest.framework.is_empty() {
        let bc = cloud.projects.get_exact(&project, incarnation)?.build;
        let fw = bc.framework.trim();
        if !fw.is_empty() {
            manifest.framework = fw.to_string();
        }
    }

    // Inject project env vars + function settings. FILTERED BY ENVIRONMENT:
    // the classification above must happen BEFORE this, because a preview
    // deployment launching with the project's production secrets is exactly
    // the isolation the dashboard's Production/Preview selector promises and
    // nothing enforced.
    let env = runtime_env;
    let project_settings = cloud.projects.get_exact(&manifest.project, incarnation)?;
    let dedicated_ipv4_alloc = project_settings.dedicated_ipv4;
    let fsettings = project_settings.functions;
    if !env.is_empty() {
        log(format!("Loaded {} environment variable(s).", env.len()));
    }
    for f in manifest.functions.iter_mut() {
        for (k, v) in &env {
            f.env.insert(k.clone(), v.clone());
        }
        // A fluid.json-declared `max_duration_secs` must WIN over the project-level
        // dashboard default — settings can only fill in what a function left
        // unset, never silently discard what it explicitly asked for (the exact
        // "settings can only turn GPU ON, never strip a function's own declared
        // need" precedent a few lines below). This was previously an unconditional
        // overwrite: every deploy silently replaced a real fluid.json
        // `max_duration_secs` (e.g. 60 for a bot that legitimately needs an LLM
        // round trip) with whatever the project's dashboard default happened to
        // be, with no way for the manifest's own declaration to survive even one
        // redeploy. `FunctionConfig::default().max_duration_secs` (300, serde's
        // own fallback when fluid.json omits the field) can't be distinguished
        // from a developer explicitly writing 300 — falling back to the project
        // setting in that one case is a no-op (300 already IS the default), so
        // every OTHER explicit value is preserved with no observable downside.
        if f.max_duration_secs == fluid_core::FunctionConfig::default().max_duration_secs {
            f.max_duration_secs = fsettings.default_max_duration_secs;
        }
        // Same rule, same reason, for the size knobs: a value the FUNCTION
        // declared must survive the project default. These two were left as
        // unconditional overwrites when `max_duration_secs` above was fixed,
        // so a fluid.json `{"memory_mib": 8192}` (or a container's
        // `{"container":{"memory":"8g"}}`, which lands here as `memory_mib`)
        // was silently replaced by the dashboard default on EVERY deploy. The
        // user-visible shape is the reported one: the setting has no effect,
        // and a container that genuinely needs the memory is OOM-killed
        // (exit 137) — which then presents as repeated cold-start failure and
        // an open circuit, blamed on the app.
        //
        // Distinguishing "explicitly wrote the default" from "omitted" is not
        // possible through serde here, and it does not matter: falling back to
        // the project setting when the function is AT the default is a no-op
        // whenever they agree, and honours the project default when they
        // differ — which is exactly the intent.
        if f.vcpus == fluid_core::FunctionConfig::default().vcpus {
            f.vcpus = fsettings.vcpus.max(1);
        }
        if f.memory_mib == fluid_core::FunctionConfig::default().memory_mib {
            f.memory_mib = fsettings.memory_mib;
        }
        f.vcpus = f.vcpus.max(1);
        // Per-plan resource CEILING (capacity policy): an enterprise function
        // is capped at 2 vCPU / 4 GiB. `plan_resource_ceiling` returns None for
        // every other plan, so a legacy/grandfathered tenant is untouched.
        // Applied AFTER the project-default fallback and the min-1 floor so it
        // is the last word on sizing — a fluid.json or project setting can ask
        // for more, but the plan caps it (and the deploy log says so).
        if let Some((max_vcpus, max_mem)) =
            crate::billing::plan_resource_ceiling(&crate::admin::team_plan(cloud, &project))
        {
            if f.vcpus > max_vcpus {
                log(format!(
                    "Function '{}' requested {} vCPU; capped to {} by the plan.",
                    f.name, f.vcpus, max_vcpus
                ));
                f.vcpus = max_vcpus;
            }
            if f.memory_mib > max_mem {
                log(format!(
                    "Function '{}' requested {} MiB; capped to {} MiB by the plan.",
                    f.name, f.memory_mib, max_mem
                ));
                f.memory_mib = max_mem;
            }
        }
        // Serverless GPU: the project-level toggle marks every function; a
        // per-function fluid.json `gpu: true` (already parsed into the manifest)
        // is preserved — settings can only turn GPU ON, never strip a
        // function's own declared need.
        f.gpu = f.gpu || fsettings.gpu;
        // Dedicated public IPv4 is a PAID add-on, not a free fluid.json
        // opt-in like GPU: the project setting is the ONLY source (an
        // assignment, not an OR) — a fluid.json author can no longer
        // self-grant the feature by writing `dedicatedIpv4: true` in their
        // own manifest. The setting itself is only ever flipped on by
        // `tencent_eip::provision_from_checkout` after a real purchase.
        f.dedicated_ipv4 = fsettings.dedicated_ipv4;
        // Stamp (or clear) the actual allocated address alongside the flag —
        // every redeploy re-adopts the SAME claim from `ProjectSettings`
        // rather than purchasing a new one (`Manifest::dedicated_ipv4_binding`
        // hoists whichever function carries this onto `DeploymentInfo`).
        f.dedicated_ipv4_alloc = if f.dedicated_ipv4 {
            dedicated_ipv4_alloc.clone()
        } else {
            None
        };
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
        if let Some(vc) = vercel_config.as_ref() {
            apply_vercel_config(&mut manifest, vc, &|s| log(s));
        }
    }

    log("Uploading build outputs…".into());
    log(format!(
        "Functions: {}, Static assets prepared.",
        manifest.functions.len()
    ));

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
    // Captured for the same reason and at the same point as `is_stateful`: the
    // multi-region fanout below needs it after `manifest` has moved. Resolved
    // through `Runtime::resolve` (config value wins, else argv basename) rather
    // than a bare string compare, so a function whose runtime was inferred from
    // a `wasmer …` start_cmd counts too.
    let is_wasm = manifest.functions.iter().any(|f| {
        hive_core::Runtime::resolve(&f.runtime, &f.start_cmd) == hive_core::Runtime::Wasmer
    });
    let is_bun = manifest
        .functions
        .iter()
        .any(|f| hive_core::Runtime::resolve(&f.runtime, &f.start_cmd) == hive_core::Runtime::Bun);

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
    // Build-time server warmups execute the repository's production entrypoint.
    // They are intentionally disabled until they have a dedicated executor API;
    // a performance hint can never justify escaping the mandatory isolation
    // boundary after install/build already ran inside it.
    if !build_failed && !is_container {
        log("Compile-cache warmup skipped under isolated builds.".into());
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
                if is_container {
                    "container"
                } else {
                    "framework-detected"
                },
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
            if !matches!(runtime, hive_core::Runtime::Node | hive_core::Runtime::Bun) {
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

    let _finalization = crate::project_settings::lifecycle_write(&project).await;
    let final_settings = cloud.projects.get_exact(&project, incarnation)?;
    if let Some(newer) = cloud.builds.get(bid).and_then(|build| {
        (build.project_incarnation == Some(incarnation))
            .then_some(build.superseded_by)
            .flatten()
    }) {
        return Err(anyhow::anyhow!(
            "build superseded by newer build {newer} for project {project}; skipping deployment registration"
        ));
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
            if let Err(e) =
                crate::browser_artifacts::persist(&bundled.source, &bundled.descriptor).await
            {
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

    // No repository process survives an isolated step. Re-seal once after every
    // platform-authored adaptation so the host static tree and the runtime
    // artifact are derived from the same bounded immutable snapshot. On a
    // builder-less node this normalization pass has no executor to run through
    // — and nothing repository-controlled ever ran (the command chokepoints
    // refuse without a session), so the checkout on disk IS the platform
    // truth; skip with a log rather than failing the zero-command static lane.
    if !build_failed && !is_container {
        match crate::build_executor::get() {
            Ok(_) => reseal_platform_output(&checkout_dir).await?,
            Err(error) => log(format!(
                "Skipping final reseal: no isolated build executor on this node ({error}); no repository-controlled command ran, the checkout is platform-authored as-is."
            )),
        }
    }

    // An isolated backend cannot read the host checkout directly, so derive the
    // host static root and guest function cwd together from one server-built
    // descriptor. The host path must remain the deployment's static root; only
    // the guest path enters the function pool.
    //
    // Containers serve from their OCI image and failed builds do not launch, so
    // neither needs the transitive runtime closure or artifact delivery.
    let artifact_relative = build_dir.strip_prefix(&checkout_dir).map_err(|_| {
        anyhow::anyhow!(
            "selected application {} is outside checkout {}",
            build_dir.display(),
            checkout_dir.display()
        )
    })?;
    let runtime_artifact = if !build_failed && !is_container {
        hive_backend::RuntimeArtifactSpec::with_includes(
            checkout_dir.clone(),
            artifact_relative.to_path_buf(),
            crate::app_discovery::runtime_include_rel(&checkout_dir, &build_dir).await?,
        )
    } else {
        hive_backend::RuntimeArtifactSpec::new(
            checkout_dir.clone(),
            artifact_relative.to_path_buf(),
        )
    };
    let runtime_paths = cloud.gw.runtime_artifact_paths(&runtime_artifact)?;
    let host_static_root = runtime_paths
        .host_static_root
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("validated host static root is not UTF-8"))?;
    let runtime_workdir = runtime_paths
        .guest_workdir
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("validated runtime workdir is not UTF-8"))?;
    if !build_failed && !is_container && runtime_paths.delivery_required {
        let image = format!("dpl-{}", sanitize_tag(bid));
        match cloud.gw.deliver_build(&image, &runtime_artifact).await {
            Ok(()) => {
                log(format!(
                    "Delivered closed runtime artifact to an isolated-cell image ({}; cwd {}).",
                    cloud.gw.backend_name(),
                    runtime_workdir
                ));
                manifest.image = Some(image);
            }
            // Loud, and it FAILS the build. This used to be a WARN that let the
            // deployment register as Ready with nothing delivered — on a backend
            // that hard-requires the artifact, every cold start then failed with
            // a message about a missing tar, long after the build said success.
            // A deployment that cannot possibly start is a build failure.
            Err(e) => {
                let msg = format!(
                    "could not deliver the build to an isolated cell image on backend {}: {e}",
                    cloud.gw.backend_name()
                );
                log(format!("ERROR: {msg}"));
                return Err(anyhow::anyhow!(msg));
            }
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
        Ok(ports) if !ports.is_empty() => {
            log(format!(
                "Allocated public raw port(s): {} (stable across redeploys).",
                ports
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            // A compose publish request asked for a LITERAL host port. Name the
            // outcome for every such spec — grant == request is confirmation,
            // grant != request is the loud never-silent fallback the compose
            // author needs to see (their :9000 is somewhere else).
            for f in &manifest.functions {
                for spec in &f.ports {
                    let (Some(want), Some(got)) = (spec.preferred_public_port, spec.public_port)
                    else {
                        continue;
                    };
                    if want == got {
                        log(format!(
                            "✓ '{}' port {}: published on :{got} as requested — reach it at \
                             <host>:{got} (plain TCP passthrough; HTTPS stays on 443).",
                            f.name, spec.container_port
                        ));
                    } else {
                        log(format!(
                            "⚠ '{}' port {}: requested public port :{want} is reserved or \
                             already taken fleet-wide — published on :{got} INSTEAD. Update \
                             clients to <host>:{got}, or free :{want} and redeploy.",
                            f.name, spec.container_port
                        ));
                    }
                }
            }
        }
        Ok(_) => {}
        Err(e) => log(format!(
            "WARN: could not allocate public raw port(s): {e} — raw TCP/UDP ingress is unavailable for this deployment."
        )),
    }

    // Capture vercel.json crons before the manifest is moved into the gateway —
    // they're registered (production only) after the deployment is live.
    let cron_specs = manifest.crons.clone();

    // Tenant = the project's team; tags the deployment + every cell it spawns so
    // compute is partitioned and quota'd per team (same resolver billing/audit use).
    let tenant = {
        // STICKY: never downgrade an already-tagged project to untagged. The
        // team tag lives in node-local ProjectSettings; on a node that never
        // ran set_team (webhook/poll-triggered build landing on a fresh
        // placement) team_of comes back untagged, and stamping THAT onto the
        // record hid the deployment from every fail-closed tenant listing —
        // "the project disappeared from the account". Inherit from the newest
        // record that knows (local, then gossiped peers) before accepting
        // untagged as truth.
        let own = final_settings.team.clone();
        if crate::admin::record_tenant(&own) != crate::admin::UNTAGGED_TENANT {
            own
        } else {
            let inherited = cloud
                .gw
                .list()
                .into_iter()
                .filter(|d| {
                    d.project == manifest.project && d.project_incarnation == Some(incarnation)
                })
                .max_by_key(|d| d.created_at_ms)
                .map(|d| d.tenant.clone())
                .filter(|t| crate::admin::record_tenant(t) != crate::admin::UNTAGGED_TENANT)
                .or_else(|| {
                    cloud
                        .peer_deployments
                        .read()
                        .values()
                        .flatten()
                        .filter(|d| {
                            d.project == manifest.project
                                && d.project_incarnation == Some(incarnation)
                        })
                        .max_by_key(|d| d.created_at_ms)
                        .map(|d| d.tenant.clone())
                        .filter(|t| crate::admin::record_tenant(t) != crate::admin::UNTAGGED_TENANT)
                });
            match inherited {
                Some(t) => {
                    // Repair the local settings row too, so the NEXT build (and
                    // every listing served from this node) has the tag directly.
                    cloud
                        .projects
                        .set_team_exact(&manifest.project, incarnation, &t)?;
                    t
                }
                None => own,
            }
        }
    };
    // A FAILED build must never take the production alias off a WORKING
    // deployment. `deploy_full`'s production branch unconditionally demotes
    // every other deployment of the project and re-points the project alias +
    // default route — so passing `is_production` while `build_failed` handed
    // the live URL to a build-failed page fleet-wide AND let keep-warm drain
    // the previous good deployment to zero instances. One bad push took a
    // healthy app down until a human pushed a fix or promoted by hand; nothing
    // self-healed. Vercel's semantics (which this path's own comments cite)
    // are the opposite: a failed build never touches production.
    //
    // The record is still REGISTERED (browsable at its immutable `dpl-<id>`
    // alias, listed in the dashboard, logs intact) — only the production flip
    // is withheld. `deploy_full`'s own `!has_production` fallback still gives a
    // FIRST-EVER deploy's failure page the project alias, so a brand-new
    // project still resolves to something that explains itself.
    let flip_production = is_production && !build_failed;
    if is_production && build_failed {
        log(
            "Build failed — the current production deployment keeps serving; this build is \
             browsable at its own deployment URL. Fix and push again, or promote another \
             deployment."
                .to_string(),
        );
    }
    if is_container {
        let volume_names = project_volume_names(&project, incarnation, &manifest.functions)
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            !volume_names.is_empty(),
            "container deployment has no exact current-incarnation volume claim"
        );
        cloud
            .projects
            .claim_volumes_exact(&project, incarnation, volume_names)?;
    }
    let info = cloud.gw.deploy_full_with_runtime_exact_marketplace(
        host_static_root,
        Some(runtime_workdir),
        manifest,
        req.creator.clone().unwrap_or_else(|| "you".into()),
        Some(git),
        flip_production,
        if build_failed {
            DeployState::Error
        } else {
            DeployState::Ready
        },
        tenant.clone(),
        incarnation,
        req.marketplace_placement.clone(),
    );
    crate::admin::causal_stamp_new_deployment(cloud, &project, &info.id.0);

    // Record deployment ownership of each browser artifact now that the
    // deployment id exists (`deploy_full` mints it). The bytes are already
    // persisted; ownership is bookkeeping — the GC keep-set derives from live
    // deployment records, so a failed write here is a WARN, never fatal.
    for (function, descriptor) in &browser_bundles {
        if let Err(e) =
            crate::browser_artifacts::add_owner(&descriptor.policy_digest, &info.id.0).await
        {
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
                tenant: crate::admin::norm(&tenant).to_string(),
            })
            .collect();
        let n = cloud.cron.set_source_jobs(&project, "vercel.json", jobs);
        crate::persist::persist(cloud);
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
            let team = crate::admin::norm(&tenant).to_string();
            if let Ok(queue_token) = crate::auth::issue(
                "world-queue-client",
                &team,
                "service",
                false,
                365 * 24 * 3600,
            ) {
                cloud.projects.put_env_exact(
                    &info.project,
                    incarnation,
                    crate::project_settings::EnvVar {
                        key: "HIVE_QUEUE_ENDPOINT".into(),
                        value: queue_url,
                        target: "all".into(),
                        scope: "runtime".into(),
                        sensitive: false,
                        updated_ms: now_ms(),
                    },
                )?;
                cloud.projects.put_env_exact(
                    &info.project,
                    incarnation,
                    crate::project_settings::EnvVar {
                        key: "HIVE_QUEUE_TOKEN".into(),
                        value: queue_token,
                        target: "all".into(),
                        scope: "runtime".into(),
                        sensitive: true,
                        updated_ms: now_ms(),
                    },
                )?;
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
            let ready_project = info.project.clone();
            // Provisioning completes after this build returns. Keep it in the
            // exact-incarnation drain set so project deletion cannot inventory
            // storage until this callback has either committed under lifecycle
            // authority or yielded to the delete.
            let completion_guard = Arc::new(parking_lot::Mutex::new(Some(
                ActiveCheckoutGuard::new(build_dir.clone(), &ready_project, incarnation),
            )));
            let workflow_database = crate::databases::provision(
                cloud.databases.clone(),
                cloud.region.clone(),
                req,
                cloud.db_domain.clone(),
                cloud.node_name.clone(),
                cloud.api_base(),
                move |d| {
                    let cloud_ready = cloud_ready.clone();
                    let ready_project = ready_project.clone();
                    let completion_guard = completion_guard.clone();
                    tokio::spawn(async move {
                        // `provision` accepts `Fn`, not `FnOnce`, so transfer
                        // the single reservation out exactly once. Never hold
                        // it while awaiting the lifecycle writer: deletion owns
                        // the writer while it drains this exact reservation.
                        let completion_guard = completion_guard.lock().take();
                        drop(completion_guard);
                        let _lifecycle =
                            crate::project_settings::lifecycle_write(&ready_project).await;
                        if cloud_ready
                            .projects
                            .get_exact(&ready_project, incarnation)
                            .is_err()
                        {
                            tracing::warn!(
                                project = %ready_project,
                                %incarnation,
                                database = %d.id,
                                "discarded delayed workflow-storage completion for a deleted project incarnation"
                            );
                            return;
                        }
                        if matches!(d.status, crate::databases::DbStatus::Ready) {
                            crate::admin::apply_db_egress(&cloud_ready, &d);
                        }
                        crate::persist::persist(&cloud_ready);
                    });
                },
            );
            if let Err(error) = cloud.projects.claim_database_exact(
                &info.project,
                incarnation,
                workflow_database.id.clone(),
            ) {
                crate::databases::note_teardown_request(&workflow_database.id);
                cloud
                    .databases
                    .remove_db_and_purge_data(&workflow_database.id, &workflow_database.team);
                return Err(anyhow::anyhow!(
                    "workflow storage lost project-incarnation authority before admission: {error}"
                ));
            }
            log(format!(
                "Detected Vercel Workflow SDK ({}): auto-wired hive's managed World -- Queue (HIVE_QUEUE_ENDPOINT/_TOKEN) now, Storage (a provisioned Redis, UPSTASH_REDIS_REST_URL/_TOKEN) finishing in the background -- both active from the next deploy.",
                if ingested > 0 { "JS/TS" } else { "Python" }
            ));
        }
    }

    // The host THIS deployment answers on. `info.alias` is the PROJECT's
    // production host — correct for a production deploy, and exactly wrong for
    // a PREVIEW: stamping it on the build record sent the deploy page, the
    // GitHub commit status, the audit line and the webhook all to the
    // production URL, so "selecting the preview" showed production. A preview's
    // own host is its immutable per-deployment alias (`id_alias`), which always
    // routes to THIS deployment — including while a different deployment holds
    // the production domain.
    let self_alias = if is_production {
        info.alias.clone()
    } else if !info.commit_alias.is_empty() {
        // Prefer the commit alias for previews: it is minted on EVERY fanout
        // target (same commit), so it routes through pooled ingress even when
        // round-robin lands on a different target node — the per-deployment
        // id_alias exists only on the node that registered that record.
        info.commit_alias.clone()
    } else if !info.id_alias.is_empty() {
        info.id_alias.clone()
    } else {
        info.alias.clone()
    };
    if build_failed {
        log(format!(
            "Deployment created (build failed). Aliased to {self_alias}"
        ));
    } else if is_production {
        log(format!("Deployment ready. Aliased to {self_alias}"));
    } else {
        log(format!(
            "Preview deployment ready. Aliased to {self_alias} (production stays on {})",
            info.alias
        ));
    }
    cloud.builds.update(bid, |b| {
        b.state = if build_failed {
            DeployState::Error
        } else {
            DeployState::Ready
        };
        b.finished_ms = Some(now_ms());
        b.deployment_id = Some(info.id.to_string());
        b.alias = Some(self_alias.clone());
    });
    // Persist HERE, not only at the tail of this function: `deploy_full` above
    // already minted the (in-memory-only) Deployment record, and this update
    // just settled the Build record's terminal state — both are the durable
    // facts a crash/OOM-kill must not lose. The ~230 lines between here and the
    // function's own tail persist (cron/workflow-manifest/env-wiring/audit) are
    // all best-effort follow-up that a crash mid-way should never cost the
    // deployment record itself.
    crate::persist::persist(cloud);

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
        let checkout_guard = ActiveCheckoutGuard::new(bd.clone(), &proj, incarnation);
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
            // Release source ownership before waiting for the lifecycle writer.
            // Project deletion holds that writer while draining checkout owners;
            // retaining this guard across the await would deadlock the two.
            drop(checkout_guard);
            let _lifecycle = crate::project_settings::lifecycle_write(&proj).await;
            let Ok(settings) = cloud2.projects.get_exact(&proj, incarnation) else {
                return;
            };
            if !cloud2.gw.deployment_records().iter().any(|record| {
                record.id == dep_id && record.project_incarnation == Some(incarnation)
            }) {
                return;
            }
            let env_keys: Vec<String> = settings.env.iter().map(|e| e.key.clone()).collect();
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
        &tenant,
        &req.creator.clone().unwrap_or_else(|| "you".into()),
        if build_failed {
            "create_failed"
        } else {
            "create"
        },
        "deployment",
        &info.id.to_string(),
        &format!("{} → {self_alias}", info.project),
    );
    crate::persist::persist(cloud);
    // Best-effort final GitHub commit status (success/failure). No-op without a
    // GITHUB_TOKEN; points the check at the live deployment URL.
    {
        let (repo, sha) = (req.repo_url.clone(), full_sha.clone());
        let url = cloud.deploy_url(&self_alias);
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
            "url": cloud.deploy_url(&self_alias),
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
        let tail_settings = cloud.projects.get_exact(&project, incarnation)?;
        let regions = tail_settings.functions.regions.clone();
        // `stateful = is_stateful` (captured above, before `manifest` moved into
        // `deploy_full`): this is THE fanout hazard site — a container built and
        // hosted here would otherwise be replicated to every other selected region
        // with a brand-new, independent, non-synced volume. See
        // `schedule::place`'s `stateful` doc.
        let needs_gpu = tail_settings.functions.gpu;
        let targets = crate::schedule::place(
            cloud,
            &regions,
            false,
            is_stateful,
            needs_gpu,
            crate::schedule::InterpreterNeeds {
                wasm: is_wasm,
                bun: is_bun,
            },
            true,
            true,
            marketplace_approved_nodes.as_ref(),
        );
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
                let _ =
                    fanout_remote(cloud, bid, &req, &project, incarnation, &remote, false).await;
            }
            if is_production {
                let names: Vec<String> = targets.iter().map(|t| t.node.clone()).collect();
                cleanup_non_targets(cloud, &project, &names).await;
            }
        }
    }
    Ok(())
}

/// What happened to ONE target of a fan-out, kept distinct instead of folded
/// into a bare `bool`.
///
/// The distinction is the whole point: `DispatchFailed` means the request never
/// reached the node, so that node's application NEVER RAN and has said nothing
/// about whether it builds. `BuildFailed` means the node received the deploy,
/// ran it, and the application genuinely failed. Collapsing both to `false` let
/// one unreachable node veto an otherwise-successful multi-region deploy:
/// measured in production with targets fc-sanjose-gpu-3 (built, ready),
/// fc-virginia-3 (built, ready) and shadw1 (never reached —
/// "iroh: no reply (peer unreachable over the mesh, or timed out after 20s)"),
/// where the whole deployment was stamped Error and the user was told
/// "Build failed — keeping the existing deployment in place (no relocation)"
/// despite two healthy regions being live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetOutcome {
    /// The remote build reached a Ready deployment.
    Ready,
    /// The deploy was DELIVERED and the remote build reported failure — a real
    /// application fault, and the only outcome that may fail a deployment.
    BuildFailed,
    /// The request never reached this node (no transport succeeded), or the
    /// node accepted it but its state could never be read back. Nothing ran
    /// there, so this is a FLEET-health condition, not an app fault.
    DispatchFailed,
    /// The user cancelled the build.
    Cancelled,
    /// The node received the deploy and deliberately DECLINED to host it — the
    /// stateful `fanout_secondary` guard, which stamps its own build Ready with
    /// NO deployment id precisely because it is not registering a replica.
    ///
    /// This must never count toward promotion. It reads as success on the wire
    /// (`state: "ready"`), and under the at-least-one-ready policy a deploy
    /// whose primary was unreachable and whose two secondaries both declined
    /// would otherwise promote with ZERO nodes actually serving it — strictly
    /// worse than the veto bug this enum exists to fix. It is equally not a
    /// failure: declining is the correct, designed behavior for a single-writer
    /// service, so it must not fail the deploy either.
    Declined,
}

/// The aggregate verdict over every target of one fan-out.
pub(crate) struct FanoutOutcome {
    pub per_target: Vec<(String, TargetOutcome)>,
}

impl FanoutOutcome {
    fn count(&self, want: TargetOutcome) -> usize {
        self.per_target.iter().filter(|(_, o)| *o == want).count()
    }
    /// Targets that are live and serving.
    pub(crate) fn ready(&self) -> usize {
        self.count(TargetOutcome::Ready)
    }
    /// Targets that ran the app and failed it.
    pub(crate) fn build_failed(&self) -> usize {
        self.count(TargetOutcome::BuildFailed)
    }
    /// Targets we could not reach at all — degraded capacity, repairable by a
    /// later reconcile once the node returns.
    pub(crate) fn unreachable(&self) -> Vec<String> {
        self.per_target
            .iter()
            .filter(|(_, o)| *o == TargetOutcome::DispatchFailed)
            .map(|(n, _)| n.clone())
            .collect()
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.count(TargetOutcome::Cancelled) > 0
    }
    /// Targets that deliberately refused to host (stateful single-writer guard).
    pub(crate) fn declined(&self) -> usize {
        self.count(TargetOutcome::Declined)
    }
    /// PROMOTION POLICY: at least one target is genuinely serving.
    ///
    /// Chosen over quorum deliberately. Each target of a stateless fan-out is
    /// an INDEPENDENT full replica that serves the project on its own (the
    /// sub-deploys carry `no_fanout: true` and each registers its own
    /// deployment record), so one Ready region is a working deployment, not a
    /// minority of a consensus group — there is no shared log or split-brain
    /// risk to protect against here. Quorum would mean discarding a region
    /// that is demonstrably serving users because a DIFFERENT region is
    /// unreachable, which is the bug this type exists to fix.
    pub(crate) fn promotable(&self) -> bool {
        self.ready() > 0
    }
}

/// Dispatch a per-target deploy to each remote target's admin and MIRROR its
/// build into this coordinator build record (so the dashboard's existing build
/// page streams the real, remote build log). Returns the PER-TARGET outcome —
/// see [`TargetOutcome`] for why this is not a bool. Each dispatched deploy
/// carries `no_fanout:true` so the
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
    incarnation: ProjectIncarnation,
    remote: &[crate::schedule::Target],
    primary_first: bool,
) -> FanoutOutcome {
    let log = |s: String| cloud.builds.log(bid, s);
    let settings = match cloud.projects.get_exact(project, incarnation) {
        Ok(settings) => settings,
        Err(error) => {
            log(format!(
                "remote dispatch cancelled: project incarnation {incarnation} is no longer active ({error})"
            ));
            return FanoutOutcome {
                per_target: remote
                    .iter()
                    .map(|target| (target.node.clone(), TargetOutcome::Cancelled))
                    .collect(),
            };
        }
    };
    let team = settings
        .team
        .trim()
        .is_empty()
        .then(|| crate::admin::UNTAGGED_TENANT.to_string())
        .unwrap_or_else(|| settings.team.trim().to_string());
    // Forward the user's configured build settings + compute tier so the target
    // builds identically to the coordinator (not just from auto-detect).
    let build_config = serde_json::to_value(settings.build.clone()).ok();
    let function_settings = serde_json::to_value(settings.functions.clone()).ok();
    // PHASE 1 — dispatch every target CONCURRENTLY, then PHASE 2 mirrors them
    // all concurrently (below). This loop used to do both serially per target:
    // dispatch region A, then await A's ENTIRE remote build (clone + npm
    // install + framework build, minutes) before region B was even told to
    // start. Two regions cost 2x a full build end-to-end, three cost 3x — the
    // single largest multiplier on the reported 8-minute deploys, and pure
    // dead time since the sub-builds are completely independent (each carries
    // `no_fanout: true`, so none of them fans out again).
    let dispatch_futs = remote.iter().enumerate().map(|(idx, t)| {
        let mut dreq = req.clone();
        dreq.no_fanout = true;
        // Recompute and stamp the server-derived lane at the dispatch boundary;
        // the target recomputes it again before doing any work. A contradictory
        // production assertion carries no env and is rejected by the target.
        let dispatch_trust = resolve_build_trust(cloud, &dreq, project, incarnation).ok();
        if let Some(context) = &dispatch_trust {
            dreq.target = Some(context.lane.environment().into());
        }
        // Everyone but the designated primary is a secondary replica — see this
        // fn's doc + the stateful fanout-replica guard in `run_build`.
        dreq.fanout_secondary = !(primary_first && idx == 0);
        dreq.project = Some(project.to_string());
        dreq.project_incarnation = Some(incarnation);
        dreq.env = dispatch_trust.as_ref().map(|context| {
            crate::project_settings::env_map_from_settings(
                &settings,
                context.lane.environment(),
                crate::project_settings::EnvExecutionScope::Runtime,
                !context.lane.is_fork(),
            )
        });
        dreq.build_config = build_config.clone();
        dreq.function_settings = function_settings.clone();
        let team = team.clone();
        async move {
        let route = if t.admin.is_some() { "http" } else { "iroh" };
        cloud.builds.log(bid, format!("→ {}: dispatching deploy (via {route})", t.node));
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
                .post(format!("{admin}{RUNTIME_ARTIFACT_FANOUT_PATH}"))
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
                    cloud.builds.log(bid, format!("→ {}: HTTP dispatch failed, retrying via iroh", t.node));
                }
                let body = serde_json::to_vec(&dreq).unwrap_or_default();
                let path = format!(
                    "{RUNTIME_ARTIFACT_FANOUT_PATH}?{}",
                    crate::admin::mesh_team_qs(&team)
                );
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
            cloud.builds.log(bid, format!(
                "✗ {}: deploy dispatch failed — {}",
                t.node,
                attempt_failures.join("; ")
            ));
            return None;
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
            cloud.builds.log(bid, format!("✗ {}: {err}", t.node));
            return None;
        };
        Some((target_bid, dreq.fanout_secondary))
        }
    });

    // Dispatch all targets at once. `join_all` polls these futures on THIS
    // task (no spawn), so the borrows of `cloud`/`req`/`remote` stay valid and
    // nothing needs to be 'static.
    let dispatched: Vec<Option<(String, bool)>> = futures::future::join_all(dispatch_futs).await;

    // Register the PRIMARY's cancel mirror before any mirroring begins — see
    // this fn's doc: `cancel_build` on the coordinator's mirror build id has to
    // reach the REAL process, which runs on the primary target, not here.
    // Secondaries are extras a cancel doesn't need to chase.
    let mut live: Vec<(&crate::schedule::Target, String)> = Vec::new();
    let mut per_target: Vec<(String, TargetOutcome)> = Vec::new();
    for (t, d) in remote.iter().zip(dispatched.into_iter()) {
        match d {
            None => {
                // Dispatch never landed (every transport failed, or the node
                // advertised none) — the deploy did NOT run here, so this can
                // never be reported as an application failure. Already logged
                // with its real cause by the dispatch future.
                per_target.push((t.node.clone(), TargetOutcome::DispatchFailed));
            }
            Some((target_bid, secondary)) => {
                if !secondary {
                    cloud.build_cancels.set_mirror(
                        bid,
                        MirrorTarget {
                            admin: t.admin.clone(),
                            iroh: t.iroh.clone(),
                            target_bid: target_bid.clone(),
                        },
                    );
                }
                live.push((t, target_bid));
            }
        }
    }

    // PHASE 2 — mirror every dispatched build concurrently. Wall-clock is now
    // the SLOWEST remote build, not their sum.
    let mirrors = futures::future::join_all(
        live.iter()
            .map(|(t, target_bid)| mirror_remote_build(cloud, bid, t, target_bid, &t.node)),
    )
    .await;
    for ((t, _), outcome) in live.iter().zip(mirrors.iter()) {
        per_target.push((t.node.clone(), *outcome));
    }

    // Sync auto-detected Build settings back from ONE reachable HTTP target
    // (they all built the same project, so N syncs wrote the same fields N
    // times — and doing it inside the now-concurrent mirror phase would race
    // `set_build` against itself).
    let sync_from = live
        .iter()
        .zip(mirrors.iter())
        .find(|(entry, o)| **o == TargetOutcome::Ready && entry.0.admin.is_some())
        .map(|(entry, _)| entry.0);
    if let Some(t) = sync_from {
        if let Some(admin) = &t.admin {
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
                            let mut cur = match cloud.projects.get_exact(project, incarnation) {
                                Ok(settings) => settings.build,
                                Err(error) => {
                                    log(format!(
                                        "skipped remote build-settings adoption: project incarnation {incarnation} is no longer active ({error})"
                                    ));
                                    return FanoutOutcome { per_target };
                                }
                            };
                            let mut changed = false;
                            // Filling framework from the remote must carry its
                            // AUTO marker too — copying only the string persisted
                            // the target's auto-detected slug here as if
                            // user-explicit, and the next fanout forwarded that
                            // frozen value back, permanently pinning a first-build
                            // misdetection (the exact defect framework_auto fixes).
                            let fw_fill = cur.framework.trim().is_empty()
                                && !remote_bc.framework.trim().is_empty();
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
                            if fw_fill {
                                cur.framework_auto = remote_bc.framework_auto;
                            }
                            if changed {
                                if let Err(error) =
                                    cloud.projects.set_build_exact(project, incarnation, cur)
                                {
                                    log(format!(
                                        "skipped remote build-settings adoption: project incarnation {incarnation} is no longer active ({error})"
                                    ));
                                    return FanoutOutcome { per_target };
                                }
                                crate::persist::persist(cloud);
                            }
                        }
                    }
                }
            }
        }
    }
    FanoutOutcome { per_target }
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
) -> TargetOutcome {
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
            return TargetOutcome::Cancelled;
        }
        // Poll the target's build over HTTP admin when one is advertised, and
        // FALL BACK to iroh IN THE SAME ITERATION when HTTP yields nothing.
        //
        // This used to be an `else if` — HTTP *or* iroh, chosen once by which
        // field was populated — and that is a real, measured 10-minute stall,
        // not a theoretical one. `fanout_remote`'s DISPATCH already falls back
        // (it logs "HTTP dispatch failed, retrying via iroh"), so on a node
        // whose 8786/8787 is firewalled — a documented condition on this fleet
        // (AGENTS.md, "a restrictive cloud security group that blocks
        // 8786/8787 … pushes deploys onto the iroh path"; the GPU/CVM hosts
        // allow inbound 22 only) — the deploy is dispatched fine over iroh
        // while every subsequent poll retries only the dead HTTP route. The
        // remote build finishes in seconds; the coordinator keeps polling a
        // black hole until the 10-minute deadline below and then reports
        // "lost contact with remote build". A target that advertises an admin
        // URL is therefore NOT evidence that the admin URL is reachable, and
        // the poll must never assume it is.
        let sep = if team_qs.is_empty() { "" } else { "?" };
        let mut tried_http = false;
        let mut v: Option<serde_json::Value> = None;
        if let Some(admin) = &target.admin {
            tried_http = true;
            v = match cloud
                .http
                .get(format!("{admin}/v1/builds/{target_bid}{sep}{team_qs}"))
                .timeout(Duration::from_secs(8))
                .send()
                .await
                .ok()
            {
                Some(r) => r.json::<serde_json::Value>().await.ok(),
                None => None,
            };
        }
        if v.is_none() {
            if let Some((id, addr)) = &target.iroh {
                // 20s, matching the DISPATCH timeout above. This poll used to get 8s,
                // which is a shorter budget than the request that successfully placed
                // the deploy in the first place — an iroh dial that has to fall back
                // through a relay can exceed it, and then every single poll returns
                // None while the remote build has actually already finished.
                v = crate::gossip::request_to(
                    cloud,
                    id,
                    addr,
                    hive_p2p::GOSSIP_GET,
                    &format!("/v1/builds/{target_bid}{sep}{team_qs}"),
                    &[],
                    20,
                )
                .await
                .and_then(|b| serde_json::from_slice(&b).ok());
            }
        }
        let Some(v) = v else {
            // A failed poll used to be SILENT (request_to just returns None), so a
            // build stuck at "Building" for a deploy that had really succeeded gave
            // the next reader nothing to go on — it took reading the TARGET's own
            // build record to discover it was `ready` all along. Log the first
            // failure and then every ~30s so the transport, not the app, is
            // implicated immediately.
            polls_failed += 1;
            if polls_failed == 1 || polls_failed % 20 == 0 {
                let transport = match (tried_http, target.iroh.is_some()) {
                    (true, true) => "http+iroh",
                    (true, false) => "http",
                    (false, true) => "iroh",
                    (false, false) => "none",
                };
                tracing::warn!(
                    node = %node, build = %bid, target_build = %target_bid,
                    transport,
                    polls_failed,
                    "cannot read the remote build state — mirrored deploy status will stall"
                );
            }
            if now_ms() > deadline {
                cloud.builds.log(
                    bid,
                    format!(
                        "✗ {node}: lost contact with remote build after {polls_failed} failed polls"
                    ),
                );
                // NOT a build failure: this node never told us its app failed —
                // we simply could not read it. Treated as unreachable so it
                // degrades capacity instead of vetoing healthy regions.
                return TargetOutcome::DispatchFailed;
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
            // A Ready state with NO deployment id is not a replica: it is the
            // stateful `fanout_secondary` guard declining to host (it stamps its
            // own build Ready on the way out). Counting it as Ready would let a
            // deploy promote with nothing serving it, so the deployment id — the
            // proof that something is actually registered and routable on that
            // node — is what distinguishes the two.
            let Some(dep) = v.get("deployment_id").and_then(|x| x.as_str()) else {
                cloud.builds.log(
                    bid,
                    format!("• {node}: declined to host (stateful service, not replicated here)"),
                );
                return TargetOutcome::Declined;
            };
            let dep = dep.to_string();
            let alias = v.get("alias").and_then(|x| x.as_str()).map(String::from);
            cloud.builds.update(bid, |b| {
                b.deployment_id = Some(dep.clone());
                if let Some(a) = &alias {
                    b.alias = Some(a.clone());
                }
            });
            cloud.builds.log(bid, format!("✓ {node}: deployment ready"));
            return TargetOutcome::Ready;
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
            return TargetOutcome::BuildFailed;
        }
        if now_ms() > deadline {
            cloud
                .builds
                .log(bid, format!("✗ {node}: remote build timed out"));
            // Distinct from "lost contact": we COULD read this node's state, so
            // the deploy really did run there and never reached Ready.
            return TargetOutcome::BuildFailed;
        }
    }
}

/// After placing a project on its target node(s), tell every OTHER node that
/// still hosts it to delete it — so changing regions RELOCATES the deployment
/// rather than leaving stale copies. Best-effort; never fails the deploy.
/// Resolve a request's environment WITHOUT running the build: Some(true/false)
/// when this node can classify it (explicit target, or branch vs a locally
/// KNOWN production_branch), None when it genuinely cannot. Callers that gate
/// destructive production-lane actions treat None as "not production".
fn request_is_production(
    cloud: &Arc<CloudState>,
    req: &fluid_core::GitDeployRequest,
    project: &str,
) -> Option<bool> {
    match req.target.as_deref().map(str::trim) {
        Some("production") => Some(true),
        Some("preview") => Some(false),
        _ => {
            let pb = cloud.projects.production_branch_of(project);
            let branch = req.branch.as_deref().unwrap_or("").trim();
            if !pb.is_empty() && !branch.is_empty() {
                Some(branch == pb)
            } else {
                None
            }
        }
    }
}

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
            // Superseded PRODUCTION records only — a preview hosted here is not
            // a stale copy of the production placement; reaping it is what made
            // preview URLs 404 after every production push.
            let removed = cloud.gw.remove_project_superseded(project).await;
            tracing::info!(
                project,
                removed = removed.len(),
                "relocation: removed stale local production copy from coordinator"
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
    // CONCURRENTLY, not one peer at a time. `dispatch_project_delete` retries
    // 3x with a 15s HTTP timeout plus a 20s iroh timeout per attempt, so a
    // single registry-healthy-but-unreachable peer costs ~107s serially and
    // every peer behind it waits — on the pure-remote branch this ran BEFORE
    // the build was stamped Ready, i.e. it showed up to the user as minutes of
    // "Building" on a deployment that was already serving. The receiving arm
    // is idempotent and order-independent, so fanning out is safe.
    // SCOPED reap, never the full project delete: the receiving arm removes
    // only superseded production-lane records — previews survive, and so do
    // the peer's node-local ProjectSettings (team tag, production_branch,
    // env). Reusing the full-teardown primitive here is what made projects
    // vanish from accounts (the team tag is how listings find them) and made
    // preview records disappear after every production push. A pre-upgrade
    // peer answers NO_HANDLER and keeps its stale copies until it upgrades —
    // retention, not correctness.
    futures::future::join_all(
        peers
            .iter()
            .filter(|node| !target_names.iter().any(|t| t == *node))
            .map(|node| crate::admin::dispatch_deployments_reap(cloud, node, project, &team)),
    )
    .await;
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
    let tail = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))
        .or_else(|| s.strip_prefix("ssh://git@github.com/"))?;
    let mut parts = tail.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((owner, repo))
}

fn same_repository(left: &str, right: &str) -> bool {
    match (parse_owner_repo(left), parse_owner_repo(right)) {
        (Some((left_owner, left_repo)), Some((right_owner, right_repo))) => {
            left_owner.eq_ignore_ascii_case(&right_owner)
                && left_repo.eq_ignore_ascii_case(&right_repo)
        }
        _ => {
            fn normalize(value: &str) -> String {
                value
                    .trim()
                    .trim_end_matches('/')
                    .strip_suffix(".git")
                    .unwrap_or_else(|| value.trim().trim_end_matches('/'))
                    .to_string()
            }
            let left = normalize(left);
            !left.is_empty() && left == normalize(right)
        }
    }
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

struct FdiPreparation {
    build_config: Option<crate::project_settings::BuildConfig>,
    settings_are_auto: bool,
    install_override: Option<String>,
    build_override: Option<String>,
    runtime_override: Option<hive_core::Runtime>,
    framework: fluid_build::FrameworkPreset,
    root_workspace: Option<crate::workspace::Workspace>,
    install_dir: PathBuf,
    foreign_subdir: bool,
    is_monorepo: bool,
    package_manager: fluid_build::PackageManagerDetection,
    plan: fluid_build::BuildPlan,
    build_output: Option<fluid_build::BuildOutput>,
    vercel_config_present: bool,
}

async fn prepare_fdi(
    cloud: &Arc<CloudState>,
    repo_root: &Path,
    dir: &Path,
    project: &str,
    vercel_config: Option<&fluid_build::VercelConfig>,
    workspace_member: bool,
) -> anyhow::Result<FdiPreparation> {
    let build_config = cloud
        .projects
        .get_if_set(project)
        .map(|settings| settings.build);
    let settings_are_auto = build_config.as_ref().is_some_and(|build| {
        build.framework_auto
            || matches!(
                build.framework.trim().to_ascii_lowercase().as_str(),
                "other" | "auto" | "auto-detect"
            )
    });
    let project_pick = |field: fn(&crate::project_settings::BuildConfig) -> &String| {
        build_config
            .as_ref()
            .map(field)
            .filter(|value| !value.trim().is_empty())
            .cloned()
    };
    let vercel_pick = |field: fn(&fluid_build::VercelConfig) -> Option<&String>| {
        vercel_config
            .and_then(field)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let vercel_command_pick = |field: fn(&fluid_build::VercelConfig) -> Option<&String>| {
        vercel_config
            .and_then(field)
            .filter(|value| !value.trim().is_empty())
            .cloned()
    };
    let install_override = vercel_command_pick(|config| config.install_command.as_ref())
        .or_else(|| project_pick(|build| &build.install_command));
    let build_override = vercel_command_pick(|config| config.build_command.as_ref())
        .or_else(|| project_pick(|build| &build.build_command));
    let output_override = vercel_pick(|config| config.output_directory.as_ref())
        .or_else(|| project_pick(|build| &build.output_dir));
    let framework_override = vercel_pick(|config| config.framework.as_ref()).or_else(|| {
        build_config
            .as_ref()
            .filter(|build| !build.framework_auto)
            .map(|build| build.framework.clone())
            .filter(|framework| {
                let framework = framework.trim();
                !framework.is_empty()
                    && !framework.eq_ignore_ascii_case("other")
                    && !framework.eq_ignore_ascii_case("auto")
                    && !framework.eq_ignore_ascii_case("auto-detect")
            })
    });
    let runtime_override = vercel_pick(|config| config.runtime.as_ref())
        .and_then(|runtime| hive_core::Runtime::from_config_str(&runtime))
        .or_else(|| {
            vercel_config
                .and_then(|config| config.bun_version.as_ref())
                .map(|_| hive_core::Runtime::Bun)
        })
        .or_else(|| {
            build_config
                .as_ref()
                .map(|build| build.runtime.as_str())
                .filter(|runtime| !runtime.trim().is_empty())
                .and_then(hive_core::Runtime::from_config_str)
        });

    let root_workspace = crate::workspace::load(repo_root).await.map_err(|error| {
        fluid_build::BuildContractError::invalid_metadata("load checkout workspace metadata", error)
    })?;
    let root_is_workspace = root_workspace.is_some();
    let is_monorepo = dir != repo_root && workspace_member && root_is_workspace;
    let install_dir = if is_monorepo {
        repo_root.to_path_buf()
    } else {
        dir.to_path_buf()
    };
    let foreign_subdir = !is_monorepo && dir != repo_root && root_is_workspace;
    let package_manager =
        fluid_build::detect_package_manager_checked(&install_dir).map_err(|error| {
            fluid_build::BuildContractError::invalid_metadata(
                "resolve package-manager metadata",
                error,
            )
        })?;

    // A workspace root owns command selection, but a selected member's own
    // package/workspace declarations are still untrusted metadata. Validate them
    // without allowing them to override the root package manager.
    if dir != install_dir {
        let selected_package_manager =
            fluid_build::detect_package_manager_checked(dir).map_err(|error| {
                fluid_build::BuildContractError::invalid_metadata(
                    "validate selected-application package-manager metadata",
                    error,
                )
            })?;
        let selected_workspace = crate::workspace::load(dir).await.map_err(|error| {
            fluid_build::BuildContractError::invalid_metadata(
                "validate selected-application workspace metadata",
                error,
            )
        })?;
        crate::workspace::validate(dir, selected_workspace.as_ref(), &selected_package_manager)
            .await
            .map_err(|error| {
                fluid_build::BuildContractError::invalid_metadata(
                    "validate selected-application workspace metadata",
                    error,
                )
            })?;
    }

    let nested_workspace;
    let install_workspace = if install_dir == repo_root {
        root_workspace.as_ref()
    } else {
        nested_workspace = crate::workspace::load(&install_dir)
            .await
            .map_err(|error| {
                fluid_build::BuildContractError::invalid_metadata(
                    "load selected-root workspace metadata",
                    error,
                )
            })?;
        nested_workspace.as_ref()
    };
    crate::workspace::validate(&install_dir, install_workspace, &package_manager)
        .await
        .map_err(|error| {
            fluid_build::BuildContractError::invalid_metadata(
                "validate package workspace metadata",
                error,
            )
        })?;

    let resolution = fluid_build::resolve_build_checked(
        dir,
        &package_manager,
        framework_override.as_deref(),
        install_override.as_deref(),
        build_override.as_deref(),
        output_override.as_deref(),
    )?;
    let framework = resolution.plan.framework.clone();

    Ok(FdiPreparation {
        build_config,
        settings_are_auto,
        install_override,
        build_override,
        runtime_override,
        framework,
        root_workspace,
        install_dir,
        foreign_subdir,
        is_monorepo,
        package_manager,
        plan: resolution.plan,
        build_output: resolution.build_output,
        vercel_config_present: vercel_config.is_some(),
    })
}

/// Produce the deployment manifest from a cloned repo: Dockerfile (podman),
/// explicit `fluid.json`, or Framework-Defined Infrastructure. Any error here is
/// recoverable by the caller (the deployment is still created with a fallback).
async fn produce_manifest(
    cloud: &Arc<CloudState>,
    bid: &str,
    isolated: Option<&mut IsolatedBuild>,
    repo_root: &Path,
    dir: &Path,
    project: &str,
    incarnation: ProjectIncarnation,
    _commit: &str,
    first_deploy: bool,
    use_cache: bool,
    build_env: &std::collections::BTreeMap<String, String>,
    trust: &BuildTrustContext,
    vercel_config: Option<&fluid_build::VercelConfig>,
    fdi_preparation: Option<FdiPreparation>,
    workspace_member: bool,
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
            incarnation,
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
    // `isolated` stays Optional past this point: a builder-less node may still
    // complete a plan that executes NO repository-controlled command (the
    // zero-command static lane). Every site that would run a command — or
    // materialize a sealed workspace one produced — requires `Some` and
    // refuses with BUILD_ISOLATION_UNAVAILABLE otherwise.
    let mut isolated = isolated;
    // docker-compose / compose.yaml: a multi-service container deployment in ONE
    // project namespace. Takes precedence over a lone Dockerfile (it expresses the
    // full topology). Single-Dockerfile projects are unaffected.
    let compose_path = crate::compose::compose_file(dir);
    if compose_path.is_some() {
        anyhow::bail!(
            "BUILD_ISOLATION_UNSUPPORTED_SURFACE: builder protocol v1 rejects Compose source builds; no repository command was run on the host"
        );
    }
    let dockerfile = container_build_file(dir);
    if dockerfile.is_some() {
        anyhow::bail!(
            "BUILD_ISOLATION_UNSUPPORTED_SURFACE: builder protocol v1 rejects Dockerfile and Containerfile source builds; no repository command was run on the host"
        );
    }
    if let Ok(s) = tokio::fs::read_to_string(dir.join("fluid.json")).await {
        if let Some(session) = isolated.as_mut() {
            session.finish().await?;
        }
        let mut m = Manifest::from_json(&s)?;
        if m.project.is_empty() {
            m.project = project.to_string();
        }
        log("Detected fluid.json — using project configuration.".into());
        Ok(m)
    } else {
        let preparation = match fdi_preparation {
            Some(preparation) => preparation,
            None => {
                let preparation = prepare_fdi(
                    cloud,
                    repo_root,
                    dir,
                    project,
                    vercel_config,
                    workspace_member,
                )
                .await?;
                if let Some(warning) = preparation.package_manager.conflict_warning.as_ref() {
                    log(format!("WARN: {warning}"));
                }
                preparation
            }
        };
        build_via_fdi(
            cloud,
            bid,
            isolated,
            repo_root,
            dir,
            preparation,
            project,
            incarnation,
            first_deploy,
            use_cache,
            build_env,
            trust,
        )
        .await
    }
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

struct PackageManagerLauncher<'a> {
    detection: &'a fluid_build::PackageManagerDetection,
}

impl<'a> PackageManagerLauncher<'a> {
    fn new(detection: &'a fluid_build::PackageManagerDetection) -> anyhow::Result<Self> {
        anyhow::ensure!(
            detection.validation_error.is_none()
                && matches!(detection.manager, "npm" | "pnpm" | "yarn" | "bun"),
            "invalid package-manager snapshot: {}",
            detection
                .validation_error
                .as_deref()
                .unwrap_or(detection.manager)
        );
        Ok(Self { detection })
    }

    fn invoke(&self, arguments: &str) -> String {
        let manager = self.detection.manager;
        match self.detection.declaration.as_ref() {
            Some(declaration) if manager == "bun" => format!(
                "actual=$(bun --version 2>/dev/null) || {{ printf '%s\\n' 'BUILD_TOOLCHAIN_MISSING: bun' >&2; exit 127; }}; [ \"$actual\" = \"{}\" ] || {{ printf '%s\\n' \"BUILD_TOOLCHAIN_MISMATCH: packageManager requires bun@{}, builder has bun@$actual\" >&2; exit 42; }}; bun {arguments}",
                declaration.version, declaration.version
            ),
            Some(declaration) => format!(
                "command -v corepack >/dev/null 2>&1 || {{ printf '%s\\n' 'BUILD_TOOLCHAIN_MISSING: corepack' >&2; exit 127; }}; actual=$(COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack {} --version 2>/dev/null) || {{ printf '%s\\n' 'BUILD_TOOLCHAIN_MISSING: exact packageManager tool {} is unavailable through corepack' >&2; exit 127; }}; [ \"$actual\" = \"{}\" ] || {{ printf '%s\\n' \"BUILD_TOOLCHAIN_MISMATCH: packageManager requires {}, builder resolved {}@$actual\" >&2; exit 42; }}; COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack {} {arguments}",
                declaration.raw,
                declaration.raw,
                declaration.version,
                declaration.raw,
                manager,
                declaration.raw
            ),
            None => format!(
                "command -v {manager} >/dev/null 2>&1 || {{ printf '%s\\n' 'BUILD_TOOLCHAIN_MISSING: {manager}' >&2; exit 127; }}; {manager} {arguments}"
            ),
        }
    }

    fn install(&self, use_npm_ci: bool) -> String {
        let arguments = match (self.detection.manager, self.detection.lockfile) {
            ("npm", Some(fluid_build::PackageManagerLockfile::Npm)) if use_npm_ci => {
                "ci --no-audit --no-fund"
            }
            ("npm", _) => "install --no-audit --no-fund",
            ("pnpm", Some(fluid_build::PackageManagerLockfile::Pnpm)) => {
                "install --frozen-lockfile"
            }
            ("pnpm", _) => "install",
            ("yarn", Some(fluid_build::PackageManagerLockfile::YarnClassic)) => {
                "install --frozen-lockfile"
            }
            ("yarn", Some(fluid_build::PackageManagerLockfile::YarnModern)) => {
                "install --immutable"
            }
            ("yarn", _) => "install",
            ("bun", Some(fluid_build::PackageManagerLockfile::Bun)) => "install --frozen-lockfile",
            ("bun", _) => "install",
            _ => unreachable!("launcher validates package-manager name"),
        };
        self.invoke(arguments)
    }

    fn run_script(&self, script: &str) -> String {
        self.invoke(&format!("run {script}"))
    }

    fn exec(&self, command: &str, bun_runtime: bool) -> String {
        let arguments = match self.detection.manager {
            "npm" => format!("exec --offline -- {command}"),
            "pnpm" | "yarn" => format!("exec {command}"),
            "bun" if bun_runtime => format!("x --bun --no-install {command}"),
            "bun" => format!("x --no-install {command}"),
            _ => unreachable!("launcher validates package-manager name"),
        };
        self.invoke(&arguments)
    }

    fn add_svelte_adapter(&self) -> String {
        let arguments = match self.detection.manager {
            "npm" => {
                "install -D --no-save --package-lock=false --no-audit --no-fund --legacy-peer-deps \"$spec\""
            }
            "pnpm" => "add -D --lockfile=false --config.strict-peer-dependencies=false \"$spec\"",
            "yarn" => "add -D \"$spec\"",
            "bun" => "add -d \"$spec\"",
            _ => unreachable!("launcher validates package-manager name"),
        };
        self.invoke(arguments)
    }
}

/// Framework-Defined Infrastructure: detect the framework, run its real install
/// + build commands (streamed), then normalize the output into a Manifest —
/// either static assets or a serverless server. This is the executor that turns
/// a source repo into the Build Output API contract (`fluid-build`).
async fn build_via_fdi(
    cloud: &Arc<CloudState>,
    bid: &str,
    mut isolated: Option<&mut IsolatedBuild>,
    repo_root: &Path,
    dir: &Path,
    preparation: FdiPreparation,
    project: &str,
    incarnation: ProjectIncarnation,
    first_deploy: bool,
    use_cache: bool,
    build_env: &std::collections::BTreeMap<String, String>,
    _trust: &BuildTrustContext,
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);

    let FdiPreparation {
        build_config,
        settings_are_auto,
        install_override,
        build_override,
        runtime_override,
        framework,
        root_workspace,
        install_dir,
        foreign_subdir,
        is_monorepo,
        package_manager,
        plan,
        build_output,
        vercel_config_present,
    } = preparation;
    if vercel_config_present {
        log("Detected vercel.json — applying configuration overrides.".into());
    }
    if let Some(runtime) = runtime_override {
        log(format!(
            "Explicit runtime: {} (vercel.json/project settings).",
            runtime.as_str()
        ));
    }

    // Auto-detected framework provenance is persisted independently from package
    // planning so package-less static and Wasmer lanes retain their bypasses.
    if runtime_override != Some(hive_core::Runtime::Wasmer) {
        let current = build_config.clone().unwrap_or_default();
        if current.framework.trim().is_empty() || settings_are_auto {
            let mut next = current;
            next.framework = framework.slug.to_string();
            next.framework_auto = true;
            cloud.projects.set_build_exact(project, incarnation, next)?;
            log(format!(
                "Auto-detected framework: {} (saved to Build settings).",
                framework.name
            ));
        }
    }

    // An explicit Wasmer runtime is a precompiled `.wasm` artifact: no `npm
    // install`, no framework build step, and no `fluid_build::plan_build`
    // call at all (there is no package.json for any of it to act on, and
    // `detect()` has no Wasmer preset — a `.wasm`-only project falls
    // through to its own "unknown -> static" default, which would
    // mislabel the dashboard's Build settings AND, left to reach the
    // `Primitive::Static` arm further down, silently serve the project as
    // a static site with no function at all). Short-circuits before any
    // of that runs — an explicit `runtime: "wasmer"` config must win over
    // every auto-detection step below, the same precedence every other
    // explicit runtime choice already gets.
    if runtime_override == Some(hive_core::Runtime::Wasmer) {
        // An explicitly configured install/build command is NOT silently
        // dropped just because this path skips framework detection. The user
        // set it deliberately (vercel.json > project settings, the precedence
        // this function documents at the top), and a `.wasm` very often IS the
        // output of a build step — `cargo wasix build --release`, `tinygo
        // build`, `wasm-pack`. Discarding it without a word would leave the
        // deployment serving a stale committed artifact, or nothing at all,
        // with a green build and no explanation.
        if let Some(cmd) = install_override
            .as_deref()
            .filter(|command| !command.trim().is_empty())
        {
            log(format!("Running configured install command: `{cmd}`."));
            run_streamed(
                require_build_session(&mut isolated)?,
                dir,
                cmd,
                cloud,
                bid,
                build_env,
            )
            .await?;
        }
        if let Some(cmd) = build_override
            .as_deref()
            .filter(|command| !command.trim().is_empty())
        {
            log(format!("Running configured build command: `{cmd}`."));
            run_streamed(
                require_build_session(&mut isolated)?,
                dir,
                cmd,
                cloud,
                bid,
                build_env,
            )
            .await?;
        }
        if let Some(session) = isolated.as_deref_mut() {
            session.finish().await?;
        }
        // Resolved AFTER the build step above, so a `.wasm` the build just
        // produced is found rather than only one committed to the repo.
        let Some(start) = detect_wasmer_start_cmd(dir).await else {
            anyhow::bail!(
                "runtime \"wasmer\" is set, but no `.wasm` entry module was found — \
                 expected server.wasm / app.wasm / main.wasm, or exactly one *.wasm \
                 file at the project root (a file must start with the \\0asm magic \
                 to count; a Git-LFS pointer or truncated download does not)"
            );
        };
        log(format!(
            "Provisioning Wasmer server: `{}`.",
            start.join(" ")
        ));
        let mut manifest = function_manifest(project, start, runtime_override);
        manifest.framework = "wasmer".to_string();
        return Ok(manifest);
    }

    let launcher = PackageManagerLauncher::new(&package_manager)?;

    let pm = package_manager.manager;
    log(format!(
        "Detected framework: {} — primitive: {:?}, package manager: {} ({:?}){}{}",
        plan.framework.name,
        plan.framework.primitive,
        package_manager.manager,
        package_manager.source,
        if is_monorepo {
            " (workspace monorepo — installing at root)"
        } else {
            ""
        },
        if foreign_subdir {
            " (standalone non-member — installing in selected root)"
        } else {
            ""
        }
    ));

    let has_pkg = install_dir.join("package.json").exists();

    // Generated install commands contain no repository-controlled path text.
    // Installing the declared workspace whole is more work than a filter, but it
    // preserves the package manager's graph semantics and removes a shell-
    // injection boundary from hostile workspace directory names.
    // `npm ci` is the clean, lockfile-exact install. We use it ONLY for an npm
    // project with a committed package-lock.json (never yarn/pnpm — those have
    // their own lockfiles), and ONLY when:
    //   • this is the project's FIRST deployment (Task 1 — clean initial build), or
    //   • the redeploy explicitly disabled the build cache (Task 2 — fresh install).
    // Every other build uses `npm install` + the warm node_modules cache (fast).
    // `npm ci` wipes node_modules, so it never benefits from a restored cache.
    let use_npm_ci = should_use_npm_ci(
        pm,
        package_manager.lockfile == Some(fluid_build::PackageManagerLockfile::Npm),
        first_deploy,
        use_cache,
    );
    // Explicit commands are repository authority and remain byte-for-byte
    // unchanged. Only platform-generated installs enter the checked launcher.
    let install_cmd = install_override
        .clone()
        .unwrap_or_else(|| launcher.install(use_npm_ci));
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
    let build_contract = if has_pkg {
        Some(
            crate::app_discovery::build_contract(
                repo_root,
                dir,
                is_monorepo,
                root_workspace.as_ref(),
                build_override.is_some(),
            )
            .await?,
        )
    } else {
        None
    };
    let build_exec_dir = if matches!(
        build_contract.as_ref(),
        Some(crate::app_discovery::BuildContract::WorkspaceRoot { .. })
    ) {
        repo_root
    } else {
        dir
    };
    let build_cmd = if let Some(explicit) = build_override.as_ref() {
        explicit.clone()
    } else if let Some(crate::app_discovery::BuildContract::WorkspaceRoot {
        orchestrator: crate::app_discovery::WorkspaceOrchestrator::Turbo,
        package_name,
    }) = build_contract.as_ref()
    {
        let turbo = format!("turbo run build --filter={package_name}");
        let command = launcher.exec(&turbo, pm == "bun");
        log(format!(
            "Workspace Turbo build contract: running exact package filter {package_name:?} while preserving declared task dependency edges ({command})."
        ));
        command
    } else if build_contract.as_ref() == Some(&crate::app_discovery::BuildContract::SelectedApp) {
        let command = launcher.run_script("build");
        log(format!(
            "Selected application build contract: running its package build lifecycle, including prebuild hooks ({command})."
        ));
        command
    } else {
        let generated = plan.framework.build_command.trim();
        if let Some(script) = generated.strip_prefix("npm run ") {
            launcher.run_script(script)
        } else if generated.is_empty() {
            String::new()
        } else {
            launcher.exec(generated, runtime_override == Some(hive_core::Runtime::Bun))
        }
    };
    // Host tar extraction/creation would put repository-controlled cache bytes
    // back outside the executor. Do not inspect repository toolchains on the host
    // while isolated cache import/export is disabled; future cache identity must
    // come from the pinned builder capability and sealed artifact protocol.
    if use_cache {
        log("Build cache bypassed: isolated cache import/export is not yet enabled.".into());
    } else {
        log("Build cache disabled — installing dependencies fresh.".into());
    }
    // 1) Install dependencies at the install dir (root for monorepos). With a
    // restored node_modules this is a fast verify; otherwise a clean install.
    if (has_pkg || install_override.is_some()) && !install_cmd.trim().is_empty() {
        log(format!(
            "Running \"{}\"{}",
            install_cmd,
            if is_monorepo { " (workspace root)" } else { "" }
        ));
        run_streamed(
            require_build_session(&mut isolated)?,
            &install_dir,
            &install_cmd,
            cloud,
            bid,
            build_env,
        )
        .await
        .map_err(|error| classify_command_failure("install command failed", &install_cmd, error))?;
    }
    // 1.5) SvelteKit: perform both dependency adaptation and config rewrite in
    // the same isolated workspace as install/build. No installed package or
    // repository path is read or mutated on the host between build steps.
    let is_sveltekit = has_pkg && plan.framework.slug == "sveltekit";
    if is_sveltekit {
        let add_adapter = launcher.add_svelte_adapter();
        let selected_relative = dir.strip_prefix(&install_dir).map_err(|_| {
            anyhow::anyhow!(
                "selected application {} is outside package-manager root {}",
                dir.display(),
                install_dir.display()
            )
        })?;
        let depth = selected_relative.components().count();
        let install_root_argument = if depth == 0 {
            ".".to_string()
        } else {
            std::iter::repeat("..")
                .take(depth)
                .collect::<Vec<_>>()
                .join("/")
        };
        let script = format!(
            r#"set -eu
root=$1
cfg=svelte.config.js
if [ -L "$cfg" ]; then
  printf '%s\n' 'UNSAFE_BUILD_INPUT: svelte.config.js may not be a symlink' >&2
  exit 41
fi
[ -f "$cfg" ] || exit 0
grep -Fq '@sveltejs/adapter-auto' "$cfg" || exit 0
grep -Fq '@sveltejs/adapter-node' "$cfg" && exit 0
backup=$(mktemp -d .hive-svelte-inputs.XXXXXX)
restore_inputs() {{
  if [ -f "$backup/package.json" ]; then cp -- "$backup/package.json" package.json; fi
  if [ -f "$backup/root-package.json" ]; then cp -- "$backup/root-package.json" "$root/package.json"; fi
  for name in package-lock.json pnpm-lock.yaml yarn.lock bun.lock bun.lockb; do
    if [ -f "$backup/$name" ]; then
      cp -- "$backup/$name" "$root/$name"
    else
      rm -f -- "$root/$name"
    fi
  done
  rm -rf -- "$backup"
}}
trap restore_inputs EXIT HUP INT TERM
cp -- package.json "$backup/package.json"
if [ "$root" != . ] && [ -f "$root/package.json" ]; then
  cp -- "$root/package.json" "$backup/root-package.json"
fi
for name in package-lock.json pnpm-lock.yaml yarn.lock bun.lock bun.lockb; do
  if [ -f "$root/$name" ]; then cp -- "$root/$name" "$backup/$name"; fi
done
major=$(node -e 'try {{ process.stdout.write(String(require("./node_modules/@sveltejs/kit/package.json").version).split(".")[0]) }} catch {{}}')
if [ "$major" = 1 ]; then
  spec='@sveltejs/adapter-node@^1'
else
  spec='@sveltejs/adapter-node'
fi
{add_adapter}
restore_inputs
trap - EXIT HUP INT TERM
node -e 'const fs=require("fs"); const p="svelte.config.js"; const s=fs.readFileSync(p,"utf8"); if (!s.includes("@sveltejs/adapter-auto") || s.includes("@sveltejs/adapter-node")) process.exit(42); fs.writeFileSync(p,s.split("@sveltejs/adapter-auto").join("@sveltejs/adapter-node"));'
printf '%s\n' "SvelteKit adapter switched to $spec"
"#
        );
        require_build_session(&mut isolated)?
            .run(
                dir,
                &script,
                "adapt SvelteKit for self-hosting",
                &[install_root_argument],
                false,
                cloud,
                bid,
                build_env,
            )
            .await
            .context("adapting SvelteKit failed")?;
    }
    // 1.6) OpenNext's default wrapper is Lambda-shaped. Generate the fixed Node
    // wrapper inside the isolated workspace, refusing repository symlinks rather
    // than letting a platform write follow one.
    if has_pkg && plan.framework.slug == "opennext" {
        let script = r#"set -eu
for cfg in open-next.config.ts open-next.config.js open-next.config.mjs; do
  if [ -L "$cfg" ]; then
    printf '%s\n' "UNSAFE_BUILD_INPUT: $cfg may not be a symlink" >&2
    exit 41
  fi
  if [ -e "$cfg" ]; then
    printf '%s\n' 'OpenNext: using repository configuration'
    exit 0
  fi
done
umask 022
set -C
cat >open-next.config.ts <<'HIVE_OPENNEXT_CONFIG'
// Generated by the platform: run OpenNext as a standalone Node HTTP server.
export default {
  default: {
    override: {
      wrapper: "node",
      converter: "node",
    },
  },
};
HIVE_OPENNEXT_CONFIG
printf '%s\n' 'OpenNext: generated Node wrapper configuration'
"#;
        require_build_session(&mut isolated)?
            .run(
                dir,
                script,
                "configure OpenNext self-hosting",
                &[],
                false,
                cloud,
                bid,
                build_env,
            )
            .await
            .context("configuring OpenNext failed")?;
    }
    // 2) Build through the workspace root orchestrator when it exists; otherwise
    // build in the selected project directory.
    if (has_pkg || build_override.is_some()) && !build_cmd.trim().is_empty() {
        log(format!("Running \"{}\"", build_cmd));
        run_streamed(
            require_build_session(&mut isolated)?,
            build_exec_dir,
            &build_cmd,
            cloud,
            bid,
            build_env,
        )
        .await
        .map_err(|error| classify_command_failure("build command failed", &build_cmd, error))?;
    }
    if matches!(
        &plan.framework.primitive,
        fluid_build::Primitive::Serverless | fluid_build::Primitive::Hybrid
    ) && runtime_override != Some(hive_core::Runtime::Bun)
    {
        let script = r#"set -eu
p=.hive-after-shim.cjs
[ ! -L "$p" ] || { printf '%s\n' 'UNSAFE_BUILD_INPUT: after shim path may not be a symlink' >&2; exit 41; }
tmp=$(mktemp .hive-after-shim.XXXXXX)
trap 'rm -f -- "$tmp"' EXIT HUP INT TERM
printf '%s' "$1" >"$tmp"
chmod 0444 "$tmp"
mv -f -- "$tmp" "$p"
trap - EXIT HUP INT TERM
"#;
        require_build_session(&mut isolated)?
            .run(
                dir,
                script,
                "stage platform after shim",
                &[AFTER_SHIM_JS.to_string()],
                false,
                cloud,
                bid,
                &std::collections::BTreeMap::new(),
            )
            .await
            .context("staging platform after shim failed")?;
    }
    // Seal once after the complete ordered command graph. A checked-in static
    // Build Output has its own exact output directory; otherwise the normalized
    // selected-app outputDirectory is the directory that must exist in the sealed
    // volume before any bytes are materialized on the host.
    let explicit_commands = install_override.is_some() || build_override.is_some();
    if !explicit_commands {
        if let Some(output) = build_output.as_ref() {
            build_output_manifest(project, plan.framework.slug, output)?;
        }
    }
    if explicit_commands {
        // Repository-controlled install/build commands choose their own
        // output shape — which may be a package-less Build Output API v3
        // artifact under `.vercel/output`, a directory the framework's own
        // `output_dir` heuristic never anticipated. Seal the whole checkout
        // unconditionally: `finish()` still materializes every byte
        // (`finish_inner`'s `sealed.materialize_replace` runs regardless of
        // any directory precondition), so nothing is dropped, and the real
        // `.vercel/output` parse right below decides what actually shipped
        // instead of a precondition that can name a directory the explicit
        // build never promised to produce.
        require_build_session(&mut isolated)?.finish().await?;
    } else if let Some(session) = isolated.as_deref_mut() {
        let expected_output = if build_output.is_some() {
            fluid_build::OutputDirectory::parse(".vercel/output")?
        } else {
            plan.output_dir.clone()
        };
        session.finish_with_output(dir, &expected_output).await?;
    } else if !plan.output_dir.as_str().trim_matches('/').is_empty()
        && plan.output_dir.as_str() != "."
        && !dir.join(plan.output_dir.as_str()).is_dir()
    {
        // Builder-less zero-command lane: no sealed workspace exists (the
        // checkout bytes ARE the output), but the plan's declared output
        // directory must still exist — a framework plan whose output only a
        // build step would have produced must fail here exactly like
        // `finish_with_output`'s precondition, never serve an empty site.
        anyhow::bail!(
            "BUILD_ISOLATION_UNAVAILABLE: plan expects output directory {:?} which does not exist in the checkout, and no isolated build executor is available to produce it. No repository-controlled command was run on the host.",
            plan.output_dir.as_str()
        );
    }

    let parsed_build_output = fluid_build::resolve_build_output_checked(dir)?;
    if let Some(output) = parsed_build_output.as_ref() {
        log("Build Output API v3 detected (.vercel/output).".into());
        let mut manifest = build_output_manifest(project, plan.framework.slug, output)?;
        // Stage the platform-owned launcher AFTER the artifact is fully
        // materialized (`finish`/`finish_with_output` above) and validated
        // (`build_output_manifest` just succeeded) — never before.
        stage_build_output_node_launchers(dir, &mut manifest)?;
        return Ok(manifest);
    }

    use fluid_build::Primitive;
    match plan.framework.primitive {
        Primitive::Static => {
            let static_dir = plan.output_dir.as_str();
            log(format!("Serving static assets from \"{static_dir}\"."));
            let mut manifest = static_manifest(project, static_dir);
            manifest.framework = plan.framework.slug.to_string();
            Ok(manifest)
        }
        Primitive::Serverless | Primitive::Hybrid => {
            // Next.js DEPLOYMENT ADAPTERS (OpenNext / vinext): the build emits a
            // Node HTTP server + a separate immutable-assets dir. Run the server as
            // the Fluid `api` function and serve assets from the CDN, falling
            // through to the function on a miss (the CDN→origin model). This is what
            // gives OpenNext/vinext apps Fluid compute (warm pool + concurrency).
            if let Some(mut m) =
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
                m.framework = plan.framework.slug.to_string();
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
            //
            // Monorepo launch-CWD correction: for a workspace member, `dir` is
            // the workspace INSTALL ROOT (`apps/web`'s parent), but the function
            // must boot from the SELECTED app subdirectory itself — not the root.
            // `detect_start_cmd` against the root reads the ROOT's own
            // package.json, and a root that is a Turbo wrapper
            // (`{"scripts":{"build":"turbo run build"}}`, NO `start` script)
            // falls through to the catch-all `["npm","start"]`; `npm start` at
            // the root then runs `turbo run build`, which exits without ever
            // binding $PORT — the cold-start loop retries it 5 times and the
            // circuit opens as `DEPLOYMENT_CIRCUIT_OPEN`, live-witnessed on a
            // real production deployment. The selected app's own package.json
            // (`apps/web` -> `"start": "next start"`) is the correct authority
            // for BOTH the command and the launch CWD. `cwd_relative` is
            // relative to the deployment's runtime workdir, which the isolated
            // backend already resolves as the checkout root — for a monorepo
            // the correct value is the selected app's own path relative to that
            // root (`apps/web`); for a non-monorepo it stays `None` so the
            // existing single-workdir behavior is byte-identical.
            let cwd_relative = if is_monorepo {
                Some(
                    dir.strip_prefix(&install_dir)
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "selected application {} is outside package-manager root {}",
                                dir.display(),
                                install_dir.display()
                            )
                        })?
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                None
            };
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
            // Fail-loud gate: a resolved `npm start`/`bun run start` whose
            // OWN directory's package.json declares NO `scripts.start` is
            // exactly the workspace-root-without-start bug — the catch-all in
            // `detect_start_cmd` emitted `["npm","start"]` blindly, and the
            // resulting manifest would cold-start a build script, never bind
            // $PORT, and open the deployment circuit. Refuse NOW with a named
            // error instead of registering a guaranteed-broken deployment.
            let catch_all_start = (!runtime_override.is_some_and(|r| r == hive_core::Runtime::Bun)
                && start.len() == 2
                && start[0] == "npm"
                && start[1] == "start")
                || (runtime_override.is_some_and(|r| r == hive_core::Runtime::Bun)
                    && start.len() == 4
                    && start[0] == "bun"
                    && start[1] == "run"
                    && start[2] == "--bun"
                    && start[3] == "start");
            if catch_all_start {
                let pkg_raw = tokio::fs::read_to_string(dir.join("package.json"))
                    .await
                    .unwrap_or_default();
                let has_start = serde_json::from_str::<serde_json::Value>(&pkg_raw)
                    .ok()
                    .and_then(|v| v.get("scripts").and_then(|s| s.get("start")).map(|_| ()))
                    .is_some();
                if !has_start {
                    return Err(anyhow::anyhow!(
                        "build produced no usable production server entry for {}: the build \
                         directory {} has no package.json `scripts.start`, no recognizable server \
                         entry (server.js/index.js/…), and its workspace root has no `scripts.start` \
                         either — a catch-all `{}` here would exit without ever binding $PORT and \
                         open the deployment circuit in production. For a monorepo, the selected \
                         app subdirectory (not the workspace root) is the directory that must own a \
                         `start` script.",
                        project,
                        dir.display(),
                        start.join(" ")
                    ));
                }
            }
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
                        prm.routes.len(),
                        prm.eligible_count(),
                        prm.fallback_count()
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
                    log(format!(
                        "per-route: attached {} route policies to the manifest. Serving still uses `next start` (fallback).",
                        route_policies.len()
                    ));
                }
            }
            let mut m = function_manifest(project, start, runtime_override);
            m.framework = plan.framework.slug.to_string();
            m.route_policies = route_policies;
            // Stamp the monorepo launch CWD onto the single produced function
            // (function_manifest always emits exactly one, named "api"). For a
            // non-monorepo this is `None` and behavior is byte-identical to
            // before.
            if let (Some(f), Some(cwd)) = (m.functions.first_mut(), cwd_relative) {
                f.cwd_relative = Some(cwd);
            }
            // after()/waitUntil runtime support for Node-runtime deployments: drop
            // the platform shim into the build dir and inject it via NODE_OPTIONS
            // so Next.js `after()` (which funnels into the platform waitUntil at
            // globalThis[Symbol.for('@next/request-context')].get().waitUntil) keeps
            // the instance warm for background work via the existing
            // x-fluid-wait-until-ms keep-alive convention. Skipped for Bun (doesn't
            // honor NODE_OPTIONS --require); harmless no-op if after() is never used.
            if runtime_override != Some(hive_core::Runtime::Bun) {
                wire_after_shim(&mut m);
            }
            Ok(m)
        }
    }
}

/// The Node `--require` preload that provides `after()`/`waitUntil` support. Read
/// at compile time from the crate's assets and written into each Node deployment.
const AFTER_SHIM_JS: &str = include_str!("../assets/after-shim.cjs");
const AFTER_SHIM_FILE: &str = ".hive-after-shim.cjs";

/// Wire the already-sealed after()-support shim into the first function's serve
/// environment. The bytes themselves are staged inside `IsolatedBuild` before
/// sealing; this function must remain metadata-only.
fn wire_after_shim(m: &mut Manifest) {
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

fn build_output_contract_error(
    operation: &'static str,
    refusal: fluid_core::BuildOutputV3Refusal,
) -> fluid_build::BuildContractError {
    let code = match &refusal {
        fluid_core::BuildOutputV3Refusal::Invalid { .. } => {
            fluid_build::BuildContractErrorCode::InvalidBuildOutput
        }
        fluid_core::BuildOutputV3Refusal::Unsupported { .. } => {
            fluid_build::BuildContractErrorCode::UnsupportedBuildOutput
        }
    };
    fluid_build::BuildContractError::new(code, operation, refusal.to_string())
}

fn build_output_manifest(
    project: &str,
    framework: &str,
    output: &fluid_build::BuildOutput,
) -> Result<Manifest, fluid_build::BuildContractError> {
    let descriptor = fluid_core::BuildOutputV3::from_parser_value(output.descriptor_value())
        .map_err(|refusal| {
            build_output_contract_error("convert Build Output API v3 descriptor", refusal)
        })?;
    let config = descriptor.config_view().map_err(|refusal| {
        build_output_contract_error("project Build Output API v3 configuration", refusal)
    })?;
    let mut manifest = if descriptor.assets.is_empty() {
        Manifest {
            project: project.to_string(),
            ..Default::default()
        }
    } else {
        static_manifest(project, ".vercel/output/static")
    };

    // Project each checked-in Build Output API v3 function into a real
    // FunctionConfig with a start_cmd that points into the pre-built function
    // directory. The evaluator (build_output_v3_evaluator) validates each
    // projection against the descriptor before the deployment is registered.
    for function in &descriptor.functions {
        let runtime = function.runtime().ok_or_else(|| {
            build_output_contract_error(
                "provision Build Output API v3 functions",
                fluid_core::BuildOutputV3Refusal::invalid(
                    format!("functions[{:?}].config.runtime", function.name),
                    "is missing",
                ),
            )
        })?;
        // Exact allowlist — never a loose "nodejs*.x" pattern. Mirrors
        // fluid_gateway::SUPPORTED_BUILD_OUTPUT_NODE_RUNTIMES, the real
        // enforcement point (`is_supported_build_output_runtime`, applied
        // again just below via `build_output_v3_evaluator`); duplicated here
        // ONLY so an unsupported runtime fails fast with a Build Contract
        // error before a FunctionConfig is even constructed.
        if !matches!(runtime, "nodejs20.x" | "nodejs22.x" | "nodejs24.x") {
            return Err(build_output_contract_error(
                "provision Build Output API v3 functions",
                fluid_core::BuildOutputV3Refusal::unsupported(format!(
                    "function runtime {runtime:?} for {:?}",
                    function.name
                )),
            ));
        }
        // `descriptor.validate()` (called by `from_parser_value` above)
        // already REQUIRES a non-empty `handler` string that names a real
        // entry in this function's own `files` inventory — never defaulted.
        // This lookup can only fail if that invariant were ever weakened, and
        // must still fail the build loudly rather than silently guessing
        // `index.js` (a guess that could point at a file the tenant never
        // shipped, or shadow a same-named file in a sibling function).
        let handler = function
            .config
            .get("handler")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                build_output_contract_error(
                    "provision Build Output API v3 functions",
                    fluid_core::BuildOutputV3Refusal::invalid(
                        format!("functions[{:?}].config.handler", function.name),
                        "is missing",
                    ),
                )
            })?;
        let memory = function
            .config
            .get("memory")
            .and_then(|v| v.as_u64())
            .unwrap_or(1024) as u32;
        // Platform default (5 minutes), matching `default_max_duration` — the
        // prior `10` here was a Build-Output-only regression from that
        // default, silently killing any handler that ran longer than 10s.
        let max_duration = function
            .config
            .get("maxDuration")
            .and_then(|v| v.as_u64())
            .unwrap_or(fluid_core::FunctionConfig::default().max_duration_secs);
        let func_dir_rel = format!(".vercel/output/functions/{}.func", function.name);
        manifest.functions.push(FunctionConfig {
            name: function.name.clone(),
            runtime: "node".to_string(),
            // Direct Node argv against the platform-owned launcher staged by
            // `stage_build_output_node_launchers` right after this manifest is
            // built from the fully materialized artifact — no shell, no
            // package manager. The handler path passed as argv[1] is relative
            // to the function's OWN `.func` CWD (`cwd_relative` below), the
            // exact directory the launcher's `import()` resolves against.
            start_cmd: vec![
                "node".to_string(),
                BUILD_OUTPUT_NODE_LAUNCHER_FILE.to_string(),
                handler.to_string(),
            ],
            memory_mib: memory,
            max_duration_secs: max_duration,
            cwd_relative: Some(func_dir_rel),
            ..Default::default()
        });
    }
    manifest.framework = framework.to_string();
    manifest.images = config
        .images
        .as_ref()
        .map(|images| fluid_core::ImagesConfig {
            sizes: images.sizes.clone().unwrap_or_default(),
            qualities: images.qualities.clone(),
            formats: images.formats.clone(),
            minimum_cache_ttl: images.minimum_cache_ttl,
            domains: images.domains.clone().unwrap_or_default(),
            remote_patterns: images
                .remote_patterns
                .iter()
                .map(|pattern| fluid_core::RemotePattern {
                    protocol: pattern.protocol.clone(),
                    hostname: pattern.hostname.clone(),
                    port: pattern.port.clone(),
                    pathname: pattern.pathname.clone(),
                    search: pattern.search.clone(),
                })
                .collect(),
            local_patterns: images
                .local_patterns
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|pattern| fluid_core::LocalPattern {
                    pathname: pattern.pathname.clone(),
                    search: pattern.search.clone(),
                })
                .collect(),
            dangerously_allow_svg: images.dangerously_allow_svg,
            content_security_policy: images.content_security_policy.clone(),
            content_disposition_type: images.content_disposition_type.clone(),
        });
    manifest.crons = config
        .crons
        .iter()
        .map(|cron| fluid_core::CronSpec {
            path: cron.path.clone(),
            schedule: cron.schedule.clone(),
        })
        .collect();
    manifest.build_output_v3 = Some(descriptor);

    let compiled = fluid_gateway::build_output_v3_evaluator(&manifest).map_err(|refusal| {
        build_output_contract_error("compile Build Output API v3 deployment contract", refusal)
    })?;
    if compiled.is_none() {
        return Err(fluid_build::BuildContractError::new(
            fluid_build::BuildContractErrorCode::InvalidBuildOutput,
            "compile Build Output API v3 deployment contract",
            "server-derived descriptor was not authoritative",
        ));
    }
    Ok(manifest)
}

/// The ONE platform-owned Node HTTP bridge for Build Output API v3 `.func`
/// bundles (build-output-immutable-launcher). Read at compile time from the
/// crate's own source tree (the `AFTER_SHIM_JS` precedent above) and staged
/// verbatim into every Node Build Output function's OWN `.func` directory —
/// never into the deployment root, and never shared byte-for-byte with a
/// tenant-writable path.
const BUILD_OUTPUT_NODE_LAUNCHER_SRC: &str = include_str!("build-output-node-launcher.mjs");
const BUILD_OUTPUT_NODE_LAUNCHER_FILE: &str = ".hive-build-output-launcher.mjs";

/// Stage [`BUILD_OUTPUT_NODE_LAUNCHER_SRC`] into every Node Build Output
/// function's own `.func` directory on the now-fully-materialized host
/// checkout, ONLY after: (1) every repository-controlled install/build
/// command has finished (`isolated.finish`/`finish_with_output` already ran),
/// (2) the artifact has been parsed and validated
/// (`fluid_build::resolve_build_output_checked` + `build_output_manifest`
/// already succeeded, which is the only way this function is ever reached),
/// and (3) the checks below — no symlinked `.func` directory, no existing
/// entry at the reserved launcher path — pass for THIS function. Never called
/// from the pre-materialization fast-fail check (that call site discards its
/// `Manifest`; there is nothing to deploy and no host directory to write
/// into yet).
fn stage_build_output_node_launchers(
    dir: &Path,
    manifest: &mut Manifest,
) -> Result<(), fluid_build::BuildContractError> {
    let stage_error = |function: &str, detail: String| {
        fluid_build::BuildContractError::new(
            fluid_build::BuildContractErrorCode::InvalidBuildOutput,
            "stage Build Output API v3 Node launcher",
            format!("function {function:?}: {detail}"),
        )
    };
    let canonical_root = dir.canonicalize().map_err(|e| {
        fluid_build::BuildContractError::new(
            fluid_build::BuildContractErrorCode::InvalidBuildOutput,
            "stage Build Output API v3 Node launcher",
            format!("checkout root {} is not readable: {e}", dir.display()),
        )
    })?;
    for function in manifest.functions.iter_mut() {
        if function.runtime != "node" || function.cwd_relative.is_none() {
            continue;
        }
        let func_dir_rel = function.cwd_relative.clone().unwrap_or_default();
        let func_dir = dir.join(&func_dir_rel);
        let canonical_func_dir = func_dir.canonicalize().map_err(|e| {
            stage_error(
                &function.name,
                format!("function directory is not readable: {e}"),
            )
        })?;
        // The parser already proved every FILE under this function is a
        // portable relative regular-file path; the DIRECTORY CHAIN down to
        // it is platform-controlled attack surface once we are about to
        // write a new file into it, so it is checked independently here —
        // a symlinked `.func` directory (or an ancestor of it) could
        // otherwise redirect this write anywhere on the host.
        if !canonical_func_dir.starts_with(&canonical_root) {
            return Err(stage_error(
                &function.name,
                "function directory escapes the checkout root (symlink?)".to_string(),
            ));
        }
        let launcher_path = canonical_func_dir.join(BUILD_OUTPUT_NODE_LAUNCHER_FILE);
        // Collision refusal: this exact filename is platform-reserved inside
        // every `.func` directory. `symlink_metadata` (not `metadata`) so an
        // existing SYMLINK at this path is caught even if its target is
        // missing/circular.
        if std::fs::symlink_metadata(&launcher_path).is_ok() {
            return Err(stage_error(
                &function.name,
                format!(
                    "already contains a reserved platform launcher path {BUILD_OUTPUT_NODE_LAUNCHER_FILE:?}"
                ),
            ));
        }
        let tmp_path = canonical_func_dir.join(format!("{BUILD_OUTPUT_NODE_LAUNCHER_FILE}.tmp"));
        let write_result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut tmp = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            tmp.write_all(BUILD_OUTPUT_NODE_LAUNCHER_SRC.as_bytes())?;
            tmp.sync_all()?;
            drop(tmp);
            std::fs::rename(&tmp_path, &launcher_path)?;
            let mut perms = std::fs::metadata(&launcher_path)?.permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&launcher_path, perms)
        })();
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(stage_error(
                &function.name,
                format!("cannot stage launcher: {e}"),
            ));
        }
    }
    Ok(())
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
    if runtime == Some(hive_core::Runtime::Wasmer) {
        if let Some(cmd) = detect_wasmer_start_cmd(dir).await {
            return cmd;
        }
    }
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

/// Locate a Wasmer function's compiled `.wasm` entry module and build its
/// `wasmer run` invocation. Conventional names first (`server.wasm` /
/// `app.wasm` / `main.wasm`, mirroring the `server.js`/`app.py` convention
/// above); otherwise a SINGLE `*.wasm` file at the project root is
/// unambiguous. `--net` (live-verified against a real `cargo wasix build`
/// axum server) grants the guest socket access — WASIX's own opt-in gate,
/// analogous to a container needing `-p` published ports — and
/// `--forward-host-env` carries the backend-assigned `$PORT` (and any
/// project env vars) from the spawning process into the guest; the port is
/// chosen at COLD-START time by fluid-compute, long after this build-time
/// detection runs, so it can never be baked into `--env PORT=<literal>`
/// here. Returns `None` (never a guess) when no `.wasm` entry is found or
/// more than one sits at the root with no convention name to disambiguate —
/// callers fall back to the generic Node/npm detection below, which will
/// itself fail loudly (no server to start) rather than silently running the
/// wrong module.
async fn detect_wasmer_start_cmd(dir: &Path) -> Option<Vec<String>> {
    let wasmer_run = |entry: String| {
        vec![
            "wasmer".to_string(),
            "run".to_string(),
            "--net".to_string(),
            "--forward-host-env".to_string(),
            entry,
        ]
    };
    for entry in ["server.wasm", "app.wasm", "main.wasm"] {
        if is_wasm_module(&dir.join(entry)).await {
            return Some(wasmer_run(entry.to_string()));
        }
    }
    let mut rd = tokio::fs::read_dir(dir).await.ok()?;
    let mut found: Option<String> = None;
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".wasm") && is_wasm_module(&e.path()).await {
            if found.is_some() {
                return None;
            }
            found = Some(name);
        }
    }
    found.map(wasmer_run)
}

/// Is `p` a real WebAssembly module — a regular file whose first four bytes are
/// the `\0asm` magic? A `.wasm` NAME proves nothing: an unresolved Git-LFS
/// pointer, a directory, a truncated download or an HTML error page saved by a
/// fetch step all carry the extension and none of them can be executed.
///
/// Checked at BUILD time on purpose. This runtime's build path compiles nothing
/// and runs nothing (a `.wasm` is already compiled), so without this check the
/// first thing that ever opens the file is a cold start on some other node —
/// turning a bad artifact into a green build, a Ready deployment and a
/// permanent per-request crash loop, instead of one loud build failure. Cheap:
/// a 4-byte read of a file already on local disk.
async fn is_wasm_module(p: &Path) -> bool {
    use tokio::io::AsyncReadExt;
    if !tokio::fs::metadata(p)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return false;
    }
    let Ok(mut f) = tokio::fs::File::open(p).await else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).await.is_ok() && &magic == b"\0asm"
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

/// Extract an uploaded ZIP into `dir` through a descriptor-relative, no-follow,
/// bounded importer. Every entry is validated BEFORE any byte touches the filesystem:
/// symlinks, absolute paths, `..` components, and non-regular files are rejected.
///
/// When the archive is a single top-level directory (the common "zip a folder" /
/// GitHub "Download ZIP" shape), the wrapper prefix is stripped so the project root
/// lands directly at `dir`. macOS cruft (`__MACOSX`, `.DS_Store`) is skipped.
///
/// Ceilings: `MAX_ZIP_ENTRIES` (16 384) and `MAX_ZIP_BYTES` (256 MiB) are hard
/// bounds — the admin handler already caps the raw upload at 10 MiB, so the byte
/// ceiling is a defense-in-depth floor against a future code path that bypasses
/// the handler's check. Returns the resulting file count.
const MAX_ZIP_ENTRIES: usize = 16_384;
const MAX_ZIP_BYTES: u64 = 256 * 1024 * 1024;

async fn extract_zip_into(bytes: &[u8], dir: &Path) -> anyhow::Result<u64> {
    tokio::fs::create_dir_all(dir).await?;
    let reader = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| anyhow::anyhow!("invalid zip archive: {e}"))?;

    // --- pass 1: collect entry paths, validate, and detect wrapper prefix ---
    let mut entries: Vec<(String, usize)> = Vec::new(); // (normalized path, zip index)
    let mut wrapper_prefix: Option<String> = None;

    for i in 0..archive.len() {
        anyhow::ensure!(
            entries.len() < MAX_ZIP_ENTRIES,
            "zip archive exceeds {MAX_ZIP_ENTRIES} entries"
        );
        let entry = archive
            .by_index(i)
            .map_err(|e| anyhow::anyhow!("zip entry {i}: {e}"))?;
        let name = entry.name().to_string();

        // Skip macOS cruft before any path processing.
        if is_macos_cruft(&name) {
            continue;
        }

        // Reject symlinks — the zip crate exposes symlink targets via
        // `entry.link()` (Unix symlink extra field) and `entry.is_symlink()`.
        // Neither is a regular file — fail loudly.
        anyhow::ensure!(
            !entry.is_symlink(),
            "zip entry {i}: symlinks are not allowed ({name})"
        );

        // Reject directories-as-entries (they carry no content; the zip crate
        // reports them via `entry.is_dir()`). We only accept regular files.
        anyhow::ensure!(
            entry.is_file(),
            "zip entry {i}: only regular files are allowed, got: {name}"
        );

        // Validate the path: no absolute, no `..` components, no empty names.
        let normalized = validate_zip_entry_path(&name, i)?;
        entries.push((normalized, i));
    }

    anyhow::ensure!(
        !entries.is_empty(),
        "zip archive contains no files after stripping macOS cruft"
    );

    // Detect a single wrapper directory: every entry shares the same first
    // component, and it is NOT the whole path (i.e. there is content inside).
    if let Some(prefix) = common_entry_prefix(&entries) {
        wrapper_prefix = Some(prefix);
    }

    // --- pass 2: extract files, bounded by byte count ---
    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    for (rel_path, idx) in &entries {
        // Strip the wrapper prefix if one was detected.
        let output_path = if let Some(ref prefix) = wrapper_prefix {
            strip_prefix_component(rel_path, prefix)
                .unwrap_or(rel_path.as_str())
                .to_string()
        } else {
            rel_path.clone()
        };

        // Defense-in-depth: the output path must still be relative and safe.
        let output_path = validate_zip_entry_path(&output_path, *idx)?;

        let dest = dir.join(&output_path);
        // Create parent directories. The path is validated above, so parent
        // components are known-safe relative segments.
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Read the entry bytes synchronously BEFORE any async I/O. The ZIP
        // source is in-memory (a `Cursor<&[u8]>`), so the read is instant.
        // `ZipFile` borrows from the non-Send `Cursor<&[u8]>`, so holding it
        // across an `.await` makes the whole future non-Send. We read the
        // bytes here and drop the entry before the first `.await` below.
        let (entry_bytes, entry_size) = {
            let mut entry = archive
                .by_index(*idx)
                .map_err(|e| anyhow::anyhow!("zip entry {idx}: {e}"))?;
            let size = entry.size();
            let mut buf = Vec::with_capacity(size as usize);
            let mut reader = std::io::Read::take(&mut entry, size + 1); // +1 for overflow detect
            let read_bytes = std::io::copy(&mut reader, &mut buf)?;
            anyhow::ensure!(
                read_bytes == size,
                "zip entry {idx}: declared {size} bytes but read {read_bytes}"
            );
            (buf, size)
        };

        total_bytes = total_bytes
            .checked_add(entry_size)
            .ok_or_else(|| anyhow::anyhow!("zip byte count overflow"))?;
        anyhow::ensure!(
            total_bytes <= MAX_ZIP_BYTES,
            "zip archive exceeds {MAX_ZIP_BYTES} bytes uncompressed"
        );

        // Write to a temp file next to the destination, then atomically rename.
        // This keeps a partial write from being observed as a complete file.
        let tmp = dest.with_file_name(format!(
            ".tmp-{}-{}",
            dest.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            idx
        ));
        let mut out = tokio::fs::File::create(&tmp).await?;
        tokio::io::AsyncWriteExt::write_all(&mut out, &entry_bytes).await?;
        drop(out);
        tokio::fs::rename(&tmp, &dest).await?;
        file_count += 1;
    }

    Ok(file_count)
}

/// True when the entry name is macOS archive cruft that should never be
/// materialized.
fn is_macos_cruft(name: &str) -> bool {
    // Strip trailing slashes (directory markers) for comparison.
    let name = name.trim_end_matches('/');
    if name == "__MACOSX" || name.starts_with("__MACOSX/") || name.starts_with("__MACOSX\\") {
        return true;
    }
    if name == ".DS_Store" || name.ends_with("/.DS_Store") || name.ends_with("\\.DS_Store") {
        return true;
    }
    false
}

/// Validate a ZIP entry path: reject absolute paths, `..` components, empty
/// names, and Windows-style absolute paths (e.g. `C:\foo`). Returns the
/// normalized path (forward slashes, no leading slash).
fn validate_zip_entry_path(name: &str, idx: usize) -> anyhow::Result<String> {
    anyhow::ensure!(!name.is_empty(), "zip entry {idx}: empty name");

    // Reject Windows-style absolute paths (drive letter + colon).
    if name.as_bytes().len() >= 2
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name.as_bytes()[1] == b':'
    {
        anyhow::bail!("zip entry {idx}: absolute Windows path rejected: {name}");
    }

    // Normalize separators to forward slashes.
    let normalized = name.replace('\\', "/");

    // Split into components and validate each.
    let mut clean = Vec::new();
    for component in normalized.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            anyhow::bail!("zip entry {idx}: parent-directory component rejected: {name}");
        }
        // Reject any component that starts with a drive letter (catches
        // `C:` without a backslash, which the prefix check above misses).
        if component.len() >= 2
            && component.as_bytes()[0].is_ascii_alphabetic()
            && component.as_bytes()[1] == b':'
        {
            anyhow::bail!("zip entry {idx}: drive-letter component rejected: {name}");
        }
        clean.push(component);
    }

    anyhow::ensure!(
        !clean.is_empty(),
        "zip entry {idx}: path resolves to empty after normalization: {name}"
    );

    Ok(clean.join("/"))
}

/// If every entry shares the same first path component, return that component
/// (the "wrapper directory" prefix). Returns `None` when entries are already at
/// the root or when there is no common prefix.
fn common_entry_prefix(entries: &[(String, usize)]) -> Option<String> {
    if entries.len() < 2 {
        // A single entry with a first component is a candidate wrapper dir.
        // But a single entry IS the content — if it has a single component
        // it's already at the root (e.g. "package.json"), not a wrapper.
        // If it has a first component, that IS the wrapper.
        let first = entries.first()?.0.split('/').next()?;
        if first == entries.first()?.0 {
            // Single-component path — no wrapper.
            return None;
        }
        return Some(first.to_string());
    }

    let first = entries.first()?.0.split('/').next()?;
    if first.is_empty() || first == entries.first()?.0 {
        return None; // already at root
    }

    for (path, _) in entries.iter().skip(1) {
        match path.split('/').next() {
            Some(c) if c == first => {}
            _ => return None,
        }
    }

    Some(first.to_string())
}

/// Strip the given prefix component from a path. `path` is a normalized
/// forward-slash path. Returns the remainder after the first component and
/// separator.
fn strip_prefix_component<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some(""); // path IS the prefix — shouldn't happen for a file, but safe
    }
    // Strip the separator after the prefix.
    Some(rest.strip_prefix('/').unwrap_or(rest))
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
            log(format!(
                "Bytecode-cache: bundled + precompiled `{entry_arg}` -> `{rel}` (bun {ver}, with external source map)."
            ));
            vec!["bun".to_string(), "run".to_string(), rel]
        }
        Ok(o) => {
            let stderr: String = String::from_utf8_lossy(&o.stderr)
                .trim()
                .chars()
                .take(300)
                .collect();
            log(format!(
                "Bytecode-cache: bun build failed ({stderr}); using the original entry uncached — app still starts normally."
            ));
            original
        }
        Err(e) => {
            log(format!(
                "Bytecode-cache: could not run `bun build` ({e}); using the original entry uncached."
            ));
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

fn build_executor_platform_fault(error: &anyhow::Error) -> bool {
    let Some(error) = error.downcast_ref::<crate::build_executor::BuildExecutorError>() else {
        return false;
    };
    matches!(
        error.code,
        crate::build_executor::BuildExecutorErrorCode::InvalidConfig
            | crate::build_executor::BuildExecutorErrorCode::InvalidRequest
            | crate::build_executor::BuildExecutorErrorCode::CapabilityUnavailable
            | crate::build_executor::BuildExecutorErrorCode::CapabilityMismatch
            | crate::build_executor::BuildExecutorErrorCode::UnsupportedSurface
            | crate::build_executor::BuildExecutorErrorCode::PodmanFailed
            | crate::build_executor::BuildExecutorErrorCode::OutputLimitExceeded
            | crate::build_executor::BuildExecutorErrorCode::OutputEntryLimitExceeded
            | crate::build_executor::BuildExecutorErrorCode::UnsafeOutput
            | crate::build_executor::BuildExecutorErrorCode::SealIntegrityMismatch
            | crate::build_executor::BuildExecutorErrorCode::CleanupFailed
    )
}

struct IsolatedBuild {
    root: PathBuf,
    session: Option<crate::build_executor::BuildSession>,
}

impl IsolatedBuild {
    async fn begin(root: &Path) -> anyhow::Result<Self> {
        let executor = crate::build_executor::get()
            .map_err(anyhow::Error::new)
            .context("BUILD_ISOLATION_UNAVAILABLE: acquire live-probed executor")?;
        let session = executor
            .begin(crate::build_executor::BuildRequest {
                checkout: root.to_path_buf(),
                surface: crate::build_executor::BuildSurface::RepositoryCommands,
            })
            .await
            .map_err(anyhow::Error::new)
            .context("BUILD_ISOLATION_UNAVAILABLE: begin isolated build")?;
        Ok(Self {
            root: root.to_path_buf(),
            session: Some(session),
        })
    }

    fn workspace_path(&self, dir: &Path) -> anyhow::Result<crate::build_executor::WorkspacePath> {
        let relative = dir.strip_prefix(&self.root).map_err(|_| {
            anyhow::anyhow!(
                "build command directory {} is outside isolated checkout {}",
                dir.display(),
                self.root.display()
            )
        })?;
        let relative = relative
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("build command directory is not valid UTF-8"))?;
        crate::build_executor::WorkspacePath::parse(relative.replace('\\', "/"))
            .map_err(anyhow::Error::new)
    }

    async fn run(
        &mut self,
        dir: &Path,
        command: &str,
        label: &str,
        args: &[String],
        accept_nonzero: bool,
        cloud: &Arc<CloudState>,
        bid: &str,
        env: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<i32> {
        let cwd = self.workspace_path(dir)?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("isolated build session was already sealed"))?;
        let mut step = crate::build_executor::BuildStep::shell(label, command);
        step.args = args.to_vec();
        step.cwd = cwd;
        step.env = env.clone();
        step.accept_nonzero = accept_nonzero;
        let cancelled = async {
            loop {
                if cloud.build_cancels.is_cancelled(bid) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let result = tokio::select! {
            result = session.run(step, |line| {
                let stream = match line.stream {
                    crate::build_executor::BuildLogStream::Stdout => "",
                    crate::build_executor::BuildLogStream::Stderr => "stderr: ",
                };
                let suffix = if line.dropped_bytes == 0 {
                    String::new()
                } else {
                    format!(" … [{} bytes dropped]", line.dropped_bytes)
                };
                cloud.builds.log(bid, format!("  {stream}{}{suffix}", line.text));
            }) => result,
            _ = cancelled => return Err(BuildCancelled.into()),
        };
        let result = result.map_err(anyhow::Error::new)?;
        if cloud.build_cancels.is_cancelled(bid) {
            return Err(BuildCancelled.into());
        }
        Ok(result.exit_code)
    }

    fn output_workspace_path(
        &self,
        dir: &Path,
        output: &fluid_build::OutputDirectory,
    ) -> anyhow::Result<crate::build_executor::WorkspacePath> {
        let app = self.workspace_path(dir)?;
        let path = match (app.as_str(), output.as_str()) {
            (".", output) => output.to_string(),
            (app, ".") => app.to_string(),
            (app, output) => format!("{app}/{output}"),
        };
        crate::build_executor::WorkspacePath::parse(path).map_err(anyhow::Error::new)
    }

    async fn finish_with_output(
        &mut self,
        dir: &Path,
        output: &fluid_build::OutputDirectory,
    ) -> anyhow::Result<()> {
        let expected = self.output_workspace_path(dir, output)?;
        self.finish_inner(Some(&expected)).await
    }

    async fn finish(&mut self) -> anyhow::Result<()> {
        self.finish_inner(None).await
    }

    async fn finish_inner(
        &mut self,
        expected: Option<&crate::build_executor::WorkspacePath>,
    ) -> anyhow::Result<()> {
        let session = self
            .session
            .take()
            .ok_or_else(|| anyhow::anyhow!("isolated build session was already sealed"))?;
        let sealed = session
            .seal(crate::build_executor::WorkspacePath::root())
            .await
            .map_err(anyhow::Error::new)?;
        if let Some(expected) = expected {
            sealed
                .require_directory(expected)
                .await
                .map_err(anyhow::Error::new)?;
        }
        sealed
            .materialize_replace(&self.root)
            .await
            .map_err(anyhow::Error::new)
    }
}

async fn reseal_platform_output(root: &Path) -> anyhow::Result<()> {
    let executor = crate::build_executor::get()
        .map_err(anyhow::Error::new)
        .context("BUILD_ISOLATION_UNAVAILABLE: acquire executor for final seal")?;
    let session = executor
        .begin(crate::build_executor::BuildRequest {
            checkout: root.to_path_buf(),
            surface: crate::build_executor::BuildSurface::RepositoryCommands,
        })
        .await
        .map_err(anyhow::Error::new)
        .context("BUILD_ISOLATION_UNAVAILABLE: import platform-finalized output")?;
    let sealed = session
        .seal(crate::build_executor::WorkspacePath::root())
        .await
        .map_err(anyhow::Error::new)
        .context("seal platform-finalized output")?;
    sealed
        .materialize_replace(root)
        .await
        .map_err(anyhow::Error::new)
        .context("publish platform-finalized output")
}

/// The one gate every repository-controlled build step funnels through when
/// the session is optional: a builder-less node (only the zero-command static
/// lane can succeed there) refuses with the canonical message instead of ever
/// running the step on the host.
fn require_build_session<'a>(
    isolated: &'a mut Option<&mut IsolatedBuild>,
) -> anyhow::Result<&'a mut IsolatedBuild> {
    isolated.as_deref_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "BUILD_ISOLATION_UNAVAILABLE: this build step is a repository-controlled command and requires an isolated build executor, but none is available on this node. No repository-controlled command was run on the host."
        )
    })
}

async fn run_streamed(
    isolated: &mut IsolatedBuild,
    dir: &Path,
    command: &str,
    cloud: &Arc<CloudState>,
    bid: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<()> {
    isolated
        .run(
            dir,
            command,
            "repository build command",
            &[],
            false,
            cloud,
            bid,
            env,
        )
        .await
        .map(|_| ())
}

/// A repository command dying with the shell's 127 has failed on a MISSING
/// EXECUTABLE, and whose fault that is depends on where the executable was
/// supposed to come from. Platform-generated commands probe their own
/// toolchain and print `BUILD_TOOLCHAIN_MISSING`/`BUILD_TOOLCHAIN_MISMATCH`
/// themselves before exiting 127/42 (see `PackageManagerLauncher::invoke`),
/// but an explicit override is repository authority and runs byte-for-byte
/// (see the "Explicit commands" comment at the `install_cmd` assignment), so
/// no probe ever wraps it — this classifier is the honest-failure half for
/// that lane. A bare-word command (`bun install`) names the tool it invoked;
/// a compound or path-shaped command stays generic (the streamed build log
/// already carries the shell's own `x: command not found` line naming it).
/// Either way the failure is reported as the BUILD ENVIRONMENT's missing
/// tool with the operator remedy — never as the tenant's application
/// failing, because the command text was preserved exactly and the absent
/// piece is a platform-provisioned executable. Live witness for why this
/// classification exists: tokenhun build dpl-0c9ba9d462 (2026-08-24), whose
/// explicit `bun install` died `/bin/sh: bun: command not found` on a fleet
/// where bun is provisioned on no build host, and surfaced to the tenant as
/// "install command failed: exited with exit status: 127" — an app-failure
/// shape for a platform provisioning gap.
fn classify_command_failure(context: &str, command: &str, error: anyhow::Error) -> anyhow::Error {
    let rendered = format!("{error:#}");
    let (marker, summary) = if rendered.contains("exit status: 127") {
        (
            "BUILD_TOOLCHAIN_MISSING",
            "an executable it invokes is not present in this node's build environment",
        )
    } else if rendered.contains("exit status: 42") {
        (
            "BUILD_TOOLCHAIN_MISMATCH",
            "the repository's declared toolchain version does not match this node's build environment",
        )
    } else {
        return error.context(context.to_string());
    };
    let simple = !command
        .split_whitespace()
        .any(|word| word.chars().any(|c| "&|;<>()$`\\\"'".contains(c)));
    let tool = if simple {
        command
            .split_whitespace()
            .next()
            .filter(|word| !word.contains('/'))
            .map(|word| format!(" ({word:?} is the tool it names first)"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    anyhow::anyhow!(
        "{marker}: {context}: {command:?} failed — {summary}{tool}. This is a \
         build-environment fault, not an application error. Operator remedy: \
         provision the toolchain on build-capable hosts and in the build \
         executor image; tenant remedy: choose a toolchain the builder \
         provides (node, npm, pnpm, yarn, bun)."
    )
}

async fn run_ignored_command(
    isolated: &mut IsolatedBuild,
    dir: &Path,
    command: &str,
    cloud: &Arc<CloudState>,
    bid: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<bool> {
    isolated
        .run(
            dir,
            command,
            "ignored build command",
            &[],
            true,
            cloud,
            bid,
            env,
        )
        .await
        .map(|exit| exit == 0)
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

async fn command_version(program: &Path, args: &[&str], cwd: &Path) -> String {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).env("CI", "1");
    match tokio::time::timeout(Duration::from_secs(5), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stdout.is_empty() { stderr } else { stdout }
        }
        _ => "unavailable".to_string(),
    }
}

async fn build_toolchain_identity(install_dir: &Path, pm: &str) -> String {
    if let Ok(explicit) = std::env::var("HIVE_BUILD_TOOLCHAIN_ID") {
        if !explicit.trim().is_empty() {
            return explicit;
        }
    }
    let node = preferred_node_bin()
        .map(PathBuf::from)
        .map(|dir| dir.join("node"))
        .unwrap_or_else(|| PathBuf::from("node"));
    let node_version = command_version(&node, &["--version"], install_dir).await;
    let pm_program = if pm == "npm" {
        node.parent()
            .map(|dir| dir.join("npm"))
            .filter(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from("npm"))
    } else {
        PathBuf::from(pm)
    };
    let pm_version = command_version(&pm_program, &["--version"], install_dir).await;
    format!(
        "os={};arch={};node={};pm={pm}@{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        node_version,
        pm_version,
    )
}

fn cache_hash_field(hasher: &mut sha2::Sha256, label: &str, value: &[u8]) {
    use sha2::Digest;
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

/// Cache identity binds every authority and executable-input dimension. A
/// production key cannot name a preview/fork artifact because the signed key
/// commits to the lane as well as tenant, canonical and actual repositories,
/// lock inputs, selected toolchain, and build policy.
async fn compute_cache_key(
    install_dir: &Path,
    pm: &str,
    tenant: &str,
    trust: &BuildTrustContext,
    toolchain: &str,
    policy: &str,
    build_env: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    cache_hash_field(&mut hasher, "domain", b"hive-build-cache-v3");
    cache_hash_field(&mut hasher, "tenant", tenant.as_bytes());
    cache_hash_field(
        &mut hasher,
        "canonical-repo",
        trust.canonical_repo.as_bytes(),
    );
    cache_hash_field(&mut hasher, "actual-repo", trust.actual_repo.as_bytes());
    cache_hash_field(
        &mut hasher,
        "trust-lane",
        trust.lane.cache_label().as_bytes(),
    );
    cache_hash_field(&mut hasher, "package-manager", pm.as_bytes());
    cache_hash_field(&mut hasher, "toolchain", toolchain.as_bytes());
    cache_hash_field(&mut hasher, "policy", policy.as_bytes());
    for (key, value) in build_env {
        cache_hash_field(&mut hasher, "build-env-key", key.as_bytes());
        let value_digest = Sha256::digest(value.as_bytes());
        cache_hash_field(&mut hasher, "build-env-value-sha256", &value_digest);
    }

    let mut found = false;
    for name in [
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lock",
        "bun.lockb",
        "package-lock.json",
        "package.json",
    ] {
        if let Ok(bytes) = tokio::fs::read(install_dir.join(name)).await {
            cache_hash_field(&mut hasher, name, &bytes);
            found = true;
        }
    }
    if !found {
        return None;
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
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

pub fn cache_artifact_sig(secret: &str, key: &str, bytes: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key len");
    mac.update(b"hive-build-cache-artifact-v1\0");
    mac.update(&(key.len() as u64).to_le_bytes());
    mac.update(key.as_bytes());
    mac.update(bytes);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cache_artifact_sig_valid(secret: &str, key: &str, bytes: &[u8], sig_hex: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let Some(expected) = hex_decode(sig_hex) else {
        return false;
    };
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key len");
    mac.update(b"hive-build-cache-artifact-v1\0");
    mac.update(&(key.len() as u64).to_le_bytes());
    mac.update(key.as_bytes());
    mac.update(bytes);
    mac.verify_slice(&expected).is_ok()
}

/// Verify a pulled artifact against the requested cache identity as well as its
/// content. A valid signature for key A cannot be replayed as key B.
fn verify_pulled_artifact(
    key: &str,
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
            Some(sig) if cache_artifact_sig_valid(&secret, key, bytes, sig) => {}
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
    // RACE the peers instead of walking them one at a time. This sits directly
    // in front of `npm install` on every build whose lockfile hash isn't
    // cached locally, and several fleet nodes' 8786/8787 are firewalled such
    // that a request SYN-drops and burns the whole timeout rather than failing
    // fast — serially that was up to 20s x (unreachable peers) of dead air
    // before the install could even start. Concurrently the cost is one
    // timeout total, and the whole phase is additionally capped: a cache pull
    // that is slower than a clean install is not worth waiting for.
    let per_peer = Duration::from_secs(
        std::env::var("HIVE_BUILDCACHE_PEER_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(6),
    );
    let total_budget = Duration::from_secs(
        std::env::var("HIVE_BUILDCACHE_FETCH_BUDGET_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
    );
    let attempts = peers.into_iter().map(|peer| {
        let url = format!("{}/v1/buildcache/{}", peer.trim_end_matches('/'), key);
        async move { (peer, cloud.http.get(&url).timeout(per_peer).send().await) }
    });
    let results = match tokio::time::timeout(total_budget, futures::future::join_all(attempts))
        .await
    {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(
                key = %key,
                budget_secs = total_budget.as_secs(),
                "build-cache peer fetch exceeded its budget — installing fresh instead of waiting"
            );
            return false;
        }
    };
    for (peer, resp) in results {
        if let Ok(resp) = resp {
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
                        verify_pulled_artifact(key, &bytes, sha.as_deref(), sig.as_deref())
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
    let mut tar = Command::new("tar");
    tar.args(&args).current_dir(install_dir);
    let out = run_cancellable_output(&mut tar, cloud, bid).await;
    if let Ok(o) = out {
        if o.status.success() && !cloud.build_cancels.is_cancelled(bid) {
            if tokio::fs::rename(&tmp, &final_).await.is_ok() {
                cloud.builds.log(bid, "Saved build cache for next time.");
                return;
            }
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
        CondValue, CronSpec, Header, HeaderRule, ImagesConfig, LocalPattern, Redirect,
        RemotePattern, Rewrite, RuleCondition, redirect_status,
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

/// The mount point for the automatic per-container persistent volume.
/// `project_override` — a project's dashboard-managed
/// `ContainerSettings::volume_mount_path` (see `project_settings.rs`) — wins
/// when set; falls back to the node-wide `HIVE_CONTAINER_VOLUME_PATH` env,
/// then `/data`. The per-project override exists because not every image
/// follows the `/data` convention `itzg/minecraft-server` uses, and a
/// node-wide env var would misconfigure every OTHER project on the node too.
fn container_volume_path(project_override: Option<&str>) -> String {
    if let Some(p) = project_override.map(str::trim).filter(|s| !s.is_empty()) {
        return p.to_string();
    }
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
fn container_volume_cfg(
    project: &str,
    incarnation: ProjectIncarnation,
    service: Option<&str>,
    volume_path: Option<&str>,
) -> String {
    let name = project_volume_name(project, incarnation, service);
    // TENANT/PROJECT NETWORK ISOLATION: standalone containers also get their own
    // per-project DNS-less podman network (same deterministic subnet scheme as
    // compose). Without this they land on podman's shared default bridge, where
    // ANY tenant's container can reach ANY other's by bridge IP. No static `ip`
    // is pinned (unlike compose) so scale-out instances coexist; the backend
    // assigns dynamic addresses within the project subnet.
    let (net, subnet, gw) = project_net(project);
    serde_json::json!({
        "vol": name,
        "volpath": container_volume_path(volume_path),
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
    incarnation: ProjectIncarnation,
    image: &str,
    internal: u16,
    protocol: &str,
    memory_mib: u32,
    cpus: f64,
    pids: u32,
    volume_path: Option<&str>,
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
                container_volume_cfg(project, incarnation, None, volume_path),
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
    incarnation: ProjectIncarnation,
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
    // An image deploy has no fluid.json to read a `container` override from at
    // all, so the dashboard-managed `ProjectSettings::container` is the ONLY
    // way to redirect the automatic volume's mount path here (unlike the
    // Dockerfile-build path, which also honors an explicit fluid.json value).
    let volume_path = cloud
        .projects
        .get_exact(project, incarnation)?
        .container
        .and_then(|s| s.volume_mount_path);
    log(format!(
        "Container port {port}/{protocol}{}. Attaching persistent volume (≥1 GB) at {}.",
        if port_override.is_some() || protocol_override.is_some() {
            " (configured)"
        } else {
            " (from image ExposedPorts)"
        },
        container_volume_path(volume_path.as_deref()),
    ));
    let mut manifest = container_manifest(
        project,
        incarnation,
        image,
        port,
        protocol.as_str(),
        memory_mib,
        cpus,
        pids,
        volume_path.as_deref(),
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
    incarnation: ProjectIncarnation,
    req: &GitDeployRequest,
    bid: &str,
) -> Option<String> {
    let _lifecycle = crate::project_settings::lifecycle_write(project).await;
    let settings = cloud.projects.get_exact(project, incarnation).ok()?;
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
    let info = cloud.gw.deploy_full_with_runtime_exact(
        dir.to_string_lossy().into_owned(),
        None,
        static_manifest(project, "."),
        req.creator.clone().unwrap_or_else(|| "you".into()),
        None,
        false, // not production — superseded by the real deploy when it's ready
        DeployState::Building,
        settings.team,
        incarnation,
    );
    crate::admin::causal_stamp_new_deployment(cloud, project, &info.id.0);
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
    let stale: Vec<(String, String, Option<ProjectIncarnation>)> = cloud
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
        .map(|rec| (rec.id, rec.project, rec.project_incarnation))
        .collect();
    if stale.is_empty() {
        return 0;
    }
    for (id, project, incarnation) in &stale {
        match incarnation {
            Some(incarnation) => {
                cloud.gw.remove_exact(id, *incarnation).await;
            }
            None => {
                cloud.gw.remove(id).await;
            }
        }
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

/// One project's outcome from a single `git_poll_cycle` pass — folded into the
/// cycle's aggregate counters after every project's check has run, so the
/// concurrent fan-out below never needs a shared mutable counter.
enum GitPollOutcome {
    /// Not a git source, or a never-deployed/unbound project — not counted.
    NotGit,
    /// A real git project this cycle inspected but took no further action on
    /// (empty tracked branch, HEAD unchanged, or a build already in flight).
    Polled,
    /// HEAD could not be read (branch missing, remote unreachable, auth failed).
    RemoteUnreadable,
    /// HEAD advanced past the deployed commit; a build was started.
    Deployed,
}

/// Concurrent `git_poll_one` fan-out width, per cycle. Unlike the fleet/peer
/// fan-outs this pattern was borrowed from (`vercel_dns.rs`, `gpu_pool.rs`,
/// `admin.rs` — all bounded by the ~14-node fleet roster), the input here is
/// `cloud.projects` — platform-wide and, on the "enterprise" plan tier,
/// literally unbounded per tenant (`billing.rs`'s `plan_max_projects`). An
/// unbounded `join_all` over that set forks one OS `git ls-remote` subprocess
/// PER PROJECT simultaneously on the single control-plane leader every tick —
/// real FD/process-table/socket pressure that scales with tenant-controlled
/// project count, not with anything this node's own resources bound.
/// `buffer_unordered` keeps the "one slow remote can't block another" property
/// the unbounded version was written for, while capping how many `git`
/// subprocesses exist at once; `HIVE_GIT_POLL_CONCURRENCY` overrides the
/// default for a fleet running an unusually large git-sourced project count.
fn git_poll_concurrency() -> usize {
    std::env::var("HIVE_GIT_POLL_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(16)
}

/// One poll pass over every git-sourced project. Never panics: a per-project
/// failure (unreachable remote, missing branch, auth failure) is logged at debug
/// and skipped so it can't wedge the other projects or the loop.
///
/// Each project's check does its own network round trip (`git ls-remote`, up to
/// a 20s timeout) before deciding whether to deploy. A bounded concurrent
/// fan-out (see [`git_poll_concurrency`]) means one project's stale/slow check
/// never blocks another's — while capping the simultaneous `git` subprocess
/// count regardless of how many git-sourced projects the platform has.
async fn git_poll_cycle(cloud: &Arc<CloudState>) {
    let projects: Vec<String> = cloud.projects.snapshot().into_keys().collect();
    let outcomes: Vec<GitPollOutcome> =
        futures::stream::iter(projects.into_iter().map(|project| {
            let cloud = Arc::clone(cloud);
            async move { git_poll_one(&cloud, project).await }
        }))
        .buffer_unordered(git_poll_concurrency())
        .collect()
        .await;

    let mut n_git = 0u32; // git-sourced projects actually polled this cycle
    let mut n_deployed = 0u32; // projects whose HEAD advanced -> build started
    let mut n_skipped = 0u32; // git projects whose remote HEAD could not be read
    for outcome in outcomes {
        match outcome {
            GitPollOutcome::NotGit => {}
            GitPollOutcome::Polled => n_git += 1,
            GitPollOutcome::RemoteUnreadable => {
                n_git += 1;
                n_skipped += 1;
            }
            GitPollOutcome::Deployed => {
                n_git += 1;
                n_deployed += 1;
            }
        }
    }
    // INFO when anything was skipped, so a fleet at RUST_LOG=info sees that
    // auto-deploy is silently not running for some projects; debug otherwise, to
    // keep a healthy cycle quiet.
    if n_skipped > 0 {
        tracing::info!(
            git_projects = n_git,
            deployed = n_deployed,
            unreadable = n_skipped,
            "git_poll: cycle complete — {n_skipped} project(s) could not be polled (see the \
             per-project warnings above); auto-deploy is inert for those"
        );
    } else {
        tracing::debug!(
            git_projects = n_git,
            deployed = n_deployed,
            "git_poll: cycle complete"
        );
    }
}

/// One project's check-and-maybe-deploy, factored out of `git_poll_cycle` so it
/// can run concurrently with every other project's — every side effect
/// (`git_poll_seen` dedup, the already-building guard, `start_build`) is
/// unchanged from the prior serial loop, just scoped to a single project.
async fn git_poll_one(cloud: &Arc<CloudState>, project: String) -> GitPollOutcome {
    // Fleet-aware source; skip non-git (zip `upload://`, image `image://`)
    // and never-deployed / unbound projects.
    let Some(src) = crate::admin::git_for_project_fleet(cloud, &project) else {
        return GitPollOutcome::NotGit;
    };
    if !src.is_real_git() {
        return GitPollOutcome::NotGit;
    }
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
        return GitPollOutcome::Polled;
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
        //
        // WARN, not debug and not silent. This arm is the single point where
        // "my pushes stopped deploying" becomes invisible: it swallows an
        // auth failure identically to a renamed branch, and the fleet runs
        // RUST_LOG=info so a debug line would not be read even if one
        // existed (the function's own doc claimed one that was never
        // written). The token-presence flag is the actionable half — with no
        // App private key and no GITHUB_TOKEN on a node, EVERY private repo
        // lands here forever, and without this line nothing anywhere says so.
        _ => {
            tracing::warn!(
                project = %project,
                branch = %branch,
                had_token = token.is_some(),
                "git poll: could not read the remote HEAD — branch missing, remote \
                 unreachable, or auth failed. Auto-deploy for this project is INERT \
                 until it resolves. A private repo with had_token=false has no \
                 credential on this node (GitHub App private key or GITHUB_TOKEN)."
            );
            return GitPollOutcome::RemoteUnreadable;
        }
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
        return GitPollOutcome::Polled;
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
        return GitPollOutcome::Polled;
    }

    let root_dir = Some(cloud.projects.root_dir_of(&project)).filter(|s| !s.is_empty());
    let req = GitDeployRequest {
        source_deployment_ids: Vec::new(),
        repo_url: src.repo_url.clone(),
        branch: Some(branch.clone()).filter(|b| !b.is_empty()),
        // Pin the EXACT polled SHA (same race protection as the webhook path).
        commit: Some(head.clone()),
        head_repo_url: None, // polling only ever tracks the project's own repo
        project: Some(project.clone()),
        project_incarnation: None,
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
        marketplace_placement: None,
    };
    let build_id = match start_build(cloud.clone(), req, None, None).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                project = %project,
                repo = %src.repo_url,
                error = %e,
                "git_poll: start_build rejected the poll-triggered deploy — retrying next cycle"
            );
            return GitPollOutcome::RemoteUnreadable;
        }
    };
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
    GitPollOutcome::Deployed
}

/// True when two commit strings refer to the same commit, tolerant of one being
/// an abbreviated prefix of the other (a deployment record may store a short
/// SHA while `ls-remote` returns the full 40-char one). Empty never matches.
///
/// `pub(crate)`: `admin::git_webhook` reuses this for its own already-building
/// dedup, matching `git_poll_cycle`'s.
pub(crate) fn commit_eq(a: &str, b: &str) -> bool {
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
    // Same FD-backed credential helper as the clone path — never a token
    // embedded in the URL (argv-visible), and only ever offered for
    // github.com (the helper itself is scoped there; `token` is already
    // resolved only for github.com callers, see `resolve_git_poll_token`).
    let mut cmd = Command::new("git");
    let _cred = apply_credential(&mut cmd, token).ok()?;
    cmd.arg("ls-remote")
        .arg(repo_url)
        .arg(format!("refs/heads/{branch}"))
        .env("GIT_TERMINAL_PROMPT", "0") // never block on an interactive prompt
        // Own the process group and reap on drop — `tokio::time::timeout` below
        // drops (not cancels-and-waits) this future on expiry, and without
        // these two the underlying `git` process would be orphaned rather than
        // killed, silently leaking a hung process per timed-out poll.
        .process_group(0)
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output())
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
            match crate::github_app_auth::installation_token_for_repo(owner, repo).await {
                Ok(Some(tok)) => return Some(tok),
                // Name the failure instead of swallowing it: a working App key
                // with the App simply NOT INSTALLED on the repo's org reads
                // identically to "no credential configured" downstream
                // (had_token=false), which mis-steered a real diagnosis toward
                // installing keys that already existed. Live-witnessed: App
                // 4658598 valid, installed on dywongcloud only —
                // numo-gg/numo0 404s until that org installs the App.
                Ok(None) => {
                    static REPORTED: std::sync::OnceLock<
                        parking_lot::Mutex<std::collections::HashSet<String>>,
                    > = std::sync::OnceLock::new();
                    let repo_key = format!("{owner}/{repo}");
                    if REPORTED
                        .get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()))
                        .lock()
                        .insert(repo_key)
                    {
                        tracing::warn!(
                            owner,
                            repo,
                            "GitHub App is configured but NOT INSTALLED on this repo's \
                             owner — install the App on the org (Settings → GitHub Apps) \
                             or set GITHUB_TOKEN"
                        );
                    }
                }
                Err(e) => tracing::warn!(
                    owner,
                    repo,
                    error = %e,
                    "GitHub App installation-token mint failed (key/parse/API)"
                ),
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
        let incarnation = fluid_core::ProjectIncarnation::mint();
        let m = container_manifest(
            "my-proj",
            incarnation,
            "fruitbox12/simplifi:latest",
            8080,
            "http",
            0,
            0.0,
            0,
            None,
        );
        let f = &m.functions[0];
        assert_eq!(f.runtime, "container");
        assert_eq!(f.start_cmd[0], "__container__");
        assert_eq!(f.start_cmd[1], "fruitbox12/simplifi:latest");
        assert_eq!(f.start_cmd[2], "8080");
        let cfg: serde_json::Value = serde_json::from_str(&f.start_cmd[3]).unwrap();
        assert_eq!(cfg["vol"], format!("hive-vol-my-proj-{incarnation}"));
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
        assert!(
            adapter_manifest("p", "opennext", &dir, None)
                .await
                .is_none()
        );
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

        let root_is_workspace = crate::workspace::load(&repo_root)
            .await
            .expect("workspace manifest must parse")
            .is_some();
        assert!(
            root_is_workspace,
            "root package.json has a \"workspaces\" field"
        );
        let workspace_member = crate::app_discovery::is_member(&repo_root, &api_dir)
            .await
            .expect("membership check must not error");
        assert!(
            workspace_member,
            "packages/api must be recognized as a workspace member"
        );
        let is_monorepo = api_dir != repo_root && workspace_member && root_is_workspace;
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
        let incarnation = fluid_core::ProjectIncarnation::mint();
        let m = container_manifest(
            "proj",
            incarnation,
            "img:tag",
            50051,
            "grpc",
            0,
            0.0,
            0,
            None,
        );
        assert_eq!(m.functions.len(), 1);
        assert_eq!(m.functions[0].protocol_or_http(), "grpc");
        assert!(m.functions[0].needs_raw_proxy());
        assert_eq!(
            &m.functions[0].start_cmd[..3],
            &["__container__", "img:tag", "50051"]
        );
        // start_cmd[3] now carries the automatic persistent-volume run-config.
        let cfg: serde_json::Value = serde_json::from_str(&m.functions[0].start_cmd[3]).unwrap();
        assert_eq!(cfg["vol"], format!("hive-vol-proj-{incarnation}"));
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
        let m = container_manifest(
            "proj",
            fluid_core::ProjectIncarnation::mint(),
            "img:tag",
            8080,
            "http",
            4096,
            0.0,
            0,
            None,
        );
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
        let m = container_manifest(
            "proj",
            fluid_core::ProjectIncarnation::mint(),
            "img:tag",
            8080,
            "http",
            0,
            4.0,
            2048,
            None,
        );
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
        let key = "npm:team:acme:abc123";
        let bytes = b"the artifact";
        let good_sha = artifact_sha256(bytes);

        // --- No secret configured (dev / single node) ---
        std::env::remove_var("HIVE_ARTIFACT_SECRET");
        std::env::remove_var("HIVE_JWT_SECRET");
        assert!(
            verify_pulled_artifact(key, bytes, Some(&good_sha), None).is_ok(),
            "good digest accepted"
        );
        assert!(
            verify_pulled_artifact(key, bytes, Some("deadbeef"), None).is_err(),
            "corruption rejected"
        );
        assert!(
            verify_pulled_artifact(key, bytes, None, None).is_ok(),
            "legacy peer accepted in dev"
        );

        // --- Fleet secret configured (production) ---
        std::env::set_var("HIVE_ARTIFACT_SECRET", "s3cret");
        let sig = cache_artifact_sig("s3cret", key, bytes);
        assert!(
            verify_pulled_artifact(key, bytes, Some(&good_sha), Some(&sig)).is_ok(),
            "valid sha+sig accepted"
        );
        assert!(
            verify_pulled_artifact(key, bytes, Some(&good_sha), None).is_err(),
            "missing sig rejected when secret set"
        );
        let forged = cache_artifact_sig("wrong", key, bytes);
        assert!(
            verify_pulled_artifact(key, bytes, Some(&good_sha), Some(&forged)).is_err(),
            "forged sig rejected"
        );
        // A signature minted for a different cache key must not verify under this one.
        let other_key_sig = cache_artifact_sig("s3cret", "npm:team:other:abc123", bytes);
        assert!(
            verify_pulled_artifact(key, bytes, Some(&good_sha), Some(&other_key_sig)).is_err(),
            "signature bound to a different key must not be replayable"
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
        assert!(
            sanitize_tag("Foo/Bar:Baz")
                .chars()
                .all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || c == '.'
                    || c == '_'
                    || c == '-')
        );
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

        let trust = BuildTrustContext {
            lane: BuildTrustLane::Production,
            canonical_repo: "github.com/acme/app".into(),
            actual_repo: "github.com/acme/app".into(),
        };
        let build_env: std::collections::BTreeMap<String, String> = Default::default();

        let k1 = compute_cache_key(
            &base,
            "npm",
            "team:acme",
            &trust,
            "node-22",
            "prod",
            &build_env,
        )
        .await;
        let k2 = compute_cache_key(
            &base,
            "npm",
            "team:acme",
            &trust,
            "node-22",
            "prod",
            &build_env,
        )
        .await;
        assert!(k1.is_some());
        assert_eq!(k1, k2, "same lockfile+pm must yield the same key");

        // Different package manager → different key.
        let k_pnpm = compute_cache_key(
            &base,
            "pnpm",
            "team:acme",
            &trust,
            "node-22",
            "prod",
            &build_env,
        )
        .await;
        assert_ne!(k1, k_pnpm);

        // Changed lockfile → different key.
        tokio::fs::write(base.join("package-lock.json"), b"{\"v\":2}")
            .await
            .unwrap();
        let k3 = compute_cache_key(
            &base,
            "npm",
            "team:acme",
            &trust,
            "node-22",
            "prod",
            &build_env,
        )
        .await;
        assert_ne!(k1, k3, "changed lockfile must change the key");

        // TENANT SCOPING: identical lockfile + package manager must NOT collide
        // across tenants — the cache restores content a previous build could
        // write, so a shared key is a cross-tenant code path.
        let k_other = compute_cache_key(
            &base,
            "npm",
            "team:other",
            &trust,
            "node-22",
            "prod",
            &build_env,
        )
        .await;
        assert_ne!(
            k3, k_other,
            "the same lockfile must not share a key across tenants"
        );

        // Different trust lane (e.g. a preview fork) must not share a key.
        let fork_trust = BuildTrustContext {
            lane: BuildTrustLane::PreviewFork,
            ..trust.clone()
        };
        let k_fork = compute_cache_key(
            &base,
            "npm",
            "team:acme",
            &fork_trust,
            "node-22",
            "prod",
            &build_env,
        )
        .await;
        assert_ne!(
            k3, k_fork,
            "a preview-fork build must not share a cache key with a production build"
        );

        // No lockfile/package.json → None.
        let empty = std::env::temp_dir().join(format!("oe-cachekey-empty-{}", now_ms()));
        tokio::fs::create_dir_all(&empty).await.unwrap();
        assert_eq!(
            compute_cache_key(
                &empty,
                "npm",
                "team:acme",
                &trust,
                "node-22",
                "prod",
                &build_env
            )
            .await,
            None
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
        let _ = tokio::fs::remove_dir_all(&empty).await;
    }

    #[test]
    fn build_store_insert_log_update() {
        let store = BuildStore::new();
        store.insert(Build {
            id: "dpl-test".into(),
            project: "demo".into(),
            project_incarnation: None,
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
