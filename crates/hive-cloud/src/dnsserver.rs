//! **Seer (Plane A, client→node): the platform's authoritative DNS.** Answers queries
//! over UDP **and** TCP. Two kinds of answer:
//!   * **Static** records from the dashboard-managed `dns::DomainStore` (A, AAAA,
//!     CNAME, TXT) — apex, `www`, custom domains, CAA/TXT.
//!   * **Dynamic, health-aware** A/AAAA for the deploy wildcard zone `HIVE_DEPLOY_ZONE`
//!     (e.g. `*.deploy.shadow.app`): returns the **public IPs of healthy nodes** so a
//!     browser connects straight to a reachable node — the piece that replaces ngrok as
//!     ingress. NAT'd / unhealthy nodes are excluded (a client must only get a node it
//!     can actually reach over HTTPS). It's a **local read** of the gossiped registry —
//!     no per-query network/DHT lookup (discovery stays off the hot path).
//!
//! NOTE: distinct from `discovery.rs` (Plane B, node↔node pkarr relay for iroh daemons).
//! Different protocol, consumer, job — the names once collided; keep them apart.
//!
//! Binds a non-privileged port by default (`HIVE_DNS_ADDR`, default 127.0.0.1:5354) so
//! it runs without root; set `HIVE_DNS_ADDR=0.0.0.0:53` in prod and delegate the zone's
//! NS here. Focused wire-format impl — no heavy dependency.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};

use tokio::net::{TcpListener, UdpSocket};

use crate::state::CloudState;
use hive_edge::NodeInfo;

/// The deploy wildcard zone whose A/AAAA are answered dynamically (healthy node IPs).
/// `HIVE_DEPLOY_ZONE`, e.g. `deploy.shadow.app` → `deploy.shadow.app` + `*.deploy.shadow.app`.
/// Cached: a per-query env read would be a syscall on the hot path. Lowercased, dot-trimmed.
fn deploy_zone() -> Option<&'static str> {
    static Z: OnceLock<Option<String>> = OnceLock::new();
    Z.get_or_init(|| {
        std::env::var("HIVE_DEPLOY_ZONE")
            .ok()
            .map(|s| s.trim().trim_matches('.').to_lowercase())
            .filter(|s| !s.is_empty())
    })
    .as_deref()
}

/// TTL (seconds) for dynamic deploy-zone answers — short, so an unhealthy node drains
/// from resolver caches quickly when it's dropped from the registry.
const DEPLOY_TTL: u32 = 60;

pub async fn serve(cloud: Arc<CloudState>, addr: SocketAddr) -> std::io::Result<()> {
    let sock = UdpSocket::bind(addr).await?;
    // TCP on the same addr: resolvers retry over TCP (truncation, some validators always
    // do); a real authoritative server must answer both. Best-effort — UDP is the hot path.
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
        if let Some(resp) = handle_query(&cloud, &buf[..n]) {
            let _ = sock.send_to(&resp, peer).await;
        }
    }
}

/// DNS-over-TCP: each message is framed by a 2-byte big-endian length prefix.
async fn serve_tcp(cloud: Arc<CloudState>, listener: TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut stream, _)) = listener.accept().await else { continue };
        let cloud = cloud.clone();
        tokio::spawn(async move {
            // One query per accept is sufficient for our resolvers; keep it simple.
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
            if let Some(resp) = handle_query(&cloud, &msg) {
                let mut framed = (resp.len() as u16).to_be_bytes().to_vec();
                framed.extend_from_slice(&resp);
                let _ = stream.write_all(&framed).await;
            }
        });
    }
}

/// Parse a DNS query and build a response from the platform's records.
fn handle_query(cloud: &Arc<CloudState>, q: &[u8]) -> Option<Vec<u8>> {
    if q.len() < 12 {
        return None;
    }
    let id = [q[0], q[1]];
    let rd = q[2] & 0x01; // recursion-desired bit, echoed back
    let qdcount = u16::from_be_bytes([q[4], q[5]]);
    if qdcount < 1 {
        return None;
    }

    // ---- parse the (first) question ----
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
            return None; // compression in question — unsupported (rare)
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
    let q_end = off + 4; // include qtype(2)+qclass(2)
    let question = &q[12..q_end];

    // ---- look up matching records ----
    let (answers, found_domain) = lookup(cloud, &qname, qtype);

    // ---- build response ----
    let mut resp = Vec::with_capacity(64);
    resp.extend_from_slice(&id);
    // flags: QR=1, AA=1, RD echoed, RA=0; rcode 0 (NOERROR) or 3 (NXDOMAIN)
    let rcode: u8 = if !found_domain { 3 } else { 0 };
    let flags: u16 = 0x8000 | 0x0400 | ((rd as u16) << 8) | rcode as u16;
    resp.extend_from_slice(&flags.to_be_bytes());
    resp.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    resp.extend_from_slice(&(answers.len() as u16).to_be_bytes()); // ANCOUNT
    resp.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    resp.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    resp.extend_from_slice(question);
    for (atype, ttl, rdata) in &answers {
        resp.extend_from_slice(&[0xC0, 0x0C]); // NAME → pointer to question (offset 12)
        resp.extend_from_slice(&atype.to_be_bytes());
        resp.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
        resp.extend_from_slice(&ttl.to_be_bytes());
        resp.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        resp.extend_from_slice(rdata);
    }
    Some(resp)
}

