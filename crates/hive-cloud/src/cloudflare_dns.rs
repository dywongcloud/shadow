//! Cloudflare DNS provider: authoritative writes for zones Vercel does not serve.
//!
//! A DB-gateway domain frequently lives in a zone the platform does not control through Vercel (a
//! Cloudflare-authoritative apex, say). Nothing else in this crate could write such a zone, so the
//! gateway's DNS had to be created by hand. This module is that provider, and it serves exactly two
//! jobs:
//!
//! * `A`/`AAAA` for the gateway apex and its wildcard, **DNS-only** (`proxied: false`). The gateway
//! terminates RAW TLS and routes the tenant by SNI; a Cloudflare-proxied record makes Cloudflare
//! terminate TLS with its own certificate, and every tenant then looks like one unknown host.
//! * `TXT` `_acme-challenge.*` for ACME **DNS-01**, the only way to get the `*.{db_domain}`
//! wildcard the per-tenant gateway needs (Let's Encrypt never offers HTTP-01 for a wildcard
//! identifier).
//!
//! It implements [`crate::vercel_dns::DnsApi`], the trait `VercelApi` implements, so every caller
//! (the ACME DNS-01 writer, the DB-zone publisher) works against either provider unchanged.
//!
//! Config (env, treated as secrets, never logged):
//! * `HIVE_CLOUDFLARE_API_TOKEN`: bearer token; absent means the provider is off.
//! * `HIVE_CLOUDFLARE_ZONE_ID`: pin one zone instead of resolving by name.
//! * `HIVE_CLOUDFLARE_ACCOUNT_ID`: scope the zone lookup to one account.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::vercel_dns::{DesiredRecord, DnsApi, RecordView};

const API: &str = "https://api.cloudflare.com/client/v4";

/// TTL for every record this provider writes. 60 keeps a gateway move (the
/// per-slug A record following a database to another node) fast to converge.
pub const DEFAULT_TTL: u32 = 60;

/// Cloudflare's code (81053) for "record already exists" on create. A duplicate is not an error for
/// us: a reconciler that insists on creating would fail every pass after the first.
const ALREADY_EXISTS_CODE: i64 = 81_053;

/// Real Cloudflare REST client. `HIVE_CLOUDFLARE_API_TOKEN` (never logged).
pub struct CloudflareApi {
    http: reqwest::Client,
    token: String,
    account_id: Option<String>,
    /// Explicit `HIVE_CLOUDFLARE_ZONE_ID`. Used only for names inside that zone.
    zone_pin: Option<String>,
    /// `zone name -> zone id`, including negatives (`None`) so a miss is not
    /// re-queried on every record of every pass.
    zones: RwLock<HashMap<String, Option<String>>>,
}

impl CloudflareApi {
    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let token = std::env::var("HIVE_CLOUDFLARE_API_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())?;
        let account_id = std::env::var("HIVE_CLOUDFLARE_ACCOUNT_ID")
            .ok()
            .filter(|s| !s.is_empty());
        let zone_pin = std::env::var("HIVE_CLOUDFLARE_ZONE_ID")
            .ok()
            .filter(|s| !s.is_empty());
        Some(Self {
            http,
            token,
            account_id,
            zone_pin,
            zones: RwLock::new(HashMap::new()),
        })
    }

    fn cached(&self, name: &str) -> Option<Option<String>> {
        self.zones.read().ok()?.get(name).cloned()
    }

    fn remember(&self, name: &str, id: Option<String>) {
        if let Ok(mut m) = self.zones.write() {
            m.insert(name.to_string(), id);
        }
    }

