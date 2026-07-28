//! EDNS0 Client Subnet (RFC 7871) + client-proximity answers for the
//! authoritative DNS in [`crate::dnsserver`].
//!
//! Why this exists: `dnsserver`'s deploy-zone answer used to be every healthy
//! node's IP, ordered by *the serving node's own* `latency_ms` — a number that
//! says nothing about the client, and which resolvers round-robin away anyway.
//! So a browser in Bangkok was as likely to be handed a Virginia node as a
//! Bangkok one. Answering by proximity needs two things this module provides:
//! knowing WHO is asking (the client subnet, or failing that the query source),
//! and turning that into a location that can be compared against the registry's
//! per-node lat/lon.
//!
//! Two deliberate design constraints:
//!
//! * **Nothing blocks the query.** A geo lookup is a network call, and a DNS
//!   response budget is milliseconds. An unknown subnet is answered from the
//!   existing health-ordered set *immediately* and resolved in the background,
//!   so proximity is an optimisation that arrives shortly after, never a stall.
//! * **The answer says how specific it is.** A proximity answer is only valid
//!   for the subnet it was computed for, so the response echoes the ECS option
//!   with a SCOPE PREFIX-LENGTH; a generic answer echoes scope 0. Without that,
//!   a resolver caches one client's nearest-node answer and serves it to
//!   everyone behind it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use hive_edge::{haversine_km, NodeInfo};

/// EDNS0 option code for Client Subnet (RFC 7871 §6).
const OPT_CODE_ECS: u16 = 8;
/// The OPT pseudo-RR type (RFC 6891).
pub const TYPE_OPT: u16 = 41;
/// Our advertised UDP payload size when echoing an OPT RR. 1232 is the widely
/// recommended value (1280-byte IPv6 MTU minus headers) that avoids
/// fragmentation, and it comfortably exceeds anything this server emits.
const OUR_UDP_PAYLOAD: u16 = 1232;

/// How many nearest nodes a proximity answer contains. Capped low ON PURPOSE:
/// hand a resolver eight addresses and it round-robins across them, which
/// discards the proximity decision we just made. Two keeps a second option for
/// failover while still steering traffic.
fn nearest_count() -> usize {
    std::env::var("HIVE_DNS_NEAREST_N")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(2)
}

/// A parsed EDNS0 Client Subnet option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientSubnet {
    /// The subnet itself, already masked to `source_prefix`.
    pub addr: IpAddr,
    /// SOURCE PREFIX-LENGTH: how many bits of `addr` the client vouched for.
    pub source_prefix: u8,
}

/// What the wire layer knows about the asker: the resolver's own address, plus
/// the client subnet it forwarded on the real client's behalf, if any.
#[derive(Clone, Copy, Debug)]
pub struct Asker {
    pub source: IpAddr,
    pub subnet: Option<ClientSubnet>,
}

impl Asker {
    /// The address to geolocate: the forwarded client subnet when present,
    /// otherwise the query source. RFC 7871's whole point is that the resolver's
    /// own address is often nowhere near the client, so ECS wins when offered.
    pub fn locate_addr(&self) -> IpAddr {
        self.subnet.map(|s| s.addr).unwrap_or(self.source)
    }

    /// The prefix length an answer derived from this asker is valid for — the
    /// SCOPE PREFIX-LENGTH to echo. Falls back to the conventional /24 (v4) and
    /// /48 (v6) aggregation used when we geolocated a bare source address.
    pub fn scope_prefix(&self) -> u8 {
        match self.subnet {
            Some(s) => s.source_prefix,
            None => match self.source {
                IpAddr::V4(_) => 24,
                IpAddr::V6(_) => 48,
            },
        }
    }
}

