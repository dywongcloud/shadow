//! **Seer (Plane A, client to node): the platform's authoritative DNS**, over UDP **and** TCP. It
//! answers static records from the dashboard-managed `dns::DomainStore` (A, AAAA, CNAME, TXT; apex,
//! `www`, custom domains, CAA/TXT) and **dynamic, health-aware** A/AAAA for the deploy wildcard
//! zone `HIVE_DEPLOY_ZONE` (e.g. `*.deploy.shadw.app`): the **public IPs of healthy nodes**, so a
//! browser connects straight to a reachable node (the piece that replaced ngrok as ingress). NAT'd
//! or unhealthy nodes are excluded (a client must only get a node it can reach over HTTPS); it is a
//! **local read** of the gossiped registry, with no per-query network/DHT lookup (discovery stays
//! off the hot path).
//!
//! Distinct from `discovery.rs` (Plane B, node-to-node pkarr relay for iroh daemons): different
//! protocol, consumer and job; the names once collided, keep them apart. Binds a non-privileged
//! port by default (`HIVE_DNS_ADDR`, 127.0.0.1:5354) so it runs without root; set
//! `HIVE_DNS_ADDR=0.0.0.0:53` in prod and delegate the zone's NS here. A focused wire-format
//! implementation, no heavy dependency.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::net::{TcpListener, UdpSocket};

use crate::state::CloudState;
use hive_edge::NodeInfo;

/// Live Seer query counters: geo-DNS is otherwise invisible, with no way to tell a working
/// proximity path from a silently generic one except by hand-digging from several vantages.
/// Surfaced by `GET /v1/dns/stats` beside the GeoCache's own hit/pending/unlocatable counts.
pub struct DnsStats {
    pub queries: AtomicU64,
    pub queries_a: AtomicU64,
    pub queries_aaaa: AtomicU64,
    pub queries_other: AtomicU64,
    /// Answers ordered for THIS client (proximity applied) vs the generic set.
    pub tailored: AtomicU64,
    pub generic: AtomicU64,
    /// Client sent EDNS Client Subnet (a resolver forwarding its client's
    /// prefix) vs bare source-address geolocation.
    pub with_ecs: AtomicU64,
    /// Queries for a name this server is not authoritative for (NXDOMAIN).
    pub nxdomain: AtomicU64,
    pub over_tcp: AtomicU64,
}

pub static DNS_STATS: DnsStats = DnsStats {
    queries: AtomicU64::new(0),
    queries_a: AtomicU64::new(0),
    queries_aaaa: AtomicU64::new(0),
    queries_other: AtomicU64::new(0),
    tailored: AtomicU64::new(0),
    generic: AtomicU64::new(0),
    with_ecs: AtomicU64::new(0),
    nxdomain: AtomicU64::new(0),
    over_tcp: AtomicU64::new(0),
};

/// Which node each answer actually handed out first — the histogram that shows
/// whether traffic is really being spread by proximity or collapsing onto one
/// node. Keyed by the first A/AAAA address in the answer.
pub static ANSWER_FIRST: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Render an A/AAAA rdata blob back to a printable address for the answer
/// histogram (nothing else needs this — the wire format is built, not parsed).
fn rdata_ip(atype: u16, rdata: &[u8]) -> Option<String> {
    match (atype, rdata.len()) {
        (1, 4) => Some(std::net::Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]).to_string()),
        (28, 16) => {
            let mut o = [0u8; 16];
            o.copy_from_slice(rdata);
            Some(std::net::Ipv6Addr::from(o).to_string())
        }
        _ => None,
    }
}

