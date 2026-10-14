//! Push delivery for the notification inbox, scoped per tenant: Web Push (VAPID + RFC 8291
//! `aes128gcm`) and SMS (hosted Textbelt, then a self-hosted fallback, then a direct-MX
//! carrier-gateway blast).
//!
//! Subscriptions and SMS targets are stored under the caller's VERIFIED tenant (the JWT claim,
//! never a raw header), and the dispatcher fans a tenant's notifications out ONLY to rows stored
//! under that exact tenant, so cross-tenant delivery is structurally impossible rather than merely
//! filtered.
//!
//! The store rides the leader→follower `store_sync` registry, and the delivery loop acts only where
//! `leadership::may_act(PushDispatch)` admits, so the fleet never double-sends. Web Push is
//! implemented natively on `ring` (ES256 VAPID JWT per RFC 8292; ECDH + HKDF + AES-128-GCM content
//! encryption per RFC 8291) over the shared `reqwest` client.

use crate::state::CloudState;
use parking_lot::RwLock;
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Max subscription rows per (user, tenant) — a browser has one per profile;
/// this bounds a malicious/looping client from unbounded row growth.
const MAX_SUBS_PER_USER_TENANT: usize = 20;
/// Per-tenant delivered-id retention (FIFO): bounds memory while covering far more than any
/// tenant's live-notification set, so a stable-id notification never re-delivers within a
/// reasonable horizon.
const DELIVERED_CAP_PER_TENANT: usize = 1000;
/// SMS verification code validity; the resend cooldown (60 s) is enforced in `set_sms_pending`.
const SMS_CODE_TTL_MS: u64 = 10 * 60_000;

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
    /// The user WANTS SMS on (their toggle); delivery also requires `verified`, so a number never
    /// receives notifications until its owner proves control by entering the code texted to it.
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

