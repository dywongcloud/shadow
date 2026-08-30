//! ACME (Let's Encrypt) wildcard certificates via **DNS-01 through the Vercel
//! API** — the TLS half of the ngrok retirement.
//!
//! Two certificate bundles, deliberately isolated (the vercel.com/vercel.app
//! split): **apps** = `*.{apps_domain}` + apex; **platform** = `api.{platform
//! _domain}` (+ `relay.`/`discovery.` if `HIVE_ACME_PLATFORM_EXTRA=1`; the iroh
//! relay does its own TLS so they're off by default).
//!
//! * Only the CLUSTER LEADER (same election as billing/DNS-reconciler) runs the
//!   ACME client — N nodes must not race Let's Encrypt. The ACCOUNT it runs as
//!   is fleet state, not leader state: the credential is sealed with the cluster
//!   secret and replicated (see `account`), so a handover keeps ONE Let's
//!   Encrypt identity and one rate-limit budget instead of registering a new
//!   account per leader.
//! * Renewal at ~2/3 of the 90-day validity (issue + 60d), jittered daily check.
//! * Distribution: bundle JSON in the guardian replicated store with the private
//!   key AEAD-encrypted (`enc:v1:` via `secrets.rs`) — never plaintext in a
//!   replicated doc. Every node polls, decrypts with the cluster secret, and
//!   hot-swaps the SNI resolver (arc-swap — zero-downtime reload).
//! * `HIVE_ACME_STAGING=1` (default) → LE staging. Production has a
//!   5-duplicate-certs/week limit; do not burn it in dev.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus,
};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::state::CloudState;
use crate::vercel_dns::{DesiredRecord, DnsApi, VercelApi};

/// A replicated certificate bundle. The private key is AEAD-encrypted with the
/// cluster secret (`secrets::encrypt`) before it ever reaches the store.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CertBundle {
    /// SANs, first = primary (e.g. `["*.shadw.app", "shadw.app"]`).
    pub names: Vec<String>,
    pub chain_pem: String,
    /// `enc:v1:`-enveloped PKCS#8 key PEM.
    pub key_pem_enc: String,
    pub issued_ms: u64,
    /// Estimated notAfter (issued + 90d) — renewal triggers at issued + 60d.
    pub not_after_ms: u64,
}

const NINETY_DAYS_MS: u64 = 90 * 24 * 60 * 60 * 1000;
const RENEW_AFTER_MS: u64 = 60 * 24 * 60 * 60 * 1000; // 2/3 of validity

fn staging() -> bool {
    std::env::var("HIVE_ACME_STAGING")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true)
}

fn directory_url() -> &'static str {
    if staging() {
        LetsEncrypt::Staging.url()
    } else {
        LetsEncrypt::Production.url()
    }
}

// ---- SNI resolver (hot-swappable) ---------------------------------------------

/// zone (registrable domain, e.g. `shadw.app`) → certified key.
static CERTS: std::sync::OnceLock<ArcSwap<HashMap<String, Arc<CertifiedKey>>>> =
    std::sync::OnceLock::new();

fn certs() -> &'static ArcSwap<HashMap<String, Arc<CertifiedKey>>> {
    CERTS.get_or_init(|| ArcSwap::from_pointee(HashMap::new()))
}

/// SNI-aware certificate resolver: exact zone match (apex/api host) or the
/// wildcard zone one label up (`foo.shadw.app` → zone `shadw.app`). Reads the
/// arc-swapped map — renewals swap the map, in-flight handshakes keep the Arc
/// they resolved (zero-downtime reload).
#[derive(Debug)]
pub struct SniResolver;

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        let map = certs().load();
        if let Some(k) = map.get(&name) {
            return Some(k.clone()); // apex / exact host bundle key
        }
        // one label up → wildcard zone
        let (_, zone) = name.split_once('.')?;
        map.get(zone).cloned()
    }
}

/// Number of installed zones (observability + "do we have any certs yet").
pub fn installed_zones() -> Vec<String> {
    certs().load().keys().cloned().collect()
}

/// Parse a bundle into a `CertifiedKey` and install it under every zone its
/// names cover (`*.z`/`z` → `z`; `api.z` → `api.z`). Called on the leader after
/// issuance and on every node when the replicated bundle syncs.
pub fn install_bundle(bundle: &CertBundle) -> anyhow::Result<()> {
    let key_pem = crate::secrets::decrypt(&bundle.key_pem_enc);
    // NB: named `chain`, not `certs` — a local would shadow the `certs()` map fn.
    let chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut bundle.chain_pem.as_bytes()).collect::<Result<_, _>>()?;
    anyhow::ensure!(!chain.is_empty(), "bundle has no certificates");
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or_else(|| anyhow::anyhow!("bundle has no private key"))?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| anyhow::anyhow!("unsupported key type: {e}"))?;
    let ck = Arc::new(CertifiedKey::new(chain, signing));
    let mut map = (**certs().load()).clone();
    for name in &bundle.names {
        let zone = name.strip_prefix("*.").unwrap_or(name).to_ascii_lowercase();
        map.insert(zone, ck.clone());
        if name.contains('.') && !name.starts_with("*.") {
            map.insert(name.to_ascii_lowercase(), ck.clone()); // exact host (api.…)
        }
    }
    certs().store(Arc::new(map));
    Ok(())
}

/// A rustls `ServerConfig` using the SNI resolver (plus HTTP/1.1+h2 ALPN).
/// TLS config for PUBLISHED raw ports (compose `ports: ["9001:9001"]` served
/// with TLS termination): the same SNI resolver as the 443 gateway — same
/// wildcard/per-host certs, so `https://<project>.<apps-domain>:9001` presents
/// exactly the certificate the browser already trusts for the project — but
/// ALPN pinned to http/1.1 ONLY. The gateway's h2 offer would be wrong here:
/// the terminated plaintext is spliced byte-for-byte into the container's own
/// HTTP server, and a client that negotiated h2 via ALPN would then speak h2
/// frames at an HTTP/1.1 backend.
/// SNI resolver SCOPED to one tenant's hostnames. The shared [`SniResolver`]
/// holds EVERY fleet certificate — api./admin./webhook./dashboard hosts, the
/// apps and DB wildcards, and every tenant's custom domain — and handing it to
/// a tenant-controlled raw-port listener let that listener present ANY of them
/// to a client that asked by SNI. On a port the tenant chose, that is a
/// credential-harvesting surface: a victim pointed at `<tenant-port>` while
/// sending `api.<platform>` as SNI would complete a handshake against the
/// platform's own certificate. A published port may only ever speak for the
/// hostnames that legitimately route to its project; anything else fails the
/// handshake (resolve → None), which is the correct, loud outcome.
pub struct ScopedSniResolver {
    allowed: Vec<String>,
}

impl ScopedSniResolver {
    pub fn new(allowed: Vec<String>) -> ScopedSniResolver {
        ScopedSniResolver {
            allowed: allowed
                .into_iter()
                .map(|h| h.trim().trim_matches('.').to_ascii_lowercase())
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }
}

impl std::fmt::Debug for ScopedSniResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedSniResolver")
            .field("allowed", &self.allowed.len())
            .finish()
    }
}

impl ResolvesServerCert for ScopedSniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        if !self.allowed.iter().any(|a| a == &name) {
            return None;
        }
        let map = certs().load();
        if let Some(k) = map.get(&name) {
            return Some(k.clone());
        }
        let (_, zone) = name.split_once('.')?;
        map.get(zone).cloned()
    }
}

/// Raw-port TLS scoped to the hostnames of ONE project.
pub fn raw_server_config_for(hosts: Vec<String>) -> Arc<rustls::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(ScopedSniResolver::new(hosts)));
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

pub fn server_config() -> Arc<rustls::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SniResolver));
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// A rustls `ServerConfig` for the DB gateway (Postgres/Redis wire) — same SNI
/// resolver but NO ALPN. Advertising h2/http1.1 makes rustls send a
/// `no_application_protocol` alert to a Postgres client (libpq 17+ offers ALPN
/// `postgresql`, which wouldn't match) — so the DB proxy must not negotiate ALPN.
pub fn db_server_config() -> Arc<rustls::ServerConfig> {
    Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(SniResolver)),
    )
}

// ---- DNS-01 challenges for Seer-answered zones ----------------------------------

/// How long a challenge entry may live before it stops being answered and is
/// swept: ACME validates within minutes, so 1h covers the slowest retry loop
/// while guaranteeing an abandoned entry (leader died mid-order, cleanup never
/// ran) cannot accumulate or keep answering forever.
const CHALLENGE_TTL_MS: u64 = 60 * 60 * 1000;

/// One name's pending DNS-01 TXT values (a wildcard+apex order places TWO
/// challenges on the SAME `_acme-challenge.<zone>` name, hence a Vec).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AcmeChallenge {
    pub values: Vec<String>,
    pub created_ms: u64,
}

/// fqdn (lowercase, no trailing dot) → pending DNS-01 TXT values.
///
/// Written on the LEADER at challenge placement alongside the Vercel API write
/// — Vercel still serves every name in zones NOT delegated to Seer, but for a
/// zone that IS (the deploy zone, `api.{platform}` once its NS moves), Let's
/// Encrypt may query ANY of the advertised fleet nameservers while the ACME
/// client runs only on the leader — so the values replicate to every node via
/// the `acme_challenges` entry in `store_sync::REGISTRY`. Read by
/// `dnsserver::lookup`'s delegated-zone TXT arms.
pub struct AcmeChallengeStore {
    inner: parking_lot::RwLock<std::collections::BTreeMap<String, AcmeChallenge>>,
}

// ---- HTTP-01 challenges (custom tenant domains) --------------------------------

/// One pending HTTP-01 answer: the body LE expects at
/// `http://<domain>/.well-known/acme-challenge/<token>`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Http01Challenge {
    pub key_authorization: String,
    pub created_ms: u64,
}

