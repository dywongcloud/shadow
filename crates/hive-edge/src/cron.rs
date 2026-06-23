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
}

fn default_cron_source() -> String {
    "manual".into()
}

pub struct CronScheduler {
    jobs: RwLock<Vec<CronJob>>,
}

impl CronScheduler {
    pub fn new() -> CronScheduler {
        CronScheduler { jobs: RwLock::new(Vec::new()) }
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

/// Next fire time (epoch ms) strictly after `after_ms` for a cron expression.
pub fn next_after(expr: &str, after_ms: u64) -> Option<u64> {
    let sched = Schedule::from_str(expr).ok()?;
    let after = Utc.timestamp_millis_opt(after_ms as i64).single()?;
    sched.after(&after).next().map(|dt| dt.timestamp_millis() as u64)
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
            })
            .expect("valid");
        assert!(job.next_run_ms.is_some());
        // Tick far in the future -> due.
        let due = sched.tick(now_ms() + 5_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].runs, 1);
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
        });
        assert!(r.is_err());
    }
}