/// VAPID signing keypair. The PKCS#8 secret is FLEET-UNIFORM (generated once on the leader, adopted
/// by followers through the sync registry): a push subscription is bound to the public key it was
/// created against, so per-node keys would strand every subscription minted through another node.
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
    /// tenant → notification ids already delivered (FIFO-bounded). Deduping on the stable
    /// notification id, not a timestamp watermark, delivers an ongoing anomaly whose id is constant
    /// but whose `ts` churns on every recompute exactly ONCE, and a leader flap cannot re-deliver
    /// it.
    #[serde(default)]
    pub delivered: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub vapid: VapidKeys,
    /// Operator-set Textbelt API key, preferred over `HIVE_TEXTBELT_KEY`. Refilling is a paid
    /// action that mints a NEW key on Textbelt's side, and activating it through per-node env edits
    /// left SMS dead even after the user paid. Set through the operator-gated `POST
    /// /v1/push/sms-key`, it replicates fleet-wide with the rest of this store, so pasting it once
    /// activates every node, including the NA-egress relay peers. Empty means unset (env fallback);
    /// never serialized back to clients unmasked.
    #[serde(default)]
    pub sms_key_override: String,
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

    /// The operator-set Textbelt key, if any (trimmed, non-empty).
    pub fn sms_key_override(&self) -> Option<String> {
        let s = self.inner.read();
        let k = s.sms_key_override.trim();
        if k.is_empty() {
            None
        } else {
            Some(k.to_string())
        }
    }

    /// Set (non-empty) or clear (empty) the operator Textbelt key.
    pub fn set_sms_key_override(&self, key: &str) {
        self.inner.write().sms_key_override = key.trim().to_string();
    }

    /// Upserts this browser's subscription, keyed by endpoint. Refuses to re-home an endpoint that
    /// already belongs to a DIFFERENT user (an authenticated caller who learned another user's
    /// endpoint URL must not be able to silence or hijack their row) and caps rows per (user,
    /// tenant) at `MAX_SUBS_PER_USER_TENANT`. A same-user re-subscribe, possibly under a newly
    /// switched tenant, MOVES the row: a device follows the tenant its own user last registered it
    /// for and an endpoint is never fanned across tenants.
    pub fn upsert_subscription(&self, sub: PushSubscription) -> SubscribeResult {
        let mut s = self.inner.write();
        if let Some(existing) = s.subs.iter().find(|x| x.endpoint == sub.endpoint) {
            if existing.user_id != sub.user_id {
                return SubscribeResult::EndpointOwnedByOther;
            }
        } else {
            let owned = s
                .subs
                .iter()
                .filter(|x| x.user_id == sub.user_id && x.tenant == sub.tenant)
                .count();
            if owned >= MAX_SUBS_PER_USER_TENANT {
                return SubscribeResult::CapReached;
            }
        }
        s.subs.retain(|x| x.endpoint != sub.endpoint);
        s.subs.push(sub);
        s.subs.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
        SubscribeResult::Ok
    }

    /// Removes by endpoint. Scoped to `user_id` when `Some` (an API caller may only remove their
    /// own rows); `None` is the dispatcher pruning an endpoint its push service reported dead,
    /// regardless of owner.
    pub fn remove_subscription(&self, endpoint: &str, user_id: Option<&str>) -> bool {
        let mut s = self.inner.write();
        let before = s.subs.len();
        s.subs
            .retain(|x| x.endpoint != endpoint || user_id.is_some_and(|u| x.user_id != u));
        s.subs.len() != before
    }

    /// Purges every push subscription and SMS target of a (user, tenant): the membership-revocation
    /// hook run when a user is removed from a team, so an ex-member stops receiving that team's
    /// notifications immediately.
    pub fn purge_user_tenant(&self, user_id: &str, tenant: &str) -> usize {
        let mut s = self.inner.write();
        let before = s.subs.len() + s.sms.len();
        s.subs
            .retain(|x| !(x.user_id == user_id && x.tenant == tenant));
        s.sms
            .retain(|x| !(x.user_id == user_id && x.tenant == tenant));
        before - (s.subs.len() + s.sms.len())
    }

    pub fn subscriptions_for_tenant(&self, tenant: &str) -> Vec<PushSubscription> {
        self.inner
            .read()
            .subs
            .iter()
            .filter(|s| s.tenant == tenant)
            .cloned()
            .collect()
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

    /// Stores a phone as PENDING verification: keeps `verified=false` (and `enabled=false`) and
    /// stashes the code, so nothing is delivered until `verify_sms`; changing the number clears any
    /// prior verification. Returns false, storing no code, when the same number is already verified
    /// or a code was sent within the last 60 s (the resend cooldown bounds resend-driven SMS
    /// spend).
    pub fn set_sms_pending(
        &self,
        user_id: &str,
        tenant: &str,
        phone: &str,
        code: &str,
        now: u64,
    ) -> bool {
        let mut s = self.inner.write();
        if let Some(t) = s
            .sms
            .iter_mut()
            .find(|x| x.user_id == user_id && x.tenant == tenant)
        {
            if t.phone == phone && t.verified {
                return false;
            }
            if now.saturating_sub(t.code_sent_ms) < 60_000 {
                return false;
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
        s.sms
            .sort_by(|a, b| (&a.user_id, &a.tenant).cmp(&(&b.user_id, &b.tenant)));
        true
    }

    /// Confirm ownership of the number. On a code match within TTL, marks the
    /// row verified + enabled and clears the code.
    pub fn verify_sms(&self, user_id: &str, tenant: &str, code: &str, now: u64) -> bool {
        let mut s = self.inner.write();
        let Some(t) = s
            .sms
            .iter_mut()
            .find(|x| x.user_id == user_id && x.tenant == tenant)
        else {
            return false;
        };
        let ok = t
            .pending_code
            .as_deref()
            .is_some_and(|pending| crate::admin::ct_eq(pending, code))
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
        if let Some(t) = s
            .sms
            .iter_mut()
            .find(|x| x.user_id == user_id && x.tenant == tenant && x.verified)
        {
            t.enabled = enabled;
            true
        } else {
            false
        }
    }

    pub fn sms_for(&self, user_id: &str, tenant: &str) -> Option<SmsTarget> {
        self.inner
            .read()
            .sms
            .iter()
            .find(|x| x.user_id == user_id && x.tenant == tenant)
            .cloned()
    }

    /// Delivery targets: verified AND enabled only.
    pub fn sms_targets_for_tenant(&self, tenant: &str) -> Vec<SmsTarget> {
        self.inner
            .read()
            .sms
            .iter()
            .filter(|x| x.tenant == tenant && x.enabled && x.verified)
            .cloned()
            .collect()
    }

    /// Tenants with at least one deliverable target — the dispatcher's work list.
    pub fn tenants_with_targets(&self) -> Vec<String> {
        let s = self.inner.read();
        let mut t: Vec<String> = s
            .subs
            .iter()
            .map(|x| x.tenant.clone())
            .chain(
                s.sms
                    .iter()
                    .filter(|x| x.enabled && x.verified)
                    .map(|x| x.tenant.clone()),
            )
            .collect();
        t.sort();
        t.dedup();
        t
    }

    pub fn was_delivered(&self, tenant: &str, id: &str) -> bool {
        self.inner
            .read()
            .delivered
            .get(tenant)
            .is_some_and(|v| v.iter().any(|x| x == id))
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

    /// Reads the current VAPID keypair WITHOUT generating: followers must never mint their own
    /// (that forks the fleet key). Empty until the leader generates it and the sync registry
    /// propagates it.
    pub fn vapid(&self) -> VapidKeys {
        self.inner.read().vapid.clone()
    }

    /// Generates the VAPID keypair if absent. LEADER-ONLY: the single caller is
    /// `ensure_vapid_on_leader`, gated on `leadership::may_act(PushDispatch)`. Returns true if it
    /// just generated (so the caller persists).
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

/// Generates and persists the fleet VAPID keypair, but only on the push dispatcher's writer
/// (`leadership::may_act(PushDispatch)`: a node that merely believes it leads for a moment would
/// fork the key, stranding every subscription made against the other one). Safe to call every tick,
/// and from the key GET handler: a no-op once the key exists.
pub fn ensure_vapid_on_leader(c: &Arc<CloudState>) {
    if c.push.vapid().public_b64.is_empty()
        && crate::leadership::may_act(c, crate::leadership::Job::PushDispatch)
        && c.push.ensure_vapid()
    {
        crate::persist::persist(c);
    }
}

/// Whether `endpoint` is an https URL on a known Web Push service: FCM (`fcm.googleapis.com` for
/// Chrome/Android, plus the legacy `android.googleapis.com`), Firefox autopush
/// (`.push.services.mozilla.com`), Edge WNS (`.notify.windows.com`) or Safari (`.push.apple.com`).
/// Refusing arbitrary endpoints stops our VAPID-signed, server-side POSTs from being aimed at
/// attacker-chosen URLs (SSRF/amplification) via a forged subscription.
fn allowed_push_host(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    const SUFFIXES: &[&str] = &[
        "fcm.googleapis.com",
        "android.googleapis.com",
        ".push.services.mozilla.com",
        ".notify.windows.com",
        ".push.apple.com",
    ];
    SUFFIXES.iter().any(|s| {
        if let Some(dom) = s.strip_prefix('.') {
            host == *s || host.ends_with(s) || host == dom
        } else {
            host == *s
        }
    })
}

/// Public entry for the handler: validate an endpoint (scheme + host allowlist)
/// and the subscriber key sizes before storing a row.
pub fn valid_subscription_input(
    endpoint: &str,
    p256dh: &str,
    auth: &str,
) -> Result<(), &'static str> {
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

fn generate_vapid() -> Option<VapidKeys> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &rng,
    )
    .ok()?;
    let pair = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        pkcs8.as_ref(),
        &rng,
    )
    .ok()?;
    use ring::signature::KeyPair;
    Some(VapidKeys {
        public_b64: b64u(pair.public_key().as_ref()),
        pkcs8_b64: b64u(pkcs8.as_ref()),
    })
}

/// `Authorization: vapid t=<jwt>, k=<pub>` for a push-service origin.
/// The audience is the endpoint's origin (`scheme://host[:port]`), cut by hand instead of pulling
/// in a URL crate because endpoints are always absolute https URLs from the browser's push service.
fn vapid_auth_header(keys: &VapidKeys, endpoint: &str) -> Option<String> {
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
    let pair = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &pkcs8,
        &rng,
    )
    .ok()?;
    let sig = pair.sign(&rng, signing_input.as_bytes()).ok()?;
    Some(format!(
        "vapid t={signing_input}.{}, k={}",
        b64u(sig.as_ref()),
        keys.public_b64
    ))
}

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

