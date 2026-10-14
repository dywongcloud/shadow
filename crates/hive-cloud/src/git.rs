//! Deploy from a git repository with a live build log: `start_build` creates the build record and
//! returns its id at once, a background task clones, logs timestamped lines (region, machine
//! config, install/build commands, ready) and registers the routable deployment via the gateway.
//! The dashboard polls `GET /v1/builds/:id`.

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
    /// Set when a NEWER build of the same project started mid-flight: `run_build` checks it just
    /// before `deploy_full` and vetoes its own production flip, so the last flip is the newest
    /// push, never merely the last-finishing build (latest-push-wins).
    #[serde(default)]
    pub superseded_by: Option<String>,
    /// Structured reason a build reached `Error`, so callers need not parse `lines` for it;
    /// `#[serde(default)]` keeps older persisted records deserializing.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_build: Option<fluid_build::RepositoryBuildSnapshot>,
    #[serde(default)]
    pub lines: Vec<LogLine>,
}

/// Newest builds (by `started_ms`) the snapshot persists: bounds the state file while covering
/// every deployment a user can realistically open.
const SNAPSHOT_BUILD_CAP: usize = 200;

/// Builds retained IN MEMORY: bounds the live map of a node that builds continuously, set above
/// `SNAPSHOT_BUILD_CAP` so nothing that would be persisted is evicted first.
const MEMORY_BUILD_CAP: usize = 400;

/// Bytes retained for ONE log line. Every build-output reader already caps what it takes off the
/// pipe (the host lane through `hive_core::logcap`, the isolated executor through its own
/// `read_capped_line`); this bound sits at the store boundary too, because `log()` also takes
/// internally formatted messages (command echoes, error text embedding subprocess output) and no
/// future caller may reintroduce an unbounded line.
const MAX_BUILD_LOG_LINE_BYTES: usize = 16 * 1024;

/// Fanout capability boundary for paired host-static/runtime-artifact semantics.
/// Its location deliberately cannot match the legacy mesh prefix arm.
const RUNTIME_ARTIFACT_FANOUT_PATH: &str = "/v1/runtime-artifact/v1/git/deploy";

const CANCEL_GRACE_POLLS: u32 = 20;
const CANCEL_GRACE_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    /// Persistable view: the newest `SNAPSHOT_BUILD_CAP` builds.
    pub fn snapshot(&self) -> Vec<Build> {
        let mut v: Vec<Build> = self.map.lock().values().cloned().collect();
        v.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
        v.truncate(SNAPSHOT_BUILD_CAP);
        v
    }
    /// Boot-time restore: a build persisted mid-flight (`Queued`/`Building`) lost its process in
    /// the restart, so it is finalized as `Error` with an explanatory log line instead of
    /// presenting as running forever.
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
    /// Removes the build records proved to belong to one deleted incarnation; legacy records carry
    /// no identity and are retained rather than guessed from a project-name prefix.
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

/// Where a MIRRORED build's real process lives: the "pure remote placement" fanout branch of
/// `run_build` dispatches the real build to a peer, under its OWN build id, and mirrors only its
/// logs/state here (`mirror_remote_build`), so cancelling the mirror must also cancel that process.
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

/// In-flight builds' cancellation handles by build id (`cloud.build_cancels`), deliberately NOT
/// persisted or gossiped: they describe live local OS processes, which cannot survive a restart
/// (`BuildStore::load` finalizes Queued/Building records to `Error` on boot). Every long-running
/// step publishes its process GROUP id here (`run_cancellable_output`, `run_streamed`) so `cancel`
/// can SIGKILL the whole tree, not one pid.
#[derive(Default)]
pub struct BuildCancelRegistry {
    map: Mutex<HashMap<String, Arc<BuildCancelSlot>>>,
}

impl BuildCancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a fresh slot before the driving task is spawned; returning the exact Arc lets the
    /// task's Drop guard remove only its own slot, never a later replacement that reused the build
    /// id.
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

    /// Whether `bid` was asked to cancel: checked at step boundaries in `run_build`, so a build
    /// already mid-cancel never starts a fresh step.
    pub fn is_cancelled(&self, bid: &str) -> bool {
        self.map
            .lock()
            .get(bid)
            .is_some_and(|s| s.cancelled.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Publishes (or clears, `None`) the process-GROUP id THIS build is blocked on: set right after
    /// spawn and cleared when the command exits, so a later step never targets a stale or reused
    /// pid.
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

    /// Cancels an in-flight build: flags it, SIGKILLs the process GROUP it is blocked on, asks its
    /// mirror target (if any) to cancel the REAL remote build, then gives the driving task a grace
    /// window to unwind NORMALLY (running its own cleanup: a first-deploy placeholder, `forget`ting
    /// this slot) before force-aborting it, since abort skips all remaining cleanup. Completion is
    /// owned by the task's Drop guard, so the slot is never erased here: a deletion may hold the
    /// Arc and must see the task and every checkout guard unwind before reclaiming source. Returns
    /// `false` when `bid` has no live slot.
    pub async fn cancel(&self, cloud: &Arc<CloudState>, bid: &str) -> bool {
        let slot = match self.slot(bid) {
            Some(s) => s,
            None => return false,
        };
        slot.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let pgid = *slot.pgid.lock();
        if let Some(pg) = pgid {
            let whole_group = format!("-{pg}");
            let _ = tokio::process::Command::new("kill")
                .arg("-KILL")
                .arg(whole_group)
                .output()
                .await;
        }
        let mirror = slot.mirror.lock().clone();
        if let Some(m) = mirror {
            let _ = cancel_remote_build(cloud, &m).await;
        }
        let task = slot.task.lock().clone();
        if let Some(task) = task {
            for _ in 0..CANCEL_GRACE_POLLS {
                if task.is_finished() {
                    break;
                }
                tokio::time::sleep(CANCEL_GRACE_POLL_INTERVAL).await;
            }
            if !task.is_finished() {
                task.abort();
            }
        }
        let _ =
            Self::wait_completed(&slot, tokio::time::Instant::now() + Duration::from_secs(2)).await;
        true
    }

    /// Cancels every active build of one exact incarnation and waits for their Drop-owned
    /// completion; the returned residual ids are observable cleanup failures, so callers must not
    /// reclaim source while it is non-empty.
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

/// Every exit of a build task (return, `?`, cancel abort, panic) passes here, so a build record is
/// guaranteed a terminal state and the project row learns the outcome.
impl Drop for BuildCompletionGuard {
    fn drop(&mut self) {
        let cancelled = self
            .slot
            .cancelled
            .load(std::sync::atomic::Ordering::SeqCst);
        let mut outcome = None;
        self.cloud.builds.update(&self.bid, |b| {
            if matches!(b.state, DeployState::Queued | DeployState::Building) {
                b.state = if cancelled {
                    DeployState::Cancelled
                } else {
                    DeployState::Error
                };
                b.finished_ms.get_or_insert_with(now_ms);
                if !cancelled {
                    let msg = "build ended without reporting a result";
                    b.error.get_or_insert_with(|| msg.into());
                    b.lines.push(LogLine {
                        ts_ms: now_ms(),
                        line: msg.into(),
                    });
                }
            }
            outcome = crate::project_settings::LastBuild::from_build(b);
        });
        if let Some(outcome) = outcome {
            if self.cloud.leader_forward_target().is_none() {
                let _ = self.cloud.projects.record_last_build_exact(
                    &self.slot.project,
                    self.slot.incarnation,
                    outcome,
                );
                crate::persist::persist(&self.cloud);
            }
        }
        self.cloud.build_cancels.complete(&self.bid, &self.slot);
    }
}

/// Asks a remote node to cancel the build it is actually running (the target of a
/// mirrored/fanned-out build) over the same transport as `fanout_remote`/`mirror_remote_build`;
/// best-effort, the local kill and the cancelled flag stay in effect when it fails.
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

/// Like `Command::output()`, but drops `cmd` into its OWN process group and publishes it to the
/// per-build cancel registry, cleared once the command exits, so `cancel` can SIGKILL the whole
/// tree if the user cancels while THIS command is hung. Used by the git clone/fetch/checkout chain;
/// `run_streamed` does the equivalent inline.
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
    let cancel_landed_before_spawn = cloud.build_cancels.is_cancelled(bid);
    if cancel_landed_before_spawn {
        let _ = child.start_kill();
    }
    let out = child.wait_with_output().await;
    cloud.build_cancels.set_running(bid, None);
    out
}

/// Feeds a GitHub token to a git subprocess without it ever touching the command line, an
/// environment variable or `.git/config`: a FIXED `credential.helper` one-liner reads
/// `username=...\npassword=...\n` off an inherited fd backed by a 0600 temp file that is unlinked
/// at once (readable fd, no path, root-owned) with FD_CLOEXEC cleared so it survives git's
/// `exec()`. `-c credential.helper=` comes FIRST to clear any inherited helper (a base image's
/// keychain/store entry would otherwise be consulted ahead of ours and leak an unrelated stored
/// credential into a tenant's build) and the helper is scoped to
/// `credential.https://github.com.helper`, matching the github.com-only token resolvers
/// (`resolve_git_poll_token`, `git_webhook`). Callers MUST keep the returned `File` alive until
/// `Command::spawn` returns: `fork()` copies the fd table first, so the child already holds its own
/// reference.
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

/// Applies credential config to a git subprocess: always clears any inherited `credential.helper`
/// first (closing the ambient-credential surface even for anonymous clones), then wires the fixed
/// fd-backed helper when a token is present. Returns the `File` the caller must keep alive until
/// the command has been spawned.
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

/// Marker error: `run_build` returned `Err` because the user cancelled, not because a step failed,
/// so the driving task sets `DeployState::Cancelled` instead of `Error` without string-matching the
/// message.
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

/// Current-incarnation volume names the server-authored container run configuration will actually
/// hand to podman; a stored legacy or malformed configuration is not cleanup authority (callers
/// reject registration or retain it as an observable cleanup residual).
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

/// Project name from a repo URL; for an `image://` ref it is the image NAME without registry,
/// namespace or `:tag`, so a tag change does not fork the project.
pub fn project_name_from_url(url: &str) -> String {
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

/// Durable deployment root `$HIVE_DATA/deploys`: the mock backend serves files straight from a
/// deployment's `root`, so it must outlive a host reboot exactly as long as the replicated
/// deployment RECORD does (under `/tmp` a reboot wiped the checkout and the node 404'd
/// `DEPLOYMENT_NOT_FOUND`). Roots under [`legacy_deploy_root`] keep working untouched until their
/// host reboots, so the lookup, purge and GC paths all fall back to it.
pub(crate) fn deploy_root() -> PathBuf {
    crate::persist::data_dir().join("deploys")
}

/// The pre-durability root (`$TMPDIR/hive-deploys`): nothing writes here anymore, it remains a read
/// fallback (retained source for redeploys) and a GC/purge target until every pre-change checkout
/// has aged out or rebooted away.
pub(crate) fn legacy_deploy_root() -> PathBuf {
    std::env::temp_dir().join("hive-deploys")
}

/// A project's on-disk name component under the deploy root: tenant-controlled text is NEVER
/// interpolated into a path verbatim, `sanitize_tag` maps it to `[a-z0-9._-]` (the `hive-vol-`
/// discipline), so `..` or `a/b` cannot escape the root. Pre-sanitization checkouts used the raw
/// name; readers that must still find them take both prefixes (`checkout_prefixes`).
fn checkout_tag(project: &str) -> String {
    sanitize_tag(project)
}

/// Dir-name prefixes a project's checkouts can carry: the sanitized component plus the raw project
/// name (pre-sanitization dirs), so redeploy, GC and purge see checkouts written by older binaries.
pub(crate) fn checkout_prefixes(project: &str) -> Vec<String> {
    let tag = checkout_tag(project);
    let mut v = vec![format!("{tag}-")];
    let raw = format!("{project}-");
    if !v.contains(&raw) {
        v.push(raw);
    }
    v
}

/// Checkout directories being created or read by a build: GC and the source selector share this one
/// lock, so either a build owns a directory or the reaper owns its deletion, never both; counts
/// keep nested reads honest without a premature release.
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

/// Waits until every checkout reader/writer of one deleted incarnation has released its Drop-owned
/// reservation; the bounded error names the paths still owned, so deletion retains and reports them
/// rather than racing a reader or claiming complete cleanup.
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

/// The newest on-disk deploy checkout of a project (`<deploy_root>/<tag>-*`, durable and legacy
/// roots), preferring a completed one with a package.json/Dockerfile: lets a node that holds the
/// SOURCE derive the service graph without a deployment RECORD (container deploys register the
/// record on the coordinator and run on the lease owner). None when no source is on disk.
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
            let lacks_build_input = !p.join("package.json").exists()
                && !p.join("Dockerfile").exists()
                && !p.join("Containerfile").exists();
            if lacks_build_input {
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

/// [`newest_deploy_dir`] restricted to checkouts of specific BUILD ids (dirs are named
/// `{tag}-{ms}-{build_id}`); an empty allowlist is never an upload-redeploy wildcard, absent
/// provenance fails closed.
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
            let lacks_build_input = !p.join("package.json").exists()
                && !p.join("Dockerfile").exists()
                && !p.join("Containerfile").exists();
            if lacks_build_input {
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

/// Selects and pins retained source under the same lock the reaper takes before deletion, so the
/// selector never hands `copy_dir_into` a path GC already decided to remove, nor can GC claim it
/// after selection.
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

/// Whether `project` already has a deployment ANYWHERE in the fleet (local gateway or gossiped
/// `peer_deployments`), which tells a FIRST deploy from a REDEPLOY: for a remotely-placed project a
/// local-only `serves_host` check would treat every redeploy as a first deploy, spawning a phantom
/// "Building…" placeholder and forcing `npm ci`.
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

/// Claims and removes one stale candidate under the lock that registers active checkouts: a
/// completed build registers its deployment before its guard drops, so checking active owners first
/// and deployment roots second leaves no gap where neither protects the directory.
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

/// Self-management GC: reaps stale clone/build working dirs (`<root>/<tag>-<stamp>-<bid>`,
/// `-building-<ms>-<bid>`; never removed after a build) untouched for longer than `max_age`, UNLESS
/// the dir is the `root` of a LIVE deployment (the mock backend serves straight from `root` and the
/// restart restore keys off it existing). Sweeps the durable and the legacy root; best-effort,
/// returns the number removed.
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

/// Releases the `Building…` placeholder's hold on the project's host alias even when the build task
/// never finishes: `Drop` runs when the task is ABORTED or panics, the cases that used to leak it.
/// Removal is async, so the guard hands it to a detached task; outside a runtime (process teardown)
/// the boot reconciler and the reaper reap the same shells.
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

/// Starts a build and returns its id once its lifecycle reservations exist. `expected_incarnation`
/// is authority passed separately by the authenticated fanout receiver; the identically-named
/// request field is never trusted here.
pub async fn start_build(
    cloud: Arc<CloudState>,
    mut req: GitDeployRequest,
    expected_incarnation: Option<ProjectIncarnation>,
    admission_team: Option<String>,
) -> anyhow::Result<String> {
    validate_deploy_source(&req)?;
    let id = format!("dpl-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let project = req
        .project
        .clone()
        .unwrap_or_else(|| project_name_from_url(&req.repo_url));
    req.project_incarnation = None;
    drop_git_token_for_non_git_source(&mut req);

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
        cloud
            .projects
            .claim_team_exact(&project, incarnation, team)
            .map_err(|error| match error {
                crate::project_settings::ProjectClaimError::OwnedByOther => {
                    anyhow::anyhow!("project {project} belongs to a different team")
                }
                crate::project_settings::ProjectClaimError::Incarnation(error) => error.into(),
            })?;
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
    let started_ms = now_ms();
    if cloud.leader_forward_target().is_none() {
        let _ = cloud
            .projects
            .record_build_started_exact(&project, incarnation, &id, started_ms);
    }
    crate::admin::adopt_authorized_legacy_deployments(
        &cloud,
        &project,
        incarnation,
        legacy_deployment_ids,
    )?;
    cloud.git_index.set_project_repo(&project, &req.repo_url);
    req.project_incarnation = Some(incarnation);

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
        repository_build: None,
        lines: Vec::new(),
    });
    crate::persist::persist(&cloud);
    let wh_team = cloud.projects.team_of(&project);
    crate::webhooks::dispatch(
        &cloud.webhooks,
        &wh_team,
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
        let result = run_build(&cloud, &bid, req, project, incarnation, first_deploy).await;
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
                    &wh_team,
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

/// Whether THIS node holds retained source that can rebuild one of `ids` (platform-issued BUILD ids
/// stamped into upload `GitSource.commit`); empty or legacy provenance fails closed instead of
/// selecting a different lane.
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

/// Durable retained source for one upload lineage: the build id is platform-issued and validated
/// before it becomes a path component, so previews and production uploads never overwrite one
/// project-wide archive.
/// Stored as a FILE beside the checkouts: `gc_build_dirs` only reaps directories, so it survives
/// build-dir GC and host reboots; written via tmp + rename so readers never see a torn zip.
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

/// Resolves a Node SERVER entry for a framework app with no handler-shaped file: `package.json`
/// (`main`/`module`/`exports`/`scripts.start` naming a JS file) and the built server outputs of the
/// detected frameworks. Returns a deployment-root-relative path (the shape `infer_browser_entry`
/// returns); only real, existing files are returned, so an unbuilt framework directory is not an
/// entry.
fn infer_package_server_entry(root: &Path) -> Option<String> {
    let is_js = |p: &str| {
        let lower = p.to_ascii_lowercase();
        lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs")
    };
    let exists = |rel: &str| root.join(rel).is_file();
    const BUILT_SERVER_ENTRIES: [&str; 8] = [
        ".next/standalone/server.js",
        "build/index.js",
        "build/server/index.js",
        "dist/server/entry.mjs",
        "dist/server/server.js",
        ".output/server/index.mjs",
        "server.js",
        "app.js",
    ];
    for rel in BUILT_SERVER_ENTRIES {
        if exists(rel) {
            return Some(rel.to_string());
        }
    }
    let raw = std::fs::read_to_string(root.join("package.json")).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let mut candidates: Vec<String> = Vec::new();
    for key in ["main", "module"] {
        if let Some(value) = parsed.get(key).and_then(|v| v.as_str()) {
            if is_js(value) {
                candidates.push(value.trim_start_matches("./").to_string());
            }
        }
    }
    if let Some(value) = parsed.get("exports").and_then(|v| v.as_str()) {
        if is_js(value) {
            candidates.push(value.trim_start_matches("./").to_string());
        }
    }
    if let Some(start) = parsed
        .get("scripts")
        .and_then(|s| s.get("start"))
        .and_then(|v| v.as_str())
    {
        for token in start.split_whitespace() {
            if is_js(token) {
                candidates.push(token.trim_start_matches("./").to_string());
                break;
            }
        }
    }
    candidates.into_iter().find(|rel| exists(rel))
}

/// Finds a browser-handler file for a function that did NOT opt in via fluid.json (automatic
/// browser eligibility): `<fn>.browser.*` and `browser.*` first, then the function's own plausible
/// entries (`<fn>.handler.*`, `<fn>.*`, `handler.*`, `index.*`, `main.*`), first existing match
/// wins. Probing a function's own entry is safe because `bundle()` gates at build time
/// (`handler_export_present` drops server entries that assign no handler, the
/// unimplementable-surface scan drops machine code); a synthesized candidate that fails to bundle
/// is skipped silently and only an explicit fluid.json opt-in fails the build. `start_cmd` is never
/// parsed for an entry, and `bundle()`'s `resolve_entry` keeps the final path inside the deployment
/// root.
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

/// The ONE platform-generated browser entry adapter (full contract in
/// `assets/browser-adapter.cjs`): a CommonJS module assigning `module.exports` to one
/// `async (request, ops)` function, so a function with no handler-shaped file still passes
/// `bundle()`'s handler-export gate. The server it adapts is EMBEDDED at build time and the
/// remaining candidates are probed with guarded RELATIVE `require`s (the artifact's own directory
/// is the base). It adds no capability (no host path, port, fork or socket), evaluates the embedded
/// server EXACTLY ONCE (promise memoized on `globalThis`), answers `require("http")` with an honest
/// stub (`listen()` calls back and emits `'listening'`) and makes `res.statusCode`/`statusMessage`
/// accessors so `res.statusCode = 404` reaches the envelope.
const BROWSER_ADAPTER_JS: &str = include_str!("../assets/browser-adapter.cjs");

/// Platform-reserved name of the generated entry in a deployment root: dot-prefixed and
/// function-suffixed so two functions never collide and a repository file is never mistaken for it.
const BROWSER_ADAPTER_FILE_PREFIX: &str = ".hive-browser-entry-";
const BROWSER_ADAPTER_FILE_SUFFIX: &str = ".cjs";

/// Hard bound on a server source embedded into the generated adapter, checked BEFORE the entry
/// string is built rather than discovered as a rejected bundle.
const BROWSER_ADAPTER_EMBED_MAX_BYTES: usize =
    fluid_core::BROWSER_ENTRY_MAX_SOURCE_BYTES - BROWSER_ADAPTER_HEADROOM_BYTES;
const BROWSER_ADAPTER_HEADROOM_BYTES: usize = 64 * 1024;

/// The framework server-entry probe table in one deterministic order, deliberately WIDER than
/// `infer_package_server_entry`'s list: it decides which server the generated adapter embeds and
/// what it probes at runtime, so an omitted output serves nothing in browsers.
const BROWSER_SERVER_PROBES: &[&str] = &[
    ".next/standalone/server.js",
    ".next/server/index.js",
    ".output/server/index.mjs",
    ".output/server/index.js",
    ".nuxt/dist/server/index.mjs",
    ".svelte-kit/output/server/index.js",
    "build/server/index.js",
    "dist/server/entry.mjs",
    "dist/server/server.js",
    "dist/server/index.js",
    "dist/server/main.js",
    "build/index.js",
    "build/main.js",
    "build/server.js",
    "build/app.js",
    "build/server/main.js",
    "dist/index.js",
    "dist/main.js",
    "dist/server.js",
    "dist/app.js",
    "out/index.js",
    "output/index.js",
    "server/index.js",
    "server/main.js",
    "server.js",
    "app.js",
    "main.js",
    "index.js",
];

/// A probe/target path as a `require` SPECIFIER: `./`-prefixed, never absolute. The artifact sits
/// at the root of the guest filesystem with its own directory as the `require` base, and a BARE
/// specifier resolves through `node_modules`, which a one-file artifact never has (every probe a
/// guaranteed MODULE_NOT_FOUND); the same string is still a valid build-dir-relative path, so one
/// list serves both halves.
fn browser_probe_spec(rel: &str) -> String {
    let trimmed = rel.trim().trim_start_matches('/');
    let stripped = trimmed.strip_prefix("./").unwrap_or(trimmed);
    format!("./{stripped}")
}

/// [`BROWSER_SERVER_PROBES`] resolved for one function: its own directory first when the manifest
/// names one (Build Output API v3 `.func` directories), then the deployment root, in table order.
fn browser_server_probe_table(f: &fluid_core::FunctionConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(BROWSER_SERVER_PROBES.len() * 2);
    if let Some(cwd) = f.cwd_relative.as_deref() {
        let cwd = cwd.trim().trim_matches('/');
        if !cwd.is_empty() {
            for probe in BROWSER_SERVER_PROBES {
                out.push(browser_probe_spec(&format!("{cwd}/{probe}")));
            }
        }
    }
    for probe in BROWSER_SERVER_PROBES {
        out.push(browser_probe_spec(probe));
    }
    out
}

/// Reduces a function name to the `[a-z0-9._-]` shape a generated file name may carry (the
/// `hive-vol-` discipline applied to a write path): a tenant-controlled string is never
/// interpolated into a path unsanitized, and the platform prefix is prepended AFTER sanitization.
fn browser_adapter_file_name(fn_name: &str) -> String {
    let mut slug = String::with_capacity(fn_name.len() + 32);
    for ch in fn_name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            slug.push(ch.to_ascii_lowercase());
        } else {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches(|c| c == '.' || c == '-' || c == '_');
    format!(
        "{BROWSER_ADAPTER_FILE_PREFIX}{}{BROWSER_ADAPTER_FILE_SUFFIX}",
        if slug.is_empty() { "fn" } else { slug }
    )
}

/// Generates and stages the platform's browser entry adapter for one Node/Bun function that owns no
/// handler-shaped entry. `preferred` is a file inference located but `bundle()` rejected (typically
/// a real server that exports nothing, the shape this adapter hosts) and is tried first, else the
/// probe table decides. Returns `(entry path, embedded server path)`. The file is written
/// ATOMICALLY (tmp + rename) and read-only (0444), like `stage_build_output_node_launchers`: a
/// pre-existing symlink at the reserved path is a refusal, a regular file is replaced, never
/// followed. The embedded source passes the unimplementable-surface gate here, and the synthesis
/// REFUSES when no server file could be embedded: an entry that adapts nothing throws in every
/// donor's browser while looking perfectly bundled.
fn synthesize_browser_entry(
    build_dir: &Path,
    f: &fluid_core::FunctionConfig,
    preferred: Option<&str>,
) -> Result<(String, String), String> {
    struct EmbeddedServer {
        rel: String,
        source: String,
        uses_import_meta: bool,
    }
    fn lf_normalize(raw: &str) -> String {
        raw.replace("\r\n", "\n").replace('\r', "\n")
    }
    let file_name = browser_adapter_file_name(&f.name);
    let table = browser_server_probe_table(f);
    let mut candidates: Vec<String> = Vec::with_capacity(table.len() + 1);
    if let Some(p) = preferred.map(str::trim).filter(|p| !p.is_empty()) {
        let spec = browser_probe_spec(p);
        if !candidates.contains(&spec) {
            candidates.push(spec);
        }
    }
    for probe in &table {
        if !candidates.iter().any(|c| c == probe) {
            candidates.push(probe.clone());
        }
    }

    let mut embedded: Option<EmbeddedServer> = None;
    let mut present_but_unembeddable: Vec<String> = Vec::new();
    for rel in &candidates {
        let path = build_dir.join(rel);
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "js" | "mjs" | "cjs") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            present_but_unembeddable.push(format!("{rel}: unreadable"));
            continue;
        };
        let src = lf_normalize(&raw);
        if src.is_empty() || src.len() > BROWSER_ADAPTER_EMBED_MAX_BYTES {
            present_but_unembeddable.push(format!(
                "{rel}: {} bytes (embeddable range 1..{BROWSER_ADAPTER_EMBED_MAX_BYTES})",
                src.len()
            ));
            continue;
        }
        let unimplementable = crate::browser_artifacts::unimplementable_lines(&src);
        if !unimplementable.is_empty() {
            present_but_unembeddable.push(format!(
                "{rel}: line {} {}",
                unimplementable[0].0, unimplementable[0].1
            ));
            continue;
        }
        let Ok(rewritten) = crate::browser_esm::rewrite_esm(&src) else {
            present_but_unembeddable
                .push(format!("{rel}: module syntax with no CommonJS equivalent"));
            continue;
        };
        embedded = Some(EmbeddedServer {
            rel: rel.clone(),
            source: rewritten.source,
            uses_import_meta: rewritten.uses_import_meta,
        });
        break;
    }

    let EmbeddedServer {
        rel,
        source,
        uses_import_meta,
    } = match embedded {
        Some(found) => found,
        None => {
            let mut reason = format!(
                "no server file in the deployment root to adapt — looked for [{}]",
                summarize_list(&candidates, 12)
            );
            if !present_but_unembeddable.is_empty() {
                reason.push_str(&format!(
                    "; present but not embeddable [{}]",
                    summarize_list(&present_but_unembeddable, 8)
                ));
            }
            return Err(reason);
        }
    };
    let embed_name = rel.strip_prefix("./").unwrap_or(&rel).to_string();
    let embed_block = {
        let comment_safe_name = embed_name.replace("*/", "* /");
        let meta = if uses_import_meta {
            crate::browser_artifacts::import_meta_binding(&rel)
        } else {
            String::new()
        };
        format!(
            "async function __hive_embedded_init() {{\n\
             \x20 const module = {{ exports: {{}} }};\n\
             \x20 const exports = module.exports;\n\
             \x20 const require = __hive_require_shim;\n\
             \x20 {meta}\
             \x20 /* ---- begin embedded server: {name} ---- */\n\
             {source}\n\
             \x20 /* ---- end embedded server ---- */\n\
             \x20 return module.exports;\n\
             }}\n",
            meta = meta,
            name = comment_safe_name,
            source = source
        )
    };
    let probes_json = serde_json::to_string(&candidates)
        .map_err(|e| format!("cannot encode the browser probe table: {e}"))?;
    let probes_literal = serde_json::to_string(&probes_json)
        .map_err(|e| format!("cannot encode the browser probe table: {e}"))?;
    let embed_name_literal = serde_json::to_string(&embed_name)
        .map_err(|e| format!("cannot encode the embedded entry name: {e}"))?;
    let text = BROWSER_ADAPTER_JS
        .replace("__HIVE_EMBED_NAME__", &embed_name_literal)
        .replace("__HIVE_EMBED_BLOCK__", &embed_block)
        .replace("__HIVE_PROBES_JSON__", &probes_literal);
    if text.contains("__HIVE_") {
        return Err("generated browser entry still carries an unsubstituted placeholder".into());
    }

    let target = build_dir.join(&file_name);
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "the reserved platform browser entry path {file_name:?} is a symlink"
            ));
        }
        Ok(_) => std::fs::remove_file(&target)
            .map_err(|e| format!("cannot replace {file_name:?}: {e}"))?,
        Err(_) => {}
    }
    let tmp = build_dir.join(format!("{file_name}.tmp"));
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &target)?;
        let mut perms = std::fs::metadata(&target)?.permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&target, perms)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot stage {file_name:?}: {e}"));
    }
    Ok((file_name, embed_name))
}

