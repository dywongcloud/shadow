//! Local IP-prefix to coordinate table: the geo half of Seer's proximity answers
//! (`crate::dns_geo`), resolved in-process with no network call at all.
//!
//! # Why a table
//!
//! A shipped table replaced a paced HTTP lookup against a free third-party service. That service
//! rate-limits per source IP, can go down, and was handed every client prefix to log, while
//! degradation was graceful (an unplaceable prefix just gets the generic health-ordered answer), so
//! it was a dependency with no upside. The table gives the same answer with no request, no rate
//! limit, nothing leaked, and it works on a node with no egress at all.
//!
//! # Where the table comes from
//!
//! `assets/geoloc.bin`, built by `scripts/gen-geo-table.py` from DB-IP's "IP to City Lite" CSV
//! (CC-BY-4.0, which is why it, and not MaxMind's otherwise equivalent GeoLite2, is the input). The
//! blob is COMMITTED and `include_bytes!`-embedded: a build needs no network and a node needs no
//! provisioning step. Refreshing it is deliberate (re-run the generator, commit, roll the fleet
//! like any other binary change); prefix to city assignments move over months, and a stale table
//! degrades to "slightly wrong nearest node", never to a failure.
//!
//! # Size and memory cost
//!
//! About 5 MB of `.rodata`, on the order of a million six-byte rows. Being static, it is
//! demand-paged straight from the executable and never copied to the heap: a binary search touches
//! ~20 pages, so the RESIDENT cost is tens of KB of clean, evictable page cache, which matters on
//! small-RAM nodes. Loading the same data into a `HashMap` would cost tens of MB of dirty heap on
//! every node to answer the same question no faster. The source ranges reduce to those rows through
//! two lossy steps (coordinates snapped to a grid cell, adjacent runs folded within a distance
//! tolerance), both measured against the raw data by the generator.
//!
//! # Lookup
//!
//! Rows are `(start: u32, cell: u16)` sorted ascending, meaning "from `start` until the next row's
//! start, the location is `cell`", so a longest-prefix match degenerates to `partition_point` over
//! a sorted range table. IPv6 keys on the address's top 32 bits (a /32, one RIR allocation), so
//! both families share one row shape and one search. `cell == 0xFFFF` is an explicit hole: space
//! the source cannot place. It is a row, not a gap, precisely so a lookup inside it returns
//! "unknown" instead of silently inheriting the previous row's city.
//!
//! # When the table is absent
//!
//! It cannot be, for the embedded copy: a build without it does not link. The `HIVE_DNS_GEO_TABLE`
//! override can name a file that is missing or corrupt; a failed header/checksum check logs once
//! and falls back to the embedded table rather than running blind. A lookup that legitimately
//! misses (a hole, or a prefix newer than the table) returns `None`, the state `GeoCache` already
//! had a path for: a generic answer now, an optional remote lookup in the background if the
//! operator configured one.

use std::net::IpAddr;
use std::sync::OnceLock;

/// The table shipped with the binary: `include_bytes!` of a committed asset (an input, not a
/// derived artifact), because a build script that regenerated it would put the source CSV's
/// availability back on the build path.
static EMBEDDED: &[u8] = include_bytes!("../assets/geoloc.bin");

/// File header sentinel. The format version is the last byte, so a future v2 blob fails the magic
/// check outright instead of being parsed as v1 with garbage counts.
const MAGIC: &[u8; 8] = b"HIVEGEO\x01";
const HEADER_LEN: usize = 32;
const ROW_LEN: usize = 6;
/// Cell value meaning "the source data cannot place this span".
const CELL_UNKNOWN: u16 = 0xFFFF;

/// A validated, borrowed view of the blob. Holds no owned data: both the
/// embedded asset and a leaked override file are `&'static [u8]`.
struct Table {
    rows4: &'static [u8],
    rows6: &'static [u8],
    /// Grid geometry, read from the header rather than assumed, so a table
    /// generated at another resolution still decodes correctly.
    nlon: u32,
    deg: f64,
    /// Where this table came from — reported by `GET /v1/dns/stats` so an
    /// operator can tell the embedded table from an override at a glance.
    source: &'static str,
}

