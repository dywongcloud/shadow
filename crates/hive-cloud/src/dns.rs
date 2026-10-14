//! Domain management — DNS records, nameservers, and a free auto-issued SSL
//! certificate per domain. Scoped per tenant; persisted + audited.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

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
    /// `"auto"` once a real bundle is installed, `"pending"` before.
    pub renewal: String,
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub provider: String,
}

/// One domain the platform serves DNS for. It is NOT a registrar: the registration facts
/// (`registrar`, `renewal_price`, `auto_renew`, `expires_ms`) are never measured, so they are
/// always empty, `false` and `0` (unknown), and no client may show them. The keys stay on the wire
/// only because a node on an earlier build deserializes `state.json` and replicated snapshots
/// with them REQUIRED, and a failed parse there resets that node's whole platform state: emptying
/// a key is safe across a roll or rollback, removing it is not. [`DomainStore::load`] scrubs
/// records written when `default_domain` still invented them.
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
    /// Custom-domain ownership proof. `None` for DNS-only records and for attachments that predate
    /// verification (grandfathered: they keep routing, only NEW attaches are gated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<DomainVerify>,
}

/// TXT-challenge ownership proof for a custom-domain attach. The alias activates only once the
/// applying node itself observes `txt_value` in a `_hive-verify.<domain>` TXT answer (DoH,
/// re-checked at activation, never trusted from a caller).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DomainVerify {
    /// `"pending"` until the TXT is observed, then `"verified"`; `"failed"` is reserved for probe
    /// errors, never for "not yet propagated".
    pub status: String,
    pub txt_name: String,
    pub txt_value: String,
    /// The project this verification gates (the attach target).
    pub project: String,
    pub created_ms: u64,
    pub checked_ms: u64,
    pub verified_ms: u64,
    /// Latest probe outcome for the UI ("no TXT answer yet", "NXDOMAIN",
    /// "resolver error"), honest about what was seen — never a fake error.
    pub last_probe: String,
}

#[derive(Default)]
pub struct DomainStore {
    map: RwLock<HashMap<String, DomainRecord>>,
}

/// The honest SSL block of a record with no installed bundle. A record never claims a certificate
/// at creation: the dashboard would show "Managed SSL active" for a domain with no bundle on any
/// edge, and the HTTP-01-only custom path can issue only apex+`www` SANs.
/// [`DomainStore::set_ssl_issued`] fills it from the REAL bundle when `custom_cert_pass` installs
/// one.
fn pending_ssl() -> SslCert {
    SslCert {
        id: String::new(),
        cns: vec![],
        renewal: "pending".into(),
        issued_ms: 0,
        expires_ms: 0,
        provider: "OpenEdge (Let's Encrypt)".into(),
    }
}

