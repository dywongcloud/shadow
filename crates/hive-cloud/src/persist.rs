//! Durable persistence for platform state.
//!
//! The platform's operational records (deployments, per-project settings, WAF
//! rules, cron jobs, routing) are snapshotted to disk so a node restart restores
//! the full state — the underlying persisted database for platform operation.
//!
//! Backends:
//! * **file** (default) — atomic JSON snapshot under `$HIVE_DATA` (default
//!   `~/.hive-cloud`). Always available, no external services.
//! * **guardian-db** (cargo feature `guardian`) — an iroh-native, content-
//!   addressed, P2P-replicated store ([wmaslonek/guardian-db]) so state
//!   replicates across the mesh. Enabled with `--features guardian`; the file
//!   snapshot remains the local source of truth and bootstrap.
//!
//! [wmaslonek/guardian-db]: https://github.com/wmaslonek/guardian-db

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use fluid_core::DeployRecord;
use hive_edge::{CronJob, Redirect, Rewrite, WafRule, WorkflowDef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::project_settings::{ProjectSettings, SyncedProjects};
use crate::state::CloudState;

#[derive(Default, Serialize, Deserialize)]
pub struct PlatformSnapshot {
    /// When this snapshot was SAVED (ms epoch). The guardian restore-on-rollback
    /// guard compares this against replicated commit time to detect a local snapshot
    /// that regressed (older file restored after a crash / disk swap): if the
    /// GuardianDB replica is NEWER than what's on disk, the replica wins at boot.
    /// Guardian canonical payloads omit this write-attempt-only field and restore
    /// it from the iroh-docs entry timestamp, avoiding a new blob for unchanged state.
    #[serde(default)]
    pub saved_ms: u64,
    #[serde(default)]
    pub deployments: Vec<DeployRecord>,
    #[serde(default)]
    pub projects: HashMap<String, ProjectSettings>,
    #[serde(default)]
    pub waf_rules: Vec<WafRule>,
    /// L7 rate-limit config (enabled, limit, window_ms). Held in-process as
    /// atomics, so without this the setting silently vanished on every restart.
    #[serde(default)]
    pub ratelimit: Option<(bool, u32, u64)>,
    #[serde(default)]
    pub cron: Vec<CronJob>,
    #[serde(default)]
    pub redirects: Vec<Redirect>,
    #[serde(default)]
    pub rewrites: Vec<Rewrite>,
    #[serde(default)]
    pub teams: HashMap<String, crate::teams::Team>,
    /// Permanent causal deletions for team aggregates. Absence cannot encode a
    /// deletion because a peer may return after any bounded retention window.
    #[serde(default)]
    pub team_tombstones: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    pub webhooks: Vec<crate::webhooks::Webhook>,
    #[serde(default)]
    pub databases: Vec<crate::databases::Database>,
    /// Durable data for the in-process stores (queue + vector) so they survive a
    /// restart like blob (disk) and the DB records themselves.
    #[serde(default)]
    pub database_data: crate::databases::DataSnapshot,
    /// Deletions of database records, kept durable on purpose: a node that
    /// forgets its tombstones on restart re-imports every database it had
    /// deleted from whichever peer still holds them.
    #[serde(default)]
    pub database_tombstones: std::collections::BTreeMap<String, u64>,
    /// One-use Studio replay facts are resource-independent so a live Guardian
    /// restore omitting a database record cannot erase them.
    #[serde(default)]
    pub database_studio_replay: crate::databases::StudioReplaySnapshot,
    /// Same rationale for the projects store (see `SyncedProjects`).
    #[serde(default)]
    pub project_tombstones: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    pub project_incarnation_tombstones: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<fluid_core::ProjectIncarnation, u64>,
    >,
    /// Hour/day consumption-breakdown rollups (Weekly/Monthly chart data) —
    /// minute-resolution buckets are excluded (short retention, refill within
    /// minutes; see metrics.rs's module doc comment for why).
    #[serde(default)]
    pub metrics_rollup: crate::metrics::RollupSnapshot,
    /// Build records incl. logs (newest-capped; see BuildStore::snapshot) —
    /// previously in-memory only, so every restart erased all build logs while
    /// the deployments they built lived on.
    #[serde(default)]
    pub builds: Vec<crate::git::Build>,
    #[serde(default)]
    pub incidents: Vec<crate::incidents::Incident>,
    /// Web-push subscriptions / SMS targets / delivery watermarks / VAPID keys.
    #[serde(default)]
    pub push: crate::push::PushState,
    #[serde(default)]
    pub apikeys: Vec<crate::apikeys::ApiKey>,
    #[serde(default)]
    pub integrations: Vec<crate::integrations::IntegrationResource>,
    #[serde(default)]
    pub svcgraphs: Vec<crate::svcgraph::ServiceGraph>,
    #[serde(default)]
    pub orgs: Vec<crate::identity::OrgRecord>,
    #[serde(default)]
    pub users: Vec<crate::identity::UserRecord>,
    #[serde(default)]
    pub billing: Vec<crate::billing::BillingAccount>,
    #[serde(default)]
    pub billing_ledger: Vec<crate::billing::LedgerEntry>,
    #[serde(default)]
    pub billing_invoices: Vec<crate::billing::Invoice>,
    /// Per-tenant metering watermarks (`BillingStore::meters`). `None` is
    /// load-bearing and means UNKNOWN — a snapshot written by a node from
    /// before this field existed, which is NOT the same as "every tenant is at
    /// zero": billing charges `current − watermark`, so a zero watermark
    /// against still-climbing fleet counters re-bills the entire cumulative
    /// total. `BillingStore::meters_load` turns `None` into a re-baseline
    /// (charge nothing on the first reading) instead.
    #[serde(default)]
    pub billing_meters: Option<Vec<crate::billing::MeterWatermark>>,
    /// Open (unconfirmed) checkouts. Previously in-memory only, so a node
    /// restart between "user redirected to Stripe" and "user redirected back"
    /// made `confirm_checkout` return `None` for a session the customer had
    /// actually paid for. The relational mirror's `billing_checkouts` table is
    /// a read-only projection for the admin/SQL view — nothing ever loads it
    /// back — so it was not, and is not, a restore path.
    #[serde(default)]
    pub billing_checkouts: Vec<crate::billing::Checkout>,
    #[serde(default)]
    pub domains: Vec<crate::dns::DomainRecord>,
    #[serde(default)]
    pub docs: Vec<crate::docstore::Doc>,
    #[serde(default)]
    pub gitops: Vec<crate::gitops::GitOpsLink>,
    /// Workflow definitions (incl. WDK-ingested ones with their graphs). Persisted
    /// so a deployed app's workflows survive node restarts — the manifest is only
    /// ingested during a live deploy, so without this they vanished on reboot.
    #[serde(default)]
    pub workflow_defs: Vec<WorkflowDef>,
    /// Enterprise feature suite state (secrets AEAD-encrypted in-struct). See
    /// [`crate::enterprise::EnterpriseSnapshot`].
    #[serde(default)]
    pub enterprise: crate::enterprise::EnterpriseSnapshot,
    /// Sandboxes (records only — live cell handles/vsock sockets don't survive
    /// a restart and are re-provisioned on next use). See
    /// [`crate::sandboxes::SandboxesSnapshot`].
    #[serde(default)]
    pub sandboxes: crate::sandboxes::SandboxesSnapshot,
}

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("HIVE_DATA") {
        return PathBuf::from(d);
    }
    dirs_home().join(".hive-cloud")
}

