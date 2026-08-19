//! Mesh-isolation watchdog: self-restart a node whose mesh subsystem has
//! wedged, instead of leaving it dark until a human notices.
//!
//! ## The failure this exists for, measured rather than imagined
//!
//! A node can keep its process alive, its systemd unit `active`, and its HTTP
//! surfaces answering, while its iroh mesh layer is functionally dead. Live on
//! this fleet (fc-sanjose, 2026-08-17, ~9h after a clean restart):
//!
//! * `/v1/mesh` reported `isolated: true`, `visible_healthy_peers: 0` — every
//!   one of 18 expected peers marked UNHEALTHY by this node's own prober.
//! * `journalctl` carried **2,174,031** `iroh::socket::transports` events, with
//!   a relay reconnect storm underneath it (3145 "Client stream read failed",
//!   2668 "Ping timeout", 1636 "Stream closed by server").
//! * **Zero** anti-entropy / gossip / roster events in a 20-minute window: the
//!   mesh had stopped doing anything at all.
//! * It never recovered on its own. The same wedge had already produced a
//!   23-hour fleet-wide outage the previous day, and `systemctl is-active`
//!   said `active` throughout both.
//!
//! Because fc-sanjose is the first entry in `HIVE_CP_OWNER_CHAIN`, an isolated
//! leader also fails every admin MUTATION fleet-wide: `leader_forward_candidates`
//! is built from the LOCAL registry, so a node that can see no healthy peers
//! produces an EMPTY candidate list and `admin_forward_to_leader` returns
//! "control-plane leader unreachable" with nothing logged. Deploys stop
//! entirely. That is the user-visible shape of this bug, and it is why a
//! liveness probe on the process is not enough — the process is fine; its view
//! of the fleet is not.
//!
//! ## Why a restart, and why that is not a cop-out here
//!
//! The wedge lives inside the iroh endpoint's own transport/relay actors, below
//! anything this crate drives; there is no supported in-process "rebuild the
//! endpoint" call to reach for, and the measured recovery for every occurrence
//! so far has been exactly one thing: restart the process, after which the node
//! rejoins in ~20-40s (measured repeatedly this session: 12-17 of 18 peers
//! visible within 35s). Converting a permanent, human-paged outage into a
//! ~30-second automatic blip is a real improvement even though it does not fix
//! iroh; when the underlying transport bug is fixed this watchdog simply stops
//! firing. `memwatch`'s RSS self-restart is the same trade and the same
//! mechanism (`exit(17)` under the unit's `Restart=always`), deliberately
//! reused rather than reinvented.
//!
//! ## Guards, so this can never become a restart loop
//!
//! Firing is gated on ALL of:
//!
//! 1. `expected_peers > 0` — peers are configured. A genuinely standalone node
//!    is never restarted.
//! 2. **This node has seen at least one healthy peer since boot.** This is the
//!    load-bearing guard: it distinguishes "the mesh worked and then broke"
//!    (the wedge) from "this node never had connectivity" (a firewall, a bad
//!    seed list, a real network partition — none of which a restart fixes, and
//!    all of which a restart loop would make worse). A node that has never
//!    converged is left alone and loud.
//! 3. Continuous isolation for `HIVE_MESH_WEDGE_SECS` (default 600s). The
//!    counter resets the instant a single healthy peer reappears, so ordinary
//!    churn, a peer restart, or a slow probe cycle never trips it.
//! 4. `HIVE_MESH_WEDGE_RESTART=0` disables the restart entirely (the WARN
//!    still fires), for an operator debugging a wedged node who does not want
//!    it yanked out from under them.

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

/// Pure decision core: the guard conjunction, isolated from the loop so the
/// firing condition is readable in one place.
///
/// `isolated_since_ms == 0` means "not currently isolated". Returns true only
/// when every guard in the module doc holds.
pub fn should_restart(
    expected_peers: usize,
    ever_saw_peer: bool,
    isolated_for_ms: u64,
    wedge_ms: u64,
) -> bool {
    expected_peers > 0 && ever_saw_peer && isolated_for_ms >= wedge_ms
}

/// The severe-degradation floor: seeing fewer than a QUARTER of the expected
/// fleet (min 1) is the measured wedge shape — fc-bangkok sat at 3 of 18
/// visible for 90 minutes. A healthy node on this fleet holds 10-16 of 18
/// (several "expected" ids are permanently-off dev nodes), so the floor stays
/// far below normal variance; and tiny fleets degrade the floor to 1, where
/// the zero-visible case is the continuous-isolation trigger's job anyway.
pub fn degraded_floor(expected_peers: usize) -> usize {
    (expected_peers / 4).max(1)
}

/// CUMULATIVE-degradation restart decision — the flapping blind spot's fix.
/// The continuous trigger above resets whenever isolation clears for one tick,
/// and the failure actually observed was exactly that: isolation clearing for
/// 30s every few minutes, forever, while the node stayed effectively dark
/// (meshwatch logged "sees NONE"→"cleared" cycles for 90 minutes and never
/// fired). This trigger sums DEGRADED time (visible < [`degraded_floor`])
/// over a sliding window, so clearing for a tick no longer erases the streak;
/// it must also be degraded RIGHT NOW (a node that genuinely recovered is
/// never restarted for its history). Restart cadence self-limits to one per
/// `trigger_ms` while genuinely stuck.
/// Adversarial review of the first cut found three ways the naive form fired
/// synchronized restarts on states a restart cannot fix; each added guard is
/// one finding:
///  - `ever_converged` (was ONCE at/above floor+1), not merely ever-saw-one-
///    peer: a node that has NEVER converged (firewall misconfig, minority
///    partition since boot) must stay alone-and-loud, not kill-loop — the
///    continuous trigger's guard-2 doctrine applied here too.
///  - `audible_peers >= floor`: the fleet must be gossip-AUDIBLE to us while
///    our probes fail — that is a LOCAL transport wedge, which a restart
///    heals. Survivors of a mass outage / a minority partition hear almost
///    nobody, and restarting the platform's last remaining capacity every 20
///    minutes is the opposite of degraded operation.
///  - the caller staggers the effective trigger per node (`node_stagger_ms`),
///    so a fleet-wide shared onset can never fire every node — including all
///    three control-plane leaders — inside one 30s tick.
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