/// A bounded, comma-joined rendering of a list for one log/ refusal line — the
/// probe table is 30 entries wide and a reason is read by a human, not parsed.
fn summarize_list(items: &[String], limit: usize) -> String {
    let mut out = items
        .iter()
        .take(limit)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > limit {
        out.push_str(&format!(", … +{} more", items.len() - limit));
    }
    out
}

/// Every function name a repo's `fluid.json` EXPLICITLY opted into browser execution, read straight
/// off the raw file. Load-bearing because only the plain-`fluid.json` branch of `produce_manifest`
/// deserializes the file into a `Manifest`: the prebuilt-image, compose, Dockerfile and FDI
/// branches synthesize `functions`, so the opt-in would silently evaporate. `deploy_full` compares
/// this list with the manifest it produced and fails the build naming each lost function
/// (loud-or-honored). Parsed with a minimal local shape, so a container-only or malformed
/// `fluid.json` yields an empty list instead of a new way to fail a build.
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

/// Redeploys a project whose retained source lives on a SPECIFIC host node: dispatches the rebuild
/// to it over the fanout transport (HTTP admin or iroh mesh), where an `upload://` request with no
/// archive rebuilds from ITS OWN retained source; the remote build is mirrored into a local build
/// record so the dashboard shows one build. Returns the build id.
pub(crate) async fn redeploy_on_host(
    cloud: Arc<CloudState>,
    project: String,
    mut req: GitDeployRequest,
    host: crate::schedule::Target,
    expected_incarnation: Option<ProjectIncarnation>,
) -> anyhow::Result<String> {
    validate_deploy_source(&req)?;
    drop_git_token_for_non_git_source(&mut req);
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
        repository_build: None,
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

/// An OCI reference may contain at most one `@`, only as a content digest pin (`@sha256:<hex>` or
/// another registered digest algorithm); a string shaped like HTTP userinfo
/// (`user:token@registry/...`) is not valid OCI syntax, yet nothing downstream re-validates it
/// before it reaches `podman pull`'s argv and every log line naming the image, so it is rejected
/// here.
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

/// Rejects a credential-bearing or structurally-unsafe deploy source before it can reach a `Build`
/// row, a webhook payload, a log line or process argv; callers MUST run it before
/// `cloud.builds.insert` (`start_build` and `redeploy_on_host` both call it first).
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

fn drop_git_token_for_non_git_source(req: &mut GitDeployRequest) {
    let is_non_git_source = req.image_ref.is_some()
        || req.zip_b64.is_some()
        || req.repo_url.starts_with("upload://")
        || req.repo_url.starts_with("image://");
    if is_non_git_source {
        req.git_token = None;
    }
}

/// Project function settings only fill in what a function left at its default: a fluid.json value
/// always survives, and an explicit value equal to the default is indistinguishable from omission,
/// which is a no-op whenever the two agree.
fn fill_unset_from_project_defaults(
    f: &mut fluid_core::FunctionConfig,
    fsettings: &crate::project_settings::FunctionSettings,
) {
    if f.max_duration_secs == fluid_core::FunctionConfig::default().max_duration_secs {
        f.max_duration_secs = fsettings.default_max_duration_secs;
    }
    if f.vcpus == fluid_core::FunctionConfig::default().vcpus {
        f.vcpus = fsettings.vcpus.max(1);
    }
    if f.memory_mib == fluid_core::FunctionConfig::default().memory_mib {
        f.memory_mib = fsettings.memory_mib;
    }
    f.vcpus = f.vcpus.max(1);
}

/// Fluid Compute ON (the default): one warm instance serves many concurrent requests (in-instance
/// concurrency suits I/O-bound work) and at least one instance stays warm; OFF is classic
/// one-request-per-instance with scale-to-zero.
fn apply_compute_mode(f: &mut fluid_core::FunctionConfig, fluid_enabled: bool) {
    if fluid_enabled {
        f.max_concurrency = f.max_concurrency.max(10);
        f.min_instances = f.min_instances.max(1);
    } else {
        f.max_concurrency = 1;
        f.min_instances = 0;
    }
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

    let first_source_deploy = production_branch.is_empty();
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
            _ if first_source_deploy => BuildTrustLane::Production,
            _ => BuildTrustLane::PreviewBase,
        }
    };

    Ok(BuildTrustContext {
        lane,
        canonical_repo,
        actual_repo,
    })
}

