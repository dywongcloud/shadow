//! Generic leader-to-follower store replication.
//!
//! A whole CLASS of `CloudState` stores are node-local: their mutations (POST/PUT/DELETE) forward
//! to the control-plane leader via `admin_ingress`, but their GET handlers serve the LOCAL store,
//! so under multi-A DNS a dashboard read lands on a random node that never saw the leader's writes
//! and returns empty or stale data. This module is ONE mechanism for all of them: a [`REGISTRY`] of
//! [`SyncedStore`] entries, each a `(name, snapshot, adopt)` triple. The gossip layer exposes every
//! entry at `GET /v1/store-snapshot/<name>` (see `gossip::dispatch`), and the follower pull
//! (`crate::store_follower`) iterates the registry every tick, pulls each store's snapshot from the
//! leader through [`fetch_snapshot`] (the one size-aware transfer policy) and adopts it when it
//! differs.
//!
//! Contract: `snapshot` must produce DETERMINISTIC bytes for equal state (maps serialized via
//! sorted `BTreeMap`/pre-sorted `Vec`), because the follower's change-gate is a raw byte comparison
//! of the leader's bytes against its own `snapshot` bytes (no `PartialEq` on the payload types
//! needed). `adopt` returns `Some(count)` when it actually loaded new state (for the log line) and
//! `None` when it declined (empty or unparsable: never wipe a follower on a momentarily unreachable
//! or booting leader).
//!
//! Scope: only stores whose full contents are safe to replicate wholesale under the single-writer
//! model are members (the registry's own doc lists the exceptions and why each store is in).
//! Secret-bearing members (apikeys hashes, database credentials, enterprise SAML/SCIM secrets) ride
//! the peer-trust-enforced, signed gossip mesh, the same transport TLS bundles, billing and zkauth
//! rosters already replicate over.

use crate::state::CloudState;
use std::collections::HashMap;
use std::sync::Arc;

/// Wire path prefix every registry store is served under (`gossip::dispatch` matches it,
/// [`fetch_snapshot`] builds it). It is also the ONE gossip path whose bulk responses must NOT be
/// deprioritized on the serving trunk (`hive_p2p::serve_tunnels_full` takes it as `bulk_exempt`): a
/// snapshot is multi-MB by design (billing 6.3 MB, incidents 10.5 MB) and the follower is BLOCKED
/// on it, so queueing it below every default-priority stream (probes, tunnels, the follower's own
/// batch) starves it indefinitely: measured live, billing ran out its whole 240 s budget at under
/// 26 KB/s while a 223,637-byte snapshot one step under `hive_p2p`'s bulk threshold finished in 2.8
/// s on the same trunk at the same instant. Every other bulk reply (browser artifacts, TLS bundles,
/// drive files) is still deprioritized.
pub const SNAPSHOT_PATH_PREFIX: &str = "/v1/store-snapshot/";

/// One replicated store: its wire name plus serialize/adopt function pointers.
pub struct SyncedStore {
    pub name: &'static str,
    /// Deterministic serialized snapshot of the local store.
    pub snapshot: fn(&Arc<CloudState>) -> Vec<u8>,
    /// Deserialize `bytes` and load into the local store. Returns the adopted
    /// element count on success, `None` if it declined (unparsable).
    pub adopt: fn(&Arc<CloudState>, &[u8]) -> Option<usize>,
}

/// Serialize any value to CANONICAL JSON bytes, empty on failure (an
/// unserializable snapshot degrades to "nothing to sync", never a panic).
/// Routing through `serde_json::Value` (BTreeMap-backed here — no
/// `preserve_order` feature) re-serializes every map with SORTED keys, so a
/// struct carrying a nested `HashMap` still produces identical bytes
/// process-to-process — required for the follower's byte-compare change-gate.
fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_value(v)
        .ok()
        .and_then(|val| serde_json::to_vec(&val).ok())
        .unwrap_or_default()
}