impl Table {
    /// Validates a blob and borrows its two row arrays. Every field the reader later trusts is
    /// checked HERE, once: magic, grid geometry, declared row counts against the actual length, and
    /// an FNV-1a checksum over the row bytes. Re-verifying per lookup would hash megabytes to
    /// answer one query, and the structure is immutable from this point on.
    fn parse(buf: &'static [u8], source: &'static str) -> Result<Self, String> {
        if buf.len() < HEADER_LEN {
            return Err(format!("truncated: {} bytes", buf.len()));
        }
        if &buf[..8] != MAGIC {
            return Err("bad magic (not a HIVEGEO v1 table)".into());
        }
        let deg_tenths = u16::from_le_bytes([buf[8], buf[9]]);
        let nlon = u16::from_le_bytes([buf[10], buf[11]]);
        let nlat = u16::from_le_bytes([buf[12], buf[13]]);
        let n4 = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]) as usize;
        let n6 = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]) as usize;
        let want_ck = u64::from_le_bytes(buf[24..32].try_into().unwrap_or_default());
        if deg_tenths == 0 || nlon == 0 || nlat == 0 {
            return Err("degenerate grid geometry".into());
        }
        if (nlat as u32) * (nlon as u32) > CELL_UNKNOWN as u32 {
            return Err("grid does not fit a u16 cell id".into());
        }
        let want_len = HEADER_LEN + (n4 + n6) * ROW_LEN;
        if buf.len() != want_len {
            return Err(format!("length {} != declared {}", buf.len(), want_len));
        }
        let rows = &buf[HEADER_LEN..];
        if fnv1a64(rows) != want_ck {
            return Err("row checksum mismatch".into());
        }
        let split = n4 * ROW_LEN;
        Ok(Self {
            rows4: &rows[..split],
            rows6: &rows[split..],
            nlon: nlon as u32,
            deg: deg_tenths as f64 / 10.0,
            source,
        })
    }

    /// Longest-prefix match: the last row whose start is `<= key`, found by a binary search over
    /// the borrowed bytes (~20 iterations, no allocation, no lock, no syscall), which is what makes
    /// it safe to call inline from the DNS loop.
    fn cell(&self, rows: &[u8], key: u32) -> u16 {
        let n = rows.len() / ROW_LEN;
        let idx = partition_point(n, |i| start_at(rows, i) <= key);
        if idx == 0 {
            return CELL_UNKNOWN;
        }
        let off = (idx - 1) * ROW_LEN + 4;
        u16::from_le_bytes([rows[off], rows[off + 1]])
    }

    /// Centre of a grid cell. The cell is a square of `deg` degrees, so its
    /// centre — not its corner — is the least-wrong single point for it.
    fn center(&self, cell: u16) -> Option<(f64, f64)> {
        if cell == CELL_UNKNOWN {
            return None;
        }
        let li = (cell as u32 / self.nlon) as f64;
        let lo = (cell as u32 % self.nlon) as f64;
        Some((
            li * self.deg - 90.0 + self.deg / 2.0,
            lo * self.deg - 180.0 + self.deg / 2.0,
        ))
    }

    fn locate(&self, ip: IpAddr) -> Option<(f64, f64)> {
        let cell = match ip {
            IpAddr::V4(a) => self.cell(self.rows4, u32::from_be_bytes(a.octets())),
            IpAddr::V6(a) => {
                let o = a.octets();
                self.cell(self.rows6, u32::from_be_bytes([o[0], o[1], o[2], o[3]]))
            }
        };
        self.center(cell)
    }
}

fn start_at(rows: &[u8], i: usize) -> u32 {
    let off = i * ROW_LEN;
    u32::from_le_bytes([rows[off], rows[off + 1], rows[off + 2], rows[off + 3]])
}

/// Index of the first `i` in `0..n` for which the monotone `pred(i)` is false. Written out rather
/// than reached for via `slice::partition_point` because the rows are a flat byte array, not a
/// slice of a row type: decoding every row into a `Vec<(u32, u16)>` to get one method back would
/// cost several MB of heap per node.
fn partition_point(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The process-wide table. `OnceLock` because it is immutable after boot and
/// read from the DNS loop: no lock, no refcount traffic, one validation.
static TABLE: OnceLock<Option<Table>> = OnceLock::new();

/// The process-wide table, resolved once. `HIVE_DNS_GEO_TABLE` lets an operator ship a fresher or
/// higher-resolution blob without a rebuild: it SUPERSEDES the embedded table only if it validates,
/// and a missing or corrupt file falls back to the embedded one, because a typo in a path must not
/// silently switch geo routing off. An override file is leaked deliberately: the table lives for
/// the whole process, and leaking once gives every lookup the same `&'static` zero-copy shape as
/// the embedded blob. Only a corrupt committed asset, which a build cannot detect, ends with no
/// table: that logs loudly and geo degrades to generic answers, never to a wrong one.
fn table() -> Option<&'static Table> {
    TABLE
        .get_or_init(|| {
            if let Ok(p) = std::env::var("HIVE_DNS_GEO_TABLE") {
                let p = p.trim().to_string();
                if !p.is_empty() {
                    match std::fs::read(&p) {
                        Ok(bytes) => match Table::parse(Box::leak(bytes.into_boxed_slice()), "file") {
                            Ok(t) => {
                                tracing::info!(path = %p, "geoip: using override table");
                                return Some(t);
                            }
                            Err(e) => tracing::warn!(path = %p, error = %e, "geoip: override table rejected, using embedded"),
                        },
                        Err(e) => tracing::warn!(path = %p, error = %e, "geoip: override table unreadable, using embedded"),
                    }
                }
            }
            match Table::parse(EMBEDDED, "embedded") {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::error!(error = %e, "geoip: embedded table rejected; proximity answers disabled");
                    None
                }
            }
        })
        .as_ref()
}

/// Coordinates for `ip` from the local table, or `None` when the table cannot place it (an explicit
/// hole, or space allocated after the table was built). Non-blocking by construction: a binary
/// search over static bytes with no I/O, and there must never be any, since `dns_geo` depends on
/// locating a client never stalling a DNS response.
pub fn locate(ip: IpAddr) -> Option<(f64, f64)> {
    table()?.locate(ip)
}

/// `(source, ipv4_rows, ipv6_rows)` for `GET /v1/dns/stats`. `("none", 0, 0)`
/// means validation failed and every answer is generic — the one state an
/// operator needs to be able to see without reading logs.
pub fn table_info() -> (&'static str, usize, usize) {
    match table() {
        Some(t) => (t.source, t.rows4.len() / ROW_LEN, t.rows6.len() / ROW_LEN),
        None => ("none", 0, 0),
    }
}
