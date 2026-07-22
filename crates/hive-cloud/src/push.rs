//! Push delivery — Web Push (VAPID + RFC 8291 aes128gcm) and SMS (Textbelt) for
//! the notification inbox, scoped per tenant.
//!
//! Subscriptions and SMS targets are stored under the caller's VERIFIED tenant
//! (JWT claim, never a raw header — see `admin::tenant`'s priority order), and
//! the dispatcher fans a tenant's notifications out ONLY to rows stored under
//! that exact tenant, so cross-tenant delivery is structurally impossible
//! rather than merely filtered.
//!
//! The store rides the leader→follower `store_sync` REGISTRY (mutations are
//! leader-gated in `admin.rs`, same as incidents), and the delivery loop runs
//! ONLY on the control-plane leader so the fleet never double-sends. Web Push
//! is implemented natively on `ring` (ES256 VAPID JWT per RFC 8292; ECDH +
//! HKDF + AES-128-GCM content encryption per RFC 8291) — zero new
//! dependencies, delivered over the existing shared `reqwest` client.

use crate::state::CloudState;
use parking_lot::RwLock;
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Max subscription rows per (user, tenant) — a browser has one per profile;
/// this bounds a malicious/looping client from unbounded row growth.
const MAX_SUBS_PER_USER_TENANT: usize = 20;
/// Per-tenant delivered-id retention (FIFO). Bounds memory while covering far
/// more than any tenant's live-notification set, so a stable-id notification
/// never re-delivers within a reasonable horizon.
const DELIVERED_CAP_PER_TENANT: usize = 1000;
/// SMS verification code validity + resend cooldown.
const SMS_CODE_TTL_MS: u64 = 10 * 60_000;

// ============================ store ============================

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct PushSubscription {
    /// Push-service endpoint URL — the row's identity (upsert key).
    pub endpoint: String,
    /// Subscriber public key (base64url, 65-byte uncompressed P-256 point).
    pub p256dh: String,
    /// Subscriber auth secret (base64url, 16 bytes).
    pub auth: String,
    pub tenant: String,
    pub user_id: String,
    /// Human label ("Chrome · macOS") for the settings-page device list.
    #[serde(default)]
    pub label: String,
    pub created_ms: u64,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SmsTarget {
    pub user_id: String,
    pub tenant: String,
    /// E.164 ("+15551234567") — validated at the API boundary.
    pub phone: String,
    /// The user WANTS SMS on (their toggle) — but delivery also requires
    /// `verified`. A number never receives notifications until its owner
    /// proves control by entering the code we texted it.
    pub enabled: bool,
    /// Owner-of-the-number proof: set true only after `verify_sms` matches the
    /// code we sent. Reset to false whenever the phone number changes.
    #[serde(default)]
    pub verified: bool,
    /// Pending verification code (cleared once verified); never returned to any
    /// client.
    #[serde(default)]
    pub pending_code: Option<String>,
    #[serde(default)]
    pub code_sent_ms: u64,
    pub created_ms: u64,
}

/// VAPID signing keypair. The PKCS#8 secret must be FLEET-UNIFORM (generated
/// once on the leader, adopted by followers via the sync registry): a push
/// subscription is bound to the public key it was created against, so per-node
/// keys would strand every subscription minted through another node.
#[derive(Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct VapidKeys {
    /// base64url uncompressed P-256 public point (the `applicationServerKey`).
    pub public_b64: String,
    /// base64url PKCS#8 v1 document for the signing key.
    pub pkcs8_b64: String,
}

/// Serializable whole-store snapshot (sync registry + persist).
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct PushState {
    pub subs: Vec<PushSubscription>,
    pub sms: Vec<SmsTarget>,
    /// tenant → notification ids already delivered (FIFO-bounded). Deduping on
    /// the stable notification id (not a ts watermark) means an ongoing
    /// anomaly whose id is constant but whose ts churns every recompute is
    /// delivered exactly ONCE, and a leader flap can't re-deliver it.
    #[serde(default)]
    pub delivered: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub vapid: VapidKeys,
}

#[derive(Default)]
pub struct PushStore {
    inner: RwLock<PushState>,
}

