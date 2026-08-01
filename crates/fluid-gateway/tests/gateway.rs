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
            vcpus: 1,
            memory_mib: 128,
            max_concurrency: 5,
            min_instances: 0,
            max_instances: 4,
            idle_ttl_secs: 30,
            max_duration_secs: 300,
            ..Default::default()
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
        axum::serve(listener, fluid_gateway::public_router(gw))
            .await
            .unwrap();
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
            (
                status.as_u16(),
                body.contains("hello from a Fluid function"),
            )
        }));
    }

    let mut ok = 0;
    for h in handles {
        let (status, good_body) = h.await.expect("task panicked");
        assert_eq!(status, 200, "expected 200");
        assert!(good_body, "expected function body");
        ok += 1;
    }
    assert_eq!(
        ok, 100,
        "all 100 concurrent gateway requests should succeed"
    );
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
                vcpus: 1,
                memory_mib: 128,
                max_concurrency: 10,
                min_instances: 1,
                max_instances: 2,
                idle_ttl_secs: 30,
                max_duration_secs: 1, // tiny budget so /api/verylong (5s) trips it
                ..Default::default()
            }],
            routes: vec![Route {
                pattern: "/api".into(),
                target: RouteTarget::Function("api".into()),
            }],
            ..Default::default()
        },
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, fluid_gateway::public_router(gw))
            .await
            .unwrap();
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let _ = client.get(format!("{base}/api/hello")).send().await; // warm

    // Fire a too-long request + a failing request + normal requests concurrently.
    let long = {
        let c = client.clone();
        let b = base.clone();
        tokio::spawn(async move {
            c.get(format!("{b}/api/verylong"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        })
    };
    let boom = {
        let c = client.clone();
        let b = base.clone();
        tokio::spawn(async move {
            c.get(format!("{b}/api/boom"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        })
    };
    // Normal requests must keep succeeding while the above misbehave (isolation).
    let mut normal_ok = 0;
    for _ in 0..10 {
        let r = client
            .get(format!("{base}/api/hello"))
            .send()
            .await
            .unwrap();
        if r.status() == 200
            && r.text()
                .await
                .unwrap_or_default()
                .contains("hello from a Fluid function")
        {
            normal_ok += 1;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let long_status = long.await.unwrap();
    let boom_status = boom.await.unwrap();
    assert_eq!(
        long_status, 504,
        "over-budget request should 504 (max duration)"
    );
    assert!(
        boom_status >= 500,
        "failing handler should error, got {boom_status}"
    );
    assert_eq!(
        normal_ok, 10,
        "normal requests must survive (error isolation)"
    );
}

fn bun() -> Option<String> {
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        let cand = std::path::PathBuf::from(dir).join("bun");
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    let home = std::env::var("HOME").ok()?;
    let cand = std::path::PathBuf::from(home).join(".bun/bin/bun");
    cand.is_file().then(|| cand.to_string_lossy().into_owned())
}

/// waitUntil (background continuation) through the FULL real stack — gateway
/// -> Fluid pool -> mock instance -> multiplexed tunnel -> a genuine Bun
/// process (`examples/bun-basic-api/server.js`'s `/api/bg` handler) — proving
/// this mechanism is exactly as runtime-agnostic as the rest of the pipeline:
/// it is driven entirely by the `x-fluid-wait-until-ms` RESPONSE header
/// (`fluid-gateway/src/lib.rs` parses it off whatever process answered, never
/// caring what language/runtime produced it). The client gets an immediate
/// response, but the instance's lease must stay held (`inflight >= 1`) for
/// roughly the declared window afterward, then drop to 0 once it elapses.
#[tokio::test]
async fn wait_until_lease_held_open_after_response_for_a_bun_function() {
    let Some(bun) = bun() else {
        eprintln!("skipping: bun not found");
        return;
    };
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("gw-bun-wu-{}", std::process::id())),
        provision_latency: Duration::from_millis(10),
        cache_root: std::env::temp_dir().join(format!("gw-bun-wu-cache-{}", std::process::id())),
    }));
    let fluid = Fluid::start(backend, FluidConfig::default());
    let fluid_for_stats = fluid.clone();
    let gw = Gateway::new(fluid, "default".into());

    let dir = format!(
        "{}/../../examples/bun-basic-api",
        env!("CARGO_MANIFEST_DIR")
    );
    let dep = gw.deploy(
        dir,
        Manifest {
            project: "bun-basic-api".into(),
            static_dir: None,
            functions: vec![FunctionConfig {
                name: "api".into(),
                runtime: "bun".into(),
                start_cmd: vec![bun, "server.js".into()],
                env: BTreeMap::new(),
                vcpus: 1,
                memory_mib: 128,
                max_concurrency: 5,
                min_instances: 0,
                max_instances: 2,
                idle_ttl_secs: 30,
                max_duration_secs: 300,
                ..Default::default()
            }],
            routes: vec![Route {
                pattern: "/api".into(),
                target: RouteTarget::Function("api".into()),
            }],
            ..Default::default()
        },
    );
    let key = fluid_compute::func_key(dep.id.as_str(), "api");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, fluid_gateway::public_router(gw))
            .await
            .unwrap();
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Cold-start + confirm the response itself is correct and immediate.
    let resp = client
        .get(format!("{base}/api/bg"))
        .send()
        .await
        .expect("request must succeed");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap_or_default();
    assert!(body.contains("bg via waitUntil"), "unexpected body: {body}");

    // Immediately after the response, the lease must still be held (the
    // gateway is asleep in the background waitUntil window, not yet released).
    let inflight_right_after = fluid_for_stats
        .stats()
        .into_iter()
        .find(|s| s.key == key)
        .map(|s| s.inflight)
        .unwrap_or(0);
    assert!(
        inflight_right_after >= 1,
        "lease should still be held immediately after responding (waitUntil window active), got inflight={inflight_right_after}"
    );

    // After the declared window (600ms) plus margin, it must have been released.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let inflight_after_window = fluid_for_stats
        .stats()
        .into_iter()
        .find(|s| s.key == key)
        .map(|s| s.inflight)
        .unwrap_or(0);
    assert_eq!(
        inflight_after_window, 0,
        "lease must be released once the waitUntil window elapses"
    );
}

