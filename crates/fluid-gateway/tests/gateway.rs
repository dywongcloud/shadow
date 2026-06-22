//! Deterministic end-to-end gateway test: concurrent requests through the full
//! gateway -> Fluid pool -> mock instance -> multiplexed tunnel -> function.
//! Uses reqwest (no curl/bash) so it's reproducible in CI. Skips without python3.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use fluid_compute::{Fluid, FluidConfig};
use fluid_core::{FunctionConfig, Manifest, Route, RouteTarget};
use fluid_gateway::Gateway;
use hive_backend::mock::{MockBackend, MockConfig};

fn python3() -> Option<String> {
    for c in ["python3", "python"] {
        if std::process::Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(c.to_string());
        }
    }
    None
}

#[tokio::test]
async fn gateway_handles_concurrent_requests() {
    let Some(py) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };

    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("gw-test-{}", std::process::id())),
        provision_latency: Duration::from_millis(10),
        cache_root: std::env::temp_dir().join(format!("gw-cache-{}", std::process::id())),
    }));
    let fluid = Fluid::start(backend, FluidConfig::default());
    let gw = Gateway::new(fluid, "default".into());

    // examples/hello holds server.py.
    let hello_dir = format!("{}/../../examples/hello", env!("CARGO_MANIFEST_DIR"));
    let manifest = Manifest {
        project: "hello".into(),
        static_dir: Some("public".into()),
        functions: vec![FunctionConfig {
            name: "api".into(),
            runtime: "python".into(),
            start_cmd: vec![py, "server.py".into()],
            env: BTreeMap::new(),
            memory_mib: 128,
            max_concurrency: 5,
            min_instances: 0,
            max_instances: 4,
            idle_ttl_secs: 30,
            max_duration_secs: 300,
        }],
        routes: vec![Route {
            pattern: "/api".into(),
            target: RouteTarget::Function("api".into()),
        }],
        ..Default::default()
    };
    gw.deploy(hello_dir, manifest);

    // Serve the public router on an ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, fluid_gateway::public_router(gw)).await.unwrap();
    });

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Warm-up to avoid every concurrent request triggering a cold start at once.
    let _ = client.get(format!("{base}/api/hello")).send().await;

    // Fire 100 concurrent requests; every one must succeed with the right body.
    let mut handles = Vec::new();
    for i in 0..100 {
        let client = client.clone();
        let base = base.clone();
        handles.push(tokio::spawn(async move {
            let resp = tokio::time::timeout(
                Duration::from_secs(20),
                client.get(format!("{base}/api/req{i}")).send(),
            )
            .await
            .expect("request timed out")
            .expect("send failed");
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            (status.as_u16(), body.contains("hello from a Fluid function"))
        }));
    }

    let mut ok = 0;
    for h in handles {
        let (status, good_body) = h.await.expect("task panicked");
        assert_eq!(status, 200, "expected 200");
        assert!(good_body, "expected function body");
        ok += 1;
    }
    assert_eq!(ok, 100, "all 100 concurrent gateway requests should succeed");
}