fn peer_iroh_path() -> PathBuf {
    data_dir().join("peer_iroh.json")
}

/// Persist the gossip-transport map (peer admin URL -> (node_id, iroh addr_json)) so
/// a node can bootstrap gossip over iroh on restart WITHOUT the HTTP-over-SSH
/// tunnels. Safe to call every gossip round — atomic temp+rename. Paired with
/// persistent iroh identities (stable EndpointId), the saved addresses stay valid
/// across restarts, so the SSH tunnels are no longer required for rendezvous.
pub fn save_peer_iroh(map: &std::collections::HashMap<String, (String, String)>) {
    let dir = data_dir();
    let tmp = dir.join("peer_iroh.json.tmp");
    let Ok(json) = serde_json::to_vec(map) else {
        return;
    };
    if let Err(error) = write_sidecar_atomic(&peer_iroh_path(), &tmp, &json) {
        tracing::warn!(%error, "failed to persist mesh peer addresses");
    }
}

/// Load the persisted gossip-transport map (empty if none).
pub fn load_peer_iroh() -> std::collections::HashMap<String, (String, String)> {
    std::fs::read_to_string(peer_iroh_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn peer_guardian_addr_path() -> PathBuf {
    data_dir().join("peer_guardian_addr.json")
}

/// Persist peers' GuardianDB-specific addresses (node name -> serialized
/// `iroh::EndpointAddr`) — a SEPARATE identity/endpoint from the mesh
/// addresses in `peer_iroh.json` above (GuardianDB runs its own independent
/// iroh client per node). Loaded at boot to seed `guardian::set_boot_seed_peers`
/// before GuardianDB's one-time KV-store open (the only window its automatic
/// DocTicket exchange is consulted in — see guardian.rs). On a node's FIRST
/// ever boot with this feature, this file doesn't exist yet (nothing has ever
/// gossiped a guardian address) — boot-seeding is empty and this node falls
/// back to the pre-existing single-namespace-per-node behavior; the NEXT
/// restart, after the gossip loop has had a chance to populate and persist
/// this file, is when boot-seeding actually has something to work with.
pub fn save_peer_guardian_addr(map: &std::collections::HashMap<String, String>) {
    let dir = data_dir();
    let tmp = dir.join("peer_guardian_addr.json.tmp");
    let Ok(json) = serde_json::to_vec(map) else {
        return;
    };
    if let Err(error) = write_sidecar_atomic(&peer_guardian_addr_path(), &tmp, &json) {
        tracing::warn!(%error, "failed to persist Guardian peer addresses");
    }
}

/// Load the persisted peer GuardianDB-address map (empty if none).
pub fn load_peer_guardian_addr() -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(peer_guardian_addr_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_sidecar_atomic(
    path: &std::path::Path,
    tmp: &std::path::Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sidecar path has no parent",
        )
    })?;
    std::fs::create_dir_all(dir)?;
    {
        use std::io::Write;
        let file = std::fs::File::create(tmp)?;
        let mut writer = std::io::BufWriter::new(&file);
        writer.write_all(bytes)?;
        writer.flush()?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, path)?;
    std::fs::File::open(dir)?.sync_all()
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

/// Load the snapshot from disk (empty if none).
pub fn load() -> PlatformSnapshot {
    match std::fs::read_to_string(state_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => PlatformSnapshot::default(),
    }
}

fn save_with_guardian_generation(snap: &PlatformSnapshot) -> std::io::Result<Option<u64>> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join("state.json.tmp");
    let json = serde_json::to_string_pretty(snap).unwrap_or_else(|_| "{}".into());
    // Durability: write + fsync the temp file, then atomically rename into place.
    {
        let f = std::fs::File::create(&tmp)?;
        use std::io::Write;
        let mut w = std::io::BufWriter::new(&f);
        w.write_all(json.as_bytes())?;
        w.flush()?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, state_path())?;
    // The rename is not crash-durable until the directory entry itself reaches
    // stable storage. A power loss must not resurrect a row whose tombstone was
    // already acknowledged and written.
    std::fs::File::open(&dir)?.sync_all()?;
    // Write per-tenant namespace documents (the multi-tenant schema partition).
    save_namespaces(snap)?;
    // Replicate into the always-on GuardianDB (durable + peer-replicated copy).
    Ok(crate::guardian::replicate(snap))
}