/// Whether this server answers the customer-facing apps zone. Off by default:
/// the zone is Vercel-served until an operator delegates it here.
fn serve_apps_zone() -> bool {
    std::env::var("HIVE_DNS_SERVE_APPS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// The node currently serving `label` in the apps zone, if any: local aliases first (authoritative
/// for what THIS node serves), then the gossiped peer route table with the lowest-latency healthy
/// route winning. The same ownership rule the DNS reconciler's affinity records encode, so the two
/// paths cannot disagree about who owns a host.
fn apps_host_owner(cloud: &Arc<CloudState>, label: &str) -> Option<String> {
    if label.is_empty() {
        return None;
    }
    let norm = |h: &str| {
        h.split(':')
            .next()
            .unwrap_or(h)
            .split('.')
            .next()
            .unwrap_or(h)
            .trim()
            .to_ascii_lowercase()
    };
    if cloud.gw.served_hosts().iter().any(|h| norm(h) == label) {
        return Some(cloud.node_name.clone());
    }
    let routes = cloud.peer_routes.read();
    for (host, rs) in routes.iter() {
        if norm(host) == label {
            if let Some(best) = rs.iter().filter(|r| r.healthy).min_by_key(|r| r.latency_ms) {
                return Some(best.node_id.clone());
            }
        }
    }
    None
}

/// A/AAAA answer RRs for one node, matching the requested family.
fn node_addr_rrs(n: &NodeInfo, qtype: u16) -> Vec<(u16, u32, Vec<u8>)> {
    let mut out = Vec::new();
    match qtype {
        1 => {
            if let Some(ip) = n
                .public_ip
                .as_deref()
                .and_then(|s| s.parse::<Ipv4Addr>().ok())
            {
                if !ip.is_unspecified() && !ip.is_loopback() {
                    out.push((1u16, DEPLOY_TTL, ip.octets().to_vec()));
                }
            }
        }
        28 => {
            if let Some(ip) = n
                .public_ip6
                .as_deref()
                .and_then(|s| s.parse::<Ipv6Addr>().ok())
            {
                if !ip.is_unspecified() && !ip.is_loopback() {
                    out.push((28u16, DEPLOY_TTL, ip.octets().to_vec()));
                }
            }
        }
        _ => {}
    }
    out
}

/// The deploy wildcard zone whose A/AAAA are answered dynamically (healthy node IPs):
/// `HIVE_DEPLOY_ZONE`, e.g. `deploy.shadw.app` serves `deploy.shadw.app` and `*.deploy.shadw.app`.
/// Cached (a per-query env read would be a syscall on the hot path), lowercased and dot-trimmed.
pub(crate) fn deploy_zone() -> Option<&'static str> {
    static Z: OnceLock<Option<String>> = OnceLock::new();
    Z.get_or_init(|| {
        std::env::var("HIVE_DEPLOY_ZONE")
            .ok()
            .map(|s| s.trim().trim_matches('.').to_lowercase())
            .filter(|s| !s.is_empty())
    })
    .as_deref()
}

/// The platform API zone (`api.{platform_domain}`), served health-aware and proximity-ordered like
/// the deploy zone, replacing the static round-robin A set the Vercel reconciler used to publish
/// for `api`. Derived from `HIVE_PLATFORM_DOMAIN` (no extra env: serving a zone nobody has
/// delegated yet is harmless, and the delegation side is gated separately on the gossiped `dns_api`
/// capability).
pub(crate) fn api_zone() -> Option<&'static str> {
    static Z: OnceLock<Option<String>> = OnceLock::new();
    Z.get_or_init(|| {
        std::env::var("HIVE_PLATFORM_DOMAIN")
            .ok()
            .map(|s| s.trim().trim_matches('.').to_lowercase())
            .filter(|s| !s.is_empty())
            .map(|d| format!("api.{d}"))
    })
    .as_deref()
}

/// TTL (seconds) for dynamic deploy-zone answers — short, so an unhealthy node drains
/// from resolver caches quickly when it's dropped from the registry.
const DEPLOY_TTL: u32 = 60;

/// Serves UDP and, best effort, TCP on the same address: resolvers retry over TCP on truncation and
/// some validators always do, so a real authoritative server answers both (a failed TCP bind leaves
/// UDP serving, the hot path).
pub async fn serve(cloud: Arc<CloudState>, addr: SocketAddr) -> std::io::Result<()> {
    let sock = UdpSocket::bind(addr).await?;
    match TcpListener::bind(addr).await {
        Ok(tcp) => {
            let c = cloud.clone();
            tokio::spawn(async move { serve_tcp(c, tcp).await });
        }
        Err(e) => tracing::warn!(%addr, error=%e, "DNS TCP bind failed (UDP still serving)"),
    }
    tracing::info!(%addr, "Seer authoritative DNS listening (UDP+TCP)");
    let mut buf = vec![0u8; 1500];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(_) => continue,
        };
        if let Some(resp) = handle_query(&cloud, &buf[..n], peer.ip()) {
            let _ = sock.send_to(&resp, peer).await;
        }
    }
}

/// DNS-over-TCP: each message is framed by a 2-byte big-endian length prefix.
async fn serve_tcp(cloud: Arc<CloudState>, listener: TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut stream, peer)) = listener.accept().await else {
            continue;
        };
        let cloud = cloud.clone();
        tokio::spawn(async move {
            let mut len = [0u8; 2];
            if stream.read_exact(&mut len).await.is_err() {
                return;
            }
            let n = u16::from_be_bytes(len) as usize;
            if n == 0 || n > 4096 {
                return;
            }
            let mut msg = vec![0u8; n];
            if stream.read_exact(&mut msg).await.is_err() {
                return;
            }
            if let Some(resp) = handle_query(&cloud, &msg, peer.ip()) {
                DNS_STATS.over_tcp.fetch_add(1, Ordering::Relaxed);
                let mut framed = (resp.len() as u16).to_be_bytes().to_vec();
                framed.extend_from_slice(&resp);
                let _ = stream.write_all(&framed).await;
            }
        });
    }
}