/// The ONE `BUILD_ISOLATION_UNAVAILABLE` message, shared by both refusal sites in `run_build` (the
/// fleet-wide gate and the region-filtered one) so they cannot drift. It states the real
/// requirement (a node that can run repository build commands: an isolated BuildExecutor v1 or a
/// host-exec mock/litebox backend, `schedule::build_isolation_capable`) and keeps three counts
/// apart: capable fleet-wide, capable in the configured region(s), and capable AND
/// healthy/reachable there, the one that explains an empty placement.
fn build_isolation_unavailable_message(
    diagnosis: &crate::schedule::BuildPlacementDiagnosis,
    regions: &[String],
) -> String {
    let requirement = "this source deployment needs a node that can run repository build commands (an isolated BuildExecutor v1, or a host-exec mock/litebox backend that builds on the host), but no healthy reachable placement satisfying the request offers one";
    let mut sentences: Vec<String> = Vec::new();
    let fleet_has_no_build_capable_node = diagnosis.capable_total == 0;
    if fleet_has_no_build_capable_node {
    } else if regions.is_empty() {
        sentences.push(format!(
            "{} node(s) known to the mesh can run repository build commands, of which {} currently pass placement (healthy, reachable and otherwise eligible).",
            diagnosis.capable_total, diagnosis.placeable_in_region
        ));
    } else {
        let head = format!(
            "{} node(s) known to the mesh can run repository build commands; {} of those are in the project's configured region(s) ({})",
            diagnosis.capable_total,
            diagnosis.capable_in_region,
            regions.join(", ")
        );
        let nothing_in_configured_regions = diagnosis.capable_in_region == 0;
        if nothing_in_configured_regions {
            sentences.push(format!("{head}."));
        } else {
            sentences.push(format!(
                "{head}, of which {} currently pass placement (healthy, reachable and otherwise eligible).",
                diagnosis.placeable_in_region
            ));
        }
    }
    let why = if diagnosis.capable_total == 0 {
        "No node in the mesh advertises either an isolated BuildExecutor v1 or a host-exec mock/litebox backend, so this is not a health, region or connectivity problem.".to_string()
    } else if diagnosis.capable_in_region == 0 && !regions.is_empty() {
        "Every build-capable node is OUTSIDE the project's configured region(s): add a region that has one, or move a build-capable node into one of them.".to_string()
    } else if diagnosis.placeable_in_region == 0 {
        let blockers = if diagnosis.blockers.is_empty() {
            "no per-node reason recorded".to_string()
        } else {
            diagnosis.blockers.join("; ")
        };
        format!(
            "The build capability IS present there — what blocks it is node health/connectivity or another placement constraint ({blockers}), not a missing BuildExecutor: check that those nodes are healthy and reachable from the coordinator, or add another region to the project."
        )
    } else {
        format!(
            "{} node(s) there pass every per-node check, so the empty placement came from a request-level constraint (e.g. a stateful/single-writer deployment is constrained to a single region), not from build capability.",
            diagnosis.placeable_in_region
        )
    };
    sentences.push(why);
    sentences.push("No repository-controlled command was run on the host.".to_string());
    format!(
        "BUILD_ISOLATION_UNAVAILABLE: {requirement}. {}",
        sentences.join(" ")
    )
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
    let region = &cloud.region;
    let region_label = region_label(region);
    let log = |s: String| cloud.builds.log(bid, s);

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

    if let Some(env) = &req.env {
        let direct_deploy_may_persist_env = !req.no_fanout && !trust.lane.is_fork();
        if direct_deploy_may_persist_env {
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

    let coordinator_places_project = !req.no_fanout;
    if coordinator_places_project {
        let placement_settings = cloud.projects.get_exact(&project, incarnation)?;
        let regions = placement_settings.functions.regions.clone();
        let known_container = req.image_ref.is_some();
        let needs_gpu = placement_settings.functions.gpu;
        let known_wasm = hive_core::Runtime::from_config_str(&placement_settings.build.runtime)
            == Some(hive_core::Runtime::Wasmer);
        let known_bun = hive_core::Runtime::from_config_str(&placement_settings.build.runtime)
            == Some(hive_core::Runtime::Bun);
        let known_build_toolchain = build_toolchain_need(
            &placement_settings.build.install_command,
            &placement_settings.build.build_command,
        );
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
            known_build_toolchain,
        );
        let build_diagnosis = crate::schedule::build_placement_diagnosis(
            cloud,
            &regions,
            known_container,
            needs_gpu,
            crate::schedule::InterpreterNeeds {
                wasm: known_wasm,
                bun: known_bun,
            },
            !known_container,
            known_build_toolchain,
        );
        if needs_build_isolation && targets.is_empty() && build_diagnosis.capable_total == 0 {
            let settings_prove_repository_commands =
                !placement_settings.build.install_command.trim().is_empty()
                    || !placement_settings.build.build_command.trim().is_empty();
            if settings_prove_repository_commands {
                let msg = build_isolation_unavailable_message(&build_diagnosis, &regions);
                log(msg.clone());
                tracing::warn!(project = %project, "deploy refused: no isolated builder capability");
                return Err(anyhow::anyhow!(msg));
            }
            log("No node on this fleet can run repository build commands — only zero-command static deploys can succeed; any repository command will be refused.".into());
        }
        if needs_gpu && targets.is_empty() {
            let gpu_nodes = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| n.gpu_count > 0)
                .count();
            let msg = format!(
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
            let settings_prove_repository_commands =
                !placement_settings.build.install_command.trim().is_empty()
                    || !placement_settings.build.build_command.trim().is_empty();
            if settings_prove_repository_commands {
                let msg = build_isolation_unavailable_message(&build_diagnosis, &regions);
                log(msg.clone());
                tracing::warn!(
                    project = %project,
                    capable = build_diagnosis.capable_total,
                    in_region = build_diagnosis.capable_in_region,
                    placeable = build_diagnosis.placeable_in_region,
                    regions = ?regions,
                    "deploy refused: no placeable node for this repository build"
                );
                return Err(anyhow::anyhow!(msg));
            }
        }
        let auto_chosen_regions_to_surface = regions.is_empty() && !targets.is_empty();
        if auto_chosen_regions_to_surface {
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
        let local_selected = targets
            .iter()
            .any(|t| t.admin.is_none() && t.iroh.is_none());
        let remote: Vec<crate::schedule::Target> = targets
            .iter()
            .filter(|t| t.admin.is_some() || t.iroh.is_some())
            .cloned()
            .collect();

        if !targets.is_empty() && !local_selected {
            let mut names: Vec<String> = targets.iter().map(|t| t.node.clone()).collect();
            log(format!(
                "Placement: region-aware scheduler → {}",
                names.join(", ")
            ));
            let primary_first = true;
            let mut ok = fanout_remote(
                cloud,
                bid,
                &req,
                &project,
                incarnation,
                &remote,
                primary_first,
            )
            .await;
            let mut fallback_landed: Option<String> = None;
            if ok.nothing_ran() && !cloud.build_cancels.is_cancelled(bid) {
                let pinned = cloud
                    .leases
                    .owner_of(&project)
                    .filter(|holder| names.iter().any(|n| n == holder));
                if let Some(holder) = pinned {
                    log(format!(
                        "No dispatch fallback: {holder} holds this project's live container lease (its state volume lives there), so the deploy is not moved to another node."
                    ));
                } else {
                    let fallbacks = crate::schedule::dispatch_fallbacks(
                        cloud,
                        &regions,
                        known_container,
                        needs_gpu,
                        crate::schedule::InterpreterNeeds {
                            wasm: known_wasm,
                            bun: known_bun,
                        },
                        !known_container,
                        needs_build_isolation,
                        known_build_toolchain,
                        &names,
                    );
                    let max = dispatch_fallback_max().min(fallbacks.len());
                    if fallbacks.is_empty() {
                        log("No dispatch fallback available: no other healthy, capable, reachable node is known to the mesh.".into());
                    }
                    let node_region: std::collections::HashMap<String, String> = cloud
                        .registry
                        .nodes()
                        .into_iter()
                        .map(|n| (n.name, n.region))
                        .collect();
                    for (idx, cand) in fallbacks.iter().take(max).enumerate() {
                        if cloud.build_cancels.is_cancelled(bid) {
                            break;
                        }
                        let region = node_region.get(&cand.node).cloned().unwrap_or_default();
                        let in_region = regions
                            .iter()
                            .any(|r| r.trim().eq_ignore_ascii_case(&region));
                        log(format!(
                            "→ fallback {}/{}: {} ({}{}) — {} could not be reached, dispatching the deploy there instead",
                            idx + 1,
                            max,
                            cand.node,
                            region,
                            if in_region { "" } else { ", outside the configured region(s)" },
                            ok.unreachable().join(", ")
                        ));
                        let attempt = fanout_remote(
                            cloud,
                            bid,
                            &req,
                            &project,
                            incarnation,
                            std::slice::from_ref(cand),
                            true,
                        )
                        .await;
                        names.push(cand.node.clone());
                        let ran = !attempt.nothing_ran();
                        ok.per_target.extend(attempt.per_target);
                        if ran {
                            fallback_landed = Some(cand.node.clone());
                            break;
                        }
                    }
                }
            }
            let promotable = ok.promotable();
            let unreachable = ok.unreachable();
            let cancelled = (!promotable && ok.cancelled())
                || (!promotable && cloud.build_cancels.is_cancelled(bid));
            let had_prior_deployment =
                cloud.gw.deployment_records().iter().any(|r| {
                    r.project.eq_ignore_ascii_case(&project) && r.state == DeployState::Ready
                }) || cloud.peer_deployments.read().values().flatten().any(|d| {
                    d.project.eq_ignore_ascii_case(&project) && d.state == DeployState::Ready
                });
            if promotable {
                if let Some(landed) = fallback_landed.as_deref().filter(|_| ok.ready() > 0) {
                    log(format!(
                        "⚠ Deployed on {landed} because {} could not be reached. The configured region is a preference, not a fence: the next deploy re-evaluates placement and lands back in region once that node is reachable again.",
                        unreachable.join(", ")
                    ));
                } else if !unreachable.is_empty() {
                    log(format!(
                        "⚠ Deployed to {} of {} target(s). Could not reach: {} — those regions ran \
                         nothing and are DEGRADED, not failed; replication is repaired when they \
                         return. Serving continues from the healthy region(s).",
                        ok.ready(),
                        ok.per_target.len(),
                        unreachable.join(", ")
                    ));
                }
                let provably_production =
                    request_is_production(cloud, &req, &project) == Some(true);
                if provably_production {
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
    }

    log(format!("Running build in {region_label} - {region}"));
    log("Build machine configuration: 4 cores, 8 GB".into());

    let stamp = now_ms();
    let dir = deploy_root().join(format!("{}-{}-{}", checkout_tag(&project), stamp, bid));
    let _active_checkout = ActiveCheckoutGuard::new(dir.clone(), &project, incarnation);
    tokio::fs::create_dir_all(deploy_root()).await?;
    let branch = req.branch.clone().unwrap_or_default();

    let mut source_snapshot: Option<fluid_build::SourceSnapshot> = None;
    let (commit, full_sha, commit_message) = if let Some(image) = req.image_ref.clone() {
        tokio::fs::create_dir_all(&dir).await?;
        log(format!("Deploying prebuilt image: {image}"));
        (
            "image".to_string(),
            String::new(),
            format!("Image: {image}"),
        )
    } else if let Some(zip_b64) = req.zip_b64.take() {
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
        source_snapshot = zip_source_snapshot(&bytes);
        let files = extract_zip_into(&bytes, &dir).await?;
        log(format!(
            "Extracted {files} file(s) in {}ms",
            now_ms().saturating_sub(t0)
        ));
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
            source_snapshot = zip_source_snapshot(&bytes);
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
        let git_token: Option<String> = match req.git_token.clone().filter(|t| !t.is_empty()) {
            Some(token) => Some(token),
            None => match std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()) {
                Some(token) => Some(token),
                None => resolve_git_poll_token(&clone_source_url).await,
            },
        }
        .filter(|_| clone_source_url.starts_with("https://github.com/"));

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
                                .env("GIT_ASKPASS", "/bin/echo")
                                .env("GIT_HTTP_LOW_SPEED_LIMIT", "1024")
                                .env("GIT_HTTP_LOW_SPEED_TIME", "60");
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
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    let mut cmd = Command::new("git");
                    cmd.env("GIT_TERMINAL_PROMPT", "0")
                        .env("GIT_ASKPASS", "/bin/echo")
                        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1024")
                        .env("GIT_HTTP_LOW_SPEED_TIME", "60");
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
                    let mut cmd = Command::new("git");
                    cmd.env("GIT_TERMINAL_PROMPT", "0")
                        .env("GIT_ASKPASS", "/bin/echo")
                        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1024")
                        .env("GIT_HTTP_LOW_SPEED_TIME", "60");
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
                || s.contains("Repository not found")
        };

        let used_token = git_token.is_some();
        struct AbortOnDrop(tokio::task::JoinHandle<()>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _heartbeat = AbortOnDrop({
            let cloud = cloud.clone();
            let bid = bid.to_string();
            tokio::spawn(async move {
                let mut secs = 0u64;
                loop {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    secs += 20;
                    cloud.builds.log(&bid, format!("… still cloning ({secs}s)"));
                }
            })
        });
        let mut with_token = used_token;
        let (mut ok, mut stderr) = run_clone(with_token).await;
        if !ok && used_token && auth_failed(&stderr) {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            log("Retrying clone without the stored GitHub credential…".into());
            with_token = false;
            let (ok2, stderr2) = run_clone(false).await;
            ok = ok2;
            stderr = stderr2;
        }
        let mut attempt = 1usize;
        while !ok && !auth_failed(&stderr) && attempt < 3 {
            if cloud.build_cancels.is_cancelled(bid) {
                return Err(BuildCancelled.into());
            }
            attempt += 1;
            let cause = stderr.lines().last().unwrap_or("unknown error").to_string();
            log(format!(
                "Clone failed ({cause}); retrying (attempt {attempt}/3)…"
            ));
            tokio::time::sleep(Duration::from_secs(5 * (attempt as u64 - 1))).await;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            let (ok2, stderr2) = run_clone(with_token).await;
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
        drop(_heartbeat);
        req.git_token = None;
        log(format!(
            "Cloning completed: {}ms",
            now_ms().saturating_sub(t0)
        ));
        let commit = run_git(&dir, &["rev-parse", "--short", "HEAD"])
            .await
            .unwrap_or_default();
        let full_sha = run_git(&dir, &["rev-parse", "HEAD"])
            .await
            .unwrap_or_else(|| commit.clone());
        let commit_message = run_git(&dir, &["log", "-1", "--pretty=%s"])
            .await
            .unwrap_or_default();
        let tree_sha = run_git(&dir, &["rev-parse", "HEAD^{tree}"])
            .await
            .unwrap_or_default();
        source_snapshot = git_source_snapshot(&clone_source_url, &full_sha, &tree_sha);
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
    if let Some(fluid_build::SourceSnapshot::Git { branch, .. }) = source_snapshot.as_mut() {
        *branch = Some(actual_branch.clone()).filter(|value| !value.is_empty());
    }

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
    {
        let runtime_only: Vec<&str> = runtime_env
            .keys()
            .filter(|k| !build_env.contains_key(*k))
            .map(String::as_str)
            .collect();
        if !runtime_only.is_empty() {
            log(format!(
                "Note: {} runtime-scoped variable(s) are NOT exposed to the build: {}. \
                 Set scope to \"Build + Runtime\" in Settings → Environment Variables if the build needs them.",
                runtime_only.len(),
                runtime_only.join(", ")
            ));
        }
    }
    let build_cache_enabled = req.use_cache;

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
    let root_package_manager = if req.image_ref.is_none() {
        Some(
            fluid_build::detect_package_manager_checked(&checkout_dir).map_err(|error| {
                fluid_build::BuildContractError::invalid_metadata(
                    "resolve checkout package-manager metadata",
                    error,
                )
            })?,
        )
    } else {
        None
    };
    let root_workspace = if let Some(package_manager) = root_package_manager.as_ref() {
        crate::workspace::load_for_manager(&checkout_dir, package_manager)
            .await
            .map_err(|error| {
                fluid_build::BuildContractError::invalid_metadata(
                    "load checkout workspace metadata",
                    error,
                )
            })?
    } else {
        None
    };
    let (
        build_dir,
        workspace_member,
        application_source,
        application_evidence,
        application_decision_digest,
    ) = if let Some(root) = effective_root.as_deref() {
        log(format!("Root directory: {root}"));
        let selected = resolve_checkout_dir(&checkout_dir, Some(root)).await?;
        let member = if selected == checkout_dir {
            false
        } else {
            crate::app_discovery::is_member(&checkout_dir, &selected, root_workspace.as_ref())?
        };
        let source = if req
            .root_dir
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            "deploy_request"
        } else {
            "project_setting"
        };
        (
            selected,
            member,
            source.to_string(),
            vec![format!("Root Directory {root:?}")],
            None,
        )
    } else if req.image_ref.is_some() {
        (
            checkout_dir.clone(),
            false,
            "prebuilt_image".to_string(),
            Vec::new(),
            None,
        )
    } else if let Some(selection) =
        crate::app_discovery::select(&checkout_dir, root_workspace.as_ref()).await?
    {
        let relative = if selection.relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            selection.relative.to_string_lossy().replace('\\', "/")
        };
        log(format!(
            "Auto-detected workspace app: {relative} ({}; evidence: {}).",
            selection.workspace_source,
            selection.evidence.join(", ")
        ));
        let selected = if selection.relative.as_os_str().is_empty() {
            checkout_dir.clone()
        } else {
            resolve_checkout_dir(&checkout_dir, Some(&relative)).await?
        };
        (
            selected,
            !selection.relative.as_os_str().is_empty(),
            "automatic_workspace_discovery".to_string(),
            selection.evidence,
            Some(selection.decision_digest),
        )
    } else {
        (
            checkout_dir.clone(),
            false,
            "repository_root".to_string(),
            Vec::new(),
            None,
        )
    };
    let selected_coordinates =
        crate::build_coordinates::MonorepoCoordinates::selected(&checkout_dir, &build_dir)?;
    let build_dir = selected_coordinates.selected_app();
    let vercel_config = fluid_build::load_vercel_config_checked(&build_dir).map_err(|error| {
        fluid_build::BuildContractError::invalid_metadata("load selected-app vercel.json", error)
    })?;
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
                root_package_manager
                    .as_ref()
                    .expect("source build package manager"),
                root_workspace.as_ref(),
                &application_source,
                application_evidence.clone(),
                application_decision_digest.clone(),
                first_deploy,
                build_cache_enabled,
            )
            .await?,
        )
    } else {
        None
    };
    let coordinates = fdi_preparation
        .as_ref()
        .map(|preparation| preparation.coordinates.clone())
        .unwrap_or(selected_coordinates);
    let repository_workspace = if workspace_member {
        root_workspace.clone()
    } else {
        None
    };
    let repository_build = fdi_preparation
        .as_ref()
        .map(|preparation| preparation.repository_build.clone());
    if let Some(repository_build) = repository_build.as_ref() {
        repository_build.verify()?;
        cloud.builds.update(bid, |build| {
            build.repository_build = Some(repository_build.clone());
        });
    }
    if fdi_preparation.is_none() {
        if let Some(output) = vercel_config
            .as_ref()
            .and_then(|config| config.output_directory.as_deref())
            .filter(|output| !output.trim().is_empty())
        {
            fluid_build::OutputDirectory::parse(output)?;
        }
    }
    let host_exec_backend = matches!(cloud.gw.backend_name(), "mock" | "litebox");
    let mut isolated = if req.image_ref.is_some() {
        None
    } else if host_exec_backend {
        log(format!(
            "Backend {} runs builds as host processes; repository commands execute directly in the checkout.",
            cloud.gw.backend_name()
        ));
        Some(IsolatedBuild::begin_host(&checkout_dir))
    } else {
        match crate::build_executor::get() {
            Ok(_) => Some(IsolatedBuild::begin(&checkout_dir).await?),
            Err(error) => {
                log(format!(
                    "No isolated build executor on this node ({error}); repository commands are disabled for this build — only a zero-command static plan can succeed."
                ));
                None
            }
        }
    };

    if let Some(warning) = fdi_preparation
        .as_ref()
        .and_then(|preparation| preparation.package_manager.conflict_warning.as_ref())
    {
        log(format!("WARN: {warning}"));
    }

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
                        b.state = DeployState::Cancelled;
                        b.finished_ms = Some(now_ms());
                    });
                    return Ok(());
                }
                Ok(false) => log("Ignored Build Step exited non-zero — continuing build.".into()),
                Err(error) => return Err(error),
            }
        }
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

    let build_failed = false;
    let mut manifest = produce_manifest(
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
    .await?;
    coordinates.normalize_function_cwds(&mut manifest)?;
    manifest.project = project.clone();

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

    if manifest.framework.is_empty() {
        let bc = cloud.projects.get_exact(&project, incarnation)?.build;
        let fw = bc.framework.trim();
        if !fw.is_empty() {
            manifest.framework = fw.to_string();
        }
    }

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
        fill_unset_from_project_defaults(f, &fsettings);
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
        f.gpu = f.gpu || fsettings.gpu;
        f.dedicated_ipv4 = fsettings.dedicated_ipv4;
        f.dedicated_ipv4_alloc = if f.dedicated_ipv4 {
            dedicated_ipv4_alloc.clone()
        } else {
            None
        };
        apply_compute_mode(f, fsettings.fluid_enabled);
    }

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

    let git = GitSource {
        repo_url: req.repo_url.clone(),
        branch: actual_branch.clone(),
        commit: commit.clone(),
        commit_message: commit_message.clone(),
    };
    let is_container = manifest.functions.iter().any(|f| f.runtime == "container");
    let is_stateful = is_container || manifest.functions.iter().any(|f| f.needs_raw_proxy());
    let is_wasm = manifest.functions.iter().any(|f| {
        hive_core::Runtime::resolve(&f.runtime, &f.start_cmd) == hive_core::Runtime::Wasmer
    });
    let is_bun = manifest
        .functions
        .iter()
        .any(|f| hive_core::Runtime::resolve(&f.runtime, &f.start_cmd) == hive_core::Runtime::Bun);

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

    {
        let ingested_early = find_workflow_manifest(&build_dir).is_some();
        let py_wdk = crate::world_queue::vercel_json_declares_workflow_worker(&build_dir);
        let opted_out = crate::world_queue::workflow_world_opted_out(cloud, &project, &build_dir);
        tracing::info!(
            %project,
            ingested_early,
            py_wdk,
            opted_out,
            "workflow World auto-wiring gate evaluated (pre-launch)"
        );
        if (ingested_early || py_wdk) && !opted_out {
            let queue_url = std::env::var("HIVE_QUEUE_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:8786".to_string());
            let team = crate::admin::norm(&project_settings.team).to_string();
            let mut queue_env_written = false;
            let mut plaintext_queue_env: Vec<(String, String)> = Vec::new();
            if let Ok(queue_token) = crate::auth::issue_queue_client_token(&team) {
                let endpoint_result = cloud.projects.put_env_exact(
                    &project,
                    incarnation,
                    crate::project_settings::EnvVar {
                        key: "HIVE_QUEUE_ENDPOINT".into(),
                        value: queue_url.clone(),
                        target: "all".into(),
                        scope: "runtime".into(),
                        sensitive: false,
                        updated_ms: now_ms(),
                    },
                );
                let token_result = cloud.projects.put_env_exact(
                    &project,
                    incarnation,
                    crate::project_settings::EnvVar {
                        key: "HIVE_QUEUE_TOKEN".into(),
                        value: queue_token.clone(),
                        target: "all".into(),
                        scope: "runtime".into(),
                        sensitive: true,
                        updated_ms: now_ms(),
                    },
                );
                match (endpoint_result, token_result) {
                    (Ok(()), Ok(())) => {
                        queue_env_written = true;
                        plaintext_queue_env = vec![
                            ("HIVE_QUEUE_ENDPOINT".to_string(), queue_url),
                            ("HIVE_QUEUE_TOKEN".to_string(), queue_token),
                        ];
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        tracing::warn!(
                            %project,
                            %incarnation,
                            %error,
                            "workflow queue env write lost project-incarnation authority — skipping World auto-wiring for this build, next deploy will retry"
                        );
                        log(
                            "Vercel Workflow SDK detected, but a concurrent deploy raced the \
                             queue env write -- skipped for this build; it will retry on the \
                             next deploy."
                                .to_string(),
                        );
                    }
                }
            }
            if queue_env_written {
                for f in manifest.functions.iter_mut() {
                    for (k, v) in &plaintext_queue_env {
                        f.env.insert(k.clone(), v.clone());
                    }
                }
                let dbreq = crate::databases::ProvisionReq {
                    name: "workflow-storage".into(),
                    project: project.clone(),
                    team,
                    kind: crate::databases::DbKind::Redis,
                    region: Some(cloud.region.clone()),
                    provider: None,
                    replicas: Vec::new(),
                };
                let cloud_ready = cloud.clone();
                let ready_project = project.clone();
                let completion_guard = Arc::new(parking_lot::Mutex::new(Some(
                    ActiveCheckoutGuard::new(build_dir.clone(), &ready_project, incarnation),
                )));
                let workflow_database = crate::databases::provision(
                    cloud.databases.clone(),
                    cloud.region.clone(),
                    dbreq,
                    cloud.db_domain.clone(),
                    cloud.node_name.clone(),
                    cloud.api_base(),
                    move |d| {
                        let cloud_ready = cloud_ready.clone();
                        let ready_project = ready_project.clone();
                        let completion_guard = completion_guard.clone();
                        tokio::spawn(async move {
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
                    &project,
                    incarnation,
                    workflow_database.id.clone(),
                ) {
                    crate::databases::note_teardown_request(&workflow_database.id);
                    cloud
                        .databases
                        .remove_db_and_purge_data(&workflow_database.id, &workflow_database.team);
                    tracing::warn!(
                        %project,
                        database = %workflow_database.id,
                        %error,
                        "workflow storage lost project-incarnation authority before admission — skipping auto-wiring for this build, next deploy will retry"
                    );
                    log(
                        "Vercel Workflow SDK detected, but a concurrent deploy raced the storage \
                         auto-wiring -- skipped for this build; it will retry on the next deploy."
                            .to_string(),
                    );
                } else {
                    log(format!(
                        "Detected Vercel Workflow SDK ({}): auto-wired hive's managed World -- Queue (HIVE_QUEUE_ENDPOINT/_TOKEN) active for this deploy, Storage (a provisioned Redis, UPSTASH_REDIS_REST_URL/_TOKEN) finishing in the background -- fully active from the next deploy.",
                        if ingested_early { "JS/TS" } else { "Python" }
                    ));
                }
            }
        }
    }

    if !build_failed && !is_container {
        log("Compile-cache warmup skipped under isolated builds.".into());
    }

    if !build_failed {
        for f in manifest.functions.iter_mut() {
            f.browser_ineligible_reason = None;
        }
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
            let msg =
                format!(
                "Browser opt-in rejected — the deployment was NOT registered. fluid.json declares \
                 `functions[].browser` for {}, but this project builds through the {} path, which \
                 constructs its own function list and cannot carry a per-function browser opt-in. \
                 The functions this build produced are [{}]. Remove the `browser` block (a \
                 container/compose/prebuilt-image service can never run in a donor's browser), or \
                 deploy this function from a plain fluid.json project with a JS/Bun `start_cmd`.",
                dropped
                    .iter()
                    .map(|n| {
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
                continue;
            }
            let runtime = hive_core::Runtime::resolve(&f.runtime, &f.start_cmd);
            if !matches!(runtime, hive_core::Runtime::Node | hive_core::Runtime::Bun) {
                f.browser_ineligible_reason = Some(format!(
                    "runs as {} — only plain JS/Bun request→response handlers can run in a browser",
                    runtime.as_str()
                ));
                continue;
            }
            let entry = infer_browser_entry(&build_dir, &f.name)
                .or_else(|| infer_package_server_entry(&build_dir));
            let entry = match entry {
                Some(entry) => {
                    log(format!(
                        "Browser artifact ({}): auto-detected entry {entry:?} — serving in browsers \
                         automatically (no fluid.json browser opt-in needed).",
                        f.name
                    ));
                    Some(entry)
                }
                None => match synthesize_browser_entry(&build_dir, f, None) {
                    Ok((generated, located)) => {
                        log(format!(
                            "Browser artifact ({}): no handler-shaped entry in the deployment root — \
                             generated {generated:?} to adapt the server it located at {located:?}, \
                             so this function is served in browsers with no fluid.json browser \
                             opt-in and no entry file shipped.",
                            f.name
                        ));
                        Some(generated)
                    }
                    Err(reason) => {
                        log(format!(
                            "Browser artifact ({}): not browser-eligible — {reason}",
                            f.name
                        ));
                        tracing::info!(project = %project, function = %f.name, %reason, "auto browser entry not generated (no server to adapt)");
                        f.browser_ineligible_reason =
                            Some(format!("could not generate a browser entry — {reason}"));
                        None
                    }
                },
            };
            if let Some(entry) = entry {
                f.browser = Some(fluid_core::BrowserPolicy {
                    entry,
                    ..Default::default()
                });
                auto_browser.insert(f.name.clone());
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
                Ok(bundled) => Ok(bundled),
                Err(reason) => {
                    if !synthesized {
                        Err(reason)
                    } else if let Some(preferred) = f.browser.as_ref().map(|p| p.entry.clone()) {
                        match synthesize_browser_entry(&build_dir, f, Some(&preferred)) {
                            Ok((generated, located)) => {
                                log(format!(
                                    "Browser artifact ({}): the auto-detected entry {preferred:?} \
                                     does not export a handler ({reason}) — generated {generated:?} \
                                     to adapt the server it located at {located:?}.",
                                    f.name
                                ));
                                f.browser = Some(fluid_core::BrowserPolicy {
                                    entry: generated,
                                    ..Default::default()
                                });
                                crate::browser_artifacts::bundle(&build_dir, f).map_err(|second| {
                                    format!(
                                        "{reason}; the generated browser entry was rejected too: {second}"
                                    )
                                })
                            }
                            Err(generate) => Err(format!(
                                "{reason}; could not generate a browser entry: {generate}"
                            )),
                        }
                    } else {
                        Err(reason)
                    }
                }
            };
            let bundled = match bundled {
                Ok(bundled) => bundled,
                Err(reason) if synthesized => {
                    log(format!(
                        "Browser artifact ({}): auto-detected handler is not browser-eligible, \
                         serving the normal fleet path instead — {reason}",
                        f.name
                    ));
                    tracing::info!(project = %project, function = %f.name, %reason, "auto browser artifact skipped (ineligible)");
                    f.browser = None;
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
            f.browser_ineligible_reason = None;
        }
    }

    if !build_failed && !is_container {
        if host_exec_backend {
        } else {
            match crate::build_executor::get() {
                Ok(_) => reseal_platform_output(&checkout_dir).await?,
                Err(error) => log(format!(
                    "Skipping final reseal: no isolated build executor on this node ({error}); no repository-controlled command ran, the checkout is platform-authored as-is."
                )),
            }
        }
    }

    let artifact_relative = coordinates.runtime_artifact_relative();
    let runtime_artifact_base = coordinates.runtime_artifact_base();
    let runtime_artifact = if !build_failed && !is_container {
        hive_backend::RuntimeArtifactSpec::with_includes(
            coordinates.checkout_root().to_path_buf(),
            artifact_relative.to_path_buf(),
            crate::app_discovery::runtime_include_rel(
                coordinates.checkout_root(),
                &runtime_artifact_base,
                repository_workspace.as_ref(),
            )
            .await?,
        )
    } else {
        hive_backend::RuntimeArtifactSpec::new(
            coordinates.checkout_root().to_path_buf(),
            artifact_relative.to_path_buf(),
        )
    };
    if !build_failed && !is_container {
        let fluid_json_present = build_dir.join("fluid.json").is_file();
        if let Err(error) =
            preflight_direct_entries(&manifest, &runtime_artifact, fluid_json_present).await
        {
            let msg = format!("{error:#}");
            log(msg.clone());
            tracing::warn!(project = %project, %msg, "launch-shape preflight refused the build");
            return Err(error);
        }
    }
    let image = format!("dpl-{}", sanitize_tag(bid));
    let (host_static_root, runtime_workdir, runtime_artifact_identity, sealed_runtime_artifact) =
        if !build_failed && !is_container {
            let sealed = hive_backend::seal_host_runtime_artifact(
                &runtime_artifact,
                &crate::persist::data_dir().join("runtime-artifacts-v1"),
            )
            .await?;
            let expected = sealed.identity(&image)?;
            let runtime_paths = cloud.gw.runtime_artifact_paths(&sealed)?;
            anyhow::ensure!(
                runtime_paths.delivery_required,
                "backend {} did not require explicit sealed runtime delivery",
                cloud.gw.backend_name()
            );
            let runtime_workdir = runtime_paths
                .guest_workdir
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("validated guest runtime workdir is not UTF-8"))?;
            let host_static_root = runtime_paths
                .host_static_root
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("sealed host static root is not UTF-8"))?;
            cloud
                .gw
                .deliver_build(&image, &sealed)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "could not deliver the sealed build to backend {}: {error}",
                        cloud.gw.backend_name()
                    )
                })?;
            let committed = cloud
                .gw
                .runtime_artifact_identity(&image)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "backend {} accepted image {image:?} without a committed artifact receipt",
                        cloud.gw.backend_name()
                    )
                })?;
            anyhow::ensure!(
                committed == expected,
                "backend {} committed artifact identity {:?}, expected {:?}",
                cloud.gw.backend_name(),
                committed,
                expected
            );
            manifest.image = Some(image.clone());
            log(format!(
                "Sealed runtime artifact {} ({}; cwd {}).",
                &expected.content_sha256[..12],
                cloud.gw.backend_name(),
                runtime_workdir
            ));
            for path in sealed.skipped_python_environments() {
                log(format!(
                    "Omitted {} from the runtime artifact: a Python environment is host-specific \
                     (its interpreter is an absolute symlink that cannot be sealed beneath the \
                     checkout), so it is not in the artifact — start the function with the \
                     platform interpreter (\"python3\"), never a venv path.",
                    path.display()
                ));
            }
            (
                host_static_root,
                runtime_workdir,
                Some(expected),
                Some(sealed),
            )
        } else {
            let host_static_root = runtime_artifact
                .host_static_root()?
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("validated host static root is not UTF-8"))?;
            (host_static_root.clone(), host_static_root, None, None)
        };

    match crate::raw_ports::allocate_raw_ports_coordinated(cloud, &project, &mut manifest).await {
        Ok(ports) if !ports.is_empty() => {
            log(format!(
                "Allocated public raw port(s): {} (stable across redeploys).",
                ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
            ));
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

    let generation_seal = if !build_failed && !is_container {
        match seal_generation_authority(&build_dir, repository_build.as_ref(), &manifest).await {
            Ok(seal) => seal,
            Err(error) => {
                log(format!(
                    "Immutable generation authority was not sealed ({error:#}); remote replication keeps the legacy per-target dispatch for this deploy."
                ));
                None
            }
        }
    } else {
        None
    };

    let cron_specs = manifest.crons.clone();

    let tenant = {
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
                    cloud
                        .projects
                        .set_team_exact(&manifest.project, incarnation, &t)?;
                    t
                }
                None => own,
            }
        }
    };
    if is_production && build_failed {
        log(
            "Build failed — the current production deployment keeps serving; this build is recorded as an error and publishes no route."
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
    let previous_production = cloud
        .gw
        .list()
        .into_iter()
        .find(|deployment| {
            deployment.project == project
                && deployment.project_incarnation == Some(incarnation)
                && deployment.production
                && deployment.state == DeployState::Ready
        })
        .map(|deployment| deployment.id.to_string());
    let source = crate::deployment_ledger::SourceIdentity {
        kind: if req.image_ref.is_some() {
            crate::deployment_ledger::SourceKind::PrebuiltImage
        } else if req.repo_url.starts_with("upload://") {
            crate::deployment_ledger::SourceKind::Upload
        } else {
            crate::deployment_ledger::SourceKind::Git
        },
        repository: req.repo_url.clone(),
        branch: actual_branch.clone(),
        revision: if let Some(image) = req.image_ref.as_ref() {
            image.clone()
        } else if full_sha.is_empty() {
            commit.clone()
        } else {
            full_sha.clone()
        },
    };
    let info = if build_failed {
        let info = cloud.gw.deploy_full_with_runtime_exact(
            host_static_root,
            Some(runtime_workdir),
            manifest,
            req.creator.clone().unwrap_or_else(|| "you".into()),
            Some(git),
            is_production,
            DeployState::Error,
            tenant.clone(),
            incarnation,
        );
        crate::admin::causal_stamp_new_deployment(cloud, &project, &info.id.0);
        info
    } else {
        let mut staged = cloud.gw.stage_full_with_runtime_exact(
            host_static_root,
            Some(runtime_workdir),
            manifest,
            req.creator.clone().unwrap_or_else(|| "you".into()),
            Some(git),
            is_production,
            tenant.clone(),
            incarnation,
        )?;
        let deployment_id = staged.info().id.to_string();
        crate::admin::causal_stamp_new_deployment(cloud, &project, &deployment_id);
        let readiness = staged.prove_ready().await?.clone();
        cloud
            .deployment_ledger
            .accept(crate::deployment_ledger::DeploymentAcceptanceInput {
                deployment_id: deployment_id.clone(),
                project: project.clone(),
                target: if is_production {
                    "production".to_string()
                } else {
                    "preview".to_string()
                },
                source,
                repository_build: repository_build.clone(),
                runtime_artifact: runtime_artifact_identity.clone(),
                readiness,
            })?;
        #[cfg(target_os = "macos")]
        match hive_backend::sep_signer::generate_for_deployment(&deployment_id) {
            Ok(sep_key) => {
                if let Err(error) = cloud.deployment_ledger.append_integrity_entry(
                    &deployment_id,
                    hive_core::IntegrityEntryKind::SepKeyProvisioned {
                        public_key_der_hex: sep_key.public_key_der_hex,
                        key_tag: sep_key.key_tag,
                    },
                ) {
                    tracing::warn!(deployment_id, %error, "integrity chain: failed to record SepKeyProvisioned");
                }
            }
            Err(error) => {
                tracing::debug!(deployment_id, %error, "no Secure Enclave signing key provisioned for this deployment (non-fatal)");
            }
        }
        let (mut info, _readiness) = staged.publish_ready()?;
        if !crate::persist::persist_durable(cloud) {
            cloud.gw.remove(&deployment_id).await;
            anyhow::bail!(
                "deployment {deployment_id} passed readiness but its Ready record could not be persisted"
            );
        }
        let ready_alias = if !info.commit_alias.is_empty() {
            info.commit_alias.clone()
        } else {
            info.id_alias.clone()
        };
        if let Err(error) = cloud.deployment_ledger.mark_published(
            &deployment_id,
            serde_json::json!({
                "id": deployment_id,
                "project": info.project,
                "url": cloud.deploy_url(&ready_alias),
                "state": "ready",
                "production": false,
                "target": info.target,
                "commit": commit,
            }),
        ) {
            cloud.gw.remove(&info.id.to_string()).await;
            let _ = crate::persist::persist_durable(cloud);
            return Err(error.context("durably publish deployment readiness"));
        }
        if is_production {
            let alias_revision = cloud.deployment_ledger.prepare_alias(
                &project,
                previous_production.clone(),
                &info.id.to_string(),
            )?;
            let Some(promoted) = cloud.gw.promote_exact(&info.id.to_string(), incarnation) else {
                cloud
                    .deployment_ledger
                    .abort_alias(alias_revision, "gateway refused accepted deployment")?;
                anyhow::bail!(
                    "accepted deployment {} could not acquire its production alias",
                    info.id
                );
            };
            if !crate::persist::persist_durable(cloud) {
                cloud
                    .deployment_ledger
                    .abort_alias(alias_revision, "platform-state alias persistence failed")?;
                if let Some(previous) = previous_production.as_deref() {
                    let _ = cloud.gw.promote_exact(previous, incarnation);
                } else {
                    cloud.gw.remove(&promoted.id.to_string()).await;
                }
                let _ = crate::persist::persist_durable(cloud);
                anyhow::bail!("production alias revision {alias_revision} could not be persisted");
            }
            info = promoted;
            if let Err(error) = cloud.deployment_ledger.mark_alias_applied(
                alias_revision,
                serde_json::json!({
                    "id": info.id.to_string(),
                    "project": info.project,
                    "url": cloud.deploy_url(&info.alias),
                    "state": "ready",
                    "production": true,
                    "target": "production",
                    "commit": commit,
                    "aliasRevision": alias_revision,
                }),
            ) {
                tracing::error!(
                    deployment = %info.id,
                    alias_revision,
                    %error,
                    "production alias is durably applied; ledger finalization remains pending"
                );
            }
        }
        info
    };

    for (function, descriptor) in &browser_bundles {
        if let Err(e) =
            crate::browser_artifacts::add_owner(&descriptor.policy_digest, &info.id.0).await
        {
            log(format!(
                "WARN: browser artifact ({function}) ownership was not recorded ({e}); the GC keep-set still protects it."
            ));
        }
    }

    if !build_failed && is_production {
        let jobs: Vec<hive_edge::CronJob> = cron_specs
            .iter()
            .enumerate()
            .map(|(i, c)| hive_edge::CronJob {
                id: format!("vc-{}-{}", sanitize_tag(&project), i),
                name: format!("vercel.json {}", c.path),
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

    let ingested = ingest_workflow_manifest(cloud, &project, &build_dir).await;
    if ingested > 0 {
        log(format!(
            "Detected Vercel WDK: registered {ingested} workflow(s) for the Workflows tab."
        ));
    }

    let self_alias = if is_production {
        info.alias.clone()
    } else if !info.commit_alias.is_empty() {
        info.commit_alias.clone()
    } else if !info.id_alias.is_empty() {
        info.id_alias.clone()
    } else {
        info.alias.clone()
    };
    if build_failed {
        log("Deployment failed; no route was published and the predecessor keeps serving.".into());
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
        b.alias = (!build_failed).then(|| self_alias.clone());
    });
    if !build_failed {
        let _ = cloud
            .projects
            .mark_first_deployed_exact(&project, incarnation, now_ms());
    }
    crate::persist::persist(cloud);

    if !build_failed && is_production {
        if let Some(git) = info.git.as_ref().filter(|g| g.is_real_git()) {
            cloud
                .production_deployments
                .record(&project, git.clone(), &cloud.node_name);
        }
    }

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

    if !req.no_fanout && !build_failed {
        let tail_settings = cloud.projects.get_exact(&project, incarnation)?;
        let regions = tail_settings.functions.regions.clone();
        let needs_gpu = tail_settings.functions.gpu;
        let tail_build_toolchain = build_toolchain_need(
            &tail_settings.build.install_command,
            &tail_settings.build.build_command,
        );
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
            tail_build_toolchain,
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
                let mut generation_replicated = false;
                if immutable_generation_fanout_enabled() {
                    match generation_fanout(
                        cloud,
                        bid,
                        &project,
                        incarnation,
                        &tenant,
                        is_production,
                        &req,
                        source_snapshot.as_ref(),
                        generation_seal.as_ref(),
                        sealed_runtime_artifact.as_ref(),
                        &targets,
                        &remote,
                    )
                    .await
                    {
                        GenerationFanoutOutcome::Committed(count) => {
                            generation_replicated = true;
                            log(format!(
                                "✓ Immutable generation committed on {count} remote target(s) from one sealed build — no remote rebuild ran."
                            ));
                        }
                        GenerationFanoutOutcome::Unavailable(reason) => {
                            log(format!(
                                "Immutable generation lane unavailable ({reason}); using the legacy per-target dispatch."
                            ));
                        }
                        GenerationFanoutOutcome::Failed(reason) => {
                            generation_replicated = true;
                            log(format!(
                                "✗ Immutable generation replication failed ({reason}). Remote regions were NOT updated this deploy and no independent rebuild was attempted; serving continues from this node and replication is repaired by the next deploy."
                            ));
                        }
                    }
                }
                if !generation_replicated {
                    let primary_first = false;
                    let _ = fanout_remote(
                        cloud,
                        bid,
                        &req,
                        &project,
                        incarnation,
                        &remote,
                        primary_first,
                    )
                    .await;
                }
            }
            if is_production {
                let names: Vec<String> = targets.iter().map(|t| t.node.clone()).collect();
                cleanup_non_targets(cloud, &project, &names).await;
            }
        }
    }
    Ok(())
}

/// Sealed final-manifest authority for one immutable deployment generation: the canonical manifest
/// bytes (every platform mutation applied) plus the typed build authority that binds their digest.
struct GenerationSeal {
    authority: fluid_build::BuildAuthoritySnapshot,
    manifest_bytes: Vec<u8>,
}

/// Dark-launch gate for the immutable-generation transfer lane: default OFF, a deliberate per-node
/// operator decision, so nothing changes in production until it is enabled.
fn immutable_generation_fanout_enabled() -> bool {
    std::env::var("HIVE_IMMUTABLE_GENERATION_FANOUT")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Seals a lane's typed build authority against the FINAL canonical manifest; `Ok(None)` = sealing
/// is not implemented for this lane yet (Build Output without a repository contract), an honest
/// lane-unavailable, never a guessed authority.
async fn seal_generation_authority(
    build_dir: &Path,
    repository_build: Option<&fluid_build::RepositoryBuildSnapshot>,
    manifest: &Manifest,
) -> anyhow::Result<Option<GenerationSeal>> {
    let manifest_bytes = fluid_core::canonical_manifest_bytes(manifest)
        .context("encode canonical deployment manifest")?;
    let normalized_manifest_sha256 = fluid_core::normalized_manifest_sha256(&manifest_bytes);
    let authority = if let Some(contract) = repository_build {
        contract.verify()?;
        fluid_build::BuildAuthoritySnapshot::DetectedApplication {
            repository_contract: contract.clone(),
            normalized_manifest_sha256,
        }
    } else {
        let Ok(bytes) = tokio::fs::read(build_dir.join("fluid.json")).await else {
            return Ok(None);
        };
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(&bytes);
        fluid_build::BuildAuthoritySnapshot::ExplicitManifest {
            fluid_json: fluid_build::MetadataInputSeal {
                path: fluid_build::RepositoryPath::parse(Path::new("fluid.json"))?,
                sha256: format!("{:x}", hash.finalize()),
                bytes: bytes.len() as u64,
            },
            normalized_manifest_sha256,
        }
    };
    Ok(Some(GenerationSeal {
        authority,
        manifest_bytes,
    }))
}

/// Seal the exact original ZIP archive bytes as an immutable source identity.
fn zip_source_snapshot(bytes: &[u8]) -> Option<fluid_build::SourceSnapshot> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(bytes);
    fluid_build::ContentSeal::new(format!("{:x}", hash.finalize()), bytes.len() as u64)
        .ok()
        .map(|archive| fluid_build::SourceSnapshot::Zip { archive })
}

/// Typed Git source identity, only when every component is proven (full hex commit AND tree from
/// the real checkout); anything less returns `None` rather than storing default-valued authority
/// fields.
fn git_source_snapshot(
    repository: &str,
    commit: &str,
    tree: &str,
) -> Option<fluid_build::SourceSnapshot> {
    fn git_hex(value: &str) -> bool {
        matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
    (git_hex(commit) && git_hex(tree) && !repository.trim().is_empty()).then(|| {
        fluid_build::SourceSnapshot::Git {
            repository: repository.trim().to_string(),
            branch: None,
            commit: commit.to_string(),
            tree: tree.to_string(),
        }
    })
}

enum GenerationFanoutOutcome {
    /// Every remote target committed the generation (count of targets).
    Committed(usize),
    /// The lane could not run for this build; legacy dispatch takes over.
    Unavailable(String),
    /// The lane ran and failed; every participant was asked to abort and NO
    /// legacy rebuild is attempted (remote replication stays degraded).
    Failed(String),
}

/// Replicates one sealed generation to every remote target through the artifact-transfer protocol:
/// Begin/Chunk/Finalize materialize the exact package, Prepare stages and proves a hidden
/// candidate, Commit only once EVERY target holds a matching prepared receipt.
#[allow(clippy::too_many_arguments)]
async fn generation_fanout(
    cloud: &Arc<CloudState>,
    bid: &str,
    project: &str,
    incarnation: ProjectIncarnation,
    tenant: &str,
    production: bool,
    req: &GitDeployRequest,
    source: Option<&fluid_build::SourceSnapshot>,
    seal: Option<&GenerationSeal>,
    sealed_artifact: Option<&hive_backend::SealedRuntimeArtifact>,
    targets: &[crate::schedule::Target],
    remote: &[crate::schedule::Target],
) -> GenerationFanoutOutcome {
    use crate::runtime_artifact_transfer_sender as sender;
    let log = |s: String| cloud.builds.log(bid, s);
    let Some(source) = source else {
        return GenerationFanoutOutcome::Unavailable(
            "this build proves no immutable source identity".into(),
        );
    };
    let Some(seal) = seal else {
        return GenerationFanoutOutcome::Unavailable(
            "this build seals no typed build authority".into(),
        );
    };
    let Some(sealed_artifact) = sealed_artifact else {
        return GenerationFanoutOutcome::Unavailable(
            "this build holds no sealed runtime artifact".into(),
        );
    };
    let advertised: HashMap<String, Option<u16>> = cloud
        .registry
        .nodes()
        .into_iter()
        .map(|node| (node.name, node.artifact_transfer_protocol))
        .collect();
    for target in remote {
        if advertised.get(&target.node).copied().flatten()
            != Some(crate::runtime_artifact_transfer_wire::PROTOCOL_VERSION)
        {
            return GenerationFanoutOutcome::Unavailable(format!(
                "target {} does not advertise artifact transfer protocol v{}",
                target.node,
                crate::runtime_artifact_transfer_wire::PROTOCOL_VERSION
            ));
        }
    }
    let snapshot =
        match fluid_build::DeploymentBuildSnapshot::new(fluid_build::DeploymentBuildContract {
            schema: fluid_build::DEPLOYMENT_BUILD_SNAPSHOT_SCHEMA,
            source: source.clone(),
            authority: seal.authority.clone(),
        }) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return GenerationFanoutOutcome::Failed(format!(
                    "deployment build snapshot refused: {error}"
                ))
            }
        };
    let transaction_id = mint_transfer_transaction_id(bid, snapshot.digest());
    let generation = match sender::TransferGeneration::new(
        transaction_id,
        project,
        incarnation,
        tenant,
        cloud.node_name.clone(),
        snapshot,
        seal.manifest_bytes.clone(),
        production,
        req.creator.clone().unwrap_or_default(),
        targets.to_vec(),
    ) {
        Ok(generation) => generation,
        Err(error) => {
            return GenerationFanoutOutcome::Failed(format!(
                "transfer generation refused: {error:#}"
            ))
        }
    };
    let package = match sealed_artifact.verified_package() {
        Ok(package) => package,
        Err(error) => {
            return GenerationFanoutOutcome::Failed(format!(
                "sealed runtime package refused reverification: {error:#}"
            ))
        }
    };
    let descriptor = package.descriptor().clone();
    log(format!(
        "Immutable generation {}: transferring the sealed artifact ({} bytes) to {} remote target(s)…",
        &descriptor.semantic_tree_sha256[..12],
        descriptor.package_bytes,
        remote.len()
    ));
    match sender::send_verified_runtime_artifact(cloud, generation.clone(), package).await {
        Ok(receipts) => log(format!(
            "Immutable generation: {} target(s) materialized and verified the exact artifact.",
            receipts.len()
        )),
        Err(error) => {
            sender::abort_generation_targets(cloud, &generation, &descriptor).await;
            return GenerationFanoutOutcome::Failed(format!("artifact transfer: {error:#}"));
        }
    }
    let prepared = match sender::prepare_transferred_targets(cloud, &generation, &descriptor).await
    {
        Ok(prepared) => {
            log(format!(
                "Immutable generation: {} target(s) prepared hidden candidates and proved readiness.",
                prepared.len()
            ));
            prepared
        }
        Err(error) => {
            sender::abort_generation_targets(cloud, &generation, &descriptor).await;
            return GenerationFanoutOutcome::Failed(format!("prepare: {error:#}"));
        }
    };
    match sender::commit_prepared_targets(cloud, &generation, &descriptor, &prepared).await {
        Ok(committed) => GenerationFanoutOutcome::Committed(committed.len()),
        Err(error) => {
            sender::abort_generation_targets(cloud, &generation, &descriptor).await;
            GenerationFanoutOutcome::Failed(format!(
                "commit: {error:#} (some targets may already serve the generation; aborted the rest)"
            ))
        }
    }
}

