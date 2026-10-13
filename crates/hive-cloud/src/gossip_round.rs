//! Gossip-round policy on top of [`crate::bounded_round`]: the round deadline, dead-target backoff,
//! the per-target leash, round telemetry, and whether the loop is still ending rounds at all.
//!
//! A round that waits on every target lasts as long as its SLOWEST one, and every relayed
//! `last_seen_ms` this observer holds is only as fresh as its round is short: one dead seed
//! stretches the round, ages the whole view past `health::GOSSIP_ALIVE_MS` and gets healthy peers
//! demoted. Two mechanisms bound that.
//!
//! * **Deadline and backoff.** The round ends at [`deadline`] (stragglers run on, see
//! `bounded_round`). A target with no evidence of life for [`DEAD_AFTER_MS`] is dialed on a 1 -> 3
//! min backoff instead of every round, but only while this node's OWN view is fresh (some target
//! answered within [`VIEW_FRESH_MS`]): a node that reaches nobody sees every target as silent and
//! must keep dialing all of them, seeds above all, every round.
//! * **Leash.** A deadline is not enough on its own: a target's own dial budget
//! (`hive_p2p::dial_fallback_ceiling`, longer than the round deadline on the fleet) lets one target
//! that never answers hold every round to the full deadline, and the backoff cannot help because
//! [`Rounds::admit`] never backs off a target another node still gossips about (the undialable
//! ones, reached only through a relay, are exactly those). After [`LEASH_AFTER_ROUNDS`] consecutive
//! missed deadlines a target is dialed exactly as often as before, but the round waits only
//! [`straggler_leash_ms`] for it (see `bounded_round::collect_leashed`). Parking is never
//! cancellation, never a skip and never a withdrawal: the sync keeps running, its result still
//! lands (one round late at worst), and one answer inside the round restores the full deadline
//! immediately.

use hive_core::now_ms;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Pause between the end of one round and the start of the next.
pub const ROUND_SLEEP: Duration = Duration::from_secs(5);
/// A target with no evidence of life (no successful sync by us AND no gossip
/// of it in our registry) for this long is dead: dialed on backoff only.
pub const DEAD_AFTER_MS: u64 = 600_000;
/// This node's own view is fresh while some target answered within this long (four-plus round
/// periods); outside it no target counts as dead, since the silence is this node's.
pub const VIEW_FRESH_MS: u64 = 60_000;
const BACKOFF_FIRST_MS: u64 = 60_000;
/// Inside the mesh's three-minute bound on anything being undialable (the
/// PeerPool caps): 60 s, 120 s, then 180 s.
const BACKOFF_MAX_MS: u64 = 180_000;
/// Consecutive rounds in which a target was dispatched and did NOT report before the round ended,
/// after which the round stops giving it the full deadline ([`straggler_leash_ms`] instead). Two,
/// never one: a single slow round is a network event, a second in a row is a target that is not
/// answering.
const LEASH_AFTER_ROUNDS: u32 = 2;
/// Round percentiles are logged (and the margin checked) this often.
const LOG_EVERY_MS: u64 = 600_000;
const MAX_SAMPLES: usize = 4096;

static COMPLETED: AtomicU64 = AtomicU64::new(0);
/// [`mono_ms`] at which the last round ended (or the loop started); 0 = no
/// loop yet.
static LAST_ROUND_MONO_MS: AtomicU64 = AtomicU64::new(0);
/// Wall-clock ms some target last answered (0 = none yet).
static LAST_REACHED_MS: AtomicU64 = AtomicU64::new(0);
static PERIOD_P50_MS: AtomicU64 = AtomicU64::new(0);
static PERIOD_P99_MS: AtomicU64 = AtomicU64::new(0);
static COLLECT_P99_MS: AtomicU64 = AtomicU64::new(0);
static RESYNC_P99_MS: AtomicU64 = AtomicU64::new(0);
static DEADLINE_HITS: AtomicU64 = AtomicU64::new(0);
static STRAGGLERS: AtomicU64 = AtomicU64::new(0);
static BACKED_OFF: AtomicU64 = AtomicU64::new(0);
/// Targets the round currently waits only [`straggler_leash_ms`] for.
static LEASHED: AtomicU64 = AtomicU64::new(0);