#[tokio::test]
async fn max_duration_504_and_error_isolation() {
    let Some(py) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("gw2-{}", std::process::id())),
        provision_latency: Duration::from_millis(10),
        cache_root: std::env::temp_dir().join(format!("gw2-cache-{}", std::process::id())),
    }));
    let fluid = Fluid::start(backend, FluidConfig::default());
    let gw = Gateway::new(fluid, "default".into());
    let hello_dir = format!("{}/../../examples/hello", env!("CARGO_MANIFEST_DIR"));
    gw.deploy(
        hello_dir,
        Manifest {
            project: "hello".into(),
            static_dir: Some("public".into()),
            functions: vec![FunctionConfig {
                name: "api".into(),
                runtime: "python".into(),
                start_cmd: vec![py, "server.py".into()],
                env: BTreeMap::new(),
                memory_mib: 128,
                max_concurrency: 10,
                min_instances: 1,
                max_instances: 2,
                idle_ttl_secs: 30,
                max_duration_secs: 1, // tiny budget so /api/verylong (5s) trips it
            }],
            routes: vec![Route { pattern: "/api".into(), target: RouteTarget::Function("api".into()) }],
            ..Default::default()
        },
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, fluid_gateway::public_router(gw)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let _ = client.get(format!("{base}/api/hello")).send().await; // warm

    // Fire a too-long request + a failing request + normal requests concurrently.
    let long = {
        let c = client.clone(); let b = base.clone();
        tokio::spawn(async move { c.get(format!("{b}/api/verylong")).send().await.unwrap().status().as_u16() })
    };
    let boom = {
        let c = client.clone(); let b = base.clone();
        tokio::spawn(async move { c.get(format!("{b}/api/boom")).send().await.unwrap().status().as_u16() })
    };
    // Normal requests must keep succeeding while the above misbehave (isolation).
    let mut normal_ok = 0;
    for _ in 0..10 {
        let r = client.get(format!("{base}/api/hello")).send().await.unwrap();
        if r.status() == 200 && r.text().await.unwrap_or_default().contains("hello from a Fluid function") {
            normal_ok += 1;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let long_status = long.await.unwrap();
    let boom_status = boom.await.unwrap();
    assert_eq!(long_status, 504, "over-budget request should 504 (max duration)");
    assert!(boom_status >= 500, "failing handler should error, got {boom_status}");
    assert_eq!(normal_ok, 10, "normal requests must survive (error isolation)");
}

/// Regression: after a restart, a project's host alias must resolve to its
/// PRODUCTION deployment — never to a stale prior deployment that happens to be
/// restored last. (This caused `ctr-demo.localhost` to serve the old broken
/// build after a reboot even though a newer working deployment existed.)
#[tokio::test]
async fn restore_prefers_production_deployment_for_alias() {
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("gw-restore-{}", std::process::id())),
        provision_latency: Duration::from_millis(1),
        cache_root: std::env::temp_dir().join(format!("gw-restore-cache-{}", std::process::id())),
    }));
    let fluid = Fluid::start(backend, FluidConfig::default());
    let gw = Gateway::new(fluid, "default".into());

    let mk = |id: &str, created: u64, production: bool| fluid_core::DeployRecord {
        id: id.into(),
        project: "ctr-demo".into(),
        root: "/nonexistent".into(),
        manifest: Manifest { project: "ctr-demo".into(), ..Default::default() },
        created_at_ms: created,
        creator: "you".into(),
        git: None,
        production,
        target: if production { "production".into() } else { "preview".into() },
        state: fluid_core::DeployState::Ready,
        tenant: "personal".into(),
    };

    // Restore the NEWER production deployment first, then the OLDER non-production
    // one LAST — the order that used to make the stale one win the alias.
    gw.restore(mk("dpl-new", 2000, true));
    gw.restore(mk("dpl-old", 1000, false));

    // The project host alias must point at the PRODUCTION deployment, regardless
    // of restore order — this is the regression being guarded.
    assert_eq!(
        gw.host_deployment_id("ctr-demo.localhost").as_deref(),
        Some("dpl-new"),
        "project alias must resolve to the production deployment, not the stale one"
    );
    // Per-deployment preview URLs still resolve each exact deployment.
    assert_eq!(gw.host_deployment_id("dpl-old.localhost").as_deref(), Some("dpl-old"));
    assert_eq!(gw.host_deployment_id("dpl-new.localhost").as_deref(), Some("dpl-new"));
}

fn test_gw() -> Arc<Gateway> {
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("gw-alias-{}-{:p}", std::process::id(), &0u8)),
        provision_latency: Duration::from_millis(1),
        cache_root: std::env::temp_dir().join(format!("gw-alias-cache-{}", std::process::id())),
    }));
    let fluid = Fluid::start(backend, FluidConfig::default());
    Gateway::new(fluid, "default".into())
}

fn git(branch: &str, commit: &str) -> fluid_core::GitSource {
    fluid_core::GitSource {
        repo_url: "https://github.com/acme/app".into(),
        branch: branch.into(),
        commit: commit.into(),
        commit_message: "msg".into(),
    }
}

/// Vercel's 3 URL types: a production deploy must be reachable at the production
/// domain (`app`), the immutable commit URL (`app-<sha>`), the branch URL
/// (`app-git-<branch>`) and its per-deployment id URL — all at once.
#[tokio::test]
async fn deploy_assigns_commit_branch_and_production_urls() {
    let gw = test_gw();
    let m = Manifest { project: "app".into(), ..Default::default() };
    let info = gw.deploy_full("/nonexistent".into(), m, "you".into(), Some(git("main", "abc1234def")), true, fluid_core::DeployState::Ready, "personal".into());
    let id = info.id.to_string();

    assert_eq!(gw.host_deployment_id("app.localhost").as_deref(), Some(id.as_str()), "production domain");
    assert_eq!(gw.host_deployment_id("app-abc1234.localhost").as_deref(), Some(id.as_str()), "commit URL (short sha)");
    assert_eq!(gw.host_deployment_id("app-git-main.localhost").as_deref(), Some(id.as_str()), "branch URL");
    assert_eq!(gw.host_deployment_id(&format!("{id}.localhost")).as_deref(), Some(id.as_str()), "per-deployment id URL");
}

