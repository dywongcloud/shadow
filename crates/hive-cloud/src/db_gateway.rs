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
//! Enabled ONLY when `HIVE_DB_DOMAIN` is set (`cloud.db_domain` non-empty).

use crate::state::CloudState;
use std::sync::Arc;
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
            let cloud = cloud.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(cloud, stream, acceptor, wire).await {
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
async fn handle(
    cloud: Arc<CloudState>,
    mut stream: TcpStream,
    acceptor: TlsAcceptor,
    wire: Wire,
) -> anyhow::Result<()> {
    if matches!(wire, Wire::Postgres) {
        let mut pre = [0u8; 8];
        stream.read_exact(&mut pre).await?;
        if !is_pg_ssl_request(&pre) {
            let _ = stream.write_all(b"N").await;
            anyhow::bail!("postgres client did not request TLS (needs sslmode=require)");
        }
        stream.write_all(b"S").await?;
    }

    let tls = acceptor.accept(stream).await?;
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
    let backend_addr = format!("127.0.0.1:{local_port}");
    let mut backend = TcpStream::connect(&backend_addr)
        .await
        .map_err(|e| anyhow::anyhow!("backend {backend_addr} unreachable: {e}"))?;

    let mut tls = tls;
    tokio::io::copy_bidirectional(&mut tls, &mut backend).await?;
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