/// `HIVE_GOSSIP_ROUND_DEADLINE_MS` (default 8000): how long a round waits for
/// its targets before it merges what it has.
pub fn deadline() -> Duration {
    let ms = std::env::var("HIVE_GOSSIP_ROUND_DEADLINE_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(8_000);
    Duration::from_millis(ms)
}

/// `HIVE_GOSSIP_STRAGGLER_LEASH_MS` (default 2500): how long a round waits for one target that has
/// already missed the round deadline [`LEASH_AFTER_ROUNDS`] times running. Above a healthy gossip
/// sync (a two-request exchange over a warm trunk, measured 70-550 ms per request fleet-wide) and
/// far below the 8 s deadline, so it parks targets that are not answering without ever parking one
/// that is.
pub fn straggler_leash_ms() -> u64 {
    std::env::var("HIVE_GOSSIP_STRAGGLER_LEASH_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(2_500)
}

/// Monotonic ms since first use (never 0): a wall-clock step can neither fake
/// nor hide a stalled loop.
fn mono_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    (EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64).saturating_add(1)
}

/// Four worst-case round periods.
fn stall_after_ms() -> u64 {
    ((deadline() + ROUND_SLEEP).as_millis() as u64).saturating_mul(4)
}

/// Has the gossip loop stopped ending rounds? True before the loop starts and when no round has
/// ended for four worst-case round periods (a whole-runtime freeze, a dead loop): this node's
/// registry is not refreshed then, so every peer looks stale from it (see
/// `health::stale_long_enough`).
pub fn stalled() -> bool {
    let last = LAST_ROUND_MONO_MS.load(Ordering::Relaxed);
    last == 0 || mono_ms().saturating_sub(last) > stall_after_ms()
}

struct TargetState {
    /// Epoch-ms of our last successful sync (first sighting until then).
    ok_ms: u64,
    /// `ok_ms` is a real sync, not the first sighting.
    synced: bool,
    backoff_ms: u64,
    next_try_ms: u64,
    /// Newest gossiped `last_seen_ms` the registry held for it (0 = never).
    heard_ms: u64,
    /// The node id it answered as on its last sync: what `heard_ms` resolves
    /// through once a failed fetch has evicted the target's transport mapping
    /// (a `--peer` URL target names no identity of its own).
    identity: Option<String>,
}

/// The endpoint a target names: `seed:<64hex>` and `<64hex>` are the same one.
fn endpoint_key(target: &str) -> &str {
    target.strip_prefix("seed:").unwrap_or(target)
}

/// Loop-owned round state (single writer: the gossip loop).
pub struct Rounds {
    targets: HashMap<String, TargetState>,
    /// Per ENDPOINT, consecutive rounds in which it was dispatched and did not report before the
    /// round ended, plus the epoch-ms it was last counted. At [`LEASH_AFTER_ROUNDS`] the round
    /// stops giving that endpoint the full deadline; a single report inside a round clears it.
    /// Entries not counted for [`DEAD_AFTER_MS`] are forgotten in `round_done`, so a node that
    /// leaves the fleet leaves no leash state behind.
    ///
    /// Keyed by ENDPOINT ID, never by the target's label: `seed:<64hex>` and the bare `<64hex>`
    /// roster entry are two labels for ONE endpoint (both are dialed every round), and a relayed
    /// registry entry that blinks for a round drops its label from the candidate set. Keyed on the
    /// label, the count restarted on both, and an endpoint that never answers re-bought the full
    /// deadline every few rounds.
    slow: HashMap<String, (u32, u64)>,
    /// Epoch-ms some target last answered (0 = none yet).
    last_reached_ms: u64,
    /// [`mono_ms`] the current round started (0 = none yet).
    round_start_mono: u64,
    /// Start-to-start round periods: the loop's real refresh interval.
    period: Vec<u64>,
    /// Collect phases of rounds that dispatched something (deadline-capped).
    collect: Vec<u64>,
    /// Per target, the interval between two successful syncs.
    resync: Vec<u64>,
    window_start_ms: u64,
    window_deadline_hits: u64,
    window_stragglers: u64,
    window_backoff_skips: u64,
    window_in_flight_skips: u64,
}

fn push_sample(samples: &mut Vec<u64>, v: u64) {
    if samples.len() < MAX_SAMPLES {
        samples.push(v);
    }
}

/// (p50, p99) of `samples`, sorted in place; (0, 0) when empty.
fn percentiles(samples: &mut [u64]) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    samples.sort_unstable();
    let n = samples.len();
    let rank = |q: f64| samples[((q * n as f64).ceil() as usize).clamp(1, n) - 1];
    (rank(0.50), rank(0.99))
}

