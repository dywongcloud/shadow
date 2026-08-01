//! Cron — scheduled function invocations. Jobs hold a standard cron expression;
//! `tick` returns the jobs due now and advances their next run. The host wires
//! each due job to an actual function invocation.

use chrono::{TimeZone, Utc};
use cron::Schedule;
use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    /// Cron expression: `sec min hour day-of-month month day-of-week`.
    pub schedule: String,
    pub deployment: String,
    /// Function route to hit, e.g. `/api/cron`.
    pub path: String,
    #[serde(default = "crate::default_true_pub")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run_ms: Option<u64>,
    #[serde(default)]
    pub next_run_ms: Option<u64>,
    #[serde(default)]
    pub runs: u64,
    /// Where the job came from: `"manual"` (created via the API/UI) or
    /// `"vercel.json"` (declared in a deployment's config). Used to replace the
    /// config-sourced set on each deploy without touching manual jobs.
    #[serde(default = "default_cron_source")]
    pub source: String,
    /// Owning team/tenant slug (empty = "personal" for jobs persisted before
    /// multi-tenant scoping). The API filters list/delete by this.
    #[serde(default)]
    pub tenant: String,
}

fn default_cron_source() -> String {
    "manual".into()
}

pub struct CronScheduler {
    jobs: RwLock<Vec<CronJob>>,
}

impl CronScheduler {
    pub fn new() -> CronScheduler {
        CronScheduler {
            jobs: RwLock::new(Vec::new()),
        }
    }

    pub fn add(&self, mut job: CronJob) -> Result<CronJob, String> {
        // Validate + compute the first next_run.
        let next = next_after(&job.schedule, now_ms())
            .ok_or_else(|| format!("invalid cron expression: {}", job.schedule))?;
        job.next_run_ms = Some(next);
        self.jobs.write().push(job.clone());
        Ok(job)
    }

    pub fn remove(&self, id: &str) {
        self.jobs.write().retain(|j| j.id != id);
    }

    /// Replace the set of jobs for a `(deployment, source)` pair with `jobs`
    /// (computing each one's first run). Used to register a deployment's
    /// `vercel.json` crons on deploy without disturbing manually-created jobs.
    /// Invalid expressions are skipped. Returns the number registered.
    pub fn set_source_jobs(&self, deployment: &str, source: &str, jobs: Vec<CronJob>) -> usize {
        let mut g = self.jobs.write();
        g.retain(|j| !(j.deployment == deployment && j.source == source));
        let mut n = 0;
        for mut job in jobs {
            if let Some(next) = next_after(&job.schedule, now_ms()) {
                job.next_run_ms = Some(next);
                g.push(job);
                n += 1;
            }
        }
        n
    }

    pub fn list(&self) -> Vec<CronJob> {
        self.jobs.read().clone()
    }

    /// Wholesale-replace the job set, DEDUPED by id (first occurrence wins),
    /// preserving each job's own schedule bookkeeping verbatim. Restore-from-
    /// snapshot must use this rather than `add()` in a loop: `add()` pushes
    /// unconditionally, so restoring a snapshot that already carried a job (and
    /// every prior restart re-persisted it) duplicated it every boot — the live
    /// bug that left `vc-shoomoo-0` present 3× on a node and fired it 3× per
    /// schedule. Deduping here converges the store on the next restart.
    pub fn replace_all(&self, jobs: Vec<CronJob>) {
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<CronJob> = jobs
            .into_iter()
            .filter(|j| seen.insert(j.id.clone()))
            .collect();
        *self.jobs.write() = deduped;
    }

    /// Look up a single job by id.
    pub fn get(&self, id: &str) -> Option<CronJob> {
        self.jobs.read().iter().find(|j| j.id == id).cloned()
    }

    /// Record a manual (out-of-band) run's bookkeeping — bumps `runs` and
    /// `last_run_ms` WITHOUT touching `next_run_ms`, since an ad-hoc trigger
    /// (the dashboard's "Run" button) doesn't affect the job's own schedule.
    /// Returns the job's updated state, or `None` if no such job exists.
    pub fn record_manual_run(&self, id: &str, now: u64) -> Option<CronJob> {
        let mut jobs = self.jobs.write();
        let job = jobs.iter_mut().find(|j| j.id == id)?;
        job.last_run_ms = Some(now);
        job.runs += 1;
        Some(job.clone())
    }

    /// Return jobs due at `now`, advancing each one's schedule. The caller runs
    /// the invocations.
    pub fn tick(&self, now: u64) -> Vec<CronJob> {
        let mut due = Vec::new();
        let mut jobs = self.jobs.write();
        for job in jobs.iter_mut() {
            if !job.enabled {
                continue;
            }
            let next = match job.next_run_ms {
                Some(n) => n,
                None => {
                    job.next_run_ms = next_after(&job.schedule, now);
                    continue;
                }
            };
            if now >= next {
                job.last_run_ms = Some(now);
                job.runs += 1;
                job.next_run_ms = next_after(&job.schedule, now);
                due.push(job.clone());
            }
        }
        due
    }
}

