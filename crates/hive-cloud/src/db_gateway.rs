//! Per-tenant DATABASE gateway — the Neon/Upstash model.
//!
//! Gives every managed database an external, cross-protocol endpoint at
//! `<slug>.{db_domain}` (e.g. `db-ab12cd34.downstash.xyz`). A single wildcard
//! `*.{db_domain}` TLS cert (the shared [`crate::acme`] SNI resolver) fronts:
//!   * Postgres wire on `:5432`
//!   * Redis wire on `:6379`
//! Each listener TLS-TERMINATES the connection, reads the SNI server-name to
//! identify the tenant DB, and byte-splices the decrypted client stream to that
//! DB's local podman container (`127.0.0.1:<local_port>`).
//!
//! SECURITY (ZeroTrust): TLS is REQUIRED — credentials never cross the wire in
//! plaintext, and SNI is the only routing key. A connection whose SNI names no DB
//! (or a DB not hosted on this node) is closed. Per-DB DNS points `<slug>` at the
//! node actually running the container (see [`crate::vercel_dns`]), so the proxy
//! always finds it locally.
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

/// Spawn the DB-gateway listeners when the gateway is enabled. No-op otherwise.
pub fn spawn(cloud: Arc<CloudState>) {
    if cloud.db_domain.is_empty() {
        return;
    }
    let pg = std::env::var("HIVE_DB_PG_LISTEN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0.0.0.0:5432".into());
    let redis = std::env::var("HIVE_DB_REDIS_LISTEN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0.0.0.0:6379".into());
    spawn_listener(cloud.clone(), pg, Wire::Postgres);
    spawn_listener(cloud, redis, Wire::Redis);
}

fn spawn_listener(cloud: Arc<CloudState>, addr: String, wire: Wire) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(%addr, wire = ?wire, error = %e, "db-gateway: cannot bind listener");
                return;
            }
        };
        // Shared SNI resolver (serves the `*.{db_domain}` wildcard bundle), but a
        // config with NO ALPN — a Postgres client (libpq 17+ offers ALPN
        // `postgresql`) must not get a no_application_protocol alert.
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

async fn handle(
    cloud: Arc<CloudState>,
    mut stream: TcpStream,
    acceptor: TlsAcceptor,
    wire: Wire,
) -> anyhow::Result<()> {
    // Postgres SSL negotiation preamble (must precede the TLS handshake).
    if matches!(wire, Wire::Postgres) {
        let mut pre = [0u8; 8];
        stream.read_exact(&mut pre).await?;
        if !is_pg_ssl_request(&pre) {
            // Client tried a plaintext startup (sslmode=disable/prefer without TLS).
            // TLS is required for SNI routing — tell it SSL is unsupported so it
            // fails fast with a clear error rather than hanging.
            let _ = stream.write_all(b"N").await;
            anyhow::bail!("postgres client did not request TLS (needs sslmode=require)");
        }
        stream.write_all(b"S").await?; // proceed with TLS
    }

    // TLS handshake — the SNI resolver picks the wildcard cert by the ClientHello
    // server-name; we then read that same name to route.
    let tls = acceptor.accept(stream).await?;
    let sni = tls
        .get_ref()
        .1
        .server_name()
        .map(|s| s.to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("no SNI server-name — cannot route"))?;

    // Resolve the tenant DB by its external hostname.
    let db = cloud
        .databases
        .by_db_host(&sni)
        .ok_or_else(|| anyhow::anyhow!("no database for SNI {sni}"))?;

    // The container is published on THIS node's loopback at `local_port` (podman
    // `-p 127.0.0.1:<local_port>:5432|6379`). Missing = simulated (no container) or
    // the DB is hosted on another node (per-DB DNS should prevent that).
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

    // Splice: client (TLS-terminated) <-> backend (plaintext loopback). The
    // client's post-handshake bytes (PG StartupMessage / RESP AUTH) flow straight
    // through to the real engine.
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