    /// The zone id owning `fqdn`, resolved longest suffix first (`example.com` is a zone,
    /// `a.b.example.com` a name inside it, not a zone of its own). `Ok(None)` when no Cloudflare
    /// zone owns the name, so the caller falls back to Vercel rather than failing an order it
    /// cannot write for.
    pub async fn zone_for(&self, fqdn: &str) -> anyhow::Result<Option<String>> {
        let fqdn = fqdn.trim().trim_end_matches('.').to_ascii_lowercase();
        if fqdn.is_empty() {
            return Ok(None);
        }
        if let Some(pin) = &self.zone_pin {
            let name = self.zone_name(pin).await?;
            if let Some(n) = name {
                if under(&fqdn, &n) {
                    return Ok(Some(pin.clone()));
                }
            }
        }
        for cand in suffixes(&fqdn) {
            let id = match self.cached(&cand) {
                Some(v) => v,
                None => {
                    let v = self.lookup_zone(&cand).await?;
                    self.remember(&cand, v.clone());
                    v
                }
            };
            if let Some(id) = id {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// The name of a pinned zone id. `None` when the pin cannot be read, which degrades the pin to
    /// ordinary name resolution.
    async fn zone_name(&self, zone_id: &str) -> anyhow::Result<Option<String>> {
        if let Some(v) = self.cached(zone_id) {
            return Ok(v);
        }
        let resp = self
            .http
            .get(format!("{API}/zones/{zone_id}"))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let v: serde_json::Value = resp.json().await?;
        let name = v
            .get("result")
            .and_then(|r| r.get("name"))
            .and_then(|n| n.as_str())
            .map(|n| n.to_ascii_lowercase());
        self.remember(zone_id, name.clone());
        Ok(name)
    }

    async fn lookup_zone(&self, name: &str) -> anyhow::Result<Option<String>> {
        let mut q: Vec<(&str, String)> = vec![("name", name.to_string()), ("status", "active".into())];
        if let Some(a) = &self.account_id {
            q.push(("account.id", a.clone()));
        }
        let resp = self
            .http
            .get(format!("{API}/zones"))
            .query(&q)
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!(
                "cloudflare zone lookup {name}: {status}{}",
                if retryable(status) { " (retryable)" } else { "" }
            );
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v.get("result")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .and_then(|z| z.get("id"))
            .and_then(|i| i.as_str())
            .map(str::to_string))
    }

    /// Every record of `rtype` named exactly `fqdn` (a name may legitimately hold
    /// several values — one A per node, one TXT per ACME order).
    pub async fn find_all(
        &self,
        zone_id: &str,
        fqdn: &str,
        rtype: &str,
    ) -> anyhow::Result<Vec<CfRecord>> {
        let resp = self
            .http
            .get(format!("{API}/zones/{zone_id}/dns_records"))
            .query(&[
                ("type", rtype.to_string()),
                ("name", fqdn.to_ascii_lowercase()),
                ("per_page", "100".into()),
            ])
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!(
                "cloudflare find {fqdn} {rtype}: {status}{}",
                if retryable(status) { " (retryable)" } else { "" }
            );
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v.get("result")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(parse_record).collect())
            .unwrap_or_default())
    }

    async fn post(
        &self,
        zone_id: &str,
        fqdn: &str,
        rtype: &str,
        content: &str,
        ttl: u32,
    ) -> anyhow::Result<String> {
        let resp = self
            .http
            .post(format!("{API}/zones/{zone_id}/dns_records"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "type": rtype,
                "name": fqdn,
                "content": content,
                "ttl": ttl,
                "proxied": false,
            }))
            .send()
            .await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!(
                "cloudflare create {fqdn} {rtype}: {status} {}",
                cf_error(&v)
            );
        }
        Ok(v.get("result")
            .and_then(|r| r.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or_default()
            .to_string())
    }

    async fn patch(
        &self,
        zone_id: &str,
        rec_id: &str,
        fqdn: &str,
        rtype: &str,
        content: &str,
        ttl: u32,
    ) -> anyhow::Result<()> {
        let resp = self
            .http
            .patch(format!("{API}/zones/{zone_id}/dns_records/{rec_id}"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "type": rtype,
                "name": fqdn,
                "content": content,
                "ttl": ttl,
                "proxied": false,
            }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            anyhow::bail!(
                "cloudflare patch {fqdn} {rtype}: {status} {}",
                cf_error(&v)
            );
        }
        Ok(())
    }

    async fn remove(&self, zone_id: &str, rec_id: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(format!("{API}/zones/{zone_id}/dns_records/{rec_id}"))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("cloudflare delete {rec_id}: {status}");
        }
        Ok(())
    }
}

/// A record as Cloudflare reports it.
#[derive(Clone, Debug)]
pub struct CfRecord {
    pub id: String,
    pub content: String,
    pub proxied: bool,
    pub ttl: u32,
}

fn parse_record(v: &serde_json::Value) -> Option<CfRecord> {
    Some(CfRecord {
        id: v.get("id")?.as_str()?.to_string(),
        content: v.get("content")?.as_str()?.to_string(),
        proxied: v.get("proxied")?.as_bool().unwrap_or(false),
        ttl: v.get("ttl")?.as_u64().unwrap_or(1) as u32,
    })
}

