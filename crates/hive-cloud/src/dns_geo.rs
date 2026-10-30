//! EDNS0 Client Subnet (RFC 7871) + client-proximity answers for the authoritative DNS in
//! [`crate::dnsserver`].
//!
//! The deploy-zone answer used to be every healthy node's IP ordered by the SERVING node's own
//! `latency_ms`, which says nothing about the client. Answering by proximity needs to know WHO is
//! asking (the client subnet, else the query source) and to turn that into a location comparable
//! with the registry's per-node lat/lon, under two constraints:
//!
//! - **Nothing blocks the query.** The primary source is a local prefix table ([`crate::geoip`], a
//! binary search over static bytes: no network, safe inline). Only the OPTIONAL remote fallback is
//! a network call, on a background worker: an address the table cannot place is answered from the
//! health-ordered set at once and resolved later, never waited on.
//! - **The answer says how specific it is.** A proximity answer is valid only for the subnet it was
//! computed for, so the response echoes the ECS option with a SCOPE PREFIX-LENGTH (scope 0 on a
//! generic answer); otherwise a resolver would cache one client's nearest-node answer and serve it
//! to everyone behind it.
//!
//! The remote-memo cache is **durable** ([`GeoCache::spawn`] loads it, a debounced background
//! writer saves it): purely in-memory, every restart (a roll, a crash loop) sent every
//! remotely-located prefix back through the generic answer until its lookup completed again,
//! de-tailoring them for a window.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use hive_edge::{haversine_km, NodeInfo};
use serde::{Deserialize, Serialize};

/// EDNS0 option code for Client Subnet (RFC 7871 §6).
const OPT_CODE_ECS: u16 = 8;
/// The OPT pseudo-RR type (RFC 6891).
pub const TYPE_OPT: u16 = 41;
/// Our advertised UDP payload size when echoing an OPT RR: the widely recommended 1232 (1280-byte
/// IPv6 MTU minus headers) avoids fragmentation and comfortably exceeds anything this server emits.
const OUR_UDP_PAYLOAD: u16 = 1232;

/// How many nearest nodes a proximity answer contains, capped low ON PURPOSE: a resolver handed
/// eight addresses round-robins across them, discarding the proximity decision. Two keeps a
/// failover option while still steering traffic.
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
    /// The address to geolocate: the forwarded client subnet when present, else the query source (a
    /// resolver's own address is often nowhere near the client, so ECS wins when offered).
    pub fn locate_addr(&self) -> IpAddr {
        self.subnet.map(|s| s.addr).unwrap_or(self.source)
    }

    /// The prefix length an answer derived from this asker is valid for (the SCOPE PREFIX-LENGTH to
    /// echo); falls back to /24 (v4) and /48 (v6), the aggregation used when geolocating a bare
    /// source address.
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