/// Atomically write the snapshot to disk and admit its Guardian generation.
pub fn save(snap: &PlatformSnapshot) -> std::io::Result<()> {
    save_with_guardian_generation(snap).map(|_| ())
}

/// Normalize a tenant namespace: empty => "personal".
fn ns_norm(team: &str) -> String {
    if team.trim().is_empty() {
        "personal".into()
    } else {
        team.trim().to_string()
    }
}

/// Partition the global snapshot into **per-tenant namespace documents**. Every
/// tenant-owned record (projects, deployments, databases, api keys, webhooks,
/// the team itself) is filed under its org/team namespace; platform-level edge
/// config (WAF, cron, routing, incidents) lives under the reserved `_global`
/// namespace. This is the schema that guardian-db replicates so data is scoped
/// and isolated by namespace across the mesh.
pub fn namespaced(snap: &PlatformSnapshot) -> BTreeMap<String, Value> {
    // A project entirely absent from the snapshot's `projects` map (settings
    // lost, or a record whose project was deleted out from under it) is
    // UNOWNED, never the owner's real "personal" namespace — filing it there
    // let another tenant's deployment/webhook re-materialize as personal-owned
    // on any node that later restores/clones this namespace doc.
    let team_of = |project: &str| -> String {
        snap.projects
            .get(project)
            .map(|s| ns_norm(&s.team))
            .unwrap_or_else(|| "__untagged__".into())
    };
    let mut docs: BTreeMap<String, serde_json::Map<String, Value>> = BTreeMap::new();
    let mut push = |ns: String, key: &str, val: Value| {
        let doc = docs.entry(ns).or_default();
        doc.entry(key.to_string())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap()
            .push(val);
    };

    for d in &snap.deployments {
        push(team_of(&d.project), "deployments", json!(d));
    }
    for (p, s) in &snap.projects {
        let mut v = json!(s);
        v["project"] = json!(p);
        push(ns_norm(&s.team), "projects", v);
    }
    for db in &snap.databases {
        push(ns_norm(&db.team), "databases", json!(db));
    }
    for k in &snap.apikeys {
        push(ns_norm(&k.team), "api_keys", json!(k));
    }
    for i in &snap.integrations {
        // Redacted view in the namespace docs — never persist secrets here.
        push(ns_norm(&i.team), "integrations", i.public());
    }
    for w in &snap.webhooks {
        push(team_of(&w.project), "webhooks", json!(w));
    }
    for o in &snap.orgs {
        push(ns_norm(&o.tenant), "orgs", json!(o));
    }
    for u in &snap.users {
        push(ns_norm(&u.tenant), "users", json!(u));
    }
    for d in &snap.domains {
        push(ns_norm(&d.tenant), "domains", json!(d));
    }
    for g in &snap.gitops {
        push(ns_norm(&g.tenant), "gitops", json!(g));
    }
    // Team aggregates are authoritative for membership/plan gates even though
    // orgs/users mirror the identity provider. Include both rows and deletion
    // generations in the namespace signature so a team-only mutation also
    // refreshes GuardianDB's full rollback snapshot.
    for (slug, team) in &snap.teams {
        if !team.synthetic_seed {
            push(ns_norm(slug), "teams", json!(team));
        }
    }
    for (slug, deleted_ms) in &snap.team_tombstones {
        push(
            ns_norm(slug),
            "team_tombstones",
            json!({ "slug": slug, "deleted_ms": deleted_ms }),
        );
    }

    // Platform-level config under the reserved global namespace.
    let global = docs.entry("_global".into()).or_default();
    global.insert("waf_rules".into(), json!(snap.waf_rules));
    global.insert("cron".into(), json!(snap.cron));
    global.insert("redirects".into(), json!(snap.redirects));
    global.insert("rewrites".into(), json!(snap.rewrites));
    global.insert("incidents".into(), json!(snap.incidents));

    // These arrays represent keyed sets, not insertion sequences. Several of
    // their stores are HashMap-backed, so iteration order changes between
    // otherwise identical captures. Normalize only those set-valued arrays;
    // WAF/redirect/rewrite order is routing policy and must remain untouched.
    const SET_FIELDS: [&str; 14] = [
        "deployments",
        "projects",
        "databases",
        "api_keys",
        "integrations",
        "webhooks",
        "orgs",
        "users",
        "domains",
        "gitops",
        "teams",
        "team_tombstones",
        "cron",
        "incidents",
    ];
    for doc in docs.values_mut() {
        for field in SET_FIELDS {
            if let Some(values) = doc.get_mut(field).and_then(Value::as_array_mut) {
                values.sort_by_cached_key(|value| serde_json::to_vec(value).unwrap_or_default());
            }
        }
    }

    docs.into_iter()
        .map(|(k, v)| (k, Value::Object(v)))
        .collect()
}

