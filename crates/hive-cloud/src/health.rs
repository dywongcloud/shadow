//! Peer health demotion — the ONE chokepoint allowed to withdraw a live node.
//!
//! A health verdict is per-OBSERVER (AGENTS.md), and the verdict that decides
//! traffic is whichever node happens to be reading it: the control-plane
//! leader's copy drives client DNS and placement, every node's copy drives its
//! own request routing. So `set_health(id, _, false)` is not a local opinion —
//! on the leader it is a fleet-wide withdrawal of a node that may be perfectly
//! alive.
//!
//! `spawn_health_loop`'s prober already learned this the expensive way and
//! carries the guard: a peer whose gossip `last_seen_ms` is fresh is alive by
//! INDEPENDENT evidence (it is still announcing to us, and `upsert_peer`
//! deliberately refreshes `last_seen_ms` without touching `healthy`), so a
//! probe failing against it is a local mesh-path artifact — a stale cached
//! addr, a wedged trunk — not a dead peer. Witnessed: a leader marked 10 of 17
//! live nodes unhealthy and withheld them from DNS/placement until it was
//! restarted, while two other vantages saw 0 and 1 unhealthy.
//!
//! That guard lived inside the prober, so **five other writers withdrew nodes
//! with no guard and no threshold at all**, each on a SINGLE failure:
//!
//! | site | trigger |
//! |---|---|
//! | `main.rs` gossip round | one failed `/v1/nodes` fetch |
//! | `edge.rs` mesh route | one `DeadPeerTimeout` on a p2p forward |
//! | `edge.rs` `ws_proxy` | one `DeadPeerTimeout` opening a raw trunk |
//! | `raw_proxy.rs` | one `DeadPeerTimeout` opening a raw port |
//! | `udp_relay.rs` | one `DeadPeerTimeout` opening the UDP leg |
//!
//! Four of those five run on the REQUEST path, so one tenant request landing
//! on a trunk that went stale (exactly what the mesh docs say happens after a
//! peer restarts with new socket addrs) could pull a healthy peer out of DNS
//! and placement on that observer, with no counter-evidence consulted and no
//! second opinion. That is the class of bug this module closes: every writer
//! now goes through [`demote`], which consults the gossip liveness evidence
//! first.
//!
//! What the callers actually wanted is still delivered, just not by lying
//! about fleet health: [`mark_cold`] records a NODE-LOCAL, time-boxed routing
//! penalty so the caller stops re-paying a connect budget against a trunk that
//! just timed out. Cold is a preference (this node, this window); `healthy` is
//! a fleet-visible verdict. Keeping them separate is the whole fix — the same
//! decomposition the deployment circuit breaker uses (a broken app opens a
//! circuit; it does not mark the host unhealthy).

use hive_core::now_ms;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// How fresh a peer's gossiped `last_seen_ms` must be to count as independent
/// proof of liveness.
///
/// Sized against MEASURED behaviour, not the nominal announce cadence. Gossip
/// announces every 3-4s, but on the live fleet delivery is itself lossy: peers
/// that were provably up (answering HTTP on their public IP) showed last_seen
/// ages of 11-24s. A tighter bound (12s was tried first) therefore fails to
/// protect exactly the nodes this guard exists for, which is how a majority of
/// the fleet stayed grey after the first attempt.
///
/// 25s sits just under `NodeRegistry::nodes()`'s own 30s staleness drop, which
/// is the mechanism that ACTUALLY removes a dead node from service — a silent
/// node disappears from the registry entirely and stops being served regardless
/// of this flag. So the practical rule becomes: still gossiping ⇒ still served;
/// gone quiet ⇒ aged out. A probe's (or a forward's, or a relay leg's) verdict
/// is retained for the narrow 25-30s band and, more importantly, for its real
/// purpose — diagnosing mesh reachability — rather than silently withdrawing
/// live nodes from client DNS, which clients reach directly by public IP and
/// never through the mesh.
pub const GOSSIP_ALIVE_MS: u64 = 25_000;

/// How long a peer stays locally cold after a transport failure. Deliberately
/// SHORTER than the health-probe interval's recovery path: it is a routing
/// hint, and an over-long penalty is itself a way to strand a live node.
fn cold_window_ms() -> u64 {
    std::env::var("HIVE_PEER_COLD_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(30_000)
}

/// immutable peer identity -> epoch-ms the local routing penalty expires.
fn cold() -> &'static RwLock<HashMap<String, u64>> {
    static COLD: OnceLock<RwLock<HashMap<String, u64>>> = OnceLock::new();
    COLD.get_or_init(|| RwLock::new(HashMap::new()))
}