/// Parses the EDNS0 OPT RR out of a query's ADDITIONAL section and returns the Client Subnet option
/// if it carries one. `rr_start` is the offset past the QUESTION section and `counts` are
/// ANCOUNT/NSCOUNT/ARCOUNT (normally 0/0/1; RRs before ADDITIONAL are skipped generically). `None`
/// for anything malformed: a bad OPT degrades to "no client subnet" and never rejects the query,
/// since that would turn an unfamiliar resolver into an outage.
pub fn parse_client_subnet(
    msg: &[u8],
    rr_start: usize,
    counts: (u16, u16, u16),
) -> Option<ClientSubnet> {
    let (ancount, nscount, arcount) = counts;
    if arcount == 0 {
        return None;
    }
    let mut off = rr_start;
    for _ in 0..(ancount as u32 + nscount as u32) {
        off = skip_rr(msg, off)?;
    }
    for _ in 0..arcount {
        let name_end = skip_name(msg, off)?;
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

/// FAMILY(2) | SOURCE PREFIX-LENGTH(1) | SCOPE PREFIX-LENGTH(1) | ADDRESS(var). The client
/// truncates the address to `ceil(source_prefix/8)` bytes and the remaining bits are zero, which is
/// exactly the masked subnet we want. Family 0 (no subnet, RFC 7871 §7.1.2) and any unknown family
/// answer generically (`None`).
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
            Some(ClientSubnet {
                addr: IpAddr::V4(Ipv4Addr::from(o)),
                source_prefix,
            })
        }
        2 => {
            if source_prefix > 128 {
                return None;
            }
            let mut o = [0u8; 16];
            let n = addr_bytes.len().min(16);
            o[..n].copy_from_slice(&addr_bytes[..n]);
            Some(ClientSubnet {
                addr: IpAddr::V6(Ipv6Addr::from(o)),
                source_prefix,
            })
        }
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

/// Skips a (possibly compressed) domain name, returning the offset just past it (a compression
/// pointer is two bytes and always ends the name). Shared with [`crate::dns_probe`]'s response
/// parser: the same wire rules, parsed once.
pub(crate) fn skip_name(msg: &[u8], mut off: usize) -> Option<usize> {
    loop {
        let len = *msg.get(off)? as usize;
        if len == 0 {
            return Some(off + 1);
        }
        if len & 0xC0 == 0xC0 {
            return if off + 2 <= msg.len() {
                Some(off + 2)
            } else {
                None
            };
        }
        if len & 0xC0 != 0 {
            return None;
        }
        off += 1 + len;
        if off > msg.len() {
            return None;
        }
    }
}

/// Builds the OPT RR to append to a response's ADDITIONAL section: root NAME, CLASS = our UDP
/// payload size, TTL = extended rcode and flags (all zero). `echo` is the client's own ECS option
/// (the SAME family and source prefix must be echoed, RFC 7871 §7.3) and `scope` is how specific
/// our answer is: `scope: 0` on a generic answer tells a resolver it may share it with every client
/// behind it.
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
    rr.push(0);
    rr.extend_from_slice(&TYPE_OPT.to_be_bytes());
    rr.extend_from_slice(&OUR_UDP_PAYLOAD.to_be_bytes());
    rr.extend_from_slice(&0u32.to_be_bytes());
    rr.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    rr.extend_from_slice(&rdata);
    rr
}

/// Orders `nodes` nearest-first for a client at `client`, keeping only nodes that report a
/// location. `None` when proximity cannot be decided (no client location, or no node has lat/lon),
/// so the caller keeps its health-ordered answer rather than inventing an order. Ties break on
/// distance, then name, so two nodes at one site do not reorder between passes and defeat resolver
/// caching; nodes with no known location still follow the placed ones, since dropping them would
/// shrink the healthy set for no reason.
pub fn nearest_first<'a>(
    nodes: &[&'a NodeInfo],
    client: Option<(f64, f64)>,
) -> Option<Vec<&'a NodeInfo>> {
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
    located.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    let keep = nearest_count().min(located.len());
    let mut out: Vec<&NodeInfo> = located.iter().take(keep).map(|(_, n)| *n).collect();
    for n in nodes {
        if (n.lat.is_none() || n.lon.is_none()) && out.len() < keep.max(1) {
            out.push(n);
        }
    }
    Some(out)
}

/// Coarse network key for caching a geolocation: /24 for IPv4, /48 for IPv6 (per-address caching
/// would be unbounded and pointless: every client behind one CPE shares a location).
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