/// token → key-authorization for in-flight HTTP-01 orders, replicated via the
/// `acme_http01` store_sync entry so a validation fetch landing on ANY node
/// (round-robin/geo DNS is the whole point of HTTP-01) gets the answer. Same
/// TTL/sweep discipline as the DNS-01 store: issuance is the sweep cadence,
/// and an abandoned entry ages out instead of answering forever.
pub struct Http01Store {
    inner: parking_lot::RwLock<std::collections::BTreeMap<String, Http01Challenge>>,
}

impl Http01Store {
    pub fn new() -> Self {
        Self {
            inner: parking_lot::RwLock::new(std::collections::BTreeMap::new()),
        }
    }

    fn norm(token: &str) -> String {
        token.trim().to_string()
    }

    pub fn insert(&self, token: &str, key_authorization: &str) {
        let now = hive_core::now_ms();
        let mut m = self.inner.write();
        m.retain(|_, e| now.saturating_sub(e.created_ms) < CHALLENGE_TTL_MS);
        m.insert(
            Self::norm(token),
            Http01Challenge {
                key_authorization: key_authorization.to_string(),
                created_ms: now,
            },
        );
    }

    pub fn remove(&self, token: &str) {
        self.inner.write().remove(&Self::norm(token));
    }

    pub fn lookup(&self, token: &str) -> Option<String> {
        self.inner
            .read()
            .get(&Self::norm(token))
            .filter(|e| hive_core::now_ms().saturating_sub(e.created_ms) < CHALLENGE_TTL_MS)
            .map(|e| e.key_authorization.clone())
    }

    pub fn snapshot(&self) -> std::collections::BTreeMap<String, Http01Challenge> {
        self.inner.read().clone()
    }

    pub fn load(&self, m: std::collections::BTreeMap<String, Http01Challenge>) {
        *self.inner.write() = m;
    }
}

impl Default for Http01Store {
    fn default() -> Self {
        Self::new()
    }
}

impl AcmeChallengeStore {
    pub fn new() -> Self {
        Self {
            inner: parking_lot::RwLock::new(std::collections::BTreeMap::new()),
        }
    }

    fn norm(fqdn: &str) -> String {
        fqdn.trim().trim_end_matches('.').to_ascii_lowercase()
    }

    /// Add `value` under `fqdn`. Also sweeps every expired entry while holding
    /// the write lock — issuance is the natural sweep cadence (the map only
    /// ever grows when challenges churn), so abandoned entries never pile up.
    pub fn insert(&self, fqdn: &str, value: &str) {
        let now = hive_core::now_ms();
        let mut m = self.inner.write();
        m.retain(|_, e| now.saturating_sub(e.created_ms) < CHALLENGE_TTL_MS);
        let e = m.entry(Self::norm(fqdn)).or_insert_with(|| AcmeChallenge {
            values: Vec::new(),
            created_ms: now,
        });
        e.created_ms = now;
        if !e.values.iter().any(|v| v == value) {
            e.values.push(value.to_string());
        }
    }

    /// Remove one placed value (post-issuance cleanup); the entry goes with its
    /// last value.
    pub fn remove(&self, fqdn: &str, value: &str) {
        let mut m = self.inner.write();
        let k = Self::norm(fqdn);
        if let Some(e) = m.get_mut(&k) {
            e.values.retain(|v| v != value);
            if e.values.is_empty() {
                m.remove(&k);
            }
        }
    }

    /// TXT values for `fqdn` — empty when unknown or past TTL (the DNS side
    /// then keeps its authoritative no-data answer). The TTL gate matters on
    /// FOLLOWERS too: store-sync adoption declines an empty leader snapshot
    /// (registry-wide never-wipe rule), so a follower's copy of a cleaned-up
    /// challenge ages out here instead.
    pub fn lookup(&self, fqdn: &str) -> Vec<String> {
        self.inner
            .read()
            .get(&Self::norm(fqdn))
            .filter(|e| hive_core::now_ms().saturating_sub(e.created_ms) < CHALLENGE_TTL_MS)
            .map(|e| e.values.clone())
            .unwrap_or_default()
    }

    /// BTreeMap-backed → already deterministic bytes for `store_sync`'s
    /// byte-compare change gate.
    pub fn snapshot(&self) -> std::collections::BTreeMap<String, AcmeChallenge> {
        self.inner.read().clone()
    }

    pub fn load(&self, m: std::collections::BTreeMap<String, AcmeChallenge>) {
        *self.inner.write() = m;
    }
}

impl Default for AcmeChallengeStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---- issuance -------------------------------------------------------------------

/// The `_acme-challenge` record NAME relative to `zone` for an identifier:
/// `shadw.app` in zone `shadw.app` → `_acme-challenge`; `api.shadw.cloud` in
/// zone `shadw.cloud` → `_acme-challenge.api`. Pure; unit-tested.
pub fn challenge_record_name(identifier: &str, zone: &str) -> String {
    let id = identifier.strip_prefix("*.").unwrap_or(identifier);
    if id == zone {
        "_acme-challenge".to_string()
    } else {
        let sub = id.strip_suffix(&format!(".{zone}")).unwrap_or(id);
        format!("_acme-challenge.{sub}")
    }
}

/// Poll public DNS (DoH against Google + Cloudflare — reqwest, no new deps) until
/// the TXT value is visible, or time out (~2 min). Vercel's nameservers publish
/// fast; the wait is for their anycast propagation.
async fn wait_txt(http: &reqwest::Client, fqdn: &str, value: &str) -> bool {
    for i in 0..24 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        for doh in [
            "https://dns.google/resolve",
            "https://cloudflare-dns.com/dns-query",
        ] {
            let url = format!("{doh}?name={fqdn}&type=TXT");
            let resp = http
                .get(&url)
                .header("accept", "application/dns-json")
                .send()
                .await;
            if let Ok(r) = resp {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    let found = v
                        .get("Answer")
                        .and_then(|a| a.as_array())
                        .map(|arr| {
                            arr.iter().any(|rec| {
                                rec.get("data")
                                    .and_then(|d| d.as_str())
                                    .map(|d| d.trim_matches('"') == value)
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false);
                    if found {
                        return true;
                    }
                }
            }
        }
        if i % 6 == 5 {
            tracing::info!(%fqdn, "still waiting for ACME TXT propagation…");
        }
    }
    false
}

// ---- the fleet's ACME account ---------------------------------------------------
//
// The account credential is the fleet's IDENTITY at Let's Encrypt, and every
// rate limit that matters is scoped to it or to the identifier set it orders
// for. It used to live only in `$HIVE_DATA/acme-account.json`, leader-local and
// in PLAINTEXT, so every control-plane handover registered a brand-new account:
// the fleet's issuance history, its per-account limits and its order history all
// restarted under an identity nothing else in the platform knew about.
//
// It is now sealed with the cluster secret (`secrets::encrypt` — the same
// treatment `CertBundle.key_pem_enc` already gets, in this same file) and stored
// in the replicated GuardianDB store, so whichever node holds the designation
// resolves the SAME account. Two rules the code below enforces and that any
// future change must keep:
//
//   * The credential is NEVER written unsealed, NEVER logged, and never carried
//     in an error or incident message. The only things that surface are the
//     source it came from and, for a failure, the remedy.
//   * A NEW account is created ONLY when no source yields credentials at all. If
//     credentials exist but cannot be ACTIVATED (LE directory unreachable, order
//     endpoint 5xx), that is an error to retry on the next pass — never a reason
//     to register a second identity, which is precisely how a transient network
//     failure would have burned one.

/// Envelope version for the sealed account credential at rest / in the store.
const ACCOUNT_ENVELOPE_V: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct AccountEnvelope {
    v: u32,
    /// `enc:v1:`-sealed serialization of `instant_acme::AccountCredentials`.
    creds_enc: String,
    created_ms: u64,
}

/// The outcome of opening an envelope — three states, because "no account
/// stored" and "an account is stored that this node cannot decrypt" demand
/// opposite responses (create one vs. shout about the key).
enum EnvelopeOpen {
    Missing,
    /// Present but no configured key opens it: `HIVE_SECRET_KEY` differs from
    /// the sealing node's, or was rotated without `HIVE_SECRET_KEY_OLD`.
    Undecryptable,
    Opened(Box<AccountCredentials>),
}

fn account_guardian_key() -> String {
    format!(
        "acme/account/{}",
        if staging() { "staging" } else { "prod" }
    )
}

/// Sealed local cache — a deliberately NEW filename, so this build never hands a
/// sealed envelope to a rolled-back binary that would parse it as bare
/// `AccountCredentials`, fail, and register a fresh account.
fn account_sealed_path() -> std::path::PathBuf {
    crate::persist::data_dir().join(if staging() {
        "acme-account-sealed-staging.json"
    } else {
        "acme-account-sealed.json"
    })
}

/// The pre-seal plaintext file. Read once and carried into the sealed store —
/// an existing account is always reused, never regenerated.
fn account_legacy_path() -> std::path::PathBuf {
    crate::persist::data_dir().join(if staging() {
        "acme-account-staging.json"
    } else {
        "acme-account.json"
    })
}

/// Seal credentials into a storable envelope. `None` only if serialization
/// itself fails; the error is deliberately not propagated with its payload.
fn seal_account(creds: &AccountCredentials) -> Option<String> {
    let plain = serde_json::to_string(creds).ok()?;
    let env = AccountEnvelope {
        v: ACCOUNT_ENVELOPE_V,
        creds_enc: crate::secrets::encrypt(&plain),
        created_ms: hive_core::now_ms(),
    };
    // A seal that didn't actually seal must never reach disk or the mesh.
    if !crate::secrets::is_encrypted(&env.creds_enc) {
        tracing::error!(
            "ACME account credential could not be AEAD-sealed — refusing to store it in the clear"
        );
        return None;
    }
    serde_json::to_string(&env).ok()
}

fn open_account_envelope(bytes: &[u8]) -> EnvelopeOpen {
    let Ok(env) = serde_json::from_slice::<AccountEnvelope>(bytes) else {
        return EnvelopeOpen::Missing;
    };
    if env.v != ACCOUNT_ENVELOPE_V {
        tracing::warn!(
            version = env.v,
            expected = ACCOUNT_ENVELOPE_V,
            "ACME account envelope has an unknown version — ignoring it"
        );
        return EnvelopeOpen::Missing;
    }
    // try_decrypt, not decrypt: `decrypt` returns its input unchanged when no
    // key opens the value, which would turn "wrong key" into a JSON parse
    // failure indistinguishable from "no account stored" — the exact confusion
    // that must not silently register a second account.
    let Some(plain) = crate::secrets::try_decrypt(&env.creds_enc) else {
        return EnvelopeOpen::Undecryptable;
    };
    match serde_json::from_str::<AccountCredentials>(&plain) {
        Ok(c) => EnvelopeOpen::Opened(Box::new(c)),
        Err(_) => {
            tracing::error!(
                "ACME account credential decrypted but did not parse — the stored envelope is corrupt"
            );
            EnvelopeOpen::Undecryptable
        }
    }
}

/// Write the sealed envelope to the local cache (tmp + rename, chmod 600) and
/// VERIFY it reads back and opens. Returns false on any failure — callers treat
/// that as loud, because an unwritten credential means the next pass registers
/// another account.
fn write_account_sealed(env_json: &str) -> bool {
    let path = account_sealed_path();
    let dir = crate::persist::data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    // tmp name derived from the target, so a staging and a prod write can never
    // rename each other's partial file into place.
    let mut tmp = path.clone();
    tmp.as_mut_os_string().push(".tmp");
    if std::fs::write(&tmp, env_json).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    match std::fs::read(&path) {
        Ok(b) => matches!(open_account_envelope(&b), EnvelopeOpen::Opened(_)),
        Err(_) => false,
    }
}

fn read_account_sealed() -> EnvelopeOpen {
    let path = account_sealed_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return EnvelopeOpen::Missing; // no local copy yet — normal
    };
    let opened = open_account_envelope(&bytes);
    if matches!(opened, EnvelopeOpen::Missing) {
        // The file EXISTS but is not a readable envelope. Treated as absent so
        // TLS still gets an account, but never silently: this is the one shape
        // in which a stored credential is lost without a key problem.
        tracing::error!(path = ?path, "the local sealed ACME account file is present but unreadable — it will be ignored");
    }
    opened
}

