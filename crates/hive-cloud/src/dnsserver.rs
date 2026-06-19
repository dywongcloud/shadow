//! A real authoritative DNS server: answers queries from the platform's own DNS
//! records (`dns::DomainStore`) over UDP. No heavy dependency — a focused wire-
//! format implementation handling the records the dashboard manages (A, AAAA,
//! CNAME, TXT). Binds a non-privileged port by default (`HIVE_DNS_ADDR`, default
//! 127.0.0.1:5354) so it runs without root; point a resolver / `dig` at it.
//!
//! This replaces the previous "records exist in memory but nothing answers DNS"
//! gap: stored records now actually resolve.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::state::CloudState;

pub async fn serve(cloud: Arc<CloudState>, addr: SocketAddr) -> std::io::Result<()> {
    let sock = UdpSocket::bind(addr).await?;
    tracing::info!(%addr, "authoritative DNS server listening (UDP)");
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
