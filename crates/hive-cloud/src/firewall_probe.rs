//! Operator diagnostic (same family as `--node-probe` / `--vue-probe`):
//! execute the REAL bounded-deadline, expiry and replicated-row primitives
//! against an in-memory scratch store. Binds no port, joins no mesh, starts no
//! node and writes no persisted state. A legacy INDEFINITE row fails closed.
//! Non-zero exit on any refusal, so it composes into a shell check.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::firewall::{bounded_deadline, compose_mode, requested_duration_secs};
use crate::project_settings::{
    AttackMode, ProjectSettings, ProjectStore, SyncedProjects,
    ATTACK_MODE_DEFAULT_DURATION_SECS, ATTACK_MODE_MAX_DURATION_MS,
    ATTACK_MODE_MAX_DURATION_SECS, ATTACK_MODE_MIN_DURATION_SECS,
};

const NOW: u64 = 1_795_674_000_000;
const SIX_HOURS_MS: u64 = 21_600_000;

fn check(cond: bool, what: &str) -> Result<()> {
    if !cond {
        bail!("{what}");
    }
    Ok(())
}

// The primitives return a fixed refusal String (the admin 400 body); the probe
// lifts that into anyhow without changing what the primitive decided.
fn req(hours: Option<u64>, secs: Option<u64>) -> Result<u64> {
    requested_duration_secs(hours, secs).map_err(anyhow::Error::msg)
}

fn deadline_at(now: u64, secs: u64) -> Result<u64> {
    bounded_deadline(now, secs).map_err(anyhow::Error::msg)
}

fn compose(enabled: bool, secs: u64, bots: bool, now: u64) -> Result<AttackMode> {
    compose_mode(enabled, secs, bots, now).map_err(anyhow::Error::msg)
}

fn legacy_indefinite() -> AttackMode {
    AttackMode {
        enabled: true,
        active_until_ms: 0,
        updated_ms: NOW - 1_000,
        duration_secs: 0,
        verified_bot_bypass: false,
    }
}

fn expired_six_hours() -> AttackMode {
    AttackMode {
        enabled: true,
        active_until_ms: NOW - 1,
        updated_ms: NOW - SIX_HOURS_MS - 1,
        duration_secs: 21_600,
        verified_bot_bypass: false,
    }
}

fn row(team: &str, mode: AttackMode, updated_ms: u64) -> ProjectSettings {
    let mut r = ProjectSettings::default();
    r.team = team.to_string();
    r.attack_mode = mode;
    r.updated_ms = updated_ms;
    r
}