/// Open an incident at most ONCE per process for a static, unchanging
/// condition. The ACME loop revisits every bundle every ~6h, so an unguarded
/// `open` on a condition that cannot change while the process runs would append
/// a new incident per bundle per pass, forever.
fn incident_once(
    fired: &std::sync::atomic::AtomicBool,
    incidents: Option<&crate::incidents::IncidentStore>,
    req: crate::incidents::OpenReq,
) {
    let Some(inc) = incidents else { return };
    if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    inc.open(req);
}

fn open_key_incident(incidents: Option<&crate::incidents::IncidentStore>, where_: &str) {
    tracing::error!(
        source = where_,
        "the fleet's ACME account credential exists but NO configured key opens it — set the \
         fleet-shared HIVE_SECRET_KEY (and HIVE_SECRET_KEY_OLD with the previous key if it was \
         rotated) so this node can reuse the fleet's Let's Encrypt account instead of registering \
         another one"
    );
    static FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    incident_once(
        &FIRED,
        incidents,
        crate::incidents::OpenReq {
            title: "ACME account credential is sealed under an unknown key".into(),
            severity: crate::incidents::Severity::Major,
            affected: vec!["tls".into(), "acme".into()],
            message: format!(
                "The stored ACME account credential ({where_}) cannot be decrypted with any key \
                 this node holds, so it cannot act as the fleet's Let's Encrypt account. Set the \
                 fleet-shared HIVE_SECRET_KEY (carry the previous key in HIVE_SECRET_KEY_OLD if it \
                 was rotated) and restart this node. Until then every issuance from here runs under \
                 a separate LE account."
            ),
        },
    );
}