/// Deterministic-order serialization for a Vec whose store is HASHMAP-backed (`snapshot()` collects
/// `.values()` in nondeterministic hash order, which differs process to process): without it the
/// follower's byte-compare change-gate never matches the leader's differently ordered bytes and it
/// re-adopts identical data every tick (wasteful load plus a misleading "adopted" log line every 60
/// s). Each element is canonicalized through `serde_json::Value` FIRST (this crate builds
/// serde_json WITHOUT `preserve_order`, so `Value::Object` is BTreeMap-backed and re-serializes
/// with SORTED keys, which makes a nested map such as `Database.connection: HashMap` deterministic
/// too) and the elements are then sorted by their canonical string, so both sides produce identical
/// bytes for identical state without needing to know the element's key field. Display order is
/// unaffected: every read handler is team-scoped and the dashboard sorts client-side.
fn enc_sorted<T: serde::Serialize>(v: Vec<T>) -> Vec<u8> {
    let mut rows: Vec<String> = v
        .iter()
        .map(|e| {
            serde_json::to_value(e)
                .ok()
                .and_then(|val| serde_json::to_string(&val).ok())
                .unwrap_or_default()
        })
        .collect();
    rows.sort();
    let joined = format!("[{}]", rows.join(","));
    joined.into_bytes()
}

