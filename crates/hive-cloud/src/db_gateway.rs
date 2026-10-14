//! Per-tenant DATABASE gateway, the Neon/Upstash model.
//!
//! Gives every managed database an external, cross-protocol endpoint at `<slug>.{db_domain}`. A
//! single wildcard `*.{db_domain}` TLS cert (the shared [`crate::acme`] SNI resolver) fronts two
//! listeners whose addresses come from `HIVE_DB_PG_LISTEN`/`HIVE_DB_REDIS_LISTEN` (default
//! `:5432`/`:6379`): the Postgres wire and the Redis wire. Each listener TLS-TERMINATES the
//! connection, reads the SNI server name to identify the tenant DB, and byte-splices the decrypted
//! client stream to that DB's local podman container (`127.0.0.1:<local_port>`).
//!
//! SECURITY: TLS is REQUIRED (credentials never cross the wire in plaintext) and SNI is the only
//! routing key. A connection whose SNI names no DB, or a DB not hosted on this node, is closed.
//! Per-DB DNS points `<slug>` at the node actually running the container (see
//! [`crate::vercel_dns`]), so the proxy always finds it locally.
//!
//! A connection is UNPROVEN until its database ACCEPTS AUTHENTICATION ([`AuthProof`]: a Postgres
//! `AuthenticationOk`, a Redis `+OK` to `AUTH` or the reply to `HELLO`; any reply at all is not enough,
//! because `-NOAUTH` and `+RESET` are answered to a client that never authenticates). Every unproven
//! stage is bounded the same way the mesh transport bounds an inbound accept (see AGENTS.md): the
//! preamble read, the TLS handshake, the FIRST CLIENT BYTE after it and the backend connect each run
//! under [`accept_deadline`], and authentication under [`auth_deadline`] counted from the first byte
//! forwarded. The first byte is read BEFORE the backend is touched, so a client that handshakes TLS and
//! goes silent never costs the database a connection slot.
//!
//! Every connection holds a [`PendingGuard`] from accept until it is proven. The budget is global
//! (`HIVE_DB_GATEWAY_PENDING_MAX`, default 4096) and per source address (`..._PENDING_PER_IP_MAX`,
//! default 1024: a NAT'd fleet of 800 clients restarting at once arrives from ONE address and must not
//! be reset; a burst beyond it has its excess refused at once and reconnects). A budget that a handful of
//! addresses could fill with half-open connections would starve everyone else, so when it is full a new
//! connection EVICTS the oldest connection that has sat unproven for `HIVE_DB_GATEWAY_PENDING_STUCK_MS`
//! (default 2 s: a legitimate connection is proven within milliseconds, so the oldest stuck one is a
//! flooder's) and is refused only when nothing is stuck, i.e. during a genuine burst.
//!
//! A PROVEN session has no idle bound: a long statement, a pooled idle connection and a
//! `LISTEN`/`SUBSCRIBE` one legitimately carry nothing for hours. A vanished peer is found by TCP
//! keepalive ([`enable_keepalive`]). The sessions one source address may hold at a backend, proven or
//! not, are capped ([`SessionGuard`], `HIVE_DB_GATEWAY_SESSIONS_PER_IP_MAX`) before the backend is
//! touched, per SOURCE address because the abuser is an address: a per-database cap would let one
//! address lock a tenant out of its own database, a global one would let it lock out every tenant, and
//! NAT'd pools stay well under the default. Failures are logged through a [`Throttle`], one line per
//! 10 s with the count it suppressed, so a flood cannot fill the log.
//!
//! The two directions are copied independently ([`splice`]), so a client that half-closes after its
//! request still receives the whole reply and a TLS `close_notify`; once either direction has ended
//! the other has [`linger`] of INACTIVITY to finish, so a client that stays open after the engine
//! closed the session (Redis `QUIT`) cannot pin the gateway's two sockets.
//!
//! Enabled ONLY when `HIVE_DB_DOMAIN` is set (`cloud.db_domain` non-empty).

use crate::state::CloudState;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Notify};
use tokio::time::Instant;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

#[derive(Clone, Copy, Debug)]
enum Wire {
    /// Postgres: the client sends an `SSLRequest` in plaintext, the server replies
    /// `S`, THEN the TLS handshake begins. We must handle that preamble first.
    Postgres,
    /// Redis (`rediss://`): TLS begins immediately, no preamble.
    Redis,
}

