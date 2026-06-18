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
use hive_edge::{CronJob, Redirect, Rewrite, WafRule};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::project_settings::ProjectSettings;
use crate::state::CloudState;

#[derive(Default, Serialize, Deserialize)]
pub struct PlatformSnapshot {
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
    #[serde(default)]
    pub incidents: Vec<crate::incidents::Incident>,
    #[serde(default)]
    pub apikeys: Vec<crate::apikeys::ApiKey>,
    #[serde(default)]
    pub orgs: Vec<crate::identity::OrgRecord>,
    #[serde(default)]
    pub users: Vec<crate::identity::UserRecord>,
    #[serde(default)]
    pub billing: Vec<crate::billing::BillingAccount>,
    #[serde(default)]
    pub billing_ledger: Vec<crate::billing::LedgerEntry>,
}

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("HIVE_DATA") {
        return PathBuf::from(d);
    }
    dirs_home().join(".hive-cloud")
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
    #[cfg(feature = "guardian")]
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
    let team_of = |project: &str| -> String {
        snap.projects.get(project).map(|s| ns_norm(&s.team)).unwrap_or_else(|| "personal".into())
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
    for w in &snap.webhooks {
        push(team_of(&w.project), "webhooks", json!(w));
    }
    for o in &snap.orgs {
        push(ns_norm(&o.tenant), "orgs", json!(o));
    }
    for u in &snap.users {
        push(ns_norm(&u.tenant), "users", json!(u));
    }
    for (slug, t) in &snap.teams {
        docs.entry(ns_norm(slug)).or_default().insert("team".into(), json!(t));
    }

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
        deployments: cloud.gw.deployment_records(),
        projects: cloud.projects.snapshot(),
        waf_rules: cloud.waf.rules(),
        cron: cloud.cron.list(),
        redirects: cloud.router.redirects(),
        rewrites: cloud.router.rewrites(),
        teams: cloud.teams.snapshot(),
        webhooks: cloud.webhooks.snapshot(),
        databases: cloud.databases.snapshot(),
        incidents: cloud.incidents.snapshot(),
        apikeys: cloud.apikeys.snapshot(),
        orgs: cloud.identity.orgs(),
        users: cloud.identity.users(),
        billing: cloud.billing.snapshot().0,
        billing_ledger: cloud.billing.snapshot().1,
    }
}

/// Persist the current state to disk (call after any mutation).
pub fn persist(cloud: &Arc<CloudState>) {
    let snap = capture(cloud);
    if let Err(e) = save(&snap) {
        tracing::warn!(error = %e, "failed to persist platform state");
    }
}

/// Apply a loaded snapshot to a freshly constructed CloudState (boot restore).
pub fn restore(cloud: &Arc<CloudState>, snap: PlatformSnapshot) {
    let mut deployments = snap.deployments;
    // Restore in chronological order so the newest becomes the default.
    deployments.sort_by_key(|d| d.created_at_ms);
    let n = deployments.len();
    for rec in deployments {
        // Only restore deployments whose files still exist on disk.
        if std::path::Path::new(&rec.root).exists() {
            cloud.gw.restore(rec);
        }
    }
    cloud.projects.load(snap.projects);
    if !snap.waf_rules.is_empty() {
        cloud.waf.set_rules(snap.waf_rules);
    }
    for j in snap.cron {
        let _ = cloud.cron.add(j);
    }
    cloud.router.set_redirects(snap.redirects);
    cloud.router.set_rewrites(snap.rewrites);
    if !snap.teams.is_empty() {
        cloud.teams.load(snap.teams);
    }
    cloud.webhooks.load(snap.webhooks);
    cloud.databases.load(snap.databases);
    cloud.incidents.load(snap.incidents);
    cloud.apikeys.load(snap.apikeys);
    cloud.identity.load(snap.orgs, snap.users);
    cloud.billing.load(snap.billing, snap.billing_ledger);
    if n > 0 {
        tracing::info!(deployments = n, "restored platform state from disk");
    }
}