/// Encrypts `payload` for a subscriber per RFC 8291 (`aes128gcm` coding) and returns the full body:
/// the coding header `salt(16) ‖ rs(4) ‖ idlen(1) ‖ keyid(as_public)` followed by one record,
/// `payload ‖ 0x02` (the last-record delimiter) sealed with AES-128-GCM under a key and nonce
/// derived by HKDF from the ECDH secret and the subscriber's auth secret. All-`Option`: malformed
/// subscription keys return `None`, never panic.
fn encrypt_payload(p256dh_b64: &str, auth_b64: &str, payload: &[u8]) -> Option<Vec<u8>> {
    let ua_public = b64u_decode(p256dh_b64)?;
    let auth_secret = b64u_decode(auth_b64)?;
    if ua_public.len() != 65 || auth_secret.len() != 16 {
        return None;
    }
    let rng = ring::rand::SystemRandom::new();
    let eph =
        ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng).ok()?;
    let as_public = eph.compute_public_key().ok()?.as_ref().to_vec();
    let peer =
        ring::agreement::UnparsedPublicKey::new(&ring::agreement::ECDH_P256, ua_public.clone());
    let ecdh = ring::agreement::agree_ephemeral(eph, &peer, |secret| secret.to_vec()).ok()?;

    let mut info = Vec::with_capacity(14 + 65 + 65);
    info.extend_from_slice(b"WebPush: info\0");
    info.extend_from_slice(&ua_public);
    info.extend_from_slice(&as_public);
    let ikm = hkdf_expand(&auth_secret, &ecdh, &info, 32)?;

    let mut salt = [0u8; 16];
    rng.fill(&mut salt).ok()?;
    let cek = hkdf_expand(&salt, &ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf_expand(&salt, &ikm, b"Content-Encoding: nonce\0", 12)?;

    let mut record = payload.to_vec();
    record.push(0x02);
    let key = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &cek).ok()?,
    );
    let n = ring::aead::Nonce::try_assume_unique_for_key(&nonce).ok()?;
    key.seal_in_place_append_tag(n, ring::aead::Aad::empty(), &mut record)
        .ok()?;

    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&record);
    Some(body)
}

