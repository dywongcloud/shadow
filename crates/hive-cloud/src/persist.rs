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

use crate::project_settings::ProjectSettings;
use crate::state::CloudState;

#[derive(Default, Serialize, Deserialize)]
pub struct PlatformSnapshot {
    /// When this snapshot was SAVED (ms epoch). The guardian restore-on-rollback
    /// guard compares this against the replicated copy to detect a local snapshot
    /// that regressed (older file restored after a crash / disk swap): if the
    /// GuardianDB replica is NEWER than what's on disk, the replica wins at boot.
    #[serde(default)]
    pub saved_ms: u64,
    #[serde(default)]
    pub deployments: Vec<DeployRecord>,
    #[serde(default)]
    pub projects: HashMap<String, ProjectSettings>,
    #[serde(default)]
    pub waf_rules: Vec<WafRule>,
    #[serde(default)]
    pub cron: Vec<CronJob>,
    #[serde(default)]
    pub redirects: Vec<Redirect>,
    #[serde(default)]
    pub rewrites: Vec<Rewrite>,
    #[serde(default)]
    pub teams: HashMap<String, crate::teams::Team>,
    #[serde(default)]
    pub webhooks: Vec<crate::webhooks::Webhook>,
    #[serde(default)]
    pub databases: Vec<crate::databases::Database>,
    /// Durable data for the in-process stores (queue + vector) so they survive a
    /// restart like blob (disk) and the DB records themselves.
    #[serde(default)]
    pub database_data: crate::databases::DataSnapshot,
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
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join("peer_iroh.json.tmp");
    let Ok(json) = serde_json::to_string(map) else { return };
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, peer_iroh_path());
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
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join("peer_guardian_addr.json.tmp");
    let Ok(json) = serde_json::to_string(map) else { return };
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, peer_guardian_addr_path());
    }
}

