//! Mainline-DHT address lookup — the only address source that needs no fleet peer reachable first.
//! Strictly additive (iroh polls every provider concurrently). Publishing is a BEP-44 item signed by
//! the node's own key under its `EndpointId`, readable by anyone: relay URL only by default,
//! `HIVE_DHT_PUBLISH_DIRECT=1` adds globally-routable direct addresses (private ranges stripped by
//! [`relay_and_public_ip_filter`]), `HIVE_DHT_PUBLISH=0` resolve-only, `HIVE_DISCOVERY_DHT=0` off.
//! Never a trust channel: a DHT-found peer still passes the gossip/ALPN trust gates. Bootstrap
//! hostnames are resolved HERE under their own budget (`DhtBuilder::build()` resolves blocking inside
//! `bind()`), and any construction failure leaves the provider unregistered with a WARN, never a
//! failed bind.

use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use iroh::address_lookup::{
    AddrFilter, AddressLookup, EndpointData, Error as LookupError, Item as LookupItem,
};
use iroh::{EndpointId, SecretKey, TransportAddr};
use iroh_mainline_address_lookup::DhtAddressLookup;
use n0_future::boxed::BoxStream;
use n0_future::{Stream, StreamExt};
use n0_mainline::DhtBuilder;

/// Bootstrap nodes for the public Mainline DHT (copied from `n0_mainline`'s unexported list,
/// resolved here rather than on the bind path); `HIVE_DHT_BOOTSTRAP` replaces it.
const DEFAULT_BOOTSTRAP: [&str; 4] = [
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
    "relay.pkarr.org:6881",
];

/// Total budget for resolving the bootstrap hostnames — far below `main.rs`'s 8 s bind timeout,
/// since this runs before `bind()`.
const DEFAULT_BOOTSTRAP_RESOLVE_MS: u64 = 2_000;

/// Per-resolve budget for the `--dht-probe` operator diagnostic.
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 30_000;

/// Live discovery counters, read by `GET /v1/mesh/discovery`; honest about what can be witnessed
/// from outside the lookup (see [`Snapshot::publishes_requested`]).
static STATS: Stats = Stats::new();

struct Stats {
    seed_providers: AtomicUsize,
    pkarr_providers: AtomicUsize,
    n0_enabled: AtomicBool,
    dht_registered: AtomicBool,
    dht_bootstrap_nodes: AtomicUsize,
    dht_port: AtomicU64,
    dht_publish: AtomicBool,
    dht_publish_direct: AtomicBool,
    publishes_requested: AtomicU64,
    last_publish_request_ms: AtomicU64,
    resolves: AtomicU64,
    resolve_hits: AtomicU64,
    resolve_misses: AtomicU64,
    resolve_errors: AtomicU64,
    skip_reason: Mutex<Option<String>>,
}

impl Stats {
    const fn new() -> Self {
        Self {
            seed_providers: AtomicUsize::new(0),
            pkarr_providers: AtomicUsize::new(0),
            n0_enabled: AtomicBool::new(false),
            dht_registered: AtomicBool::new(false),
            dht_bootstrap_nodes: AtomicUsize::new(0),
            dht_port: AtomicU64::new(0),
            dht_publish: AtomicBool::new(false),
            dht_publish_direct: AtomicBool::new(false),
            publishes_requested: AtomicU64::new(0),
            last_publish_request_ms: AtomicU64::new(0),
            resolves: AtomicU64::new(0),
            resolve_hits: AtomicU64::new(0),
            resolve_misses: AtomicU64::new(0),
            resolve_errors: AtomicU64::new(0),
            skip_reason: Mutex::new(None),
        }
    }
}