/// A fresh record: platform defaults, a pending SSL block and only the issuer CAA that a managed
/// certificate needs. The apex A/AAAA set is deliberately NOT seeded (a fake address would resolve
/// the domain somewhere that does not serve it); [`DomainStore::set_system_address_records`] pins
/// the real edge addresses when an attachment verifies.
fn default_domain(domain: &str, tenant: &str) -> DomainRecord {
    let now = now_ms();
    DomainRecord {
        domain: domain.to_string(),
        tenant: tenant.to_string(),
        registrar: String::new(),
        renewal_price: String::new(),
        auto_renew: false,
        cdn_active: true,
        created_ms: now,
        expires_ms: 0,
        nameservers: vec!["ns1.openedge-dns.com".into(), "ns2.openedge-dns.com".into()],
        verify: None,
        ssl: pending_ssl(),
        records: vec![DnsRecord {
            id: rec_id(),
            name: String::new(),
            kind: "CAA".into(),
            value: "0 issue \"letsencrypt.org\"".into(),
            ttl: 60,
            priority: None,
            comment: "Managed SSL issuer".into(),
            created_ms: now,
            system: true,
        }],
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
        m.entry(domain.to_string())
            .or_insert_with(|| default_domain(domain, tenant))
            .clone()
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
        let Some(d) = m.get_mut(domain) else {
            return false;
        };
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

    /// Bulk-imports records (DNS migration), skipping exact duplicates (type + name + value) so a
    /// re-import is idempotent. Returns the records actually added.
    pub fn import_records(&self, domain: &str, recs: Vec<DnsRecord>) -> Vec<DnsRecord> {
        let mut m = self.map.write();
        let Some(d) = m.get_mut(domain) else {
            return Vec::new();
        };
        let mut added = Vec::new();
        for mut rec in recs {
            let kind = rec.kind.to_uppercase();
            if kind.is_empty() || rec.value.trim().is_empty() {
                continue;
            }
            let dup = d.records.iter().any(|e| {
                e.kind.eq_ignore_ascii_case(&kind) && e.name == rec.name && e.value == rec.value
            });
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
        let Some(d) = m.get_mut(domain) else {
            return false;
        };
        d.nameservers = ns;
        true
    }

    /// Records the REAL installed bundle on the domain's ssl block: the only writer allowed to
    /// claim an issued cert (the creation default and [`Self::renew_ssl`] leave it pending). Called
    /// from acme's custom-domain issuance/adoption with the bundle's actual SANs and expiry.
    /// Returns `true` only when the block changed: an unattached domain, a non-`dom-*` bundle, an
    /// implausible validity window, SANs outside apex+`www` and an unchanged block all return
    /// `false`.
    pub fn set_ssl_issued(
        &self,
        domain: &str,
        bundle_id: &str,
        sans: Vec<String>,
        issued_ms: u64,
        expires_ms: u64,
    ) -> bool {
        let mut m = self.map.write();
        let Some(d) = m.get_mut(domain) else {
            return false;
        };
        let apex = d.domain.trim().trim_end_matches('.').to_ascii_lowercase();
        let www = format!("www.{apex}");
        let attached = d
            .verify
            .as_ref()
            .is_some_and(|verify| verify.status == "verified");
        if !attached
            || !bundle_id.starts_with("dom-")
            || issued_ms == 0
            || expires_ms <= issued_ms
            || !sans.iter().any(|n| n == &apex)
            || sans.iter().any(|n| n != &apex && n != &www)
        {
            return false;
        }
        if d.ssl.id == bundle_id
            && d.ssl.cns == sans
            && d.ssl.renewal == "auto"
            && d.ssl.issued_ms == issued_ms
            && d.ssl.expires_ms == expires_ms
        {
            return false;
        }
        d.ssl.id = bundle_id.to_string();
        d.ssl.cns = sans;
        d.ssl.renewal = "auto".into();
        d.ssl.issued_ms = issued_ms;
        d.ssl.expires_ms = expires_ms;
        true
    }

    /// Backs the UI's "reissue certificate" button and never fabricates an issued state: the real
    /// issuance is `acme::custom_cert_pass`, which the HTTP arm kicks after this call. Returns the
    /// CURRENT block.
    pub fn renew_ssl(&self, domain: &str) -> Option<SslCert> {
        self.get(domain).map(|d| d.ssl)
    }

    pub fn snapshot(&self) -> Vec<DomainRecord> {
        self.map.read().values().cloned().collect()
    }
    /// Adopts a snapshot. Any ssl block that is not a real HTTP-01 custom bundle of an attached
    /// domain (`dom-*` id, `auto` renewal, a valid window, apex plus optional `www` SANs) is reset
    /// to pending: records created before the honest default carried fabricated `cert_*` ids and
    /// wildcard CNs. A valid block survives restarts because it records a bundle actually installed
    /// by `custom_cert_pass`, not this node's current SNI cache.
    pub fn load(&self, mut list: Vec<DomainRecord>) {
        for d in &mut list {
            // Records written (here or by a peer on an earlier build) when `default_domain` still
            // seeded "OpenEdge", "$13 per year", auto_renew=true and a one-year expiry carry values
            // nobody measured: scrubbed on every load, boot and snapshot adoption alike.
            d.registrar.clear();
            d.renewal_price.clear();
            d.auto_renew = false;
            d.expires_ms = 0;
            let domain = d.domain.trim().trim_end_matches('.').to_ascii_lowercase();
            let www = format!("www.{domain}");
            let attached = d
                .verify
                .as_ref()
                .is_some_and(|verify| verify.status == "verified");
            let valid = attached
                && d.ssl.id.starts_with("dom-")
                && d.ssl.renewal == "auto"
                && d.ssl.issued_ms > 0
                && d.ssl.expires_ms > d.ssl.issued_ms
                && d.ssl.cns.iter().any(|n| n == &domain)
                && d.ssl.cns.iter().all(|n| n == &domain || n == &www);
            if !valid {
                d.ssl = pending_ssl();
            }
        }
        *self.map.write() = list.into_iter().map(|d| (d.domain.clone(), d)).collect();
    }

    /// Creates (or refreshes, when the attach target changed) the verification challenge for a
    /// domain attach: an existing challenge for the SAME project is kept (rotating it would strand
    /// the TXT the user just pasted), a different project re-issues (the proof is per-attach).
    ///
    /// **Only a VERIFIED record locks to its tenant.** An unproven record (never verified, or a
    /// pending claim) confers no ownership: any tenant's attach takes it over with a fresh
    /// challenge and the tenant binding moves with it, so the attaching team can manage the zone
    /// while it proves. DNS control is the only proof, and a bare claim must never lock the real
    /// owner out of their own domain. A record verified for another project is the one settled
    /// state and is refused (`Err`).
    pub fn ensure_verify(
        &self,
        domain: &str,
        tenant: &str,
        project: &str,
    ) -> Result<DomainVerify, String> {
        let mut m = self.map.write();
        let rec = m
            .entry(domain.to_string())
            .or_insert_with(|| default_domain(domain, tenant));
        if let Some(v) = &rec.verify {
            if v.project == project && rec.tenant.eq_ignore_ascii_case(tenant) {
                return Ok(v.clone());
            }
            if v.status == "verified" {
                return Err(format!(
                    "'{domain}' is already verified for project '{}' — detach it there first",
                    v.project
                ));
            }
        }
        rec.tenant = tenant.to_string();
        let v = DomainVerify {
            status: "pending".into(),
            txt_name: format!("_hive-verify.{domain}"),
            txt_value: format!(
                "hive-verify={}",
                &uuid::Uuid::new_v4().simple().to_string()[..16]
            ),
            project: project.to_string(),
            created_ms: now_ms(),
            checked_ms: 0,
            verified_ms: 0,
            last_probe: String::new(),
        };
        rec.verify = Some(v.clone());
        Ok(v)
    }

    pub fn verify_of(&self, domain: &str) -> Option<DomainVerify> {
        self.map.read().get(domain).and_then(|d| d.verify.clone())
    }

    pub fn mark_verify_probe(&self, domain: &str, outcome: &str) {
        let mut m = self.map.write();
        if let Some(d) = m.get_mut(domain) {
            if let Some(v) = &mut d.verify {
                v.checked_ms = now_ms();
                v.last_probe = outcome.to_string();
            }
        }
    }

    /// Marks the attachment verified AND rebinds the record's tenant to the proving team: real
    /// proof supersedes the first-come claim, so a squatter who pre-created the record loses it
    /// exactly when someone proves control. An empty tenant means "unknown" (personal namespaces
    /// have no team row) and never rebinds the record to nothing.
    pub fn mark_verified_as(&self, domain: &str, tenant: Option<&str>) -> Option<DomainVerify> {
        let mut m = self.map.write();
        let d = m.get_mut(domain)?;
        if let Some(t) = tenant.filter(|t| !t.is_empty()) {
            d.tenant = t.to_string();
        }
        let v = d.verify.as_mut()?;
        v.status = "verified".into();
        v.verified_ms = now_ms();
        v.last_probe = "TXT matched".into();
        Some(v.clone())
    }

    /// Pending challenges older than this are abandoned: the watcher stops polling them and the
    /// claim lapses, so an unproven claim never holds a domain or its Let's Encrypt/watcher budget
    /// forever (a claim is a statement of intent, not a lease).
    pub const PENDING_VERIFY_TTL_MS: u64 = 7 * 24 * 3600 * 1000;

    /// Every record with a live pending verification, for the leader's
    /// watcher. Expired pendings are CLEARED here (the one place that sweeps
    /// them) so an abandoned attach never outlives its TTL.
    pub fn pending_verifications(&self) -> Vec<(String, DomainVerify)> {
        let now = now_ms();
        let mut m = self.map.write();
        let mut out = Vec::new();
        for d in m.values_mut() {
            let Some(v) = &d.verify else {
                continue;
            };
            if v.status != "pending" {
                continue;
            }
            if now.saturating_sub(v.created_ms) > Self::PENDING_VERIFY_TTL_MS {
                d.verify = None;
                continue;
            }
            out.push((d.domain.clone(), v.clone()));
        }
        out
    }

    /// Every VERIFIED attachment as (domain, project), for the watcher's address refresh and
    /// routing re-assert: the pinned apex set must track fleet membership, and a redeployed
    /// project's alias must be re-applied on its new host.
    pub fn verified_attachments(&self) -> Vec<(String, String)> {
        self.map
            .read()
            .values()
            .filter_map(|d| {
                d.verify
                    .as_ref()
                    .filter(|v| v.status == "verified")
                    .map(|v| (d.domain.clone(), v.project.clone()))
            })
            .collect()
    }

    /// Clears verification AND the issued-certificate claim (detach): a later re-attach proves
    /// again and obtains a fresh bundle, and a kept `dom-*` claim would make the API say issued
    /// once cert-sync prunes the detached bundle.
    pub fn clear_verify(&self, domain: &str) {
        let mut m = self.map.write();
        if let Some(d) = m.get_mut(domain) {
            d.verify = None;
            d.ssl = pending_ssl();
        }
    }

    /// Clears verification for every domain gated on `project` unless `preserved_domains` (another
    /// live project attachment) still lists it: a tombstoned project must not keep domains or TLS
    /// metadata alive, while a shared attachment must not lose the bundle it still serves.
    /// Verification and SSL reset under the same write lock, both halves or neither. Returns the
    /// affected domains.
    pub fn clear_verify_for_project(
        &self,
        project: &str,
        preserved_domains: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let mut m = self.map.write();
        let mut out = Vec::new();
        for d in m.values_mut() {
            if d.verify.as_ref().is_some_and(|v| v.project == project)
                && !preserved_domains.contains(&d.domain)
            {
                d.verify = None;
                d.ssl = pending_ssl();
                out.push(d.domain.clone());
            }
        }
        out
    }

    /// Renames the project a verified custom domain is attached to. Routing is keyed by the FULL
    /// hostname (`aliases_full`), never the project name, so a rename cannot disturb routing; but
    /// `verify.project` names the owner and every authorize/re-attach check compares against it, so
    /// it must move or the domain detaches from its project on the next verification compare.
    /// Returns the affected domains.
    pub fn rename_project(&self, from: &str, to: &str) -> Vec<String> {
        if from == to {
            return Vec::new();
        }
        let mut m = self.map.write();
        let mut out = Vec::new();
        for d in m.values_mut() {
            if d.verify.as_ref().is_some_and(|v| v.project == from) {
                if let Some(v) = d.verify.as_mut() {
                    v.project = to.to_string();
                }
                out.push(d.domain.clone());
            }
        }
        out
    }

    /// Replaces the zone's serving records for a verified attachment: the system apex A/AAAA set
    /// (`dnsserver::lb_records_strings`, the live edge addresses, identical to what the roster
    /// endpoint tells the tenant to point at) plus a `www` CNAME to the apex, since Let's Encrypt's
    /// HTTP-01 order carries a `www` SAN and a delegated zone that does not resolve it fails the
    /// WHOLE order. An EMPTY address set clears all three (full detach), so the zone stops claiming
    /// the platform's edge. Only system records are touched: tenant records and the CAA are never
    /// disturbed.
    ///
    /// Returns `true` only when the record set changed. An unchanged set is a no-op (the watcher
    /// re-pins every cadence, and rewriting would bump every record's id and `created_ms`, churning
    /// the wholesale-replicated store on a healthy fleet). A legacy system CNAME with an EMPTY name
    /// (the `www` alias was once pushed without its name, so every cadence appended another copy)
    /// forces one rewrite, so a bloated zone converges to one correct record set.
    pub fn set_system_address_records(
        &self,
        domain: &str,
        v4: Vec<String>,
        v6: Vec<String>,
    ) -> bool {
        let mut m = self.map.write();
        let Some(d) = m.get_mut(domain) else {
            return false;
        };
        let same = |kind: &str, ips: &[String]| {
            let mut have: Vec<&str> = d
                .records
                .iter()
                .filter(|r| r.system && r.name.is_empty() && r.kind == kind)
                .map(|r| r.value.as_str())
                .collect();
            have.sort_unstable();
            let mut want: Vec<&str> = ips.iter().map(|s| s.as_str()).collect();
            want.sort_unstable();
            have == want
        };
        let attached = !(v4.is_empty() && v6.is_empty());
        let mut www_have: Vec<&str> = d
            .records
            .iter()
            .filter(|r| r.system && r.name.eq_ignore_ascii_case("www") && r.kind == "CNAME")
            .map(|r| r.value.as_str())
            .collect();
        www_have.sort_unstable();
        let www_want = if attached {
            vec![d.domain.as_str()]
        } else {
            Vec::new()
        };
        let legacy_apex_cname = d
            .records
            .iter()
            .any(|r| r.system && r.name.is_empty() && r.kind == "CNAME");
        if same("A", &v4) && same("AAAA", &v6) && www_have == www_want && !legacy_apex_cname {
            return false;
        }
        d.records.retain(|r| {
            !(r.system
                && ((r.name.is_empty() && matches!(r.kind.as_str(), "A" | "AAAA" | "CNAME"))
                    || (r.name.eq_ignore_ascii_case("www") && r.kind == "CNAME")))
        });
        let now = now_ms();
        for (kind, ips) in [("A", v4), ("AAAA", v6)] {
            for ip in ips {
                d.records.push(DnsRecord {
                    id: rec_id(),
                    name: String::new(),
                    kind: kind.into(),
                    value: ip,
                    ttl: 60,
                    priority: None,
                    comment: "Platform edge (verified attachment)".into(),
                    created_ms: now,
                    system: true,
                });
            }
        }
        if attached {
            d.records.push(DnsRecord {
                id: rec_id(),
                name: "www".into(),
                kind: "CNAME".into(),
                value: d.domain.clone(),
                ttl: 60,
                priority: None,
                comment: "www alias (verified attachment)".into(),
                created_ms: now,
                system: true,
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, name: &str, value: &str) -> DnsRecord {
        DnsRecord {
            id: String::new(),
            name: name.into(),
            kind: kind.into(),
            value: value.into(),
            ttl: 3600,
            priority: None,
            comment: String::new(),
            created_ms: 0,
            system: false,
        }
    }

    #[test]
    fn ensure_creates_default_and_is_idempotent() {
        let s = DomainStore::new();
        let d = s.ensure("acme.com", "personal");
        assert_eq!(d.domain, "acme.com");
        assert_eq!(d.tenant, "personal");
        // Default system records: the managed-SSL CAA only. No apex A/AAAA is
        // seeded — those are pinned from the live edge set on verification.
        assert!(d.records.iter().any(|r| r.kind == "CAA" && r.system));
        assert!(
            !d.records.iter().any(|r| r.kind == "A" && r.system),
            "no fake apex address is seeded"
        );
        let n = d.records.len();
        // ensure again must NOT recreate / duplicate.
        let d2 = s.ensure("acme.com", "personal");
        assert_eq!(d2.records.len(), n, "ensure is idempotent");
    }

    #[test]
    fn add_record_assigns_id_and_marks_non_system() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        let added = s
            .add_record("acme.com", rec("A", "www", "1.2.3.4"))
            .expect("added");
        assert!(added.id.starts_with("rec_"));
        assert!(!added.system);
        let got = s.get("acme.com").unwrap();
        assert!(got
            .records
            .iter()
            .any(|r| r.id == added.id && r.value == "1.2.3.4"));
    }

    #[test]
    fn system_records_are_immutable_and_undeletable() {
        let s = DomainStore::new();
        let d = s.ensure("acme.com", "t");
        let sys = d.records.iter().find(|r| r.system).unwrap().id.clone();
        // update of a system record returns None.
        assert!(s
            .update_record(
                "acme.com",
                &sys,
                "@".into(),
                "A".into(),
                "9.9.9.9".into(),
                60,
                None,
                String::new()
            )
            .is_none());
        // delete of a system record is refused; it stays.
        assert!(!s.delete_record("acme.com", &sys));
        assert!(s
            .get("acme.com")
            .unwrap()
            .records
            .iter()
            .any(|r| r.id == sys));
    }

    #[test]
    fn update_and_delete_user_record() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        let id = s
            .add_record("acme.com", rec("A", "www", "1.1.1.1"))
            .unwrap()
            .id;
        let up = s
            .update_record(
                "acme.com",
                &id,
                "www".into(),
                "A".into(),
                "2.2.2.2".into(),
                120,
                None,
                "edited".into(),
            )
            .unwrap();
        assert_eq!(up.value, "2.2.2.2");
        assert_eq!(up.ttl, 120);
        assert!(s.delete_record("acme.com", &id));
        assert!(!s
            .get("acme.com")
            .unwrap()
            .records
            .iter()
            .any(|r| r.id == id));
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
    fn nameservers_and_ssl_renew() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        assert!(s.set_nameservers(
            "acme.com",
            vec!["ns1.shadw.cloud".into(), "ns2.shadw.cloud".into()]
        ));
        assert_eq!(
            s.get("acme.com").unwrap().nameservers,
            vec!["ns1.shadw.cloud", "ns2.shadw.cloud"]
        );
        // The ssl block starts honest-pending (no fabricated cert) and only
        // set_ssl_issued — driven by a REAL installed bundle — may claim
        // issued state; renew_ssl reads, never fabricates.
        let before = s.get("acme.com").unwrap().ssl;
        assert!(before.id.is_empty(), "no cert exists before issuance");
        assert!(before.cns.is_empty(), "no fabricated SANs");
        assert_eq!(before.issued_ms, 0);
        let cert = s.renew_ssl("acme.com").unwrap();
        assert_eq!(
            cert.id, before.id,
            "renew reads current state, never fabricates"
        );
        // The issued-state writer is gated on a VERIFIED attach — prove the
        // domain first, exactly as the real activation path does.
        s.ensure_verify("acme.com", "t", "proj").unwrap();
        s.mark_verified_as("acme.com", Some("t"));
        assert!(s.set_ssl_issued(
            "acme.com",
            "dom-abc123",
            vec!["acme.com".into(), "www.acme.com".into()],
            5,
            105
        ));
        let after = s.renew_ssl("acme.com").unwrap();
        assert_eq!(after.id, "dom-abc123");
        assert_eq!(after.cns, vec!["acme.com", "www.acme.com"]);
        assert!(
            !s.set_ssl_issued(
                "acme.com",
                "dom-abc123",
                vec!["acme.com".into(), "www.acme.com".into()],
                5,
                105
            ),
            "unchanged bundle is a no-op (damping)"
        );
    }

    #[test]
    fn snapshot_load_roundtrip() {
        let s = DomainStore::new();
        s.ensure("acme.com", "t");
        s.add_record("acme.com", rec("TXT", "@", "v=spf1 ~all"));
        let snap = s.snapshot();
        let s2 = DomainStore::new();
        s2.load(snap);
        assert!(s2
            .get("acme.com")
            .unwrap()
            .records
            .iter()
            .any(|r| r.kind == "TXT"));
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