/// Parse a DNS query and build a response from the platform's records.
///
/// The OPT RR is echoed only when the client sent one (RFC 6891: never add EDNS to a response the
/// requester did not opt into), with SCOPE PREFIX-LENGTH the asker's prefix when the answer really
/// is client-specific, else 0 so a resolver may share the generic answer. Flags are QR=1, AA=1, RD
/// echoed, RA=0, with rcode NOERROR or NXDOMAIN. A malformed OPT yields a generic answer and never
/// rejects the query (an unfamiliar resolver must not become an outage). AUTHORITY records are
/// owned by the zone APEX, so they encode their owner explicitly instead of riding the
/// question-name compression pointer. A compressed name in the question is unsupported (rare).
fn handle_query(cloud: &Arc<CloudState>, q: &[u8], src: std::net::IpAddr) -> Option<Vec<u8>> {
    if q.len() < 12 {
        return None;
    }
    let id = [q[0], q[1]];
    let rd = q[2] & 0x01;
    let qdcount = u16::from_be_bytes([q[4], q[5]]);
    if qdcount < 1 {
        return None;
    }
    let counts = (
        u16::from_be_bytes([q[6], q[7]]),
        u16::from_be_bytes([q[8], q[9]]),
        u16::from_be_bytes([q[10], q[11]]),
    );

    let mut off = 12usize;
    let mut labels = Vec::new();
    loop {
        if off >= q.len() {
            return None;
        }
        let len = q[off] as usize;
        if len == 0 {
            off += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None;
        }
        off += 1;
        if off + len > q.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&q[off..off + len]).to_lowercase());
        off += len;
    }
    if off + 4 > q.len() {
        return None;
    }
    let qname = labels.join(".");
    let qtype = u16::from_be_bytes([q[off], q[off + 1]]);
    let q_end = off + 4;
    let question = &q[12..q_end];

    let asker = crate::dns_geo::Asker {
        source: src,
        subnet: crate::dns_geo::parse_client_subnet(q, q_end, counts),
    };
    let client_had_ecs = asker.subnet.is_some();

    let (answers, authority, found_domain, proximity) = lookup(cloud, &qname, qtype, &asker);

    DNS_STATS.queries.fetch_add(1, Ordering::Relaxed);
    match qtype {
        1 => DNS_STATS.queries_a.fetch_add(1, Ordering::Relaxed),
        28 => DNS_STATS.queries_aaaa.fetch_add(1, Ordering::Relaxed),
        _ => DNS_STATS.queries_other.fetch_add(1, Ordering::Relaxed),
    };
    if client_had_ecs {
        DNS_STATS.with_ecs.fetch_add(1, Ordering::Relaxed);
    }
    if proximity {
        DNS_STATS.tailored.fetch_add(1, Ordering::Relaxed);
    } else {
        DNS_STATS.generic.fetch_add(1, Ordering::Relaxed);
    }
    if !found_domain {
        DNS_STATS.nxdomain.fetch_add(1, Ordering::Relaxed);
    }
    if let Some((atype, _, rdata)) = answers.first() {
        if let Some(ip) = rdata_ip(*atype, rdata) {
            *ANSWER_FIRST.lock().entry(ip).or_insert(0) += 1;
        }
    }

    let opt_rr = client_had_ecs.then(|| {
        crate::dns_geo::encode_opt_rr(
            asker.subnet,
            if proximity { asker.scope_prefix() } else { 0 },
        )
    });
    let arcount: u16 = opt_rr.is_some() as u16;

    let mut resp = Vec::with_capacity(64);
    resp.extend_from_slice(&id);
    let rcode: u8 = if !found_domain { 3 } else { 0 };
    let flags: u16 = 0x8000 | 0x0400 | ((rd as u16) << 8) | rcode as u16;
    resp.extend_from_slice(&flags.to_be_bytes());
    resp.extend_from_slice(&1u16.to_be_bytes());
    resp.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    resp.extend_from_slice(&(authority.len() as u16).to_be_bytes());
    resp.extend_from_slice(&arcount.to_be_bytes());
    resp.extend_from_slice(question);
    for (atype, ttl, rdata) in &answers {
        resp.extend_from_slice(&[0xC0, 0x0C]);
        resp.extend_from_slice(&atype.to_be_bytes());
        resp.extend_from_slice(&1u16.to_be_bytes());
        resp.extend_from_slice(&ttl.to_be_bytes());
        resp.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        resp.extend_from_slice(rdata);
    }
    for (owner, atype, ttl, rdata) in &authority {
        resp.extend_from_slice(&encode_name(owner));
        resp.extend_from_slice(&atype.to_be_bytes());
        resp.extend_from_slice(&1u16.to_be_bytes());
        resp.extend_from_slice(&ttl.to_be_bytes());
        resp.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        resp.extend_from_slice(rdata);
    }
    if let Some(rr) = opt_rr {
        resp.extend_from_slice(&rr);
    }
    /// UDP amplification guard: never emit a datagram past the practical DNS payload. A
    /// tenant-programmed record set (an uncapped TXT, a huge RRset at one name) must not turn every
    /// fleet node into a reflector for spoofed-source queries; TC=1 is the standards answer, and
    /// the resolver retries over TCP, which this server also runs.
    const MAX_UDP_RESPONSE: usize = 1232;
    if resp.len() > MAX_UDP_RESPONSE {
        let mut t = Vec::with_capacity(64);
        t.extend_from_slice(&id);
        let tflags: u16 = 0x8000 | 0x0400 | 0x0200 | ((rd as u16) << 8) | rcode as u16;
        t.extend_from_slice(&tflags.to_be_bytes());
        t.extend_from_slice(&1u16.to_be_bytes());
        t.extend_from_slice(&0u16.to_be_bytes());
        t.extend_from_slice(&0u16.to_be_bytes());
        t.extend_from_slice(&0u16.to_be_bytes());
        t.extend_from_slice(question);
        return Some(t);
    }
    Some(resp)
}