fn cold_key(identity: &hive_edge::region::PeerIdentity) -> String {
    identity
        .endpoint_id
        .as_ref()
        .map(|id| format!("endpoint:{id}"))
        .unwrap_or_else(|| format!("name:{}", identity.name))
}

fn endpoint_cold_key(endpoint_id: &str) -> String {
    format!("endpoint:{endpoint_id}")
}

static DEMOTED: AtomicU64 = AtomicU64::new(0);
static REFUSED_GOSSIP_ALIVE: AtomicU64 = AtomicU64::new(0);
static UNKNOWN_PEER: AtomicU64 = AtomicU64::new(0);
static COLD_MARKS: AtomicU64 = AtomicU64::new(0);
static RESTORED: AtomicU64 = AtomicU64::new(0);

fn mark_cold_key(key: String) -> bool {
    let now = now_ms();
    let until = now.saturating_add(cold_window_ms());
    let mut map = cold().write();
    map.retain(|_, exp| *exp > now);
    COLD_MARKS.fetch_add(1, Ordering::Relaxed);
    map.insert(key, until).is_none()
}

/// Record a node-local routing penalty against the peer's current immutable
/// endpoint identity. Unknown peers cannot create an unresolvable penalty.
pub fn mark_cold(registry: &hive_edge::region::NodeRegistry, node: &str) -> bool {
    registry
        .peer_identity(node)
        .is_some_and(|identity| mark_cold_key(cold_key(&identity)))
}

/// Is the peer's current immutable identity inside its local routing penalty
/// window? A replacement reusing the same name never inherits the old key.
pub fn is_cold(registry: &hive_edge::region::NodeRegistry, node: &str) -> bool {
    let Some(identity) = registry.peer_identity(node) else {
        return false;
    };
    cold()
        .read()
        .get(&cold_key(&identity))
        .is_some_and(|exp| *exp > now_ms())
}

/// Clear a penalty by exact immutable endpoint after a fenced direct success.
pub fn clear_cold_identity(endpoint_id: &str) {
    let key = endpoint_cold_key(endpoint_id);
    if !cold().read().contains_key(&key) {
        return;
    }
    cold().write().remove(&key);
}

/// Clear the current peer's penalty when only a name-or-endpoint label is
/// available.
pub fn clear_cold(registry: &hive_edge::region::NodeRegistry, node: &str) {
    let Some(identity) = registry.peer_identity(node) else {
        return;
    };
    let key = cold_key(&identity);
    if !cold().read().contains_key(&key) {
        return;
    }
    cold().write().remove(&key);
}

fn demote_inner(
    registry: &hive_edge::region::NodeRegistry,
    node: &str,
    expected_endpoint_id: Option<&str>,
    reason: &str,
    observed_last_seen_ms: Option<u64>,
) -> bool {
    let outcome = registry.demote_if_gossip_stale(
        node,
        expected_endpoint_id,
        observed_last_seen_ms,
        GOSSIP_ALIVE_MS,
    );
    let (identity, demoted) = match outcome {
        hive_edge::region::PeerDemotion::Missing => {
            UNKNOWN_PEER.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        hive_edge::region::PeerDemotion::GossipAlive(identity) => (identity, false),
        hive_edge::region::PeerDemotion::Demoted(identity) => (identity, true),
    };
    let newly_cold = mark_cold_key(cold_key(&identity));
    if !demoted {
        REFUSED_GOSSIP_ALIVE.fetch_add(1, Ordering::Relaxed);
        if newly_cold {
            tracing::warn!(
                node = %identity.name,
                endpoint_id = identity.endpoint_id.as_deref().unwrap_or("legacy-name-only"),
                reason,
                "peer transport failed but the peer is still gossiping — keeping it HEALTHY \
                 (a single-observer transport fault must not withdraw a live node from \
                 DNS/placement); marked locally cold instead"
            );
        }
        return false;
    }
    DEMOTED.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        node = %identity.name,
        endpoint_id = identity.endpoint_id.as_deref().unwrap_or("legacy-name-only"),
        reason,
        "peer marked UNHEALTHY (transport failed AND gossip stale) — it leaves DNS and placement"
    );
    true
}

/// Withdraw the peer currently denoted by a name or endpoint id. Current
/// registry evidence is re-read atomically by `NodeRegistry` before mutation.
pub fn demote(
    registry: &hive_edge::region::NodeRegistry,
    node: &str,
    reason: &str,
    observed_last_seen_ms: Option<u64>,
) -> bool {
    demote_inner(registry, node, None, reason, observed_last_seen_ms)
}

