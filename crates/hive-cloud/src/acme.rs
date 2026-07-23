//! ACME (Let's Encrypt) wildcard certificates via **DNS-01 through the Vercel
//! API** — the TLS half of the ngrok retirement.
//!
//! Two certificate bundles, deliberately isolated (the vercel.com/vercel.app
//! split): **apps** = `*.{apps_domain}` + apex; **platform** = `api.{platform
//! _domain}` (+ `relay.`/`discovery.` if `HIVE_ACME_PLATFORM_EXTRA=1`; the iroh
//! relay does its own TLS so they're off by default).
//!
//! * Only the CLUSTER LEADER (same election as billing/DNS-reconciler) runs the
//!   ACME client — N nodes must not race Let's Encrypt.
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
    std::env::var("HIVE_ACME_STAGING").map(|v| v != "0" && v != "false").unwrap_or(true)
}

fn directory_url() -> &'static str {
    if staging() { LetsEncrypt::Staging.url() } else { LetsEncrypt::Production.url() }
}

// ---- SNI resolver (hot-swappable) ---------------------------------------------

/// zone (registrable domain, e.g. `shadw.app`) → certified key.
static CERTS: std::sync::OnceLock<ArcSwap<HashMap<String, Arc<CertifiedKey>>>> = std::sync::OnceLock::new();

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
        for doh in ["https://dns.google/resolve", "https://cloudflare-dns.com/dns-query"] {
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
                        .map(|arr| arr.iter().any(|rec| rec.get("data").and_then(|d| d.as_str()).map(|d| d.trim_matches('"') == value).unwrap_or(false)))
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

/// Load (or create once) the ACME account. Credentials persist in
/// `$HIVE_DATA/acme-account.json` (leader-local; a new leader creates its own).
async fn account(http: &reqwest::Client) -> anyhow::Result<Account> {
    let _ = http; // account creation uses instant-acme's own hyper client
    let path = crate::persist::data_dir().join(if staging() { "acme-account-staging.json" } else { "acme-account.json" });
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(creds) = serde_json::from_str::<AccountCredentials>(&s) {
            if let Ok(acct) = Account::builder()?.from_credentials(creds).await {
                return Ok(acct);
            }
        }
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
    if let Ok(js) = serde_json::to_string(&creds) {
        let _ = std::fs::write(&path, js);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
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
) -> anyhow::Result<CertBundle> {
    // instant-acme's internal HTTPS client needs a process-level rustls provider;
    // idempotent, so install unconditionally (the listener may not have run yet).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let acct = account(http).await?;
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
            let mut challenge = authz
                .challenge(ChallengeType::Dns01)
                .ok_or_else(|| anyhow::anyhow!("no dns-01 challenge offered"))?;
            let host = challenge.identifier().to_string();
            let value = challenge.key_authorization().dns_value();
            let rec_name = challenge_record_name(&host, zone);
            api.create(zone, &DesiredRecord { name: rec_name.clone(), rtype: "TXT".into(), value: value.clone(), ttl: 60 }).await?;
            tracing::info!(zone, record = %rec_name, "ACME dns-01 TXT written via Vercel API");
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
        cleanup_txt(api, zone, &txt_names).await;
        anyhow::bail!("ACME order not ready (status {status:?})");
    }
    let key_pem = order.finalize().await?;
    let chain = order.poll_certificate(&retry).await?;
    cleanup_txt(api, zone, &txt_names).await;

    let now = hive_core::now_ms();
    Ok(CertBundle {
        names: names.to_vec(),
        chain_pem: chain,
        key_pem_enc: crate::secrets::encrypt(&key_pem),
        issued_ms: now,
        not_after_ms: now + NINETY_DAYS_MS,
    })
}

async fn cleanup_txt(api: &VercelApi, zone: &str, txts: &[(String, String)]) {
    if txts.is_empty() {
        return;
    }
    if let Ok(records) = api.list(zone).await {
        for r in records {
            if r.rtype == "TXT" && txts.iter().any(|(n, v)| *n == r.name && (*v == r.value || r.value.trim_matches('"') == *v)) {
                let _ = api.delete(zone, &r.id).await;
            }
        }
    }
}

// ---- orchestration ---------------------------------------------------------------

fn guardian_key(bundle: &str) -> String {
    format!("tls/{}/{bundle}", if staging() { "staging" } else { "prod" })
}

fn cache_path(bundle: &str) -> std::path::PathBuf {
    crate::persist::data_dir().join(format!("tls-{bundle}{}.json", if staging() { "-staging" } else { "" }))
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
    if cloud.ingress == "ngrok" && std::env::var("HIVE_ACME_FORCE").map(|v| v == "1").unwrap_or(false) == false {
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
            let dns_pref = std::env::var("HIVE_DNS_LEADER_NODE").ok().filter(|s| !s.trim().is_empty());
            let chain = crate::cluster::Cluster::owner_chain_from_env();
            let pref = dns_pref.or_else(|| std::env::var("HIVE_CP_LEADER").ok());
            let leader =
                crate::cluster::Cluster::control_plane_owner(&chain, pref.as_deref(), &cloud.registry.nodes());
            if leader.as_deref() == Some(cloud.node_name.as_str()) {
                for (bundle, names, zone) in bundles(&cloud) {
                    // One-shot force: a sentinel file `$HIVE_DATA/acme-force-<bundle>`
                    // makes this pass re-issue the bundle regardless of freshness, then
                    // is deleted after a successful issue. This is the ONLY reliable way
                    // to re-issue after a SAN change (e.g. adding admin.) — clearing the
                    // local/guardian cache doesn't work because cert-sync pulls the old
                    // bundle back from a peer via mesh_fetch. `touch` it on the leader,
                    // restart, and the new SANs land in one pass.
                    let force_path = crate::persist::data_dir().join(format!("acme-force-{bundle}"));
                    let forced = std::fs::metadata(&force_path).is_ok();
                    let fresh = !forced
                        && load_bundle_local(&bundle)
                            .map(|b| hive_core::now_ms() < b.issued_ms + RENEW_AFTER_MS)
                            .unwrap_or(false);
                    if fresh {
                        continue;
                    }
                    tracing::info!(%bundle, ?names, forced, "ACME: issuing/renewing certificate bundle");
                    match issue(&cloud.http, &api, &names, &zone).await {
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
                        Err(e) => tracing::warn!(error = %e, %bundle, "ACME issuance failed (will retry next pass)"),
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(sleep)).await;
        }
    });
}

/// Every node: load local cache at boot, then poll the guardian replicated store
/// and hot-swap the SNI resolver when a newer bundle lands.
pub fn spawn_cert_sync(cloud: Arc<CloudState>) {
    if cloud.ingress == "ngrok" {
        return;
    }
    tokio::spawn(async move {
        let mut installed: HashMap<String, u64> = HashMap::new(); // bundle -> issued_ms
        // boot: local cache first (guardian may take a while to come online)
        for (bundle, ..) in bundles(&cloud) {
            if let Some(b) = load_bundle_local(&bundle) {
                if install_bundle(&b).is_ok() {
                    installed.insert(bundle.clone(), b.issued_ms);
                    tracing::info!(%bundle, "TLS bundle loaded from local cache");
                }
            }
        }
        loop {
            for (bundle, ..) in bundles(&cloud) {
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
                                Err(e) => tracing::debug!(%bundle, newer_issued_ms = b.issued_ms, cur, "cert-sync: guardian replica newer but uninstallable here (foreign AEAD key?): {e}"),
                            }
                        }
                    }
                }
                if candidate.is_none() {
                    match mesh_fetch(&cloud, &bundle).await {
                        Some(b) if b.issued_ms > cur => match install_bundle(&b) {
                            Ok(()) => candidate = Some(b),
                            Err(e) => tracing::warn!(%bundle, issued_ms = b.issued_ms, "cert-sync: mesh bundle failed to install: {e}"),
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
                                Err(e) => tracing::warn!(%bundle, issued_ms = b.issued_ms, "cert-sync: http bundle failed to install: {e}"),
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
            let have_all = installed.len() >= bundles(&cloud).len();
            tokio::time::sleep(std::time::Duration::from_secs(if have_all { 300 } else { 20 })).await;
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
    let token = std::env::var("HIVE_INTERNAL_TOKEN").ok().filter(|t| !t.trim().is_empty())?;
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
        let Ok(client) = builder.build() else { continue };
        let resp = match client.get(&url).header("x-hive-internal", &token).send().await {
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
        let Ok(v) = resp.json::<serde_json::Value>().await else { continue };
        let key_pem = v.get("key_pem").and_then(|k| k.as_str()).unwrap_or("");
        let chain = v.get("chain_pem").and_then(|c| c.as_str()).unwrap_or("");
        if key_pem.is_empty() || chain.is_empty() {
            continue;
        }
        let issued = v.get("issued_ms").and_then(|i| i.as_u64()).unwrap_or(0);
        if best.as_ref().map(|b| issued > b.issued_ms).unwrap_or(true) {
            tracing::info!(%bundle, %url, ?pin, issued_ms = issued, "cert-sync: http fallback fetched bundle");
            best = Some(CertBundle {
                names: v.get("names").and_then(|n| serde_json::from_value(n.clone()).ok()).unwrap_or_default(),
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
    if bundle != "apps" && bundle != "platform" && bundle != "db" {
        return Vec::new();
    }
    let Some(b) = load_bundle_local(bundle) else { return Vec::new() };
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
        if let Some(bytes) = crate::gossip::request_to(cloud, &id, &addr, hive_p2p::GOSSIP_GET, &path, &[], 15).await {
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
                        names: v.get("names").and_then(|n| serde_json::from_value(n.clone()).ok()).unwrap_or_default(),
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
            vec![format!("*.{}", cloud.apps_domain), cloud.apps_domain.clone()],
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
            ],
            cloud.platform_domain.clone(),
        ),
    ];
    // relay/discovery terminate their own TLS (iroh) — only add on request.
    if std::env::var("HIVE_ACME_PLATFORM_EXTRA").map(|x| x == "1").unwrap_or(false) {
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
        assert!(staging(), "refusing: HIVE_ACME_STAGING must be 1 for the live test");
        let http = reqwest::Client::new();
        let api = VercelApi::from_env(http.clone()).expect("VERCEL_API_TOKEN required");
        let apps = std::env::var("HIVE_APPS_DOMAIN").unwrap_or_else(|_| "shadw.app".into());
        let names = vec![format!("*.{apps}"), apps.clone()];
        let bundle = issue(&http, &api, &names, &apps).await.expect("staging issuance failed");
        assert!(bundle.chain_pem.contains("BEGIN CERTIFICATE"));
        assert!(bundle.key_pem_enc.starts_with("enc:v1:"), "key must be AEAD-encrypted");
        install_bundle(&bundle).expect("bundle must install into the SNI resolver");
        assert!(installed_zones().contains(&apps));
        println!("STAGING CERT ISSUED for {names:?}: {} bytes chain, zones {:?}", bundle.chain_pem.len(), installed_zones());
    }

    #[test]
    fn challenge_names_are_zone_relative() {
        assert_eq!(challenge_record_name("shadw.app", "shadw.app"), "_acme-challenge");
        assert_eq!(challenge_record_name("*.shadw.app", "shadw.app"), "_acme-challenge");
        assert_eq!(challenge_record_name("api.shadw.cloud", "shadw.cloud"), "_acme-challenge.api");
        assert_eq!(challenge_record_name("relay.shadw.cloud", "shadw.cloud"), "_acme-challenge.relay");
    }

    #[test]
    fn sni_zone_mapping_installs_wildcard_and_exact() {
        // install_bundle needs real key material — test the zone-derivation logic
        // via the names → zones expansion instead (pure part).
        let names = vec!["*.shadw.app".to_string(), "shadw.app".to_string(), "api.shadw.cloud".to_string()];
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
