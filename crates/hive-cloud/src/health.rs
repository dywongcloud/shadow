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

/// node id -> epoch-ms the local routing penalty expires.
fn cold() -> &'static RwLock<HashMap<String, u64>> {
    static COLD: OnceLock<RwLock<HashMap<String, u64>>> = OnceLock::new();
    COLD.get_or_init(|| RwLock::new(HashMap::new()))
}

static DEMOTED: AtomicU64 = AtomicU64::new(0);
static REFUSED_GOSSIP_ALIVE: AtomicU64 = AtomicU64::new(0);
static UNKNOWN_PEER: AtomicU64 = AtomicU64::new(0);
static COLD_MARKS: AtomicU64 = AtomicU64::new(0);
static RESTORED: AtomicU64 = AtomicU64::new(0);

/// Record the node-local routing penalty. Returns `true` when the peer was not
/// already cold — the caller uses that to log once per window instead of once
/// per request, so a hot loop cannot turn a failing peer into a log flood.
pub fn mark_cold(node: &str) -> bool {
    let now = now_ms();
    let until = now.saturating_add(cold_window_ms());
    let mut map = cold().write();
    // Opportunistic expiry — the map is bounded by the peer count, but a peer
    // that leaves the fleet should not linger in it forever either.
    map.retain(|_, exp| *exp > now);
    COLD_MARKS.fetch_add(1, Ordering::Relaxed);
    map.insert(node.to_string(), until).is_none()
}

/// Is this peer inside its local routing penalty window? Node-local and
/// advisory: it may only DEPRIORITIZE a candidate, never remove the last one.
pub fn is_cold(node: &str) -> bool {
    cold().read().get(node).is_some_and(|exp| *exp > now_ms())
}

/// Clear the penalty — call after any SUCCESSFUL exchange with the peer, so a
/// recovered trunk is preferred again immediately rather than serving out a
/// window it no longer deserves.
pub fn clear_cold(node: &str) {
    if !cold().read().contains_key(node) {
        return; // read-only fast path: the common case takes no write lock
    }
    cold().write().remove(node);
}

/// The single writer allowed to mark a peer unhealthy.
///
/// `observed_last_seen_ms` lets a caller pass the liveness timestamp from the
/// SAME snapshot it made its failing observation against (the prober does
/// this deliberately, so its liveness check can never disagree with the set it
/// actually probed); `None` reads the registry now.
///
/// Returns `true` when the withdrawal was applied. `false` means the peer is
/// still gossiping and was kept in service — the caller's failure is real, but
/// it is a local transport fault, and it is recorded as a cold trunk instead.
pub fn demote(
    registry: &hive_edge::region::NodeRegistry,
    node: &str,
    reason: &str,
    observed_last_seen_ms: Option<u64>,
) -> bool {
    // Not a registry peer at all — there is no health to withdraw and no route
    // to deprioritize, so this is neither a demotion nor a refusal.
    //
    // Witnessed live and not hypothetical: the gossip round addresses iroh
    // peers by 64-hex ENDPOINT ID while the registry is keyed by node NAME, so
    // every failed round against an unmeshed seed called this with an id no
    // peer map has ever held. `set_health` is already a no-op for an unknown
    // id, but counting those as demotions made a healthy node's counters read
    // `demoted: 5` in its first 30 seconds — an operator metric that cries
    // wolf is the same failure as no metric at all, which is the whole reason
    // this endpoint exists.
    let known = registry.peer_last_seen_ms(node);
    if known.is_none() && observed_last_seen_ms.is_none() {
        UNKNOWN_PEER.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let newly_cold = mark_cold(node);
    let last_seen = observed_last_seen_ms.or(known);
    let alive_by_gossip = last_seen.is_some_and(|ms| now_ms().saturating_sub(ms) < GOSSIP_ALIVE_MS);
    if alive_by_gossip {
        REFUSED_GOSSIP_ALIVE.fetch_add(1, Ordering::Relaxed);
        if newly_cold {
            tracing::warn!(
                node,
                reason,
                "peer transport failed but the peer is still gossiping — keeping it HEALTHY \
                 (a single-observer transport fault must not withdraw a live node from \
                 DNS/placement); marked locally cold instead"
            );
        }
        return false;
    }
    DEMOTED.fetch_add(1, Ordering::Relaxed);
    // u64::MAX, not 0: an unhealthy peer that still gets ranked anywhere should
    // sort last, and 0 (the old prober value) sorts FIRST on a latency key.
    registry.set_health(node, u64::MAX, false);
    tracing::warn!(
        node,
        reason,
        "peer marked UNHEALTHY (transport failed AND gossip stale) — it leaves DNS and placement"
    );
    true
}

/// Reverse withdrawals that the gossip evidence no longer supports.
///
/// [`demote`] refuses to withdraw a peer whose gossip is fresh, but until this
/// existed nothing ever UNDID a withdrawal: only a successful probe could clear
/// `healthy = false`, so a peer whose probe path was broken (stale cached addr,
/// wedged trunk) while its announces kept arriving stayed unhealthy for the life
/// of the process. Measured live on nine nodes simultaneously:
/// `audible_peers = 15` beside `visible_healthy_peers = 6` — a node that had
/// heard from 15 peers seconds earlier serving 6 to DNS and placement. Hand
/// restarts "fixed" it only because a restart drops the stale verdict.
///
/// Applying the same evidence symmetrically is what makes this self-healing, and
/// it cannot resurrect a genuinely dead peer: a silent node's `last_seen_ms`
/// ages past the window and `NodeRegistry::nodes()` drops it outright at 30s.
///
/// Returns the ids actually restored this pass.
///
/// The IDS, not a count: the caller's follow-up (dropping those peers' wedged
/// trunks) must act on exactly the peers this pass restored. Re-deriving that
/// set by testing `latency_ms == RESTORED_LATENCY_MS` conflates the sentinel
/// with a genuine measurement — `set_health` stores the raw probe RTT and this
/// fleet really does see ~1s cross-continent probes (AGENTS.md records a
/// successful 7462ms one), so a correctly-probed peer that happened to measure
/// exactly 999ms would have its healthy trunk closed. It also mis-fires on a
/// peer whose `NodeInfo` was relayed from an observer that had restored it,
/// since `upsert_peer` adopts an unknown peer's healthy/latency verbatim.
pub fn restore_gossip_alive(registry: &hive_edge::region::NodeRegistry) -> Vec<String> {
    let stale_ids: Vec<String> = registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && !n.healthy)
        .map(|n| n.id)
        .collect();
    let mut restored: Vec<String> = Vec::new();
    for id in stale_ids {
        if registry.restore_health_if_gossip_fresh(&id, GOSSIP_ALIVE_MS) {
            RESTORED.fetch_add(1, Ordering::Relaxed);
            // A restored peer is reachable-by-evidence, so it must not keep
            // serving out a routing penalty from the transport fault that
            // withdrew it.
            clear_cold(&id);
            tracing::info!(
                node = %id,
                "peer RESTORED to healthy — it is still gossiping, so the withdrawal that \
                 removed it from DNS/placement is no longer supported by evidence"
            );
            restored.push(id);
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
