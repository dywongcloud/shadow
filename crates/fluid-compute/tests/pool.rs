//! Tests the Fluid pool semantics against the mock backend, using a tiny
//! Python HTTP server as the "function". Skips if python3 is unavailable.

use std::sync::Arc;
use std::time::Duration;

use fluid_compute::{func_key, Fluid, FluidConfig};
use fluid_core::FunctionConfig;
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

fn test_fluid(lease_timeout_ms: u64) -> Arc<Fluid> {
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("fluid-test-{}", std::process::id())),
        provision_latency: Duration::from_millis(10),
        cache_root: std::env::temp_dir().join(format!("fluid-test-cache-{}", std::process::id())),
    }));
    Fluid::start(
        backend,
        FluidConfig {
            autoscaler_interval: Duration::from_millis(50),
            lease_timeout: Duration::from_millis(lease_timeout_ms),
            max_instances_per_tenant: 0,
        },
    )
}

fn func(max_concurrency: u32, max_instances: u32, min_instances: u32, py: &str) -> FunctionConfig {
    FunctionConfig {
        name: "api".into(),
        runtime: "python".into(),
        // A server that binds $PORT and serves anything.
        start_cmd: vec![
            py.into(),
            "-c".into(),
            "import os,http.server,socketserver;\
             socketserver.TCPServer(('127.0.0.1',int(os.environ['PORT'])),\
             http.server.SimpleHTTPRequestHandler).serve_forever()"
                .into(),
        ],
        env: Default::default(),
        memory_mib: 128,
        max_concurrency,
        min_instances,
        max_instances,
        idle_ttl_secs: 1,
        max_duration_secs: 300,
    }
}

#[tokio::test]
async fn concurrency_reuse_autoscale_and_saturation() {
    let Some(py) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let fluid = test_fluid(400);
    let key = func_key("dpl-test", "api");
    fluid.register(key.clone(), func(2, 2, 0, &py), "default".into(), ".".into(), "personal".into());

    // Two leases pack onto ONE instance (in-function concurrency = 2).
    let l1 = fluid.lease(&key).await.expect("lease 1");
    let l2 = fluid.lease(&key).await.expect("lease 2");
    assert_eq!(l1.cell_id(), l2.cell_id(), "2 reqs should reuse 1 instance");
    assert_eq!(fluid.stats()[0].instances, 1);
    assert_eq!(fluid.stats()[0].inflight, 2);

    // Third lease saturates instance 1 -> cold-start instance 2.
    let l3 = fluid.lease(&key).await.expect("lease 3");
    assert_ne!(l3.cell_id(), l1.cell_id(), "3rd req should be a new instance");
    assert_eq!(fluid.stats()[0].instances, 2);

    // Fourth fills instance 2; now both instances are full at max_instances=2.
    let _l4 = fluid.lease(&key).await.expect("lease 4");
    assert_eq!(fluid.stats()[0].inflight, 4);

    // Fifth must fail: fully saturated, can't exceed max_instances.
    let l5 = fluid.lease(&key).await;
    assert!(l5.is_err(), "should be saturated");

    // Release one slot; a new lease now succeeds (reuse).
    drop(l1);
    let l6 = fluid.lease(&key).await.expect("lease after release");
    assert_eq!(fluid.stats()[0].inflight, 4);
    drop((l2, l3, _l4, l6));
}

#[tokio::test]
async fn cost_meter_shows_savings_under_concurrency() {
    let Some(py) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let fluid = test_fluid(3000);
    let key = func_key("dpl-cost", "api");
    // One instance, up to 5 concurrent requests.
    fluid.register(key.clone(), func(5, 1, 0, &py), "default".into(), ".".into(), "personal".into());

    // 5 concurrent requests all packed onto a single instance.
    let mut leases = Vec::new();
    for _ in 0..5 {
        leases.push(fluid.lease(&key).await.expect("lease"));
    }
    assert_eq!(fluid.stats()[0].instances, 1, "all 5 should share 1 instance");

    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(leases); // each records ~250ms of active time

    let s = &fluid.stats()[0];
    // Traditional 1:1 serverless would bill ~5x the instance-time Fluid bills.
    assert!(
        s.traditional_ms > s.fluid_ms,
        "traditional {} should exceed fluid {}",
        s.traditional_ms,
        s.fluid_ms
    );
    assert!(
        s.savings_pct > 0.4,
        "expected meaningful savings, got {:.2}",
        s.savings_pct
    );
}