/// Sends one web push to one subscription. `Ok(true)`: delivered; `Ok(false)`: the push service
/// reported the subscription permanently gone (404/410), so prune it; `Err`: a transient or other
/// failure (keep the subscription, log), carrying only the status and never the push service's raw
/// response body.
pub async fn send_web_push(
    c: &Arc<CloudState>,
    sub: &PushSubscription,
    payload: &serde_json::Value,
) -> Result<bool, String> {
    let keys = c.push.vapid();
    if keys.public_b64.is_empty() {
        return Err("vapid keys unavailable".into());
    }
    let auth = vapid_auth_header(&keys, &sub.endpoint).ok_or("vapid header build failed")?;
    let bytes = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let body = encrypt_payload(&sub.p256dh, &sub.auth, &bytes)
        .ok_or("payload encryption failed (bad subscription keys?)")?;
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
        s => Err(format!("push service returned {s}")),
    }
}

/// Sends one SMS: hosted Textbelt first (`send_sms_primary`), then the self-hosted fallback
/// (`send_sms_fallback`), then a direct-MX blast to the carrier gateways. A fallback success IS a
/// delivery and reports Ok, with a WARN naming the primary failure. `test_mode` appends Textbelt's
/// `_test` key suffix (full request validation, nothing sent, no quota consumed).
///
/// Textbelt's cheaper REGIONAL keys are gated on the API caller's IP geography, so a node whose
/// egress Textbelt classifies as non-North-American is rejected with 'North America region keys can
/// only be used from North American countries'; on that error the send is retried THROUGH a peer
/// with NA egress (`HIVE_SMS_EGRESS_NODES`) over the mesh. The direct-MX last resort needs an open
/// outbound :25, which cloud nodes block, so it goes to those peers first
/// (`/v1/push/sms-direct-mx`) and then runs locally.
pub async fn send_sms(
    c: &Arc<CloudState>,
    phone: &str,
    message: &str,
    test_mode: bool,
) -> Result<(), String> {
    let primary = send_sms_primary(c, phone, message, test_mode).await;
    match primary {
        Ok(()) => Ok(()),
        Err(primary_err) => match send_sms_fallback(c, phone, message).await {
            Ok(()) => {
                tracing::warn!(node = %c.node_name, %primary_err, "primary SMS provider failed; delivered via self-hosted fallback");
                Ok(())
            }
            Err(fb_err) => {
                for peer in sms_egress_peers(&c.node_name) {
                    let dm = serde_json::json!({ "phone": phone, "message": message });
                    if let Some(v) =
                        crate::admin::put_to_host(c, &peer, "/v1/push/sms-direct-mx", "", &dm).await
                    {
                        if v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
                            tracing::warn!(node = %c.node_name, %peer, %primary_err, %fb_err, "delivered via direct-MX carrier gateway on NA-egress peer");
                            return Ok(());
                        }
                    }
                }
                if let Ok(()) = send_direct_mx(phone, message).await {
                    tracing::warn!(node = %c.node_name, %primary_err, %fb_err, "delivered via local direct-MX carrier gateway");
                    return Ok(());
                }
                Err(format!("{primary_err}; self-hosted fallback: {fb_err}"))
            }
        },
    }
}