/// Outcome of a subscribe attempt (mapped to an HTTP status by the handler).
pub enum SubscribeResult {
    Ok,
    /// Endpoint already registered to a different user — refused (integrity).
    EndpointOwnedByOther,
    /// Per-(user,tenant) subscription cap reached.
    CapReached,
}

impl PushStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Upsert this browser's subscription. Refuses to re-home an endpoint that
    /// already belongs to a DIFFERENT user (an authed caller who learned
    /// another user's endpoint URL must not be able to silence/hijack their
    /// row), and caps rows per (user, tenant).
    pub fn upsert_subscription(&self, sub: PushSubscription) -> SubscribeResult {
        let mut s = self.inner.write();
        if let Some(existing) = s.subs.iter().find(|x| x.endpoint == sub.endpoint) {
            if existing.user_id != sub.user_id {
                return SubscribeResult::EndpointOwnedByOther;
            }
        } else {
            let owned = s.subs.iter().filter(|x| x.user_id == sub.user_id && x.tenant == sub.tenant).count();
            if owned >= MAX_SUBS_PER_USER_TENANT {
                return SubscribeResult::CapReached;
            }
        }
        // Same-user re-subscribe (possibly under a newly-switched tenant) MOVES
        // the row — a device follows the tenant its own user last registered it
        // for, never fans one endpoint across tenants.
        s.subs.retain(|x| x.endpoint != sub.endpoint);
        s.subs.push(sub);
        s.subs.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
        SubscribeResult::Ok
    }

    /// Remove by endpoint. Scoped to `user_id` when `Some` (an API caller may
    /// only remove their own rows); `None` is the dispatcher pruning a
    /// push-service-reported-dead endpoint regardless of owner.
    pub fn remove_subscription(&self, endpoint: &str, user_id: Option<&str>) -> bool {
        let mut s = self.inner.write();
        let before = s.subs.len();
        s.subs.retain(|x| x.endpoint != endpoint || user_id.is_some_and(|u| x.user_id != u));
        s.subs.len() != before
    }

    /// Purge every push subscription + SMS target for a (user, tenant) — the
    /// membership-revocation hook called when a user is removed from a team so
    /// an ex-member stops receiving that team's notifications immediately.
    pub fn purge_user_tenant(&self, user_id: &str, tenant: &str) -> usize {
        let mut s = self.inner.write();
        let before = s.subs.len() + s.sms.len();
        s.subs.retain(|x| !(x.user_id == user_id && x.tenant == tenant));
        s.sms.retain(|x| !(x.user_id == user_id && x.tenant == tenant));
        before - (s.subs.len() + s.sms.len())
    }

    pub fn subscriptions_for_tenant(&self, tenant: &str) -> Vec<PushSubscription> {
        self.inner.read().subs.iter().filter(|s| s.tenant == tenant).cloned().collect()
    }

    pub fn devices_for(&self, user_id: &str, tenant: &str) -> Vec<PushSubscription> {
        self.inner
            .read()
            .subs
            .iter()
            .filter(|s| s.user_id == user_id && s.tenant == tenant)
            .cloned()
            .collect()
    }

    /// Store a phone as PENDING verification: keeps `verified=false` and stashes
    /// the code, so no delivery happens until `verify_sms`. Changing the number
    /// clears any prior verification. Returns false (no code stored) if a fresh
    /// code was sent within the cooldown, to bound resend-driven SMS spend.
    pub fn set_sms_pending(&self, user_id: &str, tenant: &str, phone: &str, code: &str, now: u64) -> bool {
        let mut s = self.inner.write();
        if let Some(t) = s.sms.iter_mut().find(|x| x.user_id == user_id && x.tenant == tenant) {
            if t.phone == phone && t.verified {
                // Re-confirming the same, already-verified number: no new code.
                return false;
            }
            if now.saturating_sub(t.code_sent_ms) < 60_000 {
                return false; // cooldown
            }
            t.phone = phone.to_string();
            t.verified = false;
            t.enabled = false;
            t.pending_code = Some(code.to_string());
            t.code_sent_ms = now;
            return true;
        }
        s.sms.push(SmsTarget {
            user_id: user_id.to_string(),
            tenant: tenant.to_string(),
            phone: phone.to_string(),
            enabled: false,
            verified: false,
            pending_code: Some(code.to_string()),
            code_sent_ms: now,
            created_ms: now,
        });
        s.sms.sort_by(|a, b| (&a.user_id, &a.tenant).cmp(&(&b.user_id, &b.tenant)));
        true
    }

    /// Confirm ownership of the number. On a code match within TTL, marks the
    /// row verified + enabled and clears the code.
    pub fn verify_sms(&self, user_id: &str, tenant: &str, code: &str, now: u64) -> bool {
        let mut s = self.inner.write();
        let Some(t) = s.sms.iter_mut().find(|x| x.user_id == user_id && x.tenant == tenant) else {
            return false;
        };
        let ok = t.pending_code.as_deref() == Some(code)
            && !code.is_empty()
            && now.saturating_sub(t.code_sent_ms) < SMS_CODE_TTL_MS;
        if ok {
            t.verified = true;
            t.enabled = true;
            t.pending_code = None;
        }
        ok
    }

    /// Toggle an ALREADY-VERIFIED number on/off (a verified user turning SMS
    /// off then back on needs no re-verification). No-op if unverified.
    pub fn set_sms_enabled(&self, user_id: &str, tenant: &str, enabled: bool) -> bool {
        let mut s = self.inner.write();
        if let Some(t) = s.sms.iter_mut().find(|x| x.user_id == user_id && x.tenant == tenant && x.verified) {
            t.enabled = enabled;
            true
        } else {
            false
        }
    }

    pub fn sms_for(&self, user_id: &str, tenant: &str) -> Option<SmsTarget> {
        self.inner.read().sms.iter().find(|x| x.user_id == user_id && x.tenant == tenant).cloned()
    }

    /// Delivery targets: verified AND enabled only.
    pub fn sms_targets_for_tenant(&self, tenant: &str) -> Vec<SmsTarget> {
        self.inner.read().sms.iter().filter(|x| x.tenant == tenant && x.enabled && x.verified).cloned().collect()
    }

    /// Tenants with at least one deliverable target — the dispatcher's work list.
    pub fn tenants_with_targets(&self) -> Vec<String> {
        let s = self.inner.read();
        let mut t: Vec<String> = s
            .subs
            .iter()
            .map(|x| x.tenant.clone())
            .chain(s.sms.iter().filter(|x| x.enabled && x.verified).map(|x| x.tenant.clone()))
            .collect();
        t.sort();
        t.dedup();
        t
    }

    pub fn was_delivered(&self, tenant: &str, id: &str) -> bool {
        self.inner.read().delivered.get(tenant).is_some_and(|v| v.iter().any(|x| x == id))
    }

    pub fn mark_delivered(&self, tenant: &str, id: &str) {
        let mut s = self.inner.write();
        let v = s.delivered.entry(tenant.to_string()).or_default();
        if v.iter().any(|x| x == id) {
            return;
        }
        v.push(id.to_string());
        let overflow = v.len().saturating_sub(DELIVERED_CAP_PER_TENANT);
        if overflow > 0 {
            v.drain(0..overflow);
        }
    }

    /// Read the current VAPID keypair WITHOUT generating — followers must never
    /// mint their own (that forks the fleet key). Empty until the leader
    /// generates and the sync registry propagates it.
    pub fn vapid(&self) -> VapidKeys {
        self.inner.read().vapid.clone()
    }

    /// Generate the VAPID keypair if absent. LEADER-ONLY — the single caller is
    /// `ensure_vapid_on_leader`, gated on `is_control_plane_leader`. Returns
    /// true if it just generated (so the caller persists).
    fn ensure_vapid(&self) -> bool {
        let mut s = self.inner.write();
        if !s.vapid.public_b64.is_empty() {
            return false;
        }
        if let Some(k) = generate_vapid() {
            s.vapid = k;
            return true;
        }
        false
    }

    pub fn snapshot(&self) -> PushState {
        self.inner.read().clone()
    }

    pub fn load(&self, st: PushState) {
        *self.inner.write() = st;
    }
}

