//! Optional per-account TOTP (RFC 6238) two-factor authentication.
//!
//! ONE human JWT mint exists (admin::mint_token), and /v1/token is deliberately exempt from
//! leader forwarding, so the OTP gate lives INSIDE that mint BEFORE auth::issue. Every protected
//! mutation is re-checked by auth::require_auth AND main::admin_ingress (both authenticate through
//! auth::principal) via auth::step_up_ok, so a forwarded mutation cannot bypass it.
//!
//! State is REPLICATED, never node-local-only: every row (secrets, challenges, replay steps, lock,
//! tombstones) rides store_sync::REGISTRY as a MERGE_STORES per-row merge and is captured into
//! persist::PlatformSnapshot behind serde(default). Rows are keyed by the verified Claims.sub,
//! NEVER by tenant, email, NameID or x-hive-team.
//!
//! Security invariants:
//! - Secret material is sealed with secrets::encrypt. That function FAILS OPEN (returns plaintext
//!   on RNG/seal failure), so enrollment asserts secrets::is_encrypted and refuses otherwise.
//!   Every read uses secrets::try_decrypt (the honest Option), never passthrough decrypt.
//! - Every secret/MAC comparison is constant-time: TOTP codes via admin::ct_eq; recovery-code
//!   hashes via Argon2id constant-time verify_password. == is used only for ids and digests.
//! - HIVE_SECRET_KEY is NEVER rotated here; HIVE_SECRET_KEY_OLD stays the rotation path.
//! - No secret, code or challenge id is ever placed in a URL, Location, HTML, log line or argv;
//!   they travel only in an authenticated JSON body over the request channel.
//! - Nothing a request path reserves is held across an .await: every MutexGuard is dropped at the
//!   end of a synchronous method.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::Claims;
use crate::state::CloudState;

/// RFC 6238 parameters: 30 s step, 6 digits, +/-1 step tolerance.
const STEP_SECS: u64 = 30;
const DIGITS: u32 = 1_000_000;
const WINDOW: i64 = 1;
/// A login challenge lives at most 5 minutes and admits at most 5 wrong codes.
const CHALLENGE_TTL_MS: u64 = 300_000;
const MAX_CHALLENGE_ATTEMPTS: u32 = 5;
/// Ten consecutive account-level failures lock the account fleet-wide (replicated row).
const MAX_CONSECUTIVE_FAILURES: u32 = 10;
const LOCK_MS: u64 = 15 * 60 * 1000;
const RECOVERY_CODE_COUNT: usize = 10;
/// Dedicated per-sub verification throttle (the mint limiter alone is not enough).
const VERIFY_PER_WINDOW: u32 = 30;
const VERIFY_WINDOW_MS: u64 = 60_000;
/// How fresh a token otp_at must be for a protected mutation on a 2FA account.
pub const STEP_UP_WINDOW_SECS: u64 = 300;

/// A server-side one-shot login challenge. nonce is CSPRNG material bound to the id; it is never
/// returned to a client, logged, or placed in a URL.
#[derive(Clone, Serialize, Deserialize)]
pub struct TotpChallenge {
    pub sub: String,
    pub exp_ms: u64,
    pub nonce: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub consumed: bool,
}

/// One row per ACCOUNT, keyed by the verified JWT sub. Every new field carries serde(default)
/// because PlatformSnapshot deserialize errors are silent total state loss (persist::load uses
/// unwrap_or_default) and a required key must never be removed in a later release.
#[derive(Clone, Serialize, Deserialize)]
pub struct TotpRow {
    pub sub: String,
    #[serde(default)]
    pub tenant: String,
    #[serde(default)]
    pub otp_required: bool,
    #[serde(default)]
    pub secret_enc: String,
    #[serde(default)]
    pub pending_secret_enc: String,
    #[serde(default)]
    pub recovery_code_hashes: Vec<String>,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub last_used_step: u64,
    #[serde(default)]
    pub locked_until_ms: u64,
    #[serde(default)]
    pub challenges: BTreeMap<String, TotpChallenge>,
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Permanent: a key id on this list is refused by auth::api_key_claims even if an older
    /// PlatformSnapshot resurrects the apikeys row.
    #[serde(default)]
    pub revoked_api_key_ids: Vec<String>,
    #[serde(default)]
    pub enrolled_ms: u64,
    /// A disabled row becomes a PERMANENT TOMBSTONE: absence must never erase a row, and an older
    /// replicated copy carrying otp_required=true must not resurrect a disable.
    #[serde(default)]
    pub deleted: bool,
}