/// The listen address of the Postgres-wire proxy: `HIVE_DB_PG_LISTEN`, default `0.0.0.0:5432`.
///
/// Overridable because the default belongs to whichever hive-cloud instance bound it first: on a
/// host running BOTH a fleet `hive-node` and a second platform instance, the second one's bind
/// fails, and it failed SILENTLY (a log line and a `return`), so the gateway looked configured
/// while serving nothing.
pub fn pg_listen_addr() -> String {
    listen_addr("HIVE_DB_PG_LISTEN", "0.0.0.0:5432")
}

/// The listen address for the Redis-wire proxy. `HIVE_DB_REDIS_LISTEN`, default
/// `0.0.0.0:6379`. See [`pg_listen_addr`].
pub fn redis_listen_addr() -> String {
    listen_addr("HIVE_DB_REDIS_LISTEN", "0.0.0.0:6379")
}

fn listen_addr(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The port a client must connect to for the Postgres wire: the port the listener actually holds,
/// not a hardcoded 5432. Published DSNs are built from it, so moving the listener moves the URL
/// with it.
pub fn published_pg_port() -> u16 {
    port_of(&pg_listen_addr(), 5432)
}

/// [`published_pg_port`] for the Redis wire.
pub fn published_redis_port() -> u16 {
    port_of(&redis_listen_addr(), 6379)
}

fn port_of(addr: &str, default: u16) -> u16 {
    addr.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// Longest time to wait for the preamble read, the TLS handshake, the first client byte after it or
/// the backend connect: a client that stalls any ONE of them must not pin the accepting task (and
/// its fds) past this. `HIVE_DB_GATEWAY_ACCEPT_DEADLINE_MS` overrides.
fn accept_deadline() -> Duration {
    Duration::from_millis(
        std::env::var("HIVE_DB_GATEWAY_ACCEPT_DEADLINE_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(15_000),
    )
}

/// Longest a session may take, from its first forwarded client bytes, to get the backend to ACCEPT
/// AUTHENTICATION (`HIVE_DB_GATEWAY_AUTH_DEADLINE_MS`, default 30 s, never under 250 ms). A legitimate
/// client authenticates within a round trip or two; one that sends a byte and goes silent, or that is
/// only ever answered `-NOAUTH`, never does, and without this bound it would hold the gateway's two
/// sockets and a database connection slot for as long as it liked.
fn auth_deadline() -> Duration {
    Duration::from_millis(
        std::env::var("HIVE_DB_GATEWAY_AUTH_DEADLINE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(30_000)
            .max(250),
    )
}

/// How long a connection must have sat unproven before a full pending budget may evict it to admit a
/// newcomer (`HIVE_DB_GATEWAY_PENDING_STUCK_MS`, default 2 s). A legitimate connection is proven in
/// milliseconds, so past this it is almost certainly a half-open flooder's.
fn stuck_after() -> Duration {
    Duration::from_millis(
        std::env::var("HIVE_DB_GATEWAY_PENDING_STUCK_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(2_000),
    )
}

/// How long the surviving direction of a session may sit SILENT once the other direction has ended
/// (`HIVE_DB_GATEWAY_LINGER_SECS`, default 60 s, never under 1 s). The clock restarts on every read, so
/// a large reply still draining to a client that half-closed is never cut; a client that stays open
/// after the engine closed the session (Redis `QUIT`) is, instead of pinning both sockets forever.
fn linger() -> Duration {
    Duration::from_secs(
        std::env::var("HIVE_DB_GATEWAY_LINGER_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(60)
            .max(1),
    )
}

/// Concurrent accepted connections (across both wires) not yet proven by a backend that accepted their
/// authentication: `HIVE_DB_GATEWAY_PENDING_MAX` overrides (default 4096). This bounds only the
/// attacker-cheap unproven phase, never a proven session (see [`Accounting::proven`]), and a full
/// budget evicts its oldest stuck connection rather than refuse a newcomer ([`PendingGuard::acquire`]).
fn pending_max() -> usize {
    std::env::var("HIVE_DB_GATEWAY_PENDING_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(4096)
}

/// The same window, per SOURCE address ([`pending_key`]): one address must not be able to hold the
/// whole budget. `HIVE_DB_GATEWAY_PENDING_PER_IP_MAX` overrides (default 1024). Every legitimate
/// connection leaves this window in milliseconds, so the default only has to cover a burst: a NAT'd
/// fleet reconnecting after a restart arrives from ONE address (800 simultaneous clients are admitted
/// with room to spare; the excess of a burst past this bound is refused at once and reconnects).
fn pending_per_ip_max() -> usize {
    std::env::var("HIVE_DB_GATEWAY_PENDING_PER_IP_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1024)
}

/// Sessions one SOURCE address may hold at a backend at once, proven or still awaiting their first
/// reply: `HIVE_DB_GATEWAY_SESSIONS_PER_IP_MAX` overrides (default 4096). A backstop for the gateway's
/// file descriptors and a database's connection slots, sized far above any legitimate NAT'd pool; it
/// never bounds how long a session may idle.
fn sessions_per_ip_max() -> usize {
    std::env::var("HIVE_DB_GATEWAY_SESSIONS_PER_IP_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(4096)
}

/// The key a source address is counted under: an IPv4 address as is (an IPv4-mapped IPv6 one in its
/// IPv4 form), an IPv6 one as its /64 — a single host routinely owns a whole /64, so counting exact
/// addresses would let it rotate past the per-source cap.
fn pending_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut octets = v6.octets();
                octets[8..].fill(0);
                IpAddr::V6(octets.into())
            }
        },
    }
}

/// One connection's place in the pending budget.
struct PendingEntry {
    source: IpAddr,
    since: Instant,
    /// Wakes the connection's task when it is evicted to make room.
    evict: Arc<Notify>,
}

#[derive(Default)]
struct PendingTable {
    next_id: u64,
    /// Oldest first: ids rise with arrival, so the first entry is the oldest.
    entries: BTreeMap<u64, PendingEntry>,
    /// `(source, id)` of every entry, so a source's OLDEST entry is one range lookup, never a scan.
    by_arrival: BTreeSet<(IpAddr, u64)>,
    by_source: HashMap<IpAddr, usize>,
}

impl PendingTable {
    /// Removes entry `id` (the guard's drop, or an eviction), keeping the indexes and counts.
    fn remove(&mut self, id: u64) -> Option<PendingEntry> {
        let entry = self.entries.remove(&id)?;
        self.by_arrival.remove(&(entry.source, id));
        match self.by_source.get_mut(&entry.source) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                self.by_source.remove(&entry.source);
            }
            None => {}
        }
        Some(entry)
    }

    /// Evicts the OLDEST entry (of `source` when given) if it has been pending at least `stuck`: the
    /// oldest is the first to qualify, so when it does not, nothing does. `false` when none qualifies.
    fn evict_oldest_stuck(&mut self, source: Option<IpAddr>, stuck: Duration) -> bool {
        let oldest = match source {
            Some(source) => self
                .by_arrival
                .range((source, 0)..=(source, u64::MAX))
                .next()
                .map(|&(_, id)| id),
            None => self.entries.keys().next().copied(),
        };
        let Some(id) = oldest else {
            return false;
        };
        let pending_for = self
            .entries
            .get(&id)
            .map(|e| Instant::now().saturating_duration_since(e.since));
        if pending_for.is_none_or(|age| age < stuck) {
            return false;
        }
        if let Some(evicted) = self.remove(id) {
            evicted.evict.notify_one();
        }
        true
    }
}

static PENDING: LazyLock<Mutex<PendingTable>> = LazyLock::new(Default::default);

/// One place in the pending budget, held from accept until the connection is proven; dropping it (once
/// the backend accepted authentication, on an early `?`, on a timeout or after an eviction) always
/// releases the place, so every exit path is covered the same way.
struct PendingGuard {
    id: u64,
}

impl PendingGuard {
    /// Takes a place for a connection from `peer`, with the signal that wakes the connection if it is
    /// evicted later. A source at [`pending_per_ip_max`] makes room from ITS OWN stuck connections, a
    /// full [`pending_max`] from the oldest stuck connection of anyone ([`stuck_after`]); `None` only
    /// when nothing is stuck, i.e. during a burst of connections that are all still progressing.
    fn acquire(peer: IpAddr) -> Option<(PendingGuard, Arc<Notify>)> {
        let key = pending_key(peer);
        let stuck = stuck_after();
        let mut table = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if table.by_source.get(&key).copied().unwrap_or(0) >= pending_per_ip_max()
            && !table.evict_oldest_stuck(Some(key), stuck)
        {
            return None;
        }
        if table.entries.len() >= pending_max() && !table.evict_oldest_stuck(None, stuck) {
            return None;
        }
        let evict = Arc::new(Notify::new());
        let id = table.next_id;
        table.next_id += 1;
        table.entries.insert(
            id,
            PendingEntry {
                source: key,
                since: Instant::now(),
                evict: evict.clone(),
            },
        );
        table.by_arrival.insert((key, id));
        *table.by_source.entry(key).or_insert(0) += 1;
        Some((PendingGuard { id }, evict))
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.id);
    }
}

static SESSIONS: LazyLock<Mutex<HashMap<IpAddr, usize>>> = LazyLock::new(Default::default);

/// One session counted against its source address ([`sessions_per_ip_max`]); dropping it ends the
/// count on every exit path.
struct SessionGuard {
    key: IpAddr,
}

impl SessionGuard {
    /// Counts a session for `peer`, or `None` when that source already holds the cap.
    fn acquire(peer: IpAddr) -> Option<SessionGuard> {
        let key = pending_key(peer);
        let mut by_source = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        if by_source.get(&key).copied().unwrap_or(0) >= sessions_per_ip_max() {
            return None;
        }
        *by_source.entry(key).or_insert(0) += 1;
        Some(SessionGuard { key })
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut by_source = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        match by_source.get_mut(&self.key) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                by_source.remove(&self.key);
            }
            None => {}
        }
    }
}