/// The hosted-Textbelt path: a direct send, retried through NA-egress peers on a region error.
async fn send_sms_primary(
    c: &Arc<CloudState>,
    phone: &str,
    message: &str,
    test_mode: bool,
) -> Result<(), String> {
    match send_sms_direct(c, phone, message, test_mode).await {
        Ok(()) => Ok(()),
        Err(raw) if is_region_error(&raw) => {
            tracing::warn!(node = %c.node_name, "textbelt rejected send for egress region; relaying via NA egress peer(s)");
            let mut last = raw;
            for peer in sms_egress_peers(&c.node_name) {
                let body = serde_json::json!({ "phone": phone, "message": message, "test_mode": test_mode });
                if let Some(v) =
                    crate::admin::put_to_host(c, &peer, "/v1/push/sms-relay", "", &body).await
                {
                    if v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
                        tracing::info!(%peer, "SMS relayed via NA egress peer");
                        return Ok(());
                    }
                    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
                        last = e.to_string();
                    }
                }
            }
            Err(sanitize_textbelt_error(c, &last))
        }
        Err(raw) => Err(sanitize_textbelt_error(c, &raw)),
    }
}

/// Self-hosted fallback: the OSS typpo/textbelt server (carrier email-to-SMS gateways) deployed as
/// a platform app at `sms.{platform_domain}`; `HIVE_SMS_FALLBACK_URL` overrides the endpoint, and
/// empty or `off` disables it. Its API takes `number` (not `phone`), requires a bare 9-10 digit US
/// number (so the leading `1` of the stored E.164 form is stripped) and replies `{success,
/// message?}`; its failure text, which carries no key material, is surfaced verbatim. The client
/// timeout is 90 s: the service fans an unknown-carrier number out to every gateway concurrently,
/// and one stalling provider can take about 66 s (6 s connect plus up to six SMTP replies at 10 s),
/// which a shorter timeout aborted mid-response-body.
async fn send_sms_fallback(c: &Arc<CloudState>, phone: &str, message: &str) -> Result<(), String> {
    let url = std::env::var("HIVE_SMS_FALLBACK_URL")
        .unwrap_or_else(|_| format!("https://sms.{}/text", c.platform_domain));
    if url.is_empty() || url.eq_ignore_ascii_case("off") {
        return Err("disabled".into());
    }
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    let local = if digits.len() == 11 && digits.starts_with('1') {
        digits[1..].to_string()
    } else {
        digits
    };
    let r = c
        .http
        .post(&url)
        .form(&[("number", local.as_str()), ("message", message)])
        .timeout(std::time::Duration::from_secs(90))
        .send()
        .await
        .map_err(|e| format!("unreachable: {e}"))?;
    let v: serde_json::Value = r.json().await.map_err(|e| format!("bad response: {e}"))?;
    if v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
        Ok(())
    } else {
        Err(v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("send failed")
            .to_string())
    }
}

/// The effective Textbelt key: the operator-set store override (dashboard, fleet-replicated) wins
/// over the `HIVE_TEXTBELT_KEY` env.
pub fn textbelt_key(c: &Arc<CloudState>) -> Option<String> {
    c.push.sms_key_override().or_else(|| {
        std::env::var("HIVE_TEXTBELT_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
    })
}

/// Drop the cached quota reading (call after the key changes so the settings
/// page reflects the NEW key's quota immediately, not the old key's for 60s).
pub fn reset_sms_quota_cache() {
    *QUOTA_CACHE.write() = None;
}

/// The single direct Textbelt call, no relay. Returns the RAW (unsanitized) Textbelt error so the
/// caller can classify it; every path that surfaces the error outward runs it through
/// `sanitize_textbelt_error` first.
async fn send_sms_direct(
    c: &Arc<CloudState>,
    phone: &str,
    message: &str,
    test_mode: bool,
) -> Result<(), String> {
    let Some(key) = textbelt_key(c) else {
        return Err("no Textbelt key configured (set one in Settings → Notifications, or HIVE_TEXTBELT_KEY)".into());
    };
    let key = if test_mode {
        format!("{key}_test")
    } else {
        key
    };
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
        Err(v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("textbelt send failed")
            .to_string())
    }
}

/// Textbelt's regional-key geography rejection (exact live wording:
/// "North America region keys can only be used from North American countries").
fn is_region_error(raw: &str) -> bool {
    let l = raw.to_ascii_lowercase();
    l.contains("region keys can only be used") || l.contains("region key can only be used")
}

/// Peers whose egress Textbelt reliably classifies as North American: comma-separated node names
/// from `HIVE_SMS_EGRESS_NODES`, default `fc-lax,fc-lax2` (the LA nodes, on residential egress).
/// Self is skipped: it already failed locally.
fn sms_egress_peers(self_name: &str) -> Vec<String> {
    std::env::var("HIVE_SMS_EGRESS_NODES")
        .unwrap_or_else(|_| "fc-lax,fc-lax2".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != self_name)
        .collect()
}