impl TotpRow {
    fn blank(sub: &str, now: u64) -> TotpRow {
        TotpRow {
            sub: sub.to_string(),
            tenant: String::new(),
            otp_required: false,
            secret_enc: String::new(),
            pending_secret_enc: String::new(),
            recovery_code_hashes: Vec::new(),
            updated_ms: now,
            last_used_step: 0,
            locked_until_ms: 0,
            challenges: BTreeMap::new(),
            consecutive_failures: 0,
            revoked_api_key_ids: Vec::new(),
            enrolled_ms: 0,
            deleted: false,
        }
    }
}

/// Strictly monotonic per-row version, so a genuine write always wins a replicated merge even
/// when two nodes wall clocks disagree slightly.
fn bump(prev: u64, now: u64) -> u64 {
    now.max(prev.saturating_add(1))
}

fn canonical(r: &TotpRow) -> String {
    serde_json::to_value(r)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_default()
}

/// Per-row MERGE (never wholesale replace) with permanent tombstones, following the
/// ProjectSettings precedent. A row with a higher updated_ms wins; on a tie the canonical JSON
/// ordering decides, so every node converges on the same bytes. Tombstoned rows are force-cleared
/// of secret material and dominate any non-deleted row that is not strictly newer.
fn merge_row(rows: &mut BTreeMap<String, TotpRow>, mut incoming: TotpRow) -> bool {
    if incoming.deleted {
        incoming.otp_required = false;
        incoming.secret_enc.clear();
        incoming.pending_secret_enc.clear();
        incoming.recovery_code_hashes.clear();
        incoming.challenges.clear();
        incoming.last_used_step = 0;
        incoming.locked_until_ms = 0;
    }
    let sub = incoming.sub.clone();
    if sub.is_empty() {
        return false;
    }
    match rows.get(&sub) {
        None => {
            rows.insert(sub, incoming);
            true
        }
        Some(cur) => {
            if cur.deleted && !incoming.deleted && incoming.updated_ms <= cur.updated_ms {
                return false;
            }
            let take = incoming.updated_ms > cur.updated_ms
                || (incoming.updated_ms == cur.updated_ms && canonical(&incoming) > canonical(cur));
            if !take {
                return false;
            }
            // Revoked key ids are permanent and must survive a newer row that forgot them.
            let mut merged = incoming;
            for id in &cur.revoked_api_key_ids {
                if !merged.revoked_api_key_ids.contains(id) {
                    merged.revoked_api_key_ids.push(id.clone());
                }
            }
            merged.revoked_api_key_ids.sort();
            rows.insert(sub, merged);
            true
        }
    }
}

pub struct TotpStore {
    rows: Mutex<BTreeMap<String, TotpRow>>,
}

impl TotpStore {
    pub fn new() -> TotpStore {
        TotpStore {
            rows: Mutex::new(BTreeMap::new()),
        }
    }