/// Guardian snapshot protocol v2's namespace projection. The ordinary local
/// namespace files retain cron for backwards compatibility; the disjoint v2
/// Guardian lane omits it because cron is node-local. Project arrays use the
/// project identity as their deterministic primary key and reject malformed or
/// duplicate identities instead of publishing an ambiguous tenant document.
pub fn guardian_v2_namespaced(snap: &PlatformSnapshot) -> anyhow::Result<BTreeMap<String, Value>> {
    let mut docs = namespaced(snap);
    if let Some(global) = docs.get_mut("_global") {
        let fields = global
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Guardian namespace _global is not an object"))?;
        fields.remove("cron");
    }

    for (namespace, document) in &mut docs {
        let fields = document
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Guardian namespace {namespace:?} is not an object"))?;
        let Some(projects) = fields.get_mut("projects") else {
            continue;
        };
        let projects = projects.as_array_mut().ok_or_else(|| {
            anyhow::anyhow!("Guardian namespace {namespace:?} projects is not an array")
        })?;
        let mut identities = std::collections::BTreeSet::new();
        for project in projects.iter() {
            let identity = project
                .as_object()
                .and_then(|fields| fields.get("project"))
                .and_then(Value::as_str)
                .filter(|identity| !identity.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Guardian namespace {namespace:?} has a project without a string identity"
                    )
                })?;
            if !identities.insert(identity.to_string()) {
                anyhow::bail!(
                    "Guardian namespace {namespace:?} has duplicate project identity {identity:?}"
                );
            }
        }
        projects.sort_by_cached_key(|project| {
            let identity = project
                .get("project")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let canonical_tiebreak = serde_json::to_vec(project).unwrap_or_default();
            (identity, canonical_tiebreak)
        });
    }
    Ok(docs)
}