/// The replicated stores. `snapshot` is deterministic: [`enc`] for stores already ordered by
/// construction (sorted Vecs, BTreeMaps, the audit buffer's insertion order, which is identical
/// once a follower adopts the leader's), [`enc_sorted`] for HashMap-backed ones. `adopt` DECLINES
/// (`None`) an empty or unparsable payload: a leader that has not loaded yet is indistinguishable
/// from one that genuinely has nothing, and adopting it would wipe the follower (a leader with zero
/// billing accounts would erase every tenant's tier and balance fleet-wide). Entries with no
/// natural count (`enterprise`, `bot_policy`, `ratelimit`) report 1 to signal that a load happened;
/// the follower loop's outer byte-compare guarantees that only fires on a real change.
///
/// Exceptions: `browser_admissions` and `browser_presence` treat an empty active set as
/// authoritative (tombstones plus a monotonic version prove a revocation, or "zero satellites now",
/// not a boot-time read failure; adoption also reconciles Gateway and BrowserPool). `teams`,
/// `projects`, `production_deployments`, `databases` and `queues` MERGE row by row with tombstones
/// instead of replacing, and carry deletions explicitly: `databases` once adopted the leader's list
/// wholesale (guarded only against an empty payload), which made "the leader has not got this
/// record yet" and "this record was deleted" the same event, so a leader OOM-killed before its
/// debounced save restarted without a record and every follower erased its own copy; wholesale
/// replacement of `projects` is how one node's row loss became "the project vanished from the
/// account fleet-wide within 60 s". A legacy bare payload from a pre-upgrade node is still
/// accepted, tombstone-less, and a pre-upgrade node receiving the new envelope fails to parse and
/// keeps its own state, the safe direction: a replicated store must not be able to destroy data by
/// omission. `queues` replicates queue/consumer METADATA only (messages never ride this: node-local
/// plus GuardianDB-mirrored).
///
/// Why each is a member: `billing` is replicated so the two halves of a tenant's tier (`teams.plan`
/// and `billing.plan`) converge by the same leader-pull mechanism instead of relying on a read-path
/// patch (`proxy_billing_read` falls back to the node's own stale row on failure, and every
/// INTERNAL consumer, `can_deploy`, quota checks and metering, reads the local row
/// unconditionally). `projects` is replicated because a control-plane failover to a peer that never
/// built a project locally would serve that project's settings/env/domains as empty defaults until
/// it happened to receive a write for it. `waf`, `router`, `bot_policy` and `ratelimit` replicate
/// edge-enforcement config, so EVERY node enforces identically instead of a follower applying only
/// its own locally authored rules (`PUT /v1/ratelimit` once configured only the leader's in-process
/// atomics while the Network page read `enabled: false` from everyone else: a security control that
/// reports as configured but is not); `ratelimit` syncs only the CONFIG (see [`RateLimitConfig`]).
/// Cron is deliberately NOT a member: its config replication is coupled to gating execution to a
/// single node, tracked separately. `acme_challenges` carries the DNS-01 TXT for Seer-answered
/// (self-delegated) zones, because Let's Encrypt resolves the zone through ANY advertised
/// nameserver node, so replication is what lets every follower's Seer answer; post-issuance cleanup
/// empties the leader's map, which adoption declines, and a follower's stale copy stops answering
/// through the store's own lookup TTL (see `AcmeChallengeStore::lookup`). `acme_http01` does the
/// same for custom tenant domains, since a validation fetch can land on ANY node.
pub static REGISTRY: &[SyncedStore] = &[
    SyncedStore {
        name: "browser_admissions",
        snapshot: crate::browser_admission::snapshot_bytes,
        adopt: crate::browser_admission::adopt_snapshot,
    },
    SyncedStore {
        name: "browser_presence",
        snapshot: crate::browser_presence::snapshot_bytes,
        adopt: crate::browser_presence::adopt_snapshot,
    },
    SyncedStore {
        name: "teams",
        snapshot: |c| enc(&c.teams.snapshot_synced()),
        adopt: |c, b| {
            let synced: crate::teams::SyncedTeams =
                serde_json::from_slice(b).ok().or_else(|| {
                    let rows: std::collections::BTreeMap<String, crate::teams::Team> =
                        serde_json::from_slice(b).ok()?;
                    Some(crate::teams::SyncedTeams {
                        rows,
                        tombstones: Default::default(),
                    })
                })?;
            if synced.rows.is_empty() && synced.tombstones.is_empty() {
                return None;
            }
            Some(c.teams.merge_synced(synced))
        },
    },
    SyncedStore {
        name: "billing",
        snapshot: |c| {
            let (accounts, ledger) = c.billing.snapshot();
            let by_tenant: std::collections::BTreeMap<String, crate::billing::BillingAccount> =
                accounts
                    .into_iter()
                    .map(|a| (a.tenant.clone(), a))
                    .collect();
            enc(&(by_tenant, ledger))
        },
        adopt: |c, b| {
            let (by_tenant, ledger): (
                std::collections::BTreeMap<String, crate::billing::BillingAccount>,
                Vec<crate::billing::LedgerEntry>,
            ) = serde_json::from_slice(b).ok()?;
            if by_tenant.is_empty() {
                return None;
            }
            let n = by_tenant.len();
            c.billing.load(by_tenant.into_values().collect(), ledger);
            Some(n)
        },
    },
    SyncedStore {
        name: "projects",
        snapshot: |c| enc(&c.projects.snapshot_synced()),
        adopt: |c, b| {
            let synced: crate::project_settings::SyncedProjects =
                serde_json::from_slice(b).ok().or_else(|| {
                    let m: std::collections::BTreeMap<
                        String,
                        crate::project_settings::ProjectSettings,
                    > = serde_json::from_slice(b).ok()?;
                    Some(crate::project_settings::SyncedProjects {
                        rows: m,
                        tombstones: Default::default(),
                        rename_tombstones: Default::default(),
                        incarnation_tombstones: Default::default(),
                    })
                })?;
            if synced.rows.is_empty()
                && synced.tombstones.is_empty()
                && synced.rename_tombstones.is_empty()
                && synced.incarnation_tombstones.is_empty()
            {
                return None;
            }
            Some(c.projects.merge_synced(synced))
        },
    },
    SyncedStore {
        name: "production_deployments",
        snapshot: |c| enc(&c.production_deployments.snapshot_synced()),
        adopt: |c, b| {
            let synced: crate::production_deployments::SyncedProductionDeployments =
                serde_json::from_slice(b).ok()?;
            if synced.rows.is_empty() && synced.tombstones.is_empty() {
                return None;
            }
            Some(c.production_deployments.merge_synced(synced))
        },
    },
    SyncedStore {
        name: "incidents",
        snapshot: |c| enc(&c.incidents.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::incidents::Incident> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.incidents.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "push",
        snapshot: |c| enc(&c.push.snapshot()),
        adopt: |c, b| {
            let v: crate::push::PushState = serde_json::from_slice(b).ok()?;
            if v.subs.is_empty() && v.sms.is_empty() && v.vapid.public_b64.is_empty() {
                return None;
            }
            let n = v.subs.len() + v.sms.len();
            c.push.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "apikeys",
        snapshot: |c| enc_sorted(c.apikeys.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::apikeys::ApiKey> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.apikeys.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "webhooks",
        snapshot: |c| enc_sorted(c.webhooks.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::webhooks::Webhook> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.webhooks.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "databases",
        snapshot: |c| enc(&c.databases.snapshot_synced()),
        adopt: |c, b| {
            let synced: crate::databases::SyncedDatabases =
                serde_json::from_slice(b).ok().or_else(|| {
                    let dbs: Vec<crate::databases::Database> = serde_json::from_slice(b).ok()?;
                    Some(crate::databases::SyncedDatabases {
                        dbs,
                        tombstones: Default::default(),
                        studio_replay: Default::default(),
                    })
                })?;
            if synced.dbs.is_empty()
                && synced.tombstones.is_empty()
                && synced.studio_replay.is_empty()
            {
                return None;
            }
            Some(c.databases.merge_synced(synced))
        },
    },
    SyncedStore {
        name: "queues",
        snapshot: |c| enc(&c.queues.snapshot_synced()),
        adopt: |c, b| {
            let synced: crate::queues::SyncedQueues = serde_json::from_slice(b).ok()?;
            if synced.queues.is_empty()
                && synced.consumers.is_empty()
                && synced.queue_tombstones.is_empty()
                && synced.consumer_tombstones.is_empty()
            {
                return None;
            }
            Some(c.queues.merge_synced(synced))
        },
    },
    SyncedStore {
        name: "domains",
        snapshot: |c| enc_sorted(c.domains.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::dns::DomainRecord> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.domains.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "integrations",
        snapshot: |c| enc_sorted(c.integrations.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::integrations::IntegrationResource> =
                serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.integrations.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "gitops",
        snapshot: |c| enc_sorted(c.gitops.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::gitops::GitOpsLink> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.gitops.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "docs",
        snapshot: |c| enc_sorted(c.docs.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::docstore::Doc> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.docs.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "notifications",
        snapshot: |c| enc(&c.notifications.snapshot()),
        adopt: |c, b| {
            let s: crate::notifications::NotificationState = serde_json::from_slice(b).ok()?;
            if s.archived.is_empty() && s.read.is_empty() {
                return None;
            }
            let n = s.archived.len() + s.read.len();
            c.notifications.load(s);
            Some(n)
        },
    },
    SyncedStore {
        name: "identity",
        snapshot: |c| enc(&c.identity.snapshot()),
        adopt: |c, b| {
            let s: crate::identity::IdentitySnapshot = serde_json::from_slice(b).ok()?;
            if s.orgs.is_empty() && s.users.is_empty() {
                return None;
            }
            let n = s.orgs.len() + s.users.len();
            c.identity.load(s.orgs, s.users);
            Some(n)
        },
    },
    SyncedStore {
        name: "enterprise",
        snapshot: |c| enc(&c.enterprise.snapshot()),
        adopt: |c, b| {
            let s: crate::enterprise::EnterpriseSnapshot = serde_json::from_slice(b).ok()?;
            c.enterprise.load(s);
            Some(1)
        },
    },
    SyncedStore {
        name: "securelinks",
        snapshot: |c| enc_sorted(c.securelinks.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::securelink::LinkRecord> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.securelinks.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "audit",
        snapshot: |c| enc(&c.audit.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::audit::AuditEntry> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.audit.load(v);
            Some(n)
        },
    },
    SyncedStore {
        name: "waf",
        snapshot: |c| {
            enc(&WafConfig {
                rules: c.waf.rules(),
                managed: c.waf.managed_enabled(),
            })
        },
        adopt: |c, b| {
            let cfg: WafConfig = serde_json::from_slice(b).ok()?;
            let n = cfg.rules.len();
            c.waf.set_rules(cfg.rules);
            c.waf.set_managed(cfg.managed);
            Some(n)
        },
    },
    SyncedStore {
        name: "router",
        snapshot: |c| {
            enc(&RouterConfig {
                redirects: c.router.redirects(),
                rewrites: c.router.rewrites(),
            })
        },
        adopt: |c, b| {
            let cfg: RouterConfig = serde_json::from_slice(b).ok()?;
            let n = cfg.redirects.len() + cfg.rewrites.len();
            c.router.set_redirects(cfg.redirects);
            c.router.set_rewrites(cfg.rewrites);
            Some(n)
        },
    },
    SyncedStore {
        name: "bot_policy",
        snapshot: |c| enc(&*c.bot_policy.read()),
        adopt: |c, b| {
            let p: hive_edge::BotPolicy = serde_json::from_slice(b).ok()?;
            *c.bot_policy.write() = p;
            Some(1)
        },
    },
    SyncedStore {
        name: "ratelimit",
        snapshot: |c| {
            let s = c.ratelimit.stats();
            enc(&RateLimitConfig {
                enabled: s.enabled,
                limit: s.limit,
                window_ms: s.window_ms,
            })
        },
        adopt: |c, b| {
            let cfg: RateLimitConfig = serde_json::from_slice(b).ok()?;
            c.ratelimit.set(cfg.enabled, cfg.limit, cfg.window_ms);
            Some(1)
        },
    },
    SyncedStore {
        name: "acme_challenges",
        snapshot: |c| enc(&c.acme_challenges.snapshot()),
        adopt: |c, b| {
            let m: std::collections::BTreeMap<String, crate::acme::AcmeChallenge> =
                serde_json::from_slice(b).ok()?;
            if m.is_empty() {
                return None;
            }
            let n = m.len();
            c.acme_challenges.load(m);
            Some(n)
        },
    },
    SyncedStore {
        name: "acme_http01",
        snapshot: |c| enc(&c.acme_http01.snapshot()),
        adopt: |c, b| {
            let m: std::collections::BTreeMap<String, crate::acme::Http01Challenge> =
                serde_json::from_slice(b).ok()?;
            if m.is_empty() {
                return None;
            }
            let n = m.len();
            c.acme_http01.load(m);
            Some(n)
        },
    },
];

/// The stores [`adopt_on_promotion`] merges from any single peer: their adoption is a per-row merge
/// with tombstones, so a partial or stale peer copy cannot erase local rows. Every other store is
/// wholesale-replace and needs two independent peers to corroborate.
pub static MERGE_STORES: &[&str] = &[
    "browser_admissions",
    "browser_presence",
    "projects",
    "teams",
    "production_deployments",
];

/// Rate-limit config wire shape, deliberately config-only: `tracked_ips` and `blocked_total` are
/// per-node counters, and including them would make every node's snapshot differ on every request
/// and defeat the content-compare that gates a replication write.
#[derive(serde::Serialize, serde::Deserialize)]
struct RateLimitConfig {
    enabled: bool,
    limit: u32,
    window_ms: u64,
}

/// WAF config wire shape (rules + managed-ruleset toggle) — the store exposes
/// these as separate accessors, so the snapshot bundles them.
#[derive(serde::Serialize, serde::Deserialize)]
struct WafConfig {
    rules: Vec<hive_edge::WafRule>,
    managed: bool,
}

/// Router config wire shape (redirects + rewrites).
#[derive(serde::Serialize, serde::Deserialize)]
struct RouterConfig {
    redirects: Vec<hive_edge::Redirect>,
    rewrites: Vec<hive_edge::Rewrite>,
}

/// Serve the named store's snapshot, or an empty vec for an unknown name
/// (an older peer requesting a store this build doesn't expose — safe skip).
pub fn serve(cloud: &Arc<CloudState>, name: &str) -> Vec<u8> {
    REGISTRY
        .iter()
        .find(|s| s.name == name)
        .map(|s| (s.snapshot)(cloud))
        .unwrap_or_default()
}

/// Snapshot size at which a store is LARGE: it leaves the follower pull's
/// concurrent batch for the serial lane and gets a size-derived budget.
pub const LARGE_STORE_BYTES: usize = 1 << 20;
/// Budget and response bound for a snapshot not known to be large.
const SMALL_FETCH_SECS: u64 = 10;
const SMALL_RESPONSE_CAP: usize = 16 << 20;
/// Response bound for a large snapshot (the memory ceiling of one pull).
const LARGE_RESPONSE_CAP: usize = 64 << 20;
/// The slowest sustained rate a large budget is sized for. A trunk carrying a snapshot beside a
/// follower's own batch sustains 25-90 KB/s live (64 ms RTT peers), so 32 KiB/s is the rate a
/// budget must assume for the size term to be the one that binds: at 64 KiB/s the DERIVED budget
/// was smaller than the flat floor for every store the fleet has (billing 6.3 MB derived 149 s,
/// incidents 10.5 MB derived 230 s), so `derived.max(floor)` always returned the floor and a 1-12
/// MB store got the same 240 s whether it was 1 MB or 10 MB.
const LARGE_MIN_RATE: usize = 32 << 10;

/// The latest snapshot size of each store any peer served this process.
static REMOTE_BYTES: std::sync::Mutex<Option<HashMap<&'static str, usize>>> =
    std::sync::Mutex::new(None);

fn remote_bytes() -> std::sync::MutexGuard<'static, Option<HashMap<&'static str, usize>>> {
    REMOTE_BYTES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The size to plan `store`'s next fetch for: the larger of the local copy
/// (`local_len`) and the latest snapshot a peer served this process.
pub fn size_hint(store: &str, local_len: usize) -> usize {
    remote_bytes()
        .as_ref()
        .and_then(|sizes| sizes.get(store).copied())
        .unwrap_or(0)
        .max(local_len)
}

/// `(timeout_secs, response_cap)` for a fetch of `hint` bytes: the ONE place that decides how a
/// store snapshot crosses a trunk, for every caller (the follower pull in `store_follower` and
/// `reconcile_on_promotion`). A trunk sustains ~170 KB/s (1200-byte PMTU, 64 ms RTT, measured), so
/// a fixed 10 s budget can never carry billing (6.2 MB) or incidents (10.9 MB), and a fixed long
/// budget still turns growth into a permanent failure once a store outgrows it: the budget is
/// derived from the store's size instead.
///
/// A hint at or below [`LARGE_STORE_BYTES`] gets the small budget. A large one gets the time `hint`
/// plus 25 % growth (capped at the response cap) takes at [`LARGE_MIN_RATE`] plus 30 s, raised to
/// the floor (`HIVE_STORE_SYNC_LARGE_TIMEOUT_SECS`, default 240 s). The floor is only a SAFETY NET
/// for a store whose size is still unknown (a suspect store planned at [`LARGE_STORE_BYTES`]): at
/// [`LARGE_MIN_RATE`] the size term dominates it for every store the fleet has, so a 10.5 MB
/// snapshot is allowed ~430 s and a 6.3 MB one ~272 s instead of both being cut off at 240 s. A
/// flat floor that binds turns "slow" into "permanent failure" and, worse, leaves a multi-MB
/// transfer occupying the trunk for the whole floor before it gives up.
pub fn fetch_budget(hint: usize) -> (u64, usize) {
    if hint <= LARGE_STORE_BYTES {
        return (SMALL_FETCH_SECS, SMALL_RESPONSE_CAP);
    }
    let floor = std::env::var("HIVE_STORE_SYNC_LARGE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(240);
    let planned = hint.saturating_add(hint / 4).min(LARGE_RESPONSE_CAP);
    let derived = (planned / LARGE_MIN_RATE) as u64 + 30;
    (derived.max(floor), LARGE_RESPONSE_CAP)
}

/// Fetch `store`'s snapshot from one peer under the budget its size calls
/// for ([`fetch_budget`] of `hint`, normally [`size_hint`]), recording the
/// size served. `None` on any failure.
pub async fn fetch_snapshot(
    cloud: &Arc<CloudState>,
    peer_id: &str,
    addr: &str,
    store: &SyncedStore,
    hint: usize,
) -> Option<Vec<u8>> {
    let (timeout_secs, response_cap) = fetch_budget(hint);
    let path = format!("{}{}", SNAPSHOT_PATH_PREFIX, store.name);
    let bytes = crate::gossip::request_to_with_response_cap(
        cloud,
        peer_id,
        addr,
        hive_p2p::GOSSIP_GET,
        &path,
        &[],
        timeout_secs,
        response_cap,
    )
    .await?;
    let mut sizes = remote_bytes();
    let seen = sizes.get_or_insert_with(HashMap::new).entry(store.name).or_default();
    *seen = bytes.len();
    Some(bytes)
}

/// Promotion reconciliation: when this node becomes the control-plane owner (the promotion loop in
/// `main.rs` runs it once per ownership term) it pulls every registered store from up to 5 healthy
/// peers and adopts what they hold, so writes the outgoing owner accepted in its trailing sync
/// window are not lost. Five candidates, not three, because wholesale adoption requires 2
/// INDEPENDENT peers to corroborate identical bytes (see [`adopt_on_promotion`]), and a wider
/// candidate set keeps that quorum reachable when one or two queried peers are momentarily slow or
/// unreachable. Each store's local snapshot is taken BEFORE its fetch window: a wholesale store
/// whose local copy changed during the window is not adopted.
///
/// Small stores are fetched all at once. Large ones ([`LARGE_STORE_BYTES`]) are fetched one store
/// at a time, from every peer at once (each peer is its own trunk) under the size-derived budget,
/// and adopted right after their own fetch, so a long transfer never widens any other store's
/// window. Promotion merges are authoritative mutations, so they are queued for persistence
/// immediately: otherwise a hard crash before the periodic capture could lose a newly recovered row
/// or tombstone and reboot into the stale local snapshot.
pub async fn reconcile_on_promotion(cloud: &Arc<CloudState>) {
    let mut peers: Vec<(String, String, String)> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.healthy)
        .filter_map(|n| Some((n.name, n.peer_id?, n.iroh_addr?)))
        .collect();
    peers.truncate(5);
    if peers.is_empty() {
        tracing::warn!(
            "store_sync: promoted to leader with no reachable peers to reconcile against \
             -- starting from local state only; any write the outgoing leader accepted in \
             its trailing sync window, if unpulled by every other peer too, is unrecoverable"
        );
        return;
    }
    let peer_names: Vec<String> = peers.iter().map(|(n, _, _)| n.clone()).collect();

    let before: Vec<Vec<u8>> = REGISTRY.iter().map(|s| (s.snapshot)(cloud)).collect();
    let hints: Vec<usize> = REGISTRY
        .iter()
        .zip(&before)
        .map(|(store, local)| size_hint(store.name, local.len()))
        .collect();
    let fetch_all = |store_idx: usize| {
        let peers = &peers;
        let hint = hints[store_idx];
        async move {
            let store = &REGISTRY[store_idx];
            let fetches = peers.iter().map(|(peer_name, peer_id, addr)| async move {
                let bytes = fetch_snapshot(cloud, peer_id, addr, store, hint).await;
                (peer_name.clone(), bytes)
            });
            futures::future::join_all(fetches)
                .await
                .into_iter()
                .filter_map(|(peer_name, bytes)| {
                    Some((peer_name, bytes.filter(|b| !b.is_empty())?))
                })
                .collect::<Vec<(String, Vec<u8>)>>()
        }
    };

    let mut adopted: Vec<&'static str> = Vec::new();
    let (large, small): (Vec<usize>, Vec<usize>) =
        (0..REGISTRY.len()).partition(|&i| hints[i] > LARGE_STORE_BYTES);
    let small_results = futures::future::join_all(small.iter().map(|&i| fetch_all(i))).await;
    for (&store_idx, candidates) in small.iter().zip(small_results) {
        adopt_on_promotion(cloud, store_idx, &before[store_idx], &candidates, &mut adopted);
    }
    for store_idx in large {
        let candidates = fetch_all(store_idx).await;
        adopt_on_promotion(cloud, store_idx, &before[store_idx], &candidates, &mut adopted);
    }
    if !adopted.is_empty() {
        crate::persist::persist(cloud);
    }
    tracing::info!(
        peers = ?peer_names,
        stores_checked = REGISTRY.len(),
        stores_adopted = ?adopted,
        "store_sync: promotion reconciliation complete"
    );
}

/// Adopts what the peers served for one store at promotion (`candidates`: `(peer, bytes)`, empties
/// dropped), against `before`, the local snapshot taken before the fetch window. The local snapshot
/// is re-read right HERE, after the peer round trip, not once before it: `admin_ingress` starts
/// serving and forwarding writes to this node the instant `is_control_plane_leader()` flips true
/// (see `state.rs`: no caching, no coordination with this function), which typically happens well
/// before this loop even starts, let alone finishes its network wait.
///
/// [`MERGE_STORES`] merge every differing peer snapshot row by row. Wholesale-replace stores
/// (everything else: apikeys, billing, webhooks, domains, integrations, enterprise SSO secrets,
/// identity, ...) get NO per-record provenance or signature check (the sole boundary is "must be a
/// trusted mesh member"), so trusting whichever single arbitrary peer answers with the longest
/// payload would let any ONE reachable trusted node, not necessarily one that was ever the
/// control-plane leader, become the adopted source of truth for fleet-wide secrets the instant some
/// OTHER node gets promoted (leadership flapping happens on this fleet with no node compromise
/// involved at all). Requiring at least 2 INDEPENDENT peers to report byte-identical, larger
/// content raises that bar to colluding two already-trusted mesh members, at no cost in the
/// legitimate recovery case: a genuinely fresher state that reached the outgoing leader before it
/// stepped down had already replicated to every follower through the ordinary store_sync pull loop,
/// so more than one surviving peer holds it, and a lone responder is the anomalous case.
///
/// A write this node accepted DURING the fetch window is in `local` and in no peer's copy, so
/// replacing wholesale would silently drop an already-acknowledged write: the local copy is kept,
/// since the outgoing leader's trailing writes are the lesser loss.
fn adopt_on_promotion(
    cloud: &Arc<CloudState>,
    store_idx: usize,
    before: &[u8],
    candidates: &[(String, Vec<u8>)],
    adopted: &mut Vec<&'static str>,
) {
    let store = &REGISTRY[store_idx];
    let local = (store.snapshot)(cloud);
    if MERGE_STORES.contains(&store.name) {
        for (peer_name, bytes) in candidates {
            if bytes == &local {
                continue;
            }
            if let Some(n) = (store.adopt)(cloud, bytes) {
                adopted.push(store.name);
                tracing::warn!(
                    store = store.name,
                    from_peer = %peer_name,
                    local_bytes = local.len(),
                    peer_bytes = bytes.len(),
                    adopted_count = n,
                    "store_sync: promotion reconciliation merged a peer's snapshot"
                );
            }
        }
        return;
    }
    let mut by_bytes: HashMap<&[u8], Vec<&str>> = HashMap::new();
    for (peer_name, bytes) in candidates {
        by_bytes
            .entry(bytes.as_slice())
            .or_default()
            .push(peer_name.as_str());
    }
    let best = by_bytes
        .into_iter()
        .filter(|(bytes, corroborators)| bytes.len() > local.len() && corroborators.len() >= 2)
        .max_by_key(|(bytes, _)| bytes.len());
    let Some((bytes, corroborators)) = best else {
        return;
    };
    if local != before {
        tracing::warn!(
            store = store.name,
            from_peers = ?corroborators,
            local_bytes = local.len(),
            peer_bytes = bytes.len(),
            "store_sync: promotion reconciliation NOT adopting a corroborated richer snapshot -- \
             this node accepted writes to the store during the fetch, which a wholesale replace \
             would drop"
        );
        return;
    }
    if let Some(n) = (store.adopt)(cloud, bytes) {
        adopted.push(store.name);
        tracing::warn!(
            store = store.name,
            from_peers = ?corroborators,
            local_bytes = local.len(),
            peer_bytes = bytes.len(),
            adopted_count = n,
            "store_sync: promotion reconciliation adopted a peer-corroborated richer \
             snapshot -- this node's own copy may have missed writes the outgoing leader \
             accepted"
        );
    }
}
