//! Incidents: the operational record the platform owner manages from the ops dashboard (severity,
//! lifecycle, timeline). An OPERATOR incident (`POST /v1/incidents`, [`IncidentStore::open_new`])
//! is declared and closed by a person and never expires. An AUTOMATED incident
//! ([`IncidentStore::open`]) is the level-triggered projection of a CONDITION: the opener
//! re-asserts it on EVERY pass, declares how long one observation stays valid (`OpenReq::ttl_ms`)
//! and calls [`IncidentStore::clear`] where the condition is gone; the leader-only
//! [`spawn_reconciler`] resolves lapsed ones, collapses duplicates and bounds the history.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Minor,
    Major,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncidentStatus {
    Investigating,
    Identified,
    Monitoring,
    Resolved,
}

/// Who owns an incident's lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Declared by a person through `POST /v1/incidents`; closed by a person.
    Operator,
    /// Opened by a reconcile loop for a condition it observes; resolves itself once the condition
    /// is no longer observed. The default for rows persisted before this field existed.
    #[default]
    Automated,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IncidentUpdate {
    pub ts_ms: u64,
    pub status: IncidentStatus,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub status: IncidentStatus,
    /// Affected regions or components.
    #[serde(default)]
    pub affected: Vec<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub updates: Vec<IncidentUpdate>,
    #[serde(default)]
    pub origin: Origin,
    /// Stable key of the observed condition (automated incidents); absent on older rows, where
    /// [`Incident::key`] derives it from title + affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Last time the opener observed the condition (re-asserted it). `0` on
    /// legacy rows, where `updated_ms` stands in.
    #[serde(default)]
    pub observed_ms: u64,
    /// When an automated incident not re-observed may be auto-resolved: `0` = never (operator
    /// incident); a legacy automated row lapses [`DEFAULT_TTL_MS`] after its last update.
    #[serde(default)]
    pub expires_ms: u64,
}

impl Incident {
    /// The condition this incident projects.
    pub fn key(&self) -> String {
        self.condition
            .clone()
            .unwrap_or_else(|| derived_key(&self.title, &self.affected))
    }

    fn is_open(&self) -> bool {
        self.status != IncidentStatus::Resolved
    }

    fn is_open_automated(&self) -> bool {
        self.is_open() && self.origin == Origin::Automated
    }

    fn observed_or_updated(&self) -> u64 {
        if self.observed_ms > 0 {
            self.observed_ms
        } else {
            self.updated_ms
        }
    }

    /// The instant this automated incident's last observation lapses.
    fn lapses_ms(&self) -> u64 {
        if self.expires_ms > 0 {
            self.expires_ms
        } else {
            self.observed_or_updated().saturating_add(DEFAULT_TTL_MS)
        }
    }
}

fn derived_key(title: &str, affected: &[String]) -> String {
    let mut a = affected.to_vec();
    a.sort();
    format!("{title}\u{0}{}", a.join(","))
}

#[derive(Serialize, Deserialize)]
pub struct OpenReq {
    pub title: String,
    pub severity: Severity,
    #[serde(default)]
    pub affected: Vec<String>,
    #[serde(default)]
    pub message: String,
    /// Stable condition key for an automated opener (`dns:geo-hold`,
    /// `listener:<node>:<port>:<pid>`); empty = derived from title + affected. Ignored by
    /// `open_new`.
    #[serde(default)]
    pub condition: String,
    /// How long one observation stays valid without re-assertion (three passes of the opener's
    /// cadence); `0` = [`DEFAULT_TTL_MS`]. Ignored by `open_new`.
    #[serde(default)]
    pub ttl_ms: u64,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateReq {
    pub status: IncidentStatus,
    pub message: String,
}

/// Observation validity for an automated opener that declares none, and for
/// legacy rows (measured from their last update).
pub const DEFAULT_TTL_MS: u64 = 15 * 60 * 1000;

const PUBLIC_BOARD_RESOLVED_LIMIT: usize = 20;

/// Which rows a paged read returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Open,
    Resolved,
    All,
}