/// Withdraw only if `node` still maps to the immutable endpoint captured with
/// the failed operation.
pub fn demote_exact(
    registry: &hive_edge::region::NodeRegistry,
    node: &str,
    expected_endpoint_id: &str,
    reason: &str,
    observed_last_seen_ms: Option<u64>,
) -> bool {
    demote_inner(
        registry,
        node,
        Some(expected_endpoint_id),
        reason,
        observed_last_seen_ms,
    )
}

/// Consecutive fresh rounds a withdrawn peer must show before it is
/// restored — the restore-side hysteresis (refutation finding F5): demotion
/// needs threshold probe misses AND >25s gossip silence, but restoration
/// previously needed a SINGLE fresh timestamp, so a lossy-announce peer
/// oscillated withdrawn↔restored and DNS/placement flapped with it. Three
/// rounds at the 5s cadence is ~15s of sustained evidence — still far
/// faster than the probe-driven demotion path.
const RESTORE_HYSTERESIS_ROUNDS: u8 = 3;

static RESTORE_STREAKS: std::sync::OnceLock<
    parking_lot::RwLock<std::collections::HashMap<String, u8>>,
> = std::sync::OnceLock::new();

/// Reverse withdrawals that current gossip evidence no longer supports —
/// gated on RESTORE_HYSTERESIS_ROUNDS of sustained freshness (F5). Health
/// mutation and immutable endpoint capture happen under one registry lock,
/// so callers can close only the trunks that were actually restored.
pub fn restore_gossip_alive(registry: &hive_edge::region::NodeRegistry) -> Vec<String> {
    let streaks = RESTORE_STREAKS
        .get_or_init(|| parking_lot::RwLock::new(std::collections::HashMap::new()));
    let candidates: std::collections::HashSet<String> = registry
        .gossip_fresh_unhealthy(GOSSIP_ALIVE_MS)
        .into_iter()
        .collect();
    let ready: Vec<String> = {
        let mut w = streaks.write();
        // A peer that went silent mid-streak starts over — the streak must
        // be CONSECUTIVE rounds, not cumulative sightings.
        w.retain(|id, _| candidates.contains(id));
        let mut ready = Vec::new();
        for id in &candidates {
            let n = w.entry(id.clone()).or_insert(0);
            *n = n.saturating_add(1);
            if *n >= RESTORE_HYSTERESIS_ROUNDS {
                ready.push(id.clone());
            }
        }
        for id in &ready {
            w.remove(id);
        }
        ready
    };
    if ready.is_empty() {
        return Vec::new();
    }
    let restored = registry.restore_only(&ready);
    if !restored.is_empty() {
        RESTORED.fetch_add(restored.len() as u64, Ordering::Relaxed);
        for endpoint_id in &restored {
            tracing::info!(
                endpoint_id,
                "peer RESTORED to healthy — sustained gossip liveness past the hysteresis, so the \
                 withdrawal that removed it from DNS/placement is no longer supported by evidence"
            );
        }
    }
    restored
}

/// Operator view (node-local, like `/v1/dns/stats`): how often this observer
/// withdrew a peer, how often it declined to, and who is currently cold.
///
/// `refused_gossip_alive` is the number that matters during an incident: a
/// large value means this node's transports are failing against peers that are
/// demonstrably alive — i.e. the fault is local, and before this guard existed
/// every one of those would have been a fleet-visible withdrawal.
pub fn stats() -> serde_json::Value {
    let now = now_ms();
    let cold_now: Vec<serde_json::Value> = cold()
        .read()
        .iter()
        .filter(|(_, exp)| **exp > now)
        .map(|(node, exp)| serde_json::json!({ "node": node, "expires_in_ms": exp - now }))
        .collect();
    serde_json::json!({
        "gossip_alive_ms": GOSSIP_ALIVE_MS,
        "cold_window_ms": cold_window_ms(),
        "demoted": DEMOTED.load(Ordering::Relaxed),
        "refused_gossip_alive": REFUSED_GOSSIP_ALIVE.load(Ordering::Relaxed),
        // Calls naming an id the registry has never held (a bare endpoint id
        // from the gossip round, a seed that never meshed). Neither a
        // withdrawal nor a refusal — reported so the two real counters above
        // stay readable.
        "unknown_peer": UNKNOWN_PEER.load(Ordering::Relaxed),
        "cold_marks": COLD_MARKS.load(Ordering::Relaxed),
        // Withdrawals REVERSED because the peer kept gossiping (see
        // `restore_gossip_alive`). A steadily climbing value means this node's
        // probe path is failing against demonstrably-live peers — the same
        // signal as `refused_gossip_alive`, for verdicts already written.
        "restored_gossip_alive": RESTORED.load(Ordering::Relaxed),
        "cold_now": cold_now,
    })
}