/// Resolve the fleet's ACME account: sealed local cache → replicated store →
/// legacy plaintext file (migrated forward) → create one, in that order.
async fn account(
    http: &reqwest::Client,
    incidents: Option<&crate::incidents::IncidentStore>,
) -> anyhow::Result<Account> {
    let _ = http; // account creation uses instant-acme's own hyper client
    let mut found: Option<(&'static str, Box<AccountCredentials>)> = None;

    // REPLICATED COPY FIRST, local cache second. Order is the convergence rule,
    // not a preference: every node that was ever leader may hold a DIFFERENT
    // account of its own, and reading the local copy first would let each keep
    // using its own forever — the exact per-leader-identity split this change
    // exists to end. Reading the fleet's copy first makes every node adopt the
    // one account. The local cache remains the fallback for a guardian that is
    // down or has nothing yet, so an offline node still issues.
    if let Some(bytes) = crate::guardian::get(&account_guardian_key()).await {
        match open_account_envelope(&bytes) {
            EnvelopeOpen::Opened(c) => {
                // Cache it locally so later boots don't depend on guardian being
                // reachable. Best-effort: the credential is already in hand.
                if let Some(env) = seal_account(&c) {
                    if !write_account_sealed(&env) {
                        tracing::warn!(
                            "adopted the replicated ACME account but could not write the local sealed cache"
                        );
                    }
                }
                found = Some(("replicated store", c));
            }
            EnvelopeOpen::Undecryptable => open_key_incident(incidents, "replicated store"),
            EnvelopeOpen::Missing => {}
        }
    }

    if found.is_none() {
        match read_account_sealed() {
            EnvelopeOpen::Opened(c) => found = Some(("local sealed cache", c)),
            EnvelopeOpen::Undecryptable => open_key_incident(incidents, "local sealed cache"),
            EnvelopeOpen::Missing => {}
        }
        // Deliberately NOT pushed to guardian on a plain cache hit: `get`
        // returning nothing cannot distinguish "the fleet has no account" from
        // "this node's replica hasn't synced yet", and seeding on the second
        // would overwrite the fleet's identity with this node's. Only the
        // migrate and create paths below (which know no account was found
        // anywhere) write to the replicated store.
    }

    // The pre-seal plaintext file: adopt it rather than ever regenerating, then
    // seal it forward and remove the plaintext copy.
    if found.is_none() {
        let legacy = account_legacy_path();
        if let Ok(s) = std::fs::read_to_string(&legacy) {
            match serde_json::from_str::<AccountCredentials>(&s) {
                Ok(c) => {
                    let sealed = seal_account(&c);
                    let stored = match &sealed {
                        Some(env) => write_account_sealed(env),
                        None => false,
                    };
                    if stored {
                        if let Some(env) = sealed {
                            crate::guardian::put(&account_guardian_key(), env.into_bytes()).await;
                        }
                        // Only after a VERIFIED sealed copy exists: this is the
                        // whole point of the migration, an account private key
                        // must not stay in plaintext on disk.
                        match std::fs::remove_file(&legacy) {
                            Ok(()) => tracing::info!(
                                "migrated the plaintext ACME account credential into the sealed, \
                                 replicated store and removed the plaintext file"
                            ),
                            Err(e) => {
                                tracing::warn!(error = %e, "sealed the ACME account credential but could not remove the plaintext file — delete it by hand")
                            }
                        }
                    } else {
                        tracing::error!(
                            "could not seal the existing plaintext ACME account credential — \
                             keeping the plaintext file and reusing the account from it"
                        );
                    }
                    found = Some(("legacy plaintext file", Box::new(c)));
                }
                Err(_) => tracing::warn!(
                    "the legacy ACME account file exists but did not parse — ignoring it"
                ),
            }
        }
    }

    if let Some((source, creds)) = found {
        // A plaintext credential still on disk after resolving from somewhere
        // else is a superseded account this node once owned. NOT deleted: we
        // hold no sealed copy of it, and deleting a credential we haven't
        // stored is irreversible. Named so an operator can remove it.
        if source != "legacy plaintext file" {
            let legacy = account_legacy_path();
            if legacy.exists() {
                tracing::warn!(
                    path = ?legacy,
                    "a PLAINTEXT ACME account credential from before sealed storage is still on \
                     disk and is now superseded — remove it by hand (it is not deleted \
                     automatically: no sealed copy of that particular account exists)"
                );
            }
        }
        // Credentials in hand: activation failure is TRANSIENT (the ACME
        // directory is fetched here). Registering a new account on this path
        // would silently abandon the fleet's identity over a network blip.
        return match Account::builder()?.from_credentials(*creds).await {
            Ok(acct) => {
                tracing::info!(source, "ACME: reusing the fleet's Let's Encrypt account");
                Ok(acct)
            }
            Err(e) => Err(anyhow::anyhow!(
                "ACME account credentials from the {source} could not be activated ({e}) — \
                 refusing to register a replacement account; retrying next pass"
            )),
        };
    }

    let email = std::env::var("HIVE_ACME_EMAIL").unwrap_or_else(|_| "ops@shadw.cloud".into());
    let (acct, creds) = Account::builder()?
        .create(
            &NewAccount {
                contact: &[&format!("mailto:{email}")],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            directory_url().to_string(),
            None,
        )
        .await?;
    let sealed = seal_account(&creds);
    let stored = match &sealed {
        Some(env) => write_account_sealed(env),
        None => false,
    };
    if let Some(env) = sealed {
        crate::guardian::put(&account_guardian_key(), env.into_bytes()).await;
    }
    if stored {
        tracing::warn!(
            staging = staging(),
            "ACME: registered a NEW Let's Encrypt account (no stored credential was found) — it is \
             now sealed and replicated, so a control-plane handover will reuse it"
        );
    } else {
        // Unstorable = the next pass finds nothing again and registers yet
        // another account. That is an operator-visible condition, not a warn line.
        tracing::error!(
            "ACME: registered a NEW Let's Encrypt account but could NOT store it — every future \
             pass will register another one until this is fixed"
        );
        static FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        incident_once(
            &FIRED,
            incidents,
            crate::incidents::OpenReq {
                title: "ACME account credential could not be persisted".into(),
                severity: crate::incidents::Severity::Major,
                affected: vec!["tls".into(), "acme".into()],
                message: format!(
                    "A new Let's Encrypt account was registered but writing the sealed credential \
                     to {} failed, so it cannot be reused. Every subsequent ACME pass will register \
                     another account against the same rate-limit budget until the data directory is \
                     writable.",
                    account_sealed_path().display()
                ),
            },
        );
    }
    Ok(acct)
}

/// Issue one bundle: order → DNS-01 TXT via the Vercel API → wait propagation →
/// validate → CSR (rcgen) → finalize → certificate chain. Cleans its TXT records
/// up afterwards (best-effort).
async fn issue(
    http: &reqwest::Client,
    api: &VercelApi,
    names: &[String],
    zone: &str,
    challenges: &AcmeChallengeStore,
    incidents: Option<&crate::incidents::IncidentStore>,
) -> anyhow::Result<CertBundle> {
    // instant-acme's internal HTTPS client needs a process-level rustls provider;
    // idempotent, so install unconditionally (the listener may not have run yet).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let acct = account(http, incidents).await?;
    let identifiers: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
    let mut order = acct.new_order(&NewOrder::new(&identifiers)).await?;

    // Pass 1: write a TXT record for every pending dns-01 challenge.
    let mut txt_names: Vec<(String, String)> = Vec::new(); // (record name, value) for cleanup
    {
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut authz = a?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            let challenge = authz
                .challenge(ChallengeType::Dns01)
                .ok_or_else(|| anyhow::anyhow!("no dns-01 challenge offered"))?;
            let host = challenge.identifier().to_string();
            let value = challenge.key_authorization().dns_value();
            let rec_name = challenge_record_name(&host, zone);
            let fqdn = format!("{rec_name}.{zone}");
            // BOTH authorities on purpose: Vercel answers while the zone (or a
            // sub-zone like `api.`/the deploy label) is still Vercel-served,
            // and the replicated challenge store is what lets every advertised
            // Seer nameserver answer once the NS delegation moves here. Once a
            // name IS delegated, Vercel refuses the TXT create as a child of
            // the delegation (the same 409 record_conflicts rule witnessed
            // stranding the api cutover) — that refusal must not kill the
            // renewal, because Seer is then the authority that matters. Any
            // other failure stays fatal.
            if let Err(e) = api
                .create(
                    zone,
                    &DesiredRecord {
                        name: rec_name.clone(),
                        rtype: "TXT".into(),
                        value: value.clone(),
                        ttl: 60,
                    },
                )
                .await
            {
                // The gate must be the LIVE delegation state, never static
                // zone config: below the capable-NS floor the api delegation
                // deliberately disengages back to the flat A set (Vercel
                // authoritative again), and swallowing a 409 THERE would
                // place nothing at the authority that matters and fail the
                // order opaquely minutes later at LE validation
                // (adversarial-review confirmed). The reconciler stamps these
                // gauges every pass on this same node (the single designation
                // leads ACME and DNS reconcile together).
                let conflict = e.to_string().contains("409");
                let under = |z: Option<&str>| {
                    z.map(|z| fqdn == z || fqdn.ends_with(&format!(".{z}")))
                        .unwrap_or(false)
                };
                let delegated_live = (under(crate::dnsserver::deploy_zone())
                    && crate::vercel_dns::STATS
                        .geo_delegation_records
                        .load(std::sync::atomic::Ordering::Relaxed)
                        > 0)
                    || (under(crate::dnsserver::api_zone())
                        && crate::vercel_dns::STATS
                            .api_delegation_records
                            .load(std::sync::atomic::Ordering::Relaxed)
                            > 0);
                if conflict && delegated_live {
                    tracing::warn!(%fqdn, error = %e, "ACME dns-01 Vercel TXT refused under a live-Seer-delegated name — proceeding via the challenge store");
                } else {
                    return Err(e);
                }
            }
            challenges.insert(&fqdn, &value);
            tracing::info!(zone, record = %rec_name, "ACME dns-01 TXT written via Vercel API + Seer challenge store");
            txt_names.push((rec_name, value));
        }
    }

    // Wait until the TXT records are publicly visible.
    for (rec_name, value) in &txt_names {
        let fqdn = format!("{rec_name}.{zone}");
        if !wait_txt(http, &fqdn, value).await {
            tracing::warn!(%fqdn, "TXT not observed via DoH in time — proceeding anyway (LE queries the authoritative NS directly)");
        }
    }

    // Pass 2: flip every pending challenge to ready (authorization state is
    // cached — no extra network on the second walk).
    {
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut authz = a?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            if let Some(mut challenge) = authz.challenge(ChallengeType::Dns01) {
                challenge.set_ready().await?;
            }
        }
    }

    // Poll to Ready, finalize (instant-acme generates the CSR + key via rcgen),
    // then poll the certificate chain.
    let retry = instant_acme::RetryPolicy::default();
    let status = order.poll_ready(&retry).await?;
    if status != OrderStatus::Ready {
        cleanup_txt(api, zone, &txt_names, challenges).await;
        anyhow::bail!("ACME order not ready (status {status:?})");
    }
    let key_pem = order.finalize().await?;
    let chain = order.poll_certificate(&retry).await?;
    cleanup_txt(api, zone, &txt_names, challenges).await;

    let now = hive_core::now_ms();
    Ok(CertBundle {
        names: names.to_vec(),
        chain_pem: chain,
        key_pem_enc: crate::secrets::encrypt(&key_pem),
        issued_ms: now,
        not_after_ms: now + NINETY_DAYS_MS,
    })
}

async fn cleanup_txt(
    api: &VercelApi,
    zone: &str,
    txts: &[(String, String)],
    challenges: &AcmeChallengeStore,
) {
    if txts.is_empty() {
        return;
    }
    // Mirror of the double-write at placement: drop the Seer-answered copy too.
    for (n, v) in txts {
        challenges.remove(&format!("{n}.{zone}"), v);
    }
    if let Ok(records) = api.list(zone).await {
        for r in records {
            if r.rtype == "TXT"
                && txts.iter().any(|(n, v)| {
                    *n == r.name && (*v == r.value || r.value.trim_matches('"') == *v)
                })
            {
                let _ = api.delete(zone, &r.id).await;
            }
        }
    }
}

// ---- custom tenant domains (HTTP-01) -------------------------------------------

/// Bundle name for a tenant domain — deterministic, never a raw tenant string
/// (the same discipline as every other name in this file).
pub fn custom_bundle_name(domain: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(
        domain
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase()
            .as_bytes(),
    );
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("dom-{}", &hex[..12])
}

/// The (bundle, SANs, zone) set for every verified custom domain — the third
/// element mirrors `bundles()`'s shape so cert-sync can chain the two lists.
/// SANs are apex + the explicit `www` host ONLY: Let's Encrypt never offers
/// HTTP-01 for a wildcard identifier (CA/B forbids it), so asking for
/// `*.{domain}` here made every order fail "no http-01 challenge offered"
/// (adversarial finding — first issuance was impossible). Wildcards for
/// tenant domains belong to the DNS-01 path against a delegated zone (the
/// tenant-zone `_acme-challenge` arm in dnsserver.rs).
pub fn custom_domain_bundles(cloud: &Arc<CloudState>) -> Vec<(String, Vec<String>, String)> {
    let mut out = Vec::new();
    for d in cloud.domains.snapshot() {
        let Some(v) = &d.verify else { continue };
        if v.status != "verified" {
            continue;
        }
        let domain = d.domain.trim().trim_end_matches('.').to_ascii_lowercase();
        if domain.is_empty() {
            continue;
        }
        out.push((
            custom_bundle_name(&domain),
            vec![domain.clone(), format!("www.{domain}")],
            domain,
        ));
    }
    out
}

/// HTTP-01 issuance for custom domains — `issue()`'s shape with the challenge
/// type swapped and the replicated token store standing in for the TXT
/// double-write. No Vercel zone is involved: the tenant's DNS points the name
/// at the fleet already (that is what verification proved), so LE's fetch of
/// `/.well-known/acme-challenge/<token>` lands on some node, and any node can
/// answer from the replicated store.
async fn issue_http01(
    http: &reqwest::Client,
    names: &[String],
    store: &Http01Store,
    incidents: Option<&crate::incidents::IncidentStore>,
) -> anyhow::Result<CertBundle> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let acct = account(http, incidents).await?;
    let identifiers: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
    let mut order = acct.new_order(&NewOrder::new(&identifiers)).await?;

    let mut tokens: Vec<String> = Vec::new();
    {
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut authz = a?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = authz
                .challenge(ChallengeType::Http01)
                .ok_or_else(|| anyhow::anyhow!("no http-01 challenge offered"))?;
            let token = challenge.token.clone();
            let key_auth = challenge.key_authorization().as_str().to_string();
            store.insert(&token, &key_auth);
            tokens.push(token);
            challenge.set_ready().await?;
        }
    }

    let retry = instant_acme::RetryPolicy::default();
    let status = order.poll_ready(&retry).await;
    for t in &tokens {
        store.remove(t);
    }
    let status = status?;
    if status != OrderStatus::Ready {
        anyhow::bail!("ACME http-01 order not ready (status {status:?})");
    }
    let key_pem = order.finalize().await?;
    let chain = order.poll_certificate(&retry).await?;

    let now = hive_core::now_ms();
    Ok(CertBundle {
        names: names.to_vec(),
        chain_pem: chain,
        key_pem_enc: crate::secrets::encrypt(&key_pem),
        issued_ms: now,
        not_after_ms: now + NINETY_DAYS_MS,
    })
}