impl Rounds {
    pub fn new(now: u64) -> Self {
        LAST_ROUND_MONO_MS.store(mono_ms(), Ordering::Relaxed);
        Self {
            targets: HashMap::new(),
            slow: HashMap::new(),
            last_reached_ms: 0,
            round_start_mono: 0,
            period: Vec::new(),
            collect: Vec::new(),
            resync: Vec::new(),
            window_start_ms: now,
            window_deadline_hits: 0,
            window_stragglers: 0,
            window_backoff_skips: 0,
            window_in_flight_skips: 0,
        }
    }

    /// Did some target answer within [`VIEW_FRESH_MS`]? Only then is a
    /// target's silence evidence about the target.
    fn view_fresh(&self, now: u64) -> bool {
        self.last_reached_ms != 0 && now.saturating_sub(self.last_reached_ms) < VIEW_FRESH_MS
    }

    fn dead(st: &TargetState, now: u64, view_fresh: bool) -> bool {
        view_fresh && now.saturating_sub(st.ok_ms.max(st.heard_ms)) >= DEAD_AFTER_MS
    }

    /// A round starts: samples the start-to-start period.
    pub fn begin_round(&mut self) {
        let now = mono_ms();
        if self.round_start_mono != 0 {
            push_sample(&mut self.period, now.saturating_sub(self.round_start_mono));
        }
        self.round_start_mono = now;
    }

    /// The node id `target` answered as on its last sync, if any.
    pub fn identity(&self, target: &str) -> Option<&str> {
        self.targets.get(target)?.identity.as_deref()
    }

    /// `target` was skipped this round because its last sync still runs.
    pub fn note_in_flight(&mut self) {
        self.window_in_flight_skips += 1;
    }

    /// Is `target` dispatched this round? `heard_ms` is the registry's gossiped `last_seen_ms` for
    /// it (0 = unknown): a target some other node still hears is never backed off, however long OUR
    /// dials to it have failed, since that is a transport fault worth retrying every round.
    pub fn admit(&mut self, target: &str, heard_ms: u64, now: u64) -> bool {
        let view_fresh = self.view_fresh(now);
        let st = self
            .targets
            .entry(target.to_string())
            .or_insert(TargetState {
                ok_ms: now,
                synced: false,
                backoff_ms: 0,
                next_try_ms: 0,
                heard_ms: 0,
                identity: None,
            });
        st.heard_ms = st.heard_ms.max(heard_ms);
        if Self::dead(st, now, view_fresh) && now < st.next_try_ms {
            self.window_backoff_skips += 1;
            return false;
        }
        true
    }

    /// How long the round waits for `target`: `None` = the round deadline is the only bound,
    /// `Some(ms)` = a leash, measured by `bounded_round` from the moment the target was dispatched.
    /// Leashed only after [`LEASH_AFTER_ROUNDS`] consecutive missed deadlines AND only while this
    /// node's own view is fresh: while it reaches nobody the silence is this node's, and shortening
    /// its waits would only make a local transport fault look like every peer being slow.
    pub fn leash_ms(&self, target: &str, now: u64) -> Option<u64> {
        let missed = self
            .slow
            .get(endpoint_key(target))
            .map(|(n, _)| *n)
            .unwrap_or(0);
        if missed < LEASH_AFTER_ROUNDS || !self.view_fresh(now) {
            return None;
        }
        let ms = straggler_leash_ms().min((deadline().as_millis() as u64).saturating_sub(1));
        (ms > 0).then_some(ms)
    }

    /// `target` was dispatched this round and had not reported when the round ended. Two of those
    /// in a row and the round stops waiting the full deadline for that endpoint; it is still dialed
    /// every single round.
    pub fn note_straggler(&mut self, target: &str, now: u64) {
        let e = self.slow.entry(endpoint_key(target).to_string()).or_insert((0, now));
        if e.0 >= LEASH_AFTER_ROUNDS {
            e.1 = now;
            return;
        }
        e.0 = e.0.saturating_add(1);
        e.1 = now;
        if e.0 == LEASH_AFTER_ROUNDS {
            tracing::info!(
                target,
                leash_ms = straggler_leash_ms(),
                "gossip target missed the round deadline twice running — the round now waits at \
                 most that long for it, and still dials it every round (parked, never cancelled, \
                 never withdrawn)"
            );
        }
    }

