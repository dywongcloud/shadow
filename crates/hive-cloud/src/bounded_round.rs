//! A periodic fan-out round that never lasts as long as its slowest target.
//!
//! `join_all` over every target makes a round as long as its SLOWEST target: one powered-off seed's
//! dial budget stretched the gossip round, and one failing peer's two fallback-ceiling probe
//! samples stretched every health-probe round. A [`BoundedRound`] runs each target in its own task
//! and waits for THIS round's targets only until a deadline. A target still running then is a
//! straggler: it keeps running under its in-flight flag (never dispatched twice at once) and its
//! result lands in whichever later round receives it ([`BoundedRound::begin`] drains those first).
//! Each task reports through a [`Completion`] drop guard, so a task that panics or is dropped still
//! clears its flag. The gossip loop and the health prober both run on it.
//!
//! The deadline alone still let ONE target set the pace of every round: the round waited the full
//! deadline whenever any target was slower than it, even after that target had missed it on every
//! round for hours. [`BoundedRound::collect_leashed`] adds a per-target leash: the round stops
//! waiting for one target after its own, much shorter budget. Parking is NOT cancellation (the task
//! keeps running and its result still lands), so a leash can never cost a peer its dial budget or
//! its discovery fallback.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::{Duration, Instant};

/// One target's result: (round it was dispatched in, target key, value).
pub type Landed<T> = (u64, String, T);

/// Sends a target's result to its round exactly once: on [`Completion::finish`], or with
/// `T::default()` when the task is dropped or panics first. Without it a panicking task would leave
/// its target's in-flight flag set forever and the target would never be dispatched again.
pub struct Completion<T: Default + Send + 'static> {
    tx: UnboundedSender<Landed<T>>,
    round: u64,
    target: String,
    sent: bool,
}

impl<T: Default + Send + 'static> Completion<T> {
    fn new(tx: UnboundedSender<Landed<T>>, round: u64, target: String) -> Self {
        Self {
            tx,
            round,
            target,
            sent: false,
        }
    }

    pub fn finish(mut self, value: T) {
        self.sent = true;
        let _ = self
            .tx
            .send((self.round, std::mem::take(&mut self.target), value));
    }
}

impl<T: Default + Send + 'static> Drop for Completion<T> {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self
                .tx
                .send((self.round, std::mem::take(&mut self.target), T::default()));
        }
    }
}

/// Loop-owned fan-out state (single writer: the loop that owns it).
pub struct BoundedRound<T: Default + Send + 'static> {
    tx: UnboundedSender<Landed<T>>,
    rx: UnboundedReceiver<Landed<T>>,
    in_flight: HashSet<String>,
    /// Targets dispatched in the current round that have not reported yet, mapped to the instant
    /// they were dispatched (a per-target leash is measured from there, not from the start of the
    /// round).
    dispatched: HashMap<String, Instant>,
    round: u64,
}

