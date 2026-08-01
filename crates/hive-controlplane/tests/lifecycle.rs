//! End-to-end-ish tests of the control plane driving the mock backend.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use hive_backend::mock::{MockBackend, MockConfig};
use hive_controlplane::{BoxConfig, Hive, HiveConfig};
use hive_core::{BuildJob, JobId, JobState, ResourceSpec};

/// Every test in this file calls `test_hive` independently, and Rust's default
/// test harness runs them CONCURRENTLY as separate threads within the SAME OS
/// process — so keying the mock backend's storage dirs by `process::id()` alone
/// gave every test in the file the identical directory, regardless of which test
/// called it. Concurrent tests then raced on the same on-disk cell/cache state
/// (`capacity_is_released_after_builds`, e.g., could see leftover cells another
/// concurrently-running test hadn't torn down yet, spuriously failing the
/// "capacity fully released" assertion). A process-wide atomic counter makes
/// each call's directory unique regardless of concurrency, at zero added
/// dependency cost.
fn unique_test_id() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    (std::process::id() as u64) << 32 | n
}

fn test_hive(warm: BTreeMap<String, usize>, provision_ms: u64) -> Arc<Hive> {
    let uid = unique_test_id();
    let cfg = HiveConfig {
        hive_id: "hive-test".into(),
        boxes: vec![BoxConfig {
            vcpus: 8,
            mem_mib: 8192,
        }],
        warm_targets: warm,
        default_warm_target: 0,
        warm_spec: ResourceSpec::default(),
        warm_idle_ttl: Duration::from_secs(60),
        max_concurrent_builds: 4,
        autoscaler_interval: Duration::from_millis(50),
    };
    let backend = Arc::new(MockBackend::new(MockConfig {
        root: std::env::temp_dir().join(format!("hive-test-{uid}")),
        provision_latency: Duration::from_millis(provision_ms),
        cache_root: std::env::temp_dir().join(format!("hive-test-cache-{uid}")),
    }));
    Hive::start(cfg, backend)
}