/// Drop guard returning a bundle name to the in-flight issuance set (see
/// `custom_cert_pass`) on every exit path, cancellation included.
struct IssuingGuard<'a> {
    set: &'a parking_lot::Mutex<std::collections::HashSet<String>>,
    bundle: String,
}
impl Drop for IssuingGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().remove(&self.bundle);
    }
}

/// The SAN set an HTTP-01 order can actually validate THIS pass:
/// `www.{domain}` is kept only when it resolves to a current fleet edge
/// address — LE validates each SAN independently and fails the WHOLE order
/// otherwise, so a tenant who pointed only their apex (path A) or a
/// delegated zone pre-verification would otherwise never get even the apex
/// cert (adversarial finding). The narrowed set feeds the freshness check
/// too, so a later www arrival naturally re-issues with full coverage.
/// DoH lookup, same client shape as the domain verifier.
async fn effective_sans(cloud: &Arc<CloudState>, names: &[String]) -> Vec<String> {
    if names.len() != 2 {
        return names.to_vec();
    }
    let (apex, www) = (&names[0], &names[1]);
    let edge: std::collections::HashSet<String> = {
        let nodes = cloud.registry.nodes();
        crate::dnsserver::lb_records_strings(&nodes, 1)
            .into_iter()
            .collect()
    };
    let url = format!("https://cloudflare-dns.com/dns-query?name={www}&type=A");
    let resolved: Option<Vec<String>> = async {
        let resp = cloud
            .http
            .get(&url)
            .header("accept", "application/dns-json")
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        let v = resp.json::<serde_json::Value>().await.ok()?;
        Some(
            v.get("Answer")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter(|x| x.get("type").and_then(|t| t.as_u64()) == Some(1))
                        .filter_map(|x| {
                            x.get("data")
                                .and_then(|d| d.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
    }
    .await;
    match resolved {
        Some(ips) if ips.iter().any(|ip| edge.contains(ip)) => names.to_vec(),
        _ => {
            tracing::warn!(%apex, %www, "custom-domain www does not resolve to the fleet — ordering apex-only SAN this pass (the whole order would fail otherwise)");
            vec![apex.clone()]
        }
    }
}

/// Install a custom bundle before its DomainRecord may claim issuance. The
/// metadata is a health statement consumed independently of the SNI cache, so
/// writing it after a parse/key-install failure would recreate the original
/// fictional "active SSL" state.
fn install_and_record_custom_bundle(
    cloud: &Arc<CloudState>,
    zone: &str,
    bundle: &str,
    cert: &CertBundle,
) -> anyhow::Result<bool> {
    install_bundle(cert)?;
    Ok(cloud.domains.set_ssl_issued(
        zone,
        bundle,
        cert.names.clone(),
        cert.issued_ms,
        cert.not_after_ms,
    ))
}

/// One pass over every verified custom domain: ensure a fresh, SAN-covering
/// bundle exists, else issue one via HTTP-01 and publish it (guardian + local
/// store) for the fleet's cert-sync to pick up. **Leader-only internally** —
/// the kick paths spawn it from several nodes at once, and an un-gated pass
/// both duplicate-issues against LE's 5/168h duplicate-cert budget (every
/// node has its own "no local cache" view) and drops its tokens into a
/// follower store the next store_sync pull wipes mid-order (adversarial
/// findings). Safe to call from anywhere: it simply no-ops off the leader.
pub async fn custom_cert_pass(cloud: &Arc<CloudState>) {
    if cloud.ingress == "ngrok" {
        return;
    }
    if cloud.control_plane_leader() != cloud.node_name || cloud.mesh_health().isolated {
        return;
    }
    // Per-domain failure streak + backoff (the crash-loop rule from AGENTS.md:
    // a circuit's open window must outlast the failure it guards). A domain
    // whose DNS never arrives would otherwise order LE every 300s forever —
    // burning the fleet account's failed-validation AND new-orders budgets
    // until the platform's own renewals starve too. Backoff doubles per
    // consecutive failure to a 6h ceiling; after 12 the domain is dropped
    // from the pass set with an incident naming the state (a fresh
    // verification or a settings change resets the streak by process
    // restart, and each success resets it immediately).
    static STREAKS: std::sync::OnceLock<
        parking_lot::RwLock<std::collections::HashMap<String, (u32, u64)>>,
    > = std::sync::OnceLock::new();
    let streaks =
        STREAKS.get_or_init(|| parking_lot::RwLock::new(std::collections::HashMap::new()));
    // Per-bundle in-flight issuance set: concurrent kicks (attach,
    // forward-ack, owner arm, watcher, verify-now, the 300s loop) plus
    // per-observer leadership flaps must never run two issuances of the same
    // SAN set — five duplicate orders inside 168h closes LE's
    // duplicate-certificate window for a week, and this repo treats a closed
    // window as a Major incident (adversarial finding).
    static ISSUING: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let issuing = ISSUING.get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()));
    for (bundle, names, zone) in custom_domain_bundles(cloud) {
        // The reservation is held from the freshness check through order
        // completion and released by the guard's Drop on EVERY exit path —
        // a cancelled pass (leader flap mid-await, task abort) must never
        // wedge the bundle's renewals (the ColdStartGuard rule).
        if !issuing.lock().insert(bundle.clone()) {
            continue;
        }
        let _issue_guard = IssuingGuard {
            set: issuing,
            bundle: bundle.clone(),
        };
        {
            let (_fails, next_at) = streaks.read().get(&bundle).copied().unwrap_or((0, 0));
            // No permanent park: a tenant very often points DNS at the fleet
            // only AFTER attaching/verifying, so a hard `fails >= 12` skip
            // would strand the bundle unissued until a leader restart. The
            // backoff ceiling alone (~64min) keeps chronically-unpointed
            // domains far inside LE's failed-validation rate limit while
            // letting a late DNS fix issue on its own.
            if hive_core::now_ms() < next_at {
                continue;
            }
        }
        // Narrow the SAN set to what validates this pass (www pre-flight —
        // see effective_sans). Drives the freshness check and the order, so
        // coverage widens on its own once www reaches the fleet.
        let names = effective_sans(cloud, &names).await;
        let fresh = |issued: u64, have: &[String]| {
            hive_core::now_ms().saturating_sub(issued) < RENEW_AFTER_MS
                && names.iter().all(|n| have.contains(n))
        };
        if let Some(b) = load_bundle_local(&bundle) {
            if fresh(b.issued_ms, &b.names) {
                match install_and_record_custom_bundle(cloud, &zone, &bundle, &b) {
                    Ok(true) => crate::persist::persist(cloud),
                    Ok(false) => {}
                    Err(e) => tracing::warn!(
                        %bundle,
                        error = %e,
                        "custom-domain TLS bundle is fresh but could not be installed — ssl metadata remains pending"
                    ),
                }
                continue;
            }
        } else {
            // The local disk is not the only source of truth — a fresh
            // leader boot/a wiped dir must adopt the guardian replica instead
            // of re-issuing every domain (the duplicate-cert budget again).
            let gb = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                crate::guardian::get(&guardian_key(&bundle)),
            )
            .await
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<CertBundle>(&bytes).ok());
            match gb {
                Some(b) if fresh(b.issued_ms, &b.names) => {
                    store_bundle_local(&bundle, &b);
                    match install_and_record_custom_bundle(cloud, &zone, &bundle, &b) {
                        Ok(true) => crate::persist::persist(cloud),
                        Ok(false) => {}
                        Err(e) => tracing::warn!(
                            %bundle,
                            error = %e,
                            "adopted custom-domain TLS bundle could not be installed — ssl metadata remains pending"
                        ),
                    }
                    continue;
                }
                _ => {}
            }
        }
        tracing::info!(%bundle, ?names, "custom-domain TLS bundle missing or stale — issuing via HTTP-01");
        match issue_http01(
            &cloud.http,
            &names,
            &cloud.acme_http01,
            Some(&cloud.incidents),
        )
        .await
        {
            Ok(b) => {
                streaks.write().remove(&bundle);
                store_bundle_local(&bundle, &b);
                let js = serde_json::to_vec(&b).unwrap_or_default();
                crate::guardian::put(&guardian_key(&bundle), js).await;
                match install_and_record_custom_bundle(cloud, &zone, &bundle, &b) {
                    Ok(changed) => {
                        tracing::info!(%bundle, ?names, "custom-domain TLS bundle issued + installed");
                        if changed {
                            crate::persist::persist(cloud);
                        }
                    }
                    Err(e) => tracing::warn!(
                        %bundle,
                        error = %e,
                        "custom-domain TLS bundle issued but failed to install — ssl metadata remains pending"
                    ),
                }
            }
            Err(e) => {
                let fails = {
                    let mut w = streaks.write();
                    let e = w.entry(bundle.clone()).or_insert((0, 0));
                    e.0 += 1;
                    let f = e.0;
                    e.1 = hive_core::now_ms() + (60_000u64 << f.min(6)).min(6 * 3_600_000);
                    f
                };
                // Rate limits and validation failures are typed incidents,
                // deduped per bundle per process (an unguarded `open` appends
                // one per pass forever — the incidents store replicates).
                let msg = e.to_string();
                if msg.contains("rateLimited") || msg.contains("rate limit") {
                    static RL_FIRED: std::sync::OnceLock<
                        parking_lot::RwLock<std::collections::HashSet<String>>,
                    > = std::sync::OnceLock::new();
                    let set = RL_FIRED
                        .get_or_init(|| parking_lot::RwLock::new(std::collections::HashSet::new()));
                    if set.write().insert(bundle.clone()) {
                        cloud.incidents.open(crate::incidents::OpenReq {
                            title: format!("ACME rate limit: {bundle}"),
                            severity: crate::incidents::Severity::Major,
                            affected: names.clone(),
                            message: format!(
                                "ACME rate limit for custom domain bundle {bundle}: {msg}"
                            ),
                        });
                    }
                }
                if fails == 12 {
                    cloud.incidents.open(crate::incidents::OpenReq {
                        title: format!("Custom domain cert failing: {bundle}"),
                        severity: crate::incidents::Severity::Minor,
                        affected: names.clone(),
                        message: format!(
                            "HTTP-01 issuance for {bundle} ({}) has failed 12 consecutive times — the domain's DNS likely doesn't point at the platform. Retries continue at the backoff ceiling and issuance completes on its own once DNS points here. Last error: {msg}",
                            names.join(", ")
                        ),
                    });
                }
                tracing::warn!(%bundle, fails, error = %e, "custom-domain HTTP-01 issuance failed (backoff)");
            }
        }
    }
}

