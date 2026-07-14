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
        name: "teams",
        // HashMap<String, Team> → sorted BTreeMap for deterministic bytes.
        snapshot: |c| {
            let m: std::collections::BTreeMap<String, crate::teams::Team> =
                c.teams.snapshot().into_iter().collect();
            enc(&m)
        },
        adopt: |c, b| {
            let m: std::collections::BTreeMap<String, crate::teams::Team> = serde_json::from_slice(b).ok()?;
            if m.is_empty() {
                return None;
            }
            let n = m.len();
            c.teams.load(m.into_iter().collect());
            Some(n)
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
        snapshot: |c| enc_sorted(c.databases.snapshot()),
        adopt: |c, b| {
            let v: Vec<crate::databases::Database> = serde_json::from_slice(b).ok()?;
            if v.is_empty() {
                return None;
            }
            let n = v.len();
            c.databases.load(v);
            Some(n)
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
            let v: Vec<crate::integrations::IntegrationResource> = serde_json::from_slice(b).ok()?;
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
];

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