/// #17 context-leak runtime test (full stack): many concurrent requests forced
/// onto ONE instance (max_instances=1, high max_concurrency) each carry a unique
/// path, `x-ctx` header, and body. Every response must reflect EXACTLY its own
/// request — and all must report the SAME pid (proving they truly shared one
/// in-instance-concurrent instance, so the isolation claim is meaningful).
#[tokio::test]
async fn no_context_leak_across_concurrent_requests_on_one_instance() {
    let Some(py) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("gw3-{}", std::process::id())),
        provision_latency: Duration::from_millis(10),
        cache_root: std::env::temp_dir().join(format!("gw3-cache-{}", std::process::id())),
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
                vcpus: 1,
                memory_mib: 128,
                max_concurrency: 64, // many requests share the instance
                min_instances: 1,
                max_instances: 1, // FORCE a single instance -> real in-instance concurrency
                idle_ttl_secs: 30,
                max_duration_secs: 300,
                ..Default::default()
            }],
            routes: vec![Route {
                pattern: "/api".into(),
                target: RouteTarget::Function("api".into()),
            }],
            ..Default::default()
        },
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, fluid_gateway::public_router(gw))
            .await
            .unwrap();
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let _ = client.get(format!("{base}/api/hello")).send().await; // warm the single instance

    let mut handles = Vec::new();
    for i in 0..80u32 {
        let client = client.clone();
        let base = base.clone();
        handles.push(tokio::spawn(async move {
            let path = format!("/api/echo/{i}");
            let ctx = format!("ctx-{i:08x}");
            let body = format!("payload-{i}-").repeat((i % 29 + 1) as usize);
            let resp = tokio::time::timeout(
                Duration::from_secs(25),
                client
                    .post(format!("{base}{path}"))
                    .header("x-ctx", &ctx)
                    .body(body.clone())
                    .send(),
            )
            .await
            .expect("timed out")
            .expect("send failed");
            assert_eq!(resp.status().as_u16(), 200);
            // Response header must carry THIS request's ctx.
            let hdr_ctx = resp
                .headers()
                .get("x-ctx")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let v: serde_json::Value = resp.json().await.expect("function must return JSON");
            assert_eq!(hdr_ctx, ctx, "x-ctx response header leaked");
            assert_eq!(v["ctx"], ctx, "echoed ctx leaked across requests");
            assert_eq!(v["path"], path, "echoed path leaked across requests");
            assert_eq!(
                v["body"], body,
                "echoed body leaked/corrupted across requests"
            );
            v["pid"].as_i64().unwrap_or(-1)
        }));
    }
    let mut pids = std::collections::BTreeSet::new();
    let mut done = 0;
    for h in handles {
        pids.insert(h.await.expect("task panicked"));
        done += 1;
    }
    assert_eq!(done, 80);
    // All requests must have been served by the SAME single instance — otherwise
    // the isolation we just proved wouldn't be exercising in-instance concurrency.
    assert_eq!(
        pids.len(),
        1,
        "expected one shared instance pid, got {pids:?}"
    );
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
        manifest: Manifest {
            project: "ctr-demo".into(),
            ..Default::default()
        },
        created_at_ms: created,
        creator: "you".into(),
        git: None,
        production,
        target: if production {
            "production".into()
        } else {
            "preview".into()
        },
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
    assert_eq!(
        gw.host_deployment_id("dpl-old.localhost").as_deref(),
        Some("dpl-old")
    );
    assert_eq!(
        gw.host_deployment_id("dpl-new.localhost").as_deref(),
        Some("dpl-new")
    );
}

