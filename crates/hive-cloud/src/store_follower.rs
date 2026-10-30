//! The follower pull: adopts the control-plane owner's `store_sync::REGISTRY` stores on every other
//! node. Two supervised loops, neither of which ever touches GuardianDB or the relational SQL
//! mirror, because replication of canonical state must never wait on either:
//!
//! * `store-follower-sync`: every `HIVE_STORE_SYNC_SECS` (60), every small store fetched
//!   CONCURRENTLY from the owner (`HIVE_STORE_SYNC_CONCURRENCY`, 8), then adopted one at a time.
//! * `store-large-lane`: LARGE stores (`store_sync::LARGE_STORE_BYTES`: billing, incidents, audit)
//!   one at a time, each under the budget its size calls for (`store_sync::fetch_budget`), in their
//!   own task so the small cadence never waits on a multi-minute transfer. One trunk sustains well
//!   under a megabyte per second to a cross-country peer, so multi-MB stores can never finish in
//!   the batch's 10 s budget, and 8-way concurrency on the same trunk only splits the bandwidth
//!   further.
//!
//! Lanes ([`StoreLanes`]): a store is in the large lane while its size (the local copy or the last
//! snapshot a peer served) is over the threshold (pulled at most once per
//! [`LARGE_STORE_PULL_EVERY`] after a success, retried after [`LARGE_STORE_RETRY`] after a
//! failure), or while it is SUSPECT: its batch fetch ran out the whole budget while other stores on
//! the same trunk fetched, which is the one failure shape that says 'too big for the batch' before
//! any size is known (a fresh follower's empty copy). A suspect store is pulled every lane round
//! until a pull succeeds and its size decides. Any other failure stays in the batch and retries
//! next tick; a tick where every store failed is the link, and the lane holds off until the batch
//! sees the link again.
//!
//! Adoption rules (wholesale only from an owner every node agrees on, merge stores from anyone):
//! see [`resolve_target`] and [`pull_batch`].

use std::sync::Arc;
use std::time::Duration;

use crate::state::CloudState;
use crate::store_sync::{self, SyncedStore};

/// A large store is pulled at most once per this interval after a success. 15 min and not 5: two
/// multi-MB stores at a 5-min cadence put sustained bulk on one trunk that a cross-country link
/// cannot carry alongside the small-store batch (the pulls then ran out their budget and left
/// megabytes of abandoned transfer on the connection), while 15 min keeps the sustained demand well
/// under the trunk, which is what lets the pull that does run finish.
const LARGE_STORE_PULL_EVERY: Duration = Duration::from_secs(900);
/// A failed large-lane pull is retried after this long the FIRST time; each consecutive failure
/// doubles the wait up to [`LARGE_STORE_PULL_EVERY`]. A flat 60 s retry was the feedback loop that
/// starved the owner: a pull that ran out its budget because the owner's egress was saturated came
/// back 60 s later and re-saturated it. A failure is still not a size proof, so the store stays in
/// the lane; it just stops hammering.
const LARGE_STORE_RETRY: Duration = Duration::from_secs(60);
/// Consecutive failures after which the retry wait stops doubling.
const LARGE_STORE_RETRY_DOUBLINGS: u32 = 4;
/// The large lane's round cadence.
const LARGE_LANE_TICK: Duration = Duration::from_secs(30);

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// State both loops share. Locked briefly, never across an await.
#[derive(Default)]
struct Shared {
    lanes: StoreLanes,
    failures: StorePullFailures,
    /// The last batch failed every store: the lane holds off.
    link_down: bool,
    /// A large-lane pull is on the trunk right now. The batch DEFERS while it is: one trunk
    /// sustains only tens of KB/s to a cross-country peer, so 8 concurrent small fetches beside a
    /// multi-MB transfer push EVERY small store past its 10 s budget and the large pull pays for
    /// the sharing too; serial wins on both counts. The deferred batch tick fires as soon as the
    /// lane releases the trunk (`MissedTickBehavior::Delay`).
    large_in_flight: bool,
    last_peer_lookup_warn: Option<std::time::Instant>,
    last_fallback_warn: Option<std::time::Instant>,
}

