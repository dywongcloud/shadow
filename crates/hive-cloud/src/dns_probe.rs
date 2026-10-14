//! **Prove-before-advertise for the geo zone's nameservers.**
//!
//! `NodeInfo::dns_ns` records only that a node *intended* to bind a public `:53`: a string read out
//! of this node's own environment, a statement of configuration and not of reachability. Two
//! failure shapes show what that costs, and the node reported itself perfectly healthy in both: a
//! nameserver whose queries never reach the host at all (a cloud security group), so no check
//! performed ON that host could ever notice; and one that answers authoritatively with ZERO answer
//! records for some clients (a proximity truncation that ran before the address-family filter),
//! which a liveness check asking 'did a packet come back' calls healthy. A recursive resolver that
//! happens to pick either one gets a timeout or an empty answer, so every name under the zone
//! resolves intermittently. This module is the fix: **a node is advertised as a nameserver only
//! while other nodes can currently prove, from their own hosts, that it answers.**
//!
//! ## Why peer attestation (and not a self-check)
//!
//! A self-check (bind test, loopback query, 'is my listener alive') is worthless for this: both
//! failures are invisible from inside the host, one being an inbound drop upstream of the NIC and
//! the other a wrong answer that only manifests for *other people's* clients. Off-host evidence
//! needs a second machine, and the mesh already has several in different regions, already
//! exchanging one gossip record per node per round. So every node queries every candidate
//! nameserver's PUBLIC `:53` over the public internet, the same question in the same wire format a
//! recursive resolver asks, and gossips the list of nodes that answered ([`NodeInfo::dns_attest`]).
//!
//! Three deliberate constraints on what counts as proof:
//!
//! 1. **Answering is not enough; the answer must be usable.** A probe passes only on `NOERROR` +
//!    `AA` + at least one address record of the queried family.
//! 2. **Two distinct attester REGIONS, not two attesters.** A cloud-level inbound block is
//!    invisible from a peer in the same datacenter or VPC, the vantage most likely to sit *inside*
//!    whatever still permits the traffic; vantages in two regions mean at least one long-haul
//!    internet path has been proven. It degrades to one when the fleet genuinely has only one other
//!    region, because refusing to ever delegate is worse.
//! 3. **Never self-attest.** A node's own opinion of itself is the thing this module exists to stop
//!    trusting.
//!
//! ## Why the probe also asks on behalf of real clients (EDNS Client Subnet)
//!
//! The empty-answer defect is CLIENT-LOCATION SPECIFIC: the same live server can answer a nearby
//! client with several records and a client subnet from another region with none, so a probe that
//! only asks on the prober's own behalf is structurally blind to the whole class. Each round
//! therefore also asks the candidate a few questions carrying an EDNS Client Subnet option for REAL
//! client networks this node has already served (`GeoCache`'s located set: no invented address
//! list, and it samples exactly the population that would be hurt); an empty answer for any of them
//! fails the round. This samples rather than proves for all clients: the sample rotates each round
//! so coverage accumulates, but a defect that only affects a client network nobody in the fleet has
//! served yet is not caught until one is.
//!
//! ## Cost and cadence
//!
//! A handful of ~100-byte UDP datagrams per candidate per round (default 30 s). Withdrawal is
//! damped by two consecutive failed rounds (`FAILED_ROUNDS_BEFORE_WITHDRAW`, the same K the DNS
//! reconciler uses for host health), so one lost UDP datagram cannot pull a nameserver out of a
//! live delegation.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hive_core::now_ms;
use hive_edge::NodeInfo;
use parking_lot::RwLock;

use crate::dns_geo::ClientSubnet;
use crate::state::CloudState;

/// Consecutive failed probe rounds before this node stops attesting a peer. Matches
/// `vercel_dns::UNHEALTHY_PASSES_BEFORE_WITHDRAW`: a single dropped UDP datagram must not evict a
/// working nameserver from a live delegation.
pub const FAILED_ROUNDS_BEFORE_WITHDRAW: u32 = 2;

/// Distinct attester REGIONS required to consider a nameserver proven. See the
/// module doc for why this is regions and not a raw attester count.
pub const MIN_ATTESTER_REGIONS: usize = 2;