/// What a connection holds against the caps: the pending slot until the backend first answers, and
/// a session slot from the moment it reaches the backend until it ends.
struct Accounting {
    pending: Option<PendingGuard>,
    session: Option<SessionGuard>,
    peer: IpAddr,
}

impl Accounting {
    /// Counts this connection as a session of its source address BEFORE the backend is touched, so
    /// a source at [`sessions_per_ip_max`] is turned away at the cost of a TLS handshake, never a
    /// database connection. `false` when the source is at its cap.
    fn admit(&mut self) -> bool {
        match SessionGuard::acquire(self.peer) {
            Some(guard) => {
                self.session = Some(guard);
                true
            }
            None => false,
        }
    }

    /// The backend answered: the connection is proven, so its pending slot is released (the session
    /// slot stays until it ends).
    fn proven(&mut self) {
        self.pending = None;
    }
}

/// TCP keepalive on the client socket. An established session has no idle bound (a long statement,
/// a pooled idle connection, `LISTEN`/`SUBSCRIBE` legitimately carry nothing for hours), so a peer
/// that vanished without a FIN is found by the kernel's probes instead of pinning two fds forever.
fn enable_keepalive(stream: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(15));
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive) {
        tracing::debug!(error = %e, "db-gateway: could not enable TCP keepalive");
    }
}