type SharedState = Arc<parking_lot::Mutex<Shared>>;

/// Spawn both follower loops.
pub fn spawn(cloud: Arc<CloudState>) {
    let shared: SharedState = Arc::default();
    let interval = Duration::from_secs(env_u64("HIVE_STORE_SYNC_SECS", 60));
    tracing::info!(?interval, "store follower sync (batch + large-store lane)");
    {
        let (cloud, shared) = (cloud.clone(), shared.clone());
        crate::supervise::spawn_supervised("store-follower-sync", move || {
            let (cloud, shared) = (cloud.clone(), shared.clone());
            async move {
                let mut tick = tokio::time::interval(interval);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    crate::supervise::beat("store-follower-sync");
                    if shared.lock().large_in_flight {
                        continue;
                    }
                    if let Some(target) = resolve_target(&cloud, &shared, true) {
                        pull_batch(&cloud, &shared, &target).await;
                    }
                }
            }
        });
    }
    crate::supervise::spawn_supervised("store-large-lane", move || {
        let (cloud, shared) = (cloud.clone(), shared.clone());
        async move {
            let mut tick = tokio::time::interval(LARGE_LANE_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                crate::supervise::beat("store-large-lane");
                if shared.lock().link_down {
                    continue;
                }
                if let Some(target) = resolve_target(&cloud, &shared, false) {
                    pull_large(&cloud, &shared, &target).await;
                }
            }
        }
    });
}

/// The owner this tick pulls from.
struct Target {
    leader: String,
    wholesale_ok: bool,
    peer_id: String,
    peer_addr: String,
}

impl Target {
    fn pulls(&self, store: &SyncedStore) -> bool {
        self.wholesale_ok || store_sync::MERGE_STORES.contains(&store.name)
    }

    /// A pull can take minutes: before it and again before adopting, the owner must still be the
    /// one in view AND still entitled to this store's adoption, so a deposed leader's snapshot is
    /// never adopted late.
    fn still_permitted(&self, cloud: &CloudState, store: &SyncedStore) -> bool {
        cloud
            .control_plane_leader_with_source()
            .is_some_and(|(owner, source)| {
                owner == self.leader
                    && (store_sync::MERGE_STORES.contains(&store.name)
                        || source.may_adopt_wholesale_from(
                            &owner,
                            &crate::cluster::Cluster::owner_chain_from_env(),
                        ))
            })
    }
}

/// Pulls from the owner when it is ANOTHER node. This node being the owner (acting, or still inside
/// the leader-writes tenure/quorum gate) pulls from nobody, and `report` (the batch loop, which
/// owns the WARNs and the retarget) then closes what was recorded against the leader it used to
/// follow; no resolvable owner (a configured chain dark in this view) pulls from nobody either, the
/// resolver having already WARNed the HOLD.
///
/// Wholesale replace is correct ONLY from an owner every node agrees on
/// (`OwnerSource::may_adopt_wholesale_from`): the chain HEAD (with differing per-node chains a
/// backup is the owner only in some observers' views, so a node resolving a backup whose own chain
/// never names itself would adopt that backup's stale fork) or the identity election on a
/// chain-less mesh. A FALLBACK forwarding target is never an authority. From anyone else only the
/// per-row MERGE stores are pulled.
fn resolve_target(cloud: &CloudState, shared: &SharedState, report: bool) -> Option<Target> {
    let resolved = cloud.control_plane_leader_with_source();
    if resolved.as_ref().is_some_and(|(owner, _)| *owner == cloud.node_name) {
        if report {
            shared.lock().failures.retarget(cloud, None);
        }
        return None;
    }
    let (leader, source) = resolved?;
    if cloud.mesh_health().isolated {
        return None;
    }
    let wholesale_ok =
        source.may_adopt_wholesale_from(&leader, &crate::cluster::Cluster::owner_chain_from_env());
    let due = |at: &mut Option<std::time::Instant>| {
        let due = at.is_none_or(|t| t.elapsed() >= Duration::from_secs(300));
        if due {
            *at = Some(std::time::Instant::now());
        }
        due
    };
    if report && !wholesale_ok && due(&mut shared.lock().last_fallback_warn) {
        tracing::warn!(
            leader = %leader,
            source = ?source,
            "store follower-sync: the resolved owner is not the configured chain's head (a \
             backup owner or a fallback forwarding target) -- pulling merge stores only, \
             adopting nothing wholesale until the head is present"
        );
    }
    let peer = cloud.registry.nodes().into_iter().find(|n| {
        n.name == leader && !n.is_self && n.healthy && n.peer_id.is_some() && n.iroh_addr.is_some()
    });
    let Some(peer) = peer else {
        if report && due(&mut shared.lock().last_peer_lookup_warn) {
            tracing::warn!(
                leader = %leader,
                "store follower-sync: no healthy, addressable registry entry for the \
                 control-plane leader -- this node cannot pull store_sync::REGISTRY snapshots \
                 and its local copies (projects/teams/billing/etc.) will silently drift stale \
                 until this resolves"
            );
        }
        return None;
    };
    Some(Target {
        leader,
        wholesale_ok,
        peer_id: peer.peer_id?,
        peer_addr: peer.iroh_addr?,
    })
}