/// How many real client subnets each round asks on behalf of. Small on purpose: the sample ROTATES,
/// so coverage accumulates across rounds instead of costing a burst of queries every 30 seconds.
const ECS_SAMPLE: usize = 3;

/// Builds a DNS query for `qname`/`qtype`, optionally carrying an EDNS Client Subnet option (scope
/// 0, RFC 7871 section 6: scope is the responder's field) so the candidate answers as if for a
/// client in `ecs`. The header is the id, flags 0 (a standard QUERY with `RD=0`), QDCOUNT 1,
/// ANCOUNT 0, NSCOUNT 0 and ARCOUNT 1 when the OPT RR follows the question (QCLASS IN).
///
/// `RD=0` deliberately: this is a direct question to a server that must be AUTHORITATIVE for the
/// name, and asking with recursion desired would let a node that is not serving the zone at all
/// still look healthy by fetching the answer from somewhere else.
fn build_query(id: u16, qname: &str, qtype: u16, ecs: Option<ClientSubnet>) -> Vec<u8> {
    let mut m = Vec::with_capacity(64);
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&0u16.to_be_bytes());
    m.extend_from_slice(&1u16.to_be_bytes());
    m.extend_from_slice(&0u16.to_be_bytes());
    m.extend_from_slice(&0u16.to_be_bytes());
    m.extend_from_slice(&(ecs.is_some() as u16).to_be_bytes());
    for label in qname
        .trim_end_matches('.')
        .split('.')
        .filter(|l| !l.is_empty())
    {
        let b = label.as_bytes();
        let n = b.len().min(63);
        m.push(n as u8);
        m.extend_from_slice(&b[..n]);
    }
    m.push(0);
    m.extend_from_slice(&qtype.to_be_bytes());
    m.extend_from_slice(&1u16.to_be_bytes());
    if ecs.is_some() {
        m.extend_from_slice(&crate::dns_geo::encode_opt_rr(ecs, 0));
    }
    m
}

fn rcode_name(rcode: u8) -> &'static str {
    match rcode {
        1 => "FORMERR",
        2 => "SERVFAIL",
        3 => "NXDOMAIN",
        4 => "NOTIMP",
        5 => "REFUSED",
        _ => "OTHER",
    }
}