impl Filter {
    pub fn parse(s: Option<&str>) -> Filter {
        match s {
            Some("open") => Filter::Open,
            Some("resolved") => Filter::Resolved,
            _ => Filter::All,
        }
    }

    fn admits(self, inc: &Incident) -> bool {
        match self {
            Filter::Open => inc.is_open(),
            Filter::Resolved => !inc.is_open(),
            Filter::All => true,
        }
    }
}

/// What one reconciler pass changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Open automated incidents whose observation lapsed and were resolved.
    pub expired: usize,
    /// Duplicate open rows of one condition folded into the earliest.
    pub collapsed: usize,
    /// Resolved automated rows dropped past the history cap or retention.
    pub pruned: usize,
}

impl ReconcileReport {
    pub fn changed(&self) -> bool {
        self.expired + self.collapsed + self.pruned > 0
    }
}

pub struct IncidentStore {
    items: RwLock<Vec<Incident>>,
}

impl IncidentStore {
    pub fn new() -> IncidentStore {
        IncidentStore {
            items: RwLock::new(Vec::new()),
        }
    }

    pub fn list(&self) -> Vec<Incident> {
        let mut v = self.items.read().clone();
        v.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
        v
    }

    /// One page, newest first, as `(rows, matching, open)`: `matching` counts every row `filter`
    /// admits and `open` every unresolved row.
    pub fn page(&self, filter: Filter, limit: usize, offset: usize) -> (Vec<Incident>, usize, usize) {
        let items = self.items.read();
        let open = items.iter().filter(|i| i.is_open()).count();
        let mut rows: Vec<&Incident> = items.iter().filter(|i| filter.admits(i)).collect();
        let matching = rows.len();
        rows.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
        let page = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();
        (page, matching, open)
    }

    /// The public status board: OPERATOR incidents only (open plus the newest
    /// `PUBLIC_BOARD_RESOLVED_LIMIT` resolved); automated rows carry node names, pids and command
    /// lines and never leave the console.
    pub fn public_board(&self) -> Vec<Incident> {
        let items = self.items.read();
        let mut rows: Vec<&Incident> = items
            .iter()
            .filter(|i| i.origin == Origin::Operator)
            .collect();
        rows.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        let mut out: Vec<Incident> = Vec::new();
        let mut resolved = 0;
        for inc in rows {
            if inc.is_open() {
                out.push(inc.clone());
            } else if resolved < PUBLIC_BOARD_RESOLVED_LIMIT {
                resolved += 1;
                out.push(inc.clone());
            }
        }
        out
    }

    pub fn open_count(&self) -> usize {
        self.items.read().iter().filter(|i| i.is_open()).count()
    }

    pub fn snapshot(&self) -> Vec<Incident> {
        self.items.read().clone()
    }

    pub fn load(&self, data: Vec<Incident>) {
        *self.items.write() = data;
    }

    /// Observe a condition: open `req` as an AUTOMATED incident or re-assert the unresolved one for
    /// the same condition (refreshing `observed_ms`/`expires_ms`, no timeline entry) and return it,
    /// under ONE write lock so concurrent openers get one incident. Openers call this every pass;
    /// stopping is how it resolves.
    pub fn open(&self, req: OpenReq) -> Incident {
        let now = now_ms();
        let ttl = if req.ttl_ms == 0 {
            DEFAULT_TTL_MS
        } else {
            req.ttl_ms
        };
        let key = if req.condition.is_empty() {
            derived_key(&req.title, &req.affected)
        } else {
            req.condition.clone()
        };
        let mut items = self.items.write();
        if let Some(inc) = items.iter_mut().find(|i| i.is_open() && i.key() == key) {
            inc.updated_ms = now;
            inc.observed_ms = now;
            inc.expires_ms = now.saturating_add(ttl);
            if inc.condition.is_none() {
                inc.condition = Some(key);
            }
            return inc.clone();
        }
        let mut inc = Self::build(req, Origin::Automated);
        inc.condition = Some(key);
        inc.observed_ms = now;
        inc.expires_ms = now.saturating_add(ttl);
        items.push(inc.clone());
        inc
    }

