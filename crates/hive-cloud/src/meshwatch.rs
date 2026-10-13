//! Mesh-isolation watchdog: self-restarts a node whose mesh subsystem has wedged, instead of
//! leaving it dark until a human notices.
//!
//! ## The failure this exists for
//!
//! A node can keep its process alive, its systemd unit `active` and its HTTP surfaces answering
//! while its iroh mesh layer is functionally dead: `/v1/mesh` reports `isolated: true` with zero
//! visible healthy peers (every expected peer marked UNHEALTHY by this node's own prober), a relay
//! reconnect storm runs underneath, no anti-entropy, gossip or roster event happens at all, and it
//! never recovers on its own. When that node heads `HIVE_CP_OWNER_CHAIN`, every leader-only job
//! holds fleet-wide (`leadership::may_act` refuses an isolated owner) and deploys stall. That is
//! why a liveness probe on the process is not enough: the process is fine, its view of the fleet is
//! not.
//!
//! ## Why a restart, and why that is not a cop-out here
//!
//! The wedge lives inside the iroh endpoint's own transport/relay actors, below anything this crate
//! drives; there is no supported in-process 'rebuild the endpoint' call to reach for, and the
//! measured recovery for every occurrence has been exactly one thing: restart the process, after
//! which the node rejoins within ~20-40 s. Converting a permanent, human-paged outage into a
//! ~30-second automatic blip is a real improvement even though it does not fix iroh; when the
//! underlying transport bug is fixed this watchdog simply stops firing. `memwatch`'s RSS
//! self-restart is the same trade and the same shared controlled-restart path (`exit(17)` under the
//! unit's `Restart=always`), deliberately reused rather than reinvented.
//!
//! ## The triggers
//!
//! Peers are counted by DIRECT reachability (healthy and not observer-cold), never by service
//! eligibility alone: gossip restoration makes a live peer `healthy` again for DNS and placement
//! but keeps its cold mark until a direct exchange succeeds, so the self-healer cannot erase its
//! own wedge signal. Four triggers feed the same controlled restart. The continuous, cumulative and
//! establishment triggers add the node's [`node_stagger_ms`] to their thresholds.
//!
//! 1. Continuous isolation ([`should_restart`]): no directly reachable peer while peers are
//!    expected, for `HIVE_MESH_WEDGE_SECS` (default 600 s; the streak resets the instant a directly
//!    reachable peer reappears, so churn, a peer restart or a slow probe cycle never trips it). It
//!    needs `expected_peers > 0` (a standalone node is never restarted) and that this node has
//!    reached at least one peer since boot, the load-bearing guard that separates 'the mesh worked
//!    and then broke' (the wedge) from 'this node never had connectivity' (a firewall, a bad seed
//!    list, a real partition: none of which a restart fixes and all of which a restart loop would
//!    make worse), so a node that has never converged is left alone and loud.
//!    `HIVE_MESH_WEDGE_RESTART=0` keeps the WARN but disables the restart, for an operator
//!    debugging a wedged node.
//! 2. Cumulative degradation ([`cumulative_should_restart`]), for the flapping case the continuous
//!    streak misses: degraded time summed over `HIVE_MESH_DEGRADED_WINDOW_SECS` (default 1800)
//!    reaching `HIVE_MESH_DEGRADED_TRIGGER_SECS` (default 1200); `HIVE_MESH_DEGRADED_RESTART=0`
//!    disables the restart.
//! 3. Boot wedge: a node whose transport wedged BEFORE its first successful exchange never arms the
//!    two triggers above and would stay dark while reporting healthy. It is told apart from a
//!    firewalled node by AUDIBILITY: hearing the fleet (audible peers at or above the floor) while
//!    never having reached anyone directly, 15 minutes past boot, is a LOCAL wedge a restart heals,
//!    whereas a firewalled node hears about nobody and stays alone-and-loud. It fires at most once
//!    per process and honours `HIVE_MESH_WEDGE_RESTART`.
//! 4. The establishment wedge, below.
//!
//! ## The establishment wedge (independent of peer counts)
//!
//! Every trigger above counts peers, and a wedge that blocks only NEW connections never moves those
//! counts: warm trunks keep gossip, probes and the visible-peer numbers up, and `ever_converged`
//! needs more than a quarter of the expected peers, which a fleet with few live servers and many
//! permanently-off dev nodes never reaches. `establishment_wedge` reads the transport's own
//! counters (`hive_p2p::establish_stats`) instead, runs first on every tick (before any early
//! `continue`), and never reads `expected_peers`. It fires when ALL of:
//!
//! 1. evidence that this node's OWN connection setup died ([`establishment_wedge_evidence`]):
//!    either (a) no fresh connection established in either direction for
//!    `HIVE_MESH_ESTABLISH_WEDGE_SECS` (default 300) while dials from here to at least two distinct
//!    directly heard peers TIMED OUT in that window, or (b) at least 3 accepts closed at the accept
//!    deadline with zero successful accepts, corroborated outbound (dials attempted, none
//!    established, at least one timed out). Stuck accepts alone only WARN, because any remote
//!    sender can keep a handshake alive past the accept deadline;
//! 2. an uptime of at least 10 minutes;
//! 3. at least one audible peer, any identity whose gossip is fresh in the registry and not only
//!    rostered ones (the fleet is alive; this node's connection setup is what died);
//! 4. the evidence has held for this node's stagger (0-10 minutes);
//! 5. the shared gates below.
//!
//! `HIVE_MESH_ESTABLISH_WEDGE_RESTART=0` turns the restart into a WARN.
//!
//! ## Shared gates
//!
//! Every automatic restart passes `ControlledRestart::request` and so `RestartReason::admissible`.
//! The cell-orphan interlock (`hive_backend::orphan_reap_ran()`: the reaper confirmed this boot
//! clean; true on a host with no podman at all and always false on macOS, whose Apple `container`
//! cells are not reaped, because each restart without it adds one more writer per stateful tenant
//! volume) holds back every trigger here. The reason's rate cap applies to the establishment wedge
//! only, one restart per 6 h counted from `restart_history.json`, so it survives the restarts it
//! limits and fails CLOSED when that history cannot be trusted; the isolation and degradation
//! triggers re-arm only after the NEW process converged, so their own guards space them. A refused
//! request WARNs and the loop keeps watching.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::state::CloudState;

