//! Durable persistence for platform state: the atomic JSON snapshot `state.json` under `$HIVE_DATA`
//! (default `~/.hive-cloud`) that boot restores, per-tenant namespace documents under `ns/`, and
//! the GuardianDB replica (`guardian::replicate`), whose rollback guard restores a replica newer
//! than the disk.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use fluid_core::DeployRecord;
use hive_edge::{CronJob, Redirect, Rewrite, WafRule, WorkflowDef, WorkflowRun};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::project_settings::{ProjectSettings, SyncedProjects};
use crate::state::CloudState;

const GLOBAL_NAMESPACE: &str = "_global";
/// Namespace of a project absent from the snapshot: UNOWNED, never the owner's real "personal" one.
const UNOWNED_NAMESPACE: &str = "__untagged__";

#[derive(Default, Serialize, Deserialize)]
pub struct PlatformSnapshot {
    /// Save time (ms epoch): `guardian::spawn_restore_guard` restores a GuardianDB replica whose
    /// `saved_ms` is newer than the on-disk snapshot. Canonical Guardian payloads omit it and
    /// restore it from the iroh-docs entry timestamp.
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
    /// Durable on purpose: a node that forgets these re-imports every deleted database from any
    /// peer that still holds it.
    #[serde(default)]
    pub database_tombstones: std::collections::BTreeMap<String, u64>,
    /// One-use Studio replay facts are resource-independent so a live Guardian
    /// restore omitting a database record cannot erase them.
    #[serde(default)]
    pub database_studio_replay: crate::databases::StudioReplaySnapshot,
    /// Queue and consumer records plus their tombstones; message bodies are node-local and never
    /// part of this snapshot (see the `crate::queues` module doc).
    #[serde(default)]
    pub queues: crate::queues::SyncedQueues,
    /// What is deployed where, for node-death self-heal (`production_deployments`); rows and
    /// tombstones travel together.
    #[serde(default)]
    pub production_deployments: crate::production_deployments::SyncedProductionDeployments,
    /// Same rationale for the projects store (see `SyncedProjects`).
    #[serde(default)]
    pub project_tombstones: std::collections::BTreeMap<String, u64>,
    /// Durable like the delete tombstones: a restart must not un-reserve a renamed-away name, or
    /// someone could create it and inherit its traffic.
    #[serde(default)]
    pub project_rename_tombstones:
        std::collections::BTreeMap<String, crate::project_settings::ProjectRenameTombstone>,
    #[serde(default)]
    pub project_incarnation_tombstones: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<fluid_core::ProjectIncarnation, u64>,
    >,
    #[serde(default)]
    pub metrics_rollup: crate::metrics::RollupSnapshot,
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
    /// `None` means UNKNOWN (a snapshot from before this field), never "every tenant at zero":
    /// `BillingStore::meters_load` re-baselines on it. This build always writes `Some`.
    #[serde(default)]
    pub billing_meters: Option<Vec<crate::billing::MeterWatermark>>,
    /// Open (unconfirmed) checkouts, so a restart between the Stripe redirect and the return keeps
    /// `confirm_checkout` working.
    #[serde(default)]
    pub billing_checkouts: Vec<crate::billing::Checkout>,
    #[serde(default)]
    pub domains: Vec<crate::dns::DomainRecord>,
    #[serde(default)]
    pub docs: Vec<crate::docstore::Doc>,
    #[serde(default)]
    pub gitops: Vec<crate::gitops::GitOpsLink>,
    /// Workflow definitions, WDK-ingested graphs included: the manifest is ingested only during a
    /// live deploy.
    #[serde(default)]
    pub workflow_defs: Vec<WorkflowDef>,
    #[serde(default)]
    pub workflow_runs: Vec<WorkflowRun>,
    /// Enterprise feature suite state (secrets AEAD-encrypted in-struct). See
    /// [`crate::enterprise::EnterpriseSnapshot`].
    #[serde(default)]
    pub enterprise: crate::enterprise::EnterpriseSnapshot,
    /// Sandbox records only: live cell handles do not survive a restart and are re-provisioned on
    /// next use.
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

/// Persist the gossip-transport map (peer admin URL -> (node id, iroh addr JSON)); atomic, so safe
/// to call every gossip round.
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

/// Persist peers' GuardianDB addresses (node name -> serialized `iroh::EndpointAddr`), a SEPARATE
/// identity from the mesh addresses in `peer_iroh.json`. Loaded at boot to seed
/// `guardian::set_boot_seed_peers` before GuardianDB's one-time KV-store open; empty on a node's
/// first boot.
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

/// Write and fsync `tmp`, rename it over the state file, then fsync the directory: a rename is not
/// crash-durable until its directory entry is, and a power loss must not resurrect a row whose
/// tombstone was already acknowledged.
fn write_state_file_durably(
    dir: &std::path::Path,
    tmp: &std::path::Path,
    json: &str,
) -> std::io::Result<()> {
    {
        let f = std::fs::File::create(tmp)?;
        use std::io::Write;
        let mut w = std::io::BufWriter::new(&f);
        w.write_all(json.as_bytes())?;
        w.flush()?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, state_path())?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn save_with_guardian_generation(snap: &PlatformSnapshot) -> std::io::Result<Option<u64>> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join("state.json.tmp");
    let json = serde_json::to_string_pretty(snap).unwrap_or_else(|_| "{}".into());
    write_state_file_durably(&dir, &tmp, &json)?;
    save_namespaces(snap)?;
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

/// Partition the snapshot into per-tenant namespace documents; platform-level edge config (WAF,
/// cron, routing, incidents) lives under the reserved `_global` namespace. Team rows and tombstones
/// are included so a team-only mutation refreshes GuardianDB's full rollback snapshot. Set-valued
/// arrays are sorted (several stores are HashMap-backed, so capture order varies); WAF, redirect
/// and rewrite order is routing policy and stays as is.
pub fn namespaced(snap: &PlatformSnapshot) -> BTreeMap<String, Value> {
    let team_of = |project: &str| -> String {
        snap.projects
            .get(project)
            .map(|s| ns_norm(&s.team))
            .unwrap_or_else(|| UNOWNED_NAMESPACE.into())
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

    let global = docs.entry(GLOBAL_NAMESPACE.into()).or_default();
    global.insert("waf_rules".into(), json!(snap.waf_rules));
    global.insert("cron".into(), json!(snap.cron));
    global.insert("redirects".into(), json!(snap.redirects));
    global.insert("rewrites".into(), json!(snap.rewrites));
    global.insert("incidents".into(), json!(snap.incidents));

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

/// Guardian snapshot protocol v2's namespace projection: `namespaced` without the node-local
/// `cron`, project arrays keyed and sorted by project identity, malformed or duplicate identities
/// rejected.
pub fn guardian_v2_namespaced(snap: &PlatformSnapshot) -> anyhow::Result<BTreeMap<String, Value>> {
    let mut docs = namespaced(snap);
    if let Some(global) = docs.get_mut(GLOBAL_NAMESPACE) {
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
    let (billing_accounts, billing_ledger) = cloud.billing.snapshot();
    let SyncedProjects {
        rows: projects,
        tombstones: project_tombstones,
        rename_tombstones: project_rename_tombstones,
        incarnation_tombstones: project_incarnation_tombstones,
    } = cloud.projects.snapshot_synced();
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
        queues: cloud.queues.snapshot_synced(),
        production_deployments: cloud.production_deployments.snapshot_synced(),
        project_tombstones,
        project_rename_tombstones,
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
        billing_meters: Some(cloud.billing.meters_snapshot()),
        billing_checkouts: cloud.billing.checkouts_snapshot(),
        domains: cloud.domains.snapshot(),
        docs: cloud.docs.snapshot(),
        gitops: cloud.gitops.snapshot(),
        workflow_defs: cloud.workflows.defs(),
        workflow_runs: cloud.workflows.runs_snapshot(),
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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

/// Coalescing background writer on a dedicated OS thread (capture and fsync block): `persist()`
/// bumps the requested generation and wakes it; it always saves the newest state and saves again
/// when more mutations arrived during a save. Before it starts, `persist()` saves synchronously.
struct Persister {
    cloud: Arc<CloudState>,
    requested: AtomicU64,
    saved: AtomicU64,
    admission_open: Mutex<bool>,
    snapshot_writer: Mutex<()>,
    cv: Condvar,
}
static PERSISTER: OnceLock<Arc<Persister>> = OnceLock::new();

/// Start the background coalescing persister. Call ONCE at boot, after `restore`.
pub fn spawn_persister(cloud: Arc<CloudState>) {
    let p = Arc::new(Persister {
        cloud,
        requested: AtomicU64::new(0),
        saved: AtomicU64::new(0),
        admission_open: Mutex::new(true),
        snapshot_writer: Mutex::new(()),
        cv: Condvar::new(),
    });
    let already_started = PERSISTER.set(p.clone()).is_err();
    if already_started {
        return;
    }
    std::thread::Builder::new()
        .name("hive-persister".into())
        .spawn(move || loop {
            {
                let mut g = p.admission_open.lock().unwrap();
                while p.requested.load(Ordering::SeqCst) <= p.saved.load(Ordering::SeqCst) {
                    g = p.cv.wait(g).unwrap();
                }
            }
            let write_result = {
                let _writer = p.snapshot_writer.lock().unwrap();
                let target = p.requested.load(Ordering::SeqCst);
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
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
        .expect("spawn persister thread");
}

/// Logged-once latch: the first producer refused after the shutdown barrier warns, the rest stay
/// silent.
static PERSIST_REFUSED_WARNED: AtomicBool = AtomicBool::new(false);

/// Admit one generation, or `None` once `flush_blocking` has closed admission. Never waits: a
/// post-barrier producer is a tokio worker, and parking it in a condvar can stall the whole runtime
/// until systemd SIGKILLs the node.
fn admit_generation(p: &Persister) -> Option<u64> {
    let admission_open = p.admission_open.lock().unwrap();
    if !*admission_open {
        drop(admission_open);
        if !PERSIST_REFUSED_WARNED.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                "persist refused after the shutdown barrier; the terminal snapshot is already durable"
            );
        }
        return None;
    }
    Some(p.requested.fetch_add(1, Ordering::SeqCst).saturating_add(1))
}

/// Persist the current state (call after any mutation). Non-blocking while the background writer
/// runs (marks dirty and wakes it), so a crash loses at most the mutations the in-flight save had
/// not captured; before the writer starts it saves synchronously. A no-op once shutdown closed
/// admission (`admit_generation`).
pub fn persist(cloud: &Arc<CloudState>) {
    if let Some(p) = PERSISTER.get() {
        if admit_generation(p).is_some() {
            p.cv.notify_one();
        }
        return;
    }
    let snap = capture(cloud);
    if let Err(e) = save(&snap) {
        tracing::warn!(error = %e, "failed to persist platform state");
    }
}

/// Persist a security decision before acknowledging it. Unlike [`persist`] it serializes with the
/// background writer and reports an fsync failure, so the caller can fail closed instead of minting
/// a session whose replay fact exists only in memory. Runs under `block_in_place`: the capture
/// clones and serializes the whole state.
pub fn persist_durable(cloud: &Arc<CloudState>) -> bool {
    tokio::task::block_in_place(|| persist_durable_blocking(cloud))
}

fn persist_durable_blocking(cloud: &Arc<CloudState>) -> bool {
    if let Some(p) = PERSISTER.get() {
        if admit_generation(p).is_none() {
            return false;
        }
        let _writer = p.snapshot_writer.lock().unwrap();
        let target = p.requested.load(Ordering::SeqCst);
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

/// Synchronously drain every admitted generation, then close persistence admission and return the
/// final Guardian generation (wired to the shutdown path in `main`). A `persist()` after that
/// boundary is refused without waiting (`admit_generation`); admission closes under the lock
/// `persist()` registers under, so a concurrent `persist()` either forces another pass or is
/// refused. The audit log's writer thread is drained first (`audit::flush_blocking`): the audit
/// line of every mutation the final snapshot covers must be on disk before the process exits.
pub fn flush_blocking() -> anyhow::Result<Option<u64>> {
    crate::audit::flush_blocking();
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
            let _writer = p.snapshot_writer.lock().unwrap();
            let target = p.requested.load(Ordering::SeqCst);
            let snap = capture(&p.cloud);
            save_with_guardian_generation(&snap).map(|guardian_generation| {
                p.saved.store(target, Ordering::SeqCst);
                guardian_generation
            })
        };

        let mut admission_open = p.admission_open.lock().unwrap();
        if let Ok(guardian_generation) = write_result {
            if p.requested.load(Ordering::SeqCst) <= p.saved.load(Ordering::SeqCst) {
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
                p.requested.load(Ordering::SeqCst),
                p.saved.load(Ordering::SeqCst)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn in_flight(rec: &DeployRecord) -> bool {
    matches!(
        rec.state,
        fluid_core::DeployState::Queued | fluid_core::DeployState::Building
    )
}

/// A record still Queued or Building on a fresh boot was mid-flight in a task of the PREVIOUS
/// process, which died with it. A `Building…` placeholder (a shell with no git, no functions and
/// one scratch page) is DROPPED, because promoting it to `Error` would make the shell the permanent
/// owner of the project's alias on this node; every other in-flight record becomes `Error`, a
/// retryable state instead of a build that spins forever.
fn reconcile_orphaned_in_flight(mut deployments: Vec<DeployRecord>) -> Vec<DeployRecord> {
    let orphaned_count = deployments.iter().filter(|d| in_flight(d)).count();
    let before = deployments.len();
    deployments.retain(|rec| !(in_flight(rec) && crate::git::is_placeholder_record(rec)));
    let dropped = before - deployments.len();
    for rec in &mut deployments {
        if in_flight(rec) {
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
    deployments
}

/// Re-apply every persisted custom-domain alias into the just-restored `cloud.gw`:
/// `Gateway::restore` replays only deployments and a runtime domain attach mutated only the
/// in-memory alias table, so a restart would drop every custom domain until the next redeploy. A
/// domain whose project is not hosted on THIS node no-ops (the gossiped route table covers it once
/// the real host restores).
fn reapply_custom_domain_aliases(cloud: &Arc<CloudState>) {
    let mut healed_domains = 0u32;
    for (project, domain) in cloud.projects.all_domains() {
        if !attach_may_route(cloud, &domain, &project) {
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
}

/// The heal honors the ownership-verification gate like a fresh activation: a pending (never
/// proven) attach is settings intent, not a route. An attach with NO verify record predates the
/// gate and is grandfathered.
fn attach_may_route(cloud: &Arc<CloudState>, domain: &str, project: &str) -> bool {
    match cloud.domains.verify_of(domain) {
        Some(v) => v.status == "verified" && v.project == project,
        None => true,
    }
}

/// Warm the git-webhook reverse index from the local half only: gossiped `peer_deployments` is
/// still empty this early in boot, projects hosted only on peers join the index as gossip arrives,
/// and `git_webhook` falls back to a full scan while the index is empty.
fn warm_git_webhook_index(cloud: &Arc<CloudState>) {
    cloud.git_index.rebuild(
        cloud.projects.snapshot().into_keys().filter_map(|p| {
            crate::admin::git_for_project_fleet(cloud, &p).map(|g| (p, g.repo_url))
        }),
    );
}

/// Apply a loaded snapshot to a freshly constructed CloudState (boot restore). It also runs from
/// the asynchronous Guardian rollback guard after newer writes were accepted, so projects and teams
/// merge by their own generations: the global `saved_ms` is no causal order for one team and must
/// never wholesale-regress it. Deployments restore oldest first.
pub fn restore(cloud: &Arc<CloudState>, snap: PlatformSnapshot) {
    cloud
        .projects
        .rename_tombstones_load(snap.project_rename_tombstones.clone());
    cloud.projects.merge_synced(SyncedProjects {
        rows: snap.projects.into_iter().collect(),
        tombstones: snap.project_tombstones,
        rename_tombstones: snap.project_rename_tombstones,
        incarnation_tombstones: snap.project_incarnation_tombstones,
    });
    let project_authority = cloud.projects.snapshot_synced();
    let mut deployments = snap.deployments;
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
    let ready_for_authority = deployments
        .iter()
        .filter(|record| record.state == fluid_core::DeployState::Ready)
        .map(|record| {
            (
                record.id.clone(),
                record.project.clone(),
                record.created_at_ms,
            )
        })
        .collect::<Vec<_>>();
    let restore_authority = cloud
        .deployment_ledger
        .authorize_restore_batch(&ready_for_authority)
        .unwrap_or_else(|error| panic!("deployment restore authority failed closed: {error:#}"));
    let mut legacy_ready = 0usize;
    let before_readiness_filter = deployments.len();
    deployments.retain(|record| {
        if record.state != fluid_core::DeployState::Ready {
            return true;
        }
        match restore_authority.get(&record.id) {
            Some(crate::deployment_ledger::RestoreAuthority::Proven) => true,
            Some(crate::deployment_ledger::RestoreAuthority::LegacyMigration) => {
                legacy_ready += 1;
                true
            }
            Some(crate::deployment_ledger::RestoreAuthority::Refused) | None => false,
        }
    });
    let readiness_rejected = before_readiness_filter - deployments.len();
    if legacy_ready > 0 {
        tracing::warn!(
            count = legacy_ready,
            "persist::restore: preserved pre-ledger Ready deployment(s) as explicit legacy predecessors; they are not readiness proof and cannot authorize promotion"
        );
    }
    if readiness_rejected > 0 {
        tracing::error!(
            count = readiness_rejected,
            "persist::restore: refused Ready deployment(s) without published acceptance evidence"
        );
    }
    let n = deployments.len();
    let deployments = reconcile_orphaned_in_flight(deployments);
    for rec in deployments {
        let is_container = rec
            .manifest
            .functions
            .iter()
            .any(|f| f.runtime == "container");
        let fc_image_backed =
            cloud.gw.backend_name() == "firecracker" && rec.manifest.image.is_some();
        let serves_without_host_root = is_container || fc_image_backed;
        if serves_without_host_root || std::path::Path::new(&rec.root).exists() {
            crate::raw_ports::adopt_record(&rec);
            cloud.gw.restore(rec);
        }
    }
    reapply_custom_domain_aliases(cloud);
    warm_git_webhook_index(cloud);
    if !snap.waf_rules.is_empty() {
        cloud.waf.set_rules(snap.waf_rules);
    }
    if let Some((enabled, limit, window_ms)) = snap.ratelimit {
        cloud.ratelimit.set(enabled, limit, window_ms);
    }
    cloud.cron.replace_all(snap.cron);
    cloud.cron.recompute_all();
    cloud.router.set_redirects(snap.redirects);
    cloud.router.set_rewrites(snap.rewrites);
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
    cloud.queues.load(snap.queues);
    cloud.production_deployments.load(
        snap.production_deployments.rows.into_values().collect(),
        snap.production_deployments.tombstones,
    );
    cloud.metrics.rollup_load(snap.metrics_rollup);
    cloud.builds.load(snap.builds);
    let terminal_builds_to_replay_into_rows = cloud.builds.list();
    for build in terminal_builds_to_replay_into_rows {
        let Some(incarnation) = build.project_incarnation else {
            continue;
        };
        if let Some(outcome) = crate::project_settings::LastBuild::from_build(&build) {
            let _ = cloud
                .projects
                .record_last_build_exact(&build.project, incarnation, outcome);
        }
    }
    cloud.incidents.load(snap.incidents);
    cloud.push.load(snap.push);
    cloud.apikeys.load(snap.apikeys);
    cloud.integrations.load(snap.integrations);
    cloud.svcgraph.load(snap.svcgraphs);
    cloud.identity.load(snap.orgs, snap.users);
    cloud.billing.load(snap.billing, snap.billing_ledger);
    cloud.billing.invoices_load(snap.billing_invoices);
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
    cloud.workflows.runs_load(snap.workflow_runs);
    if n > 0 {
        tracing::info!(deployments = n, "restored platform state from disk");
    }
}
