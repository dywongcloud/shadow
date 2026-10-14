//! Peer health demotion: the ONE chokepoint allowed to withdraw a live node.
//!
//! A health verdict is per-OBSERVER (AGENTS.md), and the verdict that decides traffic is whichever
//! node reads it: the control-plane leader's copy drives client DNS and placement, every node's
//! copy drives its own request routing. So `set_health(id, _, false)` is not a local opinion: on
//! the leader it is a fleet-wide withdrawal of a node that may be perfectly alive.
//!
//! A peer whose gossip `last_seen_ms` is fresh is alive by INDEPENDENT evidence (it is still
//! announcing to us, and `upsert_peer` deliberately refreshes `last_seen_ms` without touching
//! `healthy`), so a failed probe, fetch or dial against it is a local mesh-path artifact (a stale
//! cached addr, a wedged trunk), not a dead peer. Every writer that withdraws a node on a transport
//! failure therefore goes through [`demote`] (or [`demote_exact`]), which consults that evidence
//! first, so one failure on one observer cannot pull a healthy peer out of DNS and placement. Four
//! of the six run on the REQUEST path, where one tenant request landing on a trunk that went stale
//! (as happens after a peer restarts with new socket addrs) would otherwise do exactly that:
//!
//! | site | trigger |
//! |---|---|
//! | `main.rs` health prober (`spawn_health_loop`) | consecutive failed mesh probes |
//! | `main.rs` gossip round | a failed `/v1/nodes` fetch |
//! | `edge.rs` mesh route | `DeadPeerTimeout` on a p2p forward |
//! | `edge.rs` `ws_proxy` | `DeadPeerTimeout` opening a raw trunk |
//! | `raw_proxy.rs` | `DeadPeerTimeout` opening a raw port |
//! | `udp_relay.rs` | `DeadPeerTimeout` opening the UDP leg |
//!
//! What the callers actually want is still delivered without misreporting fleet health: [`demote`]
//! records a NODE-LOCAL, time-boxed routing penalty (the cold mark, [`mark_cold`] / [`is_cold`]) so
//! the caller stops re-paying a connect budget against a trunk that just timed out. Cold is a
//! preference (this node, this window); `healthy` is a fleet-visible verdict. The deployment
//! circuit breaker makes the same split (a broken app opens a circuit; it does not mark the host
//! unhealthy).
//!
//! One stale reading is not enough either: [`demote`] withdraws a peer only once its gossip has
//! been stale at the end of [`DEMOTE_STALE_ROUNDS`] consecutive gossip rounds
//! ([`note_gossip_round`]); before that, and while the gossip loop is not ending rounds at all, it
//! is `Deferred` and only marks the peer cold. [`restore_gossip_alive`] reverses a withdrawal the
//! gossip evidence no longer supports, after `RESTORE_HYSTERESIS_ROUNDS` consecutive fresh probe
//! rounds.

use hive_core::now_ms;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// How fresh a peer's gossiped `last_seen_ms` must be to count as independent proof of liveness.
///
/// Sized against MEASURED delivery, not the nominal announce cadence: gossip delivery is itself
/// lossy, and peers provably up (answering HTTP on their public IP) have shown `last_seen` ages of
/// 11-24 s, so a tighter bound fails to protect exactly the nodes this guard exists for.
///
/// 25 s sits just under `NodeRegistry::nodes()`'s own 30 s staleness drop, the mechanism that
/// ACTUALLY removes a dead node from service: a silent node disappears from the registry entirely
/// and stops being served regardless of this flag. So the practical rule is: still gossiping means
/// still served, gone quiet means aged out. A probe's (or a forward's, or a relay leg's) verdict is
/// retained for the narrow 25-30 s band and for its real purpose, diagnosing mesh reachability,
/// rather than withdrawing live nodes from client DNS, which clients reach directly by public IP
/// and never through the mesh.
pub const GOSSIP_ALIVE_MS: u64 = 25_000;

/// Consecutive gossip rounds that must END with a peer gossip-stale before any transport failure
/// may withdraw it. One stale reading is as often a round-timing artifact as a dead peer: a relayed
/// `last_seen_ms` ages past `GOSSIP_ALIVE_MS` whenever this observer's round (or the relay's) runs
/// long, and the next round refreshes it.
pub const DEMOTE_STALE_ROUNDS: u32 = 2;