/// Returns (answer RRs, AUTHORITY RRs, whether the zone/domain is authoritative here, whether the
/// answer was tailored to THIS client's location). The AUTHORITY slot carries `(owner name, type,
/// ttl, rdata)`: unlike answers it cannot ride the question-name compression pointer, because a
/// NODATA answer's SOA is owned by the zone APEX, not the qname (RFC 2308). The last value decides
/// the ECS scope the caller echoes: a tailored answer must not be cached for clients it was not
/// computed for.
///
/// Zones are tried in order. Each served zone is authoritative for the WHOLE zone (NOERROR, never
/// NXDOMAIN): TXT for `_acme-challenge.*` comes from the replicated challenge store (the leader's
/// acme.rs writes it and Let's Encrypt may ask ANY advertised nameserver) and everything else is
/// NODATA with the zone's SOA in AUTHORITY, without which a resolver cannot negative-cache and
/// every miss returns to us for its full lifetime (RFC 2308).
///
/// - The deploy zone (`HIVE_DEPLOY_ZONE`): AFFINITY first (a label the fleet can attribute to the
/// node hosting it resolves to that node, so the client never pays a cross-node forward), computed
/// per query from the route table and never published as a record (the Vercel-hosted apps zone
/// needed one API-created record per label), then the proximity-ordered healthy set. The zone's own
/// apex NS/SOA are answered AT the apex only: a delegation serving neither is half-configured
/// (without the SOA nonexistent names return forever, and validators flag a child missing the NS
/// RRset its parent publishes).
/// - The API zone (`api.{platform}`): the same health-before-proximity set, replacing the flat
/// Vercel round-robin where an unhealthy node stayed in the answer for the record's full TTL. The
/// parent-side NS delegation is published only with at least two `dns_api`-capable nameservers
/// (`vercel_dns::desired_api_delegation`), so until then this branch answers queries nobody routes
/// here; its ACME TXT path keeps the `api.{platform}` SAN renewable once the NS moves to Seer.
/// - The apps zone (`HIVE_APPS_DOMAIN`), opt-in via `HIVE_DNS_SERVE_APPS` (meaningful only once
/// delegated here): a host attributable to a node resolves to THAT node (anywhere else just buys a
/// cross-node forward), and the wildcard case gets the proximity-ordered healthy set.
/// - Tenant zones (`dns::DomainStore`): longest-suffix zone match; apex NS/SOA at the child;
/// platform-managed names answered from the platform's own stores and never from zone records,
/// which keeps the proof channel unforgeable (`_acme-challenge` from the LE store, `_hive-verify`
/// from the attachment's challenge, served only while PENDING so yesterday's proof is not
/// replayable); wildcards per RFC 1034/4592 (`*` synthesizes only for names that do not otherwise
/// exist, never the apex, and an exact or platform-reserved name shadows it); a CNAME answers an
/// A/AAAA query; an empty answer carries the zone's SOA.
#[allow(clippy::type_complexity)]
fn lookup(
    cloud: &Arc<CloudState>,
    qname: &str,
    qtype: u16,
    asker: &crate::dns_geo::Asker,
) -> (
    Vec<(u16, u32, Vec<u8>)>,
    Vec<(String, u16, u32, Vec<u8>)>,
    bool,
    bool,
) {
    if let Some(zone) = deploy_zone() {
        if qname == zone || qname.ends_with(&format!(".{zone}")) {
            match qtype {
                1 | 28 => {
                    let nodes = cloud.registry.nodes();
                    if qname != zone {
                        let label = qname
                            .strip_suffix(&format!(".{zone}"))
                            .unwrap_or("")
                            .rsplit('.')
                            .next()
                            .unwrap_or("")
                            .to_string();
                        if let Some(owner) = apps_host_owner(cloud, &label) {
                            if let Some(n) = nodes.iter().find(|n| n.name == owner && n.healthy) {
                                let rrs = node_addr_rrs(n, qtype);
                                if !rrs.is_empty() {
                                    return (rrs, Vec::new(), true, false);
                                }
                            }
                        }
                    }
                    let client = cloud.dns_geo.locate(asker.locate_addr());
                    let (rrs, tailored) = lb_records(&nodes, qtype, client);
                    return (rrs, Vec::new(), true, tailored);
                }
                2 if qname == zone => return (apex_ns_rrs(cloud, false), Vec::new(), true, false),
                6 if qname == zone => return (apex_soa_rrs(cloud, false), Vec::new(), true, false),
                16 if qname.starts_with("_acme-challenge.") => {
                    let rrs = acme_txt_rrs(cloud, qname);
                    let auth = if rrs.is_empty() {
                        negative_soa(cloud, zone, false)
                    } else {
                        Vec::new()
                    };
                    return (rrs, auth, true, false);
                }
                _ => return (Vec::new(), negative_soa(cloud, zone, false), true, false),
            }
        }
    }

    if let Some(zone) = api_zone() {
        if qname == zone || qname.ends_with(&format!(".{zone}")) {
            match qtype {
                1 | 28 => {
                    let client = cloud.dns_geo.locate(asker.locate_addr());
                    let (rrs, tailored) = lb_records(&cloud.registry.nodes(), qtype, client);
                    return (rrs, Vec::new(), true, tailored);
                }
                2 if qname == zone => return (apex_ns_rrs(cloud, true), Vec::new(), true, false),
                6 if qname == zone => return (apex_soa_rrs(cloud, true), Vec::new(), true, false),
                16 if qname.starts_with("_acme-challenge.") => {
                    let rrs = acme_txt_rrs(cloud, qname);
                    let auth = if rrs.is_empty() {
                        negative_soa(cloud, zone, true)
                    } else {
                        Vec::new()
                    };
                    return (rrs, auth, true, false);
                }
                _ => return (Vec::new(), negative_soa(cloud, zone, true), true, false),
            }
        }
    }

    if serve_apps_zone() {
        let apps = cloud.apps_domain.trim().trim_matches('.').to_lowercase();
        if !apps.is_empty() && (qname == apps || qname.ends_with(&format!(".{apps}"))) {
            match qtype {
                1 | 28 => {
                    let label = qname
                        .strip_suffix(&format!(".{apps}"))
                        .unwrap_or("")
                        .rsplit('.')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    let nodes = cloud.registry.nodes();
                    if let Some(owner) = apps_host_owner(cloud, &label) {
                        if let Some(n) = nodes.iter().find(|n| n.name == owner && n.healthy) {
                            let rrs = node_addr_rrs(n, qtype);
                            if !rrs.is_empty() {
                                return (rrs, Vec::new(), true, false);
                            }
                        }
                    }
                    let client = cloud.dns_geo.locate(asker.locate_addr());
                    let (rrs, tailored) = lb_records(&nodes, qtype, client);
                    return (rrs, Vec::new(), true, tailored);
                }
                16 if qname.starts_with("_acme-challenge.") => {
                    return (acme_txt_rrs(cloud, qname), Vec::new(), true, false)
                }
                _ => return (Vec::new(), Vec::new(), true, false),
            }
        }
    }

    let domains = cloud.domains.snapshot();
    let Some(zone) = domains
        .into_iter()
        .filter(|d| qname == d.domain || qname.ends_with(&format!(".{}", d.domain)))
        .max_by_key(|d| d.domain.len())
    else {
        return (Vec::new(), Vec::new(), false, false);
    };

    let rec_name = if qname == zone.domain {
        String::new()
    } else {
        qname
            .trim_end_matches(&format!(".{}", zone.domain))
            .to_string()
    };

    if rec_name.is_empty() && qtype == 2 {
        let rrs = apex_ns_rrs(cloud, false);
        return (rrs, Vec::new(), true, false);
    }
    if rec_name.is_empty() && qtype == 6 {
        let rrs = apex_soa_rrs(cloud, false);
        return (rrs, Vec::new(), true, false);
    }

    let want = match qtype {
        1 => "A",
        28 => "AAAA",
        5 => "CNAME",
        16 => "TXT",
        15 => "MX",
        2 => "NS",
        33 => "SRV",
        257 => "CAA",
        12 => "PTR",
        _ => "",
    };

    let mut out = Vec::new();
    if want == "TXT" && rec_name.eq_ignore_ascii_case("_acme-challenge") {
        out.extend(acme_txt_rrs(cloud, qname));
    }
    if want == "TXT" && rec_name.eq_ignore_ascii_case("_hive-verify") {
        if let Some(v) = &zone.verify {
            if v.status == "pending" {
                if let Some(rd) = encode_rdata("TXT", &v.txt_value) {
                    out.push((16u16, 60, rd));
                }
            }
        }
    }
    let exact_exists = zone
        .records
        .iter()
        .any(|r| r.name.eq_ignore_ascii_case(&rec_name))
        || rec_name.eq_ignore_ascii_case("_acme-challenge")
        || rec_name.eq_ignore_ascii_case("_hive-verify");
    for r in &zone.records {
        let name_match = r.name.eq_ignore_ascii_case(&rec_name)
            || (r.name == "*" && !exact_exists && !rec_name.is_empty());
        if !name_match {
            continue;
        }
        let kind = r.kind.to_uppercase();
        let serve_as_cname = matches!(qtype, 1 | 28) && kind == "CNAME";
        if kind != want && !serve_as_cname {
            continue;
        }
        let atype = match kind.as_str() {
            "A" => 1u16,
            "AAAA" => 28,
            "CNAME" => 5,
            "TXT" => 16,
            "MX" => 15,
            "NS" => 2,
            "SRV" => 33,
            "CAA" => 257,
            "PTR" => 12,
            _ => continue,
        };
        if let Some(rd) = encode_rdata_typed(&kind, &r.value, r.priority) {
            out.push((atype, r.ttl, rd));
        }
    }
    if out.is_empty() {
        return (out, negative_soa(cloud, &zone.domain, false), true, false);
    }
    (out, Vec::new(), true, false)
}