/// Generate + persist the fleet VAPID keypair, but only when THIS node is the
/// control-plane leader (a follower would fork the key). Safe to call every
/// tick — a no-op once the key exists.
pub fn ensure_vapid_on_leader(c: &Arc<CloudState>) {
    if c.is_control_plane_leader() && c.push.ensure_vapid() {
        crate::persist::persist(c);
    }
}

// ============================ endpoint allowlist ============================

/// Known Web Push service hosts. Refusing arbitrary endpoints stops our
/// VAPID-signed, server-side POSTs from being aimed at attacker-chosen URLs
/// (SSRF/amplification) via a forged subscription.
fn allowed_push_host(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("https://") else { return false };
    let host = rest.split('/').next().unwrap_or("").split(':').next().unwrap_or("");
    const SUFFIXES: &[&str] = &[
        "fcm.googleapis.com",              // Chrome / Android (FCM)
        "android.googleapis.com",          // legacy GCM/FCM
        ".push.services.mozilla.com",      // Firefox (autopush)
        ".notify.windows.com",             // Edge (WNS)
        ".push.apple.com",                 // Safari (Apple Push)
    ];
    SUFFIXES.iter().any(|s| if let Some(dom) = s.strip_prefix('.') { host == *s || host.ends_with(s) || host == dom } else { host == *s })
}

