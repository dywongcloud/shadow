//! Speculative prewarm: start the function a request is about to invoke while the request's TLS
//! handshake is still finishing.
//!
//! A cold start (litebox ~3 s, container seconds) is the whole latency a first request pays. The
//! ClientHello already tells us the host, so instead of discovering the cold pool after the
//! handshake we start it during the handshake: `acme::SniResolver::resolve` hands the SNI to a
//! bounded channel, and this worker walks the hints off the request path (on the bulkhead runtime,
//! never on a serving tokio worker).
//!
//! Two properties are load-bearing:
//!
//! * **Never block, never unbounded.** Hints are dropped when the queue is full and deduped per
//! host, and warming goes through `Fluid::warm`, which refuses without side effects when the pool
//! is already warm, in backoff, behind an open crash-loop circuit, saturated or shutting down. A
//! wrong guess costs one idle instance that scale-to-zero then drains, never a permanent one and
//! never a request.
//! * **Warm where the deployment IS, not where the handshake landed.** Public hosts are round-robin
//! DNS, so the entry node is frequently not the owner. A hint for a host this node does not serve
//! is forwarded to a healthy peer that does, which hides the mesh hop too: that hop is the larger
//! half of the latency when the entry node is on another continent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::state::CloudState;

/// Hints buffered before new ones are dropped. Bounded: a burst of handshakes
/// must never grow memory or queue cold starts faster than they complete.
const QUEUE_MAX: usize = 256;

/// Concurrent warm attempts. Small: a cold start is seconds long, and warming
/// faster than instances can start only wastes cells.
const MAX_INFLIGHT: usize = 4;

/// Ignore a repeat hint for the same host inside this window — a browser opens
/// several connections for one page load and they all carry the same SNI.
const DEDUPE_MS: u64 = 30_000;

/// Cap on the dedupe map (hosts, not bytes). Old entries are dropped on write.
const DEDUPE_MAX: usize = 4096;

static TX: OnceLock<tokio::sync::mpsc::Sender<String>> = OnceLock::new();

static HINTS: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static DUPES: AtomicU64 = AtomicU64::new(0);
static FORWARDED: AtomicU64 = AtomicU64::new(0);

/// Outcome counts since the last summary line, plus when that line was emitted. The hint path runs
/// on every handshake, so it logs at most one summary per [`SUMMARY_MS`] instead of one line per
/// hint.
static OUTCOMES: Mutex<Option<(u64, [(u64, &str); 10])>> = Mutex::new(None);
const SUMMARY_MS: u64 = 10_000;

/// Records what a hint did. `Started` is logged at once (it is rare and it is the outcome the
/// feature exists for); everything else is counted and emitted by the rate-limited summary.
pub(crate) fn record(host: &str, outcome: &str) {
    let now = hive_core::now_ms();
    let emit = {
        let mut slot = match OUTCOMES.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        let (since, counts) = slot.get_or_insert((now, [(0, "started"); 10]));
        *counts = [
            (counts[0].0, "started"),
            (counts[1].0, "already-warm"),
            (counts[2].0, "refused"),
            (counts[3].0, "failed"),
            (counts[4].0, "not-served"),
            (counts[5].0, "not-ready"),
            (counts[6].0, "static"),
            (counts[7].0, "ambiguous"),
            (counts[8].0, "forwarded"),
            (counts[9].0, "forward-failed"),
        ];
        if let Some(c) = counts.iter_mut().find(|(_, n)| *n == outcome) {
            c.0 += 1;
        }
        if now.saturating_sub(*since) >= SUMMARY_MS {
            let line = counts
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, name)| format!("{name}={n}"))
                .collect::<Vec<_>>()
                .join(" ");
            *since = now;
            *counts = [(0, "started"); 10];
            Some(line)
        } else {
            None
        }
    };
    if outcome == "started" {
        tracing::info!(host = %host, "prewarm: started an instance ahead of the request");
    }
    if let Some(line) = emit {
        tracing::info!(
            hints = HINTS.load(Ordering::Relaxed),
            dropped = DROPPED.load(Ordering::Relaxed),
            dupes = DUPES.load(Ordering::Relaxed),
            forwarded = FORWARDED.load(Ordering::Relaxed),
            "prewarm: outcomes {line}"
        );
    }
}

/// Last hint time per host, for dedupe.
static LAST: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn last_map() -> &'static Mutex<HashMap<String, u64>> {
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a hint and say whether it is new enough to act on.
fn should_warm(host: &str) -> bool {
    let now = hive_core::now_ms();
    let mut m = match last_map().lock() {
        Ok(m) => m,
        Err(e) => e.into_inner(),
    };
    if m.len() >= DEDUPE_MAX {
        m.retain(|_, t| now.saturating_sub(*t) < DEDUPE_MS);
        if m.len() >= DEDUPE_MAX {
            m.clear();
        }
    }
    match m.get(host) {
        Some(t) if now.saturating_sub(*t) < DEDUPE_MS => false,
        _ => {
            m.insert(host.to_string(), now);
            true
        }
    }
}