/// Counts the usable answer records of `qtype` in a response, or explains why the response is not
/// acceptable evidence. Everything here is a REJECTION reason a real resolver would also act on: a
/// mismatched id (not our answer), a non-authoritative reply (this server is not actually serving
/// the zone), a non-zero rcode, or a truncated or garbled message. Only an A (4-byte) or AAAA
/// (16-byte) rdata counts as usable, since anything else under those types is not an address a
/// client could connect to. `Ok(0)`, a well-formed authoritative NOERROR with no addresses, is
/// returned as such so the caller can name it `empty-answer`, the exact live defect this exists to
/// catch.
fn parse_answers(id: u16, qtype: u16, msg: &[u8]) -> Result<usize, String> {
    let bad = |s: &str| Err(s.to_string());
    if msg.len() < 12 {
        return bad("short-response");
    }
    if [msg[0], msg[1]] != id.to_be_bytes() {
        return bad("id-mismatch");
    }
    if msg[2] & 0x80 == 0 {
        return bad("not-a-response");
    }
    if msg[2] & 0x04 == 0 {
        return bad("not-authoritative");
    }
    let rcode = msg[3] & 0x0F;
    if rcode != 0 {
        return Err(format!("rcode={}", rcode_name(rcode)));
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
    let ancount = u16::from_be_bytes([msg[6], msg[7]]);
    let mut off = 12usize;
    for _ in 0..qdcount {
        off = match crate::dns_geo::skip_name(msg, off) {
            Some(o) => o,
            None => return bad("malformed-question"),
        };
        off += 4;
        if off > msg.len() {
            return bad("malformed-question");
        }
    }
    let want_rdlen = if qtype == 28 { 16 } else { 4 };
    let mut usable = 0usize;
    for _ in 0..ancount {
        off = match crate::dns_geo::skip_name(msg, off) {
            Some(o) => o,
            None => return bad("malformed-answer"),
        };
        if off + 10 > msg.len() {
            return bad("malformed-answer");
        }
        let rtype = u16::from_be_bytes([msg[off], msg[off + 1]]);
        let rdlen = u16::from_be_bytes([msg[off + 8], msg[off + 9]]) as usize;
        off += 10 + rdlen;
        if off > msg.len() {
            return bad("malformed-answer");
        }
        if rtype == qtype && rdlen == want_rdlen {
            usable += 1;
        }
    }
    Ok(usable)
}

/// Format a probed client subnet for logs/stats (`1.2.3.0/24`).
fn fmt_subnet(cs: &ClientSubnet) -> String {
    format!("{}/{}", cs.addr, cs.source_prefix)
}

/// TXT values `target` answers AUTHORITATIVELY for `qname`. Exists for ACME DNS-01 on a zone
/// delegated to this fleet's own nameservers: Let's Encrypt queries a RANDOM one of them, so a
/// challenge present on one server and missing from another fails validation intermittently, and a
/// public resolver cannot detect it (it caches and asks one server, so one success masks the rest).
/// The only honest check is to ask every nameserver directly, which is what [`super::acme`] does.
pub async fn txt_values(
    target: SocketAddr,
    qname: &str,
    timeout: Duration,
) -> Result<Vec<String>, String> {
    rr_values(target, qname, 16, timeout, true).await
}

/// Answers of `qtype` that `target` returns for `qname`. `require_aa` is what separates the two
/// callers: a fleet nameserver is being asked whether it SERVES the zone (a non-authoritative reply
/// proves nothing, so it is rejected), while a public RECURSIVE resolver answers with `AA` clear by
/// design and its answer is still the public internet's answer.
pub async fn rr_values(
    target: SocketAddr,
    qname: &str,
    qtype: u16,
    timeout: Duration,
    require_aa: bool,
) -> Result<Vec<String>, String> {
    let bind: SocketAddr = if target.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let sock = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|e| format!("bind: {e}"))?;
    sock.connect(target)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let id = uuid::Uuid::new_v4().as_u128() as u16;
    let msg = build_query(id, qname, qtype, None);
    sock.send(&msg).await.map_err(|e| format!("send: {e}"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 1500];
    loop {
        let n = match tokio::time::timeout_at(deadline, sock.recv(&mut buf)).await {
            Err(_) => return Err("timeout".into()),
            Ok(Err(e)) => return Err(format!("recv: {e}")),
            Ok(Ok(n)) => n,
        };
        match parse_rr(id, &buf[..n], qtype, require_aa) {
            Err(e) if e == "id-mismatch" => continue,
            other => return other,
        }
    }
}

/// Parse the answers of `qtype` in a response, rejecting anything a real resolver would reject:
/// id mismatch, non-zero rcode, truncation. `require_aa` additionally rejects a non-authoritative
/// reply ([`rr_values`] explains which caller needs that). TXT rdata is one or more
/// `<length><bytes>` strings (RFC 1035), concatenated into one value per record; A rdata is 4 bytes,
/// rendered dotted-quad so a caller can compare it against the addresses it already knows.
fn parse_rr(
    id: u16,
    msg: &[u8],
    qtype: u16,
    require_aa: bool,
) -> Result<Vec<String>, String> {
    let bad = |s: &str| Err(s.to_string());
    if msg.len() < 12 {
        return bad("short-response");
    }
    if [msg[0], msg[1]] != id.to_be_bytes() {
        return bad("id-mismatch");
    }
    if msg[2] & 0x80 == 0 {
        return bad("not-a-response");
    }
    if msg[2] & 0x02 != 0 {
        // Truncated: the useful records may be the ones that did not fit, so the reply is not
        // evidence either way and the caller must treat this vantage as having no opinion.
        return bad("truncated");
    }
    if require_aa && msg[2] & 0x04 == 0 {
        return bad("not-authoritative");
    }
    let rcode = msg[3] & 0x0F;
    if rcode != 0 {
        return Err(format!("rcode={}", rcode_name(rcode)));
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
    let ancount = u16::from_be_bytes([msg[6], msg[7]]);
    let mut off = 12usize;
    for _ in 0..qdcount {
        loop {
            if off >= msg.len() {
                return bad("truncated-question");
            }
            let l = msg[off] as usize;
            off += 1;
            if l == 0 {
                break;
            }
            off += l;
        }
        off += 4;
    }
    let mut out = Vec::new();
    for _ in 0..ancount {
        loop {
            if off >= msg.len() {
                return bad("truncated-answer");
            }
            let l = msg[off] as usize;
            if l & 0xC0 == 0xC0 {
                off += 2;
                break;
            }
            off += 1;
            if l == 0 {
                break;
            }
            off += l;
        }
        if off + 10 > msg.len() {
            return bad("truncated-rr");
        }
        let rtype = u16::from_be_bytes([msg[off], msg[off + 1]]);
        let rdlen = u16::from_be_bytes([msg[off + 8], msg[off + 9]]) as usize;
        let start = off + 10;
        if start + rdlen > msg.len() {
            return bad("truncated-rdata");
        }
        if rtype == qtype {
            match qtype {
                1 if rdlen == 4 => out.push(format!(
                    "{}.{}.{}.{}",
                    msg[start],
                    msg[start + 1],
                    msg[start + 2],
                    msg[start + 3]
                )),
                16 => {
                    let mut i = 0usize;
                    let mut v = String::new();
                    while i < rdlen {
                        let l = msg[start + i] as usize;
                        i += 1;
                        if i + l > rdlen {
                            break;
                        }
                        v.push_str(&String::from_utf8_lossy(&msg[start + i..start + i + l]));
                        i += l;
                    }
                    out.push(v);
                }
                _ => {}
            }
        }
        off = start + rdlen;
    }
    Ok(out)
}

/// Asks EVERY [`crate::dns_quorum::Vantage`] for `qname`, in parallel, and returns what each
/// OPERATOR said. Parallel, because one resolver's latency or black hole must not be added to
/// another's; bounded, because each vantage carries its own deadline, so the SLOWEST vantage bounds
/// the answer rather than the sum of them.
///
/// The classification in here is the whole reason the caller can trust the tally: NXDOMAIN is an
/// ANSWER (the internet really does not have the name — a genuine "no", not silence), while a
/// timeout, SERVFAIL, REFUSED or truncated reply is this vantage having NO opinion and therefore
/// abstaining. Counting the second kind as a "no" is what let one resolver's bad minute silently
/// drop a SAN from every order.
pub async fn public_answers(
    http: &reqwest::Client,
    qname: &str,
    qtype: u16,
) -> Vec<(&'static str, crate::dns_quorum::Answer)> {
    use crate::dns_quorum::{Answer, Endpoint};
    let udp_timeout = Duration::from_secs(3);
    futures::future::join_all(crate::dns_quorum::VANTAGES.iter().map(|v| async move {
        let answer = match v.endpoint {
            Endpoint::Https(url) => doh_answer(http, url, qname, qtype).await,
            Endpoint::Udp(ip) => {
                let Some(addr) = ip
                    .parse::<std::net::IpAddr>()
                    .ok()
                    .map(|a| SocketAddr::new(a, 53))
                else {
                    return (v.operator, Answer::Unavailable);
                };
                match rr_values(addr, qname, qtype, udp_timeout, false).await {
                    Ok(vals) => Answer::Records(vals),
                    Err(e) if e == "rcode=NXDOMAIN" => Answer::Records(Vec::new()),
                    Err(_) => Answer::Unavailable,
                }
            }
        };
        (v.operator, answer)
    }))
    .await
}

/// One JSON-DoH vantage. The name is passed through `.query()` rather than `format!`-ed into the
/// URL: it is a tenant-influenced string, and interpolating it would let a crafted one add a second
/// parameter to somebody else's resolver query.
async fn doh_answer(
    http: &reqwest::Client,
    url: &str,
    qname: &str,
    qtype: u16,
) -> crate::dns_quorum::Answer {
    use crate::dns_quorum::Answer;
    let qtype_s = qtype.to_string();
    let Ok(resp) = http
        .get(url)
        .query(&[("name", qname), ("type", qtype_s.as_str())])
        .header("accept", "application/dns-json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
    else {
        return Answer::Unavailable;
    };
    let Ok(v) = resp.json::<serde_json::Value>().await else {
        return Answer::Unavailable;
    };
    match v.get("Status").and_then(|s| s.as_u64()).unwrap_or(0) {
        0 => {}
        3 => return Answer::Records(Vec::new()),
        _ => return Answer::Unavailable,
    }
    let mut out = Vec::new();
    if let Some(arr) = v.get("Answer").and_then(|a| a.as_array()) {
        for rec in arr {
            if rec.get("type").and_then(|t| t.as_u64()) != Some(qtype as u64) {
                continue;
            }
            if let Some(d) = rec.get("data").and_then(|d| d.as_str()) {
                out.push(d.to_string());
            }
        }
    }
    Answer::Records(out)
}

/// Sends ONE query to `target` and reports the usable answer count, or why not. Late or stray
/// datagrams that do not match our query id are skipped rather than treated as the answer, but the
/// whole receive window is bounded by a single deadline so a chatty or hostile peer cannot hold the
/// probe open.
pub async fn probe_query(
    target: SocketAddr,
    qname: &str,
    qtype: u16,
    ecs: Option<ClientSubnet>,
    timeout: Duration,
) -> Result<usize, String> {
    let bind: SocketAddr = if target.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let sock = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|e| format!("bind: {e}"))?;
    sock.connect(target)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let id = uuid::Uuid::new_v4().as_u128() as u16;
    let msg = build_query(id, qname, qtype, ecs);
    sock.send(&msg).await.map_err(|e| format!("send: {e}"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 1500];
    loop {
        let n = match tokio::time::timeout_at(deadline, sock.recv(&mut buf)).await {
            Err(_) => return Err("timeout".into()),
            Ok(Err(e)) => return Err(format!("recv: {e}")),
            Ok(Ok(n)) => n,
        };
        match parse_answers(id, qtype, &buf[..n]) {
            Err(e) if e == "id-mismatch" => continue,
            other => return other,
        }
    }
}

/// True for a failure that is a lost or blocked DATAGRAM rather than a verdict. Retrying these once
/// is honest (UDP is lossy and a probe round asks several questions, so the chance of one unlucky
/// loss compounds); retrying an `empty-answer`, `rcode=...` or `not-authoritative` would not be,
/// since those are deterministic answers the server actually gave, and re-asking until it says
/// something nicer is exactly how a check stops meaning anything.
fn transport_failure(reason: &str) -> bool {
    reason == "timeout" || reason.starts_with("recv:") || reason.starts_with("send:")
}

/// [`probe_query`] with a single retry on a transport-level failure.
async fn probe_query_once_retried(
    target: SocketAddr,
    qname: &str,
    qtype: u16,
    ecs: Option<ClientSubnet>,
    timeout: Duration,
) -> Result<usize, String> {
    match probe_query(target, qname, qtype, ecs, timeout).await {
        Err(e) if transport_failure(&e) => probe_query(target, qname, qtype, ecs, timeout).await,
        other => other,
    }
}

/// One probe round against one candidate nameserver.
#[derive(Clone, Debug)]
pub struct ProbeReport {
    /// The candidate answered every question in this round with a usable,
    /// authoritative address answer.
    pub ok: bool,
    /// `ok` or the first failure, naming the client subnet when the failure was
    /// specific to one (`empty-answer (client 12.218.212.0/24)`).
    pub reason: String,
    pub rtt_ms: u64,
    /// Usable address records in the baseline (no-ECS) answer.
    pub answers: usize,
    /// Queries sent this round (1 baseline + one per sampled client subnet).
    pub queries: usize,
    /// The client subnets this round asked on behalf of.
    pub subnets: Vec<String>,
}

/// Proves, or fails to prove, that `ip` serves `zone` as an authoritative nameserver, from THIS
/// host, over the public internet. Round shape: one baseline query on our own behalf (does the
/// listener exist, is it reachable from here, does it answer authoritatively with an address), then
/// one query per sampled real client subnet (does it answer usefully for clients that are not us).
/// The first failure wins and short-circuits: the report is evidence for a yes/no decision, not a
/// survey.
pub async fn probe_nameserver(
    ip: IpAddr,
    zone: &str,
    clients: &[ClientSubnet],
    timeout: Duration,
) -> ProbeReport {
    let target = SocketAddr::new(ip, 53);
    let started = std::time::Instant::now();
    let mut queries = 1usize;
    let (mut ok, mut reason, answers) =
        match probe_query_once_retried(target, zone, 1, None, timeout).await {
            Err(e) => (false, e, 0),
            Ok(0) => (false, "empty-answer".to_string(), 0),
            Ok(n) => (true, "ok".to_string(), n),
        };
    if ok {
        for cs in clients {
            queries += 1;
            match probe_query_once_retried(target, zone, 1, Some(*cs), timeout).await {
                Err(e) => {
                    ok = false;
                    reason = format!("{e} (client {})", fmt_subnet(cs));
                    break;
                }
                Ok(0) => {
                    ok = false;
                    reason = format!("empty-answer (client {})", fmt_subnet(cs));
                    break;
                }
                Ok(_) => {}
            }
        }
    }
    ProbeReport {
        ok,
        reason,
        rtt_ms: started.elapsed().as_millis() as u64,
        answers,
        queries,
        subnets: clients.iter().map(fmt_subnet).collect(),
    }
}

/// This node's own latest observation of one peer's nameserver.
#[derive(Clone, Debug)]
pub struct PeerProbe {
    pub ip: String,
    pub ok: bool,
    pub reason: String,
    pub rtt_ms: u64,
    pub answers: usize,
    pub queries: usize,
    pub subnets: Vec<String>,
    pub checked_ms: u64,
    /// Consecutive failed rounds (0 while passing).
    pub fail_streak: u32,
    /// Whether this node currently ATTESTS the peer — `ok`, or failing but
    /// still inside the damping window.
    pub attested: bool,
}

/// Everything this node has directly observed about its peers' nameservers. Node-local by nature
/// (it IS this node's vantage); the derived attestation list is what gets gossiped.
#[derive(Default)]
pub struct NsProbes {
    inner: RwLock<HashMap<String, PeerProbe>>,
}

impl NsProbes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<(String, PeerProbe)> {
        let mut v: Vec<(String, PeerProbe)> = self
            .inner
            .read()
            .iter()
            .map(|(k, p)| (k.clone(), p.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Fold one round's result in, applying the withdrawal damping, and return
    /// whether this node attests the peer after it.
    fn record(&self, node: &str, ip: IpAddr, r: ProbeReport) -> bool {
        let mut w = self.inner.write();
        let prev_streak = w.get(node).map(|p| p.fail_streak).unwrap_or(0);
        let fail_streak = if r.ok {
            0
        } else {
            prev_streak.saturating_add(1)
        };
        let attested = r.ok || fail_streak < FAILED_ROUNDS_BEFORE_WITHDRAW;
        w.insert(
            node.to_string(),
            PeerProbe {
                ip: ip.to_string(),
                ok: r.ok,
                reason: r.reason,
                rtt_ms: r.rtt_ms,
                answers: r.answers,
                queries: r.queries,
                subnets: r.subnets,
                checked_ms: now_ms(),
                fail_streak,
                attested,
            },
        );
        attested
    }

    /// Drop observations for nodes that left the registry, so a renamed or
    /// decommissioned node cannot linger in this node's attestation list.
    fn retain(&self, known: &std::collections::HashSet<String>) {
        self.inner.write().retain(|k, _| known.contains(k));
    }
}

/// Whether one node may be advertised as a nameserver, and the evidence behind
/// that answer. Surfaced verbatim by `GET /v1/dns/stats`.
#[derive(Clone, Debug)]
pub struct NsVerdict {
    pub node: String,
    pub region: String,
    pub ip4: Option<String>,
    pub ip6: Option<String>,
    /// The node CLAIMS to serve DNS (`NodeInfo::dns_ns`, its own env).
    pub declared: bool,
    /// Peers that currently attest it answers from their host.
    pub attesters: Vec<String>,
    /// Distinct regions those attesters sit in — the number that actually gates.
    pub attester_regions: Vec<String>,
    pub required_regions: usize,
    pub validated: bool,
    pub reason: String,
}

/// Decides, from the gossiped registry alone, which declared nameservers are currently PROVEN.
/// Pure: the reconciler and the stats endpoint call the same function, so an operator is never
/// looking at a different answer than the one being published. A node is never its own attester,
/// and a peer this node considers down is not counted as a vantage either (a node that fails our
/// health probes has no DNS opinion worth acting on). The required region count is capped by the
/// vantages the fleet can offer: a two-region fleet cannot produce two attester regions for a
/// candidate that owns one of them, and refusing to ever delegate would be a worse failure than a
/// thinner proof.
pub fn validate_nameservers(nodes: &[NodeInfo]) -> Vec<NsVerdict> {
    let region_of: HashMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.name.as_str(), n.region.as_str()))
        .collect();
    let mut out = Vec::new();
    for cand in nodes.iter().filter(|n| n.dns_ns.is_some()) {
        let has_addr = cand.public_ip.is_some() || cand.public_ip6.is_some();
        let mut attesters: Vec<String> = nodes
            .iter()
            .filter(|a| a.name != cand.name && a.healthy)
            .filter(|a| a.dns_attest.iter().any(|x| x == &cand.name))
            .map(|a| a.name.clone())
            .collect();
        attesters.sort();
        attesters.dedup();
        let mut attester_regions: Vec<String> = attesters
            .iter()
            .filter_map(|a| region_of.get(a.as_str()).map(|r| (*r).to_string()))
            .collect();
        attester_regions.sort();
        attester_regions.dedup();
        let mut other_regions: Vec<&str> = nodes
            .iter()
            .filter(|n| n.name != cand.name)
            .map(|n| n.region.as_str())
            .collect();
        other_regions.sort();
        other_regions.dedup();
        let required_regions = MIN_ATTESTER_REGIONS.min(other_regions.len().max(1));
        let validated = has_addr && attester_regions.len() >= required_regions;
        let reason = if !has_addr {
            "no public address — nothing to publish glue for".to_string()
        } else if validated {
            format!(
                "proven by {} peer(s) across {} region(s)",
                attesters.len(),
                attester_regions.len()
            )
        } else {
            format!(
                "unproven: {} attester(s) across {} region(s), need {}",
                attesters.len(),
                attester_regions.len(),
                required_regions
            )
        };
        out.push(NsVerdict {
            node: cand.name.clone(),
            region: cand.region.clone(),
            ip4: cand.public_ip.clone(),
            ip6: cand.public_ip6.clone(),
            declared: true,
            attesters,
            attester_regions,
            required_regions,
            validated,
            reason,
        });
    }
    out.sort_by(|a, b| a.node.cmp(&b.node));
    out
}

fn probe_timeout() -> Duration {
    Duration::from_millis(
        std::env::var("HIVE_DNS_PROBE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(2000),
    )
}

/// Turn a cached client NETWORK KEY (`GeoCache` stores /24 v4, /48 v6) back
/// into the ECS option that asks a nameserver to answer for that client.
fn subnet_of(net: IpAddr) -> ClientSubnet {
    ClientSubnet {
        addr: net,
        source_prefix: if net.is_ipv4() { 24 } else { 48 },
    }
}

/// Every node runs this: probes every peer that claims to be a nameserver every
/// `HIVE_DNS_PROBE_SECS` (default 30) and gossips the ones that currently answer. Not
/// leader-elected on purpose: the whole point is independent vantages, and a single prober would be
/// a single vantage with a single network path. The first round waits 15 s for gossip to converge
/// (probing an empty registry would produce an empty attestation list that peers would then act
/// on), and a registry that currently sees only self is a transient un-synced view, not evidence
/// that every nameserver vanished, so the round is skipped rather than gossiping an empty list
/// (harmless, since the reconciler HOLDS rather than deletes, but a needless fleet-wide flap), the
/// convergence guard of the DNS reconciler. The client subnets asked on behalf of rotate through
/// the networks this node has really served, `ECS_SAMPLE` per round, so successive rounds cover
/// different parts of the population without a burst.
pub fn spawn_ns_prober(cloud: Arc<CloudState>) {
    let Some(zone) = crate::dnsserver::deploy_zone() else {
        tracing::info!(
            "nameserver prover idle: HIVE_DEPLOY_ZONE unset — no zone is delegated here to prove"
        );
        return;
    };
    let interval = Duration::from_secs(
        std::env::var("HIVE_DNS_PROBE_SECS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(30),
    );
    tracing::info!(%zone, secs = interval.as_secs(), "nameserver prover up (peer-attested prove-before-advertise)");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let mut round: usize = 0;
        loop {
            round = round.wrapping_add(1);
            let nodes = cloud.registry.nodes();
            if nodes.len() <= 1 {
                tokio::time::sleep(interval).await;
                continue;
            }
            let known: std::collections::HashSet<String> =
                nodes.iter().map(|n| n.name.clone()).collect();
            cloud.dns_probes.retain(&known);
            let candidates: Vec<(String, IpAddr)> = nodes
                .iter()
                .filter(|n| !n.is_self && n.dns_ns.is_some())
                .filter_map(|n| {
                    n.public_ip
                        .as_deref()
                        .and_then(|s| s.parse::<IpAddr>().ok())
                        .or_else(|| {
                            n.public_ip6
                                .as_deref()
                                .and_then(|s| s.parse::<IpAddr>().ok())
                        })
                        .map(|ip| (n.name.clone(), ip))
                })
                .collect();
            let mut nets = cloud.dns_geo.known_networks();
            nets.retain(|n| !n.is_loopback());
            let clients: Vec<ClientSubnet> = if nets.is_empty() {
                Vec::new()
            } else {
                (0..ECS_SAMPLE.min(nets.len()))
                    .map(|i| {
                        subnet_of(
                            nets[(round.wrapping_mul(ECS_SAMPLE).wrapping_add(i)) % nets.len()],
                        )
                    })
                    .collect()
            };
            let timeout = probe_timeout();
            let results = futures::future::join_all(candidates.into_iter().map(|(name, ip)| {
                let clients = clients.clone();
                async move {
                    (
                        name,
                        ip,
                        probe_nameserver(ip, zone, &clients, timeout).await,
                    )
                }
            }))
            .await;
            let mut attest: Vec<String> = Vec::new();
            for (name, ip, report) in results {
                let was = cloud
                    .dns_probes
                    .snapshot()
                    .into_iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, p)| p.attested);
                let ok = report.ok;
                let reason = report.reason.clone();
                let attested = cloud.dns_probes.record(&name, ip, report);
                if attested {
                    attest.push(name.clone());
                }
                match (was, attested) {
                    (Some(true), false) => tracing::error!(
                        node = %name, %ip, reason = %reason,
                        "nameserver WITHDRAWN: no longer answers DNS from this host — dropping attestation"
                    ),
                    (Some(false), true) => {
                        tracing::info!(node = %name, %ip, "nameserver recovered: answering DNS from this host again")
                    }
                    _ if !ok => {
                        tracing::warn!(node = %name, %ip, reason = %reason, "nameserver probe failed")
                    }
                    _ => {}
                }
            }
            attest.sort();
            cloud.registry.set_self_dns_attest(attest);
            tokio::time::sleep(interval).await;
        }
    });
}

/// `hive-cloud --dns-probe <ip>[,<ip>...]`: runs the REAL probe from this host against specific
/// addresses and prints the verdict, then exits. It is the same code path the prober loop runs,
/// deliberately: 'is this nameserver actually serving?' is a question an operator must answer from
/// an arbitrary vantage (a laptop, a peer, a bastion) during an incident, and answering it with a
/// different implementation than the one making the decision is how the two quietly diverge. The
/// zone comes from `HIVE_DEPLOY_ZONE`; `HIVE_DNS_PROBE_SUBNETS` (comma-separated CIDRs) adds client
/// subnets to ask on behalf of.
pub async fn run_cli(targets: &[String]) -> anyhow::Result<()> {
    let zone = std::env::var("HIVE_DEPLOY_ZONE")
        .ok()
        .map(|s| s.trim().trim_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("HIVE_DEPLOY_ZONE must be set (the delegated zone to prove)")
        })?;
    let clients: Vec<ClientSubnet> = std::env::var("HIVE_DNS_PROBE_SUBNETS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let (addr, plen) = s.split_once('/')?;
            Some(ClientSubnet {
                addr: addr.parse().ok()?,
                source_prefix: plen.parse().ok()?,
            })
        })
        .collect();
    let timeout = probe_timeout();
    let mut failures = 0usize;
    for t in targets {
        let Ok(ip) = t.trim().parse::<IpAddr>() else {
            println!("{t:<20} INVALID  not an IP address");
            failures += 1;
            continue;
        };
        let r = probe_nameserver(ip, &zone, &clients, timeout).await;
        if !r.ok {
            failures += 1;
        }
        println!(
            "{:<20} {:<7} answers={} queries={} rtt={}ms  {}",
            ip,
            if r.ok { "SERVING" } else { "BROKEN" },
            r.answers,
            r.queries,
            r.rtt_ms,
            r.reason
        );
    }
    if failures > 0 {
        anyhow::bail!(
            "{failures} of {} nameserver(s) failed to prove they serve {zone}",
            targets.len()
        );
    }
    Ok(())
}