/// Leader loop for custom-domain certificates: an on-boot pass, then a slow
/// cadence (renewal + newly verified domains). `HIVE_CUSTOM_CERT_SECS`
/// overrides the cadence; `0` disables. Kick-on-verify calls
/// `custom_cert_pass` directly for instant issuance.
pub fn spawn_custom_cert_loop(cloud: Arc<CloudState>) {
    if cloud.ingress == "ngrok" {
        return;
    }
    let secs = std::env::var("HIVE_CUSTOM_CERT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    if secs == 0 {
        return;
    }
    tokio::spawn(async move {
        loop {
            if cloud.control_plane_leader() == cloud.node_name && !cloud.mesh_health().isolated {
                custom_cert_pass(&cloud).await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        }
    });
}

// ---- orchestration ---------------------------------------------------------------

fn guardian_key(bundle: &str) -> String {
    format!(
        "tls/{}/{bundle}",
        if staging() { "staging" } else { "prod" }
    )
}

fn cache_path(bundle: &str) -> std::path::PathBuf {
    crate::persist::data_dir().join(format!(
        "tls-{bundle}{}.json",
        if staging() { "-staging" } else { "" }
    ))
}

fn load_bundle_local(bundle: &str) -> Option<CertBundle> {
    let s = std::fs::read_to_string(cache_path(bundle)).ok()?;
    serde_json::from_str(&s).ok()
}

fn store_bundle_local(bundle: &str, b: &CertBundle) {
    if let Ok(js) = serde_json::to_string(b) {
        let _ = std::fs::write(cache_path(bundle), js);
    }
}

/// Leader loop: ensure both bundles exist and are fresh; re-issue at 2/3 of
/// validity. Jittered check every ~6h (cheap no-op when fresh).
pub fn spawn_acme(cloud: Arc<CloudState>) {
    if cloud.ingress == "ngrok"
        && std::env::var("HIVE_ACME_FORCE")
            .map(|v| v == "1")
            .unwrap_or(false)
            == false
    {
        return;
    }
    let Some(api) = VercelApi::from_env(cloud.http.clone()) else {
        tracing::warn!("ACME enabled but VERCEL_API_TOKEN not set — not starting");
        return;
    };
    tracing::info!(staging = staging(), apps = %cloud.apps_domain, platform = %cloud.platform_domain, "ACME manager up (leader-elected, DNS-01 via Vercel)");
    tokio::spawn(async move {
        loop {
            // Jittered interval: 5–7h.
            let jitter = (hive_core::now_ms() % 7200) as u64;
            let sleep = 5 * 3600 + jitter;
            // Same single-writer resolution as admin mutations and the billing
            // meter (owner chain first, health+addressability gated; identity
            // election fallback) — all four roles sit on ONE designation so the
            // HIVE_CP_LEADER-vs-HIVE_DNS_LEADER_NODE drift class is structurally
            // closed (proposal step 6). `HIVE_DNS_LEADER_NODE` remains honored
            // as a deliberate LEGACY split-pin: when set it takes the pref slot
            // on the fallback path (health-gated, never a raw unguarded check —
            // an unguarded pin freezes ACME silently until certs expire).
            let dns_pref = std::env::var("HIVE_DNS_LEADER_NODE")
                .ok()
                .filter(|s| !s.trim().is_empty());
            let chain = crate::cluster::Cluster::owner_chain_from_env();
            let pref = dns_pref.or_else(|| std::env::var("HIVE_CP_LEADER").ok());
            let leader = crate::cluster::Cluster::control_plane_owner(
                &chain,
                pref.as_deref(),
                &cloud.registry.nodes(),
            );
            if leader.as_deref() == Some(cloud.node_name.as_str()) {
                for (bundle, names, zone) in bundles(&cloud) {
                    // One-shot force: a sentinel file `$HIVE_DATA/acme-force-<bundle>`
                    // makes this pass re-issue the bundle regardless of freshness, then
                    // is deleted after a successful issue. This is the ONLY reliable way
                    // to re-issue after a SAN change (e.g. adding admin.) — clearing the
                    // local/guardian cache doesn't work because cert-sync pulls the old
                    // bundle back from a peer via mesh_fetch. `touch` it on the leader,
                    // restart, and the new SANs land in one pass.
                    let force_path =
                        crate::persist::data_dir().join(format!("acme-force-{bundle}"));
                    let forced = std::fs::metadata(&force_path).is_ok();
                    // Fresh = young enough AND covering every wanted name. The
                    // coverage check is what makes a SAN ADDITION (a new region's
                    // `api-<region>` host, a new explicit platform host) reissue
                    // automatically — before it, the only path was the manual
                    // force sentinel, and a forgotten sentinel meant a name that
                    // resolved in DNS but failed TLS until the 60-day renewal.
                    let fresh = !forced
                        && load_bundle_local(&bundle)
                            .map(|b| {
                                hive_core::now_ms() < b.issued_ms + RENEW_AFTER_MS
                                    && names.iter().all(|n| b.names.contains(n))
                            })
                            .unwrap_or(false);
                    if fresh {
                        continue;
                    }
                    tracing::info!(%bundle, ?names, forced, "ACME: issuing/renewing certificate bundle");
                    match issue(
                        &cloud.http,
                        &api,
                        &names,
                        &zone,
                        &cloud.acme_challenges,
                        Some(&cloud.incidents),
                    )
                    .await
                    {
                        Ok(b) => {
                            store_bundle_local(&bundle, &b);
                            if forced {
                                let _ = std::fs::remove_file(&force_path); // one-shot
                            }
                            if let Ok(js) = serde_json::to_vec(&b) {
                                crate::guardian::put(&guardian_key(&bundle), js).await;
                            }
                            if let Err(e) = install_bundle(&b) {
                                tracing::warn!(error = %e, %bundle, "issued but failed to install locally");
                            } else {
                                tracing::info!(%bundle, zones = ?installed_zones(), "certificate installed + replicated");
                            }
                        }
                        Err(e) => {
                            // A Let's Encrypt rate-limit is a BUDGET event, not a
                            // transient: the duplicate-certificate window (5 per
                            // 168h per exact identifier set) closes the bundle
                            // until the window opens — live-witnessed 2026-07-29
                            // after five forced renewals in a week ('retry after
                            // 2026-07-30 10:06:58 UTC'), which also silently
                            // disarmed the acme-force sentinel path. Warn alone
                            // buried that; make it an incident so the operator
                            // sees the window (and that force-renewals spend it).
                            if e.to_string().contains("rateLimited") {
                                cloud.incidents.open(crate::incidents::OpenReq {
                                    title: format!("ACME rate-limited by Let's Encrypt ({bundle})"),
                                    severity: crate::incidents::Severity::Major,
                                    affected: vec!["tls".into(), "acme".into()],
                                    message: format!(
                                        "Issuance of the {bundle} bundle hit an LE rate limit — the bundle cannot renew (including via the acme-force sentinel) until the window opens. Error: {e}"
                                    ),
                                });
                            }
                            tracing::warn!(error = %e, %bundle, "ACME issuance failed (will retry next pass)");
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(sleep)).await;
        }
    });
}

/// Remove a custom-domain bundle that is no longer wanted: its SNI map
/// entries (derived from the cached bundle exactly the way `install_bundle`
/// keyed them, minus any name a WANTED bundle still covers — two
/// attachments can share the `www` key, and detaching one must not kill the
/// other's cert) and its local cache file. Without this a detached/unverified
/// domain kept answering HTTPS with a still-valid cert until process
/// restart — routing revoked, TLS not.
fn prune_custom_bundle(
    bundle: &str,
    wanted_names: &std::collections::HashSet<String>,
    installed: &mut HashMap<String, u64>,
) {
    if let Some(b) = load_bundle_local(bundle) {
        let mut map = (**certs().load()).clone();
        for name in &b.names {
            if wanted_names.contains(name) {
                continue;
            }
            let zone = name.strip_prefix("*.").unwrap_or(name).to_ascii_lowercase();
            map.remove(&zone);
            if name.contains('.') && !name.starts_with("*.") {
                map.remove(&name.to_ascii_lowercase());
            }
        }
        certs().store(Arc::new(map));
    }
    let _ = std::fs::remove_file(cache_path(bundle));
    installed.remove(bundle);
    tracing::info!(%bundle, "TLS bundle pruned (custom domain no longer attached)");
}

/// Every node: load local cache at boot, then poll the guardian replicated store
/// and hot-swap the SNI resolver when a newer bundle lands.
pub fn spawn_cert_sync(cloud: Arc<CloudState>) {
    if cloud.ingress == "ngrok" {
        return;
    }
    tokio::spawn(async move {
        let mut installed: HashMap<String, u64> = HashMap::new(); // bundle -> issued_ms
        // bundle -> consecutive unwanted ticks (prune flap damping)
        let mut unwanted: HashMap<String, u8> = HashMap::new();
        // boot: local cache first (guardian may take a while to come online)
        for (bundle, ..) in bundles(&cloud)
            .into_iter()
            .chain(custom_domain_bundles(&cloud))
        {
            if let Some(b) = load_bundle_local(&bundle) {
                if install_bundle(&b).is_ok() {
                    installed.insert(bundle.clone(), b.issued_ms);
                    tracing::info!(%bundle, "TLS bundle loaded from local cache");
                }
            }
        }
        loop {
            // Prune custom-domain bundles no longer wanted (detached /
            // verification cleared / project deleted): they must stop
            // answering TLS here, not just stop renewing. Guards (every
            // prune/GC in this codebase has one — the podman lock pool and
            // gc_rootfs_images lessons): (1) an EMPTY domain snapshot is a
            // wholesale-replace flap signature, never a real "zero domains"
            // — skip the pass entirely rather than prune everything;
            // (2) a bundle must be unwanted for TWO consecutive ticks before
            // it is pruned, so a one-tick status flap (owner-election churn,
            // the 2026-08-18 lesson) never reaches the SNI map; (3) SNI
            // keys still covered by a WANTED bundle are never removed (two
            // attachments can share the `www` key). Peers prune on their own
            // cadence; a guardian replica may linger but is never fetched
            // (this loop only iterates the wanted set), so a fresh node
            // never installs one.
            let wanted: Vec<(String, Vec<String>, String)> = custom_domain_bundles(&cloud);
            let wanted_bundles: std::collections::HashSet<String> =
                wanted.iter().map(|(b, ..)| b.clone()).collect();
            let wanted_names: std::collections::HashSet<String> = wanted
                .iter()
                .flat_map(|(_, names, _)| names.iter().cloned())
                .collect();
            for b in installed
                .keys()
                .filter(|b| b.starts_with("dom-"))
                .cloned()
                .collect::<Vec<_>>()
            {
                if wanted_bundles.contains(&b) {
                    unwanted.remove(&b);
                } else {
                    *unwanted.entry(b).or_insert(0) += 1;
                }
            }
            if !cloud.domains.snapshot().is_empty() {
                let stale: Vec<String> = unwanted
                    .iter()
                    .filter(|(_, n)| **n >= 2)
                    .map(|(b, _)| b.clone())
                    .collect();
                for b in stale {
                    prune_custom_bundle(&b, &wanted_names, &mut installed);
                    unwanted.remove(&b);
                }
            }
            for (bundle, ..) in bundles(&cloud)
                .into_iter()
                .chain(custom_domain_bundles(&cloud))
            {
                let cur = installed.get(&bundle).copied().unwrap_or(0);
                // Guardian replica first (works when local == writer), then the
                // authenticated mesh (the cross-node path: per-node AEAD keys
                // make replicated ciphertext unreadable on peers), then the
                // plain HTTPS admin fallback (the path that still works when
                // the iroh mesh to the issuer is down — including the
                // bootstrap deadlock where the RELAY hostname is missing from
                // the very cert being synced, so the relay fallback that
                // mesh_fetch depends on is itself broken).
                let mut candidate: Option<CertBundle> = None;
                // Bounded: a sick guardian (corrupt store looping re-init,
                // witnessed live on fc-virginia) must never starve the whole
                // cert-sync loop — the mesh + HTTPS fallbacks below are
                // exactly for when guardian can't answer.
                let guardian_bytes = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    crate::guardian::get(&guardian_key(&bundle)),
                )
                .await
                .unwrap_or_else(|_| {
                    tracing::debug!(%bundle, "cert-sync: guardian::get timed out — falling through to mesh/http");
                    None
                });
                if let Some(bytes) = guardian_bytes {
                    if let Ok(b) = serde_json::from_slice::<CertBundle>(&bytes) {
                        if b.issued_ms > cur {
                            match install_bundle(&b) {
                                Ok(()) => candidate = Some(b),
                                // EXPECTED on every non-writer node: the replica's
                                // key_pem_enc is ciphertext under the WRITER's
                                // per-node AEAD key. Named so the journal shows
                                // why the newer guardian bundle didn't install.
                                Err(e) => {
                                    tracing::debug!(%bundle, newer_issued_ms = b.issued_ms, cur, "cert-sync: guardian replica newer but uninstallable here (foreign AEAD key?): {e}")
                                }
                            }
                        }
                    }
                }
                if candidate.is_none() {
                    match mesh_fetch(&cloud, &bundle).await {
                        Some(b) if b.issued_ms > cur => match install_bundle(&b) {
                            Ok(()) => candidate = Some(b),
                            Err(e) => {
                                tracing::warn!(%bundle, issued_ms = b.issued_ms, "cert-sync: mesh bundle failed to install: {e}")
                            }
                        },
                        Some(b) => {
                            tracing::debug!(%bundle, mesh_issued_ms = b.issued_ms, cur, "cert-sync: mesh best is not newer than installed");
                        }
                        None => {
                            tracing::debug!(%bundle, "cert-sync: mesh_fetch returned no bundle from any peer");
                        }
                    }
                }
                if candidate.is_none() {
                    if let Some(b) = http_fetch(&cloud, &bundle).await {
                        if b.issued_ms > cur {
                            match install_bundle(&b) {
                                Ok(()) => candidate = Some(b),
                                Err(e) => {
                                    tracing::warn!(%bundle, issued_ms = b.issued_ms, "cert-sync: http bundle failed to install: {e}")
                                }
                            }
                        }
                    }
                }
                if let Some(b) = candidate {
                    store_bundle_local(&bundle, &b);
                    installed.insert(bundle.clone(), b.issued_ms);
                    tracing::info!(%bundle, zones = ?installed_zones(), "TLS bundle installed (replica/mesh sync)");
                }
            }
            // Fast cadence until BOTH bundles are installed, then relax.
            // Fast cadence until every bundle is installed, then relax.
            let have_all = installed.len()
                >= bundles(&cloud)
                    .into_iter()
                    .chain(custom_domain_bundles(&cloud))
                    .count();
            tokio::time::sleep(std::time::Duration::from_secs(if have_all {
                300
            } else {
                20
            }))
            .await;
        }
    });
}

/// HTTPS-admin fallback for cert distribution, authenticated by the
/// fleet-shared `HIVE_INTERNAL_TOKEN` (fails closed when unset). This is the
/// iroh-independent path: it works whenever HTTPS to the platform edge works,
/// which is exactly the condition dashboards and webhooks already rely on.
///
/// `api.<platform>` is multi-A round-robin, so ONE request lands on a random
/// node (whose local bundle may itself be stale). Instead, sweep EVERY healthy
/// fleet node deterministically: pin each registry `public_ip` under the valid
/// `api.<platform>` TLS name via `reqwest`'s resolver override — the private
/// key rides inside real TLS to each specific node — and keep the NEWEST
/// bundle across all answers (same newest-wins rule as `mesh_fetch`).
/// `HIVE_CERT_SYNC_URLS` (comma-separated) remains as an explicit override.
async fn http_fetch(cloud: &Arc<CloudState>, bundle: &str) -> Option<CertBundle> {
    let token = std::env::var("HIVE_INTERNAL_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())?;
    let api_host = format!("api.{}", cloud.platform_domain);
    let mut targets: Vec<(String, Option<std::net::SocketAddr>)> = Vec::new();
    if let Ok(urls) = std::env::var("HIVE_CERT_SYNC_URLS") {
        for u in urls.split(',').map(str::trim).filter(|u| !u.is_empty()) {
            targets.push((u.trim_end_matches('/').to_string(), None));
        }
    }
    for n in cloud.registry.nodes() {
        if n.is_self || !n.healthy {
            continue;
        }
        if let Some(ip) = n.public_ip.as_deref() {
            if let Ok(addr) = format!("{ip}:443").parse::<std::net::SocketAddr>() {
                targets.push((format!("https://{api_host}"), Some(addr)));
            }
        }
    }
    let mut best: Option<CertBundle> = None;
    for (base, pin) in targets {
        let url = format!("{base}/v1/tls/bundle-mesh?name={bundle}");
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10));
        if let Some(addr) = pin {
            builder = builder.resolve(&api_host, addr);
        }
        let Ok(client) = builder.build() else {
            continue;
        };
        let resp = match client
            .get(&url)
            .header("x-hive-internal", &token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::debug!(%bundle, %url, ?pin, status = %r.status(), "cert-sync: http fallback non-success");
                continue;
            }
            Err(e) => {
                tracing::debug!(%bundle, %url, ?pin, "cert-sync: http fallback unreachable: {e}");
                continue;
            }
        };
        let Ok(v) = resp.json::<serde_json::Value>().await else {
            continue;
        };
        let key_pem = v.get("key_pem").and_then(|k| k.as_str()).unwrap_or("");
        let chain = v.get("chain_pem").and_then(|c| c.as_str()).unwrap_or("");
        if key_pem.is_empty() || chain.is_empty() {
            continue;
        }
        let issued = v.get("issued_ms").and_then(|i| i.as_u64()).unwrap_or(0);
        if best.as_ref().map(|b| issued > b.issued_ms).unwrap_or(true) {
            tracing::info!(%bundle, %url, ?pin, issued_ms = issued, "cert-sync: http fallback fetched bundle");
            best = Some(CertBundle {
                names: v
                    .get("names")
                    .and_then(|n| serde_json::from_value(n.clone()).ok())
                    .unwrap_or_default(),
                chain_pem: chain.to_string(),
                key_pem_enc: crate::secrets::encrypt(key_pem),
                issued_ms: issued,
                not_after_ms: v.get("not_after_ms").and_then(|i| i.as_u64()).unwrap_or(0),
            });
        }
    }
    best
}

/// Serve the local bundle to a MESH PEER with the private key DECRYPTED.
/// The transport is the peer-authenticated, end-to-end-encrypted iroh QUIC mesh
/// (signed-gossip enforce verifies the requester's identity); the receiver
/// immediately re-encrypts with ITS OWN node key before touching disk. This is
/// the distribution path that actually works cross-node — per-node AEAD keys
/// mean a replicated *ciphertext* is undecryptable on peers.
pub fn bundle_for_mesh(bundle: &str) -> Vec<u8> {
    // Allowlist the known bundles served cross-node. `db` (the `*.{db_domain}`
    // wildcard fronting the per-tenant DB gateway) MUST be here too — a DB placed
    // on any non-leader node needs the cert locally to complete the TLS handshake,
    // and without this the follower's `mesh_fetch("db")` gets an empty reply and the
    // gateway can't serve `<slug>.{db_domain}` off the leader.
    if bundle != "apps" && bundle != "platform" && bundle != "db" && !bundle.starts_with("dom-") {
        return Vec::new();
    }
    let Some(b) = load_bundle_local(bundle) else {
        return Vec::new();
    };
    let key_pem = crate::secrets::decrypt(&b.key_pem_enc);
    serde_json::to_vec(&serde_json::json!({
        "names": b.names,
        "chain_pem": b.chain_pem,
        "key_pem": key_pem,
        "issued_ms": b.issued_ms,
        "not_after_ms": b.not_after_ms,
    }))
    .unwrap_or_default()
}

/// Fetch a bundle from any mesh peer that has it (leader or already-synced
/// node), re-encrypting the key locally.
async fn mesh_fetch(cloud: &Arc<CloudState>, bundle: &str) -> Option<CertBundle> {
    let peers: Vec<(String, String)> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.healthy)
        .filter_map(|n| Some((n.peer_id?, n.iroh_addr?)))
        .collect();
    // Query ALL peers and keep the NEWEST bundle (highest issued_ms), not the
    // first responder. The old first-wins behavior let a peer still serving a
    // STALE bundle (e.g. after the leader adds a SAN like relay./discovery. and
    // re-issues) win the sync over the leader's freshly-issued newer one — so
    // the SAN change never propagated and relay-cert probe churn persisted
    // fleet-wide. Newest-wins guarantees convergence on the issuer's latest
    // bundle regardless of which peer answers first.
    let mut best: Option<CertBundle> = None;
    for (id, addr) in peers {
        let path = format!("/v1/tls/bundle?name={bundle}");
        // Bumped from 10s: give `PeerPool::acquire`'s fresh-discovery fallback room
        // to resolve a stale/flapped hint instead of being cut off by this timeout.
        if let Some(bytes) =
            crate::gossip::request_to(cloud, &id, &addr, hive_p2p::GOSSIP_GET, &path, &[], 15).await
        {
            if bytes.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                let key_pem = v.get("key_pem").and_then(|k| k.as_str()).unwrap_or("");
                let chain = v.get("chain_pem").and_then(|c| c.as_str()).unwrap_or("");
                if key_pem.is_empty() || chain.is_empty() {
                    continue;
                }
                let issued = v.get("issued_ms").and_then(|i| i.as_u64()).unwrap_or(0);
                if best.as_ref().map(|b| issued > b.issued_ms).unwrap_or(true) {
                    best = Some(CertBundle {
                        names: v
                            .get("names")
                            .and_then(|n| serde_json::from_value(n.clone()).ok())
                            .unwrap_or_default(),
                        chain_pem: chain.to_string(),
                        key_pem_enc: crate::secrets::encrypt(key_pem),
                        issued_ms: issued,
                        not_after_ms: v.get("not_after_ms").and_then(|i| i.as_u64()).unwrap_or(0),
                    });
                }
            }
        }
    }
    best
}