/// Hands a hostname observed in a ClientHello to the prewarm worker. Called synchronously from the
/// TLS SNI resolver, so it never blocks and never waits: a full queue drops the hint. Empty,
/// over-long (more than 253 bytes) and dot-less names cannot name a served deployment and never
/// reach the queue.
pub fn hint(host: &str) {
    if host.is_empty() || host.len() > 253 || !host.contains('.') {
        return;
    }
    let Some(tx) = TX.get() else { return };
    match tx.try_send(host.to_ascii_lowercase()) {
        Ok(()) => {
            let n = HINTS.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 200 == 0 {
                tracing::info!(
                    hints = n,
                    dropped = DROPPED.load(Ordering::Relaxed),
                    dupes = DUPES.load(Ordering::Relaxed),
                    forwarded = FORWARDED.load(Ordering::Relaxed),
                    "prewarm: hint census"
                );
            }
        }
        Err(_) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `HIVE_PREWARM=0` (or `false`) disables prewarming entirely.
///
/// A disabled node leaves the SNI hook uninstalled, so the old behavior is one env var away on a
/// single node (an A/B measurement, or a quick retreat if a hint ever costs more than it saves).
fn enabled() -> bool {
    std::env::var("HIVE_PREWARM")
        .ok()
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

/// Install the SNI hook and start the worker. Called once at boot, after the
/// gateway exists.
pub fn spawn(cloud: Arc<CloudState>) {
    if !enabled() {
        tracing::info!("prewarm: disabled by HIVE_PREWARM=0");
        return;
    }
    let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_MAX);
    if TX.set(tx).is_err() {
        return;
    }
    crate::acme::install_warm_hook(Arc::new(|host: &str| hint(host)));
    crate::bulkhead::spawn(worker(cloud, rx));
}

async fn worker(cloud: Arc<CloudState>, mut rx: tokio::sync::mpsc::Receiver<String>) {
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT));
    while let Some(host) = rx.recv().await {
        if !should_warm(&host) {
            DUPES.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let permit = match Arc::clone(&sem).acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };
        let c = cloud.clone();
        crate::bulkhead::spawn(async move {
            let _permit = permit;
            handle(&c, &host).await;
        });
    }
}

/// Warms `host` here when this node serves it; otherwise forwards the hint to a healthy peer that
/// does, using the request path's owner selection (the full-host key first, since custom domains
/// are keyed by their whole hostname, then the first label). The forward is fire and forget (a hint
/// is an optimization, never a dependency, and the owner applies its own gates on arrival) and is
/// recorded like any other outcome: a forward is the common case (most deployments live on one node
/// while public DNS spreads handshakes across the fleet), so a silent majority path would be
/// unobservable.
async fn handle(cloud: &Arc<CloudState>, host: &str) {
    if cloud.gw.serves_host(host) {
        let outcome = cloud.gw.warm_host(host).await;
        record(host, outcome.as_str());
        return;
    }
    let healthy_ids: std::collections::HashSet<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.healthy)
        .map(|n| n.id)
        .collect();
    let host_key = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let sub = host_key.split('.').next().unwrap_or(&host_key).to_string();
    let owner = {
        let routes = cloud.peer_routes.read();
        routes
            .get(&host_key)
            .into_iter()
            .chain(routes.get(&sub))
            .flatten()
            .find(|r| r.healthy && healthy_ids.contains(&r.node_id))
            .map(|r| r.node_id.clone())
    };
    let Some(node_id) = owner else { return };
    let addr = cloud
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.id == node_id && !n.is_self)
        .and_then(|n| n.iroh_addr.clone());
    let Some(addr) = addr else { return };
    let body = serde_json::json!({ "host": host }).to_string();
    FORWARDED.fetch_add(1, Ordering::Relaxed);
    let sent = crate::gossip::request_to(
        cloud,
        &node_id,
        &addr,
        hive_p2p::GOSSIP_POST,
        "/v1/warm-hint",
        body.as_bytes(),
        3,
    )
    .await
    .is_some();
    record(host, if sent { "forwarded" } else { "forward-failed" });
}

/// Handles a hint that arrived over the mesh (see the `/v1/warm-hint` gossip arm). This node was
/// selected as a host that serves the deployment, so it warms locally and never re-forwards.
pub fn handle_remote(cloud: Arc<CloudState>, host: String) {
    if !should_warm(&host) {
        DUPES.fetch_add(1, Ordering::Relaxed);
        return;
    }
    crate::bulkhead::spawn(async move {
        let outcome = cloud.gw.warm_host(&host).await;
        record(&host, outcome.as_str());
    });
}
