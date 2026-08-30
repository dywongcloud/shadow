//! Generic leader→follower store replication.
//!
//! A whole CLASS of `CloudState` stores are node-local: their mutations
//! (POST/PUT/DELETE) forward to the control-plane leader via `admin_ingress`,
//! but their GET handlers serve the LOCAL store — so under multi-A DNS a
//! dashboard read lands on a random node that never saw the leader's writes and
//! returns empty/stale data. This was live-witnessed twice (the teams store,
//! then the admin incidents page showing nothing / "create doesn't work") and a
//! fleet audit found ~10 more stores with the identical shape.
//!
//! Rather than hand-write a bespoke gossip arm + follower-adoption block per
//! store, this module is ONE mechanism: a [`REGISTRY`] of [`SyncedStore`]
//! entries, each a `(name, snapshot, adopt)` triple. The gossip layer exposes
//! every entry at `GET /v1/store-snapshot/<name>` (see `gossip::dispatch`), and
//! the relational-mirror loop's follower branch (see
//! `spawn_relational_mirror_loop` in `main.rs`) iterates the registry every
//! tick, pulls each store's snapshot from the leader, and adopts it when it
//! differs.
//!
//! Contract: `snapshot` must produce DETERMINISTIC bytes for equal state (maps
//! serialized via sorted `BTreeMap`/pre-sorted `Vec`), because the follower's
//! change-gate is a raw byte comparison of the leader's bytes against the
//! follower's own `snapshot` bytes — no `PartialEq` on the payload types
//! needed. `adopt` returns `Some(count)` when it actually loaded new state
//! (for the log line), `None` when it declined (empty/unparsable — never wipe a
//! follower on a momentarily-unreachable or booting leader).
//!
//! Scope: only stores whose full contents are safe to replicate wholesale under
//! the single-writer model. Node-affinity stores (securelinks — the tunnel runs
//! on the provisioning node), append-only logs (audit — needs merge-on-read),
//! and edge-enforcement config (waf/router/cron/bot — need an enforcement
//! overlay so every node ENFORCES, not just displays) are deliberately excluded
//! and handled separately. Secret-bearing members (apikeys hashes, database
//! credentials, enterprise SAML/SCIM secrets) ride the peer-trust-enforced,
//! signed gossip mesh — the same transport TLS bundles, billing, and zkauth
//! rosters already replicate over.

use crate::state::CloudState;
use std::collections::HashMap;
use std::sync::Arc;

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

