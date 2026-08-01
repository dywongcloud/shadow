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
//! * **Nothing blocks the query.** A DNS response budget is milliseconds, so
//!   locating a client must never wait on I/O. The primary source is a local
//!   prefix table ([`crate::geoip`]) — a binary search over static bytes, no
//!   network, safe to run inline. Only the OPTIONAL remote fallback is a network
//!   call, and it stays on a background worker: an address the table cannot
//!   place is answered from the existing health-ordered set *immediately* and
//!   resolved later, never waited on.
//! * **The answer says how specific it is.** A proximity answer is only valid
//!   for the subnet it was computed for, so the response echoes the ECS option
//!   with a SCOPE PREFIX-LENGTH; a generic answer echoes scope 0. Without that,
//!   a resolver caches one client's nearest-node answer and serves it to
//!   everyone behind it.
//!
//! The remote-memo cache those constraints imply is **durable**
//! ([`GeoCache::spawn`] loads it, a debounced background writer saves it).
//! Purely in-memory, every restart — a roll, a crash loop, a `systemctl
//! restart` — emptied it and sent EVERY remotely-located client prefix back
//! through the generic answer until its background lookup completed again.
//! Live-witnessed: an already-tailored prefix (scope /24, correct nearest node
//! first) flipped to scope 0 with generic ordering immediately after a restart
//! and only re-warmed ~30s later. Across a rolling fleet update that
//! de-tailors those prefixes for a window; a crash-looping node never tailors
//! them at all.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use hive_edge::{haversine_km, NodeInfo};
use serde::{Deserialize, Serialize};

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
            return if off + 2 <= msg.len() {
                Some(off + 2)
            } else {
                None
            };
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
    // Distance, then name: a deterministic tie-break so two nodes at the same
    // site don't reorder between passes and defeat resolver caching.
    located.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
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

/// A cache entry plus WHEN it was learned. The timestamp is what makes expiry
/// (below) and reload-side pruning possible at all; without it a restored file
/// would be indistinguishable from a fresh lookup no matter how old it was.
#[derive(Clone, Copy, Debug)]
struct Slot {
    entry: Entry,
    at_ms: u64,
}

/// How long each kind of entry stays valid. Geolocation data DOES go stale
/// (prefixes get reassigned between regions, new allocations move), so entries
/// age out — but the three kinds age at deliberately different rates:
///
/// * **Known — 30 days.** A prefix→city mapping changes on the timescale of
///   registry reallocation, not hours, and the answer it feeds is "which of ~10
///   nodes is nearest", a decision that survives being a few hundred km wrong.
///   Expiring aggressively would buy accuracy nobody can measure while
///   re-spending the paced lookup budget (1 per 1.5s) on prefixes we already
///   know — the exact cost this cache exists to avoid.
/// * **Unlocatable — 6 hours.** A negative is as likely to be a rate-limited or
///   timed-out lookup as a genuinely unplaceable prefix, and a long-lived
///   negative pins that prefix to generic answers. Short TTL = the mistake
///   self-corrects.
/// * **Pending — 60s.** Pending means "a lookup is in flight". If the worker
///   died, or the process was killed mid-lookup, a Pending entry with nothing
///   behind it would pin that prefix generic FOREVER (the pre-existing
///   `contains_key` check returned early for it on every later query). Ageing it
///   out re-queues instead. This is also why Pending is never persisted.
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

