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
        if let Err(e) = run_build(&cloud, &bid, req, project).await {
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
) -> anyhow::Result<()> {
    let region = &cloud.region;
    let region_label = region_label(region);
    let log = |s: String| cloud.builds.log(bid, s);

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
    let commit_message = run_git(&dir, &["log", "-1", "--pretty=%s"]).await.unwrap_or_default();
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
    let mut manifest = match produce_manifest(cloud, bid, &dir, &build_dir, &project, &commit).await {
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

    // Register the routable deployment.
    let git = GitSource {
        repo_url: req.repo_url.clone(),
        branch: actual_branch,
        commit: commit.clone(),
        commit_message: commit_message.clone(),
    };
    let info = cloud.gw.deploy_full(
        build_dir.to_string_lossy().into_owned(),
        manifest,
        req.creator.clone().unwrap_or_else(|| "you".into()),
        Some(git),
        req.production,
        if build_failed { DeployState::Error } else { DeployState::Ready },
    );

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
    crate::webhooks::dispatch(
        &cloud.webhooks,
        &info.project,
        if req.production { "deployment.promoted" } else { "deployment.ready" },
        serde_json::json!({
            "id": info.id.to_string(),
            "project": info.project,
            "url": format!("https://{}", info.alias),
            "state": "ready",
            "production": req.production,
            "commit": commit,
        }),
    );
    Ok(())
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
) -> anyhow::Result<Manifest> {
    let log = |s: String| cloud.builds.log(bid, s);
    let dockerfile = dir.join("Dockerfile");
    if dockerfile.exists() {
        log("Detected Dockerfile — building container image.".into());
        let safe_project = sanitize_tag(project);
        let image = format!("hive-{}-{}", safe_project, &commit[..commit.len().min(7)]);
        let exposed = parse_expose(&dockerfile).await.unwrap_or(8080);
        let t1 = now_ms();
        let out = Command::new("podman")
            .arg("build")
            .arg("-t")
            .arg(&image)
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
        build_via_fdi(cloud, bid, repo_root, dir, project).await
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
    let is_monorepo = dir != repo_root
        && uses_workspace_protocol(dir).await
        && is_workspace_root(repo_root).await;
    let install_dir = if is_monorepo { repo_root } else { dir };
    let pm = fluid_build::package_manager(install_dir);

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

    // Install command: honor an explicit override, else the right command for the
    // detected package manager (pnpm/yarn via corepack so the binary is present).
    let install_cmd: String = inst.unwrap_or_else(|| match pm {
        "pnpm" => format!("corepack enable pnpm >/dev/null 2>&1; pnpm install --no-frozen-lockfile{pnpm_filter}"),
        "yarn" => "corepack enable >/dev/null 2>&1; yarn install --network-timeout 600000".into(),
        "bun" => "bun install".into(),
        _ => "npm install --no-audit --no-fund".into(),
    });
    // Build command: detected framework default (already package-manager aware),
    // re-pointed to the install PM if it's a generic `npm run` script.
    let build_cmd = if plan.build_command.starts_with("npm ") && pm != "npm" {
        plan.build_command.replacen("npm", pm, 1)
    } else {
        plan.build_command.clone()
    };

    // 0) Restore cached dependencies (local, else pull from a mesh peer).
    if let Some(k) = &cache_key {
        restore_cache(cloud, bid, install_dir, k).await;
    }
    // 1) Install dependencies at the install dir (root for monorepos). With a
    // restored node_modules this is a fast verify; otherwise a clean install.
    if has_pkg && !install_cmd.trim().is_empty() {
        log(format!("Running \"{}\"{}", install_cmd, if is_monorepo { " (workspace root)" } else { "" }));
        run_streamed(install_dir, &install_cmd, cloud, bid)
            .await
            .map_err(|e| anyhow::anyhow!("install command failed: {e}"))?;
    }
    // 2) Build in the project directory.
    if has_pkg && !build_cmd.trim().is_empty() {
        log(format!("Running \"{}\"", build_cmd));
        run_streamed(dir, &build_cmd, cloud, bid)
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
async fn run_streamed(dir: &Path, command: &str, cloud: &Arc<CloudState>, bid: &str) -> anyhow::Result<()> {
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