impl Default for CronScheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a cron expression to the 6-field (seconds-first) form the `cron`
/// crate requires. Standard 5-field cron (`min hour dom month dow` — the
/// POSIX/Vercel spec `vercel.json` "crons" actually declare, e.g. `"0 0 * * *"`
/// or `"*/5 * * * *"`) has no seconds field at all; without this, EVERY
/// `vercel.json`-declared cron job failed to parse and was silently dropped
/// (`set_source_jobs` skips jobs `next_after` can't compute a first run for).
/// A 6-or-7-field expression (this platform's own native/manual-job format)
/// passes through unchanged.
fn normalize(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

/// Next fire time (epoch ms) strictly after `after_ms` for a cron expression.
pub fn next_after(expr: &str, after_ms: u64) -> Option<u64> {
    let sched = Schedule::from_str(&normalize(expr)).ok()?;
    let after = Utc.timestamp_millis_opt(after_ms as i64).single()?;
    sched
        .after(&after)
        .next()
        .map(|dt| dt.timestamp_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_next_run_and_fires() {
        let sched = CronScheduler::new();
        // Every second.
        let job = sched
            .add(CronJob {
                id: "j1".into(),
                name: "tick".into(),
                schedule: "* * * * * *".into(),
                deployment: "dpl".into(),
                path: "/api/cron".into(),
                enabled: true,
                last_run_ms: None,
                next_run_ms: None,
                runs: 0,
                source: "manual".into(),
                tenant: String::new(),
            })
            .expect("valid");
        assert!(job.next_run_ms.is_some());
        // Tick far in the future -> due.
        let due = sched.tick(now_ms() + 5_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].runs, 1);
    }

    #[test]
    fn record_manual_run_bumps_stats_without_touching_the_schedule() {
        let sched = CronScheduler::new();
        let job = sched
            .add(CronJob {
                id: "j2".into(),
                name: "manual".into(),
                schedule: "0 0 * * * *".into(), // hourly — next_run_ms far away
                deployment: "dpl".into(),
                path: "/api/cron".into(),
                enabled: true,
                last_run_ms: None,
                next_run_ms: None,
                runs: 0,
                source: "manual".into(),
                tenant: String::new(),
            })
            .expect("valid");
        let original_next = job.next_run_ms;
        let now = now_ms();
        let updated = sched.record_manual_run("j2", now).expect("job exists");
        assert_eq!(updated.runs, 1);
        assert_eq!(updated.last_run_ms, Some(now));
        assert_eq!(
            updated.next_run_ms, original_next,
            "a manual run must not disturb the real schedule"
        );
        assert!(sched.record_manual_run("no-such-id", now).is_none());
        assert_eq!(sched.get("j2").unwrap().runs, 1);
    }

    #[test]
    fn rejects_bad_expression() {
        let sched = CronScheduler::new();
        let r = sched.add(CronJob {
            id: "bad".into(),
            name: "x".into(),
            schedule: "not a cron".into(),
            deployment: "d".into(),
            path: "/".into(),
            enabled: true,
            last_run_ms: None,
            next_run_ms: None,
            runs: 0,
            source: "manual".into(),
            tenant: String::new(),
        });
        assert!(r.is_err());
    }

    #[test]
    fn standard_five_field_vercel_json_cron_expressions_now_parse() {
        // REGRESSION TEST for a real, confirmed bug: `vercel.json`'s "crons"
        // array uses standard 5-field POSIX cron (no seconds field) per
        // Vercel's own spec — `set_source_jobs` (called on every deploy) fed
        // these straight into `next_after`, which required 6/7-field
        // expressions, so EVERY vercel.json-declared cron silently failed to
        // register. `set_source_jobs`'s return count is the real, observable
        // symptom: it reported 0 registered for a perfectly valid config.
        let sched = CronScheduler::new();
        let jobs = vec![
            CronJob {
                id: "vj1".into(),
                name: "hourly".into(),
                schedule: "0 0 * * *".into(),
                deployment: "app".into(),
                path: "/api/cron".into(),
                enabled: true,
                last_run_ms: None,
                next_run_ms: None,
                runs: 0,
                source: "vercel.json".into(),
                tenant: String::new(),
            },
            CronJob {
                id: "vj2".into(),
                name: "every-5-min".into(),
                schedule: "*/5 * * * *".into(),
                deployment: "app".into(),
                path: "/api/claw".into(),
                enabled: true,
                last_run_ms: None,
                next_run_ms: None,
                runs: 0,
                source: "vercel.json".into(),
                tenant: String::new(),
            },
        ];
        let registered = sched.set_source_jobs("app", "vercel.json", jobs);
        assert_eq!(
            registered, 2,
            "both standard 5-field vercel.json crons must register"
        );
        assert_eq!(sched.list().len(), 2);
        assert!(sched.list().iter().all(|j| j.next_run_ms.is_some()));

        // The platform's own native 6-field format still works unchanged.
        assert!(next_after("0 * * * * *", now_ms()).is_some());
        assert!(next_after("0 0 0 * * *", now_ms()).is_some());
        // Genuine garbage still rejected either way.
        assert!(next_after("not a cron", now_ms()).is_none());
    }
}