/// Builds the dynamic A (qtype 1) or AAAA (qtype 28) answer set: the public IP of every node that
/// is both `healthy` and has a public IP of the requested family (NAT'd and unhealthy nodes are
/// excluded: a browser must only receive a node it can reach over HTTPS). Both filters run BEFORE
/// proximity ordering, because proximity truncates to the nearest N: a nearby node with no public
/// address would consume a slot and vanish when the answer is built, an EMPTY response for exactly
/// the clients closest to it (seen: a San Jose client got 0 answers because its two nearest nodes
/// were NAT'd Macs with coordinates but no public IP). Ordering by the SERVING node's own latency
/// (self = 0) is only the fallback for an unknown client location, since that number describes us,
/// not the client; resolvers round-robin the set, which is capped to fit a 512-byte datagram.
///
/// `client` is resolved by the CALLER (never blocking: an unknown subnet is queued for a background
/// lookup), so this stays a pure function of (registry, qtype, client location) and can be
/// exercised without a `CloudState`. The answer counts as `tailored` only if proximity shaped it
/// AND survived the address-family filter: an empty set is not client-specific.
fn lb_records(
    nodes: &[NodeInfo],
    qtype: u16,
    client: Option<(f64, f64)>,
) -> (Vec<(u16, u32, Vec<u8>)>, bool) {
    let mut healthy: Vec<&NodeInfo> = nodes
        .iter()
        .filter(|n| n.healthy && node_addr_rrs(n, qtype).len() == 1)
        .collect();
    healthy.sort_by_key(|n| n.latency_ms);
    let mut tailored = false;
    if let Some(near) = crate::dns_geo::nearest_first(&healthy, client) {
        healthy = near;
        tailored = true;
    }
    let mut out = Vec::new();
    for n in healthy {
        match qtype {
            1 => {
                if let Some(ip) = n
                    .public_ip
                    .as_deref()
                    .and_then(|s| s.parse::<Ipv4Addr>().ok())
                {
                    if !ip.is_unspecified() && !ip.is_loopback() {
                        out.push((1u16, DEPLOY_TTL, ip.octets().to_vec()));
                    }
                }
            }
            28 => {
                if let Some(ip) = n
                    .public_ip6
                    .as_deref()
                    .and_then(|s| s.parse::<Ipv6Addr>().ok())
                {
                    if !ip.is_unspecified() && !ip.is_loopback() {
                        out.push((28u16, DEPLOY_TTL, ip.octets().to_vec()));
                    }
                }
            }
            _ => {}
        }
    }
    out.truncate(8);
    let tailored = tailored && !out.is_empty();
    (out, tailored)
}