/// Public entry for the handler: validate an endpoint (scheme + host allowlist)
/// and the subscriber key sizes before storing a row.
pub fn valid_subscription_input(endpoint: &str, p256dh: &str, auth: &str) -> Result<(), &'static str> {
    if !allowed_push_host(endpoint) {
        return Err("endpoint host is not a recognized push service");
    }
    match b64u_decode(p256dh) {
        Some(k) if k.len() == 65 => {}
        _ => return Err("p256dh must be a 65-byte base64url P-256 point"),
    }
    match b64u_decode(auth) {
        Some(a) if a.len() == 16 => {}
        _ => return Err("auth must be a 16-byte base64url secret"),
    }
    Ok(())
}

// ============================ base64url helpers ============================

fn b64u(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn b64u_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
        .or_else(|| base64::engine::general_purpose::STANDARD.decode(s).ok())
}

// ============================ VAPID (RFC 8292) ============================

fn generate_vapid() -> Option<VapidKeys> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).ok()?;
    let pair = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).ok()?;
    use ring::signature::KeyPair;
    Some(VapidKeys {
        public_b64: b64u(pair.public_key().as_ref()),
        pkcs8_b64: b64u(pkcs8.as_ref()),
    })
}

/// `Authorization: vapid t=<jwt>, k=<pub>` for a push-service origin.
fn vapid_auth_header(keys: &VapidKeys, endpoint: &str) -> Option<String> {
    // Origin (scheme://host[:port]) without pulling in a URL crate: endpoints
    // are always absolute https URLs from the browser's push service.
    let (scheme, rest) = endpoint.split_once("://")?;
    let host = rest.split('/').next()?;
    let aud = format!("{scheme}://{host}");
    let now = hive_core::now_ms() / 1000;
    let header = b64u(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = b64u(
        serde_json::to_vec(&serde_json::json!({
            "aud": aud,
            "exp": now + 12 * 3600,
            "sub": "mailto:ops@shadw.cloud",
        }))
        .ok()?
        .as_slice(),
    );
    let signing_input = format!("{header}.{claims}");
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = b64u_decode(&keys.pkcs8_b64)?;
    let pair = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng).ok()?;
    let sig = pair.sign(&rng, signing_input.as_bytes()).ok()?;
    Some(format!("vapid t={signing_input}.{}, k={}", b64u(sig.as_ref()), keys.public_b64))
}

// ============================ RFC 8291 content encryption ============================

/// HKDF with arbitrary output length (ring's typed lengths via a shim).
struct HkdfLen(usize);
impl ring::hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf_expand(salt: &[u8], ikm: &[u8], info: &[u8], out_len: usize) -> Option<Vec<u8>> {
    let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(ikm);
    let info_parts = [info];
    let okm = prk.expand(&info_parts, HkdfLen(out_len)).ok()?;
    let mut out = vec![0u8; out_len];
    okm.fill(&mut out).ok()?;
    Some(out)
}