/// Cloudflare's `errors[]` as one line. Codes only — never the request, which
/// carries the bearer token.
fn cf_error(v: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(arr) = v.get("errors").and_then(|e| e.as_array()) {
        for e in arr {
            let code = e.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
            out.push_str(&format!("[{code} {msg}]"));
        }
    }
    if out.is_empty() {
        out.push_str("(no error detail)");
    }
    out
}

fn retryable(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}

/// `true` when `fqdn` is `zone` itself or any name beneath it.
fn under(fqdn: &str, zone: &str) -> bool {
    fqdn == zone || fqdn.ends_with(&format!(".{zone}"))
}

/// Candidate zone names, longest first, never shorter than a registrable domain:
/// `storage.dev.autheo.com` → itself, `dev.autheo.com`, `autheo.com`.
fn suffixes(fqdn: &str) -> Vec<String> {
    let parts: Vec<&str> = fqdn.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return Vec::new();
    }
    (0..=parts.len() - 2)
        .map(|i| parts[i..].join("."))
        .collect()
}

/// A [`DnsApi`] record name is relative to the managed domain; Cloudflare wants
/// the name inside its zone.
fn qualify(domain: &str, name: &str) -> String {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() || name == domain {
        domain
    } else {
        format!("{name}.{domain}")
    }
}

impl DnsApi for CloudflareApi {
    /// Lists the zone's records under `domain`, named relative to it. A domain no Cloudflare zone
    /// owns lists nothing rather than failing, so a Vercel-owned name never aborts a pass that also
    /// covers Cloudflare names. Only names under `domain` are returned: one Cloudflare zone holds
    /// every name in its domain, and a foreign name is never ours to reconcile. `created_ms` is
    /// `None` because Cloudflare reports RFC3339 `created_on` while the ACME orphan sweeper's age
    /// gate wants ms epoch and no ms field exists to read.
    async fn list(&self, domain: &str) -> anyhow::Result<Vec<RecordView>> {
        let zone = match self
            .zone_for(domain)
            .await?
        {
            Some(z) => z,
            None => return Ok(Vec::new()),
        };
        let suffix = format!(
            ".{}",
            domain.trim().trim_end_matches('.').to_ascii_lowercase()
        );
        let apex = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        let mut out: Vec<RecordView> = Vec::new();
        for page in 1..=50u32 {
            let resp = self
                .http
                .get(format!("{API}/zones/{zone}/dns_records"))
                .query(&[("per_page", "100".to_string()), ("page", page.to_string())])
                .bearer_auth(&self.token)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                anyhow::bail!(
                    "cloudflare list {domain}: {status}{}",
                    if retryable(status) { " (retryable)" } else { "" }
                );
            }
            let v: serde_json::Value = resp.json().await?;
            let page_rows: Vec<RecordView> = v
                .get("result")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|r| {
                            let name = r.get("name")?.as_str()?.to_ascii_lowercase();
                            let rel = if name == apex {
                                String::new()
                            } else {
                                name.strip_suffix(&suffix)?.to_string()
                            };
                            Some(RecordView {
                                id: r.get("id")?.as_str()?.to_string(),
                                name: rel,
                                rtype: r.get("type")?.as_str()?.to_string(),
                                value: r.get("content")?.as_str()?.to_string(),
                                created_ms: None,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let empty = page_rows.is_empty();
            out.extend(page_rows);
            let pages = v
                .get("result_info")
                .and_then(|r| r.get("total_pages"))
                .and_then(|t| t.as_u64())
                .unwrap_or(1);
            if empty || page as u64 >= pages {
                break;
            }
        }
        Ok(out)
    }

    /// Creates a record, idempotently: one already carrying this exact value is returned as is,
    /// instead of stacking duplicates every pass, and is re-patched when it is proxied or on the
    /// wrong TTL (a gateway record in that state is broken exactly the way this provider exists to
    /// prevent). When the create fails and a same-name, same-type record exists, the FIRST such
    /// record is overwritten with the new value rather than accumulating (a stale challenge, a node
    /// IP that moved); that fallback is taken on ANY create failure, the "already exists" code does
    /// not gate it.
    async fn create(&self, domain: &str, rec: &DesiredRecord) -> anyhow::Result<String> {
        let fqdn = qualify(domain, &rec.name);
        let zone = self
            .zone_for(&fqdn)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no Cloudflare zone owns {fqdn}"))?;
        let existing = self.find_all(&zone, &fqdn, &rec.rtype).await?;
        if let Some(r) = existing.iter().find(|r| r.content == rec.value) {
            if r.proxied || r.ttl != rec.ttl {
                self.patch(&zone, &r.id, &fqdn, &rec.rtype, &rec.value, rec.ttl)
                    .await?;
            }
            return Ok(r.id.clone());
        }
        match self.post(&zone, &fqdn, &rec.rtype, &rec.value, rec.ttl).await {
            Ok(id) => Ok(id),
            Err(e) => {
                let dup = e.to_string().contains(&format!("[{ALREADY_EXISTS_CODE}"));
                let first = existing.first().cloned();
                match (dup, first) {
                    (_, Some(r)) => {
                        self.patch(&zone, &r.id, &fqdn, &rec.rtype, &rec.value, rec.ttl)
                            .await?;
                        Ok(r.id)
                    }
                    (_, None) => Err(e),
                }
            }
        }
    }

    async fn delete(&self, domain: &str, id: &str) -> anyhow::Result<()> {
        let zone = self
            .zone_for(domain)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no Cloudflare zone owns {domain}"))?;
        self.remove(&zone, id).await
    }
}

/// Which authoritative DNS API a node writes through.
pub enum DnsProvider {
    Vercel(crate::vercel_dns::VercelApi),
    Cloudflare(CloudflareApi),
}

impl DnsProvider {
    /// Vercel when `VERCEL_API_TOKEN` is present (every existing deployment), else Cloudflare.
    /// `HIVE_ACME_DNS_PROVIDER=cloudflare` forces Cloudflare even when a Vercel token exists, for a
    /// domain that only Cloudflare serves.
    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let force_cf = std::env::var("HIVE_ACME_DNS_PROVIDER")
            .map(|v| v.eq_ignore_ascii_case("cloudflare"))
            .unwrap_or(false);
        if !force_cf {
            if let Some(v) = crate::vercel_dns::VercelApi::from_env(http.clone()) {
                return Some(Self::Vercel(v));
            }
        }
        CloudflareApi::from_env(http).map(Self::Cloudflare)
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Vercel(_) => "vercel",
            Self::Cloudflare(_) => "cloudflare",
        }
    }
}

