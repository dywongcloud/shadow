//! **Prove-before-advertise for the geo zone's nameservers.**
//!
//! `NodeInfo::dns_ns` records only that a node *intended* to bind a public
//! `:53` — it is a string read out of this node's own environment. That is a
//! statement of configuration, not of reachability, and the live delegation for
//! `deploy.shadw.app` showed exactly what that costs: of five advertised
//! nameservers, two were broken from the public internet while both reported
//! themselves perfectly healthy.
//!
//!   * `ns-fc-hongkong` (43.128.46.225) answered nothing at all — the query
//!     never reached the host (cloud security group), so no check performed ON
//!     that host could ever have noticed.
//!   * `ns-fc-sanjose-cvm-2` (43.166.233.114) answered, authoritatively, with
//!     ZERO answer records for some clients (the pre-85f8447 `lb_records` bug,
//!     where proximity truncation ran before the address-family filter). A
//!     liveness check that asks "did a packet come back" calls that node
//!     healthy.
//!
//! A recursive resolver that happens to pick either one gets a timeout or an
//! empty answer, so every name under the zone resolves intermittently. This
//! module is the fix: **a node is advertised as a nameserver only while other
//! nodes can currently prove, from their own hosts, that it answers.**
//!
//! ## Why peer attestation (and not a self-check)
//!
//! A self-check — bind test, loopback query, "is my listener alive" — is
//! worthless for this. Both live failures are invisible from inside the host:
//! one is an inbound drop upstream of the NIC, the other is a wrong answer that
//! only manifests for *other people's* clients. Off-host evidence requires a
//! second machine, and the mesh already has ten of them in five regions,
//! already exchanging one gossip record per node per round. So the cheapest
//! real evidence available is also the best one: every node queries every
//! candidate nameserver's PUBLIC `:53` over the public internet — the same
//! question, in the same wire format, that a recursive resolver asks — and
//! gossips the list of nodes that answered ([`NodeInfo::dns_attest`]).
//!
//! Three deliberate constraints on what counts as proof:
//!
//! 1. **Answering is not enough; the answer must be usable.** A probe passes
//!    only on `NOERROR` + `AA` + at least one address record of the queried
//!    family. `cvm-2` is precisely the node that a weaker bar would have kept
//!    advertising.
//! 2. **Two distinct attester REGIONS, not two attesters.** Hong Kong's failure
//!    is a cloud-level inbound block, and a peer in the same datacenter or VPC
//!    is exactly the vantage most likely to sit *inside* whatever still permits
//!    the traffic. Requiring vantages in two different regions means at least
//!    one long-haul internet path has actually been proven. (Degrades to one
//!    when the fleet genuinely has only one other region — the alternative is
//!    refusing to ever delegate, which is worse.)
//! 3. **Never self-attest.** A node's own opinion of itself is the thing this
//!    module exists to stop trusting.
//!
//! ## Why the probe also asks on behalf of real clients (EDNS Client Subnet)
//!
//! The `cvm-2` defect is CLIENT-LOCATION SPECIFIC, and that was measured, not
//! assumed: on 2026-07-28 the same live server answered `43.166.206.175`'s own
//! query with 8 records and answered a US-West client subnet with 0. A probe
//! that only ever asks on the prober's own behalf is structurally blind to the
//! entire class. So each round also asks the candidate a few questions carrying
//! an EDNS Client Subnet option for REAL client networks this node has already
//! served (`GeoCache`'s located set — no invented address list, and it samples
//! exactly the population that would be hurt). An empty answer for any of them
//! fails the round.
//!
//! This samples rather than proves-for-all-clients: the sample rotates each
//! round so coverage accumulates, but a defect that only affects a client
//! network nobody in the fleet has served yet will not be caught until one is.
//! That is a real limit, stated rather than papered over.
//!
//! ## Cost and cadence
//!
//! A handful of ~100-byte UDP datagrams per candidate per round (default 30s).
//! Withdrawal is damped by two consecutive failed rounds — the same K the DNS
//! reconciler already uses for host health, so one lost UDP datagram cannot
//! pull a nameserver out of a live delegation.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hive_core::now_ms;
use hive_edge::NodeInfo;
use parking_lot::RwLock;

use crate::dns_geo::ClientSubnet;
use crate::state::CloudState;

/// Consecutive failed probe rounds before this node stops attesting a peer.
/// Matches `vercel_dns::UNHEALTHY_PASSES_BEFORE_WITHDRAW`: a single dropped UDP
/// datagram must not evict a working nameserver from a live delegation.
pub const FAILED_ROUNDS_BEFORE_WITHDRAW: u32 = 2;

/// Distinct attester REGIONS required to consider a nameserver proven. See the
/// module doc for why this is regions and not a raw attester count.
pub const MIN_ATTESTER_REGIONS: usize = 2;

/// How many real client subnets each round asks on behalf of. Small on purpose:
/// the sample ROTATES, so coverage accumulates across rounds instead of costing
/// a burst of queries every 30 seconds.
const ECS_SAMPLE: usize = 3;

// ---- wire format -------------------------------------------------------------