/// Parse the EDNS0 OPT RR out of a query's ADDITIONAL section and return the
/// Client Subnet option if it carries one.
///
/// `rr_start` is the offset just past the QUESTION section; `counts` are the
/// query's ANCOUNT/NSCOUNT/ARCOUNT. A query normally has 0/0/1, but the RRs
/// before ADDITIONAL are skipped generically rather than assumed away.
///
/// Returns `None` for anything malformed — a bad OPT must degrade to "no client
/// subnet", never reject the query, because that would turn an unfamiliar
/// resolver into an outage.
pub fn parse_client_subnet(msg: &[u8], rr_start: usize, counts: (u16, u16, u16)) -> Option<ClientSubnet> {
    let (ancount, nscount, arcount) = counts;
    if arcount == 0 {
        return None;
    }
    let mut off = rr_start;
    // Skip ANSWER + AUTHORITY records, then walk ADDITIONAL looking for OPT.
    for _ in 0..(ancount as u32 + nscount as u32) {
        off = skip_rr(msg, off)?;
    }
    for _ in 0..arcount {
        let name_end = skip_name(msg, off)?;
        // TYPE(2) CLASS(2) TTL(4) RDLEN(2)
        if name_end + 10 > msg.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([msg[name_end], msg[name_end + 1]]);
        let rdlen = u16::from_be_bytes([msg[name_end + 8], msg[name_end + 9]]) as usize;
        let rdata_start = name_end + 10;
        if rdata_start + rdlen > msg.len() {
            return None;
        }
        if rtype == TYPE_OPT {
            return parse_ecs_option(&msg[rdata_start..rdata_start + rdlen]);
        }
        off = rdata_start + rdlen;
    }
    None
}

/// Walk the OPT RDATA's `{code, len, data}` triples for the ECS option.
fn parse_ecs_option(mut rdata: &[u8]) -> Option<ClientSubnet> {
    while rdata.len() >= 4 {
        let code = u16::from_be_bytes([rdata[0], rdata[1]]);
        let len = u16::from_be_bytes([rdata[2], rdata[3]]) as usize;
        if 4 + len > rdata.len() {
            return None;
        }
        let data = &rdata[4..4 + len];
        if code == OPT_CODE_ECS {
            return decode_ecs(data);
        }
        rdata = &rdata[4 + len..];
    }
    None
}

/// FAMILY(2) | SOURCE PREFIX-LENGTH(1) | SCOPE PREFIX-LENGTH(1) | ADDRESS(var).
/// The address is truncated by the client to `ceil(source_prefix/8)` bytes; the
/// remaining bits are zero, which is exactly the masked subnet we want.
fn decode_ecs(data: &[u8]) -> Option<ClientSubnet> {
    if data.len() < 4 {
        return None;
    }
    let family = u16::from_be_bytes([data[0], data[1]]);
    let source_prefix = data[2];
    let addr_bytes = &data[4..];
    match family {
        1 => {
            if source_prefix > 32 {
                return None;
            }
            let mut o = [0u8; 4];
            let n = addr_bytes.len().min(4);
            o[..n].copy_from_slice(&addr_bytes[..n]);
            Some(ClientSubnet { addr: IpAddr::V4(Ipv4Addr::from(o)), source_prefix })
        }
        2 => {
            if source_prefix > 128 {
                return None;
            }
            let mut o = [0u8; 16];
            let n = addr_bytes.len().min(16);
            o[..n].copy_from_slice(&addr_bytes[..n]);
            Some(ClientSubnet { addr: IpAddr::V6(Ipv6Addr::from(o)), source_prefix })
        }
        // Family 0 means "no subnet available" (RFC 7871 §7.1.2); anything else
        // is unknown to us. Both mean: answer generically.
        _ => None,
    }
}

/// Skip one RR (name + fixed header + rdata), returning the next offset.
fn skip_rr(msg: &[u8], off: usize) -> Option<usize> {
    let name_end = skip_name(msg, off)?;
    if name_end + 10 > msg.len() {
        return None;
    }
    let rdlen = u16::from_be_bytes([msg[name_end + 8], msg[name_end + 9]]) as usize;
    let end = name_end + 10 + rdlen;
    if end > msg.len() {
        return None;
    }
    Some(end)
}

/// Skip a (possibly compressed) domain name, returning the offset just past it.
/// Shared with [`crate::dns_probe`]'s response parser — the same wire rules,
/// parsed once.
pub(crate) fn skip_name(msg: &[u8], mut off: usize) -> Option<usize> {
    loop {
        let len = *msg.get(off)? as usize;
        if len == 0 {
            return Some(off + 1);
        }
        if len & 0xC0 == 0xC0 {
            // A compression pointer is two bytes and always terminates the name.
            return if off + 2 <= msg.len() { Some(off + 2) } else { None };
        }
        if len & 0xC0 != 0 {
            return None; // reserved label type
        }
        off += 1 + len;
        if off > msg.len() {
            return None;
        }
    }
}

