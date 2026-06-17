//! Concurrency scaling limits, per Vercel's model
//! (<https://vercel.com/docs/functions/concurrency-scaling>):
//!
//! * **Burst concurrency limit** — a sliding window cap of
//!   `burst_per_window` concurrent executions per `window` (default 1000 / 10s),
//!   **per region**. Exceeding it yields `503 FUNCTION_THROTTLED`.
//! * **Max concurrency** — an absolute ceiling by plan (30k Hobby/Pro,
//!   100k Enterprise).
//!
//! This is a region-wide admission gate that runs at the edge before a request
//! is routed to a function instance.

use hive_core::now_ms;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    Hobby,
    Pro,
    Enterprise,
}

impl Plan {
    /// Absolute max concurrent executions for the plan.
    pub fn max_concurrency(self) -> u64 {
        match self {
            Plan::Hobby | Plan::Pro => 30_000,
            Plan::Enterprise => 100_000,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ConcurrencyStats {
    pub plan: Plan,
    pub region: String,
    /// Executions started in the current burst window.
    pub window_count: usize,
    pub burst_limit: usize,
    pub window_ms: u64,
    pub max_concurrency: u64,
    pub throttled_total: u64,
}

pub struct ConcurrencyLimiter {
    region: String,
    plan: Plan,
    burst_limit: usize,
    window_ms: u64,
    /// Timestamps (ms) of admissions within the window.
    admissions: Mutex<VecDeque<u64>>,
    throttled_total: Mutex<u64>,
}

impl ConcurrencyLimiter {
    pub fn new(region: impl Into<String>, plan: Plan) -> ConcurrencyLimiter {
        ConcurrencyLimiter {
            region: region.into(),
            plan,
            // Vercel: 1000 concurrent executions per 10 seconds, per region.
            burst_limit: 1000,
            window_ms: 10_000,
            admissions: Mutex::new(VecDeque::new()),
            throttled_total: Mutex::new(0),
        }
    }

    /// Override the burst limit (for tests / tuning).
    pub fn with_burst(mut self, limit: usize, window_ms: u64) -> Self {
        self.burst_limit = limit;
        self.window_ms = window_ms;
        self
    }

    /// Try to admit one execution. Returns true if allowed, false => the caller
    /// should respond `503 FUNCTION_THROTTLED`.
    pub fn try_admit(&self) -> bool {
        let now = now_ms();
        let mut q = self.admissions.lock();
        // Drop timestamps outside the window.
        while let Some(&front) = q.front() {
            if now.saturating_sub(front) >= self.window_ms {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= self.burst_limit {
            drop(q);
            *self.throttled_total.lock() += 1;
            return false;
        }
        q.push_back(now);
        true
    }

    pub fn stats(&self) -> ConcurrencyStats {
        let now = now_ms();
        let mut q = self.admissions.lock();
        while let Some(&front) = q.front() {
            if now.saturating_sub(front) >= self.window_ms {
                q.pop_front();
            } else {
                break;
            }
        }
        ConcurrencyStats {
            plan: self.plan,
            region: self.region.clone(),
            window_count: q.len(),
            burst_limit: self.burst_limit,
            window_ms: self.window_ms,
            max_concurrency: self.plan.max_concurrency(),
            throttled_total: *self.throttled_total.lock(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttles_beyond_burst_limit() {
        let lim = ConcurrencyLimiter::new("sfo1", Plan::Pro).with_burst(3, 10_000);
        assert!(lim.try_admit());
        assert!(lim.try_admit());
        assert!(lim.try_admit());
        // 4th in the same window is throttled.
        assert!(!lim.try_admit());
        let s = lim.stats();
        assert_eq!(s.window_count, 3);
        assert_eq!(s.throttled_total, 1);
        assert_eq!(s.max_concurrency, 30_000);
    }

    #[test]
    fn window_slides() {
        let lim = ConcurrencyLimiter::new("sfo1", Plan::Enterprise).with_burst(1, 0);
        assert!(lim.try_admit());
        // window_ms = 0 means every prior admission is immediately stale.
        assert!(lim.try_admit());
        assert_eq!(lim.stats().max_concurrency, 100_000);
    }
}