/// Mesh/HTTP relay executor: runs the DIRECT send on this node (never re-relays: the caller already
/// chose this node for its egress) and reports a sanitized result. Served by the
/// `/v1/push/sms-relay` admin route and its gossip arm.
pub async fn sms_relay_exec(c: &Arc<CloudState>, body: serde_json::Value) -> serde_json::Value {
    let phone = body
        .get("phone")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let test_mode = body
        .get("test_mode")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if phone.is_empty() || message.is_empty() {
        return serde_json::json!({ "success": false, "error": "missing phone/message" });
    }
    match send_sms_direct(c, phone, message, test_mode).await {
        Ok(()) => serde_json::json!({ "success": true }),
        Err(raw) => {
            serde_json::json!({ "success": false, "error": sanitize_textbelt_error(c, &raw) })
        }
    }
}

/// US carrier email-to-SMS gateway domains: the live subset of typpo/textbelt's `providers.us` (US
/// Cellular, Verizon, T-Mobile, AT&T, Nextech, Google Fi, Republic Wireless, Qwest), with dead
/// carriers whose MX no longer resolves dropped so a blast is not dominated by NXDOMAIN noise. An
/// unknown-carrier send still fans out to every listed gateway, and the recipient's real carrier is
/// whichever one accepts and delivers.
const US_SMS_GATEWAYS: &[&str] = &[
    "email.uscc.net",
    "vtext.com",
    "tmomail.net",
    "txt.att.net",
    "sms.ntwls.net",
    "msg.fi.google.com",
    "text.republicwireless.com",
    "qwestmp.com",
];

/// Deliver ONE plaintext message to one carrier-gateway recipient over SMTP.
/// Returns Ok on a 2xx to the final `.`; Err with the SMTP reason otherwise.
/// The MX is the lowest-preference exchange, contacted on port 25 with a 6 s connect timeout, and
/// every SMTP reply read is bounded at 10 s, so a peer that accepts the TCP session but never
/// replies cannot hang the whole blast. A domain with no MX answers an error; there is no
/// implicit-MX fallback to its address record.
async fn smtp_deliver_one(to: &str, from: &str, text: &str) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let domain = to
        .split('@')
        .nth(1)
        .ok_or_else(|| format!("bad recipient {to}"))?;
    let resolver = hickory_resolver::Resolver::builder_tokio()
        .map_err(|e| format!("resolver init: {e}"))?
        .build()
        .map_err(|e| format!("resolver build: {e}"))?;
    let host = match resolver.mx_lookup(domain).await {
        Ok(lookup) => {
            let mut mxs: Vec<(u16, String)> = lookup
                .answers()
                .iter()
                .filter_map(|r| match &r.data {
                    hickory_resolver::proto::rr::RData::MX(mx) => {
                        Some((mx.preference, mx.exchange.to_utf8()))
                    }
                    _ => None,
                })
                .collect();
            mxs.sort_by_key(|(pref, _)| *pref);
            mxs.into_iter()
                .next()
                .map(|(_, h)| h)
                .ok_or_else(|| format!("no MX for {domain}"))?
        }
        Err(_) => return Err(format!("no MX for {domain}")),
    };
    let host = host.trim_end_matches('.').to_string();

    let connect = tokio::time::timeout(
        std::time::Duration::from_secs(6),
        tokio::net::TcpStream::connect((host.as_str(), 25)),
    )
    .await
    .map_err(|_| format!("connect timeout to {host}:25"))?
    .map_err(|e| format!("connect {host}:25: {e}"))?;
    let mut sock = connect;

    /// Reads one SMTP reply: complete at a line whose fourth character is a space (`NNN `, final)
    /// rather than `-` (continuation), and its first digit must be `want`.
    async fn read_reply(sock: &mut tokio::net::TcpStream, want: u8) -> Result<String, String> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let n = tokio::time::timeout(std::time::Duration::from_secs(10), sock.read(&mut chunk))
                .await
                .map_err(|_| "SMTP reply timeout".to_string())?
                .map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Err("connection closed".into());
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(line) = String::from_utf8_lossy(&buf)
                .lines()
                .last()
                .map(str::to_string)
            {
                if line.len() >= 4 && line.as_bytes()[3] == b' ' {
                    let code = &line[..3];
                    if code.starts_with(want as char) {
                        return Ok(line);
                    }
                    return Err(format!("SMTP {line}"));
                }
            }
        }
    }
    async fn send_line(sock: &mut tokio::net::TcpStream, line: &str) -> Result<(), String> {
        sock.write_all(format!("{line}\r\n").as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))
    }

    let helo = std::env::var("SMS_HELO").unwrap_or_else(|_| "sms.shadw.cloud".into());
    read_reply(&mut sock, b'2').await?;
    send_line(&mut sock, &format!("HELO {helo}")).await?;
    read_reply(&mut sock, b'2').await?;
    send_line(&mut sock, &format!("MAIL FROM:<{from}>")).await?;
    read_reply(&mut sock, b'2').await?;
    send_line(&mut sock, &format!("RCPT TO:<{to}>")).await?;
    read_reply(&mut sock, b'2').await?;
    send_line(&mut sock, "DATA").await?;
    read_reply(&mut sock, b'3').await?;
    let body = format!(
        "From: {from}\r\nTo: {to}\r\nSubject: \r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{text}\r\n."
    );
    send_line(&mut sock, &body).await?;
    read_reply(&mut sock, b'2').await?;
    let _ = send_line(&mut sock, "QUIT").await;
    Ok(())
}