/// True for addresses no external geo service can place (private, loopback, link-local, CGNAT
/// 100.64/10), which must never be sent to one; a local resolver's loopback/link-local traffic
/// would otherwise burn the whole lookup budget.
fn unlocatable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_documentation()
                || a.octets()[0] == 100 && (64..128).contains(&a.octets()[1])
        }
        IpAddr::V6(a) => {
            a.is_loopback()
                || a.is_unspecified()
                || (a.segments()[0] & 0xfe00) == 0xfc00
                || (a.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Pending,
    Known(f64, f64),
    Unlocatable,
}

/// A cache entry plus WHEN it was learned: the timestamp is what makes expiry and reload-side
/// pruning possible, since a restored file would otherwise be indistinguishable from a fresh
/// lookup.
#[derive(Clone, Copy, Debug)]
struct Slot {
    entry: Entry,
    at_ms: u64,
}

/// How long each kind of entry stays valid; geolocation goes stale (prefixes get reassigned), but
/// the kinds age at deliberately different rates:
///
/// - **Known, 30 days.** A prefix-to-city mapping changes on the timescale of registry
/// reallocation, and the answer it feeds (which of ~10 nodes is nearest) survives being a few
/// hundred km wrong; expiring faster would re-spend the paced lookup budget (1 per 1.5 s) on known
/// prefixes.
/// - **Unlocatable, 6 hours.** A negative is as likely a rate-limited or timed-out lookup as a
/// truly unplaceable prefix, so a short TTL lets the mistake self-correct.
/// - **Pending, 60 s.** A lookup is in flight; if the worker died, an immortal Pending entry would
/// pin the prefix generic FOREVER, so it ages out and re-queues (and is never persisted).
const KNOWN_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const UNLOCATABLE_TTL_MS: u64 = 6 * 60 * 60 * 1000;
const PENDING_TTL_MS: u64 = 60_000;

impl Slot {
    fn ttl_ms(&self) -> u64 {
        match self.entry {
            Entry::Known(..) => KNOWN_TTL_MS,
            Entry::Unlocatable => UNLOCATABLE_TTL_MS,
            Entry::Pending => PENDING_TTL_MS,
        }
    }
    fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.at_ms) < self.ttl_ms()
    }
}

/// Subnet to location resolver, in two tiers: (1) the local table ([`crate::geoip`]), a
/// prefix-to-coordinate blob embedded in the binary that answers in hundreds of nanoseconds with
/// nothing about the client leaving the node (the whole path by default); (2) an OPTIONAL remote
/// endpoint (`HIVE_DNS_GEO_ENDPOINT`) for prefixes the table cannot place, OFF unless configured (a
/// production data path owes nothing to a third party), queued to a paced background worker with
/// results memoised here so it can never stall a response.
///
/// The memo exists only for tier 2: tier-1 hits are deliberately NOT cached (recomputing a binary
/// search costs less than the write lock, and free entries would evict costly remote answers from a
/// capped map). It is DURABLE: loaded from `$HIVE_DATA/dns_geo.json` at boot and saved by a
/// debounced writer (see the module doc).
pub struct GeoCache {
    entries: parking_lot::RwLock<std::collections::HashMap<IpAddr, Slot>>,
    queue: tokio::sync::mpsc::UnboundedSender<IpAddr>,
    /// Prefixes answered from the local table vs handed to the remote path, reported by `GET
    /// /v1/dns/stats`: without the split there is no telling whether the table carries the traffic
    /// or quietly misses everything.
    local_hits: AtomicU64,
    local_misses: AtomicU64,
    /// Bumped whenever a DURABLE entry changes (the background worker is the only writer of
    /// Known/Unlocatable); the saver compares it against `saved` and skips the write when nothing
    /// moved.
    dirty: AtomicU64,
    saved: AtomicU64,
    /// Entries restored from disk at boot and durable writes so far, both on `GET /v1/dns/stats`
    /// ("did the cache survive the restart" is otherwise unanswerable without a live dig).
    loaded_at_boot: AtomicU64,
    writes: AtomicU64,
    /// Serializes file writes so the debounced saver and a shutdown
    /// `flush_blocking()` can never interleave two temp-file+rename sequences.
    file_lock: std::sync::Mutex<()>,
}

/// Upper bound on memoised remote lookups: a DNS server is an open surface, so arbitrary queries
/// would grow the map without a cap. Enforced on LOAD too, so no on-disk file (older build,
/// hand-edited, corrupt) can reload past it.
const MAX_ENTRIES: usize = 8192;

