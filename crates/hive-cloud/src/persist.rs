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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use fluid_core::DeployRecord;
use hive_edge::{CronJob, Redirect, Rewrite, WafRule};
use serde::{Deserialize, Serialize};

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
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, state_path())?;
    #[cfg(feature = "guardian")]
    crate::guardian::replicate(snap);
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
    if n > 0 {
        tracing::info!(deployments = n, "restored platform state from disk");
    }
}
