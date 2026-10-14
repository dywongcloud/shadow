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
//! Every accepted connection is bounded the same way the mesh transport bounds an inbound accept
//! (see AGENTS.md): the preamble read, the TLS handshake, the FIRST CLIENT BYTE after it and the
//! backend connect each run under [`accept_deadline`], so a client that stalls any one of them
//! cannot pin the accepting task forever, and a [`PendingGuard`] cap (global,
//! `HIVE_DB_GATEWAY_PENDING_MAX`, plus per source address, `HIVE_DB_GATEWAY_PENDING_PER_IP_MAX`)
//! bounds how many connections may sit in that not-yet-established window at once. The first byte
//! is read BEFORE the backend is touched, so a client that handshakes TLS and then goes silent
//! never costs the database a connection slot either. The slot is released once that first byte is
//! forwarded, so the caps never limit an established session.
//!
//! An ESTABLISHED session has no idle bound: a long statement, a pooled idle connection and a
//! `LISTEN`/`SUBSCRIBE` one legitimately carry nothing for hours. A vanished peer is found by TCP
//! keepalive ([`enable_keepalive`]) instead, and the two directions are copied independently
//! (`copy_bidirectional`) so a client that half-closes after its request still receives the whole
//! reply and a TLS `close_notify`.
//!
//! Enabled ONLY when `HIVE_DB_DOMAIN` is set (`cloud.db_domain` non-empty).

use crate::state::CloudState;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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

/// Concurrent accepted connections (across both wires) still inside the preamble/handshake/first-byte
/// window: `HIVE_DB_GATEWAY_PENDING_MAX` overrides. This bounds only the attacker-cheap,
/// not-yet-proven-legitimate phase, never an established session (see [`PendingGuard::acquire`]).
fn pending_max() -> usize {
    std::env::var("HIVE_DB_GATEWAY_PENDING_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(512)
}

/// The same window, per SOURCE address ([`pending_key`]): one client must not be able to fill the
/// global cap alone and lock every other client out of the gateway.
/// `HIVE_DB_GATEWAY_PENDING_PER_IP_MAX` overrides (default 32; raise it when every client arrives
/// through one NAT or proxy address).
fn pending_per_ip_max() -> usize {
    std::env::var("HIVE_DB_GATEWAY_PENDING_PER_IP_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(32)
}

#[derive(Default)]
struct PendingCounts {
    total: usize,
    by_source: HashMap<IpAddr, usize>,
}

static PENDING: LazyLock<Mutex<PendingCounts>> = LazyLock::new(Default::default);

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

/// One reserved slot against the pending-connection caps; dropping it (past the first client byte,
/// on an early `?`, or on a timeout) always releases the slot, so every exit path is covered the
/// same way.
struct PendingGuard {
    key: IpAddr,
}

impl PendingGuard {
    /// Reserves a slot for a connection from `peer`, or `None` when [`pending_max`] or
    /// [`pending_per_ip_max`] is already reached.
    fn acquire(peer: IpAddr) -> Option<PendingGuard> {
        let key = pending_key(peer);
        let mut counts = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let from_source = counts.by_source.get(&key).copied().unwrap_or(0);
        if counts.total >= pending_max() || from_source >= pending_per_ip_max() {
            return None;
        }
        counts.total += 1;
        *counts.by_source.entry(key).or_insert(0) += 1;
        Some(PendingGuard { key })
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut counts = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        counts.total = counts.total.saturating_sub(1);
        match counts.by_source.get_mut(&self.key) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                counts.by_source.remove(&self.key);
            }
            None => {}
        }
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

/// Logs a refused accept at most once per 10 s: under a flood the refusals arrive by the thousand
/// per second and would otherwise fill the log (and the disk) faster than the cap protects it.
fn warn_refused_accept(addr: &str, wire: Wire, peer: std::net::SocketAddr) {
    static LAST_WARN_MS: AtomicU64 = AtomicU64::new(0);
    let now = hive_core::now_ms();
    let last = LAST_WARN_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 10_000
        && LAST_WARN_MS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(%addr, wire = ?wire, %peer, max = pending_max(), per_source_max = pending_per_ip_max(), "db-gateway: pending-connection cap reached, refusing accept (logged at most once per 10 s)");
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
            let Some(pending) = PendingGuard::acquire(peer.ip()) else {
                // `stream` drops here, closing the just-accepted socket immediately.
                warn_refused_accept(&addr, wire, peer);
                continue;
            };
            enable_keepalive(&stream);
            let cloud = cloud.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(cloud, stream, acceptor, wire, pending).await {
                    tracing::warn!(%peer, wire = ?wire, error = %e, "db-gateway: connection failed");
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
/// `pending` is held until the client's FIRST byte after the handshake has been forwarded (see the
/// module doc), then dropped before the unbounded copy: an established session is never counted
/// against the pending-connection caps, and the backend is not even connected for a client that
/// handshakes and then says nothing.
async fn handle(
    cloud: Arc<CloudState>,
    mut stream: TcpStream,
    acceptor: TlsAcceptor,
    wire: Wire,
    pending: PendingGuard,
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

    let backend_addr = format!("127.0.0.1:{local_port}");
    let mut backend = tokio::time::timeout(deadline, TcpStream::connect(&backend_addr))
        .await
        .map_err(|_| anyhow::anyhow!("backend {backend_addr} connect timed out"))?
        .map_err(|e| anyhow::anyhow!("backend {backend_addr} unreachable: {e}"))?;
    tokio::time::timeout(deadline, backend.write_all(&first[..n]))
        .await
        .map_err(|_| anyhow::anyhow!("backend {backend_addr} first write timed out"))?
        .map_err(|e| anyhow::anyhow!("backend {backend_addr} first write failed: {e}"))?;

    // Established: release the pending slot before the (potentially hours-long) copy. There is no
    // idle bound from here on; `enable_keepalive` finds a vanished peer, and each direction drains
    // on its own so a half-closing client still receives the full reply and a `close_notify`.
    drop(pending);
    match tokio::io::copy_bidirectional(&mut tls, &mut backend).await {
        Ok(_) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
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