    /// Dedicated per-sub limiter, charged on EVERY verification including wrong codes.
    fn limiter() -> &'static hive_edge::RateLimiter {
        static LIMITER: std::sync::OnceLock<hive_edge::RateLimiter> = std::sync::OnceLock::new();
        LIMITER.get_or_init(|| hive_edge::RateLimiter::new(VERIFY_PER_WINDOW, VERIFY_WINDOW_MS))
    }

    /// Deterministic snapshot: BTreeMap order, tombstone rows INCLUDED so absence never erases a
    /// row. Routes through serde_json::Value + store_sync::enc_sorted on the wire.
    pub fn snapshot_rows(&self) -> Vec<TotpRow> {
        self.rows.lock().values().cloned().collect()
    }

    /// Boot restore: per-row merge, never a wholesale replace.
    pub fn restore(&self, rows: Vec<TotpRow>) {
        let mut s = self.rows.lock();
        for r in rows {
            merge_row(&mut s, r);
        }
    }

    /// MERGE_STORES adoption: per-row merge, returns the number of changed rows.
    pub fn merge_synced(&self, rows: Vec<TotpRow>) -> usize {
        let mut s = self.rows.lock();
        let mut n = 0;
        for r in rows {
            if merge_row(&mut s, r) {
                n += 1;
            }
        }
        n
    }

    pub fn account(&self, sub: &str) -> Option<TotpRow> {
        self.rows.lock().get(sub).cloned()
    }

    pub fn requires_otp(&self, sub: &str) -> bool {
        self.rows
            .lock()
            .get(sub)
            .is_some_and(|r| !r.deleted && r.otp_required)
    }

    /// Tri-state lookup for the request-path step-up gate. None means the store could not be read
    /// (a contended lock here, or any future fallible backing store); the caller MUST fail CLOSED
    /// on None rather than silently treating it as "no 2FA". Some(false) is a genuine no-2FA
    /// account, Some(true) is a stored otp_required row.
    pub fn otp_required_checked(&self, sub: &str) -> Option<bool> {
        let s = self.rows.try_lock()?;
        Some(s.get(sub).is_some_and(|r| !r.deleted && r.otp_required))
    }

    /// A hive_ API key is not a 2FA-satisfied principal: refuse one that was revoked at
    /// enrollment (permanent id list), or one in this tenant minted before the enrollment.
    pub fn api_key_blocked(&self, id: &str, team: &str, created_ms: u64) -> bool {
        self.rows.lock().values().any(|r| {
            !r.deleted
                && r.otp_required
                && r.tenant == team
                && (r.revoked_api_key_ids.iter().any(|k| k == id)
                    || (created_ms > 0 && created_ms <= r.enrolled_ms))
        })
    }

    pub fn record_revoked_api_keys(&self, sub: &str, ids: Vec<String>, now: u64) {
        let mut s = self.rows.lock();
        if let Some(r) = s.get_mut(sub) {
            for id in ids {
                if !r.revoked_api_key_ids.contains(&id) {
                    r.revoked_api_key_ids.push(id);
                }
            }
            r.revoked_api_key_ids.sort();
            r.updated_ms = bump(r.updated_ms, now);
        }
    }

    /// (1) ENROLL: 20-byte CSPRNG key, sealed at rest, stored as PENDING, base32 returned ONCE
    /// over the authenticated JSON channel. otp_required stays false until CONFIRM.
    pub fn enroll(&self, sub: &str, tenant: &str, now: u64) -> Result<String, TotpError> {
        let key = random_bytes(20).ok_or(TotpError::SealFailed)?;
        let b32 = base32_encode(&key);
        let sealed = crate::secrets::encrypt(&hex::encode(&key));
        // encrypt FAILS OPEN on RNG/seal failure. Assert, never assume.
        if !crate::secrets::is_encrypted(&sealed) {
            return Err(TotpError::SealFailed);
        }
        let mut s = self.rows.lock();
        let row = s
            .entry(sub.to_string())
            .or_insert_with(|| TotpRow::blank(sub, now));
        if !row.deleted && row.otp_required {
            return Err(TotpError::AlreadyEnrolled);
        }
        row.deleted = false;
        row.tenant = tenant.to_string();
        row.pending_secret_enc = sealed;
        row.secret_enc.clear();
        row.updated_ms = bump(row.updated_ms, now);
        Ok(b32)
    }

    /// (2) CONFIRM: verify the first code against the pending secret, then COMPARE-AND-SET
    /// otp_required=true, clear the pending secret and mint 10 one-use recovery codes whose
    /// plaintext is returned exactly once. The expensive Argon2id work runs BEFORE the lock.
    pub fn confirm(
        &self,
        sub: &str,
        tenant: &str,
        code: &str,
        now: u64,
    ) -> Result<Vec<String>, TotpError> {
        let row = self.account(sub).ok_or(TotpError::NoPending)?;
        if !row.deleted && row.otp_required {
            return Err(TotpError::AlreadyEnrolled);
        }
        let pending = row.pending_secret_enc.clone();
        if pending.is_empty() {
            return Err(TotpError::NoPending);
        }
        let key = decode_secret(&pending).ok_or(TotpError::SealFailed)?;
        let step = verify_code(&key, code, now / 1000, row.last_used_step)
            .ok_or(TotpError::BadCode)?;

        let mut plaintext = Vec::with_capacity(RECOVERY_CODE_COUNT);
        let mut hashes = Vec::with_capacity(RECOVERY_CODE_COUNT);
        for _ in 0..RECOVERY_CODE_COUNT {
            let raw = random_bytes(10).ok_or(TotpError::SealFailed)?;
            let h = hex::encode(raw);
            let pretty = format!(
                "{}-{}-{}-{}-{}",
                &h[0..4],
                &h[4..8],
                &h[8..12],
                &h[12..16],
                &h[16..20]
            );
            let hash = hash_recovery(&pretty).ok_or(TotpError::SealFailed)?;
            plaintext.push(pretty);
            hashes.push(hash);
        }

        let mut s = self.rows.lock();
        // CAS: the pending secret must be exactly the one we verified.
        let cur = s.get_mut(sub).ok_or(TotpError::NoPending)?;
        if cur.otp_required || !crate::admin::ct_eq(&cur.pending_secret_enc, &pending) {
            return Err(TotpError::NoPending);
        }
        cur.deleted = false;
        cur.tenant = tenant.to_string();
        cur.otp_required = true;
        cur.secret_enc = pending;
        cur.pending_secret_enc.clear();
        cur.recovery_code_hashes = hashes;
        cur.last_used_step = step;
        cur.locked_until_ms = 0;
        cur.consecutive_failures = 0;
        cur.enrolled_ms = now;
        cur.challenges.clear();
        cur.updated_ms = bump(cur.updated_ms, now);
        Ok(plaintext)
    }

    /// (3) LOGIN: create a server-side challenge bound to sub. Fails closed while locked.
    pub fn create_challenge(&self, sub: &str, now: u64) -> Result<String, TotpError> {
        let mut s = self.rows.lock();
        let row = s.get(sub).ok_or(TotpError::NotEnrolled)?;
        if row.deleted || !row.otp_required {
            return Err(TotpError::NotEnrolled);
        }
        if row.locked_until_ms > now {
            return Err(TotpError::Locked);
        }
        let id = random_token(16).ok_or(TotpError::SealFailed)?;
        let nonce = random_token(16).ok_or(TotpError::SealFailed)?;
        let row = s.get_mut(sub).ok_or(TotpError::NotEnrolled)?;
        row.challenges.retain(|_, c| !c.consumed && c.exp_ms > now);
        if row.challenges.len() >= 8 {
            return Err(TotpError::TooMany);
        }
        row.challenges.insert(
            id.clone(),
            TotpChallenge {
                sub: sub.to_string(),
                exp_ms: now.saturating_add(CHALLENGE_TTL_MS),
                nonce,
                attempts: 0,
                consumed: false,
            },
        );
        row.updated_ms = bump(row.updated_ms, now);
        Ok(id)
    }

    /// (3) LOGIN verification. Charges the per-sub limiter on EVERY verification (correct or not),
    /// consumes the challenge by a COMPARE-AND-SET on any terminal outcome, enforces
    /// step greater than last_used_step (replicated replay defense), and tombstones a recovery
    /// code atomically on use. Returns the unix seconds to stamp into the token otp_at claim.
    pub fn verify_login(
        &self,
        sub: &str,
        challenge_id: &str,
        submitted: &str,
        now: u64,
    ) -> Result<u64, TotpError> {
        if !Self::limiter().check(sub, now) {
            return Err(TotpError::TooMany);
        }
        let row = self.account(sub).ok_or(TotpError::NotEnrolled)?;
        if row.deleted || !row.otp_required {
            return Err(TotpError::NotEnrolled);
        }
        if row.locked_until_ms > now {
            return Err(TotpError::Locked);
        }
        let ch = row
            .challenges
            .get(challenge_id)
            .cloned()
            .ok_or(TotpError::ChallengeMissing)?;
        if ch.sub != sub || ch.consumed || ch.exp_ms <= now {
            return Err(TotpError::ChallengeMissing);
        }
        let key = decode_secret(&row.secret_enc).ok_or(TotpError::SealFailed)?;
        let submitted = submitted.trim();
        let totp_step = verify_code(&key, submitted, now / 1000, row.last_used_step);
        let recovery_idx = if totp_step.is_none() && submitted.len() >= 8 {
            row.recovery_code_hashes
                .iter()
                .position(|h| recovery_matches(h, submitted))
        } else {
            None
        };
        let ok = totp_step.is_some() || recovery_idx.is_some();

        let mut s = self.rows.lock();
        let cur = s.get_mut(sub).ok_or(TotpError::NotEnrolled)?;
        // CAS on the row version: a concurrent verification that already advanced the row loses.
        if cur.updated_ms != row.updated_ms {
            return Err(TotpError::Busy);
        }
        let Some(ch) = cur.challenges.get_mut(challenge_id) else {
            return Err(TotpError::ChallengeMissing);
        };
        if ch.consumed || ch.exp_ms <= now {
            ch.consumed = true;
            cur.updated_ms = bump(cur.updated_ms, now);
            return Err(TotpError::ChallengeMissing);
        }
        if !ok {
            ch.attempts = ch.attempts.saturating_add(1);
            cur.consecutive_failures = cur.consecutive_failures.saturating_add(1);
            if ch.attempts >= MAX_CHALLENGE_ATTEMPTS {
                ch.consumed = true;
            }
            if cur.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                cur.locked_until_ms = now.saturating_add(LOCK_MS);
                ch.consumed = true;
            }
            cur.updated_ms = bump(cur.updated_ms, now);
            return Err(TotpError::BadCode);
        }
        ch.consumed = true;
        if let Some(step) = totp_step {
            cur.last_used_step = cur.last_used_step.max(step);
        }
        if let Some(i) = recovery_idx {
            if i < cur.recovery_code_hashes.len() {
                cur.recovery_code_hashes.remove(i); // single use, tombstoned atomically
            }
        }
        cur.consecutive_failures = 0;
        cur.challenges.retain(|_, c| !c.consumed && c.exp_ms > now);
        cur.updated_ms = bump(cur.updated_ms, now);
        Ok(now / 1000)
    }

    /// (6) DISABLE: fresh re-auth (the route step-up gate) + a valid OTP or recovery code through
    /// the SAME challenge/rate-limit/replay path as login, then CAS otp_required=false and write a
    /// permanent tombstone. Existing JWTs keep living until their own expiry; rotating
    /// HIVE_JWT_SECRET to kill them is explicitly forbidden.
    pub fn disable(&self, sub: &str, submitted: &str, now: u64) -> Result<(), TotpError> {
        let id = self.create_challenge(sub, now)?;
        self.verify_login(sub, &id, submitted, now)?;
        let mut s = self.rows.lock();
        let cur = s.get_mut(sub).ok_or(TotpError::NotEnrolled)?;
        let revoked = cur.revoked_api_key_ids.clone();
        let tenant = cur.tenant.clone();
        let mut next = TotpRow::blank(sub, now);
        next.tenant = tenant;
        next.revoked_api_key_ids = revoked;
        next.enrolled_ms = cur.enrolled_ms;
        next.deleted = true;
        next.updated_ms = bump(cur.updated_ms, now);
        *cur = next;
        Ok(())
    }
}