/// A PREVIEW deploy (non-production branch) must get its own commit + branch URLs
/// but must NOT steal the production domain from the live production deployment.
#[tokio::test]
async fn preview_deploy_does_not_hijack_production_domain() {
    let gw = test_gw();
    let prod = gw.deploy_full("/nonexistent".into(), Manifest { project: "app".into(), ..Default::default() }, "you".into(), Some(git("main", "aaaaaaa")), true, fluid_core::DeployState::Ready, "personal".into());
    let prod_id = prod.id.to_string();
    let prev = gw.deploy_full("/nonexistent".into(), Manifest { project: "app".into(), ..Default::default() }, "you".into(), Some(git("feature/login", "bbbbbbb")), false, fluid_core::DeployState::Ready, "personal".into());
    let prev_id = prev.id.to_string();

    // Production domain stays on the production deployment.
    assert_eq!(gw.host_deployment_id("app.localhost").as_deref(), Some(prod_id.as_str()), "preview must not hijack prod");
    // Each has its own immutable + branch URLs (branch name slugified).
    assert_eq!(gw.host_deployment_id("app-git-main.localhost").as_deref(), Some(prod_id.as_str()));
    assert_eq!(gw.host_deployment_id("app-git-feature-login.localhost").as_deref(), Some(prev_id.as_str()));
    assert_eq!(gw.host_deployment_id("app-bbbbbbb.localhost").as_deref(), Some(prev_id.as_str()));
}

/// The branch URL must always track the LATEST deployment on that branch (a new
/// commit on `main` re-points `app-git-main` while old commit URLs stay pinned).
#[tokio::test]
async fn branch_url_tracks_latest_commit_on_branch() {
    let gw = test_gw();
    let d1 = gw.deploy_full("/nonexistent".into(), Manifest { project: "app".into(), ..Default::default() }, "you".into(), Some(git("main", "1111111")), true, fluid_core::DeployState::Ready, "personal".into());
    let d2 = gw.deploy_full("/nonexistent".into(), Manifest { project: "app".into(), ..Default::default() }, "you".into(), Some(git("main", "2222222")), true, fluid_core::DeployState::Ready, "personal".into());
    let (id1, id2) = (d1.id.to_string(), d2.id.to_string());

    assert_eq!(gw.host_deployment_id("app-git-main.localhost").as_deref(), Some(id2.as_str()), "branch tracks newest");
    assert_eq!(gw.host_deployment_id("app-1111111.localhost").as_deref(), Some(id1.as_str()), "old commit URL stays pinned");
    assert_eq!(gw.host_deployment_id("app-2222222.localhost").as_deref(), Some(id2.as_str()));
    assert_eq!(gw.host_deployment_id("app.localhost").as_deref(), Some(id2.as_str()), "prod domain follows newest production");
}

/// Promotion / rollback is pure alias reassignment — promoting an older preview
/// makes it production WITHOUT a rebuild, re-pointing the production domain.
#[tokio::test]
async fn promote_is_alias_reassignment() {
    let gw = test_gw();
    let prod = gw.deploy_full("/nonexistent".into(), Manifest { project: "app".into(), ..Default::default() }, "you".into(), Some(git("main", "ccccccc")), true, fluid_core::DeployState::Ready, "personal".into());
    let prev = gw.deploy_full("/nonexistent".into(), Manifest { project: "app".into(), ..Default::default() }, "you".into(), Some(git("hotfix", "ddddddd")), false, fluid_core::DeployState::Ready, "personal".into());
    let (prod_id, prev_id) = (prod.id.to_string(), prev.id.to_string());
    assert_eq!(gw.host_deployment_id("app.localhost").as_deref(), Some(prod_id.as_str()));

    // Promote the preview → it becomes production; same deployment, no rebuild.
    let promoted = gw.promote(&prev_id).expect("promote");
    assert_eq!(promoted.id.to_string(), prev_id);
    assert_eq!(gw.host_deployment_id("app.localhost").as_deref(), Some(prev_id.as_str()), "prod domain re-points on promote");
    // The promoted deployment keeps its own commit + branch URLs.
    assert_eq!(gw.host_deployment_id("app-ddddddd.localhost").as_deref(), Some(prev_id.as_str()));
    assert_eq!(gw.host_deployment_id("app-git-hotfix.localhost").as_deref(), Some(prev_id.as_str()));
}

/// The build `target` (environment) is IMMUTABLE: a production build that gets
/// superseded by a newer production deploy keeps target="production" even though
/// it is no longer the promoted deployment (Vercel separates env from promotion).
#[tokio::test]
async fn target_environment_is_immutable_across_promotion() {
    let gw = test_gw();
    let old = gw.deploy_full("/nonexistent".into(), Manifest { project: "app".into(), ..Default::default() }, "you".into(), Some(git("main", "eeeeeee")), true, fluid_core::DeployState::Ready, "personal".into());
    let _new = gw.deploy_full("/nonexistent".into(), Manifest { project: "app".into(), ..Default::default() }, "you".into(), Some(git("main", "fffffff")), true, fluid_core::DeployState::Ready, "personal".into());
    let old_id = old.id.to_string();

    let list = gw.list();
    let old_view = list.iter().find(|d| d.id.to_string() == old_id).unwrap();
    assert!(!old_view.production, "superseded build is no longer promoted");
    assert_eq!(old_view.target, "production", "but its build environment stays production");
}