/// Returns (answer RRs, whether the zone/domain is authoritative here).
fn lookup(cloud: &Arc<CloudState>, qname: &str, qtype: u16) -> (Vec<(u16, u32, Vec<u8>)>, bool) {
    // ---- Plane A: dynamic, health-aware deploy zone (the Seer load balancer) ----
    // For any name in HIVE_DEPLOY_ZONE (apex or wildcard subdomain), A/AAAA resolve to
    // the public IPs of healthy nodes — bypassing static records entirely. We're
    // authoritative for the whole zone (NOERROR, never NXDOMAIN), so other types fall to
    // the clean insertion point below (ACME DNS-01 TXT / CAA land there in phase 2).
    if let Some(zone) = deploy_zone() {
        if qname == zone || qname.ends_with(&format!(".{zone}")) {
            match qtype {
                1 | 28 => return (lb_records(&cloud.registry.nodes(), qtype), true),
                // FORWARD-COMPAT (phase 2): `_acme-challenge.<zone>` TXT for ACME DNS-01,
                // and CAA, branch here. Phase 1 is authoritative no-data for them.
                _ => return (Vec::new(), true),
            }
        }
    }

    let domains = cloud.domains.snapshot();
    // Longest-suffix match → the authoritative zone for this query.
    let Some(zone) = domains
        .into_iter()
        .filter(|d| qname == d.domain || qname.ends_with(&format!(".{}", d.domain)))
        .max_by_key(|d| d.domain.len())
    else {
        return (Vec::new(), false);
    };

    // The record name within the zone ("" = apex).
    let rec_name = if qname == zone.domain {
        String::new()
    } else {
        qname.trim_end_matches(&format!(".{}", zone.domain)).to_string()
    };

    let want = match qtype {
        1 => "A",
        28 => "AAAA",
        5 => "CNAME",
        16 => "TXT",
        _ => "",
    };

    let mut out = Vec::new();
    for r in &zone.records {
        let name_match = r.name.eq_ignore_ascii_case(&rec_name) || r.name == "*";
        if !name_match {
            continue;
        }
        // For an A/AAAA query, a CNAME is a valid (and conventional) answer.
        let kind = r.kind.to_uppercase();
        let serve_as_cname = matches!(qtype, 1 | 28) && kind == "CNAME";
        if kind != want && !serve_as_cname {
            continue;
        }
        if let Some(rd) = encode_rdata(&kind, &r.value) {
            let atype = match kind.as_str() {
                "A" => 1u16,
                "AAAA" => 28,
                "CNAME" => 5,
                "TXT" => 16,
                _ => continue,
            };
            out.push((atype, r.ttl, rd));
        }
    }
    (out, true)
}

/// Build the dynamic A (qtype 1) or AAAA (qtype 28) answer set for the deploy zone:
/// the public IP of every node that is **both** `healthy` **and** has a public IP of the
/// requested family. NAT'd (no public IP) and unhealthy nodes are excluded — a browser
/// must only ever receive a node it can actually reach over HTTPS. Ordered lowest-latency
/// first (self = 0) for a sensible default; resolvers round-robin the set. Capped so the
/// answer comfortably fits a 512-byte UDP datagram.
fn lb_records(nodes: &[NodeInfo], qtype: u16) -> Vec<(u16, u32, Vec<u8>)> {
    let mut healthy: Vec<&NodeInfo> = nodes.iter().filter(|n| n.healthy).collect();
    healthy.sort_by_key(|n| n.latency_ms);
    let mut out = Vec::new();
    for n in healthy {
        match qtype {
            1 => {
                if let Some(ip) = n.public_ip.as_deref().and_then(|s| s.parse::<Ipv4Addr>().ok()) {
                    if !ip.is_unspecified() && !ip.is_loopback() {
                        out.push((1u16, DEPLOY_TTL, ip.octets().to_vec()));
                    }
                }
            }
            28 => {
                if let Some(ip) = n.public_ip6.as_deref().and_then(|s| s.parse::<Ipv6Addr>().ok()) {
                    if !ip.is_unspecified() && !ip.is_loopback() {
                        out.push((28u16, DEPLOY_TTL, ip.octets().to_vec()));
                    }
                }
            }
            _ => {}
        }
    }
    out.truncate(8);
    out
}