    /// Always open a NEW OPERATOR incident — `POST /v1/incidents`. Never for an
    /// automated opener (use [`Self::open`]).
    pub fn open_new(&self, req: OpenReq) -> Incident {
        let inc = Self::build(req, Origin::Operator);
        self.items.write().push(inc.clone());
        inc
    }

    /// The condition is gone: resolve every open automated incident for `condition` with `message`.
    /// Idempotent and cheap, so openers call it on every healthy pass; returns how many it
    /// resolved.
    pub fn clear(&self, condition: &str, message: &str) -> usize {
        let now = now_ms();
        let mut n = 0;
        for inc in self
            .items
            .write()
            .iter_mut()
            .filter(|i| i.is_open_automated() && i.key() == condition)
        {
            resolve_in_place(inc, now, message.to_string());
            n += 1;
        }
        n
    }

    fn build(req: OpenReq, origin: Origin) -> Incident {
        let now = now_ms();
        Incident {
            id: format!("inc_{}", uuid::Uuid::new_v4().simple()),
            title: req.title,
            severity: req.severity,
            status: IncidentStatus::Investigating,
            affected: req.affected,
            created_ms: now,
            updated_ms: now,
            updates: vec![IncidentUpdate {
                ts_ms: now,
                status: IncidentStatus::Investigating,
                message: if req.message.is_empty() {
                    "Incident opened.".into()
                } else {
                    req.message
                },
            }],
            origin,
            condition: None,
            observed_ms: 0,
            expires_ms: 0,
        }
    }

    pub fn update(&self, id: &str, req: UpdateReq) -> Option<Incident> {
        let now = now_ms();
        let mut items = self.items.write();
        let inc = items.iter_mut().find(|i| i.id == id)?;
        inc.status = req.status;
        inc.updated_ms = now;
        inc.updates.push(IncidentUpdate {
            ts_ms: now,
            status: req.status,
            message: req.message,
        });
        Some(inc.clone())
    }

    /// Remove an incident entirely (`update` only transitions its status); followers adopt the
    /// leader's post-delete snapshot wholesale.
    pub fn remove(&self, id: &str) -> Option<Incident> {
        let mut items = self.items.write();
        let pos = items.iter().position(|i| i.id == id)?;
        Some(items.remove(pos))
    }

    /// One leader-only reconciler pass: COLLAPSE open automated duplicates of one condition into
    /// the earliest, EXPIRE open automated rows whose observation lapsed (with a timeline entry
    /// naming the staleness), PRUNE resolved automated rows past `history_max` or `retention_ms`.
    /// Operator rows are never pruned.
    pub fn reconcile(&self, now: u64, history_max: usize, retention_ms: u64) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let mut items = self.items.write();

        let mut survivor: HashMap<String, usize> = HashMap::new();
        for (i, inc) in items.iter().enumerate() {
            if !inc.is_open_automated() {
                continue;
            }
            let key = inc.key();
            match survivor.get(&key) {
                Some(&s) if items[s].created_ms <= inc.created_ms => {}
                _ => {
                    survivor.insert(key, i);
                }
            }
        }
        let mut folded: HashMap<usize, (usize, u64)> = HashMap::new();
        let mut drop = vec![false; items.len()];
        for (i, inc) in items.iter().enumerate() {
            if !inc.is_open_automated() {
                continue;
            }
            let s = survivor[&inc.key()];
            if s == i {
                continue;
            }
            drop[i] = true;
            let e = folded.entry(s).or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(inc.observed_or_updated());
        }
        for (s, (n, latest)) in folded {
            let inc = &mut items[s];
            inc.observed_ms = inc.observed_or_updated().max(latest);
            inc.updated_ms = inc.updated_ms.max(latest);
            inc.updates.push(IncidentUpdate {
                ts_ms: now,
                status: inc.status,
                message: format!(
                    "Collapsed {n} duplicate record(s) of this condition into this incident \
                     (they were opened before deduplication existed)."
                ),
            });
            report.collapsed += n;
        }
        retain_kept(&mut items, &drop);