/// A log line allowed once per 10 s that reports how many it suppressed since the last: under a flood
/// the lines would otherwise arrive by the thousand per second and fill the log (and the disk) faster
/// than any cap protects it.
struct Throttle {
    last_ms: AtomicU64,
    suppressed: AtomicU64,
}

impl Throttle {
    const fn new() -> Throttle {
        Throttle {
            last_ms: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    /// `Some(n)` when this call may log (`n` calls were suppressed since the last line that did),
    /// `None` when it was counted instead.
    fn allow(&self) -> Option<u64> {
        let now = hive_core::now_ms();
        let last = self.last_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 10_000
            && self
                .last_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            Some(self.suppressed.swap(0, Ordering::Relaxed))
        } else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

static REFUSED_ACCEPTS: Throttle = Throttle::new();
static SESSION_CAPS: Throttle = Throttle::new();
static FAILED_CONNECTIONS: Throttle = Throttle::new();

/// Logs a refused accept (the pending budget is full and nothing in it is stuck), throttled.
fn warn_refused_accept(addr: &str, wire: Wire, peer: std::net::SocketAddr) {
    if let Some(suppressed) = REFUSED_ACCEPTS.allow() {
        tracing::warn!(%addr, wire = ?wire, %peer, max = pending_max(), per_source_max = pending_per_ip_max(), suppressed, "db-gateway: pending-connection budget full and nothing in it is stuck, refusing accept (logged at most once per 10 s)");
    }
}

/// [`warn_refused_accept`] for a source at its session cap, throttled.
fn warn_session_cap(peer: IpAddr) {
    if let Some(suppressed) = SESSION_CAPS.allow() {
        tracing::warn!(%peer, per_source_max = sessions_per_ip_max(), suppressed, "db-gateway: source holds its cap of established sessions, closing a new one (logged at most once per 10 s)");
    }
}

/// Logs a connection that failed before it was proven (a TLS handshake that never finished, a client
/// that sent nothing, an unknown SNI), throttled: every one of these is the client's doing and a
/// half-open flood produces them by the thousand. Every failure is still visible at `debug`.
fn warn_connection_failed(peer: std::net::SocketAddr, wire: Wire, error: &anyhow::Error) {
    tracing::debug!(%peer, wire = ?wire, %error, "db-gateway: connection failed");
    if let Some(suppressed) = FAILED_CONNECTIONS.allow() {
        tracing::warn!(%peer, wire = ?wire, %error, suppressed, "db-gateway: connection failed (logged at most once per 10 s; `debug` shows every one)");
    }
}

/// Spawn the DB-gateway listeners when the gateway is enabled. No-op otherwise.
pub fn spawn(cloud: Arc<CloudState>) {
    if cloud.db_domain.is_empty() {
        return;
    }
    spawn_listener(cloud.clone(), pg_listen_addr(), Wire::Postgres);
    spawn_listener(cloud, redis_listen_addr(), Wire::Redis);
}

/// Binds one wire's listener and serves it. A gateway that cannot bind is INVISIBLE (no crash, no
/// failing request, databases that simply never connect), so a bind failure opens a level-triggered
/// incident (`db-gateway:bind-failed:<addr>`, re-asserted every start, cleared on a successful
/// bind) that names the env var that resolves it. The TLS config is the shared SNI resolver (it
/// serves the `*.{db_domain}` wildcard bundle) but with NO ALPN: a Postgres client (libpq 17+
/// offers ALPN `postgresql`) must not get a `no_application_protocol` alert.
fn spawn_listener(cloud: Arc<CloudState>, addr: String, wire: Wire) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                let var = match wire {
                    Wire::Postgres => "HIVE_DB_PG_LISTEN",
                    Wire::Redis => "HIVE_DB_REDIS_LISTEN",
                };
                let condition = format!("db-gateway:bind-failed:{}", addr.replace(':', "."));
                let _ = cloud.incidents.open(crate::incidents::OpenReq {
                    condition: condition.clone(),
                    ttl_ms: 30 * 60 * 1000,
                    title: format!("DB gateway listener dead ({wire:?} on {addr})"),
                    severity: crate::incidents::Severity::Major,
                    affected: vec!["databases".into(), "db-gateway".into()],
                    message: format!(
                        "The {wire:?} DB-gateway listener could not bind {addr}: {e}. \
                         Every `<slug>.{}` endpoint on this node is unreachable. \
                         Set {var} to a free port (another hive-cloud instance on this \
                         host already holds the default) and restart.",
                        cloud.db_domain
                    ),
                });
                tracing::error!(%addr, wire = ?wire, error = %e, "db-gateway: cannot bind listener");
                return;
            }
        };
        cloud.incidents.clear(
            &format!("db-gateway:bind-failed:{}", addr.replace(':', ".")),
            "Listener bound — the DB gateway is serving.",
        );
        let acceptor = TlsAcceptor::from(crate::acme::db_server_config());
        tracing::info!(%addr, wire = ?wire, domain = %cloud.db_domain, "db-gateway listener up");
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "db-gateway: accept error");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let Some((pending, evicted)) = PendingGuard::acquire(peer.ip()) else {
                // `stream` drops here, closing the just-accepted socket immediately.
                warn_refused_accept(&addr, wire, peer);
                continue;
            };
            enable_keepalive(&stream);
            let cloud = cloud.clone();
            let acceptor = acceptor.clone();
            let accounting = Accounting {
                pending: Some(pending),
                session: None,
                peer: peer.ip(),
            };
            tokio::spawn(async move {
                // Evicted (the budget was full and this connection was the oldest stuck one): the
                // handler future is dropped, which closes both sockets.
                let outcome = tokio::select! {
                    outcome = handle(cloud, stream, acceptor, wire, accounting) => outcome,
                    () = evicted.notified() => {
                        tracing::debug!(%peer, wire = ?wire, "db-gateway: connection evicted to admit a newcomer");
                        Ok(())
                    }
                };
                if let Err(e) = outcome {
                    warn_connection_failed(peer, wire, &e);
                }
            });
        }
    });
}