/// TTL for ACME DNS-01 TXT answers: mirrors the 60 s acme.rs asks Vercel for on the same records,
/// short like `DEPLOY_TTL` so a finished challenge drains from resolver caches quickly.
const ACME_TXT_TTL: u32 = 60;

/// TXT answer RRs for a `_acme-challenge.*` qname in a Seer-answered zone, straight from the
/// replicated challenge store, through the static-record TXT encoder (one wire encoding, never
/// two).
fn acme_txt_rrs(cloud: &Arc<CloudState>, qname: &str) -> Vec<(u16, u32, Vec<u8>)> {
    cloud
        .acme_challenges
        .lookup(qname)
        .iter()
        .filter_map(|v| encode_rdata("TXT", v).map(|rd| (16u16, ACME_TXT_TTL, rd)))
        .collect()
}

fn encode_rdata(kind: &str, value: &str) -> Option<Vec<u8>> {
    encode_rdata_typed(kind, value, None)
}

/// Typed record-data encoder for the static (tenant-zone) arm: MX/SRV take their numeric fields
/// from the record's `priority` (SRV also parses `<weight> <port> <target>` from the value, the one
/// shape the record model holds); CAA parses `<flags> <tag> "<value>"` (quotes optional); anything
/// unparseable is skipped, never encoded wrong. TXT RDATA is one or more `<length><bytes>`
/// character-strings of at most 255 bytes (RFC 1035): long values (DKIM keys) split into
/// consecutive strings that resolvers re-concatenate and are never dropped (they once silently
/// vanished from the zone); an empty value is one zero-length string.
fn encode_rdata_typed(kind: &str, value: &str, priority: Option<u32>) -> Option<Vec<u8>> {
    match kind {
        "A" => value
            .parse::<Ipv4Addr>()
            .ok()
            .map(|ip| ip.octets().to_vec()),
        "AAAA" => value
            .parse::<Ipv6Addr>()
            .ok()
            .map(|ip| ip.octets().to_vec()),
        "CNAME" | "NS" | "PTR" => Some(encode_name(value)),
        "TXT" => {
            let bytes = value.as_bytes();
            let mut v = Vec::with_capacity(bytes.len() + bytes.len() / 255 + 1);
            for chunk in bytes.chunks(255) {
                v.push(chunk.len() as u8);
                v.extend_from_slice(chunk);
            }
            if bytes.is_empty() {
                v.push(0);
            }
            Some(v)
        }
        "MX" => {
            let mut v = (priority.unwrap_or(10) as u16).to_be_bytes().to_vec();
            v.extend_from_slice(&encode_name(value));
            Some(v)
        }
        "SRV" => {
            let mut it = value.split_whitespace();
            let weight = it.next()?.parse::<u16>().ok()?;
            let port = it.next()?.parse::<u16>().ok()?;
            let target = it.next()?;
            let mut v = (priority.unwrap_or(0) as u16).to_be_bytes().to_vec();
            v.extend_from_slice(&weight.to_be_bytes());
            v.extend_from_slice(&port.to_be_bytes());
            v.extend_from_slice(&encode_name(target));
            Some(v)
        }
        "CAA" => {
            let mut it = value.splitn(3, char::is_whitespace);
            let flags = it.next()?.parse::<u8>().ok()?;
            let tag = it.next()?;
            let val = it.next().unwrap_or("").trim_matches('"');
            if tag.len() > 255 {
                return None;
            }
            let mut v = vec![flags, tag.len() as u8];
            v.extend_from_slice(tag.as_bytes());
            v.extend_from_slice(val.as_bytes());
            Some(v)
        }
        _ => None,
    }
}