impl DnsApi for DnsProvider {
    async fn list(&self, domain: &str) -> anyhow::Result<Vec<RecordView>> {
        match self {
            Self::Vercel(v) => v.list(domain).await,
            Self::Cloudflare(c) => c.list(domain).await,
        }
    }
    async fn create(&self, domain: &str, rec: &DesiredRecord) -> anyhow::Result<String> {
        match self {
            Self::Vercel(v) => v.create(domain, rec).await,
            Self::Cloudflare(c) => c.create(domain, rec).await,
        }
    }
    async fn delete(&self, domain: &str, id: &str) -> anyhow::Result<()> {
        match self {
            Self::Vercel(v) => v.delete(domain, id).await,
            Self::Cloudflare(c) => c.delete(domain, id).await,
        }
    }
}

/// Publishes the DB-gateway zone through Cloudflare: apex and wildcard (DNS-only) to every
/// publishable address, plus one record per database slug hosted on THIS node (`db_gateway` splices
/// with no forward hop, so a slug must point at the node holding the container; slugs of databases
/// on other nodes are not published here). Wanted records are kept per name as `(A values, AAAA
/// values)`, `""` being the apex and `"*"` the wildcard. A managed name that wants no address keeps
/// its stale ones rather than going dark, as in the Vercel path. Returns how many records were
/// written or removed.
///
/// Gated by `HIVE_DB_DNS_PROVIDER=cloudflare`: the Vercel path in `vercel_dns` stays the default,
/// so a fleet whose db domain IS a Vercel zone is untouched.
pub async fn publish_db_zone(
    cloud: &Arc<crate::state::CloudState>,
    cf: &CloudflareApi,
) -> anyhow::Result<usize> {
    let db_domain = cloud.db_domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if db_domain.is_empty() {
        return Ok(0);
    }
    if cf.zone_for(&db_domain).await?.is_none() {
        anyhow::bail!("db domain {db_domain} is not in a Cloudflare zone");
    }
    let nodes = cloud.registry.nodes();
    let me = nodes.iter().find(|n| n.name == cloud.node_name);

    let mut want: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    let apex = (
        nodes
            .iter()
            .filter(|n| n.healthy)
            .filter_map(|n| n.public_ip.clone())
            .collect::<Vec<String>>(),
        nodes
            .iter()
            .filter(|n| n.healthy)
            .filter_map(|n| n.public_ip6.clone())
            .collect::<Vec<String>>(),
    );
    want.insert(String::new(), apex.clone());
    want.insert("*".into(), apex);
    let suffix = format!(".{db_domain}");
    if let Some(me) = me {
        for d in cloud.databases.list(None) {
            if d.host_node != cloud.node_name || d.db_host.is_empty() {
                continue;
            }
            let host = d.db_host.trim().to_ascii_lowercase();
            let slug = match host.strip_suffix(&suffix) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            let e = want.entry(slug).or_default();
            if let Some(ip) = &me.public_ip {
                if !e.0.contains(ip) {
                    e.0.push(ip.clone());
                }
            }
            if let Some(ip) = &me.public_ip6 {
                if !e.1.contains(ip) {
                    e.1.push(ip.clone());
                }
            }
        }
    }

    let existing = crate::vercel_dns::DnsApi::list(cf, &db_domain).await?;
    let mut written = 0usize;
    for (name, (v4, v6)) in &want {
        for ip in v4 {
            if !existing
                .iter()
                .any(|r| r.name == *name && r.rtype == "A" && r.value == *ip)
            {
                DnsApi::create(
                    cf,
                    &db_domain,
                    &DesiredRecord {
                        name: name.clone(),
                        rtype: "A".into(),
                        value: ip.clone(),
                        ttl: DEFAULT_TTL,
                    },
                )
                .await?;
                written += 1;
            }
        }
        for ip in v6 {
            if !existing
                .iter()
                .any(|r| r.name == *name && r.rtype == "AAAA" && r.value == *ip)
            {
                DnsApi::create(
                    cf,
                    &db_domain,
                    &DesiredRecord {
                        name: name.clone(),
                        rtype: "AAAA".into(),
                        value: ip.clone(),
                        ttl: DEFAULT_TTL,
                    },
                )
                .await?;
                written += 1;
            }
        }
        if v4.is_empty() && v6.is_empty() {
            continue;
        }
        for r in existing
            .iter()
            .filter(|r| r.name == *name && matches!(r.rtype.as_str(), "A" | "AAAA"))
        {
            let wanted = if r.rtype == "A" { v4 } else { v6 };
            if !wanted.iter().any(|v| *v == r.value) {
                let _ = DnsApi::delete(cf, &db_domain, &r.id).await;
                written += 1;
            }
        }
    }
    Ok(written)
}