/// The postgres `SSLRequest` message: length=8, request code = 80877103.
const PG_SSL_REQUEST_CODE: i32 = 80_877_103;

/// Is this 8-byte preamble a well-formed postgres `SSLRequest` (Int32 length=8,
/// Int32 code=80877103, big-endian)? Pure so the wire parsing is unit-tested.
fn is_pg_ssl_request(pre: &[u8; 8]) -> bool {
    let len = i32::from_be_bytes([pre[0], pre[1], pre[2], pre[3]]);
    let code = i32::from_be_bytes([pre[4], pre[5], pre[6], pre[7]]);
    len == 8 && code == PG_SSL_REQUEST_CODE
}

/// Serves one client connection. Postgres first: the client sends an `SSLRequest` in plaintext, the
/// server answers `S`, THEN the TLS handshake begins; a client that starts in plaintext
/// (`sslmode=disable`, or `prefer` without TLS) gets `N` and is closed, so it fails fast with a
/// clear error rather than hanging, because TLS is required for SNI routing. After the handshake
/// the SNI server name (the one the resolver picked the wildcard cert by) is the routing key: it
/// resolves the tenant database by its external hostname, and the connection is spliced,
/// TLS-terminated client to plaintext loopback backend (`127.0.0.1:<local_port>`, the port podman
/// publishes on THIS node), so the client's post-handshake bytes (PG StartupMessage, RESP AUTH)
/// reach the engine untouched. A database without a `local_port` is simulated (no container) or
/// hosted on another node, which per-DB DNS should prevent.
///
/// `accounting` carries the pending slot until the backend ACCEPTS AUTHENTICATION (see the module
/// doc) and the source's session slot from the moment the backend is reached: the backend is not even
/// connected for a client that handshakes and says nothing or whose source is at its session cap, and
/// a connection that never authenticates is cut at [`auth_deadline`] instead of holding its slots
/// forever.
async fn handle(
    cloud: Arc<CloudState>,
    mut stream: TcpStream,
    acceptor: TlsAcceptor,
    wire: Wire,
    mut accounting: Accounting,
) -> anyhow::Result<()> {
    let deadline = accept_deadline();
    if matches!(wire, Wire::Postgres) {
        let mut pre = [0u8; 8];
        tokio::time::timeout(deadline, stream.read_exact(&mut pre))
            .await
            .map_err(|_| anyhow::anyhow!("postgres preamble read timed out"))?
            .map_err(|e| anyhow::anyhow!("postgres preamble read failed: {e}"))?;
        if !is_pg_ssl_request(&pre) {
            let _ = tokio::time::timeout(deadline, stream.write_all(b"N")).await;
            anyhow::bail!("postgres client did not request TLS (needs sslmode=require)");
        }
        tokio::time::timeout(deadline, stream.write_all(b"S"))
            .await
            .map_err(|_| anyhow::anyhow!("postgres TLS-ack write timed out"))?
            .map_err(|e| anyhow::anyhow!("postgres TLS-ack write failed: {e}"))?;
    }

    let mut tls = tokio::time::timeout(deadline, acceptor.accept(stream))
        .await
        .map_err(|_| anyhow::anyhow!("TLS handshake timed out"))?
        .map_err(|e| anyhow::anyhow!("TLS handshake failed: {e}"))?;
    let sni = tls
        .get_ref()
        .1
        .server_name()
        .map(|s| s.to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("no SNI server-name — cannot route"))?;

    let db = cloud
        .databases
        .by_db_host(&sni)
        .ok_or_else(|| anyhow::anyhow!("no database for SNI {sni}"))?;

    let local_port = db
        .connection
        .get("local_port")
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("db {} not locally hosted / not live (no local_port)", db.id)
        })?;
    let proof = AuthProof::new(
        wire,
        db.connection.get("password").is_some_and(|p| !p.is_empty()),
    );
    // The client must speak before the backend is touched: a client that handshakes with a valid SNI
    // and then goes silent would otherwise hold a database connection slot (Postgres
    // `max_connections`) while costing the attacker nothing. Both wires are client-first (the PG
    // StartupMessage, the first RESP command), so a legitimate client sends this at once.
    let mut first = vec![0u8; 16 * 1024];
    let n = tokio::time::timeout(deadline, tls.read(&mut first))
        .await
        .map_err(|_| anyhow::anyhow!("no first byte within the deadline after the TLS handshake"))?
        .map_err(|e| anyhow::anyhow!("first client read failed: {e}"))?;
    if n == 0 {
        return Ok(());
    }

    if !accounting.admit() {
        warn_session_cap(accounting.peer);
        return Ok(());
    }
    let backend_addr = format!("127.0.0.1:{local_port}");
    let mut backend = tokio::time::timeout(deadline, TcpStream::connect(&backend_addr))
        .await
        .map_err(|_| anyhow::anyhow!("backend {backend_addr} connect timed out"))?
        .map_err(|e| anyhow::anyhow!("backend {backend_addr} unreachable: {e}"))?;
    tokio::time::timeout(deadline, backend.write_all(&first[..n]))
        .await
        .map_err(|_| anyhow::anyhow!("backend {backend_addr} first write timed out"))?
        .map_err(|e| anyhow::anyhow!("backend {backend_addr} first write failed: {e}"))?;

    splice(tls, backend, accounting, proof).await
}