/// How long a node must be CONTINUOUSLY isolated before it is considered
/// wedged rather than merely reconverging.
fn wedge_secs() -> u64 {
    std::env::var("HIVE_MESH_WEDGE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(600)
}

/// Is the self-restart arm enabled? (`0`/`false` = observe + warn only.)
fn restart_enabled() -> bool {
    !matches!(
        std::env::var("HIVE_MESH_WEDGE_RESTART")
            .unwrap_or_default()
            .trim(),
        "0" | "false" | "no"
    )
}

/// Pure decision core of the continuous-isolation trigger: the guard conjunction, isolated from the
/// loop so the firing condition is readable in one place. `isolated_for_ms` is how long the current
/// isolation streak has run (0 = not isolated). Returns true only when peers are expected, this
/// node has reached a peer since boot, and the streak has lasted `wedge_ms`.
pub fn should_restart(
    expected_peers: usize,
    ever_saw_peer: bool,
    isolated_for_ms: u64,
    wedge_ms: u64,
) -> bool {
    expected_peers > 0 && ever_saw_peer && isolated_for_ms >= wedge_ms
}

/// The severe-degradation floor: seeing fewer than a QUARTER of the expected fleet (min 1) is the
/// measured wedge shape. A healthy node holds well above that, since several expected ids are
/// permanently-off dev nodes, so the floor stays far below normal variance; tiny fleets degrade the
/// floor to 1, where the zero-visible case is the continuous-isolation trigger's job anyway.
pub fn degraded_floor(expected_peers: usize) -> usize {
    (expected_peers / 4).max(1)
}

/// Cumulative-degradation restart decision, the fix for the flapping blind spot: the continuous
/// trigger resets whenever isolation clears for one tick, so a node whose isolation clears for 30 s
/// every few minutes stays effectively dark forever. This trigger sums DEGRADED time (visible <
/// [`degraded_floor`], one 30 s sample per tick) over a sliding window, and the node must also be
/// degraded RIGHT NOW (a node that genuinely recovered is never restarted for its history). Cadence
/// self-limits to one restart per `trigger_ms` while genuinely stuck.
///
/// Three guards keep it from firing synchronized restarts on states a restart cannot fix:
/// `ever_converged` (loop-local: was ONCE above the floor, so a node that never converged, e.g. a
/// firewall misconfiguration or a minority partition since boot, stays alone-and-loud instead of
/// kill-looping); `audible_peers >= floor` (the fleet must be gossip-AUDIBLE while our probes fail,
/// a LOCAL transport wedge that a restart heals; survivors of a mass outage hear almost nobody, and
/// restarting the last remaining capacity every 20 minutes is the opposite of degraded operation);
/// and the per-node stagger ([`node_stagger_ms`]) the caller adds, so a shared onset never fires
/// every node, all three control-plane leaders included, in one 30 s tick.
pub fn cumulative_should_restart(
    expected_peers: usize,
    ever_converged: bool,
    degraded_now: bool,
    audible_peers: usize,
    degraded_ms_in_window: u64,
    trigger_ms: u64,
) -> bool {
    expected_peers > 0
        && ever_converged
        && degraded_now
        && audible_peers >= degraded_floor(expected_peers)
        && degraded_ms_in_window >= trigger_ms
}

/// Deterministic per-node stagger added to the restart triggers' thresholds: FNV-1a over the node
/// name, spread across 0..10 minutes. Identity-derived (never shared wall time), so nodes crossing
/// a threshold in the same tick still exit minutes apart: the first restart usually heals the wedge
/// for the rest, and a synchronized fleet-wide bounce (the deploy playbook's own forbidden state)
/// is structurally impossible.
pub fn node_stagger_ms(node_name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in node_name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h % 600_000
}

/// Window over which the establishment-wedge trigger judges 'no new connection in either
/// direction'. Also the window `/v1/mesh` reports the establishment counters over, so the page
/// shows what the trigger sees.
pub fn establish_wedge_secs() -> u64 {
    std::env::var("HIVE_MESH_ESTABLISH_WEDGE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(300)
}

fn establish_restart_enabled() -> bool {
    !matches!(
        std::env::var("HIVE_MESH_ESTABLISH_WEDGE_RESTART")
            .unwrap_or_default()
            .trim(),
        "0" | "false" | "no" | "off"
    )
}

/// Minimum process uptime before the establishment trigger may fire.
const ESTABLISH_MIN_UPTIME_MS: u64 = 10 * 60 * 1000;
/// Accepts closed at the deadline (with none succeeding) that count as
/// inbound evidence.
const ESTABLISH_STUCK_MIN: u64 = 3;
/// Distinct directly-heard peers whose dials TIMED OUT on this side for "no
/// new connection" to count — one is that peer's path, not our endpoint.
const ESTABLISH_FAILING_PEERS_MIN: usize = 2;
/// The outbound corroboration stuck accepts need: at least this many
/// directly-heard peers whose dials timed out on this side.
const ESTABLISH_STUCK_CORROBORATING_PEERS: usize = 1;

/// What condition 1 of the establishment trigger found (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WedgeEvidence {
    /// Restart evidence, named.
    Restart(&'static str),
    /// Accepts are stuck but this node's OWN outbound connection setup is not shown dead: any
    /// remote can hold a handshake open past the accept deadline, so this is WARN-only and never
    /// starts the restart streak.
    InboundOnly,
}

/// Condition 1 of the establishment trigger (module doc): which evidence holds, if any. Pure over
/// the transport's counters so the firing rule is readable in one place. `failing_live_peers`
/// counts only peers that reached this node DIRECTLY in the freshness window and whose latest dial
/// from here ran out its connect budget: never a refusal (the peer answered: our setup works),
/// never a negative-discovery short-circuit (nothing was sent). A peer's own wedge completes our
/// handshakes (its accept is what hangs), so our dials to it succeed or are refused, never time
/// out: each restart arm rests on this node's own timed-out dials, which no remote sender can forge
/// (stuck accepts can be, so alone they are only `InboundOnly`).
pub fn establishment_wedge_evidence(
    es: &hive_p2p::EstablishStats,
    now_ms: u64,
    boot_ms: u64,
    window_ms: u64,
    failing_live_peers: usize,
) -> Option<WedgeEvidence> {
    let inbound_stuck =
        es.accept_stuck_window >= ESTABLISH_STUCK_MIN && es.inbound_established_window == 0;
    let outbound_dead = es.outbound_attempts_window > 0 && es.outbound_established_window == 0;
    if inbound_stuck && outbound_dead && failing_live_peers >= ESTABLISH_STUCK_CORROBORATING_PEERS
    {
        return Some(WedgeEvidence::Restart("accepts_stuck"));
    }
    let last_fresh = es
        .last_inbound_established_ms
        .unwrap_or(0)
        .max(es.last_outbound_established_ms.unwrap_or(0))
        .max(boot_ms);
    let quiet_ms = now_ms.saturating_sub(last_fresh);
    if quiet_ms >= window_ms && failing_live_peers >= ESTABLISH_FAILING_PEERS_MIN {
        return Some(WedgeEvidence::Restart("no_new_connections"));
    }
    inbound_stuck.then_some(WedgeEvidence::InboundOnly)
}

/// Conditions 2-4 of the establishment trigger, in order: `Err(blocker)` names the first one that
/// holds the restart back. Condition 5 (the rate cap and the orphan-reap interlock) is shared by
/// every automatic restart and lives in `RestartReason::admissible`.
pub fn establishment_wedge_gate(
    uptime_ms: u64,
    audible_peers: usize,
    evidence_for_ms: u64,
    stagger_ms: u64,
) -> Result<(), &'static str> {
    if uptime_ms < ESTABLISH_MIN_UPTIME_MS {
        return Err("uptime below 10 min");
    }
    if audible_peers == 0 {
        return Err("no audible peer (the fleet itself may be down)");
    }
    if evidence_for_ms < stagger_ms {
        return Err("inside this node's stagger");
    }
    Ok(())
}

/// `(audible, failing)`: gossip-fresh peers (any identity the registry currently hears,
/// deliberately not intersected with the expected roster, which this trigger must not depend on),
/// and the directly-heard peers (own self-report received by this node inside the direct-freshness
/// window, never a relayed copy) whose latest dial from here inside `window` TIMED OUT with no
/// success since. A peer heard only through third parties may be partitioned from us rather than us
/// from everyone, and a failed dial to a peer nobody hears is a dead peer: neither is our wedge.
fn live_peer_evidence(cloud: &CloudState, window: Duration) -> (usize, usize) {
    let audible = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.peer_id.is_some())
        .count();
    let fresh = CloudState::direct_freshness();
    let heard: std::collections::HashSet<String> = cloud
        .registry
        .gossip_evidence_snapshot()
        .into_iter()
        .filter(|e| e.last_received_ago <= fresh)
        .map(|e| e.endpoint_id)
        .collect();
    let Some(pool) = cloud.mesh.read().clone() else {
        return (audible, 0);
    };
    let failing = pool
        .dial_evidence_snapshot()
        .into_iter()
        .filter(|e| heard.contains(&e.endpoint_id))
        .filter(|e| {
            e.last_timeout_ago.is_some_and(|timed_out| {
                timed_out < window && e.last_success_ago.is_none_or(|ok| ok > timed_out)
            })
        })
        .count();
    (audible, failing)
}