/// Adopts `bytes` for `store` when they differ from the local snapshot. The change-gate is a raw
/// byte comparison: `snapshot` is deterministic, so equal bytes mean no change. Empties are skipped
/// (an old leader without this arm returns `[]`). Returns whether the store adopted.
fn adopt(
    cloud: &Arc<CloudState>,
    target: &Target,
    store: &SyncedStore,
    local: &[u8],
    bytes: &[u8],
    elapsed: Duration,
) -> bool {
    if bytes.is_empty() || bytes == local {
        return false;
    }
    let Some(n) = (store.adopt)(cloud, bytes) else {
        return false;
    };
    tracing::info!(
        leader = %target.leader,
        store = store.name,
        count = n,
        bytes = bytes.len(),
        elapsed_ms = elapsed.as_millis() as u64,
        "store follower-sync: adopted the leader's snapshot"
    );
    true
}

/// One batch round: every small store fetched concurrently, adopted after (`adopt` is synchronous
/// and takes store locks, so it runs in a plain loop after the join).
///
/// A whole CLASS of stores (teams, incidents, apikeys, webhooks, databases, domains, integrations,
/// gitops, docs, notifications, identity, enterprise) take mutations only on the leader (the
/// admin_ingress forward) but serve GETs from the local store, so a follower's copy otherwise
/// diverges forever. The merge stores (`store_sync::MERGE_STORES`) are written on whichever node
/// the request reached, so a wholesale replace would silently drop every record admitted through
/// another node: each one's own `adopt` merges per key, and must not be 'fixed' to replace again.
/// Every `adopt` declines an empty or unparsable payload, so an unreachable or booting leader can
/// never wipe a follower.
///
/// A store not fetched here goes to the large lane when proven large or suspect. A node without a
/// large lane (see `pulls_large_stores`) skips only stores PROVEN large by size, because a suspect
/// mark is cleared solely by a large-lane pull, so on such a node it would park a small store for
/// the life of the process. A fetch that ran out the WHOLE budget while the trunk carried other
/// stores marks the store suspect (the one batch failure that suggests size). A merge changes local
/// state, recovered permanent tombstones included, so persistence is queued at once instead of
/// leaving a crash-loss window until the unrelated periodic capture.
async fn pull_batch(cloud: &Arc<CloudState>, shared: &SharedState, target: &Target) {
    use futures::StreamExt as _;
    let large_lane = pulls_large_stores(cloud);
    let mut small: Vec<(&'static SyncedStore, Vec<u8>, usize)> = Vec::new();
    for store in store_sync::REGISTRY.iter().filter(|s| target.pulls(s)) {
        let local = (store.snapshot)(cloud);
        let mut sh = shared.lock();
        sh.lanes.observe_local(store.name, local.len());
        let skip = if large_lane {
            sh.lanes.in_lane(store.name)
        } else {
            sh.lanes.proven_large(store.name)
        };
        if skip {
            continue;
        }
        drop(sh);
        let hint = store_sync::size_hint(store.name, local.len());
        small.push((store, local, hint));
    }
    let futs: Vec<_> = small
        .into_iter()
        .map(|(store, local, hint)| {
            let (cloud, peer_id, peer_addr) =
                (cloud.clone(), target.peer_id.clone(), target.peer_addr.clone());
            async move {
                let started = std::time::Instant::now();
                let bytes =
                    store_sync::fetch_snapshot(&cloud, &peer_id, &peer_addr, store, hint).await;
                (store, local, bytes, started.elapsed())
            }
        })
        .collect();
    let fetched = futures::stream::iter(futs)
        .buffer_unordered(env_u64("HIVE_STORE_SYNC_CONCURRENCY", 8) as usize)
        .collect::<Vec<_>>()
        .await;
    let outcomes: Vec<(&'static str, bool, Duration)> = fetched
        .iter()
        .map(|(store, _, bytes, elapsed)| (store.name, bytes.is_some(), *elapsed))
        .collect();
    let link_down = outcomes.len() > 1 && outcomes.iter().all(|(_, ok, _)| !ok);
    let budget = Duration::from_secs(store_sync::fetch_budget(0).0);
    {
        let mut sh = shared.lock();
        sh.link_down = link_down;
        for &(store, ok, elapsed) in &outcomes {
            if large_lane && !ok && !link_down && elapsed + Duration::from_millis(500) >= budget {
                sh.lanes.suspect(store);
            }
        }
    }
    let mut adopted = false;
    for (store, local, bytes, elapsed) in fetched {
        if let Some(bytes) = bytes {
            adopted |= adopt(cloud, target, store, &local, &bytes, elapsed);
        }
    }
    shared
        .lock()
        .failures
        .tick(cloud, &target.leader, &outcomes);
    if adopted {
        crate::persist::persist(cloud);
    }
}

/// One large-lane round: every due large store, one at a time.
/// A completed multi-MB pull proves the link even when the batch has not run for minutes (it defers
/// to this lane), so it clears a stale `link_down` that would otherwise park the lane behind it.
async fn pull_large(cloud: &Arc<CloudState>, shared: &SharedState, target: &Target) {
    if !pulls_large_stores(cloud) {
        static NOTED: std::sync::Once = std::sync::Once::new();
        NOTED.call_once(|| {
            tracing::info!(
                "store large-lane: this node runs the mock cell backend and is never a public \
                 ingress -- not pulling large stores (billing/incidents/audit) from the owner; \
                 small stores still replicate through the batch (HIVE_STORE_SYNC_LARGE=1 forces on)"
            );
        });
        return;
    }
    let due: Vec<(&'static SyncedStore, usize)> = {
        let sh = shared.lock();
        store_sync::REGISTRY
            .iter()
            .filter(|s| target.pulls(s))
            .filter(|s| sh.lanes.in_lane(s.name) && sh.lanes.due(s.name))
            .map(|s| (s, sh.lanes.hint(s.name)))
            .collect()
    };
    let mut adopted = false;
    for (store, hint) in due {
        crate::supervise::beat("store-large-lane");
        if shared.lock().link_down || !target.still_permitted(cloud, store) {
            break;
        }
        let started = std::time::Instant::now();
        shared.lock().large_in_flight = true;
        let bytes =
            store_sync::fetch_snapshot(cloud, &target.peer_id, &target.peer_addr, store, hint)
                .await;
        let elapsed = started.elapsed();
        {
            let mut sh = shared.lock();
            sh.large_in_flight = false;
            if bytes.is_some() {
                sh.link_down = false;
            }
            sh.lanes.pulled(store.name, bytes.as_ref().map(Vec::len));
            sh.failures
                .record_lane(cloud, &target.leader, store.name, bytes.is_some(), elapsed);
        }
        let Some(bytes) = bytes else {
            continue;
        };
        if !target.still_permitted(cloud, store) {
            continue;
        }
        let local = (store.snapshot)(cloud);
        adopted |= adopt(cloud, target, store, &local, &bytes, elapsed);
    }
    if adopted {
        crate::persist::persist(cloud);
    }
}

/// Which lane each store's follower pull takes (module doc). Process-local:
/// a fresh process re-learns sizes on its first pulls.
#[derive(Default)]
struct StoreLanes {
    stores: std::collections::HashMap<&'static str, StoreLane>,
}

#[derive(Default)]
struct StoreLane {
    /// The local snapshot's size at the batch loop's last look.
    local_len: usize,
    /// Ran out the batch budget while other stores fetched; size unknown.
    suspect: bool,
    /// When the lane may pull this store again (`None` = now).
    next_pull: Option<std::time::Instant>,
    /// Consecutive failed lane pulls; reset by a success. Drives the retry
    /// backoff in [`StoreLanes::pulled`].
    failures: u32,
}

impl StoreLanes {
    fn observe_local(&mut self, store: &'static str, len: usize) {
        self.stores.entry(store).or_default().local_len = len;
    }

    /// The size to plan this store's pull for (`store_sync::size_hint`); a
    /// suspect store of unknown size is planned as large.
    fn hint(&self, store: &str) -> usize {
        let lane = self.stores.get(store);
        let hint = store_sync::size_hint(store, lane.map_or(0, |lane| lane.local_len));
        if lane.is_some_and(|lane| lane.suspect) {
            hint.max(store_sync::LARGE_STORE_BYTES + 1)
        } else {
            hint
        }
    }

    /// Whether the large lane owns this store: proven large or suspect.
    fn in_lane(&self, store: &str) -> bool {
        self.hint(store) > store_sync::LARGE_STORE_BYTES
    }

    /// Proven large by a measured size (the local copy or the last snapshot a
    /// peer served), ignoring any suspect mark.
    fn proven_large(&self, store: &str) -> bool {
        let local_len = self.stores.get(store).map_or(0, |lane| lane.local_len);
        store_sync::size_hint(store, local_len) > store_sync::LARGE_STORE_BYTES
    }

    fn due(&self, store: &str) -> bool {
        self.stores
            .get(store)
            .and_then(|lane| lane.next_pull)
            .is_none_or(|at| std::time::Instant::now() >= at)
    }

    fn suspect(&mut self, store: &'static str) {
        let lane = self.stores.entry(store).or_default();
        if !lane.suspect {
            lane.suspect = true;
            lane.next_pull = None;
        }
    }

    /// A lane pull finished; `len` is the fetched size when it succeeded. Success settles the size
    /// (`store_sync` recorded it; a small store returns to the batch) and starts the
    /// `LARGE_STORE_PULL_EVERY` floor; failure retries after [`LARGE_STORE_RETRY`], doubling per
    /// consecutive failure.
    fn pulled(&mut self, store: &'static str, len: Option<usize>) {
        let lane = self.stores.entry(store).or_default();
        let now = std::time::Instant::now();
        match len {
            Some(_) => {
                lane.suspect = false;
                lane.failures = 0;
                lane.next_pull = Some(now + LARGE_STORE_PULL_EVERY);
            }
            None => {
                lane.failures = lane.failures.saturating_add(1);
                let doublings = lane.failures.saturating_sub(1).min(LARGE_STORE_RETRY_DOUBLINGS);
                let wait = (LARGE_STORE_RETRY * 2u32.pow(doublings)).min(LARGE_STORE_PULL_EVERY);
                lane.next_pull = Some(now + wait);
            }
        }
    }
}

/// Whether this node pulls LARGE stores at all. A laptop-class node (one running the mock cell
/// backend: every macOS node and every dev node behind a home NAT) is never a public ingress and
/// serves no tenant traffic, so it has no use for the multi-megabyte billing, incidents and audit
/// snapshots, and several of them behind one residential NAT can eat the owner's capped public
/// egress and slow every cloud follower's pull of the same stores; it still receives every small
/// store through the batch. Deciding here, at the one place the lane decides to pull, fixes every
/// such node at once. `HIVE_STORE_SYNC_LARGE=1` forces the lane on (a mock node that must hold
/// billing for local development), `0` forces it off.
fn pulls_large_stores(cloud: &CloudState) -> bool {
    match std::env::var("HIVE_STORE_SYNC_LARGE").ok().as_deref() {
        Some("1") => return true,
        Some("0") => return false,
        _ => {}
    }
    let mock = cloud
        .registry
        .nodes()
        .iter()
        .find(|n| n.is_self)
        .is_some_and(|n| n.backend == "mock");
    !mock
}

/// Per-store accounting for the follower pull. Every failed snapshot fetch counts (it used to be a
/// silent `continue`, so a store that never fit its budget simply never replicated and nothing said
/// so), for the CURRENT leader only:
///
/// * a WARN names the leader, the store and how long the attempt took (at most once per 5 min per
///   store), and every [`STORE_PULL_INCIDENT_AFTER`] consecutive failures open (deduped by
///   `IncidentStore::open`) one incident per store, resolved on the next successful fetch;
/// * a batch where EVERY store failed is the leader link being down, not N store faults: one WARN
///   and one 'every store' incident instead of one per store, resolved as soon as any store fetches
///   again;
/// * a change of leader, or this node becoming the owner, resolves everything recorded against the
///   previous leader: those incidents describe a link this node no longer uses, and an owner that
///   stops pulling would otherwise replicate them to every follower forever.
///
/// The incidents live in this follower's own store, which the next wholesale adoption of
/// `incidents` replaces; they are re-asserted on the streak cadence with a 30 min TTL (two missed
/// large-lane rounds lapse it), and the WARN lines are the durable signal on a follower.
#[derive(Default)]
struct StorePullFailures {
    /// The leader the recorded streaks are against.
    leader: Option<String>,
    /// Per store; [`ALL_STORES`] is the whole-link streak.
    stores: std::collections::HashMap<&'static str, StorePullFailure>,
}

#[derive(Default)]
struct StorePullFailure {
    consecutive: u32,
    last_warn: Option<std::time::Instant>,
    incident: Option<String>,
}

const STORE_PULL_INCIDENT_AFTER: u32 = 10;
const ALL_STORES: &str = "*";
const LINK_RECOVERED: &str =
    "The leader link recovered: at least one store snapshot fetched again.";

impl StorePullFailure {
    fn warn_due(&mut self) -> bool {
        let due = self
            .last_warn
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(300));
        if due {
            self.last_warn = Some(std::time::Instant::now());
        }
        due
    }

    fn resolve(&mut self, cloud: &CloudState, message: &str) {
        self.consecutive = 0;
        if let Some(id) = self.incident.take() {
            cloud.incidents.update(
                &id,
                crate::incidents::UpdateReq {
                    status: crate::incidents::IncidentStatus::Resolved,
                    message: message.into(),
                },
            );
        }
    }
}

impl StorePullFailures {
    /// This node is about to pull from `leader`, or (`None`) no longer pulls at all because it is
    /// the owner. Anything recorded against a different leader is closed.
    fn retarget(&mut self, cloud: &CloudState, leader: Option<&str>) {
        if self.leader.as_deref() == leader {
            return;
        }
        if let Some(previous) = self.leader.take() {
            let message = match leader {
                Some(next) => format!(
                    "No longer pulling from {previous}: the control-plane leader in this node's \
                     view is now {next}."
                ),
                None => format!(
                    "No longer pulling from {previous}: this node is the control-plane owner."
                ),
            };
            for failure in self.stores.values_mut() {
                failure.resolve(cloud, &message);
            }
        }
        self.stores.clear();
        self.leader = leader.map(str::to_string);
    }

    /// Fold one batch's fetch outcomes (`store`, fetched?, elapsed).
    fn tick(
        &mut self,
        cloud: &CloudState,
        leader: &str,
        outcomes: &[(&'static str, bool, Duration)],
    ) {
        self.retarget(cloud, Some(leader));
        let failed = outcomes.iter().filter(|(_, ok, _)| !ok).count();
        let link_down = outcomes.len() > 1 && failed == outcomes.len();
        for &(store, ok, elapsed) in outcomes {
            self.record(cloud, leader, store, ok, elapsed, link_down);
        }
        let link = self.stores.entry(ALL_STORES).or_default();
        if !link_down {
            link.resolve(cloud, LINK_RECOVERED);
            return;
        }
        link.consecutive = link.consecutive.saturating_add(1);
        if link.warn_due() {
            tracing::warn!(
                leader,
                stores = outcomes.len(),
                consecutive_ticks = link.consecutive,
                "store follower-sync: EVERY store snapshot fetch from the leader failed this tick \
                 -- nothing is replicating to this node"
            );
        }
        if link.consecutive % STORE_PULL_INCIDENT_AFTER == 0 {
            let elapsed = outcomes.iter().map(|o| o.2).max().unwrap_or_default();
            let incident = open_pull_incident(cloud, leader, ALL_STORES, link.consecutive, elapsed);
            link.incident = Some(incident);
        }
    }

    /// One large-lane pull's outcome. A success also proves the link.
    fn record_lane(
        &mut self,
        cloud: &CloudState,
        leader: &str,
        store: &'static str,
        ok: bool,
        elapsed: Duration,
    ) {
        self.retarget(cloud, Some(leader));
        self.record(cloud, leader, store, ok, elapsed, false);
        if ok {
            self.stores
                .entry(ALL_STORES)
                .or_default()
                .resolve(cloud, LINK_RECOVERED);
        }
    }

    /// One store's outcome; `link_down` defers its report to the link line.
    fn record(
        &mut self,
        cloud: &CloudState,
        leader: &str,
        store: &'static str,
        ok: bool,
        elapsed: Duration,
        link_down: bool,
    ) {
        let f = self.stores.entry(store).or_default();
        if ok {
            if f.consecutive > 0 {
                tracing::info!(
                    store,
                    after_failures = f.consecutive,
                    "store follower-sync: snapshot fetch recovered"
                );
            }
            f.resolve(
                cloud,
                "Replication recovered: the snapshot fetched successfully again.",
            );
            return;
        }
        f.consecutive = f.consecutive.saturating_add(1);
        if link_down {
            return;
        }
        if f.warn_due() {
            tracing::warn!(
                leader,
                store,
                elapsed_ms = elapsed.as_millis() as u64,
                consecutive_failures = f.consecutive,
                "store follower-sync: snapshot fetch from the leader FAILED -- this store is \
                 not replicating to this node"
            );
        }
        if f.consecutive % STORE_PULL_INCIDENT_AFTER == 0 {
            let incident = open_pull_incident(cloud, leader, store, f.consecutive, elapsed);
            f.incident = Some(incident);
        }
    }
}

fn open_pull_incident(
    cloud: &CloudState,
    leader: &str,
    store: &str,
    consecutive: u32,
    elapsed: Duration,
) -> String {
    let what = if store == ALL_STORES {
        "every store".to_string()
    } else {
        store.to_string()
    };
    cloud
        .incidents
        .open(crate::incidents::OpenReq {
            condition: format!("store-pull:{leader}:{store}"),
            ttl_ms: 30 * 60 * 1000,
            title: format!("store replication from {leader} failing: {what}"),
            severity: crate::incidents::Severity::Minor,
            affected: vec![cloud.node_name.clone(), format!("store:{store}")],
            message: format!(
                "{} has failed {consecutive} consecutive pulls of {what} from the control-plane \
                 leader {leader} (last attempt {} ms). Its local copy is stale until this clears, \
                 and reads this node serves from it are stale with it.",
                cloud.node_name,
                elapsed.as_millis()
            ),
        })
        .id
}