#[tokio::test]
async fn mark_dead_reaps_and_reroutes() {
    let Some(py) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let fluid = test_fluid(2000);
    let key = func_key("dpl-dead", "api");
    fluid.register(key.clone(), func(2, 3, 0, &py), "default".into(), ".".into(), "personal".into());

    let l1 = fluid.lease(&key).await.expect("lease 1");
    let cid1 = l1.cell_id().clone();
    assert_eq!(fluid.stats()[0].instances, 1);
    drop(l1);

    // Simulate the router/health-check declaring the instance dead (nack).
    fluid.mark_dead(&key, &cid1).await;
    let s = fluid.stats()[0].clone();
    assert_eq!(s.instances, 0, "dead instance removed");
    assert_eq!(s.dead_reaped, 1);

    // The next request reroutes by cold-starting a brand-new instance.
    let l2 = fluid.lease(&key).await.expect("lease after reroute");
    assert_ne!(l2.cell_id(), &cid1, "should be a different instance");
    assert_eq!(fluid.stats()[0].instances, 1);
}

#[tokio::test]
async fn scales_to_zero_when_idle() {
    let Some(py) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let fluid = test_fluid(400);
    let key = func_key("dpl-test2", "api");
    // min_instances = 0 -> can scale fully to zero.
    fluid.register(key.clone(), func(4, 3, 0, &py), "default".into(), ".".into(), "personal".into());

    let l = fluid.lease(&key).await.expect("lease");
    assert_eq!(fluid.stats()[0].instances, 1);
    drop(l); // idle now

    // idle_ttl is 1s; wait for the autoscaler to reap it.
    let mut zeroed = false;
    for _ in 0..40 {
        if fluid.stats()[0].instances == 0 {
            zeroed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(zeroed, "idle instance should scale to zero");
}

// ---- Multi-tenancy --------------------------------------------------------

/// A pool records its owning tenant (normalized; empty => "personal"), and the
/// tenant surfaces in stats. No backend traffic — pure registration.
#[tokio::test]
async fn register_tags_pool_tenant_and_normalizes_empty() {
    let fluid = test_fluid(100);
    let ka = func_key("dpl-acme", "api");
    fluid.register(ka.clone(), func(1, 1, 0, "python"), "img".into(), ".".into(), "acme".into());
    assert_eq!(fluid.tenant_of(&ka).as_deref(), Some("acme"));

    let kp = func_key("dpl-blank", "api");
    fluid.register(kp.clone(), func(1, 1, 0, "python"), "img".into(), ".".into(), String::new());
    assert_eq!(fluid.tenant_of(&kp).as_deref(), Some("personal"), "empty tenant => personal");

    assert!(fluid.stats().iter().any(|s| s.key == ka && s.tenant == "acme"));
}

/// A per-tenant instance quota caps ONE tenant's total live instances across all
/// its function pools — and never starves another tenant (backpressure, not
/// cross-tenant eviction). Boots real mock cells, so skips without python3.
#[tokio::test]
async fn per_tenant_instance_quota_is_isolated() {
    let Some(py) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("fluid-quota-{}", std::process::id())),
        provision_latency: Duration::from_millis(10),
        cache_root: std::env::temp_dir().join(format!("fluid-quota-cache-{}", std::process::id())),
    }));
    let fluid = Fluid::start(
        backend,
        FluidConfig {
            autoscaler_interval: Duration::from_millis(50),
            lease_timeout: Duration::from_millis(300),
            max_instances_per_tenant: 2, // alpha may hold at most 2 live instances
        },
    );
    // concurrency=1 so each held lease pins its own instance.
    let ka = func_key("dpl-alpha", "api");
    fluid.register(ka.clone(), func(1, 10, 0, &py), "default".into(), ".".into(), "alpha".into());
    let kb = func_key("dpl-beta", "api");
    fluid.register(kb.clone(), func(1, 10, 0, &py), "default".into(), ".".into(), "beta".into());

    // alpha brings up its 2 allowed instances and holds them.
    let _l1 = fluid.lease(&ka).await.expect("alpha lease 1");
    let _l2 = fluid.lease(&ka).await.expect("alpha lease 2");
    assert_eq!(fluid.tenant_live_instances("alpha"), 2, "alpha should hold 2 live instances");

    // A 3rd alpha instance is over quota -> refused (backpressure -> timeout).
    assert!(
        fluid.lease(&ka).await.is_err(),
        "alpha exceeded its per-tenant instance quota but a 3rd instance was allowed"
    );

    // beta has its OWN budget and must not be starved by alpha's saturation.
    let lb = fluid.lease(&kb).await;
    assert!(lb.is_ok(), "beta was starved by alpha's quota (cross-tenant interference)");
    assert_eq!(fluid.tenant_live_instances("beta"), 1);
}