/// Mint one transfer transaction id: unique per attempt, bound to this build
/// and the exact generation snapshot it replicates.
fn mint_transfer_transaction_id(bid: &str, snapshot_digest: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"hive-artifact-transfer-transaction-v1\0");
    hash.update(bid.as_bytes());
    hash.update(snapshot_digest.as_bytes());
    hash.update(now_ms().to_be_bytes());
    hash.update(Uuid::new_v4().as_bytes());
    format!("{:x}", hash.finalize())
}

/// What happened to ONE target of a fan-out, kept distinct instead of a bare `bool`:
/// `DispatchFailed` means the request never reached the node (its application NEVER RAN, nothing is
/// known about whether it builds) while `BuildFailed` means the node ran the deploy and the
/// application genuinely failed. Folding both into `false` let one unreachable node veto an
/// otherwise successful multi-region deploy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetOutcome {
    /// The remote build reached a Ready deployment.
    Ready,
    /// The deploy was DELIVERED and the remote build reported failure — a real
    /// application fault, and the only outcome that may fail a deployment.
    BuildFailed,
    /// The request never reached this node (no transport succeeded) or it accepted it but its state
    /// could never be read back: nothing ran there, a FLEET-health condition, not an app fault.
    DispatchFailed,
    /// The user cancelled the build.
    Cancelled,
    /// The node received the deploy and deliberately DECLINED to host it (the stateful
    /// `fanout_secondary` guard stamps its build Ready with NO deployment id because it registers
    /// no replica). It must never count toward promotion (it reads as `state: "ready"` on the wire,
    /// and a deploy whose primary was unreachable and whose secondaries all declined would promote
    /// with ZERO nodes serving) nor fail the deploy: declining is the designed behavior for a
    /// single-writer service.
    Declined,
}