/// Postgres `AuthenticationOk`: message type `R`, length 8, code 0. A NUL-free text field (an error
/// message echoing the client's own input) can never contain it.
const PG_AUTH_OK: [u8; 9] = *b"R\0\0\0\x08\0\0\0\0";

/// Most backend bytes [`AuthProof`] keeps while it looks for its proof.
const PROOF_SCAN_MAX: usize = 64 * 1024;

/// Watches the backend's replies while a session is unproven and says when they show the backend
/// ACCEPTED authentication. A bare reply is no proof: a Redis that requires a password answers
/// `-NOAUTH` to everything but `AUTH`/`HELLO`/`QUIT`/`RESET`, and `+RESET` (and a `QUIT`'s `+OK`, then
/// the engine closes) come back to a client that never authenticated; a Postgres backend sends an
/// authentication REQUEST before it accepts anything.
struct AuthProof {
    wire: Wire,
    /// Redis only: whether the database has a password. Without one there is nothing to authenticate
    /// and the first non-error reply proves a working session.
    password_required: bool,
    /// What is not yet decided: the last, still incomplete reply line (Redis) or the last bytes that
    /// could begin a pattern split across two reads (Postgres).
    carry: Vec<u8>,
}

impl AuthProof {
    fn new(wire: Wire, password_required: bool) -> AuthProof {
        AuthProof {
            wire,
            password_required,
            carry: Vec::new(),
        }
    }