impl<T: Default + Send + 'static> Default for BoundedRound<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Default + Send + 'static> BoundedRound<T> {
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            tx,
            rx,
            in_flight: HashSet::new(),
            dispatched: HashMap::new(),
            round: 0,
        }
    }

    /// Starts the next round. Returns the stragglers of earlier rounds that landed since the last
    /// [`collect`](Self::collect), their flags already cleared so they are dispatched again this
    /// round. Every returned item carries a round number BELOW the new one, i.e. it reported late.
    pub fn begin(&mut self) -> Vec<Landed<T>> {
        self.round += 1;
        self.dispatched.clear();
        let mut out = Vec::new();
        while let Ok(landed) = self.rx.try_recv() {
            self.in_flight.remove(&landed.1);
            out.push(landed);
        }
        out
    }

    /// The number of the round [`begin`](Self::begin) last started: a result
    /// carrying it reported inside its own round.
    pub fn round(&self) -> u64 {
        self.round
    }

    /// Is `key` still running (dispatched in this or an earlier round)?
    pub fn in_flight(&self, key: &str) -> bool {
        self.in_flight.contains(key)
    }

    pub fn in_flight_keys(&self) -> &HashSet<String> {
        &self.in_flight
    }

    /// Run `work` for `key` in its own task this round. Returns false, and
    /// spawns nothing, while `key` is still in flight.
    pub fn spawn<F>(&mut self, key: &str, work: F) -> bool
    where
        F: Future<Output = T> + Send + 'static,
    {
        if !self.in_flight.insert(key.to_string()) {
            return false;
        }
        self.dispatched
            .insert(key.to_string(), tokio::time::Instant::now());
        let done = Completion::new(self.tx.clone(), self.round, key.to_string());
        tokio::spawn(async move { done.finish(work.await) });
        true
    }

    /// Waits until every target dispatched THIS round has reported, or until `deadline`, with no
    /// per-target leash (every target may hold the round to the full deadline). See
    /// [`collect_leashed`](Self::collect_leashed).
    pub async fn collect(&mut self, deadline: Instant) -> (Vec<Landed<T>>, usize) {
        self.collect_leashed(deadline, |_| None).await
    }

    /// [`collect`](Self::collect), with a per-target leash.
    ///
    /// `leash_ms(key)` returns how long THIS round waits for `key`, measured from the instant it
    /// was dispatched; `None` means the round deadline is the only bound. A target that has not
    /// reported when its leash expires is PARKED, not cancelled: the round stops waiting for it,
    /// but its task keeps running under its in-flight flag and its result lands in a later round
    /// exactly as a plain straggler's does. Parking therefore cannot cost a target its dial budget
    /// (never cut short mid-discovery) and cannot withdraw it from anything: only `health::demote`
    /// and the registry's staleness drop remove a node.
    ///
    /// Each pass waits for the soonest leash that has not fired yet, else for the round deadline;
    /// once every remaining target is parked there is nothing left to wait for, and when the round
    /// deadline passes whatever is left is a straggler.
    ///
    /// Returns everything that landed meanwhile (stragglers of earlier rounds included) and how
    /// many of this round's targets are still in flight, parked ones included, so the count keeps
    /// meaning "did not answer in time" and does not fall just because the round stopped waiting.
    pub async fn collect_leashed(
        &mut self,
        deadline: Instant,
        leash_ms: impl Fn(&str) -> Option<u64>,
    ) -> (Vec<Landed<T>>, usize) {
        let mut out = Vec::new();
        let mut parked: HashSet<String> = HashSet::new();
        while !self.dispatched.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let mut next_leash: Option<Instant> = None;
            let mut unleashed = false;
            for (k, started) in &self.dispatched {
                if parked.contains(k) {
                    continue;
                }
                match leash_ms(k) {
                    Some(ms) => {
                        let exp = (*started + Duration::from_millis(ms)).min(deadline);
                        if exp <= now {
                            parked.insert(k.clone());
                        } else {
                            next_leash = Some(next_leash.map_or(exp, |n: Instant| n.min(exp)));
                        }
                    }
                    None => unleashed = true,
                }
            }
            if next_leash.is_none() && !unleashed {
                break;
            }
            let wait = next_leash.unwrap_or(deadline);
            match tokio::time::timeout_at(wait, self.rx.recv()).await {
                Ok(Some(landed)) => {
                    self.in_flight.remove(&landed.1);
                    if landed.0 == self.round {
                        self.dispatched.remove(&landed.1);
                    }
                    out.push(landed);
                }
                Ok(None) | Err(_) => {
                    if wait >= deadline {
                        break;
                    }
                }
            }
        }
        (out, self.dispatched.len())
    }

    /// Targets dispatched this round that had not reported when the last
    /// [`collect_leashed`](Self::collect_leashed) returned: the round's stragglers, parked ones
    /// included.
    pub fn outstanding(&self) -> Vec<String> {
        self.dispatched.keys().cloned().collect()
    }
}