/// The remote endpoint, `None` unless configured (the default). Read per call: it is consulted only
/// on a local miss, which is rare, and a plain env read beats another global.
fn remote_endpoint() -> Option<String> {
    std::env::var("HIVE_DNS_GEO_ENDPOINT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl GeoCache {
    /// Creates the cache and spawns the OPTIONAL remote-lookup worker (`http` is the platform's
    /// shared client; it idles unless `HIVE_DNS_GEO_ENDPOINT` is set). Lookups are PACED
    /// (`HIVE_DNS_GEO_PACE_MS`, 1.5 s) because a remote geo service rate-limits per source IP and a
    /// block would take proximity down fleet-wide; a FAILED lookup never downgrades a location
    /// already held, only a prefix with no known fix becomes a negative.
    ///
    /// The prior memo loads from disk FIRST, synchronously (once, at `CloudState::new`, one small
    /// file), so the first query for a known prefix is already tailored. Saving is a SEPARATE
    /// debounced task, never a write per learned entry: the DNS hot path must never touch the disk,
    /// and ticking only when the dirty counter moved coalesces a warm-up burst into one write,
    /// bounding an unclean kill to a few prefixes that simply re-look-up. A failed write leaves
    /// `saved` behind `dirty`, so the next tick retries.
    pub fn spawn(http: reqwest::Client) -> Arc<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<IpAddr>();
        let restored = load_from_disk();
        let n_restored = restored.len();
        if n_restored > 0 {
            tracing::info!(entries = n_restored, path = %cache_path().display(), "dns_geo: restored subnet→location cache from disk");
        }
        let cache = Arc::new(Self {
            entries: parking_lot::RwLock::new(restored),
            queue: tx,
            local_hits: AtomicU64::new(0),
            local_misses: AtomicU64::new(0),
            dirty: AtomicU64::new(0),
            saved: AtomicU64::new(0),
            loaded_at_boot: AtomicU64::new(n_restored as u64),
            writes: AtomicU64::new(0),
            file_lock: std::sync::Mutex::new(()),
        });
        let weak = Arc::downgrade(&cache);
        tokio::spawn(async move {
            let pace = std::time::Duration::from_millis(
                std::env::var("HIVE_DNS_GEO_PACE_MS")
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(1500),
            );
            while let Some(net) = rx.recv().await {
                let Some(cache) = weak.upgrade() else { return };
                let located = lookup_geo(&http, net).await;
                let now = hive_core::now_ms();
                {
                    let mut w = cache.entries.write();
                    let prev = w.get(&net).copied();
                    let entry = match located {
                        Some((lat, lon)) => Entry::Known(lat, lon),
                        None => match prev {
                            Some(Slot {
                                entry: Entry::Known(lat, lon),
                                ..
                            }) => Entry::Known(lat, lon),
                            _ => Entry::Unlocatable,
                        },
                    };
                    w.insert(net, Slot { entry, at_ms: now });
                }
                cache.dirty.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(pace).await;
            }
        });
        if let Some(interval) = save_interval() {
            let weak = Arc::downgrade(&cache);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    let Some(cache) = weak.upgrade() else { return };
                    let target = cache.dirty.load(Ordering::Relaxed);
                    if target == cache.saved.load(Ordering::Relaxed) {
                        continue;
                    }
                    let rows = cache.disk_rows();
                    let arc = cache.clone();
                    let done = tokio::task::spawn_blocking(move || arc.write_rows(rows)).await;
                    match done {
                        Ok(Ok(())) => {
                            cache.saved.store(target, Ordering::Relaxed);
                            cache.writes.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "dns_geo: cache save failed; retrying on next tick")
                        }
                        Err(e) => tracing::warn!(error = %e, "dns_geo: cache save task failed"),
                    }
                }
            });
        }
        cache
    }

    /// The client's location. Never blocks, no disk I/O. Order is the design: the local table first
    /// (in-process, answers the FIRST query for a prefix as well as the thousandth), then the memo
    /// of earlier remote answers, then, only if a remote endpoint is configured, a queued
    /// background lookup this query does not wait for.
    ///
    /// A fresh memo entry answers under the read lock alone. With no remote endpoint a local miss
    /// is honest (the query gets the generic health-ordered set) and records nothing, keeping the
    /// map exclusively about remote work. An unknown or expired entry is (re)queued under the write
    /// lock so exactly one caller queues it. At the cap, expired entries then locationless ones are
    /// dropped before refusing to learn, so unlocatable traffic cannot freeze the cache against
    /// real clients. An expired Known is served stale while revalidating (only its timestamp
    /// moves): expiry must never itself cause the de-tailoring blip.
    pub fn locate(&self, ip: IpAddr) -> Option<(f64, f64)> {
        if unlocatable(ip) {
            return None;
        }
        let net = network_key(ip);
        if let Some(loc) = crate::geoip::locate(net) {
            self.local_hits.fetch_add(1, Ordering::Relaxed);
            return Some(loc);
        }
        self.local_misses.fetch_add(1, Ordering::Relaxed);
        let now = hive_core::now_ms();
        if let Some(s) = self.entries.read().get(&net).copied() {
            if s.is_fresh(now) {
                return match s.entry {
                    Entry::Known(lat, lon) => Some((lat, lon)),
                    _ => None,
                };
            }
        }
        if remote_endpoint().is_none() {
            return None;
        }
        let mut w = self.entries.write();
        let existing = w.get(&net).copied();
        if let Some(s) = existing {
            if s.is_fresh(now) {
                return match s.entry {
                    Entry::Known(lat, lon) => Some((lat, lon)),
                    _ => None,
                };
            }
        }
        if existing.is_none() && w.len() >= MAX_ENTRIES {
            w.retain(|_, v| v.is_fresh(now));
            if w.len() >= MAX_ENTRIES {
                w.retain(|_, v| matches!(v.entry, Entry::Known(..)));
            }
            if w.len() >= MAX_ENTRIES {
                return None;
            }
        }
        let served = match existing {
            Some(Slot {
                entry: Entry::Known(lat, lon),
                ..
            }) => {
                w.insert(
                    net,
                    Slot {
                        entry: Entry::Known(lat, lon),
                        at_ms: now,
                    },
                );
                Some((lat, lon))
            }
            _ => {
                w.insert(
                    net,
                    Slot {
                        entry: Entry::Pending,
                        at_ms: now,
                    },
                );
                None
            }
        };
        drop(w);
        let _ = self.queue.send(net);
        served
    }

    /// The client networks this node has actually LOCATED, in a stable order, for the nameserver
    /// prover ([`crate::dns_probe`]) to ask a candidate how it would answer for clients that are
    /// not the prober. Real served traffic, not a hand-written list of representative prefixes,
    /// keeps the probe pointed at the population a wrong answer would hurt.
    pub fn known_networks(&self) -> Vec<IpAddr> {
        let r = self.entries.read();
        let mut v: Vec<IpAddr> = r
            .iter()
            .filter(|(_, e)| matches!(e.entry, Entry::Known(..)))
            .map(|(k, _)| *k)
            .collect();
        v.sort();
        v
    }

    /// What the DNS stats endpoint reports, split by TIER on purpose: "the table answered it" and
    /// "a remote service answered it" are different facts, and merging them would hide a table that
    /// silently stopped covering real traffic.
    pub fn stats(&self) -> GeoStats {
        let r = self.entries.read();
        let mut s = GeoStats {
            local_hits: self.local_hits.load(Ordering::Relaxed),
            local_misses: self.local_misses.load(Ordering::Relaxed),
            remote_enabled: remote_endpoint().is_some(),
            ..Default::default()
        };
        for v in r.values() {
            match v.entry {
                Entry::Known(..) => s.remote_known += 1,
                Entry::Pending => s.remote_pending += 1,
                Entry::Unlocatable => s.remote_unlocatable += 1,
            }
        }
        s
    }

    /// (entries restored from disk at boot, durable writes since boot) — the
    /// operator's answer to "did the cache actually survive the restart?".
    pub fn persist_stats(&self) -> (u64, u64) {
        (
            self.loaded_at_boot.load(Ordering::Relaxed),
            self.writes.load(Ordering::Relaxed),
        )
    }

    /// Writes the cache NOW, synchronously (graceful shutdown, beside `persist::flush_blocking` on
    /// SIGTERM), so a clean restart loses nothing from the debounce window. Best-effort: a cache is
    /// never worth failing a shutdown over.
    pub fn flush_blocking(&self) {
        if save_interval().is_none() {
            return;
        }
        let target = self.dirty.load(Ordering::Relaxed);
        if target == self.saved.load(Ordering::Relaxed) {
            return;
        }
        let rows = self.disk_rows();
        match self.write_rows(rows) {
            Ok(()) => {
                self.saved.store(target, Ordering::Relaxed);
                self.writes.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => tracing::warn!(error = %e, "dns_geo: shutdown cache flush failed"),
        }
    }

    /// Snapshots the durable half of the map. Pending is excluded: it describes an in-flight lookup
    /// on a process about to not exist, and restoring one would suppress the re-queue for its whole
    /// TTL.
    fn disk_rows(&self) -> Vec<DiskEntry> {
        let r = self.entries.read();
        r.iter()
            .filter_map(|(net, slot)| match slot.entry {
                Entry::Pending => None,
                Entry::Known(lat, lon) => Some(DiskEntry {
                    n: net.to_string(),
                    lat: Some(lat),
                    lon: Some(lon),
                    t: slot.at_ms,
                }),
                Entry::Unlocatable => Some(DiskEntry {
                    n: net.to_string(),
                    lat: None,
                    lon: None,
                    t: slot.at_ms,
                }),
            })
            .take(MAX_ENTRIES)
            .collect()
    }

    /// Atomic temp-file + fsync + rename, the same durability shape every other
    /// sidecar under the data dir uses (`persist::save`, `save_peer_iroh`).
    fn write_rows(&self, entries: Vec<DiskEntry>) -> std::io::Result<()> {
        let _g = self.file_lock.lock().unwrap_or_else(|e| e.into_inner());
        let dir = crate::persist::data_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(CACHE_FILE);
        let tmp = dir.join(format!("{CACHE_FILE}.tmp"));
        let file = DiskFile {
            v: FORMAT_VERSION,
            saved_ms: hive_core::now_ms(),
            entries,
        };
        let json = serde_json::to_string(&file).unwrap_or_else(|_| "{}".into());
        {
            let f = std::fs::File::create(&tmp)?;
            use std::io::Write;
            let mut w = std::io::BufWriter::new(&f);
            w.write_all(json.as_bytes())?;
            w.flush()?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// On-disk form: `$HIVE_DATA/dns_geo.json` (`persist::data_dir()`), a small node-local sidecar like
/// `peer_iroh.json`, deliberately NOT a field of `PlatformSnapshot`: every write of that snapshot
/// re-serializes and fsyncs the ENTIRE state and replicates into GuardianDB, while this cache is
/// derived data shaped by whichever clients query THIS node and must neither ride that write path
/// nor replicate across the mesh. One row per prefix: the masked network key as a string, lat/lon
/// (absent = the lookup ran and could not place it) and the learn time, ~55 bytes per entry, so a
/// full 8192-entry cache is well under a megabyte.
const CACHE_FILE: &str = "dns_geo.json";
/// Bumped only if the row shape changes incompatibly; a file carrying anything
/// else is ignored wholesale rather than half-interpreted.
const FORMAT_VERSION: u32 = 1;
/// Refuse to read a file larger than this: the write cap bounds what this process writes, not what
/// it finds on disk, and slurping a huge corrupt sidecar into a String at boot would OOM the node
/// before it served a query.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct DiskEntry {
    /// Masked network key (/24 or /48), e.g. "203.0.113.0".
    n: String,
    /// Absent on both = Unlocatable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lat: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lon: Option<f64>,
    /// When this entry was learned (ms epoch) — the expiry input.
    t: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct DiskFile {
    #[serde(default)]
    v: u32,
    #[serde(default)]
    saved_ms: u64,
    #[serde(default)]
    entries: Vec<DiskEntry>,
}

pub(crate) fn cache_path() -> std::path::PathBuf {
    crate::persist::data_dir().join(CACHE_FILE)
}

/// Debounce interval of the background saver, 10 s by default (lookups are paced at one per 1.5 s,
/// so a tick that fires only on change folds a warm-up burst into one write while bounding an
/// unclean-kill loss to a few prefixes, each costing one generic answer). `HIVE_DNS_GEO_SAVE_MS=0`
/// turns persistence off entirely (no load, no save) for an operator who wants this node's DNS path
/// to touch no disk.
fn save_interval() -> Option<std::time::Duration> {
    let ms = std::env::var("HIVE_DNS_GEO_SAVE_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(10_000);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// Loads the persisted cache. EVERY failure mode (missing, unreadable, oversized, malformed, wrong
/// version, garbage rows) degrades to an empty cache and a log line: a geolocation cache is an
/// optimisation, and refusing to boot over a corrupt scratch file would be a DNS outage for the
/// whole delegated zone. Rows are re-masked on the way in (a foreign or hand-edited file must not
/// seed a full host address as a key), missing or garbage coordinates read as the negative entry
/// (never a bogus location that steers real clients), a future timestamp is clamped to now (no
/// immortal entries), and the freshest rows win under the cap (duplicate keys: the newest wins).
fn load_from_disk() -> std::collections::HashMap<IpAddr, Slot> {
    let empty = std::collections::HashMap::new();
    if save_interval().is_none() {
        return empty;
    }
    let path = cache_path();
    let Ok(meta) = std::fs::metadata(&path) else {
        return empty;
    };
    if meta.len() > MAX_FILE_BYTES {
        tracing::warn!(bytes = meta.len(), path = %path.display(), "dns_geo: cache file too large; ignoring");
        return empty;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "dns_geo: cache file unreadable; starting empty");
            return empty;
        }
    };
    let file: DiskFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "dns_geo: cache file corrupt; starting empty");
            return empty;
        }
    };
    if file.v != FORMAT_VERSION {
        tracing::warn!(
            found = file.v,
            want = FORMAT_VERSION,
            "dns_geo: cache file version mismatch; starting empty"
        );
        return empty;
    }
    let now = hive_core::now_ms();
    let mut rows: Vec<(IpAddr, Slot)> = file
        .entries
        .iter()
        .filter_map(|e| {
            let ip: IpAddr = e.n.parse().ok()?;
            let net = network_key(ip);
            let entry = match (e.lat, e.lon) {
                (Some(lat), Some(lon))
                    if lat.is_finite()
                        && lon.is_finite()
                        && (-90.0..=90.0).contains(&lat)
                        && (-180.0..=180.0).contains(&lon) =>
                {
                    Entry::Known(lat, lon)
                }
                _ => Entry::Unlocatable,
            };
            let slot = Slot {
                entry,
                at_ms: e.t.min(now),
            };
            slot.is_fresh(now).then_some((net, slot))
        })
        .collect();
    rows.sort_by(|a, b| b.1.at_ms.cmp(&a.1.at_ms));
    let mut out = std::collections::HashMap::with_capacity(rows.len().min(MAX_ENTRIES));
    for (net, slot) in rows {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        out.entry(net).or_insert(slot);
    }
    out
}