impl Default for TotpStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed refusals: fixed static strings, never carrying a secret, code or challenge id.
#[derive(Debug)]
pub enum TotpError {
    NotEnrolled,
    AlreadyEnrolled,
    NoPending,
    BadCode,
    ChallengeMissing,
    Locked,
    TooMany,
    Busy,
    SealFailed,
}

impl TotpError {
    pub fn status(&self) -> StatusCode {
        match self {
            TotpError::TooMany | TotpError::Busy => StatusCode::TOO_MANY_REQUESTS,
            TotpError::Locked => StatusCode::FORBIDDEN,
            TotpError::SealFailed => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
    pub fn message(&self) -> &'static str {
        match self {
            TotpError::NotEnrolled => "2fa is not enabled for this account",
            TotpError::AlreadyEnrolled => "2fa is already enabled; disable it first",
            TotpError::NoPending => "no pending 2fa enrollment",
            TotpError::BadCode => "invalid or reused authentication code",
            TotpError::ChallengeMissing => "invalid or expired 2fa challenge",
            TotpError::Locked => "account temporarily locked",
            TotpError::TooMany => "too many authentication attempts",
            TotpError::Busy => "concurrent authentication attempt; retry",
            TotpError::SealFailed => "2fa secret sealing unavailable",
        }
    }
}

fn random_bytes(n: usize) -> Option<Vec<u8>> {
    use ring::rand::SecureRandom;
    let mut b = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut b).ok()?;
    Some(b)
}