/// One read of [`STATS`]. Field names state exactly what was witnessed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Snapshot {
    /// `MemoryLookup` entries registered from `HIVE_BOOTSTRAP_PEERS`.
    pub seed_providers: usize,
    /// pkarr publisher+resolver PAIRS registered from `HIVE_DISCOVERY_DNS`.
    pub pkarr_providers: usize,
    /// Whether n0's public pkarr/DNS lookup is registered (`HIVE_DISCOVERY_N0`).
    pub n0_enabled: bool,
    /// Whether the mainline-DHT provider is registered on this process.
    pub dht_registered: bool,
    /// Why it is NOT registered, when it is not. `None` while registered.
    pub dht_skip_reason: Option<String>,
    /// Bootstrap addresses this node actually resolved and handed to the DHT.
    pub dht_bootstrap_nodes: usize,
    /// `0` = the crate default (try 6881, fall back to ephemeral).
    pub dht_port: u64,
    pub dht_publish: bool,
    pub dht_publish_direct: bool,
    /// Publishes REQUESTED of the lookup by iroh (one per address change), counted only while
    /// publishing is enabled; `AddressLookup::publish` is fire-and-forget, so success is not witnessable.
    pub publishes_requested: u64,
    /// Unix ms of the most recent publish request, `0` if never.
    pub last_publish_request_ms: u64,
    /// `resolve()` calls iroh made against the DHT provider.
    pub resolves: u64,
    /// Resolve streams that yielded at least one address item.
    pub resolve_hits: u64,
    /// Resolve streams that ended without yielding any address item.
    pub resolve_misses: u64,
    /// Address items that came back as an error.
    pub resolve_errors: u64,
}

/// Read the live discovery counters.
pub fn stats() -> Snapshot {
    Snapshot {
        seed_providers: STATS.seed_providers.load(Ordering::Relaxed),
        pkarr_providers: STATS.pkarr_providers.load(Ordering::Relaxed),
        n0_enabled: STATS.n0_enabled.load(Ordering::Relaxed),
        dht_registered: STATS.dht_registered.load(Ordering::Relaxed),
        dht_skip_reason: STATS.skip_reason.lock().ok().and_then(|g| g.clone()),
        dht_bootstrap_nodes: STATS.dht_bootstrap_nodes.load(Ordering::Relaxed),
        dht_port: STATS.dht_port.load(Ordering::Relaxed),
        dht_publish: STATS.dht_publish.load(Ordering::Relaxed),
        dht_publish_direct: STATS.dht_publish_direct.load(Ordering::Relaxed),
        publishes_requested: STATS.publishes_requested.load(Ordering::Relaxed),
        last_publish_request_ms: STATS.last_publish_request_ms.load(Ordering::Relaxed),
        resolves: STATS.resolves.load(Ordering::Relaxed),
        resolve_hits: STATS.resolve_hits.load(Ordering::Relaxed),
        resolve_misses: STATS.resolve_misses.load(Ordering::Relaxed),
        resolve_errors: STATS.resolve_errors.load(Ordering::Relaxed),
    }
}

/// Record the non-DHT providers `bind_full` registered, so the stats endpoint can answer which
/// address sources this node has.
pub(crate) fn record_providers(seeds: usize, pkarr: usize, n0: bool) {
    STATS.seed_providers.store(seeds, Ordering::Relaxed);
    STATS.pkarr_providers.store(pkarr, Ordering::Relaxed);
    STATS.n0_enabled.store(n0, Ordering::Relaxed);
}

fn skip(reason: impl Into<String>) -> Option<CountedDht> {
    let reason = reason.into();
    tracing::warn!(
        reason = %reason,
        "mainline DHT address lookup NOT registered — seeds/Seer/relay paths unchanged"
    );
    mark_unregistered(Some(reason));
    None
}

/// Reset every DHT-provider field to "not registered" — CLEAR, not merely not-set, so a second
/// bind that skips cannot leave a stale `registered: true`.
fn mark_unregistered(reason: Option<String>) {
    STATS.dht_registered.store(false, Ordering::Relaxed);
    STATS.dht_bootstrap_nodes.store(0, Ordering::Relaxed);
    STATS.dht_port.store(0, Ordering::Relaxed);
    STATS.dht_publish.store(false, Ordering::Relaxed);
    STATS.dht_publish_direct.store(false, Ordering::Relaxed);
    if let Ok(mut g) = STATS.skip_reason.lock() {
        *g = reason;
    }
}