/// Deterministic-order serialization for a Vec whose store is HASHMAP-backed
/// (`snapshot()` collects `.values()` in nondeterministic hash order, which
/// differs process-to-process). Without this the follower's byte-compare
/// change-gate never matches the leader's differently-ordered bytes and it
/// re-adopts the identical data every tick (wasteful load + a misleading
/// "adopted" log line every 60s) — live-witnessed for databases/domains/gitops.
/// Sorting by each element's own canonical JSON makes both sides produce
/// identical bytes for identical state, without needing to know the element's
/// key field. Display order is unaffected: every read handler is team-scoped
/// and the dashboard sorts client-side.
fn enc_sorted<T: serde::Serialize>(v: Vec<T>) -> Vec<u8> {
    // Canonicalize via serde_json::Value FIRST: this crate builds serde_json
    // WITHOUT the `preserve_order` feature, so `Value::Object` is BTreeMap-
    // backed and re-serializes with SORTED keys — which is what makes a nested
    // map deterministic too (e.g. `Database.connection: HashMap`, whose own key
    // order is otherwise process-random and would defeat element sorting). Then
    // sort the elements by their now-canonical string.
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

pub static REGISTRY: &[SyncedStore] = &[
    SyncedStore {
        // Allocation state is written on the leader but read on any API node;
        // replicate it rather than trusting a node-local cache after DNS
        // round-robin changes.
        name: "marketplace_allocations",
        snapshot: |c| enc_sorted(c.marketplace_allocations.snapshot()),
        adopt: |c, b| {
            let rows: Vec<crate::marketplace::Allocation> = serde_json::from_slice(b).ok()?;
            // An empty leader response is ambiguous during cold start, so never
            // wipe a follower's known authorizations from it.
            if rows.is_empty() {
                return None;
            }
            let count = rows.len();
            c.marketplace_allocations.load(rows);
            Some(count)
        },
    },
    SyncedStore {
        name: "browser_admissions",
        // Unlike durable business stores, an empty active set is authoritative:
        // tombstones + the monotonic version prove it is a revocation, not a
        // boot-time read failure. Adoption also reconciles Gateway and BrowserPool.
        snapshot: crate::browser_admission::snapshot_bytes,
        adopt: crate::browser_admission::adopt_snapshot,
    },
    SyncedStore {
        name: "browser_presence",
        // Same authoritative-empty shape as browser_admissions, for the same
        // reason: a leader whose last browser peer just disconnected must be
        // able to replicate "zero satellites now", not be mistaken for a
        // follower that hasn't synced yet.
        snapshot: crate::browser_presence::snapshot_bytes,
        adopt: crate::browser_presence::adopt_snapshot,
    },
    SyncedStore {
        name: "teams",
        snapshot: |c| enc(&c.teams.snapshot_synced()),
        // Merge the versioned aggregate and permanent tombstones. A legacy bare
        // map is accepted without tombstones; a pre-upgrade peer safely declines
        // the new envelope rather than replacing its state by omission.
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
        // The OTHER half of a tenant's tier. `teams` has been replicated here
        // for a long time while `billing` was not, and that asymmetry is exactly
        // how ONE logical fact ended up with two per-node values: measured live
        // 2026-08-03, `teams.plan` was unanimously `enterprise` across all 8
        // nodes while `billing.plan` read `hobby` on 7 and `enterprise` on 1 (a
        // 13-day-stale holdout) for the SAME tenant — and the leader held the
        // wrong half, so the owner account was quota-locked out of deploying.
        //
        // Reads were patched over at runtime by `proxy_billing_read` forwarding
        // to the billing authority, but that proxy falls back to the node's own
        // stale row on failure, and every INTERNAL consumer (`can_deploy`, quota
        // checks, metering) reads the local row unconditionally with no proxy at
        // all. Replicating it converges the halves by the same leader-pull
        // mechanism `teams` already uses instead of relying on a read-path patch.
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
            // Authoritative-empty is NOT meaningful here: a leader with zero
            // billing accounts is indistinguishable from one that has not
            // loaded yet, and adopting that would wipe every tenant's tier and
            // balance fleet-wide. Decline, exactly as `teams` does.
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
        // HashMap<String, ProjectSettings> → sorted BTreeMap for deterministic
        // bytes, same shape as `teams`. Confirmed-real gap: `ProjectStore` is
        // node-local like every other store this registry fixes, but was
        // excluded from the first pass — a control-plane failover to a peer
        // that never locally built a given project serves that project's
        // settings/env/domains as empty defaults until it happens to receive a
        // write for it.
        snapshot: |c| enc(&c.projects.snapshot_synced()),
        // MERGE with tombstones, never replace — the `databases` fix applied to
        // the store whose wholesale replacement is how a single node's row loss
        // became "the project vanished from the account fleet-wide within 60s".
        // Legacy payloads (a bare map, from a pre-upgrade node) are accepted
        // tombstone-less; a pre-upgrade node receiving THIS shape fails to
        // parse and keeps its own state — the safe direction, as with databases.
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
                        incarnation_tombstones: Default::default(),
                    })
                })?;
            if synced.rows.is_empty()
                && synced.tombstones.is_empty()
                && synced.incarnation_tombstones.is_empty()
            {
                return None;
            }
            Some(c.projects.merge_synced(synced))
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
        // PushState's Vecs are kept sorted by the store's own mutators and the
        // watermark map is a BTreeMap, so `enc` alone is already deterministic.
        snapshot: |c| enc(&c.push.snapshot()),
        adopt: |c, b| {
            let v: crate::push::PushState = serde_json::from_slice(b).ok()?;
            // Decline an entirely-empty leader payload (fresh leader before its
            // own first persist load) — never wipe a follower's adopted copy.
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
        // MERGE, never replace — and carry deletions explicitly.
        //
        // This adopted the leader's list wholesale, guarded only against a fully
        // EMPTY payload, which makes "the leader has not got this record yet" and
        // "this record was deleted" the same event. Witnessed end to end: a managed
        // SQLite database created through the leader (HTTP 200, reached `ready`)
        // existed on 6 of 11 nodes and then vanished from ALL of them — the leader
        // was OOM-killed before its debounced save ran (SIGKILL bypasses
        // `flush_blocking`), restarted without the record, and every follower
        // adopted that and erased its own copy. A replicated store must not be able
        // to destroy data by omission.
        //
        // Legacy payloads (a bare array, from a node still running the previous
        // binary) are still accepted so a mixed fleet keeps converging; they simply
        // carry no tombstones. A pre-upgrade node receiving the NEW object shape
        // fails to parse and declines to adopt, which leaves its own state intact —
        // the safe direction.
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
        // Queue/consumer METADATA only — messages never ride this (node-local
        // + GuardianDB-mirrored, see queues.rs's module doc). Same
        // empty-payload guard as `databases`: a fully-empty snapshot means
        // "the leader hasn't published anything yet", never "delete
        // everything" — tombstones carry deletions explicitly.
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
            // read/archived state can legitimately be empty on the leader (no
            // one has read anything yet); the follower loop's outer byte-compare
            // already skips a no-change tick, so only decline the truly-empty
            // payload to avoid a churny load-of-nothing.
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
            // Enterprise config has no single "count"; report 1 to signal a load
            // happened (the outer byte-compare guarantees this only fires on a
            // real change).
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
        // Insertion order (oldest→newest) is deterministic and identical once a
        // follower adopts the leader's buffer, so `enc` (not `enc_sorted`) is
        // right — and preserves chronological order for the operator view.
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
    // Edge-enforcement config: replicating the leader's WAF rules / redirects /
    // rewrites / bot policy makes EVERY node enforce identically, not just
    // display the same thing — a follower node serving edge traffic otherwise
    // applies only its own locally-authored rules. (cron is deliberately NOT
    // here: its config-replication is coupled to gating execution to a single
    // node, tracked separately.)
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
    // L7 rate limiting was the one edge control NOT replicated here, which made
    // it worse than merely mis-displayed: `PUT /v1/ratelimit` is a mutation, so
    // it configured the leader's in-process atomics only — the limit was
    // ENFORCED on the leader alone while the Network page, polling back through
    // the round-robin, read `enabled: false` from everyone else. A security
    // control that reports as configured but isn't.
    //
    // Only the CONFIG is synced. `tracked_ips`/`blocked_total` are per-node
    // counters; including them would make every node's snapshot differ on every
    // request and defeat the content-compare that gates a replication write.
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
    // ACME DNS-01 challenge TXT for Seer-answered (self-delegated) zones: the
    // leader's acme.rs places challenges, but Let's Encrypt resolves the zone
    // through ANY of the advertised nameserver nodes — replication is what lets
    // every follower's Seer answer. Post-issuance cleanup empties the leader's
    // map, which adoption declines per the registry-wide never-wipe rule; a
    // follower's stale copy stops answering via the store's own lookup TTL
    // instead (see `AcmeChallengeStore::lookup`).
    SyncedStore {
        name: "acme_challenges",
        // BTreeMap-backed store → `enc` alone is already deterministic.
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
    // HTTP-01 equivalents for custom tenant domains: a validation fetch can
    // land on ANY node (that is the whole point of the challenge type), so
    // the token → key-authorization map replicates exactly like the TXT one.
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

pub static MERGE_STORES: &[&str] = &[
    "browser_admissions",
    "browser_presence",
    "projects",
    "teams",
];

/// Rate-limit config wire shape — deliberately config-only (see the registry
/// entry above for why the live counters are excluded).
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

pub async fn reconcile_on_promotion(cloud: &Arc<CloudState>) {
    let mut peers: Vec<(String, String, String)> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.healthy)
        .filter_map(|n| Some((n.name, n.peer_id?, n.iroh_addr?)))
        .collect();
    // 5, not 3: the non-merge-store adoption below now requires 2 INDEPENDENT
    // peers to corroborate identical bytes before adopting (see below) — a
    // wider candidate set makes that quorum reachable even when one or two
    // queried peers are momentarily slow/unreachable.
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

    let mut fetches = Vec::with_capacity(REGISTRY.len() * peers.len());
    for (store_idx, store) in REGISTRY.iter().enumerate() {
        for (peer_name, peer_id, addr) in &peers {
            let path = format!("/v1/store-snapshot/{}", store.name);
            fetches.push(async move {
                let bytes = crate::gossip::request_to(
                    cloud,
                    peer_id,
                    addr,
                    hive_p2p::GOSSIP_GET,
                    &path,
                    &[],
                    10,
                )
                .await;
                (store_idx, peer_name.clone(), bytes)
            });
        }
    }
    let results = futures::future::join_all(fetches).await;
    let mut by_store: Vec<Vec<(String, Vec<u8>)>> = vec![Vec::new(); REGISTRY.len()];
    for (store_idx, peer_name, resp) in results {
        let Some(bytes) = resp else { continue };
        if bytes.is_empty() {
            continue;
        }
        by_store[store_idx].push((peer_name, bytes));
    }

    // Freshly re-read right HERE, after the (up to 10s) peer round trip above
    // — not once before it. `admin_ingress` starts serving/forwarding writes
    // to this node the instant `is_control_plane_leader()` flips true
    // (state.rs, no caching, no coordination with this function), which
    // typically happens well before this loop even starts, let alone
    // finishes its network wait. A snapshot taken before the wait cannot see
    // a write that legitimately landed locally during it, so comparing
    // against one risks silently discarding an already-200'd write under a
    // peer's (necessarily pre-promotion, i.e. stale) snapshot — the exact
    // scenario this whole mechanism exists to guard against, turned back on
    // itself. Re-reading here shrinks that window from "up to 10s" to the
    // (now negligible) gap between this line and each store's `adopt()` call.
    let local_snapshots: Vec<Vec<u8>> = REGISTRY.iter().map(|s| (s.snapshot)(cloud)).collect();

    let mut adopted: Vec<&'static str> = Vec::new();
    for (store_idx, store) in REGISTRY.iter().enumerate() {
        if MERGE_STORES.contains(&store.name) {
            for (peer_name, bytes) in &by_store[store_idx] {
                if bytes == &local_snapshots[store_idx] {
                    continue;
                }
                if let Some(n) = (store.adopt)(cloud, bytes) {
                    adopted.push(store.name);
                    tracing::warn!(
                        store = store.name,
                        from_peer = %peer_name,
                        local_bytes = local_snapshots[store_idx].len(),
                        peer_bytes = bytes.len(),
                        adopted_count = n,
                        "store_sync: promotion reconciliation merged a peer's snapshot"
                    );
                }
            }
            continue;
        }
        // Wholesale-replace stores (apikeys/teams/billing/webhooks/domains/
        // integrations/enterprise SSO secrets/identity/... — everything not
        // in MERGE_STORES) get NO per-record provenance or signature check
        // (this module's own doc: the sole boundary is "must be a trusted
        // mesh member"), so trusting whichever single arbitrary peer answers
        // fastest with the longest payload would let ANY ONE reachable
        // trusted node — not necessarily one that was ever the control-plane
        // leader, merely one that is alive and answers
        // `GET /v1/store-snapshot/<name>` — become the adopted source of
        // truth for fleet-wide secrets the instant some OTHER node gets
        // promoted (a real, non-rare trigger: leadership flapping happens on
        // this fleet with no node compromise involved at all). Requiring at
        // least 2 INDEPENDENT peers to report byte-identical content raises
        // that bar to "collude two already-trusted mesh members" while
        // costing nothing in the legitimate recovery case: a genuinely
        // fresher state that reached the outgoing leader before it stepped
        // down had already replicated to every follower via the ordinary
        // 60s store_sync pull loop, so more than one surviving peer holds it
        // — a single lone responder is the anomalous case, not the common one.
        let mut by_bytes: HashMap<&[u8], Vec<&str>> = HashMap::new();
        for (peer_name, bytes) in &by_store[store_idx] {
            by_bytes
                .entry(bytes.as_slice())
                .or_default()
                .push(peer_name.as_str());
        }
        let best = by_bytes
            .into_iter()
            .filter(|(bytes, corroborators)| {
                bytes.len() > local_snapshots[store_idx].len() && corroborators.len() >= 2
            })
            .max_by_key(|(bytes, _)| bytes.len());
        let Some((bytes, corroborators)) = best else {
            continue;
        };
        if let Some(n) = (store.adopt)(cloud, bytes) {
            adopted.push(store.name);
            tracing::warn!(
                store = store.name,
                from_peers = ?corroborators,
                local_bytes = local_snapshots[store_idx].len(),
                peer_bytes = bytes.len(),
                adopted_count = n,
                "store_sync: promotion reconciliation adopted a peer-corroborated richer \
                 snapshot -- this node's own copy may have missed writes the outgoing leader \
                 accepted"
            );
        }
    }
    if !adopted.is_empty() {
        // Promotion merges are authoritative mutations. Queue them immediately;
        // otherwise a hard crash before the periodic capture can lose a newly
        // recovered row or tombstone and reboot into the stale local snapshot.
        crate::persist::persist(cloud);
    }
    tracing::info!(
        peers = ?peer_names,
        stores_checked = REGISTRY.len(),
        stores_adopted = ?adopted,
        "store_sync: promotion reconciliation complete"
    );
}