/// Build a DNS query for `qname`/`qtype`, optionally carrying an EDNS Client
/// Subnet option so the candidate answers as if for a client in `ecs`.
///
/// `RD=0` deliberately: this is a direct question to a server that must be
/// AUTHORITATIVE for the name. Asking with recursion desired would let a node
/// that is not serving the zone at all still look healthy by fetching the
/// answer from somewhere else.
fn build_query(id: u16, qname: &str, qtype: u16, ecs: Option<ClientSubnet>) -> Vec<u8> {
    let mut m = Vec::with_capacity(64);
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&0u16.to_be_bytes()); // flags: standard QUERY, RD=0
    m.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    m.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    m.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    m.extend_from_slice(&(ecs.is_some() as u16).to_be_bytes()); // ARCOUNT (the OPT RR)
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
    m.extend_from_slice(&1u16.to_be_bytes()); // QCLASS IN
    if ecs.is_some() {
        // Scope 0 in a QUERY (RFC 7871 §6): scope is the responder's field.
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

/// Count the usable answer records of `qtype` in a response, or explain why the
/// response is not acceptable evidence.
///
/// Everything here is a REJECTION reason a real resolver would also act on: a
/// mismatched id (not our answer), a non-authoritative reply (this server is
/// not actually serving the zone), a non-zero rcode, or a truncated/garbled
/// message. `Ok(0)` — a well-formed authoritative NOERROR with no addresses —
/// is returned as such so the caller can name it `empty-answer`, which is the
/// exact live defect this exists to catch.
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
        off += 4; // QTYPE + QCLASS
        if off > msg.len() {
            return bad("malformed-question");
        }
    }
    // An A rdata is 4 bytes and an AAAA rdata is 16; anything else under those
    // types is not an address a client could connect to.
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

/// Send ONE query to `target` and report the usable answer count, or why not.
///
/// Late/stray datagrams that don't match our query id are skipped rather than
/// treated as the answer, but the whole receive window is bounded by a single
/// deadline so a chatty or hostile peer cannot hold the probe open.
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

/// True for a failure that is a lost/blocked DATAGRAM rather than a verdict.
/// Retrying these once is honest (UDP is lossy and a probe round asks several
/// questions, so the chance of one unlucky loss compounds); retrying a
/// `empty-answer` / `rcode=…` / `not-authoritative` would not be — those are
/// deterministic answers the server actually gave, and re-asking until it says
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

/// Prove — or fail to prove — that `ip` serves `zone` as an authoritative
/// nameserver, from THIS host, over the public internet.
///
/// Round shape: one baseline query on our own behalf (does the listener exist,
/// is it reachable from here, does it answer authoritatively with an address),
/// then one query per sampled real client subnet (does it answer usefully for
/// clients that are not us). First failure wins and short-circuits — the report
/// is evidence for a yes/no decision, not a survey.
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

// ---- what THIS node currently observes --------------------------------------

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

/// Everything this node has directly observed about its peers' nameservers.
/// Node-local by nature (it IS this node's vantage); the derived attestation
/// list is what gets gossiped.
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

// ---- the verdict the reconciler acts on -------------------------------------

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

/// Decide, from the gossiped registry alone, which declared nameservers are
/// currently PROVEN. Pure — the reconciler and the stats endpoint call the same
/// function so an operator can never be looking at a different answer than the
/// one being published.
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
            // Never self-attest, and never count a peer we consider down: a
            // node that is not answering our health probes is not a vantage
            // whose DNS opinion we should be acting on either.
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
        // How many independent vantages the fleet could possibly offer. A
        // two-region fleet cannot produce two attester regions for a candidate
        // that owns one of them, and refusing to ever delegate in that case
        // would be a worse failure than a thinner proof.
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

// ---- the prober loop ---------------------------------------------------------

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

/// Every node runs this: probe every peer that claims to be a nameserver, and
/// gossip the ones that currently answer. Not leader-elected on purpose — the
/// whole point is independent vantages, and a single prober would be a single
/// vantage with a single network path.
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
        // Let gossip converge before the first round: probing an empty registry
        // just produces an empty attestation list that peers would then act on.
        tokio::time::sleep(Duration::from_secs(15)).await;
        let mut round: usize = 0;
        loop {
            round = round.wrapping_add(1);
            let nodes = cloud.registry.nodes();
            // Convergence guard, same shape as the DNS reconciler's: a registry
            // that currently sees only self is a transient un-synced view, not
            // evidence that every nameserver in the fleet vanished. Gossiping
            // an empty attestation list from it would tell every peer we
            // withdraw everything — harmless (the reconciler HOLDS rather than
            // deletes) but a needless fleet-wide flap.
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
            // Rotate through the client networks this node has really served,
            // so successive rounds cover different parts of the population
            // without sending a burst every time.
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

// ---- operator diagnostic ------------------------------------------------------

/// `hive-cloud --dns-probe <ip>[,<ip>…]` — run the REAL probe from this host
/// against specific addresses and print the verdict, then exit.
///
/// This is the same code path the prober loop runs, deliberately: "is this
/// nameserver actually serving?" is a question an operator needs to answer from
/// an arbitrary vantage (a laptop, a peer, a bastion) during an incident, and
/// answering it with a different implementation than the one making the
/// decision is how the two quietly diverge. Zone comes from `HIVE_DEPLOY_ZONE`;
/// `HIVE_DNS_PROBE_SUBNETS` (comma-separated CIDRs) adds client subnets to ask
/// on behalf of.
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