/// Best-effort direct-MX SMS to every US carrier gateway concurrently, with no external provider:
/// each gateway domain's MX is resolved and a minimal SMTP session delivers `<number>@<gateway>`.
/// It only reaches a carrier from a node whose outbound :25 is open AND whose IP that carrier's MX
/// accepts (cloud nodes block :25 entirely), so success is ANY gateway accepting, since the
/// recipient's real carrier is whichever one honors the number. Takes a US number in any common
/// form and uses its bare 10 digits (gateways want no country code); Err with the first failure
/// reason when none accepted.
pub async fn send_direct_mx(phone: &str, message: &str) -> Result<(), String> {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    let local = if digits.len() == 11 && digits.starts_with('1') {
        digits[1..].to_string()
    } else {
        digits
    };
    if local.len() != 10 {
        return Err(format!("not a US 10-digit number: {phone}"));
    }
    let from = std::env::var("SMS_FROM").unwrap_or_else(|_| "sms@shadw.cloud".into());
    let from = from
        .rsplit('<')
        .next()
        .unwrap_or(&from)
        .trim_end_matches('>')
        .to_string();
    let futs = US_SMS_GATEWAYS.iter().map(|gw| {
        let to = format!("{local}@{gw}");
        let from = from.clone();
        let message = message.to_string();
        async move { (to.clone(), smtp_deliver_one(&to, &from, &message).await) }
    });
    let results = futures::future::join_all(futs).await;
    let accepted: Vec<&String> = results
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(to, _)| to)
        .collect();
    if !accepted.is_empty() {
        tracing::info!(?accepted, "direct-MX SMS accepted by carrier gateway(s)");
        return Ok(());
    }
    let first_err = results
        .iter()
        .find_map(|(to, r)| r.as_ref().err().map(|e| format!("{to}: {e}")))
        .unwrap_or_else(|| "no gateways attempted".into());
    Err(first_err)
}

/// Direct-MX relay executor: run the direct-MX blast on THIS node (the caller
/// chose it for its open :25 egress). Served by `/v1/push/sms-direct-mx`.
pub async fn sms_direct_mx_exec(body: serde_json::Value) -> serde_json::Value {
    let phone = body
        .get("phone")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if phone.is_empty() || message.is_empty() {
        return serde_json::json!({ "success": false, "error": "missing phone/message" });
    }
    match send_direct_mx(phone, message).await {
        Ok(()) => serde_json::json!({ "success": true }),
        Err(e) => serde_json::json!({ "success": false, "error": e }),
    }
}