fn random_token(n: usize) -> Option<String> {
    Some(hex::encode(random_bytes(n)?))
}

/// RFC 4648 base32 (no padding) for the otpauth secret shown once at enrollment.
pub fn base32_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// RFC 6238 / RFC 4226 HOTP: 8-byte big-endian counter, HMAC-SHA1, dynamic truncation, 6 digits.
/// ring is already a direct dep; HMAC_SHA1_FOR_LEGACY_USE_ONLY is the exact TOTP algorithm.
pub fn totp_code(key: &[u8], step: u64) -> String {
    let tag = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key),
        &step.to_be_bytes(),
    );
    let d = tag.as_ref();
    let offset = (d[d.len() - 1] & 0x0f) as usize;
    let bin = (((d[offset] & 0x7f) as u32) << 24)
        | ((d[offset + 1] as u32) << 16)
        | ((d[offset + 2] as u32) << 8)
        | (d[offset + 3] as u32);
    format!("{:06}", bin % DIGITS)
}

/// Verifies a 6-digit code within +/-1 step and returns the ACCEPTED step, so the caller can
/// enforce step greater than last_used_step. Comparisons are constant-time (admin::ct_eq).
pub fn verify_code(key: &[u8], code: &str, now_secs: u64, last_used_step: u64) -> Option<u64> {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let base = (now_secs / STEP_SECS) as i64;
    let mut matched = false;
    let mut used = 0u64;
    for delta in -WINDOW..=WINDOW {
        let step = base + delta;
        if step < 0 {
            continue;
        }
        let step = step as u64;
        if crate::admin::ct_eq(&totp_code(key, step), code) {
            matched = true;
            used = used.max(step);
        }
    }
    (matched && used > last_used_step).then_some(used)
}