    /// Feeds the next backend-to-client bytes; `true` once they show authentication was accepted.
    fn accepted(&mut self, chunk: &[u8]) -> bool {
        self.carry.extend_from_slice(chunk);
        if self.carry.len() > PROOF_SCAN_MAX {
            let cut = self.carry.len() - PROOF_SCAN_MAX;
            self.carry.drain(..cut);
        }
        match self.wire {
            Wire::Postgres => {
                let found = self
                    .carry
                    .windows(PG_AUTH_OK.len())
                    .any(|window| window == PG_AUTH_OK);
                let keep = PG_AUTH_OK.len() - 1;
                if self.carry.len() > keep {
                    let cut = self.carry.len() - keep;
                    self.carry.drain(..cut);
                }
                found
            }
            Wire::Redis => {
                let mut start = 0;
                let mut found = false;
                while let Some(end) = self.carry[start..].iter().position(|&b| b == b'\n') {
                    if redis_reply_proves_auth(
                        &self.carry[start..start + end],
                        self.password_required,
                    ) {
                        found = true;
                        break;
                    }
                    start += end + 1;
                }
                self.carry.drain(..start);
                found
            }
        }
    }
}

/// Whether one reply line (the first line of a reply) shows Redis accepted authentication: with a
/// password, only what `AUTH` and `HELLO` answer (`+OK`, a RESP3 map, a RESP2 array); without one, any
/// non-error reply. Errors (`-`, `!`) and the `+RESET` an unauthenticated client can obtain prove nothing.
fn redis_reply_proves_auth(line: &[u8], password_required: bool) -> bool {
    match line.first() {
        None | Some(b'-') | Some(b'!') => false,
        Some(_) if line.starts_with(b"+RESET") => false,
        Some(first) if password_required => {
            line.starts_with(b"+OK") || matches!(first, b'%' | b'*')
        }
        Some(_) => true,
    }
}

/// Where a session stands, shared by its two directions.
#[derive(Clone, Copy)]
enum Phase {
    /// The first client bytes went to the backend and it has not accepted authentication: it must by
    /// `deadline`.
    Authenticating { deadline: Instant },
    /// The backend accepted authentication: the session is proven and nothing bounds its idleness any
    /// more.
    Open,
    /// One direction has ended: the other has [`linger`] of inactivity to finish.
    Draining,
}

/// How one direction of a session ended.
enum Exit {
    /// Its source reached end of stream (and the sink was shut down): the other direction drains.
    Eof,
    /// The wait the session's phase allows ran out: the whole session is cut.
    TimedOut,
}

const PUMP_BUF: usize = 16 * 1024;

/// One read of `from`, bounded by what the session's phase allows. `reply_bound` marks the
/// backend-to-client direction: while the session is authenticating that read has the auth
/// deadline; once either direction has ended the surviving read has [`linger`] of inactivity (the
/// clock restarts on every call, so a reply that is still draining is never cut); otherwise a read
/// waits as long as it must. `None` means the bound ran out. Cancel-safe: a phase change drops the
/// pending `read` (which loses no bytes) and re-arms it under the new bound.
async fn read_phased<R: AsyncRead + Unpin>(
    from: &mut R,
    buf: &mut [u8],
    phase: &watch::Sender<Phase>,
    reply_bound: bool,
) -> std::io::Result<Option<usize>> {
    let mut changes = phase.subscribe();
    loop {
        let deadline = match *changes.borrow_and_update() {
            Phase::Authenticating { deadline } if reply_bound => Some(deadline),
            Phase::Draining => Some(Instant::now() + linger()),
            _ => None,
        };
        let bound = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            read = from.read(buf) => return read.map(Some),
            () = bound => return Ok(None),
            _ = changes.changed() => {}
        }
    }
}