/// Counters behind `GET /v1/dns/stats`'s `geo` block.
#[derive(Default, Clone, Copy, Debug)]
pub struct GeoStats {
    /// Prefixes the local table placed — in the default configuration, all of
    /// them.
    pub local_hits: u64,
    /// Prefixes it could not place (an explicit hole, or space newer than the
    /// table). These are the only ones the remote path ever sees.
    pub local_misses: u64,
    /// Whether `HIVE_DNS_GEO_ENDPOINT` is configured at all.
    pub remote_enabled: bool,
    pub remote_known: usize,
    pub remote_pending: usize,
    pub remote_unlocatable: usize,
}

/// Geolocates one subnet through the OPTIONAL remote endpoint, the only way this runs (there is no
/// default third party): `HIVE_DNS_GEO_ENDPOINT` serves an operator's own geolocation service or a
/// commercial database covering the prefixes the shipped table misses, with the URL shape
/// `<endpoint>/<ip>?fields=status,lat,lon` returning ip-api's JSON. It asks about a host inside the
/// prefix, since a /24's first address is reserved.
async fn lookup_geo(http: &reqwest::Client, net: IpAddr) -> Option<(f64, f64)> {
    let base = remote_endpoint()?;
    let probe = match net {
        IpAddr::V4(a) => {
            let o = a.octets();
            IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], 1))
        }
        v6 => v6,
    };
    let url = format!(
        "{}/{}?fields=status,lat,lon",
        base.trim_end_matches('/'),
        probe
    );
    let resp = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    if v.get("status").and_then(|s| s.as_str()) != Some("success") {
        return None;
    }
    Some((v.get("lat")?.as_f64()?, v.get("lon")?.as_f64()?))
}