/// Encrypt `payload` for a subscriber per RFC 8291 (`aes128gcm` coding),
/// returning the full body (coding header ‖ ciphertext). All-`Option`, never
/// panics on malformed subscription keys.
fn encrypt_payload(p256dh_b64: &str, auth_b64: &str, payload: &[u8]) -> Option<Vec<u8>> {
    let ua_public = b64u_decode(p256dh_b64)?; // 65-byte uncompressed point
    let auth_secret = b64u_decode(auth_b64)?; // 16 bytes
    if ua_public.len() != 65 || auth_secret.len() != 16 {
        return None;
    }
    let rng = ring::rand::SystemRandom::new();
    let eph = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng).ok()?;
    let as_public = eph.compute_public_key().ok()?.as_ref().to_vec(); // 65 bytes
    let peer = ring::agreement::UnparsedPublicKey::new(&ring::agreement::ECDH_P256, ua_public.clone());
    let ecdh = ring::agreement::agree_ephemeral(eph, &peer, |secret| secret.to_vec()).ok()?;

    // ikm = HKDF(salt=auth_secret, ikm=ecdh, info="WebPush: info"‖0x00‖ua_pub‖as_pub, 32)
    let mut info = Vec::with_capacity(14 + 65 + 65);
    info.extend_from_slice(b"WebPush: info\0");
    info.extend_from_slice(&ua_public);
    info.extend_from_slice(&as_public);
    let ikm = hkdf_expand(&auth_secret, &ecdh, &info, 32)?;

    let mut salt = [0u8; 16];
    rng.fill(&mut salt).ok()?;
    let cek = hkdf_expand(&salt, &ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf_expand(&salt, &ikm, b"Content-Encoding: nonce\0", 12)?;

    // Single record: payload ‖ 0x02 (last-record delimiter), AES-128-GCM.
    let mut record = payload.to_vec();
    record.push(0x02);
    let key = ring::aead::LessSafeKey::new(ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &cek).ok()?);
    let n = ring::aead::Nonce::try_assume_unique_for_key(&nonce).ok()?;
    key.seal_in_place_append_tag(n, ring::aead::Aad::empty(), &mut record).ok()?;

    // aes128gcm coding header: salt(16) ‖ rs(4) ‖ idlen(1) ‖ keyid(as_public).
    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&record);
    Some(body)
}

// ============================ delivery ============================

/// One web push to one subscription. `Ok(true)` delivered; `Ok(false)` the
/// push service reported the subscription permanently gone (prune it);
/// `Err` transient/other failure (keep the subscription, log).
pub async fn send_web_push(c: &Arc<CloudState>, sub: &PushSubscription, payload: &serde_json::Value) -> Result<bool, String> {
    let keys = c.push.vapid();
    if keys.public_b64.is_empty() {
        return Err("vapid keys unavailable".into());
    }
    let auth = vapid_auth_header(&keys, &sub.endpoint).ok_or("vapid header build failed")?;
    let bytes = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let body = encrypt_payload(&sub.p256dh, &sub.auth, &bytes).ok_or("payload encryption failed (bad subscription keys?)")?;
    let r = c
        .http
        .post(&sub.endpoint)
        .header("authorization", auth)
        .header("content-encoding", "aes128gcm")
        .header("content-type", "application/octet-stream")
        .header("ttl", "86400")
        .header("urgency", "normal")
        .timeout(std::time::Duration::from_secs(15))
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    match r.status().as_u16() {
        200..=299 => Ok(true),
        404 | 410 => Ok(false),
        // Do NOT echo the push service's raw response body (belt-and-braces
        // against leaking anything sensitive into logs) — just the status.
        s => Err(format!("push service returned {s}")),
    }
}