/// Subnet → location resolver.
///
/// Two tiers, in this order:
///
/// 1. **The local table** ([`crate::geoip`]) — a committed prefix→coordinate
///    blob embedded in the binary. Answers in a few hundred nanoseconds with no
///    network, no rate limit, and nothing about the client leaving the node.
///    This is the whole path in the default configuration.
/// 2. **An optional remote endpoint** (`HIVE_DNS_GEO_ENDPOINT`) for the prefixes
///    the table cannot place. OFF unless configured: the point of the table is
///    that a production data path owes nothing to a third party. When it IS
///    configured it keeps the original shape — queued to a paced background
///    worker, results memoised here — so it can never stall a response.
///
/// The memo map exists only for tier 2. Tier-1 hits are deliberately NOT cached:
/// re-running the binary search costs less than the write lock would, and
/// filling a capped map with entries that are free to recompute would evict the
/// remote answers that actually cost something.
///
/// The tier-2 memo is DURABLE: loaded from `$HIVE_DATA/dns_geo.json` at boot
/// and saved by a debounced background writer (see the module doc), so a
/// restart does not throw remotely-learned answers away.
pub struct GeoCache {
    entries: parking_lot::RwLock<std::collections::HashMap<IpAddr, Slot>>,
    queue: tokio::sync::mpsc::UnboundedSender<IpAddr>,
    /// Prefixes answered from the local table vs. handed to the remote path.
    /// Reported by `GET /v1/dns/stats`: without the split there is no way to see
    /// whether the table is carrying the traffic or quietly missing everything.
    local_hits: AtomicU64,
    local_misses: AtomicU64,
    /// Bumped whenever a DURABLE entry changes (the background worker resolving
    /// a lookup — the only writer of Known/Unlocatable). The saver compares it
    /// against `saved` and skips the write entirely when nothing moved.
    dirty: AtomicU64,
    saved: AtomicU64,
    /// How many entries came back off disk at boot, and how many durable writes
    /// have happened. Both surfaced by `GET /v1/dns/stats`: "did the cache
    /// survive the restart" is otherwise unanswerable without a live dig.
    loaded_at_boot: AtomicU64,
    writes: AtomicU64,
    /// Serializes file writes so the debounced saver and a shutdown
    /// `flush_blocking()` can never interleave two temp-file+rename sequences.
    file_lock: std::sync::Mutex<()>,
}

/// Upper bound on memoised remote lookups. A DNS server is an open surface —
/// without a cap, arbitrary queries grow this map for as long as the process
/// runs. It is enforced on LOAD as well as at runtime, so no on-disk file
/// (older build with a larger cap, hand-edited, corrupted-by-append) can reload
/// past it.
const MAX_ENTRIES: usize = 8192;