pub fn run() -> Result<()> {
    println!("RESULT clock now_ms={NOW} six_hours_ms={SIX_HOURS_MS}");

    // Request shapes: hours (legacy dashboard), duration_secs (canonical),
    // neither (bounded default), and every refused shape.
    check(
        req(Some(6), None)? == 21_600,
        "hours=6 did not resolve to 21600 seconds",
    )?;
    check(
        req(Some(1), None)? == 3_600,
        "hours=1 did not resolve to 3600 seconds",
    )?;
    check(
        req(Some(24), None)? == 86_400,
        "hours=24 did not resolve to 86400 seconds",
    )?;
    check(
        req(None, None)? == ATTACK_MODE_DEFAULT_DURATION_SECS,
        "a request with no duration did not use the bounded 6-hour default",
    )?;
    check(
        req(Some(1), Some(21_600))? == 21_600,
        "duration_secs did not take precedence over hours",
    )?;
    let refused_requests: &[(Option<u64>, Option<u64>)] = &[
        (Some(0), None),
        (Some(25), None),
        (Some(u64::MAX), None),
        (None, Some(0)),
        (None, Some(ATTACK_MODE_MIN_DURATION_SECS - 1)),
        (None, Some(ATTACK_MODE_MAX_DURATION_SECS + 1)),
        (None, Some(u64::MAX)),
    ];
    let mut refused = 0;
    for (hours, secs) in refused_requests {
        if requested_duration_secs(*hours, *secs).is_err() {
            refused += 1;
        } else {
            bail!("an out-of-range duration was admitted: hours={hours:?} secs={secs:?}");
        }
    }
    println!("RESULT requested_duration accepted=5 refused={refused} default_secs={ATTACK_MODE_DEFAULT_DURATION_SECS}");

    // Deadlines are server-derived and bounded. A clock of zero fails closed.
    let deadline = deadline_at(NOW, 21_600)?;
    check(
        deadline == NOW + SIX_HOURS_MS,
        "hours=6 did not produce a now+21600000 deadline",
    )?;
    let mut deadline_refusals = 0;
    for (now, secs) in [
        (0_u64, 3_600_u64),
        (NOW, 0),
        (NOW, ATTACK_MODE_MIN_DURATION_SECS - 1),
        (NOW, ATTACK_MODE_MAX_DURATION_SECS + 1),
        (u64::MAX, 3_600),
        (u64::MAX - 1, u64::MAX),
    ] {
        if bounded_deadline(now, secs).is_err() {
            deadline_refusals += 1;
        } else {
            bail!("an unbounded or non-future deadline was admitted: now={now} secs={secs}");
        }
    }
    println!("RESULT bounded_deadline now={NOW} duration_secs=21600 active_until_ms={deadline} refused={deadline_refusals}");

    // Composition: the same row the replicated ProjectSettings store holds.
    let active = compose(true, 21_600, false, NOW)?;
    check(active.enabled, "an enabled mode was composed as disabled")?;
    check(
        active.active_until_ms == deadline,
        "compose_mode and bounded_deadline disagree",
    )?;
    check(
        active.duration_secs == 21_600,
        "the measured duration was not persisted on the row",
    )?;
    check(
        active.updated_ms == NOW,
        "the cookie/merge epoch was not stamped",
    )?;
    check(active.is_active(NOW), "a fresh 6-hour mode did not read active")?;
    check(
        !active.is_active(deadline),
        "a mode read active at its exact deadline",
    )?;
    check(
        !active.is_active(deadline + 1),
        "a mode read active one millisecond past its deadline",
    )?;
    check(
        !active.is_active(0),
        "a zero clock admitted an active challenge",
    )?;
    println!("RESULT compose_mode active_at_now=true active_at_deadline=false active_one_ms_after=false expires_at={deadline}");

    let off = compose(false, 0, false, NOW)?;
    check(!off.is_active(NOW), "a disabled mode read active")?;
    check(
        off.active_until_ms == 0 && off.duration_secs == 0,
        "a turn-off left a stale deadline or duration on the row",
    )?;

    // Fail-closed migration cases. Every one is a row the fleet could actually
    // hold: the INDEFINITE build's row, an expired 6-hour row, a duration
    // mismatch, a deadline beyond the 24-hour bound, no row, and a zero clock.
    let mismatched = AttackMode {
        enabled: true,
        active_until_ms: NOW + 1_000,
        updated_ms: NOW,
        duration_secs: 0,
        verified_bot_bypass: false,
    };
    let unbounded = AttackMode {
        enabled: true,
        active_until_ms: NOW + ATTACK_MODE_MAX_DURATION_MS + 1,
        updated_ms: NOW,
        duration_secs: 21_600,
        verified_bot_bypass: false,
    };
    check(
        !legacy_indefinite().is_active(NOW),
        "a legacy INDEFINITE row resurrected an unbounded challenge",
    )?;
    check(
        !expired_six_hours().is_active(NOW),
        "an expired 6-hour row was still active",
    )?;
    check(!mismatched.is_active(NOW), "a zero duration row failed open")?;
    check(
        !unbounded.is_active(NOW),
        "a deadline beyond 24 hours failed open",
    )?;
    check(
        !AttackMode::default().is_active(NOW),
        "a never-configured row failed open",
    )?;
    println!("RESULT expiry legacy_indefinite=off expired_six_hours=off mismatched=off unbounded=off absent=off zero_clock=off");

    // ALL FIVE persisted keys survive a serde round trip. A legacy row missing
    // the duration keys deserializes with defaults and reads OFF, so no stored
    // row is lost and no stored row resurrects an unbounded mode.
    let value = serde_json::to_value(active)
        .map_err(|e| anyhow::anyhow!("AttackMode did not serialize: {e}"))?;
    for key in [
        "enabled",
        "active_until_ms",
        "updated_ms",
        "duration_secs",
        "verified_bot_bypass",
    ] {
        check(
            value.get(key).is_some(),
            &format!("persisted key {key} was removed"),
        )?;
    }
    let round_trip: AttackMode = serde_json::from_value(value)
        .map_err(|e| anyhow::anyhow!("AttackMode did not deserialize: {e}"))?;
    check(
        round_trip == active,
        "the serde round trip changed the replicated row",
    )?;
    let legacy_json: AttackMode =
        serde_json::from_str(r#"{"enabled":true,"updated_ms":1795673999000}"#)
            .map_err(|e| anyhow::anyhow!("a key-less legacy row did not deserialize: {e}"))?;
    check(
        !legacy_json.is_active(NOW),
        "a legacy key-less row resurrected an unbounded challenge",
    )?;
    println!("RESULT serde persisted_keys=5 round_trip=equal legacy_keyless=off");

    // The REAL replicated store, through merge_synced and the SAME stored-tag
    // tenant scope the admin list uses. No timer, no second loop, no host I/O.
    // An expired row, a legacy indefinite row and a foreign tenant never appear.
    let store = ProjectStore::new();
    let mut rows = BTreeMap::new();
    rows.insert("geniee".to_string(), row("personal", active, NOW));
    rows.insert(
        "legacy".to_string(),
        row("personal", legacy_indefinite(), NOW),
    );
    rows.insert(
        "expired".to_string(),
        row("personal", expired_six_hours(), NOW),
    );
    rows.insert("foreign".to_string(), row("other-team", active, NOW));
    store.merge_synced(SyncedProjects {
        rows,
        ..SyncedProjects::default()
    });

    let live = store.attack_mode_projects("personal", NOW);
    check(
        live.len() == 1 && live[0].0 == "geniee" && live[0].1 == active,
        "the tenant-scoped active-project list did not report exactly geniee",
    )?;
    check(
        store.attack_mode_projects("other-team", NOW).len() == 1,
        "a stored-tag tenant boundary was crossed",
    )?;
    check(
        store.attack_mode_projects("personal", deadline).is_empty(),
        "an expired row appeared in the active-project list at its deadline",
    )?;
    check(
        store.attack_mode_projects("personal", 0).is_empty(),
        "a zero clock admitted an active-project row",
    )?;
    check(
        store.attack_mode("legacy") == legacy_indefinite(),
        "a legacy indefinite row was rewritten instead of read as OFF",
    )?;
    println!("RESULT store active_rows=1 project=geniee expired_rows=0 legacy_rows=0 foreign_rows=0 zero_clock_rows=0");
    println!("FIREWALL_PROBE=PASS");
    Ok(())
}
