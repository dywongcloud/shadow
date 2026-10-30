//! Bulkhead runtime: heavy background computation never runs on the serving runtime.
//!
//! The control-plane leader once froze its whole tokio runtime for minutes at a time. The named
//! stacks were the relational billing mirror (`spawn_relational_mirror_loop` ->
//! `relational::upsert_billing_many` -> the guardian SQL engine reloading a 21k-row table per
//! statement) running on `tokio-rt-worker` threads of the one runtime that also serves every HTTP
//! request, gossip round and timer: two workers at 94 % CPU for minutes, with the service cgroup at
//! its `memory.high` limit, throttled every allocating thread in the process.
//!
//! Work that is allowed to be slow (projections, index walks, backfills) runs here instead: a
//! second multi-thread runtime with its own small worker pool (`HIVE_BULKHEAD_THREADS`, default 2).
//! A pathological pass can at most pin these threads; the serving runtime's workers, drivers and
//! timers are untouched. The guardian store handles are runtime-agnostic (channels and async
//! mutexes), so the SQL engine runs here unchanged, and `tokio::spawn` inside a bulkhead task lands
//! on the bulkhead too (`Handle::current`).
//!
//! The rule: a loop whose per-tick cost grows with the data (rows, keys, tenants) is spawned with
//! [`spawn`], never `tokio::spawn`, and its CPU-heavy step is bounded (chunked, incremental)
//! besides. The runtime stall detector (`runtime_watch`) is the backstop that names whatever still
//! slips through.

use std::future::Future;
use std::sync::OnceLock;

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Same stack as the serving runtime (`WORKER_STACK_BYTES` in `main.rs`): the SQL engine and the
/// index walk compile to large poll frames too.
const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

fn threads() -> usize {
    std::env::var("HIVE_BULKHEAD_THREADS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(2)
}

/// The bulkhead runtime's handle (built on first use).
pub fn handle() -> tokio::runtime::Handle {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads())
            .thread_name("hive-bulkhead")
            .thread_stack_size(WORKER_STACK_BYTES)
            .enable_all()
            .build()
            .expect("build bulkhead tokio runtime")
    })
    .handle()
    .clone()
}

/// Spawn `fut` on the bulkhead runtime.
pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    handle().spawn(fut)
}