/// Build the OPT RR to append to a response's ADDITIONAL section.
///
/// `echo` is the client's own ECS option (we must echo the SAME family and
/// source prefix, per RFC 7871 §7.3) and `scope` is how specific our answer
/// actually is. Passing `scope: 0` on a generic answer is what tells a resolver
/// it may share that answer with every client behind it.
pub fn encode_opt_rr(echo: Option<ClientSubnet>, scope: u8) -> Vec<u8> {
    let mut rdata = Vec::new();
    if let Some(cs) = echo {
        let (family, full): (u16, Vec<u8>) = match cs.addr {
            IpAddr::V4(a) => (1, a.octets().to_vec()),
            IpAddr::V6(a) => (2, a.octets().to_vec()),
        };
        let keep = ((cs.source_prefix as usize) + 7) / 8;
        let mut opt = Vec::with_capacity(4 + keep);
        opt.extend_from_slice(&family.to_be_bytes());
        opt.push(cs.source_prefix);
        opt.push(scope);
        opt.extend_from_slice(&full[..keep.min(full.len())]);
        rdata.extend_from_slice(&OPT_CODE_ECS.to_be_bytes());
        rdata.extend_from_slice(&(opt.len() as u16).to_be_bytes());
        rdata.extend_from_slice(&opt);
    }
    let mut rr = Vec::with_capacity(11 + rdata.len());
    rr.push(0); // NAME = root
    rr.extend_from_slice(&TYPE_OPT.to_be_bytes());
    rr.extend_from_slice(&OUR_UDP_PAYLOAD.to_be_bytes()); // CLASS = payload size
    rr.extend_from_slice(&0u32.to_be_bytes()); // TTL = extended rcode + flags (all zero)
    rr.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    rr.extend_from_slice(&rdata);
    rr
}

/// Order `nodes` nearest-first for a client at `client`, keeping only nodes that
/// report a location. Returns `None` when proximity cannot be decided at all
/// (no client location, or no node has lat/lon), so the caller keeps its
/// existing health-ordered answer rather than inventing an order.
pub fn nearest_first<'a>(nodes: &[&'a NodeInfo], client: Option<(f64, f64)>) -> Option<Vec<&'a NodeInfo>> {
    let c = client?;
    let mut located: Vec<(f64, &NodeInfo)> = nodes
        .iter()
        .filter_map(|n| match (n.lat, n.lon) {
            (Some(lat), Some(lon)) => Some((haversine_km(c, (lat, lon)), *n)),
            _ => None,
        })
        .collect();
    if located.is_empty() {
        return None;
    }
    // Distance, then name: a deterministic tie-break so two nodes at the same
    // site don't reorder between passes and defeat resolver caching.
    located.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.name.cmp(&b.1.name)));
    let keep = nearest_count().min(located.len());
    let mut out: Vec<&NodeInfo> = located.iter().take(keep).map(|(_, n)| *n).collect();
    // Nodes with no known location still belong in the answer, after the ones we
    // could place — dropping them would shrink the healthy set for no reason.
    for n in nodes {
        if (n.lat.is_none() || n.lon.is_none()) && out.len() < keep.max(1) {
            out.push(n);
        }
    }
    Some(out)
}

/// Coarse network key for caching a geolocation: /24 for IPv4, /48 for IPv6.
/// Per-address caching would be unbounded and pointless — geo data is not that
/// precise, and every client behind one CPE shares a location.
pub fn network_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], 0))
        }
        IpAddr::V6(a) => {
            let mut o = a.octets();
            o[6..].fill(0);
            IpAddr::V6(Ipv6Addr::from(o))
        }
    }
}