/// TTL for the zone's own apex records: longer than the health-driven address TTL, since the NS/SOA
/// set changes only when the nameserver roster does and a short TTL just multiplies apex queries.
const APEX_TTL: u32 = 300;

/// Negative-cache TTL published in the SOA MINIMUM: how long a resolver may remember that a name
/// does not exist (RFC 2308), modest so a freshly created deployment name is not shadowed by a
/// stale negative.
const NEGATIVE_TTL: u32 = 60;

/// The nameserver hostnames this zone is served by, from the SAME eligible-node rule the DNS
/// reconciler uses to publish NS at the parent, so parent and child cannot disagree about the
/// zone's NS RRset. `require_api`: the API zone's NS set also demands the gossiped `dns_api`
/// capability (an older binary's Seer answers the deploy zone but would NXDOMAIN `api.{platform}`,
/// so it must never be named there); `desired_api_delegation` applies the identical filter.
fn apex_ns_names(cloud: &Arc<CloudState>, require_api: bool) -> Vec<String> {
    let apps = cloud.apps_domain.trim().trim_matches('.').to_lowercase();
    let mut names: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| {
            n.healthy && n.dns_ns.is_some() && (n.public_ip.is_some() || n.public_ip6.is_some())
        })
        .filter(|n| !require_api || n.dns_api)
        .map(|n| format!("{}.{}", crate::vercel_dns::ns_label(&n.name), apps))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Public handle for the admin roster endpoint (the wizard shows tenants the
/// exact hostnames to delegate to; private so the filter stays in one place).
pub fn apex_ns_names_pub(cloud: &Arc<CloudState>) -> Vec<String> {
    apex_ns_names(cloud, false)
}

fn apex_ns_rrs(cloud: &Arc<CloudState>, require_api: bool) -> Vec<(u16, u32, Vec<u8>)> {
    apex_ns_names(cloud, require_api)
        .into_iter()
        .map(|n| (2u16, APEX_TTL, encode_name(&n)))
        .collect()
}

/// The healthy+public address record set the apps/deploy answers are built from (`lb_records`' own
/// filter), shared so the tenant-facing roster endpoint returns byte-identical guidance to what the
/// wire would answer for the wildcard, never a separate computation of the same set.
pub fn lb_records_pub(nodes: &[NodeInfo], qtype: u16) -> (Vec<Vec<u8>>, bool) {
    let (rrs, capped) = lb_records(nodes, qtype, None);
    (rrs.into_iter().map(|(_, _, rd)| rd).collect(), capped)
}

/// The same healthy edge set as display strings (dotted-quad / compressed v6), decoded from
/// `lb_records_pub`'s wire rdata so the delegated zone's system records and the roster endpoint can
/// never render a different set than the dynamic arm answers.
pub fn lb_records_strings(nodes: &[NodeInfo], qtype: u16) -> Vec<String> {
    let (rrs, _) = lb_records_pub(nodes, qtype);
    rrs.iter()
        .filter_map(|rd| match rd.as_slice() {
            [a, b, c, d] => Some(Ipv4Addr::new(*a, *b, *c, *d).to_string()),
            v if v.len() == 16 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(v);
                Some(Ipv6Addr::from(o).to_string())
            }
            _ => None,
        })
        .collect()
}