/// Loop-local state of the establishment trigger.
#[derive(Default)]
struct EstablishWatch {
    /// When the current evidence streak began (0 = no evidence).
    since_ms: u64,
    last_warn_ms: u64,
}

impl EstablishWatch {
    /// One tick. Returns true once the shared restart path ACCEPTED the
    /// request (the caller stops watching); a refused request keeps watching.
    fn tick(
        &mut self,
        cloud: &CloudState,
        h: &crate::state::MeshHealth,
        now: u64,
        restart: &crate::ControlledRestart,
    ) -> bool {
        let window_secs = establish_wedge_secs();
        let window_ms = window_secs.saturating_mul(1000);
        let es = hive_p2p::establish_stats(window_secs);
        let (audible, failing) = live_peer_evidence(cloud, Duration::from_millis(window_ms));
        let boot_ms = now.saturating_sub(h.uptime_ms);
        let evidence = establishment_wedge_evidence(&es, now, boot_ms, window_ms, failing);
        let Some(WedgeEvidence::Restart(kind)) = evidence else {
            if self.since_ms != 0 {
                tracing::info!(
                    held_secs = now.saturating_sub(self.since_ms) / 1000,
                    "mesh watchdog: establishment wedge evidence cleared"
                );
            }
            self.since_ms = 0;
            if evidence == Some(WedgeEvidence::InboundOnly)
                && now.saturating_sub(self.last_warn_ms) >= 300_000
            {
                self.last_warn_ms = now;
                tracing::warn!(
                    window_secs,
                    accept_stuck_window = es.accept_stuck_window,
                    outbound_attempts_window = es.outbound_attempts_window,
                    outbound_established_window = es.outbound_established_window,
                    failing_live_peers = failing,
                    "mesh watchdog: inbound accepts are stuck but this node's own outbound \
                     connection setup is not shown dead — WARN only (any remote can hold a \
                     handshake open past the accept deadline)"
                );
            }
            return false;
        };
        if self.since_ms == 0 {
            self.since_ms = now;
        }
        let stagger_ms = node_stagger_ms(&cloud.node_name);
        let reason = crate::RestartReason::EstablishmentWedge;
        let gate = establishment_wedge_gate(
            h.uptime_ms,
            audible,
            now.saturating_sub(self.since_ms),
            stagger_ms,
        )
        .map_err(str::to_string)
        .and_then(|()| reason.admissible());
        let blocker = match gate {
            Ok(()) if establish_restart_enabled() => None,
            Ok(()) => Some("restart disabled by HIVE_MESH_ESTABLISH_WEDGE_RESTART=0".to_string()),
            Err(blocker) => Some(blocker),
        };
        if blocker.is_none() && restart.request(reason) {
            tracing::error!(
                evidence = kind,
                window_secs,
                failing_live_peers = failing,
                audible_peers = audible,
                accept_stuck_window = es.accept_stuck_window,
                refused_window = es.refused_window,
                outbound_attempts_window = es.outbound_attempts_window,
                inbound_initials_window = es.inbound_initials_window,
                last_inbound_established_ms = ?es.last_inbound_established_ms,
                last_outbound_established_ms = ?es.last_outbound_established_ms,
                held_secs = now.saturating_sub(self.since_ms) / 1000,
                "mesh watchdog: establishment wedge — this endpoint stopped forming new \
                 connections while the fleet stays audible over warm trunks (the iroh actor-chain \
                 wedge that only a process restart has ever cleared). Requested the shared \
                 graceful restart path. Set HIVE_MESH_ESTABLISH_WEDGE_RESTART=0 to disable."
            );
            return true;
        }
        if now.saturating_sub(self.last_warn_ms) >= 300_000 {
            self.last_warn_ms = now;
            tracing::warn!(
                evidence = kind,
                held_by = blocker.as_deref().unwrap_or("the shared restart gate"),
                window_secs,
                failing_live_peers = failing,
                audible_peers = audible,
                accept_stuck_window = es.accept_stuck_window,
                refused_window = es.refused_window,
                held_secs = now.saturating_sub(self.since_ms) / 1000,
                stagger_secs = stagger_ms / 1000,
                "mesh watchdog: establishment wedge evidence — no restart yet"
            );
        }
        false
    }
}

