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
//! the follower pull (`crate::store_follower`) iterates the registry every
//! tick, pulls each store's snapshot from the leader through [`fetch_snapshot`]
//! (the one size-aware transfer policy), and adopts it when it differs.
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
        // HMAC replay facts and payment intents must follow the same
        // leader-written/read-anywhere model as allocations.
        name: "marketplace_security",
        snapshot: |c| serde_json::to_vec(&c.marketplace_security.snapshot()).unwrap_or_default(),
        adopt: |c, b| {
            let snapshot: crate::marketplace::MarketplaceSecuritySnapshot =
                serde_json::from_slice(b).ok()?;
            c.marketplace_security.load(snapshot);
            Some(1)
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
        name: "production_deployments",
        // Same MERGE-not-replace shape as `projects`: tombstoned deletions,
        // newest-`updated_ms`-per-row wins. Genuinely-empty is normal (no
        // production deployments yet on a fresh fleet), never distinguished
        // from "not synced yet" — the row-level merge already tolerates a
        // legitimately-partial local set, unlike the wholesale-replace stores
        // above that must decline an empty payload outright.
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
    "production_deployments",
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

// ---- snapshot transfer policy ---------------------------------------------
//
// ONE place decides how a store snapshot crosses a trunk, for every caller
// (the follower pull in `store_follower`, `reconcile_on_promotion`): a trunk
// sustains ~170 KB/s (1200-byte PMTU, 64 ms RTT, measured), so a fixed 10 s
// budget can never carry billing (6.2 MB) or incidents (10.9 MB), and a fixed
// long budget still turns growth into a permanent failure once a store
// outgrows it. The budget is derived from the store's size instead.

/// Snapshot size at which a store is LARGE: it leaves the follower pull's
/// concurrent batch for the serial lane and gets a size-derived budget.
pub const LARGE_STORE_BYTES: usize = 1 << 20;
/// Budget and response bound for a snapshot not known to be large.
const SMALL_FETCH_SECS: u64 = 10;
const SMALL_RESPONSE_CAP: usize = 16 << 20;
/// Response bound for a large snapshot (the memory ceiling of one pull).
const LARGE_RESPONSE_CAP: usize = 64 << 20;
/// The slowest sustained rate a large budget is sized for — well under the
/// measured ~170 KB/s, so a slow-but-moving trunk still finishes.
const LARGE_MIN_RATE: usize = 64 << 10;

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

/// `(timeout_secs, response_cap)` for a fetch of `hint` bytes. Large: the
/// time `hint` plus 25 % growth takes at [`LARGE_MIN_RATE`] plus 30 s of
/// setup, floored at `HIVE_STORE_SYNC_LARGE_TIMEOUT_SECS` (240) — and never
/// below what the response cap could carry at that rate, so a store never
/// grows into a budget that always fails.
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
    let path = format!("/v1/store-snapshot/{}", store.name);
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

    // Each store's local snapshot BEFORE its fetch window: a wholesale store
    // whose local copy changed during the window is not adopted (below).
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
    // Small stores: every (store, peer) at once, as before. Large ones
    // (`LARGE_STORE_BYTES`): one store at a time — from every peer at once,
    // each peer being its own trunk — under the size-derived budget, and
    // adopted right after its own fetch, so a long transfer never widens any
    // other store's window.
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

/// Adopt what the peers served for one store at promotion (`candidates`:
/// `(peer, bytes)`, empties dropped), against `before` — the local snapshot
/// taken before the fetch window.
fn adopt_on_promotion(
    cloud: &Arc<CloudState>,
    store_idx: usize,
    before: &[u8],
    candidates: &[(String, Vec<u8>)],
    adopted: &mut Vec<&'static str>,
) {
    let store = &REGISTRY[store_idx];
    // Freshly re-read right HERE, after the peer round trip — not once before
    // it. `admin_ingress` starts serving/forwarding writes to this node the
    // instant `is_control_plane_leader()` flips true (state.rs, no caching, no
    // coordination with this function), which typically happens well before
    // this loop even starts, let alone finishes its network wait.
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
    // A write this node accepted DURING the fetch window is in `local` and in
    // no peer's copy: replacing wholesale would silently drop an already-200'd
    // write. Keep the local copy; the outgoing leader's trailing writes are the
    // lesser loss (CS-4's attested digests recover them exactly).
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