/// Deterministic per-node stagger added to the cumulative trigger: FNV over
/// the node name, spread across 0..10 minutes. Identity-derived (never shared
/// wall time), so nodes crossing the threshold in the same tick still exit
/// minutes apart — the first restart usually heals the wedge for the rest,
/// and a synchronized fleet-wide bounce (the deploy playbook's own forbidden
/// state) is structurally impossible.
pub fn node_stagger_ms(node_name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in node_name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h % 600_000
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
pub fn spawn(cloud: Arc<CloudState>) {
    // ms timestamp of the first tick of the CURRENT isolation streak; 0 = not
    // isolated. Reset to 0 the moment any healthy peer is visible again.
    static ISOLATED_SINCE_MS: AtomicU64 = AtomicU64::new(0);
    // Latched once this node has ever seen a healthy peer (guard 2).
    static EVER_SAW_PEER: AtomicU64 = AtomicU64::new(0);

    tokio::spawn(async move {
        let wedge_ms = wedge_secs().saturating_mul(1000) + node_stagger_ms(&cloud.node_name);
        let window_ms = degraded_window_ms();
        let trigger_ms = degraded_trigger_ms() + node_stagger_ms(&cloud.node_name);
        // Latched once this node was genuinely CONVERGED (visible above the
        // floor) — the cumulative trigger's arming bar. Loop-local: resets on
        // restart, so a fresh process must converge again before it may fire.
        let mut ever_converged = false;
        // Sliding window of (sample ts, was-degraded) — one 30s sample per tick.
        let mut samples: std::collections::VecDeque<(u64, bool)> = Default::default();
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let h = cloud.mesh_health();
            let now = hive_core::now_ms();
            // Service eligibility and direct outbound reachability are separate.
            // Gossip restoration intentionally makes a live peer `healthy`
            // again for DNS/placement, but keeps its observer-local cold mark
            // until a direct exchange succeeds. Watch the latter here so the
            // self-healer cannot erase its own wedge signal.
            let direct_reachable_peers = cloud
                .registry
                .nodes()
                .into_iter()
                .filter(|n| {
                    !n.is_self && n.healthy && !crate::health::is_cold(&cloud.registry, &n.id)
                })
                .count();

            // ---- Cumulative-degradation trigger (runs EVERY tick, before the
            // continuous-isolation logic's early continues) ----
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
                } else {
                    tracing::error!(
                        direct_reachable_peers,
                        service_healthy_peers = h.visible_healthy_peers,
                        expected_peers = h.expected_peers,
                        degraded_secs_in_window = degraded_ms / 1000,
                        "mesh watchdog: node is WEDGED BY FLAPPING — direct peer reachability sat below the floor for the cumulative trigger within the window, the exact shape the continuous-isolation trigger structurally misses. Flushing state and exiting for a clean systemd restart. Set HIVE_MESH_DEGRADED_RESTART=0 to disable."
                    );
                    crate::persist::flush_blocking();
                    std::process::exit(17);
                }
            }

            // ---- Boot-wedge arm: a node whose transport wedged BEFORE its
            // first successful exchange never latches EVER_SAW_PEER, so both
            // restart triggers stay disarmed forever while the node reports
            // healthy — a permanent, invisible wedge (refutation finding F2).
            // Distinguish it from a genuinely firewalled node by AUDIBILITY:
            // hearing the fleet (audible >= floor) while never having
            // reached anyone directly is a LOCAL wedge, which a restart
            // heals; a firewalled node hears ~0 and correctly stays
            // alone-and-loud. Fires at most once per process, past a
            // convergence budget.
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
                } else {
                    tracing::error!(
                        audible_peers = h.audible_peers,
                        expected_peers = h.expected_peers,
                        uptime_secs = h.uptime_ms / 1000,
                        "mesh watchdog: node is BOOT-WEDGED — the fleet is gossip-audible but it has never completed a direct exchange since boot (transport wedged before first contact; the never-converged guards would otherwise leave it dark forever). Flushing state and exiting for a clean systemd restart. Set HIVE_MESH_WEDGE_RESTART=0 to disable."
                    );
                    crate::persist::flush_blocking();
                    std::process::exit(17);
                }
            }

            if direct_reachable_peers > 0 {
                EVER_SAW_PEER.store(1, Ordering::Relaxed);
                // Recovered (or never lost): clear the streak.
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
                continue; // no peers expected: standalone node, nothing to do
            }

            // Start (or continue) the isolation streak.
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

            tracing::error!(
                expected_peers = h.expected_peers,
                isolated_secs = isolated_for_ms / 1000,
                "mesh watchdog: node is WEDGED — it converged earlier in this process's life \
                 and has now seen zero healthy peers for the full threshold, which is the \
                 measured signature of the iroh transport wedge (relay reconnect storm, gossip \
                 dead, process otherwise healthy). Flushing state and exiting for a clean \
                 systemd restart (Restart=always); the node rejoins in ~30s. Set \
                 HIVE_MESH_WEDGE_RESTART=0 to disable."
            );
            // Same ordering as memwatch's restart arm: the background persister
            // writes on its own cadence, so without this flush everything since
            // its last tick is lost on exit.
            crate::persist::flush_blocking();
            std::process::exit(17);
        }
    });
}