/// How many fallback candidates a pure-remote deploy tries after EVERY placed target proved
/// unreachable (`HIVE_DEPLOY_DISPATCH_FALLBACK_MAX`, default 3); each is bounded by
/// `fanout_remote`'s per-target budget, so the worst case is
/// `(1 + this) x (15 s HTTP + 2 x 20 s iroh)` of dispatch time, sized like the sandbox delegation's
/// candidate walk, not an unbounded sweep.
fn dispatch_fallback_max() -> usize {
    std::env::var("HIVE_DEPLOY_DISPATCH_FALLBACK_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(3)
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
    /// Every target was unreachable: the request arrived NOWHERE, so no node ran, declined or
    /// failed the app, the one condition under which trying another node is free of side effects
    /// (see the dispatch fallback in `run_build`).
    pub(crate) fn nothing_ran(&self) -> bool {
        !self.per_target.is_empty()
            && self
                .per_target
                .iter()
                .all(|(_, o)| *o == TargetOutcome::DispatchFailed)
    }
    /// PROMOTION POLICY: at least one target is genuinely serving. Chosen over quorum deliberately:
    /// each target of a stateless fan-out is an INDEPENDENT full replica (the sub-deploys carry
    /// `no_fanout: true` and register their own deployment records), so one Ready region is a
    /// working deployment, not a minority of a consensus group; quorum would discard a region that
    /// is demonstrably serving because a DIFFERENT region is unreachable.
    pub(crate) fn promotable(&self) -> bool {
        self.ready() > 0
    }
}

/// Dispatches a per-target deploy to each remote target's admin and MIRRORS its build into this
/// coordinator's build record (the dashboard's build page streams the real remote log); returns the
/// PER-TARGET outcome (see [`TargetOutcome`]). Each dispatched deploy carries `no_fanout: true`
/// (the target builds and hosts, no recursion), the project's current env and the owning team
/// header. `primary_first` says whether `remote[0]` is the deploy's designated PRIMARY (true on the
/// pure-remote placement branch and on a pinned single-host redeploy, false on the multi-region
/// tail where THIS coordinator hosted the build); every non-primary target gets
/// `fanout_secondary: true`, the signal `run_build`'s stateful fanout-replica guard needs, since a
/// `no_fanout` sub-build skips both coordinator-side placement gates.
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
    let build_config = serde_json::to_value(settings.build.clone()).ok();
    let function_settings = serde_json::to_value(settings.functions.clone()).ok();
    let dispatch_futs = remote.iter().enumerate().map(|(idx, t)| {
        let mut dreq = req.clone();
        dreq.no_fanout = true;
        let dispatch_trust = resolve_build_trust(cloud, &dreq, project, incarnation).ok();
        if let Some(context) = &dispatch_trust {
            dreq.target = Some(context.lane.environment().into());
        }
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
        let mut attempt_failures: Vec<String> = Vec::new();
        let resp_json: Option<serde_json::Value> = if let Some(admin) = &t.admin {
            let mut rb = cloud
                .http
                .post(format!("{admin}{RUNTIME_ARTIFACT_FANOUT_PATH}"))
                .header("x-hive-team", team.clone());
            if crate::auth::enforced() {
                if let Ok(tok) = crate::auth::issue("mesh-internal", &team, "service", false, crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS) {
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
                let mut iroh_result = None;
                let mut last_failure = String::new();
                for iroh_attempt in 1..=2 {
                    let t0 = std::time::Instant::now();
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
                        Some(b) => {
                            iroh_result = Some(b);
                            break;
                        }
                        None => {
                            let elapsed_ms = t0.elapsed().as_millis();
                            last_failure = if elapsed_ms >= 19_500 {
                                format!("iroh: no reply within the 20s request budget ({elapsed_ms}ms; the peer accepted nothing or answered nothing)")
                            } else {
                                format!("iroh: dial failed after {elapsed_ms}ms (no QUIC path to the peer: cached-hint connect timed out and fresh discovery gave up)")
                            };
                            if iroh_attempt < 2 {
                                cloud.builds.log(bid, format!("→ {}: iroh dispatch timed out, retrying once", t.node));
                            }
                        }
                    }
                }
                match iroh_result {
                    Some(b) => match serde_json::from_slice(&b) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            attempt_failures.push(format!("iroh: unparseable reply ({e})"));
                            None
                        }
                    },
                    None => {
                        attempt_failures.push(last_failure);
                        None
                    }
                }
            }
            None => None,
        };
        let Some(resp_json) = resp_json else {
            if attempt_failures.is_empty() {
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

    let dispatched: Vec<Option<(String, bool)>> = futures::future::join_all(dispatch_futs).await;

    let mut live: Vec<(&crate::schedule::Target, String)> = Vec::new();
    let mut per_target: Vec<(String, TargetOutcome)> = Vec::new();
    for (t, d) in remote.iter().zip(dispatched.into_iter()) {
        match d {
            None => {
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

    let mirrors = futures::future::join_all(
        live.iter()
            .map(|(t, target_bid)| mirror_remote_build(cloud, bid, t, target_bid, &t.node)),
    )
    .await;
    for ((t, _), outcome) in live.iter().zip(mirrors.iter()) {
        per_target.push((t.node.clone(), *outcome));
    }

    let sync_from = live
        .iter()
        .zip(mirrors.iter())
        .find(|(entry, o)| **o == TargetOutcome::Ready && entry.0.admin.is_some())
        .map(|(entry, _)| entry.0);
    if let Some(t) = sync_from {
        if let Some(admin) = &t.admin {
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

/// Polls a remote node's `/v1/builds/{id}` and streams NEW log lines into this build record
/// (prefixed with the node name) until it reaches a terminal state or times out; on success copies
/// the remote deployment's id + alias onto this record so the dashboard shows the live URL.
async fn mirror_remote_build(
    cloud: &Arc<CloudState>,
    bid: &str,
    target: &crate::schedule::Target,
    target_bid: &str,
    node: &str,
) -> TargetOutcome {
    let mut mirrored = 0usize;
    let mut polls_failed = 0usize;
    let idle_window_ms = std::env::var("HIVE_BUILD_MIRROR_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(10 * 60 * 1000);
    let ceiling_ms = std::env::var("HIVE_BUILD_MIRROR_MAX_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(120 * 60 * 1000);
    let started = now_ms();
    let mut deadline = started + idle_window_ms;
    let team = cloud
        .builds
        .get(bid)
        .map(|b| cloud.projects.team_of(&b.project))
        .unwrap_or_default();
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if cloud.build_cancels.is_cancelled(bid) {
            cloud
                .builds
                .log(bid, format!("{node}: mirror stopped (build cancelled)"));
            return TargetOutcome::Cancelled;
        }
        let team_qs = crate::admin::mesh_team_qs(&team);
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
                cloud.builds.log(bid, format!("✗ {node}: lost contact with remote build after {polls_failed} failed polls"));
                return TargetOutcome::DispatchFailed;
            }
            continue;
        };
        polls_failed = 0;
        deadline = now_ms() + idle_window_ms;
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
            if let Some(owner) = cloud
                .builds
                .get(bid)
                .and_then(|b| b.project_incarnation.map(|inc| (b.project, inc)))
            {
                let _ = cloud
                    .projects
                    .mark_first_deployed_exact(&owner.0, owner.1, now_ms());
            }
            cloud.builds.log(bid, format!("✓ {node}: deployment ready"));
            return TargetOutcome::Ready;
        }
        if state.eq_ignore_ascii_case("error") {
            if let Some(dep) = v.get("deployment_id").and_then(|x| x.as_str()) {
                let dep = dep.to_string();
                cloud.builds.update(bid, |b| {
                    b.deployment_id = Some(dep.clone());
                });
            }
            cloud.builds.log(bid, format!("✗ {node}: build failed"));
            return TargetOutcome::BuildFailed;
        }
        if now_ms().saturating_sub(started) > ceiling_ms {
            cloud.builds.log(
                bid,
                format!(
                    "✗ {node}: remote build exceeded the {} min mirror ceiling",
                    ceiling_ms / 60_000
                ),
            );
            return TargetOutcome::BuildFailed;
        }
    }
}

/// Resolves a request's environment WITHOUT running the build: `Some(true/false)` when this node
/// can classify it (explicit target, or branch vs a locally KNOWN production_branch), `None` when
/// it genuinely cannot; callers that gate destructive production-lane actions treat `None` as "not
/// production".
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

/// Relocation cleanup once a placement is serving: drops this node's stale PRODUCTION copies
/// (`remove_project_superseded`, never previews; the project settings stay, the coordinator holds
/// the authoritative env/build config) when it is not a target, and broadcasts the SCOPED reap
/// (`dispatch_deployments_reap`, never the full project delete) CONCURRENTLY to every healthy
/// non-target peer, since one registry-healthy but unreachable peer costs ~107 s serially.
/// Best-effort; never fails the deploy.
async fn cleanup_non_targets(cloud: &Arc<CloudState>, project: &str, target_names: &[String]) {
    let me = cloud.node_name.clone();
    if !target_names.iter().any(|t| t == &me) {
        let hosts_locally = cloud.gw.served_hosts().iter().any(|h| {
            let sub = h.split('.').next().unwrap_or(h);
            sub == project || sub.starts_with(&format!("{project}-"))
        });
        if hosts_locally {
            let removed = cloud.gw.remove_project_superseded(project).await;
            tracing::info!(
                project,
                removed = removed.len(),
                "relocation: removed stale local production copy from coordinator"
            );
            crate::persist::persist(cloud);
        }
    }
    let team = cloud.projects.team_of(project);
    let peers: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.healthy)
        .map(|n| n.name)
        .collect();
    futures::future::join_all(
        peers
            .iter()
            .filter(|node| !target_names.iter().any(|t| t == *node))
            .map(|node| crate::admin::dispatch_deployments_reap(cloud, node, project, &team)),
    )
    .await;
}

/// Best-effort GitHub Commit Status (the Vercel-style "shadw — Deployment ready" check on the
/// commit/PR): a no-op unless `GITHUB_TOKEN` is set and the repo is on github.com, and every
/// failure is swallowed so deploys never depend on GitHub being reachable.
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
        "state": state,
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
    coordinates: crate::build_coordinates::MonorepoCoordinates,
    build_contract: crate::app_discovery::BuildContract,
    workspace: Option<crate::workspace::Workspace>,
    repository_build: fluid_build::RepositoryBuildSnapshot,
    foreign_subdir: bool,
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
    root_package_manager: &fluid_build::PackageManagerDetection,
    root_workspace: Option<&crate::workspace::Workspace>,
    application_source: &str,
    application_evidence: Vec<String>,
    application_decision_digest: Option<String>,
    first_deploy: bool,
    use_cache: bool,
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

    let root_is_workspace = root_workspace.is_some();
    let is_monorepo = dir != repo_root && workspace_member && root_is_workspace;
    let install_dir = if is_monorepo {
        repo_root.to_path_buf()
    } else {
        dir.to_path_buf()
    };
    let foreign_subdir = !is_monorepo && dir != repo_root && root_is_workspace;
    let package_manager = if install_dir == repo_root {
        root_package_manager.clone()
    } else {
        fluid_build::detect_package_manager_checked(&install_dir).map_err(|error| {
            fluid_build::BuildContractError::invalid_metadata(
                "resolve selected-root package-manager metadata",
                error,
            )
        })?
    };
    let workspace = if install_dir == repo_root {
        root_workspace.cloned()
    } else {
        crate::workspace::load_for_manager(&install_dir, &package_manager)
            .await
            .map_err(|error| {
                fluid_build::BuildContractError::invalid_metadata(
                    "load selected-root workspace metadata",
                    error,
                )
            })?
    };
    crate::workspace::validate(&install_dir, workspace.as_ref(), &package_manager)
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
    let build_contract = crate::app_discovery::build_contract(
        repo_root,
        dir,
        is_monorepo,
        root_workspace,
        build_override.is_some(),
    )
    .await?;
    let build_cwd = dir;
    let output_relative = if install_override.is_some() || build_override.is_some() {
        Path::new(".")
    } else if resolution.build_output.is_some() {
        Path::new(".vercel/output")
    } else {
        Path::new(resolution.plan.output_dir.as_str())
    };
    let coordinates = crate::build_coordinates::MonorepoCoordinates::new(
        repo_root,
        dir,
        &install_dir,
        build_cwd,
        output_relative,
    )?;
    let use_npm_ci = should_use_npm_ci(
        package_manager.manager,
        package_manager.lockfile == Some(fluid_build::PackageManagerLockfile::Npm),
        first_deploy,
        use_cache,
    );
    let repository_build =
        crate::repository_build::snapshot(crate::repository_build::SnapshotInput {
            checkout_root: repo_root,
            workspace: workspace.as_ref(),
            application_source,
            application_evidence,
            application_decision_digest,
            package_manager: &package_manager,
            framework_source: if framework_override.is_some() {
                "explicit"
            } else {
                "detected"
            },
            plan: &resolution.plan,
            install_override: install_override.clone(),
            build_override: build_override.clone(),
            build_contract: &build_contract,
            coordinates: &coordinates,
            use_npm_ci,
        })
        .await?;

    Ok(FdiPreparation {
        build_config,
        settings_are_auto,
        install_override,
        build_override,
        runtime_override,
        framework,
        coordinates,
        build_contract,
        workspace,
        repository_build,
        foreign_subdir,
        package_manager,
        plan: resolution.plan,
        build_output: resolution.build_output,
        vercel_config_present: vercel_config.is_some(),
    })
}

/// Produces the deployment manifest from a cloned repo in one of five shapes: prebuilt image,
/// docker-compose, Dockerfile (podman), explicit `fluid.json` or Framework-Defined Infrastructure;
/// only the plain-`fluid.json` branch deserializes the file into a `Manifest` (see
/// `fluid_json_browser_optins`).
async fn produce_manifest(
    cloud: &Arc<CloudState>,
    bid: &str,
    isolated: Option<&mut IsolatedBuild>,
    _repo_root: &Path,
    dir: &Path,
    project: &str,
    incarnation: ProjectIncarnation,
    _commit: &str,
    first_deploy: bool,
    use_cache: bool,
    build_env: &std::collections::BTreeMap<String, String>,
    trust: &BuildTrustContext,
    _vercel_config: Option<&fluid_build::VercelConfig>,
    fdi_preparation: Option<FdiPreparation>,
    _workspace_member: bool,
    image_ref: Option<&str>,
    image_port: Option<u16>,
    image_protocol: Option<ServiceProtocol>,
    image_memory: Option<&str>,
    image_cpus: Option<&str>,
    image_pids: u32,
    image_ports: Option<Vec<fluid_core::PortSpec>>,
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);
    if let Some(image) = image_ref {
        let mem_mib = image_memory.map(parse_mem_mib).unwrap_or(0);
        let cpus = image_cpus.map(parse_cpus_quota).unwrap_or(0.0);
        let skip_pull = false;
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
            skip_pull,
        )
        .await;
    }
    let mut isolated = isolated;
    let compose_path = crate::compose::compose_file(dir);
    if let Some(compose_path) = compose_path {
        return compose_container_manifest(
            cloud,
            bid,
            isolated,
            dir,
            &compose_path,
            project,
            incarnation,
            image_port,
            image_protocol,
            image_memory,
            image_cpus,
            image_pids,
            image_ports,
        )
        .await;
    }
    let dockerfile = container_build_file(dir);
    if let Some(dockerfile) = dockerfile {
        return dockerfile_container_manifest(
            cloud,
            bid,
            isolated,
            dir,
            &dockerfile,
            project,
            incarnation,
            image_port,
            image_protocol,
            image_memory,
            image_cpus,
            image_pids,
            image_ports,
        )
        .await;
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
        let preparation = fdi_preparation.ok_or_else(|| {
            anyhow::anyhow!(
                "repository build snapshot is missing before framework-defined build execution"
            )
        })?;
        if let Some(warning) = preparation.package_manager.conflict_warning.as_ref() {
            log(format!("WARN: {warning}"));
        };
        build_via_fdi(
            cloud,
            bid,
            isolated,
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

/// The module a direct-interpreter `start_cmd` names, when the argv is one plain `node <entry>` /
/// `bun <entry>` / `bun run <entry>` launch; `None` for every other shape (package-manager and
/// script indirection, `next start`, container/python/command runtimes, stdin/eval/REPL forms, any
/// Node option that takes the NEXT argv as its value such as `-r x` or `--import x`, a short `-x`
/// option), meaning "not this check's business": the backend's own launch validation still runs.
/// Only `--flag` / `--flag=value` options are skipped ahead of the entry.
fn direct_launch_entry(start_cmd: &[String]) -> Option<String> {
    let first = start_cmd.first()?;
    let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
    let mut rest = start_cmd[1..].iter();
    let candidate = match base {
        "node" | "nodejs" => loop {
            let arg = rest.next()?;
            if !arg.starts_with('-') {
                break arg;
            }
            let value_taking = matches!(
                arg.as_str(),
                "--require"
                    | "--import"
                    | "--loader"
                    | "--experimental-loader"
                    | "--eval"
                    | "--print"
                    | "--input-type"
                    | "--interactive"
            );
            if value_taking || !arg.starts_with("--") {
                return None;
            }
        },
        "bun" => {
            let arg = rest.find(|arg| !arg.starts_with('-'))?;
            let arg = if arg == "run" {
                rest.find(|arg| !arg.starts_with('-'))?
            } else {
                arg
            };
            let path_shaped = arg.contains('/')
                || Path::new(arg).extension().is_some_and(|extension| {
                    matches!(
                        extension.to_str(),
                        Some("js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx")
                    )
                });
            if !path_shaped {
                return None;
            }
            arg
        }
        _ => return None,
    };
    let path = Path::new(candidate);
    let plain_relative = !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        && path
            .components()
            .any(|component| matches!(component, Component::Normal(_)));
    plain_relative.then(|| candidate.clone())
}

/// Build-time launch-shape preflight: every function whose `start_cmd` is a direct `node`/`bun`
/// launch ([`direct_launch_entry`]) must have its entry module present in the runtime artifact tree
/// about to be sealed (`checkout_root/app_rel/cwd_relative/<entry>`, plus Node's extensionless
/// `.js`/`.mjs`/`.cjs` resolution; a directory counts). Deliberately conservative: it fires only
/// when NOTHING exists at the entry, the one case provably unlaunchable on every backend. The
/// refusal names the function, the entry, the exact path examined, what the sealed root actually
/// contains and the field that chose the entry (`fluid.json functions[N].start_cmd`, whose lane
/// ships the checkout as-is with no install/build, or the build-derived start command).
async fn preflight_direct_entries(
    manifest: &Manifest,
    runtime_artifact: &hive_backend::RuntimeArtifactSpec,
    fluid_json_present: bool,
) -> anyhow::Result<()> {
    let host_root = runtime_artifact.host_static_root()?;
    for (index, function) in manifest.functions.iter().enumerate() {
        let runtime = hive_core::Runtime::resolve(&function.runtime, &function.start_cmd);
        if !matches!(runtime, hive_core::Runtime::Node | hive_core::Runtime::Bun) {
            continue;
        }
        let Some(entry) = direct_launch_entry(&function.start_cmd) else {
            continue;
        };
        let cwd = function
            .cwd_relative
            .as_deref()
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty() && *cwd != ".")
            .map(Path::new);
        let base = match cwd {
            Some(cwd) => host_root.join(cwd),
            None => host_root.clone(),
        };
        let candidates: Vec<PathBuf> = std::iter::once(base.join(&entry))
            .chain(
                ["js", "mjs", "cjs"]
                    .iter()
                    .map(|extension| base.join(format!("{entry}.{extension}"))),
            )
            .collect();
        let mut present = false;
        for candidate in &candidates {
            if tokio::fs::symlink_metadata(candidate).await.is_ok() {
                present = true;
                break;
            }
        }
        if present {
            continue;
        }

        let mut examined = runtime_artifact.app_rel.clone();
        if let Some(cwd) = cwd {
            examined.push(cwd);
        }
        examined.push(&entry);
        let examined = examined
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect::<PathBuf>()
            .to_string_lossy()
            .replace('\\', "/");
        let mut root_listing: Vec<String> = match tokio::fs::read_dir(&base).await {
            Ok(mut entries) => {
                let mut names = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let mut name = entry.file_name().to_string_lossy().into_owned();
                    if entry
                        .file_type()
                        .await
                        .map(|kind| kind.is_dir())
                        .unwrap_or(false)
                    {
                        name.push('/');
                    }
                    names.push(name);
                }
                names
            }
            Err(_) => Vec::new(),
        };
        root_listing.sort();
        let shown = root_listing.len().min(16);
        let mut listing = root_listing[..shown].join(", ");
        if root_listing.len() > shown {
            listing.push_str(&format!(", … ({} more)", root_listing.len() - shown));
        }
        let scripts = tokio::fs::read_to_string(base.join("package.json"))
            .await
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .map(|package| {
                let pick = |key: &str| {
                    package
                        .get("scripts")
                        .and_then(|scripts| scripts.get(key))
                        .and_then(|value| value.as_str())
                        .map(|value| format!("scripts.{key} = {value:?}"))
                };
                let main = package
                    .get("main")
                    .and_then(|value| value.as_str())
                    .map(|value| format!("main = {value:?}"));
                [pick("start"), pick("build"), main]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|summary| !summary.is_empty());
        let interpreter = function.start_cmd.first().cloned().unwrap_or_default();
        let function_label = if function.name.is_empty() {
            format!("functions[{index}]")
        } else {
            format!("{:?} (functions[{index}])", function.name)
        };
        let deciding_field = if fluid_json_present {
            format!(
                "fluid.json functions[{index}].start_cmd = {:?}",
                function.start_cmd
            )
        } else {
            format!(
                "the build-derived start command {:?} (package.json scripts.start / framework detection)",
                function.start_cmd
            )
        };
        let lane = if fluid_json_present {
            "A fluid.json deployment ships the repository checkout AS-IS — no install or build \
             command runs (\"Detected fluid.json — using project configuration\") — so the entry \
             must be committed to the repository. Fix one of: commit the file the start_cmd \
             names; point start_cmd at a committed file; or remove fluid.json so framework \
             detection runs the install + build and derives the start command from package.json."
                .to_string()
        } else {
            "The install/build steps that ran did not produce it — check the build output and \
             the framework's output directory, or set an explicit start command in the project's \
             build settings."
                .to_string()
        };
        let msg = format!(
            "Launch preflight failed for function {function_label}: main entry {entry:?} — chosen \
             by {deciding_field} — does not exist in the deployment tree about to be sealed. \
             Looked for {examined:?} (and {entry}.js/.mjs/.cjs) relative to the repository \
             checkout; the deployment root contains [{listing}]{}. {lane} The `{interpreter}` \
             process would exit immediately on every backend, so the deployment was NOT \
             registered.",
            scripts
                .map(|summary| format!("; package.json declares {summary}"))
                .unwrap_or_default(),
        );
        return Err(anyhow::anyhow!(msg));
    }
    Ok(())
}

/// Whether to use `npm ci` (clean, lockfile-exact install) instead of `npm install`: only for npm
/// projects with a committed `package-lock.json`, and only on the project's first deployment or
/// when the build cache was explicitly disabled on a redeploy; every other build uses `npm install`
/// + cache.
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
            "npm" => "install -D --no-save --package-lock=false --no-audit --no-fund --legacy-peer-deps \"$spec\"",
            "pnpm" => "add -D --lockfile=false --config.strict-peer-dependencies=false \"$spec\"",
            "yarn" => "add -D \"$spec\"",
            "bun" => "add -d \"$spec\"",
            _ => unreachable!("launcher validates package-manager name"),
        };
        self.invoke(arguments)
    }
}

const PNPM_DEPENDENCY_ROOT_CLEAN_SCRIPT: &str = r#"set -eu
for dependency_root do
  case "$dependency_root" in
    node_modules) ;;
    */node_modules)
      parent=${dependency_root%/node_modules}
      remaining=$parent
      current=
      while [ -n "$remaining" ]; do
        component=${remaining%%/*}
        case "$component" in
          ''|.|..)
            printf '%s\n' "UNSAFE_BUILD_INPUT: invalid workspace dependency root $dependency_root" >&2
            exit 41
            ;;
        esac
        current=${current:+$current/}$component
        if [ -L "$current" ]; then
          printf '%s\n' "UNSAFE_BUILD_INPUT: workspace dependency-root parent $current may not be a symlink" >&2
          exit 41
        fi
        if [ ! -e "$current" ]; then
          break
        fi
        if [ ! -d "$current" ]; then
          printf '%s\n' "UNSAFE_BUILD_INPUT: workspace dependency-root parent $current is not a directory" >&2
          exit 41
        fi
        if [ "$remaining" = "$component" ]; then
          remaining=
        else
          remaining=${remaining#*/}
        fi
      done
      ;;
    *)
      printf '%s\n' "UNSAFE_BUILD_INPUT: invalid workspace dependency root $dependency_root" >&2
      exit 41
      ;;
  esac
  if [ -L "$dependency_root" ]; then
    printf '%s\n' "UNSAFE_BUILD_INPUT: workspace dependency root $dependency_root may not be a symlink" >&2
    exit 41
  fi
  if [ -e "$dependency_root" ]; then
    if [ ! -d "$dependency_root" ]; then
      printf '%s\n' "UNSAFE_BUILD_INPUT: workspace dependency root $dependency_root is not a directory" >&2
      exit 41
    fi
    rm -rf -- "$dependency_root"
  fi
done
"#;

fn pnpm_runtime_dependency_roots(
    workspace: &crate::workspace::Workspace,
) -> anyhow::Result<Vec<String>> {
    let mut roots = vec!["node_modules".to_string()];
    for member in &workspace.members {
        let mut relative = String::new();
        for component in member.components() {
            let std::path::Component::Normal(name) = component else {
                anyhow::bail!(
                    "workspace member {} is not a normalized repository-relative path",
                    member.display()
                );
            };
            let name = name.to_str().ok_or_else(|| {
                anyhow::anyhow!("workspace member {} is not valid UTF-8", member.display())
            })?;
            anyhow::ensure!(
                !name.is_empty() && name != "." && name != ".." && name != "node_modules",
                "workspace member {} contains an invalid dependency-root component",
                member.display()
            );
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(name);
        }
        anyhow::ensure!(
            !relative.is_empty(),
            "workspace member path may not resolve to the package-manager root"
        );
        roots.push(format!("{relative}/node_modules"));
    }
    roots.sort_unstable();
    roots.dedup();
    Ok(roots)
}