fn encode_rdata(kind: &str, value: &str) -> Option<Vec<u8>> {
    match kind {
        "A" => value.parse::<Ipv4Addr>().ok().map(|ip| ip.octets().to_vec()),
        "AAAA" => value.parse::<Ipv6Addr>().ok().map(|ip| ip.octets().to_vec()),
        "CNAME" => Some(encode_name(value)),
        "TXT" => {
            let bytes = value.as_bytes();
            if bytes.len() > 255 {
                return None;
            }
            let mut v = Vec::with_capacity(bytes.len() + 1);
            v.push(bytes.len() as u8);
            v.extend_from_slice(bytes);
            Some(v)
        }
        _ => None,
    }
}

fn encode_name(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name.trim_end_matches('.').split('.').filter(|l| !l.is_empty()) {
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
    fn ni(name: &str, healthy: bool, ip4: Option<&str>, ip6: Option<&str>, latency: u64) -> NodeInfo {
        NodeInfo {
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
            backend: String::new(),
        }
    }

    #[test]
    fn deploy_zone_returns_only_healthy_public_nodes() {
        let nodes = vec![
            ni("healthy-public", true, Some("203.0.113.10"), None, 5),
            ni("healthy-natd", true, None, None, 1), // NAT'd: no public IP → excluded
            ni("unhealthy-public", false, Some("203.0.113.99"), None, 0), // down → excluded
        ];
        let a = lb_records(&nodes, 1);
        assert_eq!(a.len(), 1, "only the healthy+public node is returned");
        assert_eq!(a[0].0, 1, "A record");
        assert_eq!(a[0].1, DEPLOY_TTL);
        assert_eq!(a[0].2, Ipv4Addr::new(203, 0, 113, 10).octets().to_vec());
    }

    #[test]
    fn marking_unhealthy_or_nat_removes_from_answers() {
        // Healthy+public → present; flip to unhealthy → gone; NAT'd never appears.
        let up = vec![ni("n1", true, Some("198.51.100.7"), None, 0)];
        assert_eq!(lb_records(&up, 1).len(), 1);
        let down = vec![ni("n1", false, Some("198.51.100.7"), None, 0)];
        assert!(lb_records(&down, 1).is_empty(), "unhealthy node excluded");
        let natd = vec![ni("n1", true, None, None, 0)];
        assert!(lb_records(&natd, 1).is_empty(), "NAT'd node (no public IP) excluded");
    }

    #[test]
    fn aaaa_uses_public_ip6_and_skips_unspecified() {
        let nodes = vec![
            ni("v6", true, Some("203.0.113.1"), Some("2001:db8::1"), 0),
            ni("v6-bogus", true, None, Some("::"), 1), // unspecified → excluded
            ni("v4-only", true, Some("203.0.113.2"), None, 2), // no v6 → no AAAA
        ];
        let aaaa = lb_records(&nodes, 28);
        assert_eq!(aaaa.len(), 1, "only the node with a real public IPv6");
        assert_eq!(aaaa[0].0, 28, "AAAA record");
        assert_eq!(aaaa[0].2, "2001:db8::1".parse::<Ipv6Addr>().unwrap().octets().to_vec());
        // And A still works for the v4 nodes.
        assert_eq!(lb_records(&nodes, 1).len(), 2);
    }

    #[test]
    fn answers_ordered_lowest_latency_first() {
        let nodes = vec![
            ni("far", true, Some("203.0.113.3"), None, 80),
            ni("near", true, Some("203.0.113.1"), None, 2),
            ni("mid", true, Some("203.0.113.2"), None, 30),
        ];
        let a = lb_records(&nodes, 1);
        let ips: Vec<Vec<u8>> = a.iter().map(|r| r.2.clone()).collect();
        assert_eq!(ips[0], Ipv4Addr::new(203, 0, 113, 1).octets().to_vec(), "nearest first");
        assert_eq!(ips[2], Ipv4Addr::new(203, 0, 113, 3).octets().to_vec(), "farthest last");
    }
}