async fn wait_terminal(hive: &Hive, id: &JobId) -> JobState {
    for _ in 0..400 {
        if let Some(v) = hive.job_view(id) {
            if v.state.is_terminal() {
                return v.state;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job {id} did not reach a terminal state");
}

/// Wait until every box has fully released its capacity (vcpus + cells back to 0).
/// Capacity release is eventual-consistent: it lags a job's terminal-state
/// transition by a scheduler tick, so a snapshot taken the instant `wait_terminal`
/// returns can still see not-yet-released vcpus. Poll (same bound as
/// `wait_terminal`) until it drains rather than asserting on that racy snapshot —
/// a genuine leak still fails, but deterministically (the bounded panic), not
/// flakily. Returns once drained.
async fn wait_capacity_drained(hive: &Hive) {
    for _ in 0..400 {
        let status = hive.cluster_status();
        if status
            .boxes
            .iter()
            .all(|b| b.vcpus_used == 0 && b.cells == 0)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let status = hive.cluster_status();
    let leaked: Vec<_> = status
        .boxes
        .iter()
        .filter(|b| b.vcpus_used != 0 || b.cells != 0)
        .map(|b| format!("{}(vcpus_used={}, cells={})", b.id, b.vcpus_used, b.cells))
        .collect();
    panic!("box capacity did not fully release: {}", leaked.join(", "));
}

#[tokio::test]
async fn cold_build_succeeds() {
    let hive = test_hive(BTreeMap::new(), 20);
    let job = BuildJob::builder("img").command("exit 0").build();
    let id = hive.submit(job);
    assert_eq!(wait_terminal(&hive, &id).await, JobState::Succeeded);
}

#[tokio::test]
async fn failing_command_marks_job_failed() {
    let hive = test_hive(BTreeMap::new(), 5);
    let job = BuildJob::builder("img")
        .command("echo step1")
        .command("exit 3")
        .command("echo never-runs")
        .build();
    let id = hive.submit(job);
    assert_eq!(wait_terminal(&hive, &id).await, JobState::Failed);
    let v = hive.job_view(&id).unwrap();
    assert_eq!(v.exit_code, Some(3));
}

#[tokio::test]
async fn warm_pool_beats_cold_latency() {
    let mut warm = BTreeMap::new();
    warm.insert("hot".to_string(), 1usize);
    let hive = test_hive(warm, 300); // expensive cold provision

    // Let the autoscaler fill the warm pool.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let warm_job = BuildJob::builder("hot").command("exit 0").build();
    let wid = hive.submit(warm_job);
    assert_eq!(wait_terminal(&hive, &wid).await, JobState::Succeeded);
    let warm_latency = hive.job_view(&wid).unwrap().provision_latency_ms.unwrap();

    let cold_job = BuildJob::builder("cold-image").command("exit 0").build();
    let cid = hive.submit(cold_job);
    assert_eq!(wait_terminal(&hive, &cid).await, JobState::Succeeded);
    let cold_latency = hive.job_view(&cid).unwrap().provision_latency_ms.unwrap();

    // The whole point of the warm pool: a warm hit is dramatically faster.
    assert!(
        warm_latency + 100 < cold_latency,
        "warm={warm_latency}ms should be well under cold={cold_latency}ms"
    );
}

#[tokio::test]
async fn build_cache_restores_between_builds() {
    let hive = test_hive(BTreeMap::new(), 5);

    // Build 1 creates node_modules and (on success) saves it to the cache.
    let j1 = BuildJob::builder("img")
        .command("mkdir -p node_modules && echo lib > node_modules/dep.txt")
        .cache("lockhash-abc", vec!["node_modules".to_string()])
        .build();
    let id1 = hive.submit(j1);
    assert_eq!(wait_terminal(&hive, &id1).await, JobState::Succeeded);

    // Build 2 runs in a FRESH single-use cell, so node_modules exists only if it
    // was restored from cache. The command exits 0 iff the restore worked.
    let j2 = BuildJob::builder("img")
        .command("test -f node_modules/dep.txt")
        .cache("lockhash-abc", vec!["node_modules".to_string()])
        .build();
    let id2 = hive.submit(j2);
    assert_eq!(
        wait_terminal(&hive, &id2).await,
        JobState::Succeeded,
        "second build should restore node_modules from cache"
    );

    // A different cache key must NOT restore it.
    let j3 = BuildJob::builder("img")
        .command("test -f node_modules/dep.txt")
        .cache("different-key", vec!["node_modules".to_string()])
        .build();
    let id3 = hive.submit(j3);
    assert_eq!(
        wait_terminal(&hive, &id3).await,
        JobState::Failed,
        "a different cache key should be a miss"
    );
}

#[tokio::test]
async fn capacity_is_released_after_builds() {
    let hive = test_hive(BTreeMap::new(), 5);
    // Box has 8 vcpus; submit 6 two-vcpu jobs -> must serialize but all finish.
    let mut ids = Vec::new();
    for _ in 0..6 {
        let job = BuildJob::builder("img")
            .command("exit 0")
            .resources(ResourceSpec {
                vcpus: 2,
                mem_mib: 1024,
                disk_mib: 1024,
                timeout_secs: 60,
            })
            .build();
        ids.push(hive.submit(job));
    }
    for id in &ids {
        assert_eq!(wait_terminal(&hive, id).await, JobState::Succeeded);
    }
    // Capacity release lags the terminal transition by a scheduler tick — wait for
    // it to drain rather than asserting on a racy immediate snapshot (the old
    // source of this test's "leaked vcpus" flakiness).
    wait_capacity_drained(&hive).await;
    let status = hive.cluster_status();
    for b in status.boxes {
        assert_eq!(b.vcpus_used, 0, "box {} leaked vcpus", b.id);
        assert_eq!(b.cells, 0, "box {} leaked cells", b.id);
    }
}