/// Client to backend. At the client's end of stream the backend's write side is shut down (it sees
/// EOF after everything the client sent) and the session starts draining.
async fn pump_up(
    mut client: ReadHalf<TlsStream<TcpStream>>,
    mut backend: OwnedWriteHalf,
    phase: &watch::Sender<Phase>,
) -> std::io::Result<Exit> {
    let mut buf = vec![0u8; PUMP_BUF];
    loop {
        match read_phased(&mut client, &mut buf, phase, false).await? {
            None => return Ok(Exit::TimedOut),
            Some(0) => {
                let _ = backend.shutdown().await;
                phase.send_modify(|p| *p = Phase::Draining);
                return Ok(Exit::Eof);
            }
            Some(n) => backend.write_all(&buf[..n]).await?,
        }
    }
}

/// Backend to client. Until the backend's replies show it ACCEPTED authentication ([`AuthProof`]) the
/// session is unproven and bounded by the auth deadline; the proof releases the pending slot and opens
/// the phase. At the backend's end of stream the client gets its TLS `close_notify` and the session
/// starts draining.
async fn pump_down(
    mut backend: OwnedReadHalf,
    mut client: WriteHalf<TlsStream<TcpStream>>,
    phase: &watch::Sender<Phase>,
    accounting: &mut Accounting,
    proof: &mut AuthProof,
) -> std::io::Result<Exit> {
    let mut buf = vec![0u8; PUMP_BUF];
    let mut proven = false;
    loop {
        match read_phased(&mut backend, &mut buf, phase, !proven).await? {
            None => return Ok(Exit::TimedOut),
            Some(0) => {
                let _ = client.shutdown().await;
                phase.send_modify(|p| *p = Phase::Draining);
                return Ok(Exit::Eof);
            }
            Some(n) => {
                if !proven && proof.accepted(&buf[..n]) {
                    proven = true;
                    accounting.proven();
                    phase.send_modify(|p| {
                        if matches!(p, Phase::Authenticating { .. }) {
                            *p = Phase::Open;
                        }
                    });
                }
                client.write_all(&buf[..n]).await?;
                client.flush().await?;
            }
        }
    }
}

/// Copies an established session's two directions until it ends. Each direction runs on its own, so
/// a client that half-closes after its request still receives the whole reply and a `close_notify`.
/// The session is bounded only where it is unproven: the backend must accept authentication within
/// [`auth_deadline`], and once either direction has ended the other may sit silent for at most
/// [`linger`]. A proven session has no idle bound (a pooled connection, a long statement and a
/// `LISTEN` legitimately carry nothing for hours); `enable_keepalive` finds a vanished peer.
async fn splice(
    client: TlsStream<TcpStream>,
    backend: TcpStream,
    mut accounting: Accounting,
    mut proof: AuthProof,
) -> anyhow::Result<()> {
    let peer = accounting.peer;
    let (client_read, client_write) = tokio::io::split(client);
    let (backend_read, backend_write) = backend.into_split();
    let (phase, _observer) = watch::channel(Phase::Authenticating {
        deadline: Instant::now() + auth_deadline(),
    });
    let up = pump_up(client_read, backend_write, &phase);
    let down = pump_down(
        backend_read,
        client_write,
        &phase,
        &mut accounting,
        &mut proof,
    );
    tokio::pin!(up, down);
    let (mut up_ended, mut down_ended) = (false, false);
    while !(up_ended && down_ended) {
        let ended = tokio::select! {
            ended = &mut up, if !up_ended => {
                up_ended = true;
                ended
            }
            ended = &mut down, if !down_ended => {
                down_ended = true;
                ended
            }
        };
        match ended {
            Ok(Exit::Eof) => {}
            Ok(Exit::TimedOut) => {
                tracing::debug!(%peer, "db-gateway: session cut: authentication was not accepted in time, or a half-closed direction stayed silent");
                return Ok(());
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_postgres_ssl_request() {
        // Int32(8) + Int32(80877103), big-endian = the exact SSLRequest on the wire.
        let mut pre = [0u8; 8];
        pre[0..4].copy_from_slice(&8i32.to_be_bytes());
        pre[4..8].copy_from_slice(&PG_SSL_REQUEST_CODE.to_be_bytes());
        assert!(is_pg_ssl_request(&pre));
        // A plaintext StartupMessage (protocol 3.0 = 196608) must NOT be mistaken
        // for an SSLRequest — those clients get 'N' (TLS required).
        let mut startup = [0u8; 8];
        startup[0..4].copy_from_slice(&296i32.to_be_bytes());
        startup[4..8].copy_from_slice(&196608i32.to_be_bytes());
        assert!(!is_pg_ssl_request(&startup));
        // Wrong length, right code → rejected.
        let mut bad = pre;
        bad[3] = 9;
        assert!(!is_pg_ssl_request(&bad));
    }
}
