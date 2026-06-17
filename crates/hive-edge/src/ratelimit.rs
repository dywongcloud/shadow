//! L7 DDoS mitigation — a per-client-IP sliding-window rate limiter that runs at
//! the edge *before* the compute layer, so floods are shed cheaply (429) without
//! ever leasing a function instance. Complements the per-region burst limiter
//! (which bounds total concurrency) by bounding any single source.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::RwLock;
use serde::Serialize;

pub struct RateLimiter {
    enabled: AtomicBool,
    /// Max requests per window per IP.
    limit: AtomicU32,
    window_ms: AtomicU64,
    blocked_total: AtomicU64,
    hits: RwLock<HashMap<String, VecDeque<u64>>>,
}

#[derive(Serialize)]
pub struct RateLimitStats {
    pub enabled: bool,
    pub limit: u32,
    pub window_ms: u64,
    pub tracked_ips: usize,
    pub blocked_total: u64,
}

impl RateLimiter {
    pub fn new(limit: u32, window_ms: u64) -> RateLimiter {
        RateLimiter {
            enabled: AtomicBool::new(true),
            limit: AtomicU32::new(limit),
            window_ms: AtomicU64::new(window_ms),
            blocked_total: AtomicU64::new(0),
            hits: RwLock::new(HashMap::new()),
        }
    }

    /// Returns true if the request is allowed, false if it should be 429'd.
    pub fn check(&self, ip: &str, now_ms: u64) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return true;
        }
        let window = self.window_ms.load(Ordering::Relaxed);
        let limit = self.limit.load(Ordering::Relaxed);
        let cutoff = now_ms.saturating_sub(window);
        let mut map = self.hits.write();
        // Keep the table bounded under a real flood (many distinct IPs).
        if map.len() > 50_000 {
            map.retain(|_, q| q.back().map(|&t| t > cutoff).unwrap_or(false));
        }
        let q = map.entry(ip.to_string()).or_default();
        while q.front().map(|&t| t < cutoff).unwrap_or(false) {
            q.pop_front();
        }
        if q.len() as u32 >= limit {
            self.blocked_total.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        q.push_back(now_ms);
        true
    }

    pub fn set(&self, enabled: bool, limit: u32, window_ms: u64) {
        self.enabled.store(enabled, Ordering::Relaxed);
        if limit > 0 {
            self.limit.store(limit, Ordering::Relaxed);
        }
        if window_ms > 0 {
            self.window_ms.store(window_ms, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> RateLimitStats {
        RateLimitStats {
            enabled: self.enabled.load(Ordering::Relaxed),
            limit: self.limit.load(Ordering::Relaxed),
            window_ms: self.window_ms.load(Ordering::Relaxed),
            tracked_ips: self.hits.read().len(),
            blocked_total: self.blocked_total.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_over_limit_within_window() {
        let rl = RateLimiter::new(3, 1000);
        let t = 10_000;
        assert!(rl.check("1.2.3.4", t));
        assert!(rl.check("1.2.3.4", t));
        assert!(rl.check("1.2.3.4", t));
        assert!(!rl.check("1.2.3.4", t), "4th within window is blocked");
        // a different IP is unaffected
        assert!(rl.check("5.6.7.8", t));
        // after the window slides, allowed again
        assert!(rl.check("1.2.3.4", t + 1001));
        assert_eq!(rl.stats().blocked_total, 1);
    }

    #[test]
    fn disabled_allows_all() {
        let rl = RateLimiter::new(1, 1000);
        rl.set(false, 1, 1000);
        for _ in 0..10 {
            assert!(rl.check("9.9.9.9", 0));
        }
    }
}