/// #9 in-flight version pinning (structural lock): promoting a new version must
/// NOT disturb the previously-deployed version — it stays resolvable by its own
/// immutable commit/branch URLs, and its function pool key is DISTINCT from the
/// new version's. Together with handle_public cloning the `Deployment` once at
/// request entry (so an in-flight request holds an immutable snapshot), this
/// guarantees a request that started on v1 keeps serving from v1's own instance
/// pool even across a mid-flight promotion. This test fails loudly if a refactor
/// ever makes versions share a pool key or makes promotion mutate the old version.
#[tokio::test]
async fn promotion_does_not_repin_or_merge_prior_version_pool() {
    let gw = test_gw();
    let v1 = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("main", "1111111")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let v2 = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("main", "2222222")),
        false,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let (v1_id, v2_id) = (v1.id.to_string(), v2.id.to_string());

    // Production alias starts on v1.
    assert_eq!(
        gw.host_deployment_id("app.localhost").as_deref(),
        Some(v1_id.as_str())
    );

    // Promote v2 -> production alias re-points.
    gw.promote(&v2_id).expect("promote");
    assert_eq!(
        gw.host_deployment_id("app.localhost").as_deref(),
        Some(v2_id.as_str())
    );

    // v1 is NOT gone: its immutable commit URL still resolves to v1 — an in-flight
    // request that resolved v1 (or a direct hit on its commit URL) still gets v1.
    assert_eq!(
        gw.host_deployment_id("app-1111111.localhost").as_deref(),
        Some(v1_id.as_str()),
        "prior version must remain resolvable by its own immutable URL (in-flight pinning)"
    );

    // The two versions occupy DISTINCT function pools (keyed by deployment id), so
    // v1's warm instances never serve v2's requests or get merged on promotion.
    let k1 = fluid_compute::func_key(&v1_id, "api");
    let k2 = fluid_compute::func_key(&v2_id, "api");
    assert_ne!(k1, k2, "each version must have its own instance pool key");
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
    let m = Manifest {
        project: "app".into(),
        ..Default::default()
    };
    let info = gw.deploy_full(
        "/nonexistent".into(),
        m,
        "you".into(),
        Some(git("main", "abc1234def")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let id = info.id.to_string();

    assert_eq!(
        gw.host_deployment_id("app.localhost").as_deref(),
        Some(id.as_str()),
        "production domain"
    );
    assert_eq!(
        gw.host_deployment_id("app-abc1234.localhost").as_deref(),
        Some(id.as_str()),
        "commit URL (short sha)"
    );
    assert_eq!(
        gw.host_deployment_id("app-git-main.localhost").as_deref(),
        Some(id.as_str()),
        "branch URL"
    );
    assert_eq!(
        gw.host_deployment_id(&format!("{id}.localhost")).as_deref(),
        Some(id.as_str()),
        "per-deployment id URL"
    );
}

/// A PREVIEW deploy (non-production branch) must get its own commit + branch URLs
/// but must NOT steal the production domain from the live production deployment.
#[tokio::test]
async fn preview_deploy_does_not_hijack_production_domain() {
    let gw = test_gw();
    let prod = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("main", "aaaaaaa")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let prod_id = prod.id.to_string();
    let prev = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("feature/login", "bbbbbbb")),
        false,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let prev_id = prev.id.to_string();

    // Production domain stays on the production deployment.
    assert_eq!(
        gw.host_deployment_id("app.localhost").as_deref(),
        Some(prod_id.as_str()),
        "preview must not hijack prod"
    );
    // Each has its own immutable + branch URLs (branch name slugified).
    assert_eq!(
        gw.host_deployment_id("app-git-main.localhost").as_deref(),
        Some(prod_id.as_str())
    );
    assert_eq!(
        gw.host_deployment_id("app-git-feature-login.localhost")
            .as_deref(),
        Some(prev_id.as_str())
    );
    assert_eq!(
        gw.host_deployment_id("app-bbbbbbb.localhost").as_deref(),
        Some(prev_id.as_str())
    );
}

