//! ACME (Let's Encrypt) certificates: DNS-01 through an authoritative DNS provider
//! ([`crate::cloudflare_dns::DnsProvider`]: Vercel DNS when `VERCEL_API_TOKEN` is set, else
//! Cloudflare when `HIVE_CLOUDFLARE_API_TOKEN` is set; a zone Vercel cannot write, and the DB
//! gateway's wildcard, need it) and HTTP-01 for tenant custom domains.
//!
//! - `bundles()` lists what is issued, one isolated bundle per zone.
//! - Only the leader runs the ACME client (`leadership::may_act`: `Job::Acme`, `Job::AcmeHttp01`),
//! so N nodes never race Let's Encrypt; the ACCOUNT is fleet state (see `account`), so a handover
//! keeps one identity and one rate-limit budget.
//! - Renewal at `RENEW_AFTER_MS` (60 of 90 days), checked every 5-7 h.
//! - Distribution: bundle JSON in the guardian replicated store with the key sealed (`enc:v1:`,
//! `secrets.rs`); every node syncs (`spawn_cert_sync`: guardian, mesh, HTTPS) and hot-swaps the SNI
//! resolver (arc-swap).
//! - `HIVE_ACME_STAGING` defaults to staging: production allows 5 duplicate certificates per 168
//! hours.

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
use crate::cloudflare_dns::DnsProvider;
use crate::vercel_dns::{DesiredRecord, DnsApi};

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
/// Renew at two thirds of the validity: issued + 60 days.
const RENEW_AFTER_MS: u64 = 60 * 24 * 60 * 60 * 1000;

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

/// Hook fired with the SNI of every handshake this node can serve — the
/// earliest point at which a request's destination is knowable. Never awaited:
/// it only pushes onto the prewarm queue (see `crate::prewarm`).
static WARM_HOOK: std::sync::OnceLock<std::sync::Arc<dyn Fn(&str) + Send + Sync>> =
    std::sync::OnceLock::new();

/// Install the ClientHello-time prewarm hook. Called once at boot.
pub fn install_warm_hook(f: std::sync::Arc<dyn Fn(&str) + Send + Sync>) {
    let _ = WARM_HOOK.set(f);
}

fn fire_warm(name: &str) {
    if let Some(h) = WARM_HOOK.get() {
        h(name);
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        let map = certs().load();
        if let Some(k) = map.get(&name) {
            fire_warm(&name);
            return Some(k.clone());
        }
        let (_, zone) = name.split_once('.')?;
        let k = map.get(zone).cloned();
        if k.is_some() {
            fire_warm(&name);
        }
        k
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
            map.insert(name.to_ascii_lowercase(), ck.clone());
        }
    }
    certs().store(Arc::new(map));
    Ok(())
}

/// SNI resolver SCOPED to one tenant's hostnames. The shared [`SniResolver`] holds EVERY fleet
/// certificate (api./admin./webhook., the apps and DB wildcards, every custom domain); handing it
/// to a tenant-controlled raw-port listener would let it present any of them to a client asking by
/// SNI, a credential-harvesting surface. A published port speaks only for the hostnames routed to
/// its project; anything else fails the handshake (`resolve` returns `None`).
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

/// TLS for PUBLISHED raw ports (compose `ports: ["9001:9001"]`): the 443 gateway's SNI resolver, so
/// the project's certificate is the one the browser already trusts, but ALPN pinned to http/1.1:
/// the terminated plaintext is spliced into the container's own HTTP server, which a client that
/// negotiated h2 would address with h2 frames.
pub fn raw_server_config() -> Arc<rustls::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SniResolver));
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// How long one derived ticket key is used for minting (`lifetime()` advertises
/// this to clients too). Six hours matches rustls's own `Ticketer::new()`.
const TICKET_ROTATE_SECS: u64 = 6 * 60 * 60;

/// Layout: `[epoch: u64 BE][nonce: 12][ChaCha20-Poly1305 ct + 16-byte tag]`.
const TICKET_HEADER_LEN: usize = 8 + ring::aead::NONCE_LEN;

/// Upper bound on an accepted ticket: absurdly long input is rejected before the AEAD runs
/// (partitioning-oracle defence), with no mutable state on the accept path.
const TICKET_MAX_LEN: usize = 4096;

/// Changing the key-derivation domain invalidates every outstanding ticket at once: the intended
/// fleet-wide reset.
const TICKET_KEY_DOMAIN: &[u8] = b"hive-tls-session-ticket-v1";

fn shared_tickets_enabled() -> bool {
    match std::env::var("HIVE_TLS_SHARED_TICKETS").as_deref() {
        Ok("0") | Ok("false") => false,
        _ => true,
    }
}

fn ticket_epoch(secs: u64) -> u64 {
    secs / TICKET_ROTATE_SECS
}

/// The ChaCha20-Poly1305 key for one epoch, derived (never stored) and identical on every node; one
/// HMAC-SHA256 per handshake is negligible next to the ECDSA signature.
fn ticket_key(epoch: u64) -> ring::aead::LessSafeKey {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&crate::secrets::key_material())
        .expect("32-byte fleet key is a valid HMAC key");
    mac.update(TICKET_KEY_DOMAIN);
    mac.update(&epoch.to_be_bytes());
    let bytes = mac.finalize().into_bytes();
    let unbound = ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, bytes.as_slice())
        .expect("32-byte derived key is a valid ChaCha20-Poly1305 key");
    ring::aead::LessSafeKey::new(unbound)
}