/// A stored secret is the AEAD seal of the hex-encoded 20-byte key. Read with try_decrypt
/// (never the passthrough decrypt): an unopenable ciphertext must fail CLOSED.
fn decode_secret(sealed: &str) -> Option<Vec<u8>> {
    let plain = crate::secrets::try_decrypt(sealed)?;
    hex::decode(plain.trim()).ok()
}

/// Argon2id PHC hash with its own random salt (the enterprise::password_hash precedent).
fn hash_recovery(code: &str) -> Option<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(code.as_bytes(), &salt)
        .ok()
        .map(|h| h.to_string())
}

/// Argon2 verify_password is the CONSTANT-TIME comparison for a recovery code.
fn recovery_matches(stored: &str, submitted: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;
    if !stored.starts_with("$argon2") {
        return false;
    }
    PasswordHash::new(stored)
        .ok()
        .is_some_and(|p| Argon2::default().verify_password(submitted.as_bytes(), &p).is_ok())
}

/// Operator diagnostic (hive-cloud --totp-selftest): print the RFC 6238 Appendix B vectors
/// computed by THIS module totp_code, so a host can cross-check the primitive against a
/// python3 -I oracle without booting a node. Prints only the fixed public test key, never a
/// real account secret.
pub fn selftest_vectors() {
    let key = b"12345678901234567890";
    println!("base32={}", base32_encode(key));
    crate::auth::selftest_claim_roundtrip();
    for t in [
        59u64,
        1_111_111_109,
        1_111_111_111,
        1_234_567_890,
        2_000_000_000,
        20_000_000_000,
    ] {
        println!("t={} code={}", t, totp_code(key, t / STEP_SECS));
    }
}