/// The branch URL must always track the LATEST deployment on that branch (a new
/// commit on `main` re-points `app-git-main` while old commit URLs stay pinned).
#[tokio::test]
async fn branch_url_tracks_latest_commit_on_branch() {
    let gw = test_gw();
    let d1 = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("main", "1111111")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let d2 = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("main", "2222222")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let (id1, id2) = (d1.id.to_string(), d2.id.to_string());

    assert_eq!(
        gw.host_deployment_id("app-git-main.localhost").as_deref(),
        Some(id2.as_str()),
        "branch tracks newest"
    );
    assert_eq!(
        gw.host_deployment_id("app-1111111.localhost").as_deref(),
        Some(id1.as_str()),
        "old commit URL stays pinned"
    );
    assert_eq!(
        gw.host_deployment_id("app-2222222.localhost").as_deref(),
        Some(id2.as_str())
    );
    assert_eq!(
        gw.host_deployment_id("app.localhost").as_deref(),
        Some(id2.as_str()),
        "prod domain follows newest production"
    );
}

/// Promotion / rollback is pure alias reassignment — promoting an older preview
/// makes it production WITHOUT a rebuild, re-pointing the production domain.
#[tokio::test]
async fn promote_is_alias_reassignment() {
    let gw = test_gw();
    let prod = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("main", "ccccccc")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let prev = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("hotfix", "ddddddd")),
        false,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let (prod_id, prev_id) = (prod.id.to_string(), prev.id.to_string());
    assert_eq!(
        gw.host_deployment_id("app.localhost").as_deref(),
        Some(prod_id.as_str())
    );

    // Promote the preview → it becomes production; same deployment, no rebuild.
    let promoted = gw.promote(&prev_id).expect("promote");
    assert_eq!(promoted.id.to_string(), prev_id);
    assert_eq!(
        gw.host_deployment_id("app.localhost").as_deref(),
        Some(prev_id.as_str()),
        "prod domain re-points on promote"
    );
    // The promoted deployment keeps its own commit + branch URLs.
    assert_eq!(
        gw.host_deployment_id("app-ddddddd.localhost").as_deref(),
        Some(prev_id.as_str())
    );
    assert_eq!(
        gw.host_deployment_id("app-git-hotfix.localhost").as_deref(),
        Some(prev_id.as_str())
    );
}

/// The build `target` (environment) is IMMUTABLE: a production build that gets
/// superseded by a newer production deploy keeps target="production" even though
/// it is no longer the promoted deployment (Vercel separates env from promotion).
#[tokio::test]
async fn target_environment_is_immutable_across_promotion() {
    let gw = test_gw();
    let old = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("main", "eeeeeee")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let _new = gw.deploy_full(
        "/nonexistent".into(),
        Manifest {
            project: "app".into(),
            ..Default::default()
        },
        "you".into(),
        Some(git("main", "fffffff")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let old_id = old.id.to_string();

    let list = gw.list();
    let old_view = list.iter().find(|d| d.id.to_string() == old_id).unwrap();
    assert!(
        !old_view.production,
        "superseded build is no longer promoted"
    );
    assert_eq!(
        old_view.target, "production",
        "but its build environment stays production"
    );
}

/// Regression: the preview gate must key on a deployment's ACTUAL target, so
/// `deployment_for_host` has to resolve the right deployment (and its target) for
/// EVERY alias type — production domain, commit URL, and id URL. A production
/// deployment reached via its commit/id URL must still report target=production
/// (else the gate would wrongly preview-protect it); a preview reports preview.
#[tokio::test]
async fn deployment_for_host_resolves_target_for_every_alias() {
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("gw-target-{}", std::process::id())),
        provision_latency: Duration::from_millis(1),
        cache_root: std::env::temp_dir().join(format!("gw-target-cache-{}", std::process::id())),
    }));
    let gw = Gateway::new(
        Fluid::start(backend, FluidConfig::default()),
        "default".into(),
    );
    let m = || Manifest {
        project: "app".into(),
        ..Default::default()
    };

    // A production deploy (claims app.localhost) + a later preview deploy.
    let prod = gw.deploy_full(
        "/nonexistent".into(),
        m(),
        "you".into(),
        Some(git("main", "aaaaaaa")),
        true,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );
    let prev = gw.deploy_full(
        "/nonexistent".into(),
        m(),
        "you".into(),
        Some(git("feature", "bbbbbbb")),
        false,
        fluid_core::DeployState::Ready,
        "personal".into(),
    );

    let target = |host: &str| gw.deployment_for_host(host).expect("resolves").target;

    // Production: open via ALL of its URLs.
    assert_eq!(target("app.localhost"), "production", "production domain");
    assert_eq!(
        target(&prod.commit_alias),
        "production",
        "production via commit URL"
    );
    assert_eq!(
        target(&prod.id_alias),
        "production",
        "production via id URL"
    );

    // Preview: gated — its URLs must report preview, never production.
    assert_eq!(
        target(&prev.commit_alias),
        "preview",
        "preview via commit URL"
    );
    assert_eq!(target(&prev.id_alias), "preview", "preview via id URL");
}
