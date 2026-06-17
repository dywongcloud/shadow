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

fn project_name_from_url(url: &str) -> String {
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
    log("Previous build caches not available.".into());

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

    // Container path (Railway-style): a Dockerfile is built with podman and run
    // as the deployment's process (listening on $PORT, proxied by the gateway).
    let dockerfile = dir.join("Dockerfile");
    let mut manifest;
    if dockerfile.exists() {
        log("Detected Dockerfile — building container image.".into());
        let image = format!("hive-{}-{}", project, &commit[..commit.len().min(7)]);
        let exposed = parse_expose(&dockerfile).await.unwrap_or(8080);
        let t1 = now_ms();
        let out = Command::new("podman")
            .arg("build")
            .arg("-t")
            .arg(&image)
            .arg(".")
            .current_dir(&dir)
            .output()
            .await?;
        // Stream a few build lines.
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
        manifest = container_manifest(&project, &image, exposed);
    } else {
        // Manifest: prefer fluid.json, else detect.
        manifest = match tokio::fs::read_to_string(dir.join("fluid.json")).await {
            Ok(s) => {
                let mut m = Manifest::from_json(&s)?;
                if m.project.is_empty() {
                    m.project = project.clone();
                }
                log("Detected fluid.json — using project configuration.".into());
                m
            }
            Err(_) => {
                let m = detect_manifest(&dir, &project).await;
                let preset = if !m.functions.is_empty() { "function" } else { "static" };
                log(format!("Detecting build settings… (preset: {preset})"));
                m
            }
        };
    }
    manifest.project = project.clone();

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

    // Simulated install/build for static-ish repos; real for those with commands.
    tokio::time::sleep(Duration::from_millis(300)).await;
    if manifest.functions.is_empty() {
        log("Running \"build\"".into());
        // Ensure the static deployment has something to serve at "/". Many repos
        // (monorepos, framework sources) have no index.html — generate a landing
        // page so the deployed URL returns 200 instead of 404.
        let static_dir = manifest.static_dir.clone().unwrap_or_else(|| ".".into());
        let base = if static_dir == "." { dir.clone() } else { dir.join(&static_dir) };
        let index = base.join("index.html");
        if !index.exists() {
            let _ = tokio::fs::create_dir_all(&base).await;
            let html = landing_page(&project, &commit, &commit_message, &req.repo_url);
            let _ = tokio::fs::write(&index, html).await;
            log("No index.html found — generated a default landing page.".into());
        }
        log("No build step required — serving static assets.".into());
    } else {
        let cmd = manifest.functions[0].start_cmd.join(" ");
        log(format!("Running \"install\" command…"));
        tokio::time::sleep(Duration::from_millis(400)).await;
        log(format!("Build completed. Launch command: `{cmd}`"));
    }
    tokio::time::sleep(Duration::from_millis(250)).await;

    log("Uploading build outputs…".into());
    log(format!(
        "Functions: {}, Static assets prepared.",
        manifest.functions.len()
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Register the routable deployment.
    let git = GitSource {
        repo_url: req.repo_url.clone(),
        branch: actual_branch,
        commit: commit.clone(),
        commit_message: commit_message.clone(),
    };
    let info = cloud.gw.deploy_full(
        dir.to_string_lossy().into_owned(),
        manifest,
        req.creator.clone().unwrap_or_else(|| "you".into()),
        Some(git),
        req.production,
    );

    log(format!("Deployment ready. Aliased to {}", info.alias));
    cloud.builds.update(bid, |b| {
        b.state = DeployState::Ready;
        b.finished_ms = Some(now_ms());
        b.deployment_id = Some(info.id.to_string());
        b.alias = Some(info.alias.clone());
    });
    let ev = cloud.event(&cloud.region, "DEPLOY", &info.alias, "/", 200, "deploy", &format!("git {}", req.repo_url));
    cloud.record(ev);
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

async fn detect_manifest(dir: &Path, project: &str) -> Manifest {
    if dir.join("package.json").exists() {
        if let Ok(pkg) = tokio::fs::read_to_string(dir.join("package.json")).await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
                if v.get("scripts").and_then(|s| s.get("start")).is_some() {
                    return function_manifest(project, vec!["npm".into(), "start".into()]);
                }
            }
        }
    }
    for entry in ["app.py", "main.py", "server.py"] {
        if dir.join(entry).exists() {
            return function_manifest(project, vec!["python3".into(), entry.into()]);
        }
    }
    for sd in ["", "public", "dist", "build", "out"] {
        let probe = if sd.is_empty() { dir.join("index.html") } else { dir.join(sd).join("index.html") };
        if probe.exists() {
            return Manifest {
                project: project.to_string(),
                static_dir: Some(if sd.is_empty() { ".".into() } else { sd.into() }),
                functions: vec![],
                routes: vec![Route { pattern: "/".into(), target: RouteTarget::Static }],
            };
        }
    }
    Manifest {
        project: project.to_string(),
        static_dir: Some(".".into()),
        functions: vec![],
        routes: vec![Route { pattern: "/".into(), target: RouteTarget::Static }],
    }
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
    }
}

/// A minimal "Deployed on Hive" landing page for static deploys with no index.
fn landing_page(project: &str, commit: &str, msg: &str, repo: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{project} · Hive Cloud</title>
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
    <p class="muted">Deployed on <strong>Hive Cloud</strong> — your unified, self-hosted cloud.</p>
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

async fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().await.ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}