/// Render an error with every `source()` beneath it.
fn full_cause(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        let s = e.to_string();
        if !s.is_empty() && !out.contains(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        cur = e.source();
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn env_flag(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => default,
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// Bootstrap entries to resolve: `HIVE_DHT_BOOTSTRAP` (comma-separated
/// `ip:port` or `host:port`) if set and non-empty, else [`DEFAULT_BOOTSTRAP`].
fn bootstrap_entries() -> Vec<String> {
    let configured: Vec<String> = std::env::var("HIVE_DHT_BOOTSTRAP")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if configured.is_empty() {
        DEFAULT_BOOTSTRAP.iter().map(|s| s.to_string()).collect()
    } else {
        configured
    }
}

/// Resolve every bootstrap entry to IPv4 socket addresses concurrently under ONE budget; a partial
/// set is a working DHT, only an empty set disables the provider. IPv6 results are dropped
/// (`KrpcSocket` binds `0.0.0.0`).
async fn resolve_bootstrap(entries: &[String], budget: Duration) -> Vec<SocketAddr> {
    let deadline = tokio::time::Instant::now() + budget;
    let handles: Vec<_> = entries
        .iter()
        .cloned()
        .map(|e| {
            tokio::spawn(async move {
                if let Ok(sa) = e.parse::<SocketAddr>() {
                    return vec![sa];
                }
                match tokio::net::lookup_host(e.as_str()).await {
                    Ok(it) => it.filter(|sa| sa.is_ipv4()).collect(),
                    Err(err) => {
                        tracing::debug!(entry = %e, error = %err, "dht bootstrap host did not resolve");
                        Vec::new()
                    }
                }
            })
        })
        .collect();
    let mut out: Vec<SocketAddr> = Vec::new();
    for h in handles {
        match tokio::time::timeout_at(deadline, h).await {
            Ok(Ok(addrs)) => out.extend(addrs),
            Ok(Err(_)) => {}
            Err(_) => {
                tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    resolved = out.len(),
                    "dht bootstrap resolution hit its budget; using what resolved so far"
                );
                break;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Publish filter: home relay URLs plus GLOBALLY-ROUTABLE direct addresses only —
/// `AddrFilter::unfiltered()` would publish the node's RFC1918 VPC addresses to the world.
fn relay_and_public_ip_filter() -> AddrFilter {
    AddrFilter::new(|addrs| {
        Cow::Owned(
            addrs
                .iter()
                .filter(|a| match a {
                    TransportAddr::Relay(_) => true,
                    TransportAddr::Ip(sa) => is_publicly_routable(sa.ip()),
                    _ => false,
                })
                .cloned()
                .collect(),
        )
    })
}

/// Conservative "could a stranger on the internet dial this" (`IpAddr::is_global` is unstable, so
/// the classes are spelled out); when in doubt, NO.
pub(crate) fn is_publicly_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || o[0] == 0
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)))
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xffc0) == 0xfe80
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] == 0x2001 && seg[1] == 0x0db8))
        }
    }
}

