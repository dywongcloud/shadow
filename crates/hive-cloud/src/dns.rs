//! Domain management — DNS records, nameservers, and a free auto-issued SSL
//! certificate per domain. Scoped per tenant; persisted + audited.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

const YEAR_MS: u64 = 365 * 24 * 60 * 60 * 1000;
const CERT_TTL_MS: u64 = 90 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    /// Subdomain/host (e.g. "", "www", "*"). Empty = apex.
    pub name: String,
    /// A | AAAA | CNAME | MX | TXT | CAA | NS | SRV | ALIAS
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    #[serde(default)]
    pub priority: Option<u32>,
    #[serde(default)]
    pub comment: String,
    pub created_ms: u64,
    /// True for records the platform manages (shown locked in the UI).
    #[serde(default)]
    pub system: bool,
}
fn default_ttl() -> u32 {
    60
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SslCert {
    pub id: String,
    pub cns: Vec<String>,
    pub renewal: String, // "auto"
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub provider: String, // "OpenEdge (Let's Encrypt)"
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DomainRecord {
    pub domain: String,
    pub tenant: String,
    pub registrar: String,
    pub renewal_price: String,
    pub auto_renew: bool,
    pub cdn_active: bool,
    pub created_ms: u64,
    pub expires_ms: u64,
    pub nameservers: Vec<String>,
    pub ssl: SslCert,
    pub records: Vec<DnsRecord>,
}

#[derive(Default)]
pub struct DomainStore {
    map: RwLock<HashMap<String, DomainRecord>>,
}

fn default_domain(domain: &str, tenant: &str) -> DomainRecord {
    let now = now_ms();
    DomainRecord {
        domain: domain.to_string(),
        tenant: tenant.to_string(),
        registrar: "OpenEdge".into(),
        renewal_price: "$13 per year".into(),
        auto_renew: true,
        cdn_active: true,
        created_ms: now,
        expires_ms: now + YEAR_MS,
        nameservers: vec!["ns1.openedge-dns.com".into(), "ns2.openedge-dns.com".into()],
        ssl: SslCert {
            id: format!("cert_{}", &Uuid::new_v4().simple().to_string()[..20]),
            cns: vec![format!("*.{domain}"), domain.to_string()],
            renewal: "auto".into(),
            issued_ms: now,
            expires_ms: now + CERT_TTL_MS,
            provider: "OpenEdge (Let's Encrypt)".into(),
        },
        // A free managed cert needs CAA records authorizing the issuer.
        records: vec![
            DnsRecord { id: rec_id(), name: String::new(), kind: "CAA".into(), value: "0 issue \"letsencrypt.org\"".into(), ttl: 60, priority: None, comment: "Managed SSL issuer".into(), created_ms: now, system: true },
            DnsRecord { id: rec_id(), name: String::new(), kind: "A".into(), value: "76.76.21.21".into(), ttl: 60, priority: None, comment: "OpenEdge anycast".into(), created_ms: now, system: true },
        ],
    }
}

fn rec_id() -> String {
    format!("rec_{}", &Uuid::new_v4().simple().to_string()[..16])
}

impl DomainStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the domain record, creating a sensible default (free SSL, NS, CAA) if
    /// this is the first time we've seen it for this tenant.
    pub fn ensure(&self, domain: &str, tenant: &str) -> DomainRecord {
        let mut m = self.map.write();
        m.entry(domain.to_string()).or_insert_with(|| default_domain(domain, tenant)).clone()
    }

    pub fn get(&self, domain: &str) -> Option<DomainRecord> {
        self.map.read().get(domain).cloned()
    }

    pub fn add_record(&self, domain: &str, mut rec: DnsRecord) -> Option<DnsRecord> {
        let mut m = self.map.write();
        let d = m.get_mut(domain)?;
        rec.id = rec_id();
        rec.created_ms = now_ms();
        rec.system = false;
        d.records.push(rec.clone());
        Some(rec)
    }

    pub fn delete_record(&self, domain: &str, id: &str) -> bool {
        let mut m = self.map.write();
        let Some(d) = m.get_mut(domain) else { return false };
        let before = d.records.len();
        d.records.retain(|r| r.id != id || r.system);
        d.records.len() != before
    }

    /// Edit a non-system record's mutable fields. Returns the updated record.
    #[allow(clippy::too_many_arguments)]
    pub fn update_record(
        &self,
        domain: &str,
        id: &str,
        name: String,
        kind: String,
        value: String,
        ttl: u32,
        priority: Option<u32>,
        comment: String,
    ) -> Option<DnsRecord> {
        let mut m = self.map.write();
        let d = m.get_mut(domain)?;
        let r = d.records.iter_mut().find(|r| r.id == id && !r.system)?;
        r.name = name;
        r.kind = kind.to_uppercase();
        r.value = value;
        r.ttl = ttl;
        r.priority = priority;
        r.comment = comment;
        Some(r.clone())
    }

    /// Bulk-import records (DNS migration). Skips exact duplicates (same
    /// type + name + value) so re-importing is idempotent. Returns the records
    /// actually added.
    pub fn import_records(&self, domain: &str, recs: Vec<DnsRecord>) -> Vec<DnsRecord> {
        let mut m = self.map.write();
        let Some(d) = m.get_mut(domain) else { return Vec::new() };
        let mut added = Vec::new();
        for mut rec in recs {
            let kind = rec.kind.to_uppercase();
            if kind.is_empty() || rec.value.trim().is_empty() {
                continue;
            }
            let dup = d
                .records
                .iter()
                .any(|e| e.kind.eq_ignore_ascii_case(&kind) && e.name == rec.name && e.value == rec.value);
            if dup {
                continue;
            }
            rec.id = rec_id();
            rec.created_ms = now_ms();
            rec.system = false;
            rec.kind = kind;
            d.records.push(rec.clone());
            added.push(rec);
        }
        added
    }

    pub fn set_nameservers(&self, domain: &str, ns: Vec<String>) -> bool {
        let mut m = self.map.write();
        let Some(d) = m.get_mut(domain) else { return false };
        d.nameservers = ns;
        true
    }

    pub fn set_auto_renew(&self, domain: &str, on: bool) -> bool {
        let mut m = self.map.write();
        let Some(d) = m.get_mut(domain) else { return false };
        d.auto_renew = on;
        true
    }

    /// Reissue the free managed certificate (90-day Let's Encrypt-style).
    pub fn renew_ssl(&self, domain: &str) -> Option<SslCert> {
        let mut m = self.map.write();
        let d = m.get_mut(domain)?;
        let now = now_ms();
        d.ssl.issued_ms = now;
        d.ssl.expires_ms = now + CERT_TTL_MS;
        d.ssl.id = format!("cert_{}", &Uuid::new_v4().simple().to_string()[..20]);
        Some(d.ssl.clone())
    }

    pub fn snapshot(&self) -> Vec<DomainRecord> {
        self.map.read().values().cloned().collect()
    }
    pub fn load(&self, list: Vec<DomainRecord>) {
        *self.map.write() = list.into_iter().map(|d| (d.domain.clone(), d)).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, name: &str, value: &str) -> DnsRecord {
        DnsRecord { id: String::new(), name: name.into(), kind: kind.into(), value: value.into(), ttl: 3600, priority: None, comment: String::new(), created_ms: 0, system: false }
    }

    #[test]
    fn ensure_creates_default_and_is_idempotent() {
        let s = DomainStore::new();
        let d = s.ensure("acme.com", "personal");
        assert_eq!(d.domain, "acme.com");
        assert_eq!(d.tenant, "personal");
        // Default system records: a CAA (managed-SSL issuer) + the anycast A.
        assert!(d.records.iter().any(|r| r.kind == "CAA" && r.system));
        assert!(d.records.iter().any(|r| r.kind == "A" && r.system));
        let n = d.records.len();
        // ensure again must NOT recreate / duplicate.
        let d2 = s.ensure("acme.com", "personal");
        assert_eq!(d2.records.len(), n, "ensure is idempotent");
    }

    #[test]
    fn add_record_assigns_id_and_marks_non_system() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        let added = s.add_record("acme.com", rec("A", "www", "1.2.3.4")).expect("added");
        assert!(added.id.starts_with("rec_"));
        assert!(!added.system);
        let got = s.get("acme.com").unwrap();
        assert!(got.records.iter().any(|r| r.id == added.id && r.value == "1.2.3.4"));
    }

    #[test]
    fn system_records_are_immutable_and_undeletable() {
        let s = DomainStore::new();
        let d = s.ensure("acme.com", "t");
        let sys = d.records.iter().find(|r| r.system).unwrap().id.clone();
        // update of a system record returns None.
        assert!(s.update_record("acme.com", &sys, "@".into(), "A".into(), "9.9.9.9".into(), 60, None, String::new()).is_none());
        // delete of a system record is refused; it stays.
        assert!(!s.delete_record("acme.com", &sys));
        assert!(s.get("acme.com").unwrap().records.iter().any(|r| r.id == sys));
    }

    #[test]
    fn update_and_delete_user_record() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        let id = s.add_record("acme.com", rec("A", "www", "1.1.1.1")).unwrap().id;
        let up = s.update_record("acme.com", &id, "www".into(), "A".into(), "2.2.2.2".into(), 120, None, "edited".into()).unwrap();
        assert_eq!(up.value, "2.2.2.2");
        assert_eq!(up.ttl, 120);
        assert!(s.delete_record("acme.com", &id));
        assert!(!s.get("acme.com").unwrap().records.iter().any(|r| r.id == id));
    }

    #[test]
    fn import_is_idempotent_skips_exact_duplicates() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        let batch = vec![rec("A", "www", "1.2.3.4"), rec("MX", "", "mail.acme.com")];
        let first = s.import_records("acme.com", batch.clone());
        assert_eq!(first.len(), 2, "both imported");
        let second = s.import_records("acme.com", batch);
        assert_eq!(second.len(), 0, "exact duplicates are skipped");
    }

    #[test]
    fn nameservers_autorenew_and_ssl_renew() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        assert!(s.set_nameservers("acme.com", vec!["ns1.shadw.cloud".into(), "ns2.shadw.cloud".into()]));
        assert_eq!(s.get("acme.com").unwrap().nameservers, vec!["ns1.shadw.cloud", "ns2.shadw.cloud"]);
        assert!(s.set_auto_renew("acme.com", false));
        assert!(!s.get("acme.com").unwrap().auto_renew);
        let before = s.get("acme.com").unwrap().ssl.id;
        let cert = s.renew_ssl("acme.com").unwrap();
        assert_ne!(cert.id, before, "renew reissues a new cert id");
    }

    #[test]
    fn snapshot_load_roundtrip() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        s.add_record("acme.com", rec("TXT", "@", "v=spf1 ~all"));
        let snap = s.snapshot();
        let s2 = DomainStore::new();
        s2.load(snap);
        assert!(s2.get("acme.com").unwrap().records.iter().any(|r| r.kind == "TXT"));
    }

    #[test]
    fn ops_on_unknown_domain_are_safe() {
        let s = DomainStore::new();
        assert!(s.get("nope.com").is_none());
        assert!(s.add_record("nope.com", rec("A", "@", "1.1.1.1")).is_none());
        assert!(!s.delete_record("nope.com", "rec_x"));
        assert!(!s.set_nameservers("nope.com", vec![]));
        assert!(s.renew_ssl("nope.com").is_none());
    }
}