/// Framework-Defined Infrastructure: detects the framework, runs its real install + build commands
/// (streamed) and normalizes the output into a Manifest (static assets or a serverless server), the
/// executor that turns a source repo into the Build Output API contract (`fluid-build`).
async fn build_via_fdi(
    cloud: &Arc<CloudState>,
    bid: &str,
    mut isolated: Option<&mut IsolatedBuild>,
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
        coordinates,
        build_contract: _,
        workspace,
        repository_build,
        foreign_subdir,
        mut package_manager,
        plan,
        build_output,
        vercel_config_present,
    } = preparation;
    let install_dir = coordinates.install_root();
    let build_exec_dir = coordinates.build_cwd();
    let is_monorepo = coordinates.is_monorepo();
    if vercel_config_present {
        log("Detected vercel.json — applying configuration overrides.".into());
    }
    if let Some(runtime) = runtime_override {
        log(format!(
            "Explicit runtime: {} (vercel.json/project settings).",
            runtime.as_str()
        ));
    }

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

    if runtime_override == Some(hive_core::Runtime::Wasmer) {
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

    let steps = &repository_build.contract().steps;
    let runs_repository_command = !matches!(steps.install, fluid_build::StepAuthority::None)
        || !matches!(steps.build, fluid_build::StepAuthority::None);
    let available_toolchains = if runs_repository_command {
        probe_build_toolchains(&mut isolated, &install_dir, cloud, bid, build_env).await
    } else {
        None
    };
    let detected_manager = package_manager.manager;
    let mut toolchain_substituted = false;
    if let Some(available) = available_toolchains.as_ref() {
        if !available.iter().any(|tool| tool == package_manager.manager) {
            match crate::resources::package_manager_substitute(package_manager.manager, available) {
                Some(substitute) => {
                    let requested = package_manager.manager;
                    log(format!(
                        "Build toolchain substitution: this repository resolves package manager \
                         `{requested}` ({:?}), but `{requested}` is absent from this node's build \
                         environment [{}]. Using `{substitute}` instead — every install/build \
                         command below runs with `{substitute}`. Provision `{requested}` on this \
                         node's build filesystem to use it.",
                        package_manager.source,
                        available.join(","),
                    ));
                    tracing::warn!(
                        project = %project,
                        requested, substitute,
                        available = %available.join(","),
                        "BUILD_TOOLCHAIN_SUBSTITUTED: resolved package manager absent; degraded to a present one"
                    );
                    package_manager.manager = substitute;
                    package_manager.declaration = None;
                    package_manager.lockfile = None;
                    toolchain_substituted = true;
                }
                None => log(format!(
                    "WARN: this repository resolves package manager `{}`, which is absent from \
                     this node's build environment [{}], and no substitute is present — the \
                     install/build will fail naming it. Provision the toolchain on this node's \
                     build filesystem.",
                    package_manager.manager,
                    available.join(","),
                )),
            }
        }
    }

    let launcher = PackageManagerLauncher::new(&package_manager)?;

    log(format!(
        "Detected framework: {} — primitive: {:?}, package manager: {}{} ({:?}){}{}",
        plan.framework.name,
        plan.framework.primitive,
        package_manager.manager,
        if toolchain_substituted {
            format!(" — substituted for absent `{}`", detected_manager)
        } else {
            String::new()
        },
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

    log(format!(
        "Repository build snapshot: {} (selected app {}, install root {}, build cwd {}).",
        repository_build.digest(),
        repository_build
            .contract()
            .coordinates
            .selected_app
            .as_str(),
        repository_build
            .contract()
            .coordinates
            .install_root
            .as_str(),
        repository_build.contract().coordinates.build_cwd.as_str(),
    ));
    let (mut install_cmd, use_npm_ci) = match &steps.install {
        fluid_build::StepAuthority::None => (None, false),
        fluid_build::StepAuthority::Explicit(command) => {
            (Some(command.as_str().to_string()), false)
        }
        fluid_build::StepAuthority::Generated(fluid_build::GeneratedStep::Install {
            use_npm_ci,
        }) => (Some(launcher.install(*use_npm_ci)), *use_npm_ci),
        _ => anyhow::bail!("repository build snapshot carries an invalid install step"),
    };
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
    let rewrite_absent_tool = |command: &str| -> Option<(String, &'static str, &'static str)> {
        let available = available_toolchains.as_ref()?;
        let requested = command_toolchain(command)?;
        if available.iter().any(|tool| tool == requested) {
            return None;
        }
        let substitute = crate::resources::package_manager_substitute(requested, available)?;
        let rest: Vec<&str> = command.split_whitespace().skip(1).collect();
        let replacement = match rest.first().copied() {
            Some("install" | "i" | "ci" | "add") => launcher.install(false),
            Some("run") if rest.len() == 2 => format!("{substitute} run {}", rest[1]),
            _ => return None,
        };
        Some((replacement, requested, substitute))
    };
    let requested_install_tool = install_cmd.as_deref().and_then(command_toolchain);
    if let Some((replacement, requested, substitute)) =
        install_cmd.as_deref().and_then(rewrite_absent_tool)
    {
        log(format!(
            "Build toolchain substitution: the install command names `{requested}` (`{}`), \
             but `{requested}` is absent from this node's build environment [{}]. Running \
             `{replacement}` with `{substitute}` instead — the repository's own install \
             intent is preserved, only the tool it cannot find changes. Provision \
             `{requested}` on this node's build filesystem to run it as written.",
            install_cmd.as_deref().unwrap_or_default(),
            available_toolchains
                .as_ref()
                .map(|t| t.join(","))
                .unwrap_or_default(),
        ));
        tracing::warn!(
            project = %project,
            requested, substitute,
            "BUILD_TOOLCHAIN_SUBSTITUTED: explicit install command named an absent package manager"
        );
        install_cmd = Some(replacement);
    }
    if let Some(requested) = requested_install_tool {
        if requested != detected_manager {
            log(format!(
                "NOTE: repository package-manager detection resolved `{detected_manager}`, but this \
                 build's install command names `{requested}` — an explicit install command \
                 (vercel.json > Project Settings) overrides detection, so the two are expected to \
                 differ and `{requested}` is what runs."
            ));
        }
    }
    let mut turbo_repository_from_app = None;
    let mut build_cmd = match &steps.build {
        fluid_build::StepAuthority::None => None,
        fluid_build::StepAuthority::Explicit(command) => Some(command.as_str().to_string()),
        fluid_build::StepAuthority::Generated(fluid_build::GeneratedStep::RunBuildScript) => {
            Some(launcher.run_script("build"))
        }
        fluid_build::StepAuthority::Generated(fluid_build::GeneratedStep::FrameworkExec {
            argv,
        }) => Some(launcher.exec(
            &argv.as_slice().join(" "),
            runtime_override == Some(hive_core::Runtime::Bun),
        )),
        fluid_build::StepAuthority::Generated(fluid_build::GeneratedStep::TurboBuild {
            repository_from_app,
        }) => {
            turbo_repository_from_app = Some(repository_from_app.as_str().to_string());
            None
        }
        _ => anyhow::bail!("repository build snapshot carries an invalid build step"),
    };
    if let Some((replacement, requested, substitute)) =
        build_cmd.as_deref().and_then(rewrite_absent_tool)
    {
        log(format!(
            "Build toolchain substitution: the build command names `{requested}` (`{}`), \
             but `{requested}` is absent from this node's build environment [{}]. Running \
             `{replacement}` with `{substitute}` instead. Provision `{requested}` on this \
             node's build filesystem to run it as written.",
            build_cmd.as_deref().unwrap_or_default(),
            available_toolchains
                .as_ref()
                .map(|t| t.join(","))
                .unwrap_or_default(),
        ));
        tracing::warn!(
            project = %project,
            requested, substitute,
            "BUILD_TOOLCHAIN_SUBSTITUTED: explicit build command named an absent package manager"
        );
        build_cmd = Some(replacement);
    }
    if use_cache {
        log("Build cache bypassed: isolated cache import/export is not yet enabled.".into());
    } else {
        log("Build cache disabled — installing dependencies fresh.".into());
    }
    let install_is_explicit = matches!(&steps.install, fluid_build::StepAuthority::Explicit(_));
    if let Some(install_cmd) = install_cmd
        .as_deref()
        .filter(|command| !command.trim().is_empty())
    {
        let install_cwd: &Path = if install_is_explicit {
            &build_exec_dir
        } else {
            &install_dir
        };
        log(format!(
            "Running \"{}\"{}",
            install_cmd,
            if install_is_explicit && is_monorepo {
                " (selected app — explicit commands run in the Root Directory)"
            } else if is_monorepo {
                " (workspace root)"
            } else {
                ""
            }
        ));
        run_streamed(
            require_build_session(&mut isolated)?,
            install_cwd,
            install_cmd,
            cloud,
            bid,
            build_env,
        )
        .await
        .map_err(|error| classify_command_failure("install command failed", install_cmd, error))?;
    }
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
    if let Some(repository_from_app) = turbo_repository_from_app.as_deref() {
        const TURBO_BUILD_SCRIPT: &str = r#"set -eu
repo=$1
turbo=$repo/node_modules/.bin/turbo
if [ ! -x "$turbo" ]; then
  printf '%s\n' 'BUILD_TOOLCHAIN_MISSING: repository-local turbo executable' >&2
  exit 127
fi
exec "$turbo" run build
"#;
        log(format!(
            "Workspace Turbo build contract: selected-app cwd {}, repository-local executable only, fixed argv `turbo run build`, no package filter.",
            repository_build.contract().coordinates.build_cwd.as_str()
        ));
        require_build_session(&mut isolated)?
            .run(
                &build_exec_dir,
                TURBO_BUILD_SCRIPT,
                "run repository-local Turbo build",
                &[repository_from_app.to_string()],
                false,
                cloud,
                bid,
                build_env,
            )
            .await
            .context("repository-local Turbo build failed")?;
    } else if let Some(build_cmd) = build_cmd
        .as_deref()
        .filter(|command| !command.trim().is_empty())
    {
        log(format!("Running \"{}\"", build_cmd));
        run_streamed(
            require_build_session(&mut isolated)?,
            &build_exec_dir,
            build_cmd,
            cloud,
            bid,
            build_env,
        )
        .await
        .map_err(|error| classify_command_failure("build command failed", build_cmd, error))?;
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
        let launcher_script = r#"set -eu
p=.hive-express-launcher.mjs
[ ! -L "$p" ] || { printf '%s\n' 'UNSAFE_BUILD_INPUT: launcher path may not be a symlink' >&2; exit 41; }
tmp=$(mktemp .hive-express-launcher.XXXXXX)
trap 'rm -f -- "$tmp"' EXIT HUP INT TERM
printf '%s' "$1" >"$tmp"
chmod 0444 "$tmp"
mv -f -- "$tmp" "$p"
trap - EXIT HUP INT TERM
"#;
        require_build_session(&mut isolated)?
            .run(
                dir,
                launcher_script,
                "stage platform exported-app launcher",
                &[EXPRESS_LAUNCHER_JS.to_string()],
                false,
                cloud,
                bid,
                &std::collections::BTreeMap::new(),
            )
            .await
            .context("staging platform exported-app launcher failed")?;
        let worker_script = r#"set -eu
p=.hive-worker-launcher.mjs
[ ! -L "$p" ] || { printf '%s\n' 'UNSAFE_BUILD_INPUT: worker launcher path may not be a symlink' >&2; exit 41; }
tmp=$(mktemp .hive-worker-launcher.XXXXXX)
trap 'rm -f -- "$tmp"' EXIT HUP INT TERM
printf '%s' "$1" >"$tmp"
chmod 0444 "$tmp"
mv -f -- "$tmp" "$p"
trap - EXIT HUP INT TERM
"#;
        require_build_session(&mut isolated)?
            .run(
                dir,
                worker_script,
                "stage platform worker launcher",
                &[WORKER_LAUNCHER_JS.to_string()],
                false,
                cloud,
                bid,
                &std::collections::BTreeMap::new(),
            )
            .await
            .context("staging platform worker launcher failed")?;
    }
    if is_monorepo
        && package_manager.manager == "pnpm"
        && install_override.is_none()
        && build_override.is_none()
    {
        let workspace = workspace.as_ref().ok_or_else(|| {
            anyhow::anyhow!("nested pnpm application is missing its validated workspace snapshot")
        })?;
        let dependency_roots = pnpm_runtime_dependency_roots(workspace)?;
        log(format!(
            "Normalizing pnpm runtime dependencies from {} exact package-manager root(s): frozen, production-only, hoisted, offline, lifecycle scripts disabled.",
            dependency_roots.len()
        ));
        require_build_session(&mut isolated)?
            .run(
                &install_dir,
                PNPM_DEPENDENCY_ROOT_CLEAN_SCRIPT,
                "remove pnpm dependency materialization roots",
                &dependency_roots,
                false,
                cloud,
                bid,
                &std::collections::BTreeMap::new(),
            )
            .await
            .context("removing pnpm dependency materialization roots failed")?;
        let runtime_install = launcher.invoke(
            "install --prod --frozen-lockfile --config.node-linker=hoisted \
             --config.package-import-method=copy --offline --ignore-scripts",
        );
        run_streamed(
            require_build_session(&mut isolated)?,
            &install_dir,
            &runtime_install,
            cloud,
            bid,
            build_env,
        )
        .await
        .map_err(|error| {
            classify_command_failure(
                "pnpm runtime dependency normalization failed",
                &runtime_install,
                error,
            )
        })?;
        log(
            "pnpm runtime dependency graph normalized without traversing build-output directories."
                .into(),
        );
    }
    {
        const DELINK_SCRIPT: &str = r#"set -eu
[ -d node_modules ] || exit 0
find node_modules -type f -links +1 -exec sh -c '
for file do
  cp -p -- "$file" "$file.hive-delink"
  mv -f -- "$file.hive-delink" "$file"
done' hive-delink {} +
"#;
        require_build_session(&mut isolated)?
            .run(
                &install_dir,
                DELINK_SCRIPT,
                "re-materialize hardlinked dependency files",
                &[],
                false,
                cloud,
                bid,
                &std::collections::BTreeMap::new(),
            )
            .await
            .context("re-materializing hardlinked dependency files failed")?;
    }
    let explicit_commands = install_override.is_some() || build_override.is_some();
    if !explicit_commands {
        if let Some(output) = build_output.as_ref() {
            build_output_manifest(project, plan.framework.slug, output)?;
        }
    }
    if explicit_commands {
        require_build_session(&mut isolated)?.finish().await?;
    } else if let Some(session) = isolated.as_deref_mut() {
        let expected_output =
            fluid_build::OutputDirectory::parse(&coordinates.output_relative_to_app())?;
        session.finish_with_output(dir, &expected_output).await?;
    } else if !plan.output_dir.as_str().trim_matches('/').is_empty()
        && plan.output_dir.as_str() != "."
        && !coordinates.output_root().is_dir()
    {
        anyhow::bail!(
            "BUILD_ISOLATION_UNAVAILABLE: plan expects output directory {:?} which does not exist in the checkout, and no isolated build executor is available to produce it. No repository-controlled command was run on the host.",
            plan.output_dir.as_str()
        );
    }

    let parsed_build_output = fluid_build::resolve_build_output_checked(dir)?;
    if let Some(output) = parsed_build_output.as_ref() {
        log("Build Output API v3 detected (.vercel/output).".into());
        let mut manifest = build_output_manifest(project, plan.framework.slug, output)?;
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
            let mut start = if is_sveltekit && dir.join("build/index.js").exists() {
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
                    let launcher_entry =
                        if runtime_override.is_some_and(|r| r == hive_core::Runtime::Bun) {
                            None
                        } else {
                            discover_exported_server_entry(dir)
                        };
                    if let Some(entry) = launcher_entry {
                        let mut cmd = vec!["node".to_string()];
                        if entry.ends_with(".ts")
                            || entry.ends_with(".mts")
                            || entry.ends_with(".cts")
                        {
                            cmd.push("--experimental-transform-types".to_string());
                        }
                        cmd.push(EXPRESS_LAUNCHER_FILE.to_string());
                        cmd.push(entry.clone());
                        log(format!(
                            "No start script — serving the exported app entry `{entry}` via the \
                             platform launcher."
                        ));
                        start = cmd;
                    } else {
                        return Err(anyhow::anyhow!(
                            "build produced no usable production server entry for {}: the build \
                             directory {} has no package.json `scripts.start`, no recognizable server \
                             entry (server.js/index.js/src/index.ts/…), and its workspace root has no \
                             `scripts.start` either — a catch-all `{}` here would exit without ever \
                             binding $PORT and open the deployment circuit in production. For a \
                             monorepo, the selected app subdirectory (not the workspace root) is the \
                             directory that must own a `start` script.",
                            project,
                            dir.display(),
                            start.join(" ")
                        ));
                    }
                }
            }
            if let Some(entry) = discover_worker_entry(dir) {
                let mut cmd = vec!["node".to_string()];
                if entry.ends_with(".ts") || entry.ends_with(".mts") {
                    cmd.push("--experimental-transform-types".to_string());
                }
                cmd.push(WORKER_LAUNCHER_FILE.to_string());
                cmd.push(entry.clone());
                log(format!(
                    "Cloudflare Worker detected — hosting its exported `fetch` handler via the \
                     platform worker adapter (entry `{entry}`) instead of the `wrangler` CLI, \
                     which needs the Workers runtime and an account."
                ));
                start = cmd;
            }
            log(format!(
                "Provisioning serverless server: `{}`.",
                start.join(" ")
            ));
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

/// The exported-app launcher (Vercel zero-config Express parity): wraps an entry module that
/// `export default`s its app instead of binding $PORT; staged like the after shim and consumed only
/// by the no-start-script fallback in `build_via_fdi`.
const EXPRESS_LAUNCHER_JS: &str = include_str!("../assets/express-launcher.mjs");
const EXPRESS_LAUNCHER_FILE: &str = ".hive-express-launcher.mjs";
const WORKER_LAUNCHER_JS: &str = include_str!("../assets/worker-launcher.mjs");
const WORKER_LAUNCHER_FILE: &str = ".hive-worker-launcher.mjs";

/// Recognises a Cloudflare Worker application: a wrangler config plus an entry module whose default
/// export is (or owns) a `fetch` handler. Such repos cannot start the way their template does
/// (every `scripts.start` runs the `wrangler` CLI, which needs workerd and a Cloudflare account
/// that the Node substrate refuses), so the SAME exported handler is hosted on Node instead
/// (`assets/worker-launcher.mjs`).
fn discover_worker_entry(dir: &Path) -> Option<String> {
    let wrangler_config = ["wrangler.jsonc", "wrangler.json", "wrangler.toml"]
        .iter()
        .find(|name| dir.join(name).is_file())?;
    let configured = std::fs::read_to_string(dir.join(wrangler_config))
        .ok()
        .and_then(|raw| {
            for line in raw.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix("main") else {
                    continue;
                };
                let rest = rest.trim().trim_start_matches('=').trim();
                let value = rest.trim_matches(',').trim().trim_matches('"').trim();
                if !value.is_empty() && !value.contains("..") {
                    return Some(value.to_string());
                }
            }
            None
        });
    let mut candidates: Vec<String> = Vec::new();
    if let Some(main) = configured {
        candidates.push(main);
    }
    for prefix in ["src/", ""] {
        for stem in ["index", "worker", "main"] {
            for ext in [".ts", ".mts", ".js", ".mjs"] {
                candidates.push(format!("{prefix}{stem}{ext}"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|rel| !rel.contains("..") && !rel.starts_with('/') && dir.join(rel).is_file())
}

/// Finds an entry module that plausibly exports the app's server, for the no-start-script launcher
/// lane: `package.json` `main`/`module` win, then the conventional stems Vercel's zero-config Node
/// lane recognizes; a checkout-relative path, never following `..`.
fn discover_exported_server_entry(dir: &Path) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(raw) = std::fs::read_to_string(dir.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            for field in ["main", "module"] {
                if let Some(m) = v.get(field).and_then(|m| m.as_str()) {
                    let m = m.trim().trim_start_matches("./");
                    if !m.is_empty() {
                        candidates.push(m.to_string());
                    }
                }
            }
        }
    }
    for prefix in ["", "src/", "api/", "server/"] {
        for stem in ["index", "server", "app", "main"] {
            for ext in [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts"] {
                candidates.push(format!("{prefix}{stem}{ext}"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|rel| !rel.contains("..") && !rel.starts_with('/') && dir.join(rel).is_file())
}

/// Wires the already-sealed after()-support shim into the first function's serve environment; the
/// bytes are staged inside `IsolatedBuild` before sealing, so this function must remain
/// metadata-only.
fn wire_after_shim(m: &mut Manifest) {
    let Some(f) = m.functions.first_mut() else {
        return;
    };
    let max_ms = (f.max_duration_secs.max(1) as u64).saturating_mul(1000);
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
        if !matches!(runtime, "nodejs20.x" | "nodejs22.x" | "nodejs24.x") {
            return Err(build_output_contract_error(
                "provision Build Output API v3 functions",
                fluid_core::BuildOutputV3Refusal::unsupported(format!(
                    "function runtime {runtime:?} for {:?}",
                    function.name
                )),
            ));
        }
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
        let max_duration = function
            .config
            .get("maxDuration")
            .and_then(|v| v.as_u64())
            .unwrap_or(fluid_core::FunctionConfig::default().max_duration_secs);
        let func_dir_rel = format!(".vercel/output/functions/{}.func", function.name);
        manifest.functions.push(FunctionConfig {
            name: function.name.clone(),
            runtime: "node".to_string(),
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

/// The ONE platform-owned Node HTTP bridge for Build Output API v3 `.func` bundles, read at compile
/// time from this crate's source tree and staged verbatim into each Node function's OWN `.func`
/// directory: never the deployment root, never shared byte-for-byte with a tenant-writable path.
const BUILD_OUTPUT_NODE_LAUNCHER_SRC: &str = include_str!("build-output-node-launcher.mjs");
const BUILD_OUTPUT_NODE_LAUNCHER_FILE: &str = ".hive-build-output-launcher.mjs";

/// Stages [`BUILD_OUTPUT_NODE_LAUNCHER_SRC`] into every Node Build Output function's own `.func`
/// directory on the fully materialized host checkout, only after the repository commands have
/// finished, the artifact has been parsed and validated, and the per-function checks pass (no
/// symlinked `.func` directory, nothing at the reserved launcher path). Never called from the
/// pre-materialization fast-fail check, whose `Manifest` is discarded.
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
        if !canonical_func_dir.starts_with(&canonical_root) {
            return Err(stage_error(
                &function.name,
                "function directory escapes the checkout root (symlink?)".to_string(),
            ));
        }
        let launcher_path = canonical_func_dir.join(BUILD_OUTPUT_NODE_LAUNCHER_FILE);
        // symlink_metadata, not metadata: a symlink at the reserved path is caught even when its
        // target is missing or circular.
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

/// Walks up from `dir` looking for `node_modules/next/dist/bin/next` as a regular file: Next's real
/// CLI entry. npm's `.bin/next` shim is unusable on sealed runtime artifacts, because sealing
/// flattens symlinks into copies and Next's bin script requires `../server/require-hook`, valid
/// only at its real location. Mirrors `litebox.rs`'s `next_entry_for` walk-up. Returns a path
/// RELATIVE to `dir` (monorepos get `../../node_modules/...`), since `start_cmd[0]` is resolved
/// against the deployed workdir at launch, never against this build-time absolute path.
async fn find_next_dist_bin(dir: &Path) -> Option<String> {
    let mut up = PathBuf::new();
    let mut base = dir.to_path_buf();
    loop {
        let candidate = base.join("node_modules/next/dist/bin/next");
        if tokio::fs::metadata(&candidate)
            .await
            .is_ok_and(|m| m.is_file())
        {
            let relative = up.join("node_modules/next/dist/bin/next");
            return Some(relative.to_str()?.to_string());
        }
        if !base.pop() {
            return None;
        }
        up.push("..");
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
            if let Some(start_script) = v
                .get("scripts")
                .and_then(|s| s.get("start"))
                .and_then(|s| s.as_str())
            {
                if !bun {
                    let trimmed = start_script.trim();
                    let is_plain_next_start = trimmed == "next start"
                        || trimmed == "next start -p $PORT"
                        || trimmed == "next start -p ${PORT}";
                    if is_plain_next_start {
                        if let Some(next_bin) = find_next_dist_bin(dir).await {
                            return vec!["node".into(), next_bin, "start".into()];
                        }
                    }
                }
                // --bun: without it `bun run start` spawns real node for a
                // `"start": "node server.js"` script, defeating the Bun runtime choice.
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

/// Locates a Wasmer function's compiled `.wasm` entry module and builds its `wasmer run`
/// invocation: `server.wasm`/`app.wasm`/`main.wasm` first, else a SINGLE `*.wasm` at the project
/// root. `--net` is WASIX's opt-in socket gate and `--forward-host-env` carries the
/// cold-start-assigned `$PORT` and project env into the guest, so the port is never baked into
/// `--env PORT=<literal>`. Returns `None` (never a guess) when no entry exists or several are
/// ambiguous, leaving the generic Node/npm detection to fail loudly.
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

/// Whether `p` is a real WebAssembly module: a regular file whose first four bytes are the `\0asm`
/// magic. A `.wasm` NAME proves nothing (an unresolved Git-LFS pointer, a truncated download or a
/// saved HTML error page all carry it), and the build compiles and runs nothing, so without this
/// check a bad artifact becomes a green build and a Ready deployment that crash-loops on every
/// request instead of one loud build failure.
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

/// A STABLE Node 20–24 bin dir, preferred over an unstable system node (such as Homebrew's v26
/// canary) so framework builds do not fail engine checks: nvm-installed versions first, then
/// `node@NN` kegs.
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

/// Node binary for the build-time compile-cache warm-up. V8's bytecode cache is keyed by the exact
/// Node version and CPU arch, so the warm-up Node must match the RUNTIME Node or the runtime
/// silently ignores the cache: each Firecracker host sets `HIVE_WARMUP_NODE` to a copy of the
/// microVM's baked-in Node, and elsewhere (build host == runtime) the pinned stable Node directory
/// is used.
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

/// Bun binary for the build-time bytecode warm-up. Like V8's, Bun's bytecode is tied to a specific
/// Bun build (a mismatch is safe, just no speedup), so `HIVE_WARMUP_BUN` pins the RUNTIME Bun on
/// Firecracker hosts, mirroring `warmup_node_bin`; otherwise it is whatever `bun` the build host
/// has.
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

const MAX_ZIP_ENTRIES: usize = 16_384;
const MAX_ZIP_BYTES: u64 = 256 * 1024 * 1024;
/// Bytes read past an entry's declared size, so an entry whose header understates its length is
/// detected.
const OVERFLOW_PROBE_BYTES: u64 = 1;

/// Extracts an uploaded ZIP into `dir`, validating every entry BEFORE any byte touches the
/// filesystem: symlinks, absolute paths, `..` components and non-regular files are rejected. A
/// single top-level wrapper directory (the "zip a folder" / GitHub "Download ZIP" shape) is
/// stripped and macOS cruft (`__MACOSX`, `.DS_Store`) skipped. `MAX_ZIP_ENTRIES` and
/// `MAX_ZIP_BYTES` are hard bounds (the admin handler already caps the upload at 10 MiB, so the
/// byte ceiling is defense in depth against a path that bypasses it). Returns the resulting file
/// count.
async fn extract_zip_into(bytes: &[u8], dir: &Path) -> anyhow::Result<u64> {
    tokio::fs::create_dir_all(dir).await?;
    let reader = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| anyhow::anyhow!("invalid zip archive: {e}"))?;

    let mut entries: Vec<(String, usize)> = Vec::new();
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

        if is_macos_cruft(&name) {
            continue;
        }

        anyhow::ensure!(
            !entry.is_symlink(),
            "zip entry {i}: symlinks are not allowed ({name})"
        );

        anyhow::ensure!(
            entry.is_file(),
            "zip entry {i}: only regular files are allowed, got: {name}"
        );

        let normalized = validate_zip_entry_path(&name, i)?;
        entries.push((normalized, i));
    }

    anyhow::ensure!(
        !entries.is_empty(),
        "zip archive contains no files after stripping macOS cruft"
    );

    if let Some(prefix) = common_entry_prefix(&entries) {
        wrapper_prefix = Some(prefix);
    }

    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    for (rel_path, idx) in &entries {
        let output_path = if let Some(ref prefix) = wrapper_prefix {
            strip_prefix_component(rel_path, prefix)
                .unwrap_or(rel_path.as_str())
                .to_string()
        } else {
            rel_path.clone()
        };

        let output_path = validate_zip_entry_path(&output_path, *idx)?;

        let dest = dir.join(&output_path);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Read the entry synchronously and drop it before the first `.await`: `ZipFile` borrows the
        // non-Send `Cursor`, so holding it across an await makes the whole future non-Send.
        let (entry_bytes, entry_size) = {
            let mut entry = archive
                .by_index(*idx)
                .map_err(|e| anyhow::anyhow!("zip entry {idx}: {e}"))?;
            let size = entry.size();
            let mut buf = Vec::with_capacity(size as usize);
            let mut reader = std::io::Read::take(&mut entry, size + OVERFLOW_PROBE_BYTES);
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
    let name = name.trim_end_matches('/');
    if name == "__MACOSX" || name.starts_with("__MACOSX/") || name.starts_with("__MACOSX\\") {
        return true;
    }
    if name == ".DS_Store" || name.ends_with("/.DS_Store") || name.ends_with("\\.DS_Store") {
        return true;
    }
    false
}

/// Validates a ZIP entry path (rejects absolute paths, Windows drive-letter paths, `..` components
/// and empty names) and returns it normalized: forward slashes, no leading slash.
fn validate_zip_entry_path(name: &str, idx: usize) -> anyhow::Result<String> {
    anyhow::ensure!(!name.is_empty(), "zip entry {idx}: empty name");

    if name.as_bytes().len() >= 2
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name.as_bytes()[1] == b':'
    {
        anyhow::bail!("zip entry {idx}: absolute Windows path rejected: {name}");
    }

    let normalized = name.replace('\\', "/");

    let mut clean = Vec::new();
    for component in normalized.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            anyhow::bail!("zip entry {idx}: parent-directory component rejected: {name}");
        }
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

/// The first path component when every entry shares it (the "wrapper directory"); `None` when
/// entries already sit at the root or share no prefix.
fn common_entry_prefix(entries: &[(String, usize)]) -> Option<String> {
    if entries.len() < 2 {
        let first = entries.first()?.0.split('/').next()?;
        if first == entries.first()?.0 {
            return None;
        }
        return Some(first.to_string());
    }

    let first = entries.first()?.0.split('/').next()?;
    if first.is_empty() || first == entries.first()?.0 {
        return None;
    }

    for (path, _) in entries.iter().skip(1) {
        match path.split('/').next() {
            Some(c) if c == first => {}
            _ => return None,
        }
    }

    Some(first.to_string())
}

/// Strips the wrapper `prefix` component and its separator from a normalized forward-slash path.
fn strip_prefix_component<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some("");
    }
    Some(rest.strip_prefix('/').unwrap_or(rest))
}

/// Recursively copies the CONTENTS of `src` into `dst` (created if missing), for a zip-upload
/// REDEPLOY that rebuilds from the retained checkout of the prior build (symlinks are recreated
/// best-effort).
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

/// Build-time V8 compile-cache warm-up (Node cold-start contract): boots the server once under
/// `NODE_COMPILE_CACHE=<root>/.hive-compile-cache`, so bytecode for the hot modules is written INTO
/// the artifact and the runtime cell, pointing at the same directory, skips parse/compile even on a
/// fresh microVM. A `--require` preload flushes the cache and exits after a short boot window
/// (`flushCompileCache` needs Node >= 22.8; on older Node the cache stays empty). The cache is
/// keyed by each module's ABSOLUTE path, so where the runtime relocates the artifact (Firecracker
/// mounts the data disk at `/build`) `HIVE_WARMUP_BUILD_PATH` runs the warm-up in a private mount
/// namespace with `build_dir` bind-mounted there. Callers pass only functions that RESOLVE to the
/// Node runtime. Best-effort: any failure is a perf no-op, never breaks a deploy; the warm-up Node
/// must match the runtime's major+arch or V8 silently ignores the cache.
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
    let cache_dir = build_dir.join(".hive-compile-cache");
    if tokio::fs::create_dir_all(&cache_dir).await.is_err() {
        return;
    }
    let preload = build_dir.join(".hive-warmup-preload.cjs");
    let preload_js = "const m=require('module');const ms=parseInt(process.env.HIVE_WARMUP_MS||'5000',10);setTimeout(()=>{try{m.flushCompileCache&&m.flushCompileCache();}catch(e){}process.exit(0);},ms);";
    if tokio::fs::write(&preload, preload_js).await.is_err() {
        return;
    }
    let local_bin = build_dir.join("node_modules/.bin");
    let mut prefix = local_bin.to_string_lossy().into_owned();
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
    // exec: the child IS the server process, not a wrapping shell; PORT=0 binds an ephemeral port.
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
            // The real server is a child of the launcher and flushes AFTER it exits: poll the cache
            // until it stops growing, under a hard cap.
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
                let cache_settled = stable >= 3 && bytes > 0;
                let nothing_to_wait_for = exited && bytes == 0 && elapsed > Duration::from_secs(8);
                let hard_cap_reached = elapsed > Duration::from_secs(45);
                if cache_settled || nothing_to_wait_for || hard_cap_reached {
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

/// Pure entry detection: is `start_cmd` a bundleable `bun <file>` / `bun run <file>` invocation?
/// Returns the file argument, `None` for anything else (CLI wrappers such as
/// `bunx --bun next start`, non-Bun commands). Extracted so the decision is testable without a
/// `CloudState`.
fn bun_bundle_entry(start_cmd: &[String]) -> Option<String> {
    let is_bun = |s: &str| Path::new(s).file_name().and_then(|f| f.to_str()) == Some("bun");
    match start_cmd {
        [first, file] if is_bun(first) => Some(file.clone()),
        [first, run, file] if is_bun(first) && run == "run" => Some(file.clone()),
        _ => None,
    }
}

/// Build-time Bun bytecode cache, the Bun-native counterpart of `warmup_node_cache` but
/// structurally different: JavaScriptCore has no process-wide cache hook, so `bun build --bytecode`
/// BUNDLES the entry into a new file with a `.jsc` bytecode sidecar (and a `.map` source map) that
/// `bun run <bundled-file>` auto-loads with no runtime env var. On success `start_cmd` is REWRITTEN
/// to the bundled output, so callers MUST use the returned Vec. It applies only when `start_cmd` is
/// `bun <file>` / `bun run <file>` on an existing entry file under `build_dir`; a framework CLI
/// invocation (`bunx --bun next start`) is not a statically resolvable module graph Bun can bundle,
/// so it is returned UNCHANGED with a logged reason (the app still runs on Bun, just without the
/// speedup). Best-effort: any failure returns the ORIGINAL `start_cmd`.
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

/// Ingests a Vercel WDK manifest (`.well-known/workflow/v1/manifest.json`) emitted by a built app:
/// registers each workflow, with its React-Flow `graph`, in the engine so it shows in the Workflows
/// tab and renders on the canvas. Returns the number registered; best-effort (a non-WDK app has
/// none).
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

/// Locates a WDK `manifest.json` under a build dir: a bounded walk that skips vendored and
/// build-output trees, preferring the canonical relative path at each level.
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
    /// When set, repository commands run as PLAIN HOST PROCESSES in the checkout, as
    /// `MockBackend`/`LiteboxBackend` have always built: those backends sandbox tenant functions at
    /// RUN time and their build lane never used the isolated executor, so requiring it would break
    /// every source deploy on a host-exec backend. Firecracker keeps `host_build = false` and stays
    /// isolated-executor-only.
    host_build: bool,
    /// This build's verdict on the host's package registry, assessed lazily by
    /// the first host command (`registry_fallback`). `None` = not assessed yet.
    registry: Option<crate::build_registry::Plan>,
}

/// PATH prefix naming every existing `node_modules/.bin` from `dir` and up to five ancestors, each
/// followed by `:`, so an explicit `next build` or `vite build` finds the project's own installed
/// CLI first, as it does on Vercel and in npm scripts.
fn local_bin_dirs_on_path(dir: &Path) -> String {
    let mut path_value = String::new();
    let mut bin_dir = Some(dir.to_path_buf());
    for _ in 0..6 {
        let Some(current) = bin_dir.take() else { break };
        let candidate = current.join("node_modules/.bin");
        if candidate.is_dir() {
            path_value.push_str(&candidate.to_string_lossy());
            path_value.push(':');
        }
        bin_dir = current.parent().map(Path::to_path_buf);
    }
    path_value
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
            host_build: false,
            registry: None,
        })
    }

    /// A host-process build session for backends that build directly on the host (mock, litebox):
    /// no isolated executor is acquired and `finish` is a no-op, since the checkout IS the
    /// materialized output.
    fn begin_host(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            session: None,
            host_build: true,
            registry: None,
        }
    }

    /// The `npm_config_userconfig` this host command should run with, if any: the host registry is
    /// probed once per build (`build_registry`), and a dark public mirror with a live public
    /// registry yields a user-config naming the public registry; a repository's own `.npmrc` still
    /// wins by npm's config layering.
    async fn registry_fallback(
        &mut self,
        home: &Path,
        cloud: &Arc<CloudState>,
        bid: &str,
    ) -> Option<(&'static str, String)> {
        use crate::build_registry::{
            assess, Plan, FALLBACK_ENV_KEY, GLOBAL_NPMRC, PUBLIC_REGISTRY,
        };
        if self.registry.is_none() {
            let fallback_dir = crate::persist::data_dir();
            let plan = assess(home, Path::new(GLOBAL_NPMRC), &fallback_dir).await;
            match &plan {
                Plan::Fallback {
                    configured, reason, ..
                } => {
                    cloud.builds.log(
                        bid,
                        format!(
                            "WARN: BUILD_REGISTRY_FALLBACK: this node's package registry {configured} is \
                             unreachable ({reason}); npm/pnpm/yarn use {PUBLIC_REGISTRY} for this build \
                             unless the repository configures its own registry (a lockfile resolved \
                             against {configured} still fails). Operator: this build host is degraded — \
                             restore its resolver or mirror."
                        ),
                    );
                    tracing::warn!(build = %bid, %configured, %reason, "BUILD_REGISTRY_FALLBACK: host registry unreachable, using the public registry");
                }
                Plan::Dark { configured, reason } => {
                    cloud.builds.log(
                        bid,
                        format!(
                            "WARN: BUILD_REGISTRY_DARK: neither this node's package registry {configured} \
                             nor registry.npmjs.org is reachable ({reason}); installs can only succeed \
                             for dependencies already in the local package cache."
                        ),
                    );
                    tracing::warn!(build = %bid, %configured, %reason, "BUILD_REGISTRY_DARK: no package registry reachable from this build host");
                }
                Plan::Unchanged => {}
            }
            self.registry = Some(plan);
        }
        match &self.registry {
            Some(Plan::Fallback { userconfig, .. }) => {
                Some((FALLBACK_ENV_KEY, userconfig.to_string_lossy().into_owned()))
            }
            _ => None,
        }
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
        if self.host_build {
            return self
                .run_host(dir, command, label, args, accept_nonzero, cloud, bid, env)
                .await;
        }
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

    /// Runs one repository command as a PLAIN HOST PROCESS in the checkout (the mock/litebox build
    /// lane): `command` is a full shell string (e.g. `cd ../.. && pnpm install`) and `args` are
    /// appended as literal argv. Output streams line-buffered into the build log like the isolated
    /// path, and cancellation kills the process group.
    #[allow(clippy::too_many_arguments)]
    async fn run_host(
        &mut self,
        dir: &Path,
        command: &str,
        _label: &str,
        args: &[String],
        accept_nonzero: bool,
        cloud: &Arc<CloudState>,
        bid: &str,
        env: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<i32> {
        use tokio::io::BufReader;
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        // `sh -c script NAME args` binds the first word after the script to `$0`, so a placeholder
        // keeps the real args at `$1..` (the isolated executor supplies the same one).
        if !args.is_empty() {
            cmd.arg("hive-build");
        }
        for arg in args {
            cmd.arg(arg);
        }
        cmd.current_dir(dir);
        cmd.env_clear();
        let mut path_value = local_bin_dirs_on_path(dir);
        path_value.push_str(
            &std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string()),
        );
        cmd.env("PATH", path_value);
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        cmd.env("HOME", &home);
        cmd.env("CI", "1");
        // Set before the tenant's variables, so an explicit tenant value wins.
        if let Some((key, value)) = self.registry_fallback(Path::new(&home), cloud, bid).await {
            cmd.env(key, value);
        }
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn host build command in {}", dir.display()))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let bid_out = bid.to_string();
        let cloud_out = cloud.clone();
        let stdout_task = tokio::spawn(async move {
            if let Some(stdout) = stdout {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(line)) =
                    hive_core::logcap::read_capped_line(&mut reader, hive_core::MAX_LOG_LINE_BYTES)
                        .await
                {
                    cloud_out.builds.log(&bid_out, format!("  {}", line.text));
                }
            }
        });
        let bid_err = bid.to_string();
        let cloud_err = cloud.clone();
        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = stderr {
                let mut reader = BufReader::new(stderr);
                while let Ok(Some(line)) =
                    hive_core::logcap::read_capped_line(&mut reader, hive_core::MAX_LOG_LINE_BYTES)
                        .await
                {
                    cloud_err
                        .builds
                        .log(&bid_err, format!("  stderr: {}", line.text));
                }
            }
        });
        let status = loop {
            if cloud.build_cancels.is_cancelled(bid) {
                #[cfg(unix)]
                if let Some(pid) = pid {
                    unsafe {
                        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                }
                let _ = child.kill().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(BuildCancelled.into());
            }
            match child.try_wait()? {
                Some(status) => break status,
                None => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        };
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        let code = status.code().unwrap_or(-1);
        if code != 0 && !accept_nonzero {
            anyhow::bail!("host build command failed: `{command}` exited with status {code}");
        }
        Ok(code)
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
        if self.host_build {
            if let Some(expected) = expected {
                let path = self.root.join(expected.as_str());
                anyhow::ensure!(
                    tokio::fs::metadata(&path)
                        .await
                        .map(|m| m.is_dir())
                        .unwrap_or(false),
                    "build output directory {:?} does not exist after the host build",
                    expected.as_str()
                );
            }
            return Ok(());
        }
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

/// The one gate every repository-controlled build step funnels through when the session is
/// optional: a builder-less node (only the zero-command static lane can succeed there) refuses with
/// the canonical message instead of ever running the step on the host.
fn require_build_session<'a>(
    isolated: &'a mut Option<&mut IsolatedBuild>,
) -> anyhow::Result<&'a mut IsolatedBuild> {
    isolated.as_deref_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "BUILD_ISOLATION_UNAVAILABLE: this build step is a repository-controlled command and requires an isolated build executor, but none is available on this node. No repository-controlled command was run on the host."
        )
    })
}

/// Runs a shell command in `dir` through the build session, streaming stdout+stderr into the build
/// log; `env` is the project's environment, injected so install/build steps (Next.js
/// `NEXT_PUBLIC_*`, Vite `VITE_*`) see it.
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

/// The package manager or toolchain a repository command invokes, when its first bare word names
/// one; `None` for anything else (a framework binary such as `next build`, a path-shaped command,
/// an empty one), since only a package manager has a drop-in equivalent to substitute.
fn command_toolchain(command: &str) -> Option<&'static str> {
    let first = command.split_whitespace().next()?;
    let bit = crate::resources::build_toolchain_bit(first)?;
    crate::resources::BUILD_TOOLCHAINS
        .iter()
        .find(|(_, candidate)| *candidate == bit)
        .map(|(tool, _)| *tool)
}

/// The package manager this build needs, from the commands the repository (or Project Settings)
/// supplies: the install command is consulted first because it runs first (it is the one that died
/// `bun: command not found` on a node with no bun). Deterministic: same inputs, same answer, on
/// every node.
fn build_toolchain_need(install_command: &str, build_command: &str) -> Option<&'static str> {
    command_toolchain(install_command).or_else(|| command_toolchain(build_command))
}

/// Probes the package managers and toolchains the BUILD LANE can actually exec, through the very
/// session the repository commands will use (a host process on the mock/litebox lane, a shell in
/// the pinned builder image on the microVM lane), so the answer is the filesystem the command
/// resolves against; the exit code is the toolchain bitmask
/// (`resources::build_toolchain_probe_script`). `None` means the probe could not run and reads as
/// UNKNOWN: nothing is substituted, so the probe can never turn a working build into a different
/// one.
async fn probe_build_toolchains(
    isolated: &mut Option<&mut IsolatedBuild>,
    dir: &Path,
    cloud: &Arc<CloudState>,
    bid: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> Option<Vec<String>> {
    let session = require_build_session(isolated).ok()?;
    let script = crate::resources::build_toolchain_probe_script();
    let code = session
        .run(
            dir,
            &script,
            "probe build toolchain",
            &[],
            true,
            cloud,
            bid,
            env,
        )
        .await
        .ok()?;
    if code < 0 {
        return None;
    }
    Some(crate::resources::build_toolchain_names(code as u32))
}

/// Classifies a repository command that died with the shell's 127 (a MISSING EXECUTABLE) or 42 (a
/// toolchain mismatch). Platform-generated commands probe their own toolchain and print
/// `BUILD_TOOLCHAIN_MISSING`/`BUILD_TOOLCHAIN_MISMATCH` themselves (see
/// `PackageManagerLauncher::invoke`), but an explicit override is repository authority and runs
/// byte-for-byte with no probe around it, so this classifier is the honest-failure half for that
/// lane. A bare-word command (`bun install`) names the tool it invoked; a compound or path-shaped
/// one stays generic. Either way the failure is reported as the BUILD ENVIRONMENT's missing tool
/// with the operator remedy, never as the tenant's application failing.
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
            if stdout.is_empty() {
                stderr
            } else {
                stdout
            }
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

/// Cache identity binds every authority and executable-input dimension: the key commits to the
/// lane, tenant, canonical and actual repositories, lock inputs, selected toolchain and build
/// policy, so a production key cannot name a preview/fork artifact. `hive-build-cache-v4` is BLAKE3
/// ([`fluid_core::ContentHasher`]); v3 was SHA-256. A key is a LOOKUP into a cache, so a switch
/// only costs one miss per entry, never a corruption: a digest that NAMES stored bytes keeps its
/// writer on the algorithm that produced it.
async fn compute_cache_key(
    install_dir: &Path,
    pm: &str,
    tenant: &str,
    trust: &BuildTrustContext,
    toolchain: &str,
    policy: &str,
    build_env: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let mut hasher = fluid_core::ContentHasher::new(b"hive-build-cache-v4");
    hasher.field("tenant", tenant.as_bytes());
    hasher.field("canonical-repo", trust.canonical_repo.as_bytes());
    hasher.field("actual-repo", trust.actual_repo.as_bytes());
    hasher.field("trust-lane", trust.lane.cache_label().as_bytes());
    hasher.field("package-manager", pm.as_bytes());
    hasher.field("toolchain", toolchain.as_bytes());
    hasher.field("policy", policy.as_bytes());
    for (key, value) in build_env {
        hasher.field("build-env-key", key.as_bytes());
        let value_digest =
            fluid_core::content_hash(b"hive-build-cache-v4-env-value", value.as_bytes());
        hasher.field("build-env-value-blake3", value_digest.as_bytes());
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
            hasher.field(name, &bytes);
            found = true;
        }
    }
    if !found {
        return None;
    }
    Some(hasher.finish())
}

pub const ARTIFACT_SHA_HEADER: &str = "x-hive-artifact-sha256";
pub const ARTIFACT_SIG_HEADER: &str = "x-hive-artifact-sig";

/// Fleet-shared secret for artifact signatures: `HIVE_ARTIFACT_SECRET`, else a key DERIVED from
/// `HIVE_JWT_SECRET` with a purpose-specific label (one key, one purpose: the raw JWT secret is
/// never reused for a second HMAC role; set `HIVE_ARTIFACT_SECRET` in production for full
/// independence). `None` disables signing (dev / single-node).
pub fn artifact_secret() -> Option<String> {
    if let Some(s) = std::env::var("HIVE_ARTIFACT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return Some(s);
    }
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
    mac.verify_slice(&expected).is_ok()
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

/// Verifies a pulled artifact against the requested cache identity as well as its content: a valid
/// signature for key A cannot be replayed as key B. The content digest, when present, MUST match
/// (corruption guard) and accepts either the legacy SHA-256 or the BLAKE3 mint, because a
/// mixed-version roll has peers on both. Whenever THIS node has an artifact secret a valid
/// signature is REQUIRED: a peer that cannot sign is not trusted to supply build inputs.
fn verify_pulled_artifact(
    key: &str,
    bytes: &[u8],
    sha: Option<&str>,
    sig: Option<&str>,
) -> Result<(), String> {
    if let Some(sha) = sha {
        if !fluid_core::verify_raw_content(bytes, sha) {
            return Err("content digest mismatch (corrupted artifact)".into());
        }
    }
    if let Some(secret) = artifact_secret() {
        match sig {
            Some(sig) if cache_artifact_sig_valid(&secret, key, bytes, sig) => {}
            Some(_) => return Err("artifact signature invalid (untrusted/forged)".into()),
            None => return Err("artifact signature missing (peer can't authenticate)".into()),
        }
    }
    Ok(())
}

/// Fetches a cache blob from the mesh peers into `dest`; true on success, best-effort (every error
/// is swallowed). The peers are RACED, not walked: several fleet nodes' 8786/8787 are firewalled so
/// a request SYN-drops and burns its whole timeout, which serially was up to 20 s per unreachable
/// peer before the install could start. Each peer gets `HIVE_BUILDCACHE_PEER_TIMEOUT_SECS` (6) and
/// the whole phase `HIVE_BUILDCACHE_FETCH_BUDGET_SECS` (20), because a pull slower than a clean
/// install is not worth waiting for. The request carries the mesh-internal `service` delegation
/// that `/v1/buildcache/:key` requires (`admin::require_fleet_read`), and a blob that fails the
/// integrity or authenticity check is rejected without ever being written to the cache.
async fn try_peer_fetch(cloud: &Arc<CloudState>, key: &str, dest: &Path) -> bool {
    let peers = cloud.peers.read().clone();
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
        let mut req = cloud.http.get(&url);
        if let Ok(tok) = crate::auth::issue(
            "mesh-internal",
            "mesh",
            "service",
            false,
            crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
        ) {
            req = req.header("authorization", format!("Bearer {tok}"));
        }
        async move { (peer, req.timeout(per_peer).send().await) }
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
                        continue;
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
            let _ = tokio::fs::remove_file(&tar).await;
            cloud.builds.log(
                bid,
                "Build cache was unreadable — discarded; installing fresh.",
            );
            false
        }
    }
}

/// Saves node_modules (plus the framework cache when present) to the content-addressed cache,
/// best-effort and atomic (temp + rename). The temp name carries the build id: the key is a CONTENT
/// hash, so concurrent builds with the same lockfile share it, and an un-suffixed `<key>.tar.tmp`
/// let two `tar -cf` writers interleave one file and install a torn archive.
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

/// Builds a `docker-compose`/`compose.yaml` repo into a container manifest on the host, like every
/// other container deployment (builder protocol v1 accepts only `BuildSurface::RepositoryCommands`,
/// and `podman build`/`podman compose` need a container runtime, not a syscall sandbox). Only the
/// PRIMARY service becomes the deployment's routable function (the unit of deployment is one
/// project namespace with one public entrypoint): it is built from source when it declares
/// `build:`, pulled when it declares `image:`, and `build` wins when both are present, as in
/// compose.
async fn compose_container_manifest(
    cloud: &Arc<CloudState>,
    bid: &str,
    isolated: Option<&mut IsolatedBuild>,
    dir: &Path,
    compose_path: &Path,
    project: &str,
    incarnation: ProjectIncarnation,
    image_port: Option<u16>,
    image_protocol: Option<ServiceProtocol>,
    image_memory: Option<&str>,
    image_cpus: Option<&str>,
    image_pids: u32,
    image_ports: Option<Vec<PortSpec>>,
) -> anyhow::Result<Manifest> {
    if let Some(session) = isolated {
        session.finish().await?;
    }
    let mem_mib = image_memory.map(parse_mem_mib).unwrap_or(0);
    let cpus = image_cpus.map(parse_cpus_quota).unwrap_or(0.0);
    let log = |s: String| cloud.builds.log(bid, s);
    let text = tokio::fs::read_to_string(compose_path)
        .await
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", compose_path.display()))?;
    let services = crate::compose::parse_compose(&text)?;
    anyhow::ensure!(
        !services.is_empty(),
        "{} declares no services — nothing to deploy",
        compose_path.display()
    );
    let primary = crate::compose::primary_service(&services)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{} declares no primary service", compose_path.display()))?;
    log(format!(
        "Detected compose file {} — {} service(s), primary `{}`.",
        compose_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "compose".to_string()),
        services.len(),
        primary.name
    ));
    let port = image_port.or(Some(primary.port));
    let protocol = image_protocol.or(Some(primary.protocol));

    let (image, skip_pull) = match &primary.build {
        Some(build) => {
            let context = dir.join(&build.context);
            let dockerfile = context.join(
                build
                    .dockerfile
                    .clone()
                    .unwrap_or_else(|| "Dockerfile".to_string()),
            );
            anyhow::ensure!(
                dockerfile.is_file(),
                "compose service `{}` builds from {} but that file does not exist",
                primary.name,
                dockerfile.display()
            );
            let tag = format!("hive-{}-{project}:latest", sanitize_tag(project));
            let podman_path = podman_path_env();
            log(format!(
                "Building image for compose service `{}` from {}{} …",
                primary.name,
                dockerfile.display(),
                build
                    .target
                    .as_deref()
                    .map(|t| format!(" (stage `{t}`)"))
                    .unwrap_or_default()
            ));
            let t0 = now_ms();
            let mut args = vec![
                "build".to_string(),
                // --layers enables BuildKit-style cache mounts (`RUN --mount=type=cache`); without
                // it such steps fail outright.
                "--layers".to_string(),
                "-t".to_string(),
                tag.clone(),
                "-f".to_string(),
                dockerfile.to_string_lossy().to_string(),
            ];
            if let Some(target) = build.target.as_deref() {
                args.push("--target".to_string());
                args.push(target.to_string());
            }
            args.push(context.to_string_lossy().to_string());
            let out = Command::new("podman")
                .args(&args)
                .env("PATH", &podman_path)
                .output()
                .await?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                let msg = err
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("build failed");
                anyhow::bail!("podman build failed: {}", msg.trim());
            }
            log(format!(
                "Built image {tag} in {}ms",
                now_ms().saturating_sub(t0)
            ));
            (tag, true)
        }
        None => match &primary.image {
            Some(image) => (image.clone(), false),
            None => anyhow::bail!(
                "compose service `{}` declares neither `build:` nor `image:` — there is \
                 nothing to run",
                primary.name
            ),
        },
    };

    image_container_manifest(
        cloud,
        bid,
        project,
        incarnation,
        &image,
        port,
        protocol,
        mem_mib,
        cpus,
        image_pids,
        image_ports,
        skip_pull,
    )
    .await
}

