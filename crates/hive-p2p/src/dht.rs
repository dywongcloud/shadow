//! **Mainline-DHT address lookup — the only address source that needs no fleet
//! peer to be reachable first.**
//!
//! # The problem this exists to solve
//!
//! Every other way a node learns a peer's CURRENT address requires a peer it can
//! already reach: `peer_iroh.json` (a cache, and per AGENTS.md it holds the
//! peers' PRIVATE 10.x/172.16/192.168 addrs), inbound gossip (someone else must
//! reach *us*), the GuardianDB roster (replicated over the very mesh we cannot
//! join), the `MemoryLookup` seeds (only as good as `HIVE_BOOTSTRAP_PEERS`), and
//! the self-hosted pkarr relay (`HIVE_DISCOVERY_DNS`, which must itself be
//! reachable). With `HIVE_DISCOVERY_N0=0` — the fleet default — and no seeds,
//! the provider list handed to `Endpoint::builder().address_lookup(..)` is
//! EMPTY, so `PeerPool::dial_fresh` has nothing to force a fresh resolve
//! against: it burns `discovery_budget()` and returns the same failure. Wipe the
//! data dir on such a node and it is permanently dark.
//!
//! The public BitTorrent Mainline DHT breaks that chicken-and-egg: its bootstrap
//! nodes are third-party infrastructure that has nothing to do with this fleet,
//! so a node with only egress can find a peer from cold.
//!
//! # What becomes PUBLICLY RESOLVABLE (read this before enabling)
//!
//! Publishing is a pkarr [BEP-44 mutable item]: a DNS packet signed by this
//! node's ed25519 secret key and stored on the public DHT under a key that IS
//! the node's `EndpointId`. Anyone on the internet who knows (or brute-scans)
//! that 64-hex id can read it — there is no read authorization on the DHT.
//!
//! * **Default (`HIVE_DHT_PUBLISH_DIRECT` unset/0):** only this node's HOME
//!   RELAY URL is published, i.e. `EndpointId -> https://<node>.relay.shadw.app`.
//!   That reveals the node's relay hostname and that the id is live; it does NOT
//!   reveal the node's IP.
//! * **`HIVE_DHT_PUBLISH_DIRECT=1`:** additionally publishes GLOBALLY-ROUTABLE
//!   direct socket addresses, i.e. `EndpointId -> <public ip>:<HIVE_IROH_PORT>`.
//!   RFC1918/CGNAT/loopback/link-local addresses are filtered out by
//!   [`relay_and_public_ip_filter`] and are never published — iroh's own
//!   `AddrFilter::unfiltered()` would have leaked the node's private VPC
//!   topology, which is why this module does not use it.
//! * **`HIVE_DHT_PUBLISH=0`:** publishes NOTHING; resolve-only. The node can
//!   still find peers through the DHT but is not findable through it.
//! * **`HIVE_DISCOVERY_DHT=0`:** the whole mechanism is off — no DHT socket, no
//!   publish, no resolve, no third-party packets at all.
//!
//! Nothing here is a trust, membership or authorization channel. A peer reached
//! via the DHT still passes the existing gossip trust gate
//! (`HIVE_GOSSIP_VERIFY=enforce`, the ALPN trust gate, the join proof). The
//! record is self-signed by the key that IS the `EndpointId`, so impersonation
//! is structurally impossible: nobody can point endpoint X at an address they
//! control. The worst a hostile record can do is cause a failed dial.
//!
//! # Why the bootstrap hostnames are pre-resolved here
//!
//! `n0_mainline::DhtBuilder::build()` runs a **blocking** `to_socket_addrs()`
//! over its bootstrap list, and it runs INSIDE `Endpoint::builder().bind()`,
//! which `hive-cloud`'s `main.rs` wraps in an 8s timeout whose failure arm is
//! "iroh bind failed — P2P transport disabled". A node with a slow or broken
//! resolver would therefore have turned an additive feature into a fleet
//! outage. So this module resolves the bootstrap names ITSELF, asynchronously,
//! under its own budget, and hands `DhtBuilder` nothing but IP literals — for
//! which `to_socket_addrs()` is a parse, not a DNS query. Zero resolved ⇒ the
//! provider is not registered at all and boot continues unchanged.
//!
//! For the same reason the lookup is BUILT here rather than passed to iroh as an
//! `AddressLookupBuilder`: iroh's `bind()` propagates a builder error and fails
//! the whole endpoint (`create_service.into_address_lookup(&ep)?`). Building it
//! ourselves means a UDP-port conflict or any other DHT construction failure
//! degrades to a WARN and an unregistered provider instead of a dark node.
//!
//! [BEP-44 mutable item]: https://www.bittorrent.org/beps/bep_0044.html

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