/// Leader-only reconcile loop (`may_act(DnsWrite)`) for a Cloudflare-served DB-gateway zone. Off
/// unless `HIVE_DB_DNS_PROVIDER=cloudflare` AND `HIVE_CLOUDFLARE_API_TOKEN` is set, so every node
/// whose db domain is a Vercel zone keeps using `vercel_dns`, unaffected.
pub fn spawn_db_dns(cloud: Arc<crate::state::CloudState>) {
    if !std::env::var("HIVE_DB_DNS_PROVIDER")
        .map(|v| v.eq_ignore_ascii_case("cloudflare"))
        .unwrap_or(false)
    {
        return;
    }
    if cloud.db_domain.is_empty() {
        return;
    }
    let Some(cf) = CloudflareApi::from_env(cloud.http.clone()) else {
        tracing::warn!("HIVE_DB_DNS_PROVIDER=cloudflare but HIVE_CLOUDFLARE_API_TOKEN is not set — DB DNS not published");
        return;
    };
    let interval = std::env::var("HIVE_DNS_RECONCILE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60u64)
        .max(30);
    tracing::info!(domain = %cloud.db_domain, interval, "Cloudflare DB-gateway DNS publisher up (leader-elected)");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
        loop {
            tick.tick().await;
            if !crate::leadership::may_act(&cloud, crate::leadership::Job::DnsWrite) {
                continue;
            }
            match publish_db_zone(&cloud, &cf).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(written = n, domain = %cloud.db_domain, "DB-gateway DNS published via Cloudflare"),
                Err(e) => tracing::warn!(error = %e, domain = %cloud.db_domain, "Cloudflare DB-gateway DNS publish failed (retry next pass)"),
            }
        }
    });
}