/// Builds a `Dockerfile`/`Containerfile` repo into a container manifest on the host, not in the
/// isolated builder: builder protocol v1 accepts only `BuildSurface::RepositoryCommands`, and a
/// `podman build` needs a container runtime, not a syscall sandbox. Container deployments already
/// run podman on the host on every backend, so no repository command executes and nothing is
/// materialized by the sealed builder.
async fn dockerfile_container_manifest(
    cloud: &Arc<CloudState>,
    bid: &str,
    isolated: Option<&mut IsolatedBuild>,
    dir: &Path,
    dockerfile: &Path,
    project: &str,
    incarnation: ProjectIncarnation,
    image_port: Option<u16>,
    image_protocol: Option<ServiceProtocol>,
    image_memory: Option<&str>,
    image_cpus: Option<&str>,
    image_pids: u32,
    image_ports: Option<Vec<PortSpec>>,
) -> anyhow::Result<Manifest> {
    if let Some(session) = isolated {
        session.finish().await?;
    }
    let mem_mib = image_memory.map(parse_mem_mib).unwrap_or(0);
    let cpus = image_cpus.map(parse_cpus_quota).unwrap_or(0.0);
    let log = |s: String| cloud.builds.log(bid, s);
    let exposed = parse_expose(dockerfile).await;
    let port = image_port.or(exposed);
    if let Some(p) = exposed {
        log(format!("Dockerfile exposes port {p}."));
    }
    let tag = format!("hive-{}-{project}:latest", sanitize_tag(project));
    let path = podman_path_env();
    log(format!(
        "Building container image from {} …",
        dockerfile
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Dockerfile".to_string())
    ));
    let t0 = now_ms();
    let out = Command::new("podman")
        .args([
            "build",
            // --layers enables BuildKit-style cache mounts (`RUN --mount=type=cache`); without it
            // such steps fail outright.
            "--layers",
            "-t",
            &tag,
            "-f",
            &dockerfile.to_string_lossy(),
            &dir.to_string_lossy(),
        ])
        .env("PATH", &path)
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("build failed");
        anyhow::bail!("podman build failed: {}", msg.trim());
    }
    log(format!(
        "Built image {tag} in {}ms",
        now_ms().saturating_sub(t0)
    ));
    image_container_manifest(
        cloud,
        bid,
        project,
        incarnation,
        &tag,
        port,
        image_protocol,
        mem_mib,
        cpus,
        image_pids,
        image_ports,
        true,
    )
    .await
}