/// Bootstrap nodes for the public Mainline DHT.
///
/// Copied verbatim from `n0_mainline`'s own `DEFAULT_BOOTSTRAP_NODES`, which is
/// not publicly exported. Restated here because we must resolve these names
/// OURSELVES (see the module docs) rather than letting `DhtBuilder` do it on the
/// bind path. `HIVE_DHT_BOOTSTRAP` replaces the list entirely.
const DEFAULT_BOOTSTRAP: [&str; 4] = [
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
    "relay.pkarr.org:6881",
];

/// Total budget for resolving the bootstrap hostnames. Deliberately far below
/// `main.rs`'s 8s bind timeout: this runs before `bind()` and must never be the
/// reason a node fails to bring up its QUIC transport.
const DEFAULT_BOOTSTRAP_RESOLVE_MS: u64 = 2_000;

/// Per-resolve budget for the `--dht-probe` operator diagnostic.
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------------

/// Live discovery counters, read by `GET /v1/mesh/discovery`.
///
/// Without these the mechanism is unfalsifiable in production: "the DHT is
/// registered" is a config claim, whereas "37 resolves, 12 hits" is evidence.
/// Deliberately honest about what can and cannot be witnessed from outside the
/// lookup — see [`Snapshot::publishes_requested`].
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
    /// Publishes REQUESTED of the DHT lookup by iroh (each address change).
    ///
    /// Not "succeeded": `AddressLookup::publish` is fire-and-forget by contract
    /// and the crate's republish loop reports its own outcome only to its logs.
    /// Counting a request as a success would be a claim with no witness.
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

/// Record the non-DHT providers `bind_full` registered, so the stats endpoint
/// can answer "which address sources does this node actually have" — the
/// question whose real answer (none, on 12 of 14 nodes) motivated this module.
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
    if let Ok(mut g) = STATS.skip_reason.lock() {
        *g = Some(reason);
    }
    None
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Env
// ---------------------------------------------------------------------------

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