/// Write each tenant namespace document to `$HIVE_DATA/ns/<namespace>.json`.
pub fn save_namespaces(snap: &PlatformSnapshot) -> std::io::Result<()> {
    let dir = data_dir().join("ns");
    std::fs::create_dir_all(&dir)?;
    // Track current namespaces so stale files can be pruned.
    let docs = namespaced(snap);
    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (ns, doc) in &docs {
        let safe = ns.replace(['/', '\\', '.'], "_");
        keep.insert(format!("{safe}.json"));
        let path = dir.join(format!("{safe}.json"));
        let _ = std::fs::write(path, serde_json::to_string_pretty(doc).unwrap_or_default());
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if name.ends_with(".json") && !keep.contains(name) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    Ok(())
}

/// Capture the current platform state into a snapshot.
pub fn capture(cloud: &Arc<CloudState>) -> PlatformSnapshot {
    // ONE billing read: this was two separate `cloud.billing.snapshot()` calls
    // (`.0` and `.1`), which takes the accounts and ledger locks twice and can
    // therefore write an account balance from before a ledger entry that is
    // already in the same file.
    let (billing_accounts, billing_ledger) = cloud.billing.snapshot();
    let SyncedProjects {
        rows: projects,
        tombstones: project_tombstones,
        incarnation_tombstones: project_incarnation_tombstones,
    } = cloud.projects.snapshot_synced();
    // ONE team read: live rows and tombstones share a lock and must describe one
    // causal instant. Splitting the reads could persist a tombstone without the
    // recreation that already superseded it (or vice versa).
    let crate::teams::SyncedTeams {
        rows: team_rows,
        tombstones: team_tombstones,
    } = cloud.teams.snapshot_synced();
    PlatformSnapshot {
        saved_ms: hive_core::now_ms(),
        deployments: cloud.gw.deployment_records(),
        projects: projects.into_iter().collect(),
        waf_rules: cloud.waf.rules(),
        ratelimit: {
            let s = cloud.ratelimit.stats();
            Some((s.enabled, s.limit, s.window_ms))
        },
        cron: {
            let mut jobs = cloud.cron.list();
            for job in &mut jobs {
                job.last_run_ms = None;
                job.runs = 0;
                job.next_run_ms = None;
            }
            jobs
        },
        redirects: cloud.router.redirects(),
        rewrites: cloud.router.rewrites(),
        teams: team_rows.into_iter().collect(),
        team_tombstones,
        webhooks: cloud.webhooks.snapshot(),
        databases: cloud.databases.snapshot(),
        database_data: cloud.databases.data_snapshot(),
        database_tombstones: cloud.databases.tombstones_snapshot(),
        database_studio_replay: cloud.databases.studio_replay_snapshot(),
        project_tombstones,
        project_incarnation_tombstones,
        metrics_rollup: cloud.metrics.rollup_snapshot(),
        builds: cloud.builds.snapshot(),
        incidents: cloud.incidents.snapshot(),
        push: cloud.push.snapshot(),
        apikeys: cloud.apikeys.snapshot(),
        integrations: cloud.integrations.snapshot(),
        svcgraphs: cloud.svcgraph.snapshot(),
        orgs: cloud.identity.orgs(),
        users: cloud.identity.users(),
        billing: billing_accounts,
        billing_ledger,
        billing_invoices: cloud.billing.invoices_snapshot(),
        // Always `Some` from this build — an empty vec means "known: nothing
        // metered yet", which `None` (unknown) must never be confused with.
        billing_meters: Some(cloud.billing.meters_snapshot()),
        billing_checkouts: cloud.billing.checkouts_snapshot(),
        domains: cloud.domains.snapshot(),
        docs: cloud.docs.snapshot(),
        gitops: cloud.gitops.snapshot(),
        workflow_defs: cloud.workflows.defs(),
        enterprise: cloud.enterprise.snapshot(),
        sandboxes: {
            let (sandboxes, commands, snapshots, mounts) = cloud.sandboxes.snapshot();
            crate::sandboxes::SandboxesSnapshot {
                sandboxes,
                commands,
                snapshots,
                mounts,
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Coalescing background persister.
//
// `persist()` used to serialize the ENTIRE platform state + fsync + rename +
// namespace-partition + guardian-replicate SYNCHRONOUSLY on the calling request
// thread — paid in full on every single mutation (env edit, domain add, …), so a
// burst of edits did N full-state fsyncs back-to-back on the hot path.
//
// This replaces that with a single background writer that COALESCES bursts while
// preserving durability: `persist()` bumps a generation counter and wakes the
// writer; the writer always captures + saves the LATEST state, and if more
// mutations arrived during a save it immediately saves again. Properties:
//   * No lost mutation under normal operation — after the last `persist()` the
//     writer performs a save that captures state at-or-after that call.
//   * Bounded crash-loss window (the in-flight capture), vs the old zero-window
//     synchronous write — this is the accepted latency↔durability trade.
//   * `flush_blocking()` (wired to SIGTERM/SIGINT in main) makes a graceful
//     restart lose nothing.
//   * Synchronous fallback when the writer isn't started (early boot / tests),
//     so persistence is never silently dropped.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

struct Persister {
    cloud: Arc<CloudState>,
    dirty: AtomicU64,  // bumped on each persist() request
    saved: AtomicU64,  // highest generation durably written
    lock: Mutex<bool>, // admission-open flag paired with `cv`; does NOT guard state
    writer: Mutex<()>, // serializes capture + save + `saved` advancement
    cv: Condvar,
}
static PERSISTER: OnceLock<Arc<Persister>> = OnceLock::new();

/// Start the background coalescing persister. Call ONCE at boot, after `restore`.
pub fn spawn_persister(cloud: Arc<CloudState>) {
    let p = Arc::new(Persister {
        cloud,
        dirty: AtomicU64::new(0),
        saved: AtomicU64::new(0),
        lock: Mutex::new(true),
        writer: Mutex::new(()),
        cv: Condvar::new(),
    });
    if PERSISTER.set(p.clone()).is_err() {
        return; // already started
    }
    // Dedicated OS thread: capture + fsync are blocking, so keep them entirely off
    // the async runtime's worker threads.
    std::thread::Builder::new()
        .name("hive-persister".into())
        .spawn(move || loop {
            {
                let mut g = p.lock.lock().unwrap();
                while p.dirty.load(Ordering::SeqCst) <= p.saved.load(Ordering::SeqCst) {
                    g = p.cv.wait(g).unwrap();
                }
            }
            // Drain: coalesce everything up to the newest generation seen after
            // acquiring the exclusive snapshot-writer slot. A shutdown flush may
            // have satisfied this wake while we waited, in which case there is
            // nothing left to write.
            let write_result = {
                let _writer = p.writer.lock().unwrap();
                let target = p.dirty.load(Ordering::SeqCst);
                if target <= p.saved.load(Ordering::SeqCst) {
                    continue;
                }
                let snap = capture(&p.cloud);
                let result = save(&snap);
                if result.is_ok() {
                    p.saved.store(target, Ordering::SeqCst);
                }
                result
            };
            if let Err(e) = write_result {
                tracing::warn!(error = %e, "persist(bg) failed; will retry on next mutation");
                // Don't advance `saved` → the next persist() re-triggers a write.
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
        .expect("spawn persister thread");
}

fn admit_generation(p: &Persister) -> u64 {
    let mut admission_open = p.lock.lock().unwrap();
    while !*admission_open {
        admission_open = p.cv.wait(admission_open).unwrap();
    }
    p.dirty.fetch_add(1, Ordering::SeqCst).saturating_add(1)
}

/// Persist the current state to disk (call after any mutation). Non-blocking when
/// the background writer is running (just marks dirty + wakes it); synchronous
/// fallback otherwise so a mutation is never silently un-persisted.
pub fn persist(cloud: &Arc<CloudState>) {
    if let Some(p) = PERSISTER.get() {
        // Admission and the generation bump are one transaction with the
        // shutdown flush's final dirty==saved check. Once that check closes
        // admission, a late mutation producer cannot return and race process
        // exit; before it closes, every admitted generation is drained.
        let _target = admit_generation(p);
        p.cv.notify_one();
        return;
    }
    // Writer not started (early boot / tests) → write synchronously.
    let snap = capture(cloud);
    if let Err(e) = save(&snap) {
        tracing::warn!(error = %e, "failed to persist platform state");
    }
}

/// Persist a security decision before acknowledging it. Unlike [`persist`],
/// this serializes with the background writer and reports fsync failure to the
/// caller, which can then fail closed instead of minting a session whose replay
/// fact exists only in memory.
pub fn persist_durable(cloud: &Arc<CloudState>) -> bool {
    if let Some(p) = PERSISTER.get() {
        let _admitted = admit_generation(p);
        let _writer = p.writer.lock().unwrap();
        let target = p.dirty.load(Ordering::SeqCst);
        let snap = capture(&p.cloud);
        return match save(&snap) {
            Ok(()) => {
                p.saved.store(target, Ordering::SeqCst);
                true
            }
            Err(error) => {
                tracing::warn!(%error, "durable platform-state write failed");
                false
            }
        };
    }
    match save(&capture(cloud)) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "durable platform-state write failed");
            false
        }
    }
}

/// Synchronously drain every admitted generation, then atomically close
/// persistence admission before returning the exact final Guardian generation.
/// A producer whose mutation reaches `persist()` after that boundary waits until
/// process exit instead of acknowledging state absent from the terminal snapshot.
pub fn flush_blocking() -> anyhow::Result<Option<u64>> {
    let Some(p) = PERSISTER.get() else {
        return crate::guardian::close_replication_admission(None);
    };
    let timeout = std::env::var("HIVE_PERSIST_SHUTDOWN_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_secs(60));
    let started = std::time::Instant::now();
    loop {
        let write_result = {
            let _writer = p.writer.lock().unwrap();
            let target = p.dirty.load(Ordering::SeqCst);
            let snap = capture(&p.cloud);
            save_with_guardian_generation(&snap).map(|guardian_generation| {
                p.saved.store(target, Ordering::SeqCst);
                guardian_generation
            })
        };

        // persist() registers under this same lock. It therefore either bumped
        // dirty before this stable check (and forces another pass), or observes
        // closed admission and cannot return before process exit.
        let mut admission_open = p.lock.lock().unwrap();
        if let Ok(guardian_generation) = write_result {
            if p.dirty.load(Ordering::SeqCst) <= p.saved.load(Ordering::SeqCst) {
                *admission_open = false;
                drop(admission_open);
                return crate::guardian::close_replication_admission(guardian_generation);
            }
        } else if let Err(error) = &write_result {
            tracing::error!(%error, "shutdown platform-state write failed; retrying within bounded drain");
        }
        drop(admission_open);
        if started.elapsed() >= timeout {
            anyhow::bail!(
                "platform persistence drain timed out after {}ms (dirty={}, saved={})",
                timeout.as_millis(),
                p.dirty.load(Ordering::SeqCst),
                p.saved.load(Ordering::SeqCst)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Apply a loaded snapshot to a freshly constructed CloudState (boot restore).
pub fn restore(cloud: &Arc<CloudState>, snap: PlatformSnapshot) {
    cloud.projects.merge_synced(SyncedProjects {
        rows: snap.projects.into_iter().collect(),
        tombstones: snap.project_tombstones,
        incarnation_tombstones: snap.project_incarnation_tombstones,
    });
    let project_authority = cloud.projects.snapshot_synced();
    let mut deployments = snap.deployments;
    // Restore in chronological order so the newest becomes the default.
    deployments.sort_by_key(|d| d.created_at_ms);
    let before_authority_filter = deployments.len();
    deployments.retain(|record| match record.project_incarnation {
        Some(expected) => project_authority
            .rows
            .get(&record.project)
            .is_some_and(|row| row.incarnation == Some(expected)),
        None => match project_authority.rows.get(&record.project) {
            Some(row) => row.incarnation.is_none(),
            None => {
                !project_authority.tombstones.contains_key(&record.project)
                    && !project_authority
                        .incarnation_tombstones
                        .contains_key(&record.project)
            }
        },
    });
    let authority_rejected = before_authority_filter - deployments.len();
    if authority_rejected > 0 {
        tracing::warn!(
            count = authority_rejected,
            "persist::restore: skipped deployment records that lack active project-incarnation authority"
        );
    }
    let n = deployments.len();
    // Reconcile orphaned in-flight builds. A deployment/build persisted with
    // state Queued/Building was mid-flight in an async task on the PREVIOUS
    // process instance — that task died with the process, so on a fresh boot
    // this state can never legitimately still be in progress. Left as-is, it
    // stays "Building" forever (an infinite dashboard spinner with zero log
    // activity, no error, no way to retry) — live-witnessed: a preview
    // deployment orphaned by an unrelated node restart stayed stuck for 2+
    // hours, which is indistinguishable from "the build never starts" to the
    // user. Reconciling to Error on boot gives an actionable, retryable state
    // instead of a silent hang.
    let orphaned_count = deployments
        .iter()
        .filter(|d| {
            matches!(
                d.state,
                fluid_core::DeployState::Queued | fluid_core::DeployState::Building
            )
        })
        .count();
    // A `Building…` PLACEHOLDER is the one in-flight record that must be DROPPED
    // rather than reconciled. It is a shell (no git, no functions, a scratch root
    // holding one "Building…" page) whose build died with the previous process,
    // and `deploy_full` gave it the project's production alias because it was the
    // project's first deployment — so promoting it to `Error` makes the shell the
    // permanent owner of `<project>` on this node, which then serves it and
    // publishes a DNS affinity record for it while the real deployment sits on
    // whichever node placement chose. Witnessed on `archive-zip.shadw.app`
    // (2026-08-05). Dropping it costs nothing: no build can still be superseding
    // it, and `crate::git::reap_orphan_placeholders` removes the same shells on a
    // running node.
    let before = deployments.len();
    deployments.retain(|rec| {
        !(matches!(
            rec.state,
            fluid_core::DeployState::Queued | fluid_core::DeployState::Building
        ) && crate::git::is_placeholder_record(rec))
    });
    let dropped = before - deployments.len();
    for rec in &mut deployments {
        if matches!(
            rec.state,
            fluid_core::DeployState::Queued | fluid_core::DeployState::Building
        ) {
            rec.state = fluid_core::DeployState::Error;
        }
    }
    if dropped > 0 {
        tracing::warn!(
            count = dropped,
            "persist::restore: dropped orphaned Building… placeholder(s) (their builds died with a prior process; the project alias is freed for the real deployment)"
        );
    }
    if orphaned_count > dropped {
        tracing::warn!(
            count = orphaned_count - dropped,
            "persist::restore: reconciled orphaned in-flight deployment(s) to Error (interrupted by a prior node restart)"
        );
    }
    for rec in deployments {
        // Decide whether a persisted deployment can still serve after a restart:
        //   • container      → runs from a pre-built image; `root` irrelevant.
        //   • firecracker    → serves from the delivered microVM IMAGE, not the
        //                       host `root` build dir (which is scratch and may be
        //                       reaped by the build-dir GC). Restore if it has an
        //                       image — requiring `root` here wrongly dropped live
        //                       Firecracker deployments on restart (e.g. shoomoo).
        //   • mock (static/serverless) → serves files from `root`, so root must exist.
        let is_container = rec
            .manifest
            .functions
            .iter()
            .any(|f| f.runtime == "container");
        let fc_image_backed =
            cloud.gw.backend_name() == "firecracker" && rec.manifest.image.is_some();
        if is_container || fc_image_backed || std::path::Path::new(&rec.root).exists() {
            // Re-adopt any PUBLIC raw-port stamps this record carries into the
            // allocator registry (self-heal for a lost raw_ports.json): the
            // record and the registry are two durable copies of the same
            // claim, and a restored service must keep its port claimed so a
            // parallel deploy can't be handed it.
            crate::raw_ports::adopt_record(&rec);
            cloud.gw.restore(rec);
        }
    }
    // Re-apply every persisted custom-domain alias into the just-restored
    // `cloud.gw` — a runtime `POST /v1/projects/:p/domains` call only ever
    // mutated the in-memory alias table (`cloud.gw.add_alias`), never
    // anything `Gateway::restore` (above) replays; a bare-metal restart
    // silently DROPPED every custom domain until the next redeploy or a
    // fresh `add_alias` call. Live-witnessed standing up sms.shadw.cloud:
    // it worked fleet-wide, then vanished fleet-wide across an unrelated
    // binary roll's restarts. Best-effort — a domain whose project isn't
    // hosted on THIS node correctly no-ops (this node was never going to
    // serve it locally anyway; the gossiped route-table path handles that
    // case once the real host's own restore re-adds it).
    let mut healed_domains = 0u32;
    for (project, domain) in cloud.projects.all_domains() {
        // The heal must honor the ownership-verification gate exactly like a
        // fresh activation: a pending (never-proven) attach is settings
        // intent, not a route. Without this check every restart re-stole
        // routing for unproven attaches fleet-wide (adversarial finding:
        // attach-then-wait-for-a-roll hijacked any domain with zero proof).
        // Attachments from before the gate existed have NO verify record —
        // they are the deliberate grandfather clause and keep routing.
        let routable = match cloud.domains.verify_of(&domain) {
            Some(v) => v.status == "verified" && v.project == project,
            None => true, // grandfathered pre-verification attach
        };
        if !routable {
            continue;
        }
        if cloud.gw.add_alias(&domain, &project) {
            healed_domains += 1;
        }
    }
    if healed_domains > 0 {
        tracing::info!(
            count = healed_domains,
            "persist::restore: re-applied custom-domain aliases"
        );
    }
    // Warm the git-webhook reverse index (`gitops::GitRepoIndex`) from what's
    // already known locally at this point: every deployment this node hosts was
    // just restored into `cloud.gw` above, so `git_for_project_fleet`'s local
    // half is fully populated even though gossiped `peer_deployments` is still
    // empty this early in boot. Projects hosted only on peers self-heal into the
    // index as gossip arrives (see `main.rs`'s anti-entropy loop); `git_webhook`
    // also falls back to a full scan defensively for the window before this
    // call returns.
    cloud.git_index.rebuild(
        cloud.projects.snapshot().into_keys().filter_map(|p| {
            crate::admin::git_for_project_fleet(cloud, &p).map(|g| (p, g.repo_url))
        }),
    );
    if !snap.waf_rules.is_empty() {
        cloud.waf.set_rules(snap.waf_rules);
    }
    if let Some((enabled, limit, window_ms)) = snap.ratelimit {
        cloud.ratelimit.set(enabled, limit, window_ms);
    }
    // replace_all (deduped by id), NOT add() in a loop: add() pushes
    // unconditionally, so a snapshot that already carried a job — and every
    // prior restart re-persisted it — duplicated that job on every boot,
    // making the cron loop fire it N times per schedule (live-witnessed:
    // vc-shoomoo-0 present 3× on a node). replace_all converges the store.
    cloud.cron.replace_all(snap.cron);
    cloud.cron.recompute_all();
    cloud.router.set_redirects(snap.redirects);
    cloud.router.set_rewrites(snap.rewrites);
    // `restore` also runs from the asynchronous Guardian rollback guard, after
    // this process may already have accepted newer writes. Merge by the Team
    // aggregate's own generations; global snapshot `saved_ms` is not a causal
    // order for an individual team and must never wholesale-regress it.
    cloud.teams.merge_recovered(crate::teams::SyncedTeams {
        rows: snap.teams.into_iter().collect(),
        tombstones: snap.team_tombstones,
    });
    cloud.webhooks.load(snap.webhooks);
    cloud
        .databases
        .studio_replay_load(snap.database_studio_replay);
    cloud.databases.load(snap.databases);
    cloud.databases.data_load(snap.database_data);
    cloud.databases.tombstones_load(snap.database_tombstones);
    cloud.metrics.rollup_load(snap.metrics_rollup);
    // BuildStore::load() already reconciles Queued/Building -> Error for its
    // own per-build log records internally (git.rs) -- no duplicate needed
    // here, only the DeployRecord-side gap above was missing.
    cloud.builds.load(snap.builds);
    cloud.incidents.load(snap.incidents);
    cloud.push.load(snap.push);
    cloud.apikeys.load(snap.apikeys);
    cloud.integrations.load(snap.integrations);
    cloud.svcgraph.load(snap.svcgraphs);
    cloud.identity.load(snap.orgs, snap.users);
    cloud.billing.load(snap.billing, snap.billing_ledger);
    cloud.billing.invoices_load(snap.billing_invoices);
    // `None` (pre-upgrade snapshot / no snapshot) is passed through as-is:
    // meters_load must be the one place that decides what an absent watermark
    // set means, and it means UNKNOWN.
    cloud.billing.meters_load(snap.billing_meters);
    cloud.billing.checkouts_load(snap.billing_checkouts);
    cloud.domains.load(snap.domains);
    cloud.docs.load(snap.docs);
    cloud.gitops.load(snap.gitops);
    cloud.enterprise.load(snap.enterprise);
    cloud.sandboxes.load(
        snap.sandboxes.sandboxes,
        snap.sandboxes.commands,
        snap.sandboxes.snapshots,
        snap.sandboxes.mounts,
    );
    for def in snap.workflow_defs {
        cloud.workflows.define(def);
    }
    if n > 0 {
        tracing::info!(deployments = n, "restored platform state from disk");
    }
}