/// The active prober's cadence (`HIVE_HEALTH_INTERVAL`, seconds, default 5).
/// One reader for the prober and the gossip-round margin check.
pub fn probe_interval() -> std::time::Duration {
    std::time::Duration::from_secs(env_secs("HIVE_HEALTH_INTERVAL", 5))
}

/// One probe sample's budget on a peer that is not failing
/// (`HIVE_HEALTH_TIMEOUT`, seconds, default 2).
pub fn probe_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(env_secs("HIVE_HEALTH_TIMEOUT", 2))
}

/// How long a probe round waits for its probes: two samples at
/// [`probe_timeout`] plus a second of slack. A failing peer's probe (two
/// samples at the dial-fallback ceiling) runs on past it as a straggler.
pub fn probe_deadline() -> std::time::Duration {
    probe_timeout() * 2 + std::time::Duration::from_secs(1)
}

/// Upper bound on the prober's start-to-start round period.
pub fn probe_period_bound() -> std::time::Duration {
    probe_interval() + probe_deadline()
}

fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

/// cold_key(identity) -> consecutive round ends that found it gossip-stale.
fn stale_rounds() -> &'static RwLock<HashMap<String, u32>> {
    static STALE: OnceLock<RwLock<HashMap<String, u32>>> = OnceLock::new();
    STALE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Called by the gossip loop as each round ends: count, per peer, the
/// consecutive round ends at which its gossip was stale. A fresh peer (and a
/// peer the registry no longer holds) drops back to zero. After a stall
/// (`resumed_after_stall`) the count restarts: a round end from before a
/// freeze and one after it are not consecutive evidence — the freeze itself
/// aged every peer.
pub fn note_gossip_round(registry: &hive_edge::region::NodeRegistry, resumed_after_stall: bool) {
    let verdicts = registry.gossip_freshness(GOSSIP_ALIVE_MS);
    let mut map = stale_rounds().write();
    if resumed_after_stall {
        map.clear();
    }
    let next: HashMap<String, u32> = verdicts
        .into_iter()
        .filter(|(_, fresh)| !fresh)
        .map(|(identity, _)| {
            let key = cold_key(&identity);
            let n = map.get(&key).copied().unwrap_or(0).saturating_add(1);
            (key, n)
        })
        .collect();
    *map = next;
}

/// May a stale peer be withdrawn now? Only after [`DEMOTE_STALE_ROUNDS`]
/// consecutive stale round ends, and never while the gossip loop has stopped
/// ending rounds (before its first round, a whole-runtime freeze, a dead
/// loop): this node's registry is not being refreshed then, so every peer
/// looks stale from it and the answer is HOLD. A peer nobody gossips still
/// leaves DNS and placement through `NodeRegistry::nodes()`'s 30 s cutoff.
fn stale_long_enough(identity: &hive_edge::region::PeerIdentity) -> bool {
    !crate::gossip_round::stalled()
        && stale_rounds()
            .read()
            .get(&cold_key(identity))
            .is_some_and(|n| *n >= DEMOTE_STALE_ROUNDS)
}