/// The remote endpoint, or `None` when nobody configured one (the default).
/// Read per call rather than cached in a static: it is only consulted on a local
/// miss, which is rare, and a plain env read there beats another global.
fn remote_endpoint() -> Option<String> {
    std::env::var("HIVE_DNS_GEO_ENDPOINT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl GeoCache {
    /// Create the cache and spawn the OPTIONAL remote-lookup worker.
    ///
    /// `http` is the platform's shared client. The worker idles forever unless
    /// `HIVE_DNS_GEO_ENDPOINT` is set, because nothing queues to it otherwise.
    /// Lookups are PACED: a remote geo service rate-limits per source IP, and
    /// getting blocked there would take proximity down fleet-wide rather than
    /// degrade it.
    ///
    /// The prior memo contents are loaded from disk FIRST, synchronously: this
    /// runs once at `CloudState::new` and reads one small file, and doing it
    /// before the server can take a query is the whole point — the first query
    /// for a previously-known prefix must already be tailored.
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
                // The write guard is scoped to this block DELIBERATELY: a
                // `parking_lot` guard is not `Send`, so leaving it merely
                // `drop()`ed before the await below still makes the whole task
                // future non-Send and `tokio::spawn` rejects it.
                {
                    let mut w = cache.entries.write();
                    let prev = w.get(&net).copied();
                    let entry = match located {
                        Some((lat, lon)) => Entry::Known(lat, lon),
                        // A FAILED lookup is not proof the prefix is
                        // unplaceable — the service rate-limits and times out.
                        // Never downgrade a location we already hold; only a
                        // prefix with no known fix becomes a negative.
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
        // Debounced saver. Deliberately a SEPARATE task on a timer rather than a
        // write per learned entry: the DNS hot path must never touch the disk,
        // and lookups arrive one per `pace` (1.5s), so a per-entry write would
        // rewrite the file every 1.5s for hours during a cold warm-up. Ticking
        // and writing only when the dirty counter moved coalesces a burst into
        // one write; the interval bounds what an unclean kill can lose to a
        // handful of prefixes, each of which simply re-looks-up.
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
                    // spawn_blocking: the write ends in an fsync, which must not
                    // sit on an async worker thread.
                    let done = tokio::task::spawn_blocking(move || arc.write_rows(rows)).await;
                    match done {
                        Ok(Ok(())) => {
                            cache.saved.store(target, Ordering::Relaxed);
                            cache.writes.fetch_add(1, Ordering::Relaxed);
                        }
                        // Leave `saved` behind `dirty` so the next tick retries.
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

    /// The client's location. Never blocks, and no disk I/O happens here —
    /// persistence is entirely the background writer's job.
    ///
    /// Order matters and is the whole design: the local table first (in-process,
    /// no I/O, answers the FIRST query for a prefix as well as the thousandth),
    /// then the memo of previous remote answers, and only then — if an operator
    /// configured a remote endpoint at all — a queued background lookup that
    /// this query does not wait for.
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
        // Fast path: a fresh memo entry answers under the read lock alone.
        if let Some(s) = self.entries.read().get(&net).copied() {
            if s.is_fresh(now) {
                return match s.entry {
                    Entry::Known(lat, lon) => Some((lat, lon)),
                    _ => None,
                };
            }
        }
        // Nothing further to try when there is no remote endpoint: the local
        // table IS the answer, and the miss is honest ("this prefix is not
        // placeable"), so the query gets the generic health-ordered set. Not
        // recording anything here keeps the map exclusively about remote work.
        if remote_endpoint().is_none() {
            return None;
        }
        // Unknown, or expired → (re)queue a lookup under the write lock so
        // exactly one caller queues it.
        let mut w = self.entries.write();
        let existing = w.get(&net).copied();
        if let Some(s) = existing {
            if s.is_fresh(now) {
                // Raced another caller who just refreshed it.
                return match s.entry {
                    Entry::Known(lat, lon) => Some((lat, lon)),
                    _ => None,
                };
            }
        }
        if existing.is_none() && w.len() >= MAX_ENTRIES {
            // Full: drop expired entries, then the ones carrying no location,
            // before refusing to learn anything new — a burst of unlocatable
            // traffic must not permanently freeze the cache against real clients.
            w.retain(|_, v| v.is_fresh(now));
            if w.len() >= MAX_ENTRIES {
                w.retain(|_, v| matches!(v.entry, Entry::Known(..)));
            }
            if w.len() >= MAX_ENTRIES {
                return None;
            }
        }
        // Serve-stale-while-revalidate: an expired Known keeps its coordinates
        // (only the timestamp moves, so the refresh is queued exactly once)
        // rather than reverting to Pending. Expiry must never itself cause the
        // de-tailoring blip this whole module is about.
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
        let mut v: Vec<IpAddr> = r
            .iter()
            .filter(|(_, e)| matches!(e.entry, Entry::Known(..)))
            .map(|(k, _)| *k)
            .collect();
        v.sort();
        v
    }

    /// What the DNS stats endpoint reports. Split by TIER on purpose: "the
    /// table answered it" and "a remote service answered it" are different
    /// facts, and collapsing them would hide a table that has silently stopped
    /// covering real traffic.
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

    /// Write the cache NOW, synchronously (graceful shutdown). Called next to
    /// `persist::flush_blocking` on SIGTERM so a clean `systemctl restart` loses
    /// nothing from the debounce window. Best-effort by construction: a cache is
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

    /// Snapshot the durable half of the map. Pending is deliberately excluded:
    /// it describes an in-flight lookup on a process that is about to not exist,
    /// and restoring one would suppress the re-queue for its whole TTL.
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

// ---------------------------------------------------------------------------
// On-disk form.
//
// WHERE: `$HIVE_DATA/dns_geo.json` — the platform's established data dir
// (`persist::data_dir()`), as its own small sidecar file exactly like
// `peer_iroh.json` / `peer_guardian_addr.json`. Deliberately NOT a field on
// `PlatformSnapshot`: that snapshot is the platform's operational record and
// every write of it re-serializes the ENTIRE state (deployments, builds, logs,
// metrics) and fsyncs it, plus partitions per-tenant namespace docs and
// replicates into GuardianDB. A geolocation cache is node-local derived data
// shaped by whichever clients happen to query THIS node — it must not ride the
// platform-state write path, and it must not replicate across the mesh.
//
// SHAPE: one row per prefix — the masked network key as a plain string, lat/lon
// (absent = the lookup ran and could not place it), and the learn time. ~55
// bytes per entry, so a full 8192-entry cache is well under a megabyte.
// ---------------------------------------------------------------------------

const CACHE_FILE: &str = "dns_geo.json";
/// Bumped only if the row shape changes incompatibly; a file carrying anything
/// else is ignored wholesale rather than half-interpreted.
const FORMAT_VERSION: u32 = 1;
/// Refuse to read a file larger than this. The cap above bounds what this
/// process writes, but nothing bounds what it might FIND on disk — reading an
/// arbitrarily large file into a String at boot is how a corrupt sidecar turns
/// into an OOM before the node ever serves a query.
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

/// Debounce interval for the background saver. `HIVE_DNS_GEO_SAVE_MS=0` turns
/// persistence off entirely (no load, no save) for an operator who wants this
/// node's DNS path to touch no disk at all.
///
/// 10s by default: lookups are paced at one per 1.5s, so a tick that fires only
/// when something changed folds a warm-up burst into one write, while bounding
/// an unclean-kill loss to a few prefixes — each of which costs a single generic
/// answer and re-looks-up by itself.
fn save_interval() -> Option<std::time::Duration> {
    let ms = std::env::var("HIVE_DNS_GEO_SAVE_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(10_000);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// Load the persisted cache. EVERY failure mode — missing, unreadable,
/// oversized, malformed, wrong version, garbage rows — degrades to an empty
/// cache and a log line. A geolocation cache is an optimisation; refusing to
/// boot (or panicking) because of one would turn a corrupt scratch file into a
/// DNS outage for the whole delegated zone.
fn load_from_disk() -> std::collections::HashMap<IpAddr, Slot> {
    let empty = std::collections::HashMap::new();
    if save_interval().is_none() {
        return empty;
    }
    let path = cache_path();
    // Absent is the normal first-boot case, not an error worth logging.
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
            // Re-mask on the way in: a foreign or hand-edited file must not be
            // able to seed a full host address as a key, which would never be
            // hit by `locate` (it always looks up the masked key) and would
            // just consume cap.
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
                // Missing/garbage coordinates read as the negative entry, never
                // as a bogus location that would then steer real clients.
                _ => Entry::Unlocatable,
            };
            // Clamp a future timestamp (clock skew, hand-edit) to now, so no
            // entry can make itself immortal by claiming to be from 2099.
            let slot = Slot {
                entry,
                at_ms: e.t.min(now),
            };
            slot.is_fresh(now).then_some((net, slot))
        })
        .collect();
    // Newest first, then insert under the cap: whatever the file holds, the
    // in-memory map starts within MAX_ENTRIES and keeps the freshest rows.
    rows.sort_by(|a, b| b.1.at_ms.cmp(&a.1.at_ms));
    let mut out = std::collections::HashMap::with_capacity(rows.len().min(MAX_ENTRIES));
    for (net, slot) in rows {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        out.entry(net).or_insert(slot); // duplicate keys: newest wins
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

/// Geolocate one subnet through the OPTIONAL remote endpoint.
///
/// `HIVE_DNS_GEO_ENDPOINT` is now the only way this runs: there is no default
/// third party any more. It stays supported for an operator running their own
/// geolocation service (or who wants a commercial database's coverage on the
/// prefixes the shipped table misses) — the URL shape is unchanged,
/// `<endpoint>/<ip>?fields=status,lat,lon` returning ip-api's JSON.
async fn lookup_geo(http: &reqwest::Client, net: IpAddr) -> Option<(f64, f64)> {
    let base = remote_endpoint()?;
    // A /24's first address is reserved, so ask about a host inside it.
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