/// Build the mainline-DHT address lookup from the environment, or `None` on every failure path
/// (strictly additive; boot proceeds unchanged). `secret` signs the record; `None` ⇒ resolve-only.
pub(crate) async fn lookup_from_env(secret: Option<&SecretKey>) -> Option<CountedDht> {
    if !env_flag("HIVE_DISCOVERY_DHT", true) {
        tracing::info!("mainline DHT address lookup disabled (HIVE_DISCOVERY_DHT=0)");
        mark_unregistered(Some("disabled by HIVE_DISCOVERY_DHT".into()));
        return None;
    }

    let entries = bootstrap_entries();
    let budget = Duration::from_millis(env_u64(
        "HIVE_DHT_BOOTSTRAP_RESOLVE_MS",
        DEFAULT_BOOTSTRAP_RESOLVE_MS,
    ));
    let bootstrap = resolve_bootstrap(&entries, budget).await;
    if bootstrap.is_empty() {
        return skip(format!(
            "no DHT bootstrap address resolved from {} entr(ies) within {}ms",
            entries.len(),
            budget.as_millis()
        ));
    }

    let publish = secret.is_some() && env_flag("HIVE_DHT_PUBLISH", true);
    let publish_direct = publish && env_flag("HIVE_DHT_PUBLISH_DIRECT", false);
    let port = std::env::var("HIVE_DHT_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok());

    let mut dht_builder = DhtBuilder::default();
    let literals: Vec<String> = bootstrap.iter().map(|sa| sa.to_string()).collect();
    dht_builder.bootstrap(&literals);
    if let Some(p) = port {
        dht_builder.port(p);
    }

    let mut builder = DhtAddressLookup::builder()
        .dht_builder(dht_builder)
        .addr_filter(if publish_direct {
            relay_and_public_ip_filter()
        } else {
            AddrFilter::relay_only()
        });
    match secret {
        Some(sk) if publish => builder = builder.secret_key(sk.clone()),
        _ => builder = builder.no_publish(),
    }

    let lookup = match builder.build() {
        Ok(l) => l,
        Err(e) => return skip(format!("DHT construction failed: {}", full_cause(&e))),
    };

    STATS.dht_registered.store(true, Ordering::Relaxed);
    STATS
        .dht_bootstrap_nodes
        .store(bootstrap.len(), Ordering::Relaxed);
    STATS
        .dht_port
        .store(port.unwrap_or(0) as u64, Ordering::Relaxed);
    STATS.dht_publish.store(publish, Ordering::Relaxed);
    STATS
        .dht_publish_direct
        .store(publish_direct, Ordering::Relaxed);
    if let Ok(mut g) = STATS.skip_reason.lock() {
        *g = None;
    }
    tracing::info!(
        bootstrap = bootstrap.len(),
        port = port.unwrap_or(0),
        publish,
        publish_direct,
        "mainline DHT address lookup registered"
    );
    Some(CountedDht { lookup, publish })
}

/// [`DhtAddressLookup`] with publish/resolve counters attached — a pass-through that lets
/// `GET /v1/mesh/discovery` report what the provider DID.
#[derive(Debug)]
pub(crate) struct CountedDht {
    lookup: DhtAddressLookup,
    /// Whether the inner lookup holds a secret key and will actually write; iroh calls `publish()`
    /// regardless, so the counter is gated on the same flag.
    publish: bool,
}

impl AddressLookup for CountedDht {
    fn publish(&self, data: &EndpointData) {
        if self.publish {
            STATS.publishes_requested.fetch_add(1, Ordering::Relaxed);
            STATS
                .last_publish_request_ms
                .store(now_ms(), Ordering::Relaxed);
        }
        self.lookup.publish(data);
    }

    fn resolve(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<BoxStream<Result<LookupItem, LookupError>>> {
        STATS.resolves.fetch_add(1, Ordering::Relaxed);
        let inner = self.lookup.resolve(endpoint_id)?;
        Some(Box::pin(CountedResolve {
            inner,
            yielded_ok: false,
        }))
    }
}

/// Counts the OUTCOME of one resolve on `Drop`, since iroh drops a resolve stream as soon as it
/// has an answer it likes.
struct CountedResolve {
    inner: BoxStream<Result<LookupItem, LookupError>>,
    yielded_ok: bool,
}

impl Stream for CountedResolve {
    type Item = Result<LookupItem, LookupError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = self.get_mut();
        let out = Pin::new(&mut me.inner).poll_next(cx);
        match &out {
            Poll::Ready(Some(Ok(_))) => me.yielded_ok = true,
            Poll::Ready(Some(Err(_))) => {
                STATS.resolve_errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        out
    }
}

impl Drop for CountedResolve {
    fn drop(&mut self) {
        if self.yielded_ok {
            STATS.resolve_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            STATS.resolve_misses.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One successful `--dht-probe` answer.
#[derive(Debug, Clone)]
pub struct ProbeHit {
    pub endpoint_id: String,
    pub relay_urls: Vec<String>,
    pub direct_addrs: Vec<String>,
    pub elapsed_ms: u64,
    pub attempts: u32,
}

/// Resolve `endpoint_id` through the public DHT ONLY, from this host, without binding an iroh
/// `Endpoint` (no seeds, no pkarr relay, no cached `peer_iroh.json`, no QUIC-port collision).
/// Retries until `budget` expires: a cold routing table legitimately misses the first attempts.
pub async fn probe(endpoint_id: &str, budget: Duration) -> Result<Option<ProbeHit>> {
    let id: EndpointId = endpoint_id
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("not a valid iroh endpoint id: {e}"))?;
    let entries = bootstrap_entries();
    let resolve_budget = Duration::from_millis(env_u64(
        "HIVE_DHT_BOOTSTRAP_RESOLVE_MS",
        DEFAULT_BOOTSTRAP_RESOLVE_MS,
    ));
    let bootstrap = resolve_bootstrap(&entries, resolve_budget).await;
    if bootstrap.is_empty() {
        anyhow::bail!(
            "no DHT bootstrap address resolved from {} entr(ies) within {}ms",
            entries.len(),
            resolve_budget.as_millis()
        );
    }
    let mut dht_builder = DhtBuilder::default();
    let literals: Vec<String> = bootstrap.iter().map(|sa| sa.to_string()).collect();
    dht_builder.bootstrap(&literals);
    dht_builder.port(0);
    let lookup = DhtAddressLookup::builder()
        .dht_builder(dht_builder)
        .no_publish()
        .build()
        .map_err(|e| anyhow::anyhow!("DHT construction failed: {e}"))?;

    let started = Instant::now();
    let mut attempts = 0u32;
    while started.elapsed() < budget {
        attempts += 1;
        let mut relay_urls = Vec::new();
        let mut direct_addrs = Vec::new();
        if let Some(mut stream) = lookup.resolve(id) {
            let remaining = budget.saturating_sub(started.elapsed());
            let collected = tokio::time::timeout(remaining, async {
                let mut items = Vec::new();
                while let Some(item) = stream.next().await {
                    items.push(item);
                }
                items
            })
            .await
            .unwrap_or_default();
            for item in collected.into_iter().flatten() {
                for addr in item.to_endpoint_addr().addrs {
                    match addr {
                        TransportAddr::Relay(u) => relay_urls.push(u.to_string()),
                        TransportAddr::Ip(sa) => direct_addrs.push(sa.to_string()),
                        other => direct_addrs.push(other.to_string()),
                    }
                }
            }
        }
        if !relay_urls.is_empty() || !direct_addrs.is_empty() {
            relay_urls.sort();
            relay_urls.dedup();
            direct_addrs.sort();
            direct_addrs.dedup();
            return Ok(Some(ProbeHit {
                endpoint_id: id.to_string(),
                relay_urls,
                direct_addrs,
                elapsed_ms: started.elapsed().as_millis() as u64,
                attempts,
            }));
        }
        if started.elapsed() >= budget {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(None)
}

/// Default probe budget, overridable with `HIVE_DHT_PROBE_TIMEOUT_MS`.
pub fn probe_budget() -> Duration {
    Duration::from_millis(env_u64(
        "HIVE_DHT_PROBE_TIMEOUT_MS",
        DEFAULT_PROBE_TIMEOUT_MS,
    ))
}
