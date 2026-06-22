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
                .map(|r| fluid_core::Redirect { source: r.source.clone(), destination: r.destination.clone(), status: r.status })
                .collect();
            manifest.rewrites = feats
                .rewrites
                .iter()
                .map(|r| fluid_core::Rewrite { source: r.source.clone(), destination: r.destination.clone() })
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
        f.memory_mib = fsettings.memory_mib;
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

    // Register the routable deployment.
    let git = GitSource {
        repo_url: req.repo_url.clone(),
        branch: actual_branch,
        commit: commit.clone(),
        commit_message: commit_message.clone(),
    };
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
    Ok(())
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
    let inst = pick(|b| &b.install_command);
    let bld = pick(|b| &b.build_command);
    let outd = pick(|b| &b.output_dir);

    let plan = fluid_build::plan_build(dir, inst.as_deref(), bld.as_deref(), outd.as_deref());

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
            // $PORT in the build dir. The gateway proxies to it.
            let start = detect_start_cmd(dir).await;
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
            memory_mib: 512,
            max_concurrency: 20,
            min_instances: 1,
            max_instances: 5,
            idle_ttl_secs: 120,
            max_duration_secs: 300,
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
            memory_mib: 512,
            max_concurrency: 10,
            min_instances: 1,
            max_instances: 5,
            idle_ttl_secs: 60,
            max_duration_secs: 300,
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