/// Resolve every bootstrap entry to IPv4 socket addresses, concurrently, under
/// ONE total budget. Entries that answer inside the budget are kept even when a
/// sibling entry times out — a PARTIAL set is a working DHT, an empty set is
/// the only outcome that disables the provider.
///
/// IPv6 results are dropped: `n0_mainline`'s `KrpcSocket` binds `0.0.0.0` and
/// its actor filters bootstrap down to `SocketAddrV4`, so a v6 bootstrap
/// address is dead weight that would only dilute the list.
async fn resolve_bootstrap(entries: &[String], budget: Duration) -> Vec<SocketAddr> {
    let deadline = tokio::time::Instant::now() + budget;
    let handles: Vec<_> = entries
        .iter()
        .cloned()
        .map(|e| {
            tokio::spawn(async move {
                // An `ip:port` literal parses without touching the resolver at
                // all — which is what lets a node with broken DNS still use a
                // literal `HIVE_DHT_BOOTSTRAP`.
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

/// Publish filter: home relay URLs plus GLOBALLY-ROUTABLE direct addresses only.
///
/// `AddrFilter::unfiltered()` (what the upstream example uses) would publish
/// every direct address iroh knows, including the node's RFC1918 VPC addresses —
/// see AGENTS.md on `peer_iroh.json` holding private 10.x/172.16/192.168 addrs.
/// Those are useless to anyone off the VPC and are free topology intelligence to
/// everyone else, so they are filtered here rather than published.
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

/// Conservative "would a stranger on the internet be able to dial this".
/// `IpAddr::is_global` is still unstable, so the classes are spelled out; when
/// in doubt the answer is NO — a wrongly-excluded address costs a relayed
/// connection, a wrongly-included one is published to the world forever.
fn is_publicly_routable(ip: IpAddr) -> bool {
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
                // 100.64.0.0/10 carrier-grade NAT
                || (o[0] == 100 && (64..128).contains(&o[1]))
                // 0.0.0.0/8 "this network"
                || o[0] == 0
                // 192.0.0.0/24 IETF protocol assignments
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                // 198.18.0.0/15 benchmarking
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)))
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fe80::/10 link-local
                || (seg[0] & 0xffc0) == 0xfe80
                // fc00::/7 unique-local
                || (seg[0] & 0xfe00) == 0xfc00
                // 2001:db8::/32 documentation
                || (seg[0] == 0x2001 && seg[1] == 0x0db8))
        }
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

/// Build the mainline-DHT address lookup from the environment, or `None`.
///
/// `secret` is the endpoint's OWN secret key — the DHT record is signed with it,
/// which is what makes the record's key equal to the node's `EndpointId`.
/// `None` (ephemeral endpoint identity, i.e. tests/dev with no key file) ⇒
/// resolve-only: there is nothing stable worth publishing.
///
/// Returns `None` — never an error — on every failure path, because this
/// provider is strictly additive. Boot must proceed exactly as it does today.
pub(crate) async fn lookup_from_env(secret: Option<&SecretKey>) -> Option<CountedDht> {
    if !env_flag("HIVE_DISCOVERY_DHT", true) {
        // Not a warning: an operator opting out is a normal configuration.
        tracing::info!("mainline DHT address lookup disabled (HIVE_DISCOVERY_DHT=0)");
        if let Ok(mut g) = STATS.skip_reason.lock() {
            *g = Some("disabled by HIVE_DISCOVERY_DHT".into());
        }
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
    // Strings, but IP LITERALS: `to_socket_addrs()` inside `build()` parses
    // these, it does not query a resolver. That is the whole point of the
    // pre-resolve above.
    let literals: Vec<String> = bootstrap.iter().map(|sa| sa.to_string()).collect();
    dht_builder.bootstrap(&literals);
    if let Some(p) = port {
        // Explicit port removes the crate's own 6881→ephemeral fallback, so a
        // conflict here surfaces as a build error and the provider is skipped
        // (never a failed endpoint bind). Unset is the better default: the
        // crate tries 6881 and falls back on its own.
        dht_builder.port(p);
    }
    // NEVER `server_mode()`. Client mode is outbound-UDP plus the NAT return
    // path, so no inbound firewall rule is needed — which is exactly what makes
    // this deployable on the CVM/GPU hosts that are inbound-22-only and are the
    // nodes most starved of address sources today. Server mode would also make
    // this node a routing/storage peer for the whole public DHT.

    let mut builder = DhtAddressLookup::builder()
        .dht_builder(dht_builder)
        .addr_filter(if publish_direct {
            relay_and_public_ip_filter()
        } else {
            AddrFilter::relay_only()
        });
    // The filter goes on the DHT builder, NOT `Endpoint::builder().addr_filter()`:
    // the endpoint-level filter is applied once at the `AddressLookupServices`
    // layer, so setting it there would also strip the Seer pkarr publisher's
    // addresses.
    match secret {
        Some(sk) if publish => builder = builder.secret_key(sk.clone()),
        _ => builder = builder.no_publish(),
    }

    let lookup = match builder.build() {
        Ok(l) => l,
        Err(e) => return skip(format!("DHT construction failed: {e}")),
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
    Some(CountedDht(lookup))
}

// ---------------------------------------------------------------------------
// Counting wrapper
// ---------------------------------------------------------------------------

/// [`DhtAddressLookup`] with publish/resolve counters attached.
///
/// A pure pass-through otherwise. It exists so `GET /v1/mesh/discovery` can
/// report what the DHT provider actually DID, not merely that it was configured.
#[derive(Debug)]
pub(crate) struct CountedDht(DhtAddressLookup);

impl AddressLookup for CountedDht {
    fn publish(&self, data: &EndpointData) {
        STATS.publishes_requested.fetch_add(1, Ordering::Relaxed);
        STATS
            .last_publish_request_ms
            .store(now_ms(), Ordering::Relaxed);
        self.0.publish(data);
    }

    fn resolve(&self, endpoint_id: EndpointId) -> Option<BoxStream<Result<LookupItem, LookupError>>> {
        STATS.resolves.fetch_add(1, Ordering::Relaxed);
        let inner = self.0.resolve(endpoint_id)?;
        Some(Box::pin(CountedResolve {
            inner,
            yielded_ok: false,
        }))
    }
}

/// Counts the OUTCOME of one resolve, which is only knowable once the stream
/// ends or is dropped — iroh drops a resolve stream as soon as it has an answer
/// it likes, so counting on `Drop` (rather than on stream end) is what makes a
/// short-circuited success show up as a hit instead of vanishing.
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

// ---------------------------------------------------------------------------
// Operator diagnostic
// ---------------------------------------------------------------------------

/// One successful `--dht-probe` answer.
#[derive(Debug, Clone)]
pub struct ProbeHit {
    pub endpoint_id: String,
    pub relay_urls: Vec<String>,
    pub direct_addrs: Vec<String>,
    pub elapsed_ms: u64,
    pub attempts: u32,
}

/// Resolve `endpoint_id` through the public DHT ONLY, from this host.
///
/// Deliberately does not bind an iroh `Endpoint`: a `DhtAddressLookup` is a
/// self-contained resolver, so the probe cannot collide with a running node's
/// QUIC port and answers exactly one question — "can this host find that id on
/// the public DHT, right now" — with nothing else in the path to explain a hit
/// away (no seeds, no pkarr relay, no cached `peer_iroh.json`).
///
/// Retries until `budget` expires: a cold routing table legitimately misses the
/// first few attempts and succeeds seconds later, so a single-shot probe
/// produces false negatives.
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
    // Always ephemeral: the probe must never contend for a running node's
    // HIVE_DHT_PORT, and it publishes nothing at all.
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