/// Load the persisted peer GuardianDB-address map (empty if none).
pub fn load_peer_guardian_addr() -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(peer_guardian_addr_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
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

/// Atomically write the snapshot to disk.
pub fn save(snap: &PlatformSnapshot) -> std::io::Result<()> {
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
    // Write per-tenant namespace documents (the multi-tenant schema partition).
    let _ = save_namespaces(snap);
    // Replicate into the always-on GuardianDB (durable + peer-replicated copy).
    crate::guardian::replicate(snap);
    Ok(())
}

/// Normalize a tenant namespace: empty => "personal".
fn ns_norm(team: &str) -> String {
    if team.trim().is_empty() { "personal".into() } else { team.trim().to_string() }
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
        snap.projects.get(project).map(|s| ns_norm(&s.team)).unwrap_or_else(|| "__untagged__".into())
    };
    let mut docs: BTreeMap<String, serde_json::Map<String, Value>> = BTreeMap::new();
    let mut push = |ns: String, key: &str, val: Value| {
        let doc = docs.entry(ns).or_default();
        doc.entry(key.to_string()).or_insert_with(|| json!([])).as_array_mut().unwrap().push(val);
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
    // Note: teams are intentionally NOT a guardian-db collection — orgs/users
    // (synced from the identity provider) are the source of truth for tenancy.

    // Platform-level config under the reserved global namespace.
    let global = docs.entry("_global".into()).or_default();
    global.insert("waf_rules".into(), json!(snap.waf_rules));
    global.insert("cron".into(), json!(snap.cron));
    global.insert("redirects".into(), json!(snap.redirects));
    global.insert("rewrites".into(), json!(snap.rewrites));
    global.insert("incidents".into(), json!(snap.incidents));

    docs.into_iter().map(|(k, v)| (k, Value::Object(v))).collect()
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
    PlatformSnapshot {
        saved_ms: hive_core::now_ms(),
        deployments: cloud.gw.deployment_records(),
        projects: cloud.projects.snapshot(),
        waf_rules: cloud.waf.rules(),
        cron: cloud.cron.list(),
        redirects: cloud.router.redirects(),
        rewrites: cloud.router.rewrites(),
        teams: cloud.teams.snapshot(),
        webhooks: cloud.webhooks.snapshot(),
        databases: cloud.databases.snapshot(),
        database_data: cloud.databases.data_snapshot(),
        metrics_rollup: cloud.metrics.rollup_snapshot(),
        builds: cloud.builds.snapshot(),
        incidents: cloud.incidents.snapshot(),
        push: cloud.push.snapshot(),
        apikeys: cloud.apikeys.snapshot(),
        integrations: cloud.integrations.snapshot(),
        svcgraphs: cloud.svcgraph.snapshot(),
        orgs: cloud.identity.orgs(),
        users: cloud.identity.users(),
        billing: cloud.billing.snapshot().0,
        billing_ledger: cloud.billing.snapshot().1,
        billing_invoices: cloud.billing.invoices_snapshot(),
        domains: cloud.domains.snapshot(),
        docs: cloud.docs.snapshot(),
        gitops: cloud.gitops.snapshot(),
        workflow_defs: cloud.workflows.defs(),
        enterprise: cloud.enterprise.snapshot(),
        sandboxes: {
            let (sandboxes, commands, snapshots, mounts) = cloud.sandboxes.snapshot();
            crate::sandboxes::SandboxesSnapshot { sandboxes, commands, snapshots, mounts }
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
    dirty: AtomicU64, // bumped on each persist() request
    saved: AtomicU64, // highest generation durably written
    lock: Mutex<()>,  // pairs with `cv` (marker mutex; does NOT guard state)
    cv: Condvar,
}
static PERSISTER: OnceLock<Arc<Persister>> = OnceLock::new();

/// Start the background coalescing persister. Call ONCE at boot, after `restore`.
pub fn spawn_persister(cloud: Arc<CloudState>) {
    let p = Arc::new(Persister {
        cloud,
        dirty: AtomicU64::new(0),
        saved: AtomicU64::new(0),
        lock: Mutex::new(()),
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
            // Drain: coalesce everything up to the newest generation seen now.
            let target = p.dirty.load(Ordering::SeqCst);
            let snap = capture(&p.cloud);
            match save(&snap) {
                Ok(()) => p.saved.store(target, Ordering::SeqCst),
                Err(e) => {
                    tracing::warn!(error = %e, "persist(bg) failed; will retry on next mutation");
                    // Don't advance `saved` → the next persist() re-triggers a write.
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        })
        .expect("spawn persister thread");
}

/// Persist the current state to disk (call after any mutation). Non-blocking when
/// the background writer is running (just marks dirty + wakes it); synchronous
/// fallback otherwise so a mutation is never silently un-persisted.
pub fn persist(cloud: &Arc<CloudState>) {
    if let Some(p) = PERSISTER.get() {
        // Bump under the lock so the writer can't miss the wakeup between its
        // predicate check and `wait`.
        {
            let _g = p.lock.lock().unwrap();
            p.dirty.fetch_add(1, Ordering::SeqCst);
        }
        p.cv.notify_one();
        return;
    }
    // Writer not started (early boot / tests) → write synchronously.
    let snap = capture(cloud);
    if let Err(e) = save(&snap) {
        tracing::warn!(error = %e, "failed to persist platform state");
    }
}

/// Synchronously flush the latest state NOW (graceful shutdown). Safe to call even
/// if the background writer is running — it writes the current state directly.
pub fn flush_blocking() {
    if let Some(p) = PERSISTER.get() {
        let target = p.dirty.load(Ordering::SeqCst);
        let snap = capture(&p.cloud);
        if save(&snap).is_ok() {
            p.saved.store(target, Ordering::SeqCst);
        }
    }
}

/// Apply a loaded snapshot to a freshly constructed CloudState (boot restore).
pub fn restore(cloud: &Arc<CloudState>, snap: PlatformSnapshot) {
    let mut deployments = snap.deployments;
    // Restore in chronological order so the newest becomes the default.
    deployments.sort_by_key(|d| d.created_at_ms);
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
    let orphaned_count = deployments.iter().filter(|d| matches!(d.state, fluid_core::DeployState::Queued | fluid_core::DeployState::Building)).count();
    for rec in &mut deployments {
        if matches!(rec.state, fluid_core::DeployState::Queued | fluid_core::DeployState::Building) {
            rec.state = fluid_core::DeployState::Error;
        }
    }
    if orphaned_count > 0 {
        tracing::warn!(count = orphaned_count, "persist::restore: reconciled orphaned in-flight deployment(s) to Error (interrupted by a prior node restart)");
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
        let is_container = rec.manifest.functions.iter().any(|f| f.runtime == "container");
        let fc_image_backed = cloud.gw.backend_name() == "firecracker" && rec.manifest.image.is_some();
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
    cloud.projects.load(snap.projects);
    // Warm the git-webhook reverse index (`gitops::GitRepoIndex`) from what's
    // already known locally at this point: every deployment this node hosts was
    // just restored into `cloud.gw` above, so `git_for_project_fleet`'s local
    // half is fully populated even though gossiped `peer_deployments` is still
    // empty this early in boot. Projects hosted only on peers self-heal into the
    // index as gossip arrives (see `main.rs`'s anti-entropy loop); `git_webhook`
    // also falls back to a full scan defensively for the window before this
    // call returns.
    cloud.git_index.rebuild(
        cloud
            .projects
            .snapshot()
            .into_keys()
            .filter_map(|p| crate::admin::git_for_project_fleet(cloud, &p).map(|g| (p, g.repo_url))),
    );
    if !snap.waf_rules.is_empty() {
        cloud.waf.set_rules(snap.waf_rules);
    }
    // replace_all (deduped by id), NOT add() in a loop: add() pushes
    // unconditionally, so a snapshot that already carried a job — and every
    // prior restart re-persisted it — duplicated that job on every boot,
    // making the cron loop fire it N times per schedule (live-witnessed:
    // vc-shoomoo-0 present 3× on a node). replace_all converges the store.
    cloud.cron.replace_all(snap.cron);
    cloud.router.set_redirects(snap.redirects);
    cloud.router.set_rewrites(snap.rewrites);
    if !snap.teams.is_empty() {
        cloud.teams.load(snap.teams);
    }
    cloud.webhooks.load(snap.webhooks);
    cloud.databases.load(snap.databases);
    cloud.databases.data_load(snap.database_data);
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
    cloud.domains.load(snap.domains);
    cloud.docs.load(snap.docs);
    cloud.gitops.load(snap.gitops);
    cloud.enterprise.load(snap.enterprise);
    cloud.sandboxes.load(snap.sandboxes.sandboxes, snap.sandboxes.commands, snap.sandboxes.snapshots, snap.sandboxes.mounts);
    for def in snap.workflow_defs {
        cloud.workflows.define(def);
    }
    if n > 0 {
        tracing::info!(deployments = n, "restored platform state from disk");
    }
}