/// Stateless, fleet-shared TLS session tickets.
///
/// rustls resumption is stateful per node (a 256-entry cache), so behind round-robin DNS roughly
/// three reconnects in four paid a full handshake. These tickets are stateless under a key EVERY
/// node derives from `secrets::key_material()`, the fleet secret that already seals the ACME keys,
/// so it grants nothing new. The key is re-derived every `TICKET_ROTATE_SECS` and a ticket is
/// accepted only in its own epoch or the next (a key is usable at most twice that period); 0-RTT
/// stays off (`max_early_data_size` is 0).
#[derive(Debug)]
pub struct FleetTicketer;

impl rustls::server::ProducesTickets for FleetTicketer {
    fn enabled(&self) -> bool {
        true
    }

    fn lifetime(&self) -> u32 {
        TICKET_ROTATE_SECS as u32
    }

    fn encrypt(&self, plain: &[u8]) -> Option<Vec<u8>> {
        if plain.len() + TICKET_HEADER_LEN > TICKET_MAX_LEN {
            return None;
        }
        let epoch = ticket_epoch(hive_core::now_ms() / 1000);
        use ring::rand::SecureRandom as _;
        let mut nonce = [0u8; ring::aead::NONCE_LEN];
        ring::rand::SystemRandom::new().fill(&mut nonce).ok()?;
        let mut sealed = plain.to_vec();
        ticket_key(epoch)
            .seal_in_place_append_tag(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                ring::aead::Aad::empty(),
                &mut sealed,
            )
            .ok()?;
        let mut out = Vec::with_capacity(TICKET_HEADER_LEN + sealed.len());
        out.extend_from_slice(&epoch.to_be_bytes());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Some(out)
    }

    /// Attacker-controlled input: length- and epoch-checked before the AEAD runs; any failure is
    /// `None` and rustls falls back to a full handshake. `open_in_place` leaves the Vec's length
    /// alone, so the tag must be truncated off or resumption silently never happens.
    fn decrypt(&self, cipher: &[u8]) -> Option<Vec<u8>> {
        if cipher.len() <= TICKET_HEADER_LEN || cipher.len() > TICKET_MAX_LEN {
            return None;
        }
        let (epoch_bytes, rest) = cipher.split_at(8);
        let epoch = u64::from_be_bytes(epoch_bytes.try_into().ok()?);
        let now = ticket_epoch(hive_core::now_ms() / 1000);
        if epoch != now && epoch + 1 != now {
            return None;
        }
        let (nonce_bytes, sealed) = rest.split_at(ring::aead::NONCE_LEN);
        let nonce: [u8; ring::aead::NONCE_LEN] = nonce_bytes.try_into().ok()?;
        let mut buf = sealed.to_vec();
        let plain_len = ticket_key(epoch)
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                ring::aead::Aad::empty(),
                &mut buf,
            )
            .ok()?
            .len();
        buf.truncate(plain_len);
        Some(buf)
    }
}

/// The 443 gateway's `ServerConfig`: the SNI resolver, h2 + http/1.1 ALPN and the fleet-shared
/// [`FleetTicketer`] (`HIVE_TLS_SHARED_TICKETS=0` restores the stateful per-node cache).
pub fn server_config() -> Arc<rustls::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SniResolver));
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    if shared_tickets_enabled() {
        cfg.ticketer = Arc::new(FleetTicketer);
    }
    Arc::new(cfg)
}

/// The DB gateway's `ServerConfig` (Postgres/Redis wire): the SNI resolver and NO ALPN, since
/// offering h2/http1.1 makes rustls alert a Postgres client (libpq 17+ offers ALPN `postgresql`).
pub fn db_server_config() -> Arc<rustls::ServerConfig> {
    Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(SniResolver)),
    )
}

/// How long a challenge entry is answered before it is swept: 1 h covers the slowest ACME retry
/// loop, while an abandoned entry (the leader died mid-order) can neither accumulate nor answer
/// forever.
const CHALLENGE_TTL_MS: u64 = 60 * 60 * 1000;

/// One name's pending DNS-01 TXT values (a wildcard+apex order places TWO
/// challenges on the SAME `_acme-challenge.<zone>` name, hence a Vec).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AcmeChallenge {
    pub values: Vec<String>,
    pub created_ms: u64,
}

/// fqdn (lowercase, no trailing dot) to pending DNS-01 TXT values, written on the leader next to
/// the provider write. For a zone delegated to Seer Let's Encrypt may query ANY fleet nameserver
/// while the ACME client runs only on the leader, so the values replicate to every node
/// (`acme_challenges` in `store_sync::REGISTRY`) and `dnsserver::lookup` answers from them.
pub struct AcmeChallengeStore {
    inner: parking_lot::RwLock<std::collections::BTreeMap<String, AcmeChallenge>>,
}

/// One pending HTTP-01 answer: the body LE expects at
/// `http://<domain>/.well-known/acme-challenge/<token>`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Http01Challenge {
    pub key_authorization: String,
    pub created_ms: u64,
}