/// The caller of a self-service 2FA mutation: a verified, explicitly-asserted HUMAN account.
/// Tenancy and identity come from the cryptographically-bound JWT claim, never x-hive-team.
fn caller(claims: Option<Extension<Claims>>) -> Result<Claims, Response> {
    let Some(Extension(c)) = claims else {
        return Err((StatusCode::UNAUTHORIZED, "authentication required").into_response());
    };
    if c.sub.trim().is_empty()
        || c.sub == "__untagged__"
        || c.sub.starts_with("key:")
        || crate::auth::is_fleet_service(&c)
        || crate::auth::is_queue_client(&c)
    {
        return Err((StatusCode::FORBIDDEN, "a human account is required").into_response());
    }
    Ok(c)
}

#[derive(Deserialize)]
pub struct CodeReq {
    #[serde(default)]
    pub code: String,
}


/// GET /v1/account/2fa/status - reports only the caller's own enrollment flag. A read never
/// reveals the secret, recovery codes or challenge ids, and is scoped to the verified sub.
pub async fn status_handler(
    State(c): State<Arc<CloudState>>,
    claims: Option<Extension<Claims>>,
) -> Response {
    let cl = match caller(claims) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let enrolled = c.totp.account(&cl.sub).is_some_and(|r| !r.deleted && r.otp_required);
    Json(json!({ "otp_required": enrolled, "enrolled": enrolled })).into_response()
}
/// POST /v1/account/2fa/enroll - returns the base32 secret ONCE over the authenticated JSON
/// channel. No otpauth URL is emitted: a caller-controlled string or secret never enters a URL.
pub async fn enroll_handler(
    State(c): State<Arc<CloudState>>,
    claims: Option<Extension<Claims>>,
) -> Response {
    let cl = match caller(claims) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if !crate::admin::has_explicit_caller_identity(true, None) {
        return (StatusCode::FORBIDDEN, "no asserted identity").into_response();
    }
    let now = hive_core::now_ms();
    match c.totp.enroll(&cl.sub, &cl.tenant, now) {
        Ok(secret) => {
            crate::persist::persist(&c);
            Json(json!({
                "secret": secret,
                "digits": 6,
                "period": STEP_SECS,
                "algorithm": "SHA1",
                "otp_required": false,
            }))
            .into_response()
        }
        Err(e) => (e.status(), e.message()).into_response(),
    }
}

/// POST /v1/account/2fa/confirm - CAS otp_required=true, revoke the account hive_ API keys, and
/// return 10 one-use recovery codes ONCE.
pub async fn confirm_handler(
    State(c): State<Arc<CloudState>>,
    claims: Option<Extension<Claims>>,
    Json(req): Json<CodeReq>,
) -> Response {
    let cl = match caller(claims) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if !crate::admin::has_explicit_caller_identity(true, None) {
        return (StatusCode::FORBIDDEN, "no asserted identity").into_response();
    }
    let now = hive_core::now_ms();
    match c.totp.confirm(&cl.sub, &cl.tenant, &req.code, now) {
        Ok(codes) => {
            let keys = c.apikeys.list(&cl.tenant);
            let ids: Vec<String> = keys.iter().map(|k| k.id.clone()).collect();
            for k in &keys {
                c.apikeys.revoke(&k.id, &cl.tenant);
            }
            c.totp.record_revoked_api_keys(&cl.sub, ids, now);
            crate::persist::persist(&c);
            Json(json!({ "otp_required": true, "recovery_codes": codes })).into_response()
        }
        Err(e) => (e.status(), e.message()).into_response(),
    }
}

/// POST /v1/account/2fa/disable - the route step-up gate already demands a fresh otp_at;
/// re-assert it here too so a deployment without admin_ingress cannot weaken the gate.
pub async fn disable_handler(
    State(c): State<Arc<CloudState>>,
    claims: Option<Extension<Claims>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CodeReq>,
) -> Response {
    let cl = match caller(claims) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if !crate::admin::has_explicit_caller_identity(true, None) {
        return (StatusCode::FORBIDDEN, "no asserted identity").into_response();
    }
    if !crate::auth::fresh_step_up(&cl, &headers) {
        return (StatusCode::FORBIDDEN, "2fa_step_up_required").into_response();
    }
    let now = hive_core::now_ms();
    match c.totp.disable(&cl.sub, &req.code, now) {
        Ok(()) => {
            crate::persist::persist(&c);
            Json(json!({ "otp_required": false })).into_response()
        }
        Err(e) => (e.status(), e.message()).into_response(),
    }
}