/// True for addresses no external geo service can place — and which must never
/// be sent to one. Also covers the loopback/link-local traffic a local resolver
/// generates, which would otherwise burn the whole lookup budget.
fn unlocatable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_documentation()
                || a.octets()[0] == 100 && (64..128).contains(&a.octets()[1]) // CGNAT 100.64/10
        }
        IpAddr::V6(a) => {
            a.is_loopback() || a.is_unspecified() || (a.segments()[0] & 0xfe00) == 0xfc00 || (a.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Pending,
    Known(f64, f64),
    Unlocatable,
}

/// Subnet → location cache, filled by a background worker so no DNS query ever
/// waits on an HTTP geo lookup.
pub struct GeoCache {
    entries: parking_lot::RwLock<std::collections::HashMap<IpAddr, Entry>>,
    queue: tokio::sync::mpsc::UnboundedSender<IpAddr>,
}

/// Upper bound on cached subnets. A DNS server is an open surface — without a
/// cap, arbitrary queries grow this map for as long as the process runs.
const MAX_ENTRIES: usize = 8192;

impl GeoCache {
    /// Create the cache and spawn its lookup worker.
    ///
    /// `http` is the platform's shared client. Lookups are PACED because the geo
    /// service rate-limits per source IP, and getting blocked there would take
    /// proximity down fleet-wide rather than degrade it.
    pub fn spawn(http: reqwest::Client) -> Arc<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<IpAddr>();
        let cache = Arc::new(Self { entries: Default::default(), queue: tx });
        let weak = Arc::downgrade(&cache);
        tokio::spawn(async move {
            let pace = std::time::Duration::from_millis(
                std::env::var("HIVE_DNS_GEO_PACE_MS").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(1500),
            );
            while let Some(net) = rx.recv().await {
                let Some(cache) = weak.upgrade() else { return };
                let located = lookup_geo(&http, net).await;
                // The write guard is scoped to this block DELIBERATELY: a
                // `parking_lot` guard is not `Send`, so leaving it merely
                // `drop()`ed before the await below still makes the whole task
                // future non-Send and `tokio::spawn` rejects it.
                {
                    let mut w = cache.entries.write();
                    w.insert(
                        net,
                        match located {
                            Some((lat, lon)) => Entry::Known(lat, lon),
                            None => Entry::Unlocatable,
                        },
                    );
                }
                tokio::time::sleep(pace).await;
            }
        });
        cache
    }

    /// The client's location if already known. Never blocks: an unknown subnet
    /// is queued for background resolution and reported as unknown for now, so
    /// this query is answered generically and later ones benefit.
    pub fn locate(&self, ip: IpAddr) -> Option<(f64, f64)> {
        if unlocatable(ip) {
            return None;
        }
        let net = network_key(ip);
        if let Some(e) = self.entries.read().get(&net).copied() {
            return match e {
                Entry::Known(lat, lon) => Some((lat, lon)),
                _ => None,
            };
        }
        let mut w = self.entries.write();
        if w.len() >= MAX_ENTRIES {
            // Full: drop the entries that carry no location before refusing to
            // learn anything new, so a burst of unlocatable traffic can't
            // permanently freeze the cache against real clients.
            w.retain(|_, v| matches!(v, Entry::Known(..)));
            if w.len() >= MAX_ENTRIES {
                return None;
            }
        }
        if w.contains_key(&net) {
            return None; // raced another caller
        }
        w.insert(net, Entry::Pending);
        drop(w);
        let _ = self.queue.send(net);
        None
    }

    /// The client networks this node has actually LOCATED — real observed
    /// client subnets, in a stable order.
    ///
    /// Used by the nameserver prover ([`crate::dns_probe`]) to ask a candidate
    /// nameserver how it would answer for clients that are not the prober.
    /// Sourcing them from traffic this node has really served, rather than a
    /// hand-written list of "representative" prefixes, keeps the probe pointed
    /// at the population that would actually be hurt by a wrong answer.
    pub fn known_networks(&self) -> Vec<IpAddr> {
        let r = self.entries.read();
        let mut v: Vec<IpAddr> =
            r.iter().filter(|(_, e)| matches!(e, Entry::Known(..))).map(|(k, _)| *k).collect();
        v.sort();
        v
    }

    /// (known, pending, unlocatable) — surfaced by the DNS stats endpoint so the
    /// hit rate is observable rather than guessed at.
    pub fn stats(&self) -> (usize, usize, usize) {
        let r = self.entries.read();
        let mut k = 0;
        let mut p = 0;
        let mut u = 0;
        for v in r.values() {
            match v {
                Entry::Known(..) => k += 1,
                Entry::Pending => p += 1,
                Entry::Unlocatable => u += 1,
            }
        }
        (k, p, u)
    }
}

/// Geolocate one subnet. Uses the SAME service the node already uses to locate
/// itself at boot (`ip-api.com`), queried for a specific address rather than
/// "my own IP". `HIVE_DNS_GEO_ENDPOINT` overrides it for an operator who wants a
/// self-hosted database instead of a third party.
async fn lookup_geo(http: &reqwest::Client, net: IpAddr) -> Option<(f64, f64)> {
    let base = std::env::var("HIVE_DNS_GEO_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://ip-api.com/json".to_string());
    // A /24's first address is reserved, so ask about a host inside it.
    let probe = match net {
        IpAddr::V4(a) => {
            let o = a.octets();
            IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], 1))
        }
        v6 => v6,
    };
    let url = format!("{}/{}?fields=status,lat,lon", base.trim_end_matches('/'), probe);
    let resp = http.get(&url).timeout(std::time::Duration::from_secs(5)).send().await.ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    if v.get("status").and_then(|s| s.as_str()) != Some("success") {
        return None;
    }
    Some((v.get("lat")?.as_f64()?, v.get("lon")?.as_f64()?))
}
