//! Deploy from a git repository with a live, Vercel-style **build log**.
//!
//! `start_build` creates a build record (state = building) and returns its id
//! immediately; a background task clones the repo, emits timestamped log lines
//! (region, machine config, cloning, install/build commands, ready), then
//! registers the routable deployment via the gateway. The dashboard polls
//! `GET /v1/builds/:id` to stream the logs as they appear.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fluid_core::{
    DeployState, FunctionConfig, GitDeployRequest, GitSource, Manifest, Route, RouteTarget,
};
use hive_core::now_ms;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use tokio::process::Command;
use uuid::Uuid;

use crate::state::CloudState;

#[derive(Clone, Serialize)]
pub struct LogLine {
    pub ts_ms: u64,
    pub line: String,
}

#[derive(Clone, Serialize)]
pub struct Build {
    pub id: String,
    pub project: String,
    pub repo_url: String,
    pub branch: String,
    pub commit: String,
    pub commit_message: String,
    pub state: DeployState,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub deployment_id: Option<String>,
    pub alias: Option<String>,
    pub lines: Vec<LogLine>,
}

#[derive(Default)]
pub struct BuildStore {
    map: Mutex<HashMap<String, Build>>,
}

impl BuildStore {
    pub fn new() -> BuildStore {
        BuildStore { map: Mutex::new(HashMap::new()) }
    }
    pub fn get(&self, id: &str) -> Option<Build> {
        self.map.lock().get(id).cloned()
    }
    pub fn list(&self) -> Vec<Build> {
        self.map.lock().values().cloned().collect()
    }
    fn insert(&self, b: Build) {
        self.map.lock().insert(b.id.clone(), b);
    }
    fn log(&self, id: &str, line: impl Into<String>) {
        if let Some(b) = self.map.lock().get_mut(id) {
            b.lines.push(LogLine { ts_ms: now_ms(), line: line.into() });
        }
    }
    fn update(&self, id: &str, f: impl FnOnce(&mut Build)) {
        if let Some(b) = self.map.lock().get_mut(id) {
            f(b);
        }
    }
}

/// Sanitize a string for use in a container image tag ([a-z0-9._-] only).
fn sanitize_tag(s: &str) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches(|c| c == '-' || c == '.' || c == '_').to_string();
    if out.is_empty() { "app".into() } else { out }
}

pub fn project_name_from_url(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("project")
        .trim_end_matches(".git")
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

fn deploy_root() -> PathBuf {
    std::env::temp_dir().join("hive-deploys")
}

/// Start a build; returns its id immediately. The build runs in the background.
pub fn start_build(cloud: Arc<CloudState>, req: GitDeployRequest) -> String {
    let id = format!("dpl-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let project = req.project.clone().unwrap_or_else(|| project_name_from_url(&req.repo_url));
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
        lines: Vec::new(),
    });

    crate::webhooks::dispatch(
        &cloud.webhooks,
        &project,
        "deployment.created",
        serde_json::json!({ "id": id, "project": project, "repo": req.repo_url, "state": "building" }),
    );

    let bid = id.clone();
    let wh_project = project.clone();
    tokio::spawn(async move {
        // Whether this is the project's FIRST deployment — captured BEFORE the
        // placeholder registers an alias (which would otherwise make it look like a
        // redeploy). Drives `npm ci` on the initial build (Task 1).
        let first_deploy = !cloud.gw.serves_host(&format!("{project}.localhost"));
        // First-deploy only: serve a "Building…" page at the domain immediately so
        // the URL resolves throughout the build (a slow Next.js build no longer
        // 404s). The real deployment supersedes it; we then remove the placeholder.
        let placeholder = register_building_placeholder(&cloud, &project, &req).await;
        let result = run_build(&cloud, &bid, req, project, first_deploy).await;
        if let Some(pid) = &placeholder {
            // The real deploy (or the build-failed page) has taken over the alias;
            // drop the placeholder so it doesn't linger. On a hard error this also
            // clears the "Building…" page (matching prior no-deployment behavior).
            let _ = cloud.gw.remove(pid).await;
            crate::persist::persist(&cloud);
        }
        if let Err(e) = result {
            cloud.builds.log(&bid, format!("Error: {e}"));
            cloud.builds.update(&bid, |b| {
                b.state = DeployState::Error;
                b.finished_ms = Some(now_ms());
            });
            crate::webhooks::dispatch(
                &cloud.webhooks,
                &wh_project,
                "deployment.error",
                serde_json::json!({ "id": bid, "project": wh_project, "error": e.to_string() }),
            );
        }
    });
    id
}