/// Scrubs a raw Textbelt error before it can leave the process. Textbelt echoes the API key inside
/// some messages (the out-of-quota one embeds a purchase URL carrying the FULL SECRET KEY) and
/// these errors reach the browser through the push test and SMS routes. The known out-of-quota case
/// maps to a clean message with no URL; otherwise the exact key value (from BOTH sources, the store
/// override and the env, also in its `_test` form) is redacted, as is any residual `key=` fragment
/// up to the next whitespace.
fn sanitize_textbelt_error(c: &Arc<CloudState>, raw: &str) -> String {
    if raw.to_ascii_lowercase().contains("out of quota") {
        return "SMS quota exhausted — refill your Textbelt account to send more.".into();
    }
    let mut s = raw.to_string();
    let mut keys: Vec<String> = Vec::new();
    if let Some(k) = c.push.sms_key_override() {
        keys.push(k);
    }
    if let Ok(k) = std::env::var("HIVE_TEXTBELT_KEY") {
        if !k.is_empty() {
            keys.push(k);
        }
    }
    for key in keys {
        s = s.replace(&format!("{key}_test"), "***");
        s = s.replace(&key, "***");
    }
    if let Some(i) = s.find("key=") {
        let tail_end = s[i..]
            .find(char::is_whitespace)
            .map(|w| i + w)
            .unwrap_or(s.len());
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

/// Remaining Textbelt quota of the effective key, clamped to >= 0: Textbelt reports a negative
/// `quotaRemaining` for an exhausted or invalid key, which the settings page rendered as a
/// nonsensical negative count.
async fn sms_quota(c: &Arc<CloudState>) -> Option<i64> {
    let key = textbelt_key(c)?;
    let r = c
        .http
        .get(format!("https://textbelt.com/quota/{key}"))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = r.json().await.ok()?;
    v.get("quotaRemaining")
        .and_then(|q| q.as_i64())
        .map(|q| q.max(0))
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

/// SMS rate bound: at most this many SMS SENDS per tenant per hour (protects
/// the Textbelt quota; web push has no such cost).
const SMS_HOURLY_CAP: usize = 3;
/// At most this many web-push notifications per tenant per tick.
const PUSH_TICK_CAP: usize = 5;
/// Bounded fan-out concurrency for the web pushes of one notification, so one slow push service
/// cannot head-of-line-block the rest of a tenant's (or the fleet's) delivery.
const SEND_CONCURRENCY: usize = 8;

/// Delivery loop, every 30 s and only where `leadership::may_act(PushDispatch)` admits (followers
/// idle, so a re-election hands over within one tick plus the job's tenure). Per tenant with a
/// deliverable target it builds the live notifications with the SAME `build_notifications` the
/// inbox bell uses and delivers the newest `PUSH_TICK_CAP` that are not archived, not already
/// delivered (by stable id) and affirmatively owned by the tenant (`project_owned_by` guards the
/// orphan/untagged-project cross-tenant fallback).
///
/// Web pushes go out in chunks of `SEND_CONCURRENCY`, each capped at 15 s, and a 404/410 prunes the
/// subscription. An `error`-severity notification also sends an SMS to each verified, enabled
/// target, at most `SMS_HOURLY_CAP` sends per tenant per hour (an in-memory window; a restart
/// resets it). A notification is marked delivered whatever the per-send outcome, so a persistent
/// failure never becomes an every-30-seconds retry storm.
pub fn spawn_push_dispatcher(c: Arc<CloudState>) {
    tokio::spawn(async move {
        let mut sms_sent: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if !crate::leadership::may_act(&c, crate::leadership::Job::PushDispatch) {
                continue;
            }
            ensure_vapid_on_leader(&c);

            let tenants = c.push.tenants_with_targets();
            if tenants.is_empty() {
                continue;
            }
            let mut dirty = false;
            for tenant in tenants {
                let mut notifs: Vec<_> = crate::admin::build_notifications(&c, &tenant)
                    .into_iter()
                    .filter(|n| {
                        !n.archived
                            && !c.push.was_delivered(&tenant, &n.id)
                            && crate::admin::project_owned_by(&c, &n.project, &tenant)
                    })
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
                    for chunk in subs.chunks(SEND_CONCURRENCY) {
                        let results =
                            futures::future::join_all(chunk.iter().map(|sub| {
                                let c = c.clone();
                                let payload = payload.clone();
                                async move {
                                    (sub.endpoint.clone(), send_web_push(&c, sub, &payload).await)
                                }
                            }))
                            .await;
                        for (endpoint, res) in results {
                            match res {
                                Ok(true) => {}
                                Ok(false) => {
                                    tracing::info!(%endpoint, "pruning dead push subscription (404/410)");
                                    c.push.remove_subscription(&endpoint, None);
                                }
                                Err(e) => {
                                    tracing::warn!(tenant = %tenant, error = %e, "web push send failed")
                                }
                            }
                        }
                    }

                    if n.severity == "error" {
                        let now = hive_core::now_ms();
                        for target in c.push.sms_targets_for_tenant(&tenant) {
                            let window = sms_sent.entry(tenant.clone()).or_default();
                            window.retain(|t| now.saturating_sub(*t) < 3_600_000);
                            if window.len() >= SMS_HOURLY_CAP {
                                break;
                            }
                            match send_sms(&c, &target.phone, &sms_body(n), false).await {
                                Ok(()) => window.push(now),
                                Err(e) => {
                                    tracing::warn!(tenant = %tenant, error = %e, "sms send failed")
                                }
                            }
                        }
                    }

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