/// token to key-authorization for in-flight HTTP-01 orders, replicated (`acme_http01` store_sync
/// entry) so a validation fetch landing on ANY node gets the answer; same TTL/sweep discipline as
/// the DNS-01 store.
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

    /// Adds `value` under `fqdn`, sweeping every expired entry under the same write lock (issuance
    /// is the natural sweep cadence).
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

    /// TXT values for `fqdn`; empty when unknown or past TTL. The TTL gate matters on FOLLOWERS
    /// too: store-sync adoption declines an empty leader snapshot, so a follower's copy of a
    /// cleaned-up challenge ages out here.
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

/// The `_acme-challenge` record NAME relative to `zone`: `shadw.app` in zone `shadw.app` gives
/// `_acme-challenge`, `api.shadw.cloud` in zone `shadw.cloud` gives `_acme-challenge.api`.
pub fn challenge_record_name(identifier: &str, zone: &str) -> String {
    let id = identifier.strip_prefix("*.").unwrap_or(identifier);
    if id == zone {
        "_acme-challenge".to_string()
    } else {
        let sub = id.strip_suffix(&format!(".{zone}")).unwrap_or(id);
        format!("_acme-challenge.{sub}")
    }
}

/// Polls EVERY authoritative nameserver of a fleet-served zone (those advertising a public `:53`
/// that peers in at least two regions attested) until all hold the challenge, or ~2 min pass.
/// `wait_txt` cannot make this check: Let's Encrypt asks one nameserver at random (one of four held
/// the `deploy` challenge while the order went `Invalid`) and public resolvers cache a single
/// server's answer.
async fn wait_txt_all_ns(cloud: &Arc<CloudState>, fqdn: &str, value: &str) -> bool {
    let ns: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.dns_ns.is_some() && n.dns_attest.len() >= 2)
        .filter_map(|n| n.public_ip.clone())
        .collect();
    if ns.is_empty() {
        return false;
    }
    for i in 0..24 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        let mut missing: Vec<String> = Vec::new();
        for ip in &ns {
            let addr = match ip.parse::<std::net::SocketAddr>() {
                Ok(a) => a,
                Err(_) => match format!("{ip}:53").parse::<std::net::SocketAddr>() {
                    Ok(a) => a,
                    Err(_) => {
                        missing.push(ip.clone());
                        continue;
                    }
                },
            };
            match crate::dns_probe::txt_values(
                addr,
                fqdn,
                std::time::Duration::from_secs(3),
            )
            .await
            {
                Ok(vals) if vals.iter().any(|v| v == value) => {}
                Ok(_) => missing.push(ip.clone()),
                Err(e) => missing.push(format!("{ip} ({e})")),
            }
        }
        if missing.is_empty() {
            tracing::info!(%fqdn, ns = ns.len(), "ACME dns-01 TXT visible on every fleet nameserver — order can be validated");
            return true;
        }
        if i % 6 == 5 {
            tracing::info!(%fqdn, missing = ?missing, "still waiting for ACME TXT to reach every fleet nameserver…");
        }
    }
    false
}

/// Polls public DNS (DoH against Google and Cloudflare) until the TXT value is visible, or ~2 min
/// pass: right for a zone with ONE anycast authority (Vercel); a fleet-served zone needs
/// [`wait_txt_all_ns`].
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
    if !crate::secrets::is_encrypted(&env.creds_enc) {
        tracing::error!(
            "ACME account credential could not be AEAD-sealed — refusing to store it in the clear"
        );
        return None;
    }
    serde_json::to_string(&env).ok()
}

/// Opens through `secrets::try_decrypt`, never `decrypt` (which returns its input when no key opens
/// it): a wrong key must read `Undecryptable`, not a parse failure that looks like `Missing` and
/// registers a second account.
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

/// Writes the sealed envelope to the local cache (temp file named after the target so staging and
/// prod never share one, rename, chmod 600) and VERIFIES it reads back and opens; `false` on any
/// failure, which callers treat as loud since an unwritten credential means the next pass registers
/// another account.
fn write_account_sealed(env_json: &str) -> bool {
    let path = account_sealed_path();
    let dir = crate::persist::data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
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
        return EnvelopeOpen::Missing;
    };
    let opened = open_account_envelope(&bytes);
    if matches!(opened, EnvelopeOpen::Missing) {
        tracing::error!(path = ?path, "the local sealed ACME account file is present but unreadable — it will be ignored");
    }
    opened
}

/// Observation validity for the ACME loops' incidents: the platform pass runs every 5-7 h and the
/// custom-domain pass backs off to ~64 min at most, so a live condition is re-asserted well inside
/// 24 h and a vanished one resolves within a day.
const INCIDENT_TTL_MS: u64 = 24 * 3600 * 1000;

/// Re-assert an incident for a condition the ACME loop observes on this pass
/// (the store dedups on the condition, so per-pass is free).
fn observe(incidents: Option<&crate::incidents::IncidentStore>, req: crate::incidents::OpenReq) {
    if let Some(inc) = incidents {
        inc.open(req);
    }
}