async fn run_build(
    cloud: &Arc<CloudState>,
    bid: &str,
    req: GitDeployRequest,
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
            log(format!("Set {} environment variable(s) for the project.", env.iter().filter(|(k, _)| !k.trim().is_empty()).count()));
            crate::persist::persist(cloud);
        }
    }

    // ---- Placement scheduler / fanout (coordinator only) -------------------
    // Unless this is already a per-target deploy (`no_fanout`), decide WHERE this
    // project should be HOSTED from its configured regions + live mesh state, and
    // place it there rather than always building on this (the coordinator) node —
    // which is the resource-poor local Mac. See `schedule::place`.
    if !req.no_fanout {
        let regions = cloud.projects.get(&project).functions.regions;
        let targets = crate::schedule::place(cloud, &regions);
        let local_selected = targets.iter().any(|t| t.admin.is_none());
        let remote: Vec<crate::schedule::Target> =
            targets.iter().filter(|t| t.admin.is_some()).cloned().collect();

        if !targets.is_empty() && !local_selected {
            // Pure remote placement: do NOT build/host locally. Dispatch to the
            // chosen region node(s), mirror their build into this build record,
            // then remove the project from any other node that still hosts it.
            let names: Vec<String> = targets.iter().map(|t| t.node.clone()).collect();
            log(format!("Placement: region-aware scheduler → {}", names.join(", ")));
            let ok = fanout_remote(cloud, bid, &req, &project, &remote).await;
            cleanup_non_targets(cloud, &project, &names).await;
            cloud.builds.update(bid, |b| {
                b.state = if ok { DeployState::Ready } else { DeployState::Error };
                b.finished_ms = Some(now_ms());
            });
            crate::persist::persist(cloud);
            return Ok(());
        }
        if local_selected && !remote.is_empty() {
            let names: Vec<String> = remote.iter().map(|t| t.node.clone()).collect();
            log(format!("Placement: hosting here + replicating to {} (multi-region)", names.join(", ")));
        }
        // local_selected (host here, fanout extras at the tail) OR no eligible
        // target (empty → host locally as a safe fallback): fall through.
    }

    log(format!("Running build in {region_label} - {region}"));
    log("Build machine configuration: 4 cores, 8 GB".into());
    tokio::time::sleep(Duration::from_millis(350)).await;

    // Clone.
    let stamp = now_ms();
    let dir = deploy_root().join(format!("{project}-{stamp}"));
    tokio::fs::create_dir_all(deploy_root()).await?;
    let branch = req.branch.clone().unwrap_or_default();
    let short_repo = req.repo_url.trim_start_matches("https://").trim_end_matches(".git");
    log(format!(
        "Cloning {short_repo} (Branch: {}, Commit: HEAD)",
        if branch.is_empty() { "main" } else { &branch }
    ));

    let t0 = now_ms();
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1");
    if !branch.is_empty() {
        cmd.arg("--branch").arg(&branch);
    }
    cmd.arg(&req.repo_url).arg(&dir);
    let out = cmd.output().await?;
    anyhow::ensure!(
        out.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    log(format!("Cloning completed: {}ms", now_ms().saturating_sub(t0)));

    let commit = run_git(&dir, &["rev-parse", "--short", "HEAD"]).await.unwrap_or_default();
    // Full SHA for GitHub commit-status reporting (the statuses API needs it).
    let full_sha = run_git(&dir, &["rev-parse", "HEAD"]).await.unwrap_or_else(|| commit.clone());
    let commit_message = run_git(&dir, &["log", "-1", "--pretty=%s"]).await.unwrap_or_default();
    // Best-effort "pending" check on the commit (no-op without GITHUB_TOKEN).
    {
        let (repo, sha) = (req.repo_url.clone(), full_sha.clone());
        tokio::spawn(async move {
            report_github_status(&repo, &sha, "pending", "", "Build in progress…").await;
        });
    }
    let actual_branch = if branch.is_empty() {
        run_git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap_or_else(|| "main".into())
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
    let build_dir = match effective_root.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(root) => {
            log(format!("Root directory: {root}"));
            dir.join(root)
        }
        None => dir.clone(),
    };
    anyhow::ensure!(build_dir.exists(), "root directory '{}' not found in repo", effective_root.unwrap_or_default());

    // ---- Ignored Build Step (vercel.json `ignoreCommand`) ----
    // Vercel semantics: run the command in the project root; exit 0 => skip this
    // build entirely (no new deployment — the prior one keeps serving), non-zero
    // => continue. Lets a repo short-circuit commits that don't need a rebuild.
    if let Some(vc) = fluid_build::load_vercel_config(&build_dir) {
        if let Some(cmd) = vc.ignore_command.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            log(format!("Running Ignored Build Step: {cmd}"));
            if let Ok(st) = Command::new("sh").arg("-c").arg(&cmd).current_dir(&build_dir).status().await {
                if st.success() {
                    log("Ignored Build Step exited 0 — skipping this build (no changes to deploy).".into());
                    cloud.builds.update(bid, |b| {
                        b.state = DeployState::Ready;
                        b.finished_ms = Some(now_ms());
                    });
                    return Ok(());
                }
                log(format!("Ignored Build Step exited {} — continuing build.", st.code().unwrap_or(-1)));
            }
        }
        // devCommand / bunVersion are recorded for parity but not executed: the
        // platform has no local dev server and manages the runtime itself.
        if let Some(dc) = vc.dev_command.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            log(format!("vercel.json devCommand: {dc} (informational — not executed)"));
        }
        if let Some(bv) = vc.bun_version.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            log(format!("vercel.json bunVersion: {bv} (informational)"));
        }
    }

    // Produce the deployment manifest. A build failure must NOT abort the
    // deploy — Vercel still records the deployment/project — so on error we fall
    // back to a "build failed" page and keep going (build state ends as Error).
    let mut build_failed = false;
    let mut manifest = match produce_manifest(cloud, bid, &dir, &build_dir, &project, &commit, first_deploy, req.use_cache).await {
        Ok(m) => m,
        Err(e) => {
            build_failed = true;
            log(format!("Build failed: {e}"));
            log("Keeping the deployment — serving a build-status page so the project still exists.".into());
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
                .map(|r| fluid_core::Redirect { source: r.source.clone(), destination: r.destination.clone(), status: r.status, has: vec![], missing: vec![] })
                .collect();
            manifest.rewrites = feats
                .rewrites
                .iter()
                .map(|r| fluid_core::Rewrite { source: r.source.clone(), destination: r.destination.clone(), has: vec![], missing: vec![] })
                .collect();
            if let Some(mw) = &feats.middleware {
                manifest.middleware = Some(fluid_core::Middleware { matcher: mw.matcher.clone(), runtime: mw.runtime.clone() });
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
        let base = if static_dir == "." { build_dir.clone() } else { build_dir.join(&static_dir) };
        let index = base.join("index.html");
        if !index.exists() {
            let _ = tokio::fs::create_dir_all(&base).await;
            let html = landing_page(&project, &commit, &commit_message, &req.repo_url);
            let _ = tokio::fs::write(&index, html).await;
            log("No index.html found — generated a default landing page.".into());
        }
    }

    log("Uploading build outputs…".into());
    log(format!("Functions: {}, Static assets prepared.", manifest.functions.len()));

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
        if is_production { "Production" } else { "Preview" },
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
        let rc_scope = format!("{}:{}", project, if is_production { "production" } else { "preview" });
        for f in manifest.functions.iter_mut() {
            f.env.insert("HIVE_RUNTIME_CACHE_URL".into(), rc_url.clone());
            f.env.insert("HIVE_RUNTIME_CACHE_SCOPE".into(), rc_scope.clone());
        }
    }

    // Register the routable deployment.
    let git = GitSource {
        repo_url: req.repo_url.clone(),
        branch: actual_branch,
        commit: commit.clone(),
        commit_message: commit_message.clone(),
    };
    // For an isolated backend (Firecracker), a serving microVM cannot see the
    // host build dir the mock backend serves from. Pack the build output into a
    // per-deployment artifact the cell mounts at /build, and point this
    // deployment's function pool at that image. No-op for the same-host mock.
    if !build_failed && cloud.gw.backend_name() == "firecracker" {
        let image = format!("dpl-{}", sanitize_tag(bid));
        match cloud.gw.deliver_build(&image, &build_dir).await {
            Ok(()) => {
                log("Delivered build output to a microVM image (Firecracker).".into());
                manifest.image = Some(image);
            }
            Err(e) => log(format!("WARN: could not deliver build to microVM ({e}); serving may fail.")),
        }
    }

    // Capture vercel.json crons before the manifest is moved into the gateway —
    // they're registered (production only) after the deployment is live.
    let cron_specs = manifest.crons.clone();

    // Tenant = the project's team; tags the deployment + every cell it spawns so
    // compute is partitioned and quota'd per team (same resolver billing/audit use).
    let tenant = cloud.projects.team_of(&manifest.project);
    let info = cloud.gw.deploy_full(
        build_dir.to_string_lossy().into_owned(),
        manifest,
        req.creator.clone().unwrap_or_else(|| "you".into()),
        Some(git),
        is_production,
        if build_failed { DeployState::Error } else { DeployState::Ready },
        tenant,
    );

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
        log(format!("Detected Vercel WDK: registered {ingested} workflow(s) for the Workflows tab."));
    }

    if build_failed {
        log(format!("Deployment created (build failed). Aliased to {}", info.alias));
    } else {
        log(format!("Deployment ready. Aliased to {}", info.alias));
    }
    cloud.builds.update(bid, |b| {
        b.state = if build_failed { DeployState::Error } else { DeployState::Ready };
        b.finished_ms = Some(now_ms());
        b.deployment_id = Some(info.id.to_string());
        b.alias = Some(info.alias.clone());
    });
    let ev = cloud.event(&cloud.region, "DEPLOY", &info.alias, "/", 200, "deploy", &format!("git {}", req.repo_url));
    cloud.record(ev);
    cloud.audit.record(
        &cloud.projects.team_of(&info.project),
        &req.creator.clone().unwrap_or_else(|| "you".into()),
        if build_failed { "create_failed" } else { "create" },
        "deployment",
        &info.id.to_string(),
        &format!("{} → {}", info.project, info.alias),
    );
    crate::persist::persist(cloud);
    // Best-effort final GitHub commit status (success/failure). No-op without a
    // GITHUB_TOKEN; points the check at the live deployment URL.
    {
        let (repo, sha) = (req.repo_url.clone(), full_sha.clone());
        let url = format!("https://{}", info.alias);
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
        if is_production { "deployment.promoted" } else { "deployment.ready" },
        serde_json::json!({
            "id": info.id.to_string(),
            "project": info.project,
            "url": format!("https://{}", info.alias),
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
        let targets = crate::schedule::place(cloud, &regions);
        if targets.iter().any(|t| t.admin.is_none()) {
            let remote: Vec<crate::schedule::Target> =
                targets.iter().filter(|t| t.admin.is_some()).cloned().collect();
            if !remote.is_empty() {
                let _ = fanout_remote(cloud, bid, &req, &project, &remote).await;
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
async fn fanout_remote(
    cloud: &Arc<CloudState>,
    bid: &str,
    req: &GitDeployRequest,
    project: &str,
    remote: &[crate::schedule::Target],
) -> bool {
    let log = |s: String| cloud.builds.log(bid, s);
    let team = cloud.projects.team_of(project);
    let env = cloud.projects.env_map(project);
    let mut all_ok = true;
    for t in remote {
        let admin = match &t.admin {
            Some(a) => a.clone(),
            None => continue,
        };
        let mut dreq = req.clone();
        dreq.no_fanout = true;
        dreq.project = Some(project.to_string());
        dreq.env = Some(env.clone()); // carry env so a redeploy isn't env-less on the target
        log(format!("→ {}: dispatching deploy", t.node));
        let resp = cloud
            .http
            .post(format!("{admin}/v1/git/deploy"))
            .header("x-hive-team", team.clone())
            .json(&dreq)
            .timeout(Duration::from_secs(15))
            .send()
            .await;
        let target_bid = match resp {
            Ok(r) => r.json::<serde_json::Value>().await.ok().and_then(|v| v.get("build_id").and_then(|x| x.as_str()).map(String::from)),
            Err(e) => {
                log(format!("✗ {}: dispatch failed: {e}", t.node));
                all_ok = false;
                continue;
            }
        };
        let Some(target_bid) = target_bid else {
            log(format!("✗ {}: no build id returned", t.node));
            all_ok = false;
            continue;
        };
        let ok = mirror_remote_build(cloud, bid, &admin, &target_bid, &t.node).await;
        if !ok {
            all_ok = false;
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
    admin: &str,
    target_bid: &str,
    node: &str,
) -> bool {
    let mut mirrored = 0usize;
    let deadline = now_ms() + 10 * 60 * 1000; // 10 min cap
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let v = match cloud
            .http
            .get(format!("{admin}/v1/builds/{target_bid}"))
            .timeout(Duration::from_secs(8))
            .send()
            .await
            .ok()
        {
            Some(r) => r.json::<serde_json::Value>().await.ok(),
            None => None,
        };
        let Some(v) = v else {
            if now_ms() > deadline {
                cloud.builds.log(bid, format!("✗ {node}: lost contact with remote build"));
                return false;
            }
            continue;
        };
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
            cloud.builds.log(bid, format!("✗ {node}: build failed"));
            return false;
        }
        if now_ms() > deadline {
            cloud.builds.log(bid, format!("✗ {node}: remote build timed out"));
            return false;
        }
    }
}

/// After placing a project on its target node(s), tell every OTHER node that
/// still hosts it to delete it — so changing regions RELOCATES the deployment
/// rather than leaving stale copies. Best-effort; never fails the deploy.
async fn cleanup_non_targets(cloud: &Arc<CloudState>, project: &str, target_names: &[String]) {
    // Which nodes currently host this project? Derive from the gossiped routes:
    // any host alias that starts with "<project>." or "<project>-".
    let admins = cloud.node_admins.read().clone();
    let routes = cloud.peer_routes.read().clone();
    let mut hosting: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (host, rs) in routes.iter() {
        let sub = host.split('.').next().unwrap_or(host);
        if sub == project || sub.starts_with(&format!("{project}-")) {
            for r in rs {
                hosting.insert(r.node_id.clone());
            }
        }
    }
    let team = cloud.projects.team_of(project);
    for node in hosting {
        if target_names.iter().any(|t| t == &node) {
            continue; // keep the chosen targets
        }
        if let Some(admin) = admins.get(&node) {
            let _ = cloud
                .http
                .delete(format!("{admin}/v1/projects/{project}"))
                .header("x-hive-team", team.clone())
                .timeout(Duration::from_secs(15))
                .send()
                .await;
        }
    }
}

/// Best-effort GitHub Commit Status report (Vercel-style "shadw — Deployment
/// ready" check on the commit/PR). No-op unless `GITHUB_TOKEN` is set in the
/// node's environment and the repo is on github.com. All failures are swallowed
/// so deploys never depend on GitHub being reachable.
async fn report_github_status(repo_url: &str, sha: &str, state: &str, target_url: &str, description: &str) {
    let token = match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return,
    };
    if sha.is_empty() {
        return;
    }
    let Some((owner, repo)) = parse_owner_repo(repo_url) else { return };
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
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);
    let dockerfile = dir.join("Dockerfile");
    if dockerfile.exists() {
        log("Detected Dockerfile — building container image.".into());
        let safe_project = sanitize_tag(project);
        let image = format!("hive-{}-{}", safe_project, &commit[..commit.len().min(7)]);
        let exposed = parse_expose(&dockerfile).await.unwrap_or(8080);
        let t1 = now_ms();
        let mut build = Command::new("podman");
        build.arg("build").arg("-t").arg(&image);
        // Pass project env as --build-arg so a Dockerfile `ARG`/`ENV` can use them.
        for (k, v) in cloud.projects.env_map(project) {
            build.arg("--build-arg").arg(format!("{k}={v}"));
        }
        let out = build
            .arg(".")
            .current_dir(dir)
            .output()
            .await?;
        for line in String::from_utf8_lossy(&out.stderr)
            .lines()
            .chain(String::from_utf8_lossy(&out.stdout).lines())
            .filter(|l| !l.trim().is_empty())
            .take(40)
        {
            log(format!("  {line}"));
        }
        anyhow::ensure!(out.status.success(), "podman build failed");
        log(format!("Image built: {image} ({}ms), EXPOSE {exposed}", now_ms().saturating_sub(t1)));
        Ok(container_manifest(project, &image, exposed))
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
    let Ok(s) = tokio::fs::read_to_string(dir.join("package.json")).await else { return false };
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
fn should_use_npm_ci(pm: &str, has_package_lock: bool, first_deploy: bool, use_cache: bool) -> bool {
    pm == "npm" && has_package_lock && (first_deploy || !use_cache)
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
        vc.as_ref().and_then(sel).map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_string)
    };
    let inst = vc_pick(|c| c.install_command.as_ref()).or_else(|| pick(|b| &b.install_command));
    let bld = vc_pick(|c| c.build_command.as_ref()).or_else(|| pick(|b| &b.build_command));
    let outd = vc_pick(|c| c.output_directory.as_ref()).or_else(|| pick(|b| &b.output_dir));
    // An explicit framework choice (vercel.json, else project settings) overrides
    // auto-detection.
    let fwo = vc_pick(|c| c.framework.as_ref()).or_else(|| pick(|b| &b.framework));

    let plan = fluid_build::plan_build(dir, fwo.as_deref(), inst.as_deref(), bld.as_deref(), outd.as_deref());

    // Generic "Static" framework = publish the files as-is: no install, no build.
    // This honors the user's explicit choice and, crucially, never runs `npm
    // install` — so repos with a build-y `postinstall` (e.g. jor1k's `./compile`)
    // deploy as plain static sites instead of failing the install. Serve the
    // configured output dir, else the repo root (where index.html lives).
    if plan.framework.slug == "static" {
        let sd = outd.clone().unwrap_or_else(|| ".".to_string());
        log(format!("Static project (framework override) — skipping install/build; serving \"{sd}\" as-is."));
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
    let pm = if foreign_subdir { "npm" } else { fluid_build::package_manager(install_dir) };

    log(format!(
        "Detected framework: {} — primitive: {:?}, package manager: {}{}",
        plan.framework.name, plan.framework.primitive, pm,
        if is_monorepo { " (workspace monorepo — installing at root)" } else { "" }
    ));
    if let Some(nd) = preferred_node_bin() {
        log(format!("Node runtime: {nd}"));
    }

    let has_pkg = install_dir.join("package.json").exists();
    // Build cache key (lockfile + package manager) for restore/save.
    let cache_key = compute_cache_key(install_dir, pm).await;

    // For a pnpm workspace, scope the install to just this package + its deps
    // (`--filter`) so we don't install the entire monorepo.
    let rel = dir.strip_prefix(repo_root).ok().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    let pnpm_filter = if is_monorepo && !rel.is_empty() { format!(" --filter \"{{./{rel}}}...\"") } else { String::new() };

    // `npm ci` is the clean, lockfile-exact install. We use it ONLY for an npm
    // project with a committed package-lock.json (never yarn/pnpm — those have
    // their own lockfiles), and ONLY when:
    //   • this is the project's FIRST deployment (Task 1 — clean initial build), or
    //   • the redeploy explicitly disabled the build cache (Task 2 — fresh install).
    // Every other build uses `npm install` + the warm node_modules cache (fast).
    // `npm ci` wipes node_modules, so it never benefits from a restored cache.
    let use_npm_ci = should_use_npm_ci(pm, install_dir.join("package-lock.json").exists(), first_deploy, use_cache);
    // Install command: honor an explicit override, else the right command for the
    // detected package manager (pnpm/yarn via corepack so the binary is present).
    let install_cmd: String = inst.unwrap_or_else(|| match pm {
        "pnpm" => format!("corepack enable pnpm >/dev/null 2>&1; pnpm install --no-frozen-lockfile{pnpm_filter}"),
        "yarn" => "corepack enable >/dev/null 2>&1; yarn install --network-timeout 600000".into(),
        "bun" => "bun install".into(),
        _ if use_npm_ci => "npm ci --no-audit --no-fund".into(),
        _ => "npm install --no-audit --no-fund".into(),
    });
    if use_npm_ci {
        log(format!(
            "Using `npm ci` (package-lock.json present, {}).",
            if first_deploy { "first deployment" } else { "build cache disabled" }
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
    let build_cmd = {
        let bc = plan.build_command.clone();
        let first = bc.split_whitespace().next().unwrap_or("");
        let is_pm_invocation = matches!(first, "npm" | "pnpm" | "yarn" | "bun" | "npx" | "bunx" | "corepack");
        if first == "npm" && pm != "npm" {
            bc.replacen("npm", pm, 1)
        } else if !bc.trim().is_empty() && !is_pm_invocation {
            match pm {
                "pnpm" => format!("corepack enable pnpm >/dev/null 2>&1; pnpm exec {bc}"),
                "yarn" => format!("corepack enable >/dev/null 2>&1; yarn exec {bc}"),
                "bun" => format!("bunx {bc}"),
                // npm: the raw binary resolves via node_modules/.bin on PATH.
                _ => bc,
            }
        } else {
            bc
        }
    };

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
        log(format!("Running \"{}\"{}", install_cmd, if is_monorepo { " (workspace root)" } else { "" }));
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
                let kit_major = tokio::fs::read_to_string(dir.join("node_modules/@sveltejs/kit/package.json"))
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
                let add = match pm {
                    "pnpm" => format!("corepack enable pnpm >/dev/null 2>&1; pnpm add -D --config.strict-peer-dependencies=false {spec}"),
                    "yarn" => format!("corepack enable >/dev/null 2>&1; yarn add -D {spec}"),
                    "bun" => format!("bun add -d {spec}"),
                    _ => format!("npm install -D --no-audit --no-fund --legacy-peer-deps {spec}"),
                };
                run_streamed(dir, &add, cloud, bid, &proj_env)
                    .await
                    .map_err(|e| anyhow::anyhow!("installing adapter-node failed: {e}"))?;
                let patched = src.replace("@sveltejs/adapter-auto", "@sveltejs/adapter-node");
                let _ = tokio::fs::write(&cfg, patched).await;
            }
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
            let sd = if has_bo { ".vercel/output/static".to_string() } else { plan.output_dir.clone() };
            log(format!("Serving static assets from \"{sd}\"."));
            Ok(static_manifest(project, &sd))
        }
        Primitive::Serverless | Primitive::Hybrid => {
            // Node-server model: the framework was just built, so its production
            // server (`next start`, `node build`, …) will boot and listen on
            // $PORT in the build dir. The gateway proxies to it. SvelteKit (built
            // with adapter-node above) runs its standalone server via `node build`.
            let start = if is_sveltekit && dir.join("build/index.js").exists() {
                vec!["node".to_string(), "build".to_string()]
            } else {
                detect_start_cmd(dir).await
            };
            log(format!("Provisioning serverless server: `{}`.", start.join(" ")));
            Ok(function_manifest(project, start))
        }
    }
}

fn static_manifest(project: &str, static_dir: &str) -> Manifest {
    Manifest {
        project: project.to_string(),
        static_dir: Some(if static_dir.is_empty() { ".".into() } else { static_dir.to_string() }),
        functions: vec![],
        routes: vec![Route { pattern: "/".into(), target: RouteTarget::Static }],
        ..Default::default()
    }
}

/// The command that boots the built app's production server.
async fn detect_start_cmd(dir: &Path) -> Vec<String> {
    if let Ok(pkg) = tokio::fs::read_to_string(dir.join("package.json")).await {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
            if v.get("scripts").and_then(|s| s.get("start")).is_some() {
                return vec!["npm".into(), "start".into()];
            }
        }
    }
    for entry in ["server.js", "index.js", "app.py", "main.py", "server.py"] {
        if dir.join(entry).exists() {
            let runner = if entry.ends_with(".py") { "python3" } else { "node" };
            return vec![runner.into(), entry.into()];
        }
    }
    vec!["npm".into(), "start".into()]
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
            if let Some(major) = name.trim_start_matches('v').split('.').next().and_then(|s| s.parse::<u32>().ok()) {
                if (20..=24).contains(&major) {
                    let bin = e.path().join("bin");
                    if bin.join("node").exists() && best.as_ref().map(|(m, _)| major > *m).unwrap_or(true) {
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

/// Run a shell command in `dir`, streaming stdout+stderr into the build log.
/// `env` is the project's environment variables, injected into the build so
/// install/build steps (e.g. Next.js reading NEXT_PUBLIC_*, Vite VITE_*) see them.
/// Ingest a Vercel WDK manifest (`.well-known/workflow/v1/manifest.json`) emitted
/// by a built app: register each workflow — with its React-Flow `graph` — in the
/// engine so it shows up in the Workflows tab/table and renders on the canvas.
/// Returns the number registered. Best-effort (a non-WDK app simply has none).
async fn ingest_workflow_manifest(cloud: &Arc<CloudState>, project: &str, dir: &Path) -> usize {
    let Some(path) = find_workflow_manifest(dir) else { return 0 };
    let Ok(text) = tokio::fs::read_to_string(&path).await else { return 0 };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else { return 0 };
    let Some(workflows) = manifest.get("workflows").and_then(|v| v.as_object()) else { return 0 };
    let mut count = 0usize;
    for defs in workflows.values() {
        let Some(defs) = defs.as_object() else { continue };
        for (name, wf) in defs {
            let id = wf.get("workflowId").and_then(|v| v.as_str()).unwrap_or(name).to_string();
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
                            let kind = node.get("data").and_then(|d| d.get("nodeKind")).and_then(|k| k.as_str()).unwrap_or("");
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
                            hive_edge::WorkflowStep { name: label, deployment: project.to_string(), path: String::new() }
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
                if matches!(name, "node_modules" | ".git" | ".next" | "dist" | "build" | ".svelte-kit" | ".vercel") {
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
    use tokio::io::{AsyncBufReadExt, BufReader};

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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let (c1, b1) = (cloud.clone(), bid.to_string());
    let t1 = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            c1.builds.log(&b1, format!("  {l}"));
        }
    });
    let (c2, b2) = (cloud.clone(), bid.to_string());
    let t2 = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            c2.builds.log(&b2, format!("  {l}"));
        }
    });
    let status = child.wait().await?;
    let _ = tokio::join!(t1, t2);
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
    for name in ["pnpm-lock.yaml", "yarn.lock", "bun.lockb", "package-lock.json", "package.json"] {
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

/// Try to fetch a cache blob from a mesh peer; write it to `dest`. Returns true on
/// success. Best-effort: any error is swallowed.
async fn try_peer_fetch(cloud: &Arc<CloudState>, key: &str, dest: &Path) -> bool {
    let peers = cloud.peers.read().clone();
    for peer in peers {
        let url = format!("{}/v1/buildcache/{}", peer.trim_end_matches('/'), key);
        if let Ok(resp) = cloud.http.get(&url).timeout(Duration::from_secs(20)).send().await {
            if resp.status().is_success() {
                if let Ok(bytes) = resp.bytes().await {
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
        cloud.builds.log(bid, format!("Pulled build cache from peer (key {key})."));
    }
    if !tar.exists() {
        cloud.builds.log(bid, "No build cache for these dependencies — installing fresh.");
        return false;
    }
    let out = Command::new("tar").arg("-xf").arg(&tar).current_dir(install_dir).output().await;
    match out {
        Ok(o) if o.status.success() => {
            cloud.builds.log(bid, format!("Restored build cache (key {key})."));
            true
        }
        _ => {
            // Corrupt/incompatible archive → drop it and install clean.
            let _ = tokio::fs::remove_file(&tar).await;
            cloud.builds.log(bid, "Build cache was unreadable — discarded; installing fresh.");
            false
        }
    }
}

/// Save node_modules (+ framework cache if present) to the content-addressed cache.
/// Best-effort, atomic (write temp + rename).
async fn save_cache(cloud: &Arc<CloudState>, bid: &str, install_dir: &Path, key: &str) {
    if !install_dir.join("node_modules").exists() {
        return;
    }
    let dir = cache_root();
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return;
    }
    let tmp = dir.join(format!("{key}.tar.tmp"));
    let final_ = dir.join(format!("{key}.tar"));
    // Include the framework's incremental cache too when it lives under the
    // install dir (e.g. node_modules/.cache); .next/cache lives in node_modules
    // for many setups but we keep this list conservative for portability.
    let mut args: Vec<String> = vec!["-cf".into(), tmp.to_string_lossy().into_owned(), "node_modules".into()];
    if install_dir.join(".next/cache").exists() {
        args.push(".next/cache".into());
    }
    let out = Command::new("tar").args(&args).current_dir(install_dir).output().await;
    if let Ok(o) = out {
        if o.status.success() && tokio::fs::rename(&tmp, &final_).await.is_ok() {
            cloud.builds.log(bid, "Saved build cache for next time.");
            return;
        }
    }
    let _ = tokio::fs::remove_file(&tmp).await;
}

/// Parse the container's listen port from a Dockerfile: prefer `EXPOSE`, else
/// `ENV PORT=`, else None (caller defaults).
async fn parse_expose(path: &Path) -> Option<u16> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let mut env_port = None;
    for line in content.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("EXPOSE ").or_else(|| l.strip_prefix("expose ")) {
            if let Some(p) = rest.split_whitespace().next().and_then(|s| s.split('/').next()) {
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
                    fluid_build::ConditionValue::Expr { pre, suf } => {
                        CondValue::Expr { pre: pre.clone(), suf: suf.clone() }
                    }
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
                headers: h.headers.iter().map(|x| Header { key: x.key.clone(), value: x.value.clone() }).collect(),
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
                .map(|p| LocalPattern { pathname: p.pathname.clone(), search: p.search.clone() })
                .collect(),
            dangerously_allow_svg: img.dangerously_allow_svg,
            content_security_policy: img.content_security_policy.clone(),
            content_disposition_type: img.content_disposition_type.clone(),
        });
    }

    if !vc.crons.is_empty() {
        m.crons = vc.crons.iter().map(|c| CronSpec { path: c.path.clone(), schedule: c.schedule.clone() }).collect();
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

fn container_manifest(project: &str, image: &str, internal: u16) -> Manifest {
    Manifest {
        project: project.to_string(),
        static_dir: None,
        functions: vec![FunctionConfig {
            name: "web".into(),
            runtime: "container".into(),
            // Structured marker the backend recognizes: run this image as a
            // detached container, mapping the cell $PORT -> internal port.
            start_cmd: vec!["__container__".into(), image.to_string(), internal.to_string()],
            env: Default::default(),
            vcpus: 1,
            memory_mib: 512,
            max_concurrency: 20,
            min_instances: 1,
            max_instances: 5,
            idle_ttl_secs: 120,
            max_duration_secs: 300,
            ..Default::default()
        }],
        routes: vec![Route { pattern: "/".into(), target: RouteTarget::Function("web".into()) }],
        ..Default::default()
    }
}

fn function_manifest(project: &str, start_cmd: Vec<String>) -> Manifest {
    Manifest {
        project: project.to_string(),
        static_dir: None,
        functions: vec![FunctionConfig {
            name: "api".into(),
            runtime: "auto".into(),
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
        routes: vec![Route { pattern: "/".into(), target: RouteTarget::Function("api".into()) }],
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
async fn register_building_placeholder(
    cloud: &Arc<CloudState>,
    project: &str,
    req: &GitDeployRequest,
) -> Option<String> {
    if cloud.gw.serves_host(&format!("{project}.localhost")) {
        return None; // redeploy — keep the live version serving until ready
    }
    let dir = deploy_root().join(format!("{project}-building-{}", now_ms()));
    tokio::fs::create_dir_all(&dir).await.ok()?;
    tokio::fs::write(dir.join("index.html"), building_page(project)).await.ok()?;
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

async fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().await.ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            functions: vec![FunctionConfig { name: "api/hello".into(), ..Default::default() }],
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
        assert_eq!(project_name_from_url("https://github.com/vercel/next.js.git"), "next-js");
        assert_eq!(project_name_from_url("https://github.com/Owner/My_Repo"), "my-repo");
        assert_eq!(project_name_from_url("git@github.com:acme/cool-app.git"), "cool-app");
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
        assert!(sanitize_tag("Foo/Bar:Baz").chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-'));
    }

    #[test]
    fn preferred_node_bin_never_panics() {
        // May be Some or None depending on the host; must not panic and, if Some,
        // must point at an existing `node`.
        if let Some(dir) = preferred_node_bin() {
            assert!(std::path::Path::new(&dir).join("node").exists());
        }
    }

    #[tokio::test]
    async fn cache_key_is_deterministic_and_content_sensitive() {
        let base = std::env::temp_dir().join(format!("oe-cachekey-{}", now_ms()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        tokio::fs::write(base.join("package-lock.json"), b"{\"v\":1}").await.unwrap();

        let k1 = compute_cache_key(&base, "npm").await;
        let k2 = compute_cache_key(&base, "npm").await;
        assert!(k1.is_some());
        assert_eq!(k1, k2, "same lockfile+pm must yield the same key");

        // Different package manager → different key.
        let k_pnpm = compute_cache_key(&base, "pnpm").await;
        assert_ne!(k1, k_pnpm);

        // Changed lockfile → different key.
        tokio::fs::write(base.join("package-lock.json"), b"{\"v\":2}").await.unwrap();
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
}