fn degraded_window_ms() -> u64 {
    std::env::var("HIVE_MESH_DEGRADED_WINDOW_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1800)
        * 1000
}

fn degraded_trigger_ms() -> u64 {
    std::env::var("HIVE_MESH_DEGRADED_TRIGGER_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1200)
        * 1000
}

fn degraded_restart_enabled() -> bool {
    !matches!(
        std::env::var("HIVE_MESH_DEGRADED_RESTART")
            .unwrap_or_default()
            .trim(),
        "0" | "false" | "off"
    )
}

/// Spawn the watchdog loop. Cheap: one registry read every 30s.
pub fn spawn(cloud: Arc<CloudState>, restart: crate::ControlledRestart) {
    /// Timestamp (ms) of the first tick of the CURRENT isolation streak; 0 = not isolated. Reset to
    /// 0 the moment a directly reachable peer is visible again.
    static ISOLATED_SINCE_MS: AtomicU64 = AtomicU64::new(0);
    /// Latched (as 1) once this node has directly reached a peer: guard 2 of the continuous
    /// trigger.
    static EVER_SAW_PEER: AtomicU64 = AtomicU64::new(0);

    tokio::spawn(async move {
        let wedge_ms = wedge_secs().saturating_mul(1000) + node_stagger_ms(&cloud.node_name);
        let window_ms = degraded_window_ms();
        let trigger_ms = degraded_trigger_ms() + node_stagger_ms(&cloud.node_name);
        let mut ever_converged = false;
        let mut samples: std::collections::VecDeque<(u64, bool)> = Default::default();
        let mut establish = EstablishWatch::default();
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let h = cloud.mesh_health();
            let now = hive_core::now_ms();

            if establish.tick(&cloud, &h, now, &restart) {
                return;
            }
            let direct_reachable_peers = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| {
                    !n.is_self && n.healthy && !crate::health::is_cold(&cloud.registry, &n.id)
                })
                .count();

            let degraded_now =
                h.expected_peers > 0 && direct_reachable_peers < degraded_floor(h.expected_peers);
            samples.push_back((now, degraded_now));
            while samples
                .front()
                .is_some_and(|(ts, _)| now.saturating_sub(*ts) > window_ms)
            {
                samples.pop_front();
            }
            let degraded_ms: u64 = samples.iter().filter(|(_, d)| *d).count() as u64 * 30_000;
            let ever = EVER_SAW_PEER.load(Ordering::Relaxed) == 1;
            if h.expected_peers > 0 && direct_reachable_peers > degraded_floor(h.expected_peers) {
                ever_converged = true;
            }
            if degraded_now {
                tracing::warn!(
                    direct_reachable_peers,
                    service_healthy_peers = h.visible_healthy_peers,
                    expected_peers = h.expected_peers,
                    floor = degraded_floor(h.expected_peers),
                    degraded_secs_in_window = degraded_ms / 1000,
                    trigger_secs = trigger_ms / 1000,
                    window_secs = window_ms / 1000,
                    "mesh watchdog: node is severely DEGRADED (direct reachability below the peer floor)"
                );
            }
            if cumulative_should_restart(
                h.expected_peers,
                ever_converged,
                degraded_now,
                h.audible_peers,
                degraded_ms,
                trigger_ms,
            ) {
                if !degraded_restart_enabled() {
                    tracing::error!(
                        degraded_secs_in_window = degraded_ms / 1000,
                        "mesh watchdog: cumulative degradation past the trigger — restart                          disabled by HIVE_MESH_DEGRADED_RESTART=0"
                    );
                } else if restart.request(crate::RestartReason::MeshDegradation) {
                    tracing::error!(
                        direct_reachable_peers,
                        service_healthy_peers = h.visible_healthy_peers,
                        expected_peers = h.expected_peers,
                        degraded_secs_in_window = degraded_ms / 1000,
                        "mesh watchdog: node is WEDGED BY FLAPPING — direct peer reachability sat below the floor for the cumulative trigger within the window, the exact shape the continuous-isolation trigger structurally misses. Requested the shared graceful restart path. Set HIVE_MESH_DEGRADED_RESTART=0 to disable."
                    );
                    return;
                }
            }

            /// Convergence budget of the boot-wedge arm (module doc): a node that has run this long
            /// without ever reaching a peer directly while the fleet is audible is wedged, not
            /// firewalled.
            const BOOT_CONVERGENCE_BUDGET_MS: u64 = 15 * 60 * 1000;
            if !ever
                && h.expected_peers > 0
                && h.uptime_ms > BOOT_CONVERGENCE_BUDGET_MS
                && direct_reachable_peers == 0
                && h.audible_peers >= degraded_floor(h.expected_peers)
            {
                if !restart_enabled() {
                    tracing::error!(
                        audible_peers = h.audible_peers,
                        "mesh watchdog: BOOT WEDGE (fleet audible, zero direct exchanges since boot) — restart disabled by HIVE_MESH_WEDGE_RESTART=0"
                    );
                } else if restart.request(crate::RestartReason::MeshIsolation) {
                    tracing::error!(
                        audible_peers = h.audible_peers,
                        expected_peers = h.expected_peers,
                        uptime_secs = h.uptime_ms / 1000,
                        "mesh watchdog: node is BOOT-WEDGED — the fleet is gossip-audible but it has never completed a direct exchange since boot (transport wedged before first contact; the never-converged guards would otherwise leave it dark forever). Requested the shared graceful restart path. Set HIVE_MESH_WEDGE_RESTART=0 to disable."
                    );
                    return;
                }
            }

            if direct_reachable_peers > 0 {
                EVER_SAW_PEER.store(1, Ordering::Relaxed);
                if ISOLATED_SINCE_MS.swap(0, Ordering::Relaxed) != 0 {
                    tracing::info!(
                        direct_reachable_peers,
                        service_healthy_peers = h.visible_healthy_peers,
                        expected_peers = h.expected_peers,
                        "mesh watchdog: direct isolation cleared — peers reachable again"
                    );
                }
                continue;
            }

            if !crate::state::mesh_isolated(h.expected_peers, direct_reachable_peers) {
                continue;
            }

            let since = match ISOLATED_SINCE_MS.load(Ordering::Relaxed) {
                0 => {
                    ISOLATED_SINCE_MS.store(now, Ordering::Relaxed);
                    now
                }
                s => s,
            };
            let isolated_for_ms = now.saturating_sub(since);

            tracing::warn!(
                direct_reachable_peers,
                service_healthy_peers = h.visible_healthy_peers,
                expected_peers = h.expected_peers,
                isolated_secs = isolated_for_ms / 1000,
                wedge_secs = wedge_ms / 1000,
                ever_saw_peer = ever,
                uptime_ms = h.uptime_ms,
                "mesh watchdog: this node directly reaches NONE of its expected peers"
            );

            if !should_restart(h.expected_peers, ever, isolated_for_ms, wedge_ms) {
                continue;
            }

            if !restart_enabled() {
                tracing::error!(
                    isolated_secs = isolated_for_ms / 1000,
                    "mesh watchdog: node is WEDGED (isolated past the threshold after having \
                     been converged) — self-restart is disabled by HIVE_MESH_WEDGE_RESTART=0, \
                     so it will stay dark until an operator restarts it"
                );
                continue;
            }

            if !restart.request(crate::RestartReason::MeshIsolation) {
                continue;
            }
            tracing::error!(
                expected_peers = h.expected_peers,
                isolated_secs = isolated_for_ms / 1000,
                "mesh watchdog: node is WEDGED — it converged earlier in this process's life \
                 and has now seen zero healthy peers for the full threshold, which is the \
                 measured signature of the iroh transport wedge (relay reconnect storm, gossip \
                 dead, process otherwise healthy). Requested the shared graceful restart path; \
                 the node rejoins in ~30s. Set HIVE_MESH_WEDGE_RESTART=0 to disable."
            );
            return;
        }
    });
}