        for inc in items.iter_mut() {
            if !inc.is_open_automated() {
                continue;
            }
            let lapses = inc.lapses_ms();
            if lapses > now {
                continue;
            }
            let observed = inc.observed_or_updated();
            let window = lapses.saturating_sub(observed);
            resolve_in_place(
                inc,
                now,
                format!(
                    "Auto-resolved: the condition was last observed {} ago and has not been \
                     re-observed within its {} window.",
                    human(now.saturating_sub(observed)),
                    human(window)
                ),
            );
            report.expired += 1;
        }

        let mut resolved: Vec<(u64, usize)> = items
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.is_open() && i.origin == Origin::Automated)
            .map(|(ix, i)| (i.updated_ms, ix))
            .collect();
        resolved.sort_by(|a, b| b.0.cmp(&a.0));
        let mut drop = vec![false; items.len()];
        for (rank, (updated, ix)) in resolved.into_iter().enumerate() {
            if rank >= history_max || now.saturating_sub(updated) > retention_ms {
                drop[ix] = true;
                report.pruned += 1;
            }
        }
        retain_kept(&mut items, &drop);
        report
    }
}

fn retain_kept(items: &mut Vec<Incident>, drop: &[bool]) {
    if !drop.iter().any(|d| *d) {
        return;
    }
    let mut i = 0;
    items.retain(|_| {
        let keep = !drop[i];
        i += 1;
        keep
    });
}

fn resolve_in_place(inc: &mut Incident, now: u64, message: String) {
    inc.status = IncidentStatus::Resolved;
    inc.updated_ms = now;
    inc.updates.push(IncidentUpdate {
        ts_ms: now,
        status: IncidentStatus::Resolved,
        message,
    });
}

fn human(ms: u64) -> String {
    let s = ms / 1000;
    if s < 120 {
        format!("{s}s")
    } else if s < 2 * 3600 {
        format!("{}m", s / 60)
    } else if s < 2 * 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

impl Default for IncidentStore {
    fn default() -> Self {
        Self::new()
    }
}

const DEFAULT_RECONCILE_SECS: u64 = 30;
const DEFAULT_HISTORY_MAX: u64 = 500;
const DEFAULT_RETENTION_SECS: u64 = 7 * 86_400;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// The leader-only reconciler: every `HIVE_INCIDENT_RECONCILE_SECS` runs
/// [`IncidentStore::reconcile`] with `HIVE_INCIDENT_HISTORY_MAX`/`HIVE_INCIDENT_RETENTION_SECS`
/// under `leadership::may_act(IncidentReconcile)`, persisting after any change.
pub fn spawn_reconciler(cloud: Arc<crate::state::CloudState>) {
    crate::supervise::spawn_supervised("incident-reconciler", move || {
        let cloud = cloud.clone();
        async move {
            let every = env_u64("HIVE_INCIDENT_RECONCILE_SECS", DEFAULT_RECONCILE_SECS);
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(every));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                crate::supervise::beat("incident-reconciler");
                if !crate::leadership::may_act(&cloud, crate::leadership::Job::IncidentReconcile) {
                    continue;
                }
                let history_max =
                    env_u64("HIVE_INCIDENT_HISTORY_MAX", DEFAULT_HISTORY_MAX) as usize;
                let retention_ms =
                    env_u64("HIVE_INCIDENT_RETENTION_SECS", DEFAULT_RETENTION_SECS) * 1000;
                let report = cloud
                    .incidents
                    .reconcile(now_ms(), history_max, retention_ms);
                if report.changed() {
                    tracing::info!(
                        expired = report.expired,
                        collapsed = report.collapsed,
                        pruned = report.pruned,
                        open = cloud.incidents.open_count(),
                        "incident reconciler: automated incidents reconciled against their observations"
                    );
                    crate::persist::persist(&cloud);
                }
            }
        }
    });
}