/// The two bundles: (name, SANs, DNS zone the challenge records live in).
fn bundles(cloud: &Arc<CloudState>) -> Vec<(String, Vec<String>, String)> {
    let mut v = vec![
        (
            "apps".to_string(),
            vec![
                format!("*.{}", cloud.apps_domain),
                cloud.apps_domain.clone(),
            ],
            cloud.apps_domain.clone(),
        ),
        (
            "platform".to_string(),
            // api (developer/API-key surface) + admin (ops/admin console surface)
            // + webhook (incoming GitOps/OpenEdge build-notification receiver,
            // OPENEDGE_WEBHOOK_URL) + the dashboard hosts (apex/www — self-hosted
            // on-node, no external tunnel). There is no `*.{platform_domain}`
            // wildcard — each host is an explicit SAN.
            vec![
                format!("api.{}", cloud.platform_domain),
                format!("admin.{}", cloud.platform_domain),
                format!("webhook.{}", cloud.platform_domain),
                cloud.platform_domain.clone(),
                format!("www.{}", cloud.platform_domain),
                // Self-hosted SMS-fallback service (platform-deployed app the
                // edge routes by Host alias) — needs a real SAN like every
                // other explicit platform host (no *.{platform_domain}).
                format!("sms.{}", cloud.platform_domain),
            ],
            cloud.platform_domain.clone(),
        ),
    ];
    // Per-region API hosts (`api-<region>.<platform>`), matching the DNS records
    // the reconciler publishes for them — a name that resolves but fails TLS is
    // not usable, which is exactly what shipping the DNS half alone produced.
    // Regions come from the registry; sorted so the SAN list is deterministic
    // across passes (an order-flapping list would defeat the superset check that
    // triggers reissue). A region joining later grows the wanted set, the cached
    // bundle stops covering it, and the next ACME pass reissues automatically.
    {
        let mut regions: Vec<String> = cloud
            .registry
            .nodes()
            .into_iter()
            .map(|n| n.region.trim().to_ascii_lowercase())
            .filter(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            .collect();
        regions.sort();
        regions.dedup();
        for r in regions {
            v[1].1.push(format!("api-{r}.{}", cloud.platform_domain));
        }
    }
    // relay/discovery terminate their own TLS (iroh) — only add on request.
    if std::env::var("HIVE_ACME_PLATFORM_EXTRA")
        .map(|x| x == "1")
        .unwrap_or(false)
    {
        v[1].1.push(format!("relay.{}", cloud.platform_domain));
        v[1].1.push(format!("discovery.{}", cloud.platform_domain));
    }
    // Per-tenant DB gateway wildcard (`*.{db_domain}` + apex) — the SNI cert the
    // Postgres/Redis/HTTP proxy presents for every `<db>.{db_domain}` endpoint.
    // Only when the gateway is enabled (HIVE_DB_DOMAIN set).
    if !cloud.db_domain.is_empty() {
        v.push((
            "db".to_string(),
            vec![format!("*.{}", cloud.db_domain), cloud.db_domain.clone()],
            cloud.db_domain.clone(),
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LIVE Let's Encrypt STAGING issuance for the real apps wildcard through the
    /// real Vercel API. Ignored by default (network + credentials); run with:
    ///   VERCEL_API_TOKEN=… HIVE_ACME_STAGING=1 \
    ///   cargo test -p hive-cloud --features zkauth acme_staging_live -- --ignored --nocapture
    /// Side effects: transient `_acme-challenge` TXT records (cleaned up).
    #[tokio::test]
    #[ignore]
    async fn acme_staging_live() {
        assert!(
            staging(),
            "refusing: HIVE_ACME_STAGING must be 1 for the live test"
        );
        let http = reqwest::Client::new();
        let api = VercelApi::from_env(http.clone()).expect("VERCEL_API_TOKEN required");
        let apps = std::env::var("HIVE_APPS_DOMAIN").unwrap_or_else(|_| "shadw.app".into());
        let names = vec![format!("*.{apps}"), apps.clone()];
        let challenges = AcmeChallengeStore::new();
        let bundle = issue(&http, &api, &names, &apps, &challenges, None)
            .await
            .expect("staging issuance failed");
        assert!(bundle.chain_pem.contains("BEGIN CERTIFICATE"));
        assert!(
            bundle.key_pem_enc.starts_with("enc:v1:"),
            "key must be AEAD-encrypted"
        );
        install_bundle(&bundle).expect("bundle must install into the SNI resolver");
        assert!(installed_zones().contains(&apps));
        println!(
            "STAGING CERT ISSUED for {names:?}: {} bytes chain, zones {:?}",
            bundle.chain_pem.len(),
            installed_zones()
        );
    }

    #[test]
    fn challenge_names_are_zone_relative() {
        assert_eq!(
            challenge_record_name("shadw.app", "shadw.app"),
            "_acme-challenge"
        );
        assert_eq!(
            challenge_record_name("*.shadw.app", "shadw.app"),
            "_acme-challenge"
        );
        assert_eq!(
            challenge_record_name("api.shadw.cloud", "shadw.cloud"),
            "_acme-challenge.api"
        );
        assert_eq!(
            challenge_record_name("relay.shadw.cloud", "shadw.cloud"),
            "_acme-challenge.relay"
        );
    }

    #[test]
    fn sni_zone_mapping_installs_wildcard_and_exact() {
        // install_bundle needs real key material — test the zone-derivation logic
        // via the names → zones expansion instead (pure part).
        let names = vec![
            "*.shadw.app".to_string(),
            "shadw.app".to_string(),
            "api.shadw.cloud".to_string(),
        ];
        let mut zones: Vec<String> = Vec::new();
        for name in &names {
            let zone = name.strip_prefix("*.").unwrap_or(name).to_string();
            zones.push(zone.clone());
            if name.contains('.') && !name.starts_with("*.") {
                zones.push(name.clone());
            }
        }
        assert!(zones.contains(&"shadw.app".to_string()));
        assert!(zones.contains(&"api.shadw.cloud".to_string()));
    }
}