/// Locates a repo's container build file: `Dockerfile`, or its identical twin `Containerfile` (the
/// vendor-neutral OCI/Buildah/Podman name, byte-for-byte the same format), preferring `Dockerfile`
/// when both exist.
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

/// Merges a parsed `vercel.json` into the deployment manifest: routing (redirects and rewrites
/// prepended so vercel.json wins, headers), cleanUrls, trailingSlash, images, crons and
/// per-function overrides (matched by glob; more than 2 GB of memory selects Vercel's Performance
/// tier, 2 vCPUs, since Vercel scales CPU with memory).
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

    for (glob, fnc) in &vc.functions {
        for f in m.functions.iter_mut() {
            if glob_match(glob, &f.name) {
                if let Some(d) = fnc.max_duration {
                    f.max_duration_secs = d;
                }
                if let Some(mem) = fnc.memory {
                    f.memory_mib = mem;
                    let performance_tier = mem > 2048;
                    f.vcpus = if performance_tier { 2 } else { 1 };
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

/// Converts a Vercel 5-field cron (`min hour dom mon dow`) to the scheduler's 6-field form (`sec
/// min hour dom mon dow`) by prepending a 0-second field; 6-field expressions pass through
/// unchanged.
fn to_six_field_cron(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 {
        format!("0 {}", expr.trim())
    } else {
        expr.trim().to_string()
    }
}

/// Glob match for `vercel.json` `functions` keys against a function name: `*` stays within a path
/// segment, `**` crosses segments, and because function names are extension-less (`api/hello`) a
/// trailing file extension on the pattern (`api/*.js`) is also tried stripped.
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

/// The mount point for the automatic per-container persistent volume: a project's dashboard-managed
/// `ContainerSettings::volume_mount_path` wins, then the node-wide `HIVE_CONTAINER_VOLUME_PATH`,
/// then `/data`. The per-project override exists because not every image follows the `/data`
/// convention (`itzg/minecraft-server` does), and a node-wide env var would misconfigure every
/// OTHER project on the node.
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

/// The run-config JSON (container `start_cmd[3]`) that attaches an automatic persistent volume: a
/// host-backed named volume (≥1 GB) keyed STABLY per project (plus an optional service suffix for
/// compose), so a container's data survives instance restarts and redeploys. It also puts the
/// container on its own per-project podman network (the compose subnet scheme), because on podman's
/// shared default bridge ANY tenant's container could reach any other's by bridge IP; no static ip
/// is pinned (unlike compose), so scale-out instances coexist and the backend assigns dynamic
/// addresses within the project subnet. Merged into the compose network cfg when present.
fn container_volume_cfg(
    project: &str,
    incarnation: ProjectIncarnation,
    service: Option<&str>,
    volume_path: Option<&str>,
) -> String {
    let name = project_volume_name(project, incarnation, service);
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

/// Deterministic per-project podman network (name, /24 subnet, gateway) in the 10.128-191/16 space,
/// the same scheme as the compose path so a project keeps one network however it is deployed;
/// shared with the databases module so a managed DB container joins the SAME network as the app
/// containers (reachable by its network alias).
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

/// Manifest for a container deployment: the function "process" is `podman run`. `start_cmd` is the
/// structured `__container__` marker the backend recognizes,
/// `[marker, image, internal port, run-config JSON]` (the JSON carries the automatic persistent
/// volume and the per-project network); the app is told to listen on `internal` via PORT and the
/// cell's $PORT is published to that internal port, so the gateway proxies to 127.0.0.1:$PORT.
/// `protocol` is a free-form string (a Railway-style `fluid.json` override, or an already-resolved
/// wire string from an image deploy) parsed into the typed enum, falling back to http for anything
/// unrecognized rather than rejecting the build: `FunctionConfig`'s own JSON deserialize boundary
/// is where a malformed value is rejected hard. A `memory_mib`/`cpus`/`pids` of 0 means the node's
/// generous, env-tunable container defaults (a real value comes from `fluid.json`
/// `container.memory`/`cpus`/`pids` or an `ImageDeployReq`'s equivalents; the old hardcoded 512m
/// OOM-killed monolith containers). A raw-protocol (tcp/udp/grpc) deployment gets no `/` route: it
/// is reached through its allocated raw public port (`raw_ports`/`raw_proxy`), and an HTTP route
/// would advertise an endpoint that was never there.
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
    let proto: ServiceProtocol = protocol.parse().unwrap_or_default();
    Manifest {
        project: project.to_string(),
        static_dir: None,
        functions: vec![FunctionConfig {
            name: "web".into(),
            runtime: "container".into(),
            start_cmd: vec![
                "__container__".into(),
                image.to_string(),
                internal.to_string(),
                container_volume_cfg(project, incarnation, None, volume_path),
            ],
            env: Default::default(),
            vcpus: 1,
            memory_mib,
            cpus,
            pids,
            max_concurrency: 20,
            min_instances: 1,
            max_instances: 5,
            idle_ttl_secs: 120,
            max_duration_secs: 300,
            protocol: proto,
            ports: PortSpec::from_legacy_port(Some(internal), proto),
            ..Default::default()
        }],
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

/// Fully qualifies a container-image reference for podman: Linux podman ENFORCES short-name
/// resolution (`user/img:tag` fails with "short-name resolution enforced" unless registries.conf
/// lists search registries; macOS podman-machine resolves docker.io implicitly). A ref whose first
/// path component is not a registry host (no `.`/`:`, not `localhost`) gets the Docker Hub prefix,
/// and single-component names also get the `library/` namespace.
pub(crate) fn qualify_image_ref(image: &str) -> String {
    let image = image.trim();
    match image.split_once('/') {
        None => format!("docker.io/library/{image}"),
        Some((first, _)) => {
            let is_registry_host =
                first.contains('.') || first.contains(':') || first == "localhost";
            if is_registry_host {
                image.to_string()
            } else {
                format!("docker.io/{image}")
            }
        }
    }
}

/// Deploys a PRE-BUILT OCI image from any registry (Docker Hub / Quay / arbitrary): `podman pull`
/// it on this (target) node, auto-detect its listening port from the image's `ExposedPorts` (unless
/// overridden) and synthesize a container manifest with the automatic persistent volume; project
/// env is injected by the caller. `skip_pull` is true when THIS build just built the image from a
/// Dockerfile: a locally built tag is not in any registry, so it is neither pulled nor qualified
/// (it already resolves in podman's store as `localhost/<tag>`, and rewriting it to
/// `docker.io/library/<tag>` would point at an image that does not exist, so the container would
/// never start). Port and protocol: explicit values win outright, an explicit port without a
/// protocol assumes http (the detected protocol could belong to a DIFFERENT exposed port),
/// otherwise both are detected from the image, falling back to 8080/http, so a UDP-only image such
/// as Minecraft Bedrock keeps the protocol it can actually speak. An image deploy has no
/// `fluid.json`, so `ProjectSettings::container` is the only way to move the volume's mount path. A
/// declared multi-port list REPLACES the single-port list (the first entry is the primary, kept in
/// sync with `protocol`); only raw-protocol specs get a public allocation
/// (`raw_ports::allocate_raw_ports_coordinated`), an http/https secondary entry just documents an
/// extra exposed port, as for a compose service's un-exposed ports. Podman-only, even on macOS
/// (unlike the single-Dockerfile build path, which uses Apple's `container` there): Apple's
/// `container image inspect` does not expose the OCI `Config.ExposedPorts` field at all, so port
/// auto-detection, a shipped feature for registry-image deploys, would silently stop working (see
/// `hive_backend::container_cli`'s module doc for the general policy).
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
    skip_pull: bool,
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);
    let path = podman_path_env();
    let qualified = if skip_pull {
        image.to_string()
    } else {
        qualify_image_ref(image)
    };
    let image = qualified.as_str();
    if skip_pull {
        log(format!(
            "Using locally built image {image} (no registry pull)."
        ));
    } else {
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
    }

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
            if let Some(primary) = ports.first() {
                f.protocol = primary.protocol;
            }
            f.ports = ports;
        }
    }
    Ok(manifest)
}

/// Auto-detects a container's listening port and protocol from the image's `Config.ExposedPorts`
/// (`podman image inspect`); see [`parse_exposed_ports`] for the selection order (TCP first,
/// falling back to UDP-only images).
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

/// Parses `podman image inspect --format '{{json .Config.ExposedPorts}}'` output
/// (`{"8080/tcp":{},"9090/tcp":{}}`) to the lowest exposed TCP port (protocol `Http`). An image
/// that exposes NO tcp port falls back to the lowest UDP port (protocol `Udp`), so a UDP-only
/// service such as Minecraft Bedrock (`19132/udp`) can be auto-detected at all. `null`, empty or
/// unparseable input gives `None`.
fn parse_exposed_ports(json: &str) -> Option<PortSpec> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let obj = v.as_object()?;
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

/// Railway-style per-service overrides for a CONTAINER project from an optional `fluid.json`
/// `container` block (`{ "container": { "port": 50051, "protocol": "grpc" } }`): lets a
/// Dockerfile/Containerfile project pin its listen port and declare its wire protocol without
/// changing the image. All fields optional.
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
    /// CPU quota for the container (podman `--cpus`), e.g. "4", "2.0", "0.5"; overrides the node
    /// default and is clamped to `HIVE_CONTAINER_CPUS_MAX` (`ContainerLimits::for_container`).
    #[serde(default)]
    cpus: Option<String>,
    /// Max-PIDs ceiling for the container's cgroup (podman `--pids-limit`), a fork-bomb guard;
    /// overrides the node default and is clamped to `HIVE_CONTAINER_PIDS_MAX`.
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
        (t.as_str(), 1.0)
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * mult).round() as u32)
        .unwrap_or(0)
}

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

/// Builds a manifest for a Next.js deployment adapter (OpenNext / vinext): the framework's Node
/// HTTP server runs as the Fluid `api` function while immutable assets are served from the CDN,
/// falling through to the server on a miss. Both adapters emit one bundled server file, the shape
/// `bun run <file>` needs and `warmup_bun_bytecode` can bytecode-cache, so an explicit Bun runtime
/// runs that same file through Bun instead of Node. `None` for non-adapter frameworks (the caller
/// uses the generic node-server path).
async fn adapter_manifest(
    project: &str,
    slug: &str,
    dir: &Path,
    runtime: Option<hive_core::Runtime>,
) -> Option<Manifest> {
    let bun = runtime == Some(hive_core::Runtime::Bun);
    let (start_cmd, assets_dir): (Vec<String>, &str) = match slug {
        "opennext" => {
            let server = ".open-next/server-functions/default/index.mjs";
            if !dir.join(server).exists() {
                return None;
            }
            let runner = if bun { "bun" } else { "node" };
            (vec![runner.into(), server.into()], ".open-next/assets")
        }
        "vinext" => {
            // Prefer Nitro's node server directly, else `vinext start` (both honor $PORT);
            // `--no-install` avoids a runtime network fetch and `--bun` keeps the CLI's
            // shebang-invoked children on Bun.
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

/// A self-refreshing "Building…" placeholder served at the project's domain the moment a FIRST
/// deploy starts, so the URL resolves instead of 404ing for the whole build; it reloads every few
/// seconds and flips to the real app once the deployment is ready.
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

/// First-deploy only: registers the project's host immediately with a "Building…" page so the
/// domain resolves during the build, and returns the placeholder deployment id (removed once the
/// real deployment is live). A redeploy returns `None`: the current version stays live until the
/// new build is ready, and `project_has_deployment` is fleet-wide because the old local-only
/// `serves_host` check missed remotely placed projects and created a bogus "Building…" Preview row
/// on every redeploy. `bid` uniquifies the scratch dir: two concurrent first deploys of the same
/// project (both pass `project_has_deployment` before either registers) must not point two
/// deployment records at ONE shared placeholder root.
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
    let is_production = false;
    let info = cloud.gw.deploy_full_with_runtime_exact(
        dir.to_string_lossy().into_owned(),
        None,
        static_manifest(project, "."),
        req.creator.clone().unwrap_or_else(|| "you".into()),
        None,
        is_production,
        DeployState::Building,
        settings.team,
        incarnation,
    );
    crate::admin::causal_stamp_new_deployment(cloud, project, &info.id.0);
    crate::persist::persist(cloud);
    Some(info.id.to_string())
}

/// The infix `register_building_placeholder` stamps into a placeholder's root dir
/// (`<tag>-building-<ms>-<bid>`): the only durable marker that tells a placeholder from a real
/// deployment after a restart, so the boot reconciler and the reaper below both key off it.
pub(crate) const PLACEHOLDER_ROOT_INFIX: &str = "-building-";

/// Whether this deployment root is a `Building…` placeholder's scratch dir: returns the id of the
/// build that minted it, parsed from the dir name, or `None` when the root is not a placeholder.
pub(crate) fn placeholder_build_id(root: &str) -> Option<&str> {
    let name = std::path::Path::new(root).file_name()?.to_str()?;
    let (_, tail) = name.rsplit_once(PLACEHOLDER_ROOT_INFIX)?;
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

/// Reaps `Building…` placeholders whose build is over. The placeholder is removed exactly once, on
/// the happy path of the task `start_build` spawned, so ANY way that task fails to reach its last
/// line leaks it permanently: a user cancel (an aborted future never runs the removal), a panic, or
/// the process dying mid-build. A leaked placeholder is not inert: `deploy_full` hands a project's
/// FIRST deployment the production alias, so the shell owns `<project>` on that node forever,
/// `persist::restore` reconciles it from `Building` to a permanent `Error`, and the node both
/// serves it and publishes a DNS affinity record for it. The reap condition is "the build that
/// minted it is no longer in flight", decidable from local state alone; an unknown build id counts
/// as over, since build records do not outlive a restart and a placeholder for a build nobody
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

/// Settle delay before the first git poll, so the initial gossip and deployment-record sync gives
/// every project a real deployed-commit baseline (a cold leader would otherwise treat every project
/// as unknown at once).
const GIT_POLL_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_secs(45);
/// Period of the git commit poll.
const GIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(90);

/// Spawns the leader-only git commit-poll reconciler. The GitHub webhook is the only event-driven
/// auto-deploy trigger and only fires if a hook was installed: a project imported as a plain public
/// URL, or whose owner never completed the GitHub OAuth/App connection, gets neither a webhook nor
/// the Actions-workflow fallback (`git_ci == None`), so no `git push` ever deploys, with no visible
/// error. This reconciler closes that gap with no credential or owner action (a public repo needs
/// nothing): it polls each git-sourced project's tracked-branch HEAD with `git ls-remote` and
/// starts the SAME build the webhook would whenever HEAD has advanced past the deployed commit.
/// Cheap when nothing changed: one `ls-remote` per git project per tick, short-circuited by an
/// in-memory SHA cache.
pub fn spawn_git_poll_reconcile(cloud: Arc<CloudState>) {
    crate::supervise::spawn_supervised("git-poll-reconcile", move || {
        let cloud = cloud.clone();
        async move {
            tokio::time::sleep(GIT_POLL_SETTLE_DELAY).await;
            let mut tick = tokio::time::interval(GIT_POLL_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                crate::supervise::beat("git-poll-reconcile");
                if !crate::leadership::may_act(&cloud, crate::leadership::Job::GitPoll) {
                    continue;
                }
                git_poll_cycle(&cloud).await;
            }
        }
    });
}

/// One project's outcome from a single `git_poll_cycle` pass, folded into the cycle's counters
/// after every check has run so the concurrent fan-out needs no shared mutable counter.
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

/// Concurrent `git_poll_one` fan-out width per cycle (`HIVE_GIT_POLL_CONCURRENCY`, default 16). The
/// input is `cloud.projects`: platform-wide and, on the enterprise tier, unbounded per tenant
/// (`plan_max_projects`), so an unbounded `join_all` would fork one `git ls-remote` per project at
/// once on the single control-plane leader every tick (FD, process-table and socket pressure that
/// scales with tenant-controlled project count). `buffer_unordered` keeps one slow remote from
/// blocking another while capping the number of `git` subprocesses.
fn git_poll_concurrency() -> usize {
    std::env::var("HIVE_GIT_POLL_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(16)
}

/// One poll pass over every git-sourced project. Never panics: a per-project failure (unreachable
/// remote, missing branch, auth failure) is skipped so it cannot wedge the other projects or the
/// loop. Each check does its own network round trip (`git ls-remote`, up to 20 s), so the bounded
/// fan-out ([`git_poll_concurrency`]) keeps one slow project from blocking another while capping
/// simultaneous `git` subprocesses.
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

/// One project's check-and-maybe-deploy, factored out of `git_poll_cycle` so projects run
/// concurrently; every side effect (`git_poll_seen` dedup, the already-building guard,
/// `start_build`) is scoped to the single project. Only git sources are polled (not zip `upload://`
/// or image `image://`, nor never-deployed or unbound projects), on the production branch, else the
/// deployment's own branch. The baseline is the in-memory last-seen SHA, else the deployed commit,
/// so the FIRST poll after a boot deploys pushes that were never deployed (deployed != HEAD) and
/// leaves current projects alone. An unreadable remote HEAD (branch missing, remote unreachable,
/// auth failed) WARNs and skips the cycle, never reading as "no commit, deploy"; `had_token=false`
/// on a private repo means this node has no credential (GitHub App private key or `GITHUB_TOKEN`).
/// `git_poll_seen` is written BEFORE the build is enqueued, so a slow build is not re-enqueued next
/// tick and a webhook plus this poller cannot double-fire (the in-flight check is local and
/// best-effort; the SHA dedup is the real guard). The request pins the EXACT polled SHA, as the
/// webhook path does. If `start_build` rejects the deploy the baseline is restored, but only when
/// nothing newer raced in: otherwise the next cycle would read the undeployed HEAD as current and
/// silently drop the push.
async fn git_poll_one(cloud: &Arc<CloudState>, project: String) -> GitPollOutcome {
    let Some(src) = crate::admin::git_for_project_fleet(cloud, &project) else {
        return GitPollOutcome::NotGit;
    };
    if !src.is_real_git() {
        return GitPollOutcome::NotGit;
    }
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

    let token = resolve_git_poll_token(&src.repo_url).await;

    let head = match git_ls_remote_head(&src.repo_url, &branch, token.as_deref()).await {
        Some(h) if !h.is_empty() => h,
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

    let baseline = cloud
        .git_poll_seen
        .read()
        .get(&project)
        .cloned()
        .unwrap_or_else(|| src.commit.clone());

    if commit_eq(&head, &baseline) {
        cloud.git_poll_seen.write().insert(project.clone(), head);
        return GitPollOutcome::Polled;
    }

    let already_building = cloud.builds.list().iter().any(|b| {
        b.project == project
            && matches!(b.state, DeployState::Queued | DeployState::Building)
            && commit_eq(&b.commit, &head)
    });
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
    };
    let build_id = match start_build(cloud.clone(), req, None, None).await {
        Ok(id) => id,
        Err(e) => {
            {
                let mut seen = cloud.git_poll_seen.write();
                if seen.get(&project) == Some(&head) {
                    seen.insert(project.clone(), baseline.clone());
                }
            }
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

/// Whether two commit strings name the same commit, tolerating one being an abbreviated prefix (at
/// least 7 chars) of the other (a deployment record may store a short SHA while `ls-remote` returns
/// the full one); empty never matches. `pub(crate)` because `admin::git_webhook` reuses it for its
/// own already-building dedup.
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

/// `git ls-remote <url> refs/heads/<branch>` to the branch HEAD SHA: host-agnostic
/// (github/gitlab/bitbucket/self-hosted) and free of GitHub's REST rate limit. A hard 20 s timeout
/// keeps a hung remote from wedging the poll cycle, it never prompts for credentials (a private
/// repo without a token just fails and is skipped), and it returns `None` on any failure or an
/// absent branch. The token reaches git through the same FD-backed credential helper as the clone
/// path, never embedded in the URL (argv-visible), and is only offered for github.com.
async fn git_ls_remote_head(repo_url: &str, branch: &str, token: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("git");
    let _cred = apply_credential(&mut cmd, token).ok()?;
    cmd.arg("ls-remote")
        .arg(repo_url)
        .arg(format!("refs/heads/{branch}"))
        .env("GIT_TERMINAL_PROMPT", "0")
        // Own the process group and kill on drop: the timeout below drops the future on expiry,
        // which would otherwise orphan a hung `git` per timed-out poll.
        .process_group(0)
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .filter(|s| s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_string())
}

/// Token for polling/cloning a PRIVATE github repo: a GitHub App installation token (minted
/// server-side, no user session) first, else a node-wide `GITHUB_TOKEN`; `None` for a public repo
/// or a non-github host. Mirrors `git_webhook`'s resolution so both auto-deploy paths authenticate
/// identically. A working App key with the App not INSTALLED on the repo's org is named (once per
/// repo) rather than swallowed, because it otherwise reads like "no credential configured"
/// (`had_token=false`) and mis-steers the diagnosis toward installing keys that already exist.
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

        // Package-manager detection at the ROOT (where a monorepo installs)
        // must pick bun via the root bun.lock.
        let pm = fluid_build::detect_package_manager(&repo_root);
        let workspace = crate::workspace::load_for_manager(&repo_root, &pm)
            .await
            .expect("workspace manifest must parse");
        let root_is_workspace = workspace.is_some();
        assert!(
            root_is_workspace,
            "root package.json has a \"workspaces\" field"
        );
        let workspace_member =
            crate::app_discovery::is_member(&repo_root, &api_dir, workspace.as_ref())
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
            repository_build: None,
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