/// How long a peer stays locally cold after a transport failure (`HIVE_PEER_COLD_MS`, default 30
/// s). Cold is only a routing hint, and an over-long penalty is itself a way to strand a live node.
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
static DEFERRED_STALE: AtomicU64 = AtomicU64::new(0);
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
    let outcome = registry.demote_if_gossip_stale_gated(
        node,
        expected_endpoint_id,
        observed_last_seen_ms,
        GOSSIP_ALIVE_MS,
        stale_long_enough,
    );
    let (identity, demoted) = match outcome {
        hive_edge::region::PeerDemotion::Missing => {
            UNKNOWN_PEER.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        hive_edge::region::PeerDemotion::GossipAlive(identity) => (identity, false),
        hive_edge::region::PeerDemotion::Deferred(identity) => {
            DEFERRED_STALE.fetch_add(1, Ordering::Relaxed);
            if mark_cold_key(cold_key(&identity)) {
                tracing::info!(
                    node = %identity.name,
                    endpoint_id = identity.endpoint_id.as_deref().unwrap_or("legacy-name-only"),
                    reason,
                    stale_rounds_required = DEMOTE_STALE_ROUNDS,
                    gossip_loop_stalled = crate::gossip_round::stalled(),
                    "peer transport failed and its gossip is stale, but not yet for consecutive \
                     gossip rounds (or this node's gossip loop is not ending rounds) — \
                     withdrawal deferred; marked locally cold"
                );
            }
            return false;
        }
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

/// Consecutive fresh rounds a withdrawn peer must show before `restore_gossip_alive` restores it
/// (the prober calls it once per probe round, `HIVE_HEALTH_INTERVAL`, default 5 s). Restoring on a
/// SINGLE fresh timestamp let a lossy-announce peer oscillate withdrawn↔restored and flap
/// DNS/placement with it; three rounds is about 15 s of sustained evidence at the default cadence.
const RESTORE_HYSTERESIS_ROUNDS: u8 = 3;

static RESTORE_STREAKS: std::sync::OnceLock<
    parking_lot::RwLock<std::collections::HashMap<String, u8>>,
> = std::sync::OnceLock::new();

/// Reverse withdrawals that current gossip evidence no longer supports, gated on
/// `RESTORE_HYSTERESIS_ROUNDS` consecutive rounds of sustained freshness: a peer that goes silent
/// mid-streak starts over, because the streak counts CONSECUTIVE rounds, never cumulative
/// sightings. Health mutation and immutable endpoint capture happen under one registry lock, so
/// callers can close only the trunks that were actually restored; returns those endpoint ids.
pub fn restore_gossip_alive(registry: &hive_edge::region::NodeRegistry) -> Vec<String> {
    let streaks =
        RESTORE_STREAKS.get_or_init(|| parking_lot::RwLock::new(std::collections::HashMap::new()));
    let candidates: std::collections::HashSet<String> = registry
        .gossip_fresh_unhealthy(GOSSIP_ALIVE_MS)
        .into_iter()
        .collect();
    let ready: Vec<String> = {
        let mut w = streaks.write();
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

/// Operator view (node-local, like `/v1/dns/stats`): how often this observer withdrew a peer, how
/// often it declined to, and who is currently cold.
///
/// `refused_gossip_alive` is the number that matters during an incident: a large value means this
/// node's transports are failing against peers that are demonstrably alive, i.e. the fault is
/// local, and each of those would otherwise have been a fleet-visible withdrawal. The counters:
///
/// * `demoted`: peers withdrawn (transport failed AND gossip stale for `DEMOTE_STALE_ROUNDS`
///   rounds);
/// * `refused_gossip_alive`: withdrawals declined because the peer was still gossiping;
/// * `deferred_stale`: stale peers NOT withdrawn because their staleness had not yet lasted
///   `DEMOTE_STALE_ROUNDS` consecutive gossip rounds (or the gossip loop was not ending rounds);
/// * `unknown_peer`: calls naming an id the registry has never held (a bare endpoint id from the
///   gossip round, a seed that never meshed), neither a withdrawal nor a refusal, counted apart so
///   the counters above stay readable;
/// * `cold_marks` and `cold_now`: node-local cold penalties recorded, and the peers currently
///   inside their window;
/// * `restored_gossip_alive`: withdrawals REVERSED because the peer kept gossiping
///   (`restore_gossip_alive`); a steadily climbing value means this node's probe path is failing
///   against demonstrably live peers, the same signal as `refused_gossip_alive` for verdicts
///   already written;
/// * `gossip_rounds`: the gossip loop's own round counters (`gossip_round::stats`).
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
        "demote_stale_rounds": DEMOTE_STALE_ROUNDS,
        "demoted": DEMOTED.load(Ordering::Relaxed),
        "refused_gossip_alive": REFUSED_GOSSIP_ALIVE.load(Ordering::Relaxed),
        "deferred_stale": DEFERRED_STALE.load(Ordering::Relaxed),
        "gossip_rounds": crate::gossip_round::stats(),
        "unknown_peer": UNKNOWN_PEER.load(Ordering::Relaxed),
        "cold_marks": COLD_MARKS.load(Ordering::Relaxed),
        "restored_gossip_alive": RESTORED.load(Ordering::Relaxed),
        "cold_now": cold_now,
    })
}