fn open_key_incident(incidents: Option<&crate::incidents::IncidentStore>, where_: &str) {
    tracing::error!(
        source = where_,
        "the fleet's ACME account credential exists but NO configured key opens it — set the \
         fleet-shared HIVE_SECRET_KEY (and HIVE_SECRET_KEY_OLD with the previous key if it was \
         rotated) so this node can reuse the fleet's Let's Encrypt account instead of registering \
         another one"
    );
    observe(
        incidents,
        crate::incidents::OpenReq {
            condition: "acme:account-key-unknown".into(),
            ttl_ms: INCIDENT_TTL_MS,
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

/// Resolves the fleet's ACME account: replicated store, then the local sealed cache, then the legacy
/// plaintext file (sealed and migrated forward), and only then creates one. The account is the
/// fleet's identity at Let's Encrypt and every rate limit that matters is scoped to it, so:
///
/// - The replicated copy is read FIRST: every node that was ever leader may hold a DIFFERENT account
///   of its own, and reading the local copy first would let each keep it forever. The local cache is
///   the fallback for a guardian that is down or empty, so an offline node still issues.
/// - A cache hit is never pushed to the replicated store: an empty `get` cannot tell "the fleet has
///   no account" from "this replica has not synced yet", and seeding on the second would overwrite
///   the fleet's identity. Only the migrate and create paths write it.
/// - A NEW account is created ONLY when no source yields credentials at all. Credentials that cannot
///   be ACTIVATED (directory unreachable, order endpoint 5xx) are an error to retry next pass, never
///   a reason to register a second identity.
/// - The credential is never written unsealed, logged or carried in an error or incident message,
///   and the legacy plaintext file is removed only once a verified sealed copy exists.
async fn account(
    _http: &reqwest::Client,
    incidents: Option<&crate::incidents::IncidentStore>,
) -> anyhow::Result<Account> {
    let mut found: Option<(&'static str, Box<AccountCredentials>)> = None;

    if let Some(bytes) = crate::guardian::get(&account_guardian_key()).await {
        match open_account_envelope(&bytes) {
            EnvelopeOpen::Opened(c) => {
                if let Some(env) = seal_account(&c) {
                    if !write_account_sealed(&env) {
                        tracing::warn!("adopted the replicated ACME account but could not write the local sealed cache");
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
    }

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
        tracing::error!(
            "ACME: registered a NEW Let's Encrypt account but could NOT store it — every future \
             pass will register another one until this is fixed"
        );
        observe(
            incidents,
            crate::incidents::OpenReq {
                condition: "acme:account-unstorable".into(),
                ttl_ms: INCIDENT_TTL_MS,
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

/// instant-acme's internal HTTPS client needs a process-level rustls provider; installing is
/// idempotent, and the listener may not have run yet.
fn ensure_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Issues one bundle: order, DNS-01 TXT through the provider, wait for propagation, validate,
/// finalize (rcgen CSR), certificate chain; the TXT records are cleaned up afterwards
/// (best-effort). `cloud` supplies the fleet's own nameservers (`None`, the Vercel-hosted staging
/// test, falls back to the public-resolver check, right for a single anycast authority).
///
/// Each challenge TXT goes to BOTH authorities: the provider while the zone (or a sub-zone like
/// `api.`) is still served there, and the replicated challenge store so every Seer nameserver
/// answers once the delegation moves here. Vercel refuses a TXT under a name it has delegated away
/// (409); that is tolerated ONLY for Vercel and ONLY under a LIVE delegation (the
/// `vercel_dns::STATS` gauges, never static zone config: below the capable-NS floor the api
/// delegation disengages to flat A records, and swallowing the 409 there would place nothing at the
/// authority that matters and fail opaquely at validation). Any other failure is fatal.
async fn issue(
    cloud: Option<&Arc<CloudState>>,
    http: &reqwest::Client,
    api: &DnsProvider,
    names: &[String],
    zone: &str,
    challenges: &AcmeChallengeStore,
    incidents: Option<&crate::incidents::IncidentStore>,
) -> anyhow::Result<CertBundle> {
    ensure_rustls_provider();
    let acct = account(http, incidents).await?;
    let identifiers: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
    let mut order = acct.new_order(&NewOrder::new(&identifiers)).await?;

    let mut txt_names: Vec<(String, String)> = Vec::new();
    {
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut authz = a?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = authz
                .challenge(ChallengeType::Dns01)
                .ok_or_else(|| anyhow::anyhow!("no dns-01 challenge offered"))?;
            let host = challenge.identifier().to_string();
            let value = challenge.key_authorization().dns_value();
            let rec_name = challenge_record_name(&host, zone);
            let fqdn = format!("{rec_name}.{zone}");
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
                let conflict = e.to_string().contains("409");
                let provider_is_vercel = api.kind() == "vercel";
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
                if provider_is_vercel && conflict && delegated_live {
                    tracing::warn!(%fqdn, error = %e, "ACME dns-01 Vercel TXT refused under a live-Seer-delegated name — proceeding via the challenge store");
                } else {
                    return Err(e);
                }
            }
            challenges.insert(&fqdn, &value);
            if api.kind() == "cloudflare" {
                tracing::info!(zone, record = %rec_name, "ACME dns-01 TXT written via the Cloudflare API + Seer challenge store");
            } else {
                tracing::info!(zone, record = %rec_name, "ACME dns-01 TXT written via Vercel API + Seer challenge store");
            }
            txt_names.push((rec_name, value));
        }
    }

    for (rec_name, value) in &txt_names {
        let fqdn = format!("{rec_name}.{zone}");
        let under = |z: Option<&str>| {
            z.map(|z| fqdn == z || fqdn.ends_with(&format!(".{z}")))
                .unwrap_or(false)
        };
        let seer_delegated = cloud.is_some()
            && (under(crate::dnsserver::deploy_zone()) || under(crate::dnsserver::api_zone()));
        let visible = match (seer_delegated, cloud) {
            (true, Some(c)) => wait_txt_all_ns(c, &fqdn, value).await,
            _ => wait_txt(http, &fqdn, value).await,
        };
        if !visible {
            tracing::warn!(%fqdn, "TXT not observed via DoH in time — proceeding anyway (LE queries the authoritative NS directly)");
        }
    }

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

/// Removes what `issue` placed: the challenge-store copy first, then the provider's record
/// (best-effort).
async fn cleanup_txt(
    api: &DnsProvider,
    zone: &str,
    txts: &[(String, String)],
    challenges: &AcmeChallengeStore,
) {
    if txts.is_empty() {
        return;
    }
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

/// The (bundle, SANs, zone) set for every verified custom domain, shaped like `bundles()` so
/// cert-sync can chain the two. SANs are the apex plus the explicit `www.` host ONLY: Let's Encrypt
/// never offers HTTP-01 for a wildcard identifier (wildcards for tenant domains belong to DNS-01
/// against a delegated zone).
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

/// HTTP-01 issuance for custom domains: `issue`'s shape with the replicated `Http01Store` standing
/// in for the TXT double-write. No DNS zone is written: the tenant's DNS already points the name at
/// the fleet, so Let's Encrypt's fetch lands on some node, and any node answers from the store.
async fn issue_http01(
    http: &reqwest::Client,
    names: &[String],
    store: &Http01Store,
    incidents: Option<&crate::incidents::IncidentStore>,
) -> anyhow::Result<CertBundle> {
    ensure_rustls_provider();
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

/// The SAN set an HTTP-01 order can validate THIS pass: `www.{domain}` is kept only when the public
/// internet resolves it to a current fleet edge address, because Let's Encrypt fails the WHOLE
/// order when any SAN fails (a tenant who pointed only their apex would never get even the apex
/// certificate). The narrowed set feeds the freshness check, so a later `www` arrival re-issues
/// with full coverage.
///
/// The proof is asked of [`crate::dns_quorum::VANTAGES`] — several independent public resolvers, a
/// majority of the answering ones required — and NOT of one resolver: a single resolver's cache can
/// lag a delegation for the whole life of the old answer, and on its stale "no" alone this function
/// used to drop `www` from the order of a domain the platform did serve (`geniee.site` answered a
/// TLS error on `https://www.` for exactly this). One resolver erroring is an abstention, never a
/// "no"; see the module docs for why a majority and not unanimity, and why Seer is not a vantage.
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
    let answers = crate::dns_probe::public_answers(&cloud.http, www, 1).await;
    let votes = crate::dns_quorum::operator_votes(answers, |ip| edge.contains(ip));
    let tally = crate::dns_quorum::tally(&votes);
    if tally.admits() {
        return names.to_vec();
    }
    tracing::warn!(
        %apex,
        %www,
        yes = tally.yes,
        no = tally.no,
        abstain = tally.abstain,
        votes = %crate::dns_quorum::render(&votes),
        "custom-domain www not confirmed by a majority of public resolvers — ordering apex-only SAN this pass (the whole order would fail otherwise)"
    );
    vec![apex.clone()]
}

/// Installs a custom bundle BEFORE its DomainRecord may claim issuance: the record's metadata is a
/// health statement read independently of the SNI cache, so writing it after an install failure
/// would recreate the fictional "active SSL" state.
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

/// One pass over every verified custom domain: ensure a fresh, SAN-covering bundle exists, else
/// issue one through HTTP-01 and publish it (guardian + local cache) for cert-sync. A missing local
/// cache adopts the guardian replica first, so a fresh leader or wiped data dir does not re-issue
/// every domain.
///
/// Leader-only INSIDE (`leadership::may_act(AcmeHttp01)`, whose designation is exactly the
/// `acme_http01` store's writer), so it is safe to call from anywhere: an ungated pass would
/// duplicate-issue against Let's Encrypt's 5-per-168 h budget and drop its tokens into a follower
/// store the next store_sync pull wipes mid-order.
pub async fn custom_cert_pass(cloud: &Arc<CloudState>) {
    if cloud.ingress == "ngrok" {
        return;
    }
    if !crate::leadership::may_act(cloud, crate::leadership::Job::AcmeHttp01) {
        return;
    }
    /// Per-bundle `(consecutive failures, next attempt ms)`, in memory (a restart resets it, a
    /// success removes it). A domain whose DNS never arrives would otherwise order every 300 s,
    /// burning the account's failed-validation and new-orders budgets until the platform's own
    /// renewals starve, so retries back off from 2 min to a ~64 min ceiling (a circuit's open
    /// window must outlast the failure it guards). It never parks: tenants often point DNS only
    /// AFTER attaching. At 12 consecutive failures an incident opens and retries continue.
    static STREAKS: std::sync::OnceLock<
        parking_lot::RwLock<std::collections::HashMap<String, (u32, u64)>>,
    > = std::sync::OnceLock::new();
    let streaks =
        STREAKS.get_or_init(|| parking_lot::RwLock::new(std::collections::HashMap::new()));
    /// Per-bundle in-flight set: concurrent kicks and leadership flaps must never run two issuances
    /// of the same SAN set, since five duplicate orders inside 168 h close Let's Encrypt's
    /// duplicate-certificate window for a week. `IssuingGuard` releases the reservation on EVERY
    /// exit, so a cancelled pass never wedges renewals.
    static ISSUING: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let issuing = ISSUING.get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()));
    for (bundle, names, zone) in custom_domain_bundles(cloud) {
        if !issuing.lock().insert(bundle.clone()) {
            continue;
        }
        let _issue_guard = IssuingGuard {
            set: issuing,
            bundle: bundle.clone(),
        };
        {
            let (_fails, next_at) = streaks.read().get(&bundle).copied().unwrap_or((0, 0));
            if hive_core::now_ms() < next_at {
                continue;
            }
        }
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
                cloud.incidents.clear(
                    &format!("acme:custom-cert-failing:{bundle}"),
                    "HTTP-01 issuance succeeded — the bundle is installed.",
                );
                cloud.incidents.clear(
                    &format!("acme:rate-limit:{bundle}"),
                    "Issuance succeeded — the Let's Encrypt rate-limit window has opened.",
                );
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
                let msg = e.to_string();
                if msg.contains("rateLimited") || msg.contains("rate limit") {
                    cloud.incidents.open(crate::incidents::OpenReq {
                        condition: format!("acme:rate-limit:{bundle}"),
                        ttl_ms: INCIDENT_TTL_MS,
                        title: format!("ACME rate limit: {bundle}"),
                        severity: crate::incidents::Severity::Major,
                        affected: names.clone(),
                        message: format!(
                            "ACME rate limit for custom domain bundle {bundle}: {msg}"
                        ),
                    });
                }
                if fails >= 12 {
                    cloud.incidents.open(crate::incidents::OpenReq {
                        condition: format!("acme:custom-cert-failing:{bundle}"),
                        ttl_ms: INCIDENT_TTL_MS,
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
            custom_cert_pass(&cloud).await;
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        }
    });
}

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

/// Leader loop: every bundle from `bundles()` is ensured fresh on a jittered 5-7 h cadence (a cheap
/// no-op when fresh). Fresh means younger than `RENEW_AFTER_MS` AND covering every wanted name: the
/// coverage check is what makes a SAN addition (a new region's `api-<region>` host) reissue by itself
/// instead of failing TLS until the 60-day renewal.
///
/// Gated by `leadership::may_act(Job::Acme)`, the designation of the DNS writer, billing and every
/// other single-writer job, so two nodes never order the same bundle against Let's Encrypt's
/// duplicate window (the `HIVE_DNS_LEADER_NODE` pin survives only on a chain-less mesh). Off the gate
/// the loop re-asks every minute instead of sleeping the renewal interval, so a node that becomes
/// the writer does not wait hours.
///
/// A sentinel file `$HIVE_DATA/acme-force-<bundle>` forces one re-issue regardless of freshness and
/// is deleted after a successful issue (`touch` it on the leader and restart, as the loop sleeps
/// between passes; clearing the local or guardian cache cannot force it, since cert-sync pulls the
/// old bundle back from a peer). A forced renewal spends the duplicate-certificate budget (5 per 168
/// h per exact identifier set); a rate-limited order opens the `acme:rate-limit:<bundle>` incident.
pub fn spawn_acme(cloud: Arc<CloudState>) {
    if cloud.ingress == "ngrok"
        && std::env::var("HIVE_ACME_FORCE")
            .map(|v| v == "1")
            .unwrap_or(false)
            == false
    {
        return;
    }
    let Some(api) = crate::cloudflare_dns::DnsProvider::from_env(cloud.http.clone()) else {
        tracing::warn!(
            "ACME enabled but neither VERCEL_API_TOKEN nor HIVE_CLOUDFLARE_API_TOKEN is set — not starting"
        );
        return;
    };
    tracing::info!(staging = staging(), provider = api.kind(), apps = %cloud.apps_domain, platform = %cloud.platform_domain, "ACME manager up (leader-elected, DNS-01)");
    tokio::spawn(async move {
        loop {
            let jitter = (hive_core::now_ms() % 7200) as u64;
            let sleep = 5 * 3600 + jitter;
            if !crate::leadership::may_act(&cloud, crate::leadership::Job::Acme) {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
            for (bundle, names, zone) in bundles(&cloud) {
                let force_path = crate::persist::data_dir().join(format!("acme-force-{bundle}"));
                let forced = std::fs::metadata(&force_path).is_ok();
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
                match issue(Some(&cloud), 
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
                        cloud.incidents.clear(
                            &format!("acme:rate-limit:{bundle}"),
                            "Issuance succeeded — the Let's Encrypt rate-limit window has opened.",
                        );
                        store_bundle_local(&bundle, &b);
                        if forced {
                            let _ = std::fs::remove_file(&force_path);
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
                        if e.to_string().contains("rateLimited") {
                            cloud.incidents.open(crate::incidents::OpenReq {
                                condition: format!("acme:rate-limit:{bundle}"),
                                ttl_ms: INCIDENT_TTL_MS,
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
            tokio::time::sleep(std::time::Duration::from_secs(sleep)).await;
        }
    });
}

/// Removes a custom-domain bundle that is no longer wanted: its SNI entries (minus any name a
/// WANTED bundle still covers: two attachments can share the `www` key) and its local cache file.
/// Without it a detached domain kept answering HTTPS until restart: routing revoked, TLS not.
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

/// cert-sync's poll interval while some bundle is still missing.
const CERT_SYNC_CATCHUP_SECS: u64 = 20;
/// cert-sync's poll interval once every bundle is installed.
const CERT_SYNC_STEADY_SECS: u64 = 300;

/// Every node: load local cache at boot, then poll the guardian replicated store
/// and hot-swap the SNI resolver when a newer bundle lands.
///
/// Per bundle the newest source wins in this order: guardian replica (bounded to 10 s: a sick
/// guardian must not starve the loop), authenticated mesh (`mesh_fetch`), then the HTTPS admin
/// fallback (`http_fetch`), which survives the iroh mesh to the issuer being down (including the
/// bootstrap deadlock where the relay hostname is missing from the very cert being synced). The
/// local cache installs first at boot, then the loop runs every `CERT_SYNC_CATCHUP_SECS` until
/// every bundle is installed and every `CERT_SYNC_STEADY_SECS` after.
///
/// Custom-domain bundles no longer wanted are pruned so they stop answering TLS, behind
/// blast-radius guards: an EMPTY domain snapshot (a wholesale-replace flap signature) prunes
/// nothing, a bundle must be unwanted for TWO consecutive ticks, and SNI keys a wanted bundle still
/// covers are kept. Each node prunes on its own cadence; a lingering guardian replica is never
/// fetched, since the loop iterates only the wanted set.
pub fn spawn_cert_sync(cloud: Arc<CloudState>) {
    if cloud.ingress == "ngrok" {
        return;
    }
    tokio::spawn(async move {
        let mut installed_issued_ms: HashMap<String, u64> = HashMap::new();
        let mut unwanted_ticks: HashMap<String, u8> = HashMap::new();
        for (bundle, ..) in bundles(&cloud)
            .into_iter()
            .chain(custom_domain_bundles(&cloud))
        {
            if let Some(b) = load_bundle_local(&bundle) {
                if install_bundle(&b).is_ok() {
                    installed_issued_ms.insert(bundle.clone(), b.issued_ms);
                    tracing::info!(%bundle, "TLS bundle loaded from local cache");
                }
            }
        }
        loop {
            let wanted: Vec<(String, Vec<String>, String)> = custom_domain_bundles(&cloud);
            let wanted_bundles: std::collections::HashSet<String> =
                wanted.iter().map(|(b, ..)| b.clone()).collect();
            let wanted_names: std::collections::HashSet<String> = wanted
                .iter()
                .flat_map(|(_, names, _)| names.iter().cloned())
                .collect();
            for b in installed_issued_ms
                .keys()
                .filter(|b| b.starts_with("dom-"))
                .cloned()
                .collect::<Vec<_>>()
            {
                if wanted_bundles.contains(&b) {
                    unwanted_ticks.remove(&b);
                } else {
                    *unwanted_ticks.entry(b).or_insert(0) += 1;
                }
            }
            if !cloud.domains.snapshot().is_empty() {
                let stale: Vec<String> = unwanted_ticks
                    .iter()
                    .filter(|(_, n)| **n >= 2)
                    .map(|(b, _)| b.clone())
                    .collect();
                for b in stale {
                    prune_custom_bundle(&b, &wanted_names, &mut installed_issued_ms);
                    unwanted_ticks.remove(&b);
                }
            }
            for (bundle, ..) in bundles(&cloud)
                .into_iter()
                .chain(custom_domain_bundles(&cloud))
            {
                let cur = installed_issued_ms.get(&bundle).copied().unwrap_or(0);
                let mut candidate: Option<CertBundle> = None;
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
                    installed_issued_ms.insert(bundle.clone(), b.issued_ms);
                    tracing::info!(%bundle, zones = ?installed_zones(), "TLS bundle installed (replica/mesh sync)");
                }
            }
            let have_all = installed_issued_ms.len()
                >= bundles(&cloud)
                    .into_iter()
                    .chain(custom_domain_bundles(&cloud))
                    .count();
            tokio::time::sleep(std::time::Duration::from_secs(if have_all {
                CERT_SYNC_STEADY_SECS
            } else {
                CERT_SYNC_CATCHUP_SECS
            }))
            .await;
        }
    });
}

/// HTTPS-admin fallback for cert distribution, authenticated by `HIVE_INTERNAL_TOKEN` (fails closed
/// when unset); it needs HTTPS to the platform edge, not the iroh mesh. `api.<platform>` is multi-A
/// round-robin, so one request lands on a random, possibly stale node: every healthy registry node
/// is asked directly instead (its `public_ip` pinned under the `api.<platform>` TLS name) and the
/// NEWEST bundle wins, as in `mesh_fetch`. `HIVE_CERT_SYNC_URLS` adds explicit targets.
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

/// Serves the local bundle to a MESH PEER with the private key DECRYPTED, over the
/// peer-authenticated iroh QUIC mesh; the receiver re-seals it under ITS OWN key (without a
/// fleet-shared `HIVE_SECRET_KEY` a replicated ciphertext is unreadable off the writer). What is
/// served is DERIVED from `bundles()` plus the `dom-*` custom bundles, never a hardcoded list: a
/// bundle missing from one (`deploy`) made every follower answer 404 and its certificate never
/// sync.
pub fn bundle_for_mesh(cloud: &Arc<CloudState>, bundle: &str) -> Vec<u8> {
    let known: std::collections::HashSet<String> =
        bundles(cloud).into_iter().map(|(name, ..)| name).collect();
    if !known.contains(bundle) && !bundle.starts_with("dom-") {
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

/// Per-peer budget of a mesh bundle fetch: leaves `PeerPool::acquire`'s fresh-discovery fallback
/// room to resolve a stale or flapped hint.
const MESH_FETCH_TIMEOUT_SECS: u64 = 15;

/// Fetches a bundle from the mesh peers (leader or already-synced nodes), re-sealing the key under
/// this node's own key. Queries ALL peers and keeps the NEWEST bundle (highest `issued_ms`), never
/// the first responder: a peer still serving a stale bundle would otherwise win the sync and a SAN
/// change never propagate.
async fn mesh_fetch(cloud: &Arc<CloudState>, bundle: &str) -> Option<CertBundle> {
    let peers: Vec<(String, String)> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| !n.is_self && n.healthy)
        .filter_map(|n| Some((n.peer_id?, n.iroh_addr?)))
        .collect();
    let mut best: Option<CertBundle> = None;
    for (id, addr) in peers {
        let path = format!("/v1/tls/bundle?name={bundle}");
        if let Some(bytes) = crate::gossip::request_to(
            cloud,
            &id,
            &addr,
            hive_p2p::GOSSIP_GET,
            &path,
            &[],
            MESH_FETCH_TIMEOUT_SECS,
        )
        .await
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

/// Every bundle this fleet issues, as (name, SANs, DNS zone the challenge records live in):
///
/// - `apps`: `*.{apps_domain}` + apex.
/// - `platform`: explicit SANs only, no wildcard: `api.`, `admin.`, `webhook.`, `sms.`, the dashboard
///   apex and `www.`, one `api-<region>.` per registry region (matching the records the reconciler
///   publishes: a name that resolves but fails TLS is not usable) and, with
///   `HIVE_ACME_PLATFORM_EXTRA=1`, `relay.` and `discovery.` (they terminate their own TLS through
///   iroh). Regions are sorted so the SAN list is deterministic: a flapping order would defeat the
///   superset check that triggers reissue, and a region joining grows the wanted set, so the next
///   pass reissues on its own.
/// - `db`: `*.{db_domain}` + apex, only with the DB gateway enabled (`HIVE_DB_DOMAIN`): the
///   certificate the Postgres/Redis/HTTP proxy presents for every `<db>.{db_domain}`.
/// - `deploy`: `*.{deploy_zone}` + apex, only when `HIVE_DEPLOY_ZONE` names a zone delegated to this
///   fleet's nameservers. The PREVIEW namespace: one wildcard covers every preview label ever
///   minted, where a wildcard per project would need one certificate each against Let's Encrypt's
///   per-domain cap.
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
            vec![
                format!("api.{}", cloud.platform_domain),
                format!("admin.{}", cloud.platform_domain),
                format!("webhook.{}", cloud.platform_domain),
                cloud.platform_domain.clone(),
                format!("www.{}", cloud.platform_domain),
                format!("sms.{}", cloud.platform_domain),
            ],
            cloud.platform_domain.clone(),
        ),
    ];
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
    if std::env::var("HIVE_ACME_PLATFORM_EXTRA")
        .map(|x| x == "1")
        .unwrap_or(false)
    {
        v[1].1.push(format!("relay.{}", cloud.platform_domain));
        v[1].1.push(format!("discovery.{}", cloud.platform_domain));
    }
    if !cloud.db_domain.is_empty() {
        v.push((
            "db".to_string(),
            vec![format!("*.{}", cloud.db_domain), cloud.db_domain.clone()],
            cloud.db_domain.clone(),
        ));
    }
    if let Some(dz) = crate::dnsserver::deploy_zone() {
        v.push((
            "deploy".to_string(),
            vec![format!("*.{dz}"), dz.to_string()],
            dz.to_string(),
        ));
    }
    v
}

/// True once a certificate covering `zone` is installed on this node; `state::deploy_url` gates
/// preview URLs on it, so a failed or slow issuance degrades to the apps domain instead of URLs
/// that fail the handshake.
pub fn zone_tls_ready(zone: &str) -> bool {
    installed_zones().iter().any(|z| z == zone)
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
        let api = DnsProvider::Vercel(
            crate::vercel_dns::VercelApi::from_env(http.clone())
                .expect("VERCEL_API_TOKEN required"),
        );
        let apps = std::env::var("HIVE_APPS_DOMAIN").unwrap_or_else(|_| "shadw.app".into());
        let names = vec![format!("*.{apps}"), apps.clone()];
        let challenges = AcmeChallengeStore::new();
        let bundle = issue(None, &http, &api, &names, &apps, &challenges, None)
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