/// Send one SMS via Textbelt. `test_mode` appends the documented `_test`
/// suffix (full request validation, nothing sent, no quota consumed).
pub async fn send_sms(c: &Arc<CloudState>, phone: &str, message: &str, test_mode: bool) -> Result<(), String> {
    let key = std::env::var("HIVE_TEXTBELT_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err("HIVE_TEXTBELT_KEY not configured".into());
    }
    let key = if test_mode { format!("{key}_test") } else { key };
    let r = c
        .http
        .post("https://textbelt.com/text")
        .form(&[("phone", phone), ("message", message), ("key", &key)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
    if v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
        Ok(())
    } else {
        let raw = v.get("error").and_then(|e| e.as_str()).unwrap_or("textbelt send failed");
        Err(sanitize_textbelt_error(raw))
    }
}

/// Textbelt echoes the API key back inside some error messages — most notably
/// the out-of-quota one: "Out of quota. Refill at
/// https://textbelt.com/purchase?key=<THE FULL SECRET KEY>". This error is
/// surfaced all the way to the browser (via push_test / push_sms_put), so it
/// MUST be scrubbed of the key before it leaves the process. Redact the exact
/// key value AND any residual `key=`/purchase-URL fragment, and map the known
/// out-of-quota case to a clean message with no URL at all.
fn sanitize_textbelt_error(raw: &str) -> String {
    if raw.to_ascii_lowercase().contains("out of quota") {
        return "SMS quota exhausted — refill your Textbelt account to send more.".into();
    }
    let mut s = raw.to_string();
    if let Ok(key) = std::env::var("HIVE_TEXTBELT_KEY") {
        if !key.is_empty() {
            s = s.replace(&key, "***");
            s = s.replace(&format!("{key}_test"), "***");
        }
    }
    // Belt-and-braces: drop any lingering key= query fragment (and everything
    // after it up to whitespace) even if the env value didn't textually match.
    if let Some(i) = s.find("key=") {
        let tail_end = s[i..].find(char::is_whitespace).map(|w| i + w).unwrap_or(s.len());
        s.replace_range(i..tail_end, "key=***");
    }
    s
}

/// Remaining Textbelt quota, cached (short TTL) so a settings read never blocks
/// on a live Textbelt round trip.
pub async fn sms_quota_cached(c: &Arc<CloudState>) -> Option<i64> {
    {
        let g = QUOTA_CACHE.read();
        if let Some((at, v)) = *g {
            if hive_core::now_ms().saturating_sub(at) < 60_000 {
                return v;
            }
        }
    }
    let v = sms_quota(c).await;
    *QUOTA_CACHE.write() = Some((hive_core::now_ms(), v));
    v
}

static QUOTA_CACHE: RwLock<Option<(u64, Option<i64>)>> = RwLock::new(None);

async fn sms_quota(c: &Arc<CloudState>) -> Option<i64> {
    let key = std::env::var("HIVE_TEXTBELT_KEY").ok().filter(|k| !k.is_empty())?;
    let r = c
        .http
        .get(format!("https://textbelt.com/quota/{key}"))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = r.json().await.ok()?;
    // Clamp to >=0: Textbelt reports a negative quotaRemaining for an
    // exhausted/invalid key, which read as a nonsensical "-1 SMS remaining" in
    // the UI. 0 is the honest "none left" the settings page renders sensibly.
    v.get("quotaRemaining").and_then(|q| q.as_i64()).map(|q| q.max(0))
}

/// Format one notification as an SMS body (single segment, 160 chars).
pub fn sms_body(n: &crate::notifications::Notification) -> String {
    let mut s = format!("[shadw] {} {}: {}", n.severity, n.project, n.message);
    if s.len() > 160 {
        s.truncate(157);
        s.push_str("...");
    }
    s
}

/// Web-push JSON payload for one notification (minimal on purpose — no env
/// values, no URLs beyond our own dashboard route, nothing secret-bearing).
pub fn push_payload(n: &crate::notifications::Notification) -> serde_json::Value {
    serde_json::json!({
        "title": format!("{} · {}", n.project, n.category),
        "body": n.message,
        "severity": n.severity,
        "category": n.category,
        "project": n.project,
        "url": format!("/projects/{}", n.project),
        "id": n.id,
        "ts_ms": n.ts_ms,
    })
}

// ============================ dispatcher ============================

/// SMS rate bound: at most this many SMS SENDS per tenant per hour (protects
/// the Textbelt quota; web push has no such cost).
const SMS_HOURLY_CAP: usize = 3;
/// At most this many web-push notifications per tenant per tick.
const PUSH_TICK_CAP: usize = 5;
/// Bounded fan-out concurrency for the per-notification sends, so one slow push
/// service can't head-of-line-block the rest of a tenant's (or the fleet's)
/// delivery.
const SEND_CONCURRENCY: usize = 8;

/// Leader-only delivery loop. Followers idle (checked every tick, so a
/// re-election hands over within one interval); the leader computes each
/// target tenant's live notifications with the SAME `build_notifications` the
/// inbox bell uses and delivers anything not already delivered (per stable id),
/// scoped strictly to that tenant.
pub fn spawn_push_dispatcher(c: Arc<CloudState>) {
    tokio::spawn(async move {
        // tenant → send timestamps within the trailing hour (real SENDS, not
        // notifications). In-memory; a restart resets the window, bounded by
        // the cap regardless.
        let mut sms_sent: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if !c.is_control_plane_leader() {
                continue;
            }
            // Generate the fleet VAPID key on the leader if it doesn't exist yet.
            ensure_vapid_on_leader(&c);

            let tenants = c.push.tenants_with_targets();
            if tenants.is_empty() {
                continue;
            }
            let mut dirty = false;
            for tenant in tenants {
                let mut notifs: Vec<_> = crate::admin::build_notifications(&c, &tenant)
                    .into_iter()
                    // Never deliver a notification the tenant doesn't
                    // affirmatively own (guards the orphan/untagged-project
                    // cross-tenant fallback in project_owned_by), already-read,
                    // or archived, or that we've already delivered.
                    .filter(|n| !n.archived && !c.push.was_delivered(&tenant, &n.id) && crate::admin::project_owned_by(&c, &n.project, &tenant))
                    .collect();
                if notifs.is_empty() {
                    continue;
                }
                notifs.sort_by_key(|n| n.ts_ms);
                let overflow = notifs.len().saturating_sub(PUSH_TICK_CAP);
                let send: Vec<_> = notifs.into_iter().skip(overflow).collect();

                let subs = c.push.subscriptions_for_tenant(&tenant);
                for n in &send {
                    let payload = push_payload(n);
                    // Fan the web pushes out with bounded concurrency; a hung
                    // endpoint can't stall the others (each has its own 15s cap).
                    for chunk in subs.chunks(SEND_CONCURRENCY) {
                        let results = futures::future::join_all(chunk.iter().map(|sub| {
                            let c = c.clone();
                            let payload = payload.clone();
                            async move { (sub.endpoint.clone(), send_web_push(&c, sub, &payload).await) }
                        }))
                        .await;
                        for (endpoint, res) in results {
                            match res {
                                Ok(true) => {}
                                Ok(false) => {
                                    tracing::info!(%endpoint, "pruning dead push subscription (404/410)");
                                    c.push.remove_subscription(&endpoint, None);
                                    // `dirty` is set unconditionally at
                                    // mark_delivered below (runs every
                                    // iteration), so the prune is persisted too.
                                }
                                Err(e) => tracing::warn!(tenant = %tenant, error = %e, "web push send failed"),
                            }
                        }
                    }

                    if n.severity == "error" {
                        let now = hive_core::now_ms();
                        for target in c.push.sms_targets_for_tenant(&tenant) {
                            let window = sms_sent.entry(tenant.clone()).or_default();
                            window.retain(|t| now.saturating_sub(*t) < 3_600_000);
                            if window.len() >= SMS_HOURLY_CAP {
                                break; // tenant hourly SMS budget spent
                            }
                            match send_sms(&c, &target.phone, &sms_body(n), false).await {
                                Ok(()) => window.push(now), // count real SENDS
                                Err(e) => tracing::warn!(tenant = %tenant, error = %e, "sms send failed"),
                            }
                        }
                    }

                    // Mark delivered regardless of per-send outcome: a
                    // persistent failure must never turn into an every-30s
                    // retry storm (the inbox still shows it; retrying the push
                    // service forever is worse than one miss).
                    c.push.mark_delivered(&tenant, &n.id);
                    dirty = true;
                }
            }
            if dirty {
                crate::persist::persist(&c);
            }
        }
    });
}