    /// A dispatched target's sync ended (`reached` = the peer answered, as `identity` when its
    /// roster named itself). `timely` = it reported inside the round it was dispatched in; a result
    /// that landed late still counts as a missed round, since the round had to end without it. The
    /// first answer after a stretch in which this node reached nobody clears every dead-target
    /// backoff: our own transport works again, so no earlier backoff was evidence about its target.
    pub fn finished(
        &mut self,
        target: &str,
        reached: bool,
        identity: Option<String>,
        now: u64,
        timely: bool,
    ) {
        if reached {
            if !self.view_fresh(now) {
                let mut cleared = 0usize;
                for st in self.targets.values_mut().filter(|st| st.backoff_ms > 0) {
                    st.backoff_ms = 0;
                    st.next_try_ms = 0;
                    cleared += 1;
                }
                if cleared > 0 {
                    tracing::info!(
                        target,
                        cleared,
                        "gossip: a target answered after this node reached none for over a \
                         minute — every dead-target backoff cleared"
                    );
                }
            }
            self.last_reached_ms = now;
            LAST_REACHED_MS.store(now, Ordering::Relaxed);
        }
        let view_fresh = self.view_fresh(now);
        let Some(st) = self.targets.get_mut(target) else {
            return;
        };
        if timely {
            if let Some((n, _)) = self.slow.remove(endpoint_key(target)) {
                if n >= LEASH_AFTER_ROUNDS {
                    tracing::info!(
                        target,
                        "gossip target answered inside the round again — full round deadline \
                         restored"
                    );
                }
            }
        }
        if reached {
            if st.backoff_ms > 0 {
                tracing::info!(
                    target,
                    "gossip target answered again — dead-target backoff cleared"
                );
            }
            let resync = st.synced.then(|| now.saturating_sub(st.ok_ms));
            st.ok_ms = now;
            st.synced = true;
            st.backoff_ms = 0;
            st.next_try_ms = 0;
            if identity.is_some() {
                st.identity = identity;
            }
            if let Some(ms) = resync {
                push_sample(&mut self.resync, ms);
            }
        } else if Self::dead(st, now, view_fresh) {
            st.backoff_ms = if st.backoff_ms == 0 {
                BACKOFF_FIRST_MS
            } else {
                st.backoff_ms.saturating_mul(2).min(BACKOFF_MAX_MS)
            };
            st.next_try_ms = now.saturating_add(st.backoff_ms);
            if st.backoff_ms == BACKOFF_FIRST_MS {
                tracing::info!(
                    target,
                    silent_for_ms = now.saturating_sub(st.ok_ms.max(st.heard_ms)),
                    "gossip target: no successful sync and no gossip of it for 10 min while \
                     other targets answer — dialing it on a 1 -> 3 min backoff instead of \
                     every round"
                );
            }
        }
    }

    /// A round ended: `collect_ms` is its collect phase (`None` when it dispatched nothing) and
    /// `stragglers` its targets still syncing. Forgets targets that left the candidate set (never
    /// one still in flight) and, every `LOG_EVERY_MS`, logs the percentiles and checks the
    /// freshness margin. Returns true when the previous round ended more than four round periods
    /// ago, i.e. the loop is resuming from a stall.
    pub fn round_done(
        &mut self,
        candidates: &[String],
        in_flight: &HashSet<String>,
        collect_ms: Option<u64>,
        stragglers: usize,
        now: u64,
    ) -> bool {
        COMPLETED.fetch_add(1, Ordering::Relaxed);
        let mono = mono_ms();
        let prev = LAST_ROUND_MONO_MS.swap(mono, Ordering::Relaxed);
        let resumed = prev != 0 && mono.saturating_sub(prev) > stall_after_ms();
        if let Some(ms) = collect_ms {
            if ms >= deadline().as_millis() as u64 {
                DEADLINE_HITS.fetch_add(1, Ordering::Relaxed);
                self.window_deadline_hits += 1;
            }
            push_sample(&mut self.collect, ms);
        }
        STRAGGLERS.fetch_add(stragglers as u64, Ordering::Relaxed);
        self.window_stragglers += stragglers as u64;
        let keep: HashSet<&str> = candidates.iter().map(String::as_str).collect();
        self.targets
            .retain(|t, _| keep.contains(t.as_str()) || in_flight.contains(t));
        let view_fresh = self.view_fresh(now);
        let backed_off = self
            .targets
            .values()
            .filter(|st| st.backoff_ms > 0 && Self::dead(st, now, view_fresh))
            .count();
        BACKED_OFF.store(backed_off as u64, Ordering::Relaxed);
        self.slow
            .retain(|_, (_, seen)| now.saturating_sub(*seen) < DEAD_AFTER_MS);
        let leashed = if view_fresh {
            self.slow
                .values()
                .filter(|(n, _)| *n >= LEASH_AFTER_ROUNDS)
                .count()
        } else {
            0
        };
        LEASHED.store(leashed as u64, Ordering::Relaxed);
        if now.saturating_sub(self.window_start_ms) >= LOG_EVERY_MS && !self.period.is_empty() {
            self.log_window(backed_off, leashed);
            self.period.clear();
            self.collect.clear();
            self.resync.clear();
            self.window_start_ms = now;
            self.window_deadline_hits = 0;
            self.window_stragglers = 0;
            self.window_backoff_skips = 0;
            self.window_in_flight_skips = 0;
        }
        resumed
    }