/// The zone's SOA: MNAME is the lexically-first nameserver (stable across nodes, so every server
/// reports the same primary), and SERIAL derives from the node roster, advancing when the zone's
/// own NS set really changes rather than on every restart.
fn apex_soa_rrs(cloud: &Arc<CloudState>, require_api: bool) -> Vec<(u16, u32, Vec<u8>)> {
    let names = apex_ns_names(cloud, require_api);
    let Some(primary) = names.first() else {
        return Vec::new();
    };
    let apps = cloud.apps_domain.trim().trim_matches('.').to_lowercase();
    let mut rdata = encode_name(primary);
    rdata.extend_from_slice(&encode_name(&format!("hostmaster.{apps}")));
    let serial: u32 = names.iter().fold(0u32, |acc, n| {
        n.bytes()
            .fold(acc, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
    });
    for v in [serial, 7200u32, 3600u32, 1_209_600u32, NEGATIVE_TTL] {
        rdata.extend_from_slice(&v.to_be_bytes());
    }
    vec![(6u16, APEX_TTL, rdata)]
}

/// AUTHORITY-section SOA for an authoritative empty (NODATA) answer (RFC 2308): without it a
/// resolver cannot negative-cache and every miss returns to us for its full lifetime. Owner = the
/// zone apex (why the AUTHORITY slot carries an explicit name). The record's TTL is published as
/// the negative-cache TTL directly: RFC 2308 §5 defines negTTL as min(SOA's own TTL, its MINIMUM),
/// and `NEGATIVE_TTL` is already the smaller. Empty when no eligible nameserver exists (the
/// degenerate roster state in which the apex SOA is unanswerable too).
fn negative_soa(
    cloud: &Arc<CloudState>,
    zone: &str,
    require_api: bool,
) -> Vec<(String, u16, u32, Vec<u8>)> {
    apex_soa_rrs(cloud, require_api)
        .into_iter()
        .map(|(t, _ttl, rd)| (zone.to_string(), t, NEGATIVE_TTL, rd))
        .collect()
}

fn encode_name(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name
        .trim_end_matches('.')
        .split('.')
        .filter(|l| !l.is_empty())
    {
        let b = label.as_bytes();
        out.push(b.len().min(63) as u8);
        out.extend_from_slice(&b[..b.len().min(63)]);
    }
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_core::now_ms;

    /// Minimal NodeInfo for the load-balancer tests.
    fn ni(
        name: &str,
        healthy: bool,
        ip4: Option<&str>,
        ip6: Option<&str>,
        latency: u64,
    ) -> NodeInfo {
        NodeInfo {
            gpu_count: 0,
            wasm_runtime: None,
            bun_runtime: None,
            build_toolchains: None,
            runtime_artifact_protocol: None,
            build_isolation_protocol: None,
            artifact_transfer_protocol: None,
            musql_engine: None,
            gpu_model: None,
            gpu_vram_mb: 0,
            id: name.into(),
            name: name.into(),
            region: "test".into(),
            public_url: String::new(),
            public_ip: ip4.map(|s| s.to_string()),
            public_ip6: ip6.map(|s| s.to_string()),
            peer_id: None,
            iroh_addr: None,
            guardian_iroh_addr: None,
            relay_url: None,
            dns_ns: None,
            dns_api: false,
            dns_attest: Vec::new(),
            dashboard: false,
            cp_epoch: 0,
            last_seen_ms: now_ms(),
            is_self: false,
            latency_ms: latency,
            healthy,
            lat: None,
            lon: None,
            city: None,
            country: None,
            cpu_cores: 0,
            mem_total_mb: 0,
            disk_total_gb: 0,
            // 0/None = UNKNOWN, which is correct for these fixtures: DNS
            // load-balancing filters on health and public-address family only
            // (`lb_records`), never on capacity, so leaving these unknown keeps
            // the fixtures honest about what the code under test actually reads.
            disk_free_gb: 0,
            gpu_free_mb: None,
            started_ms: 0,
            oom_restarts_24h: 0,
            unclean_restarts_24h: 0,
            last_oom_ms: None,
            backend: String::new(),
            provider: None,
            private_addr: None,
        }
    }

    #[test]
    fn deploy_zone_returns_only_healthy_public_nodes() {
        let nodes = vec![
            ni("healthy-public", true, Some("203.0.113.10"), None, 5),
            ni("healthy-natd", true, None, None, 1), // NAT'd: no public IP → excluded
            ni("unhealthy-public", false, Some("203.0.113.99"), None, 0), // down → excluded
        ];
        let (a, tailored) = lb_records(&nodes, 1, None);
        assert!(!tailored, "no client location → generic answer");
        assert_eq!(a.len(), 1, "only the healthy+public node is returned");
        assert_eq!(a[0].0, 1, "A record");
        assert_eq!(a[0].1, DEPLOY_TTL);
        assert_eq!(a[0].2, Ipv4Addr::new(203, 0, 113, 10).octets().to_vec());
    }

    #[test]
    fn marking_unhealthy_or_nat_removes_from_answers() {
        // Healthy+public → present; flip to unhealthy → gone; NAT'd never appears.
        let up = vec![ni("n1", true, Some("198.51.100.7"), None, 0)];
        assert_eq!(lb_records(&up, 1, None).0.len(), 1);
        let down = vec![ni("n1", false, Some("198.51.100.7"), None, 0)];
        assert!(
            lb_records(&down, 1, None).0.is_empty(),
            "unhealthy node excluded"
        );
        let natd = vec![ni("n1", true, None, None, 0)];
        assert!(
            lb_records(&natd, 1, None).0.is_empty(),
            "NAT'd node (no public IP) excluded"
        );
    }

    #[test]
    fn aaaa_uses_public_ip6_and_skips_unspecified() {
        let nodes = vec![
            ni("v6", true, Some("203.0.113.1"), Some("2001:db8::1"), 0),
            ni("v6-bogus", true, None, Some("::"), 1), // unspecified → excluded
            ni("v4-only", true, Some("203.0.113.2"), None, 2), // no v6 → no AAAA
        ];
        let (aaaa, _) = lb_records(&nodes, 28, None);
        assert_eq!(aaaa.len(), 1, "only the node with a real public IPv6");
        assert_eq!(aaaa[0].0, 28, "AAAA record");
        assert_eq!(
            aaaa[0].2,
            "2001:db8::1".parse::<Ipv6Addr>().unwrap().octets().to_vec()
        );
        // And A still works for the v4 nodes.
        assert_eq!(lb_records(&nodes, 1, None).0.len(), 2);
    }

    #[test]
    fn answers_ordered_lowest_latency_first() {
        let nodes = vec![
            ni("far", true, Some("203.0.113.3"), None, 80),
            ni("near", true, Some("203.0.113.1"), None, 2),
            ni("mid", true, Some("203.0.113.2"), None, 30),
        ];
        let (a, _) = lb_records(&nodes, 1, None);
        let ips: Vec<Vec<u8>> = a.iter().map(|r| r.2.clone()).collect();
        assert_eq!(
            ips[0],
            Ipv4Addr::new(203, 0, 113, 1).octets().to_vec(),
            "nearest first"
        );
        assert_eq!(
            ips[2],
            Ipv4Addr::new(203, 0, 113, 3).octets().to_vec(),
            "farthest last"
        );
    }
}