    fn log_window(&mut self, backed_off: usize, leashed: usize) {
        let rounds = self.period.len();
        let (p50, p99) = percentiles(&mut self.period);
        let (collect_p50, collect_p99) = percentiles(&mut self.collect);
        let (resync_p50, resync_p99) = percentiles(&mut self.resync);
        PERIOD_P50_MS.store(p50, Ordering::Relaxed);
        PERIOD_P99_MS.store(p99, Ordering::Relaxed);
        COLLECT_P99_MS.store(collect_p99, Ordering::Relaxed);
        RESYNC_P99_MS.store(resync_p99, Ordering::Relaxed);
        tracing::info!(
            rounds,
            period_p50_ms = p50,
            period_p99_ms = p99,
            collect_p50_ms = collect_p50,
            collect_p99_ms = collect_p99,
            resync_p50_ms = resync_p50,
            resync_p99_ms = resync_p99,
            deadline_ms = deadline().as_millis() as u64,
            deadline_hits = self.window_deadline_hits,
            stragglers = self.window_stragglers,
            in_flight_skips = self.window_in_flight_skips,
            backoff_skips = self.window_backoff_skips,
            backed_off_targets = backed_off,
            leashed_targets = leashed,
            leash_ms = straggler_leash_ms(),
            "gossip rounds (last 10 min)"
        );
        let probe_ms = crate::health::probe_period_bound().as_millis() as u64;
        let needed = p99.saturating_mul(2).saturating_add(probe_ms);
        if crate::health::GOSSIP_ALIVE_MS < needed {
            tracing::warn!(
                gossip_alive_ms = crate::health::GOSSIP_ALIVE_MS,
                period_p99_ms = p99,
                probe_period_ms = probe_ms,
                needed_ms = needed,
                "gossip freshness margin is too thin: GOSSIP_ALIVE_MS < 2 x p99 round period + \
                 the probe round period, so a peer heard only through relays can age past the \
                 liveness bar between two of this node's rounds"
            );
        }
    }
}

/// Operator view, folded into `health::stats()`.
pub fn stats() -> serde_json::Value {
    let last = LAST_ROUND_MONO_MS.load(Ordering::Relaxed);
    let reached = LAST_REACHED_MS.load(Ordering::Relaxed);
    serde_json::json!({
        "deadline_ms": deadline().as_millis() as u64,
        "completed": COMPLETED.load(Ordering::Relaxed),
        "last_round_age_ms": (last != 0).then(|| mono_ms().saturating_sub(last)),
        "last_reached_age_ms": (reached != 0).then(|| now_ms().saturating_sub(reached)),
        "stalled": stalled(),
        "period_p50_ms_last_window": PERIOD_P50_MS.load(Ordering::Relaxed),
        "period_p99_ms_last_window": PERIOD_P99_MS.load(Ordering::Relaxed),
        "collect_p99_ms_last_window": COLLECT_P99_MS.load(Ordering::Relaxed),
        "resync_p99_ms_last_window": RESYNC_P99_MS.load(Ordering::Relaxed),
        "deadline_hits": DEADLINE_HITS.load(Ordering::Relaxed),
        "stragglers": STRAGGLERS.load(Ordering::Relaxed),
        "backed_off_targets": BACKED_OFF.load(Ordering::Relaxed),
        "leashed_targets": LEASHED.load(Ordering::Relaxed),
        "straggler_leash_ms": straggler_leash_ms(),
    })
}
