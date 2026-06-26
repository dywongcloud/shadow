//! Discovery (Plane B, node↔node) — the platform's own iroh discovery service
//! (self-hosted pkarr relay). NOTE: this is NOT "Seer". Seer (Plane A) is the
//! client-facing authoritative DNS in `dnsserver.rs` that hands browsers the public
//! IPs of healthy nodes. This server speaks pkarr to *iroh daemons*; they are
//! different protocols, consumers, and jobs — the names once collided, keep them apart.
//!
//! Replaces the dependency on n0's public pkarr/DNS for NodeId→address resolution.
//! A node PUBLISHES its signed address record here (via iroh's `PkarrPublisher`
//! pointed at a discovery URL) and RESOLVES peers here (via `PkarrResolver`), so a
//! fresh/wiped node can find the mesh entirely on platform-owned infra.
//!
//! Wire protocol = exactly what iroh's `PkarrRelayClient` speaks (a pkarr relay):
//!   * `PUT /<z32-pubkey>`  body = relay payload  → verify signature, store (newest wins)
//!   * `GET /<z32-pubkey>`                         → stored relay payload, or 404
//!
//! Records are self-verifying (ed25519-signed by the owning node), so no auth is
//! needed: `from_relay_payload` rejects any body whose signature doesn't match the
//! z32 key in the path. State is in-memory + TTL-pruned — small (fleet-sized) and
//! continuously republished by each node, so it needs no persistence or DHT.
//! Run on the stable PUBLIC nodes (the same ones used as bootstrap seeds); point
//! the fleet at them with `HIVE_DISCOVERY_DNS`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use parking_lot::RwLock;
use pkarr::SignedPacket;

/// Drop records not refreshed within this window. Nodes republish on iroh's pkarr
/// cadence (minutes), so this only reaps the genuinely departed; bounds memory.
const RECORD_TTL: Duration = Duration::from_secs(2 * 3600);

/// In-memory store of signed node records, keyed by z32 public key.
#[derive(Clone, Default)]
pub struct DiscoveryStore {
    inner: Arc<RwLock<HashMap<String, Entry>>>,
}

struct Entry {
    packet: SignedPacket,
    /// Wall-clock instant we last accepted this record (for TTL pruning).
    stored: std::time::Instant,
}

impl DiscoveryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of records held (for diagnostics / `/v1/overview`).
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Accept a verified record, keeping the one with the newest signed timestamp
    /// (pkarr's monotonic ordering — defeats replay of an older record).
    fn put(&self, z32: String, packet: SignedPacket) {
        let mut m = self.inner.write();
        if let Some(cur) = m.get(&z32) {
            if cur.packet.timestamp() >= packet.timestamp() {
                return;
            }
        }
        m.insert(z32, Entry { packet, stored: std::time::Instant::now() });
    }

    fn get(&self, z32: &str) -> Option<SignedPacket> {
        self.inner.read().get(z32).map(|e| e.packet.clone())
    }

    fn prune(&self) {
        let now = std::time::Instant::now();
        self.inner.write().retain(|_, e| now.duration_since(e.stored) < RECORD_TTL);
    }
}

/// Liveness + record count (for verifying publish/resolve flow through discovery).
async fn health(State(store): State<DiscoveryStore>) -> Response {
    (StatusCode::OK, format!("ok records={}", store.len())).into_response()
}

async fn get_record(State(store): State<DiscoveryStore>, Path(z32): Path<String>) -> Response {
    match store.get(&z32) {
        Some(pkt) => (StatusCode::OK, pkt.to_relay_payload()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn put_record(State(store): State<DiscoveryStore>, Path(z32): Path<String>, body: Bytes) -> StatusCode {
    // The z32 in the path IS the public key; `from_relay_payload` verifies the body's
    // signature against it, so a forged/altered record (or wrong key) is rejected.
    let Ok(pubkey) = pkarr::PublicKey::try_from(z32.as_str()) else {
        return StatusCode::BAD_REQUEST;
    };
    match SignedPacket::from_relay_payload(&pubkey, &body) {
        Ok(pkt) => {
            store.put(z32, pkt);
            StatusCode::OK
        }
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

/// Build the relay router. Shared by `serve` and tests so route syntax can't drift
/// (the `:z32` param is axum-0.7 syntax — `{z32}` would silently be a literal path).
fn router(store: DiscoveryStore) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/:z32", get(get_record).put(put_record))
        .with_state(store)
}

/// Serve the discovery pkarr relay on `addr` until error. Spawns a background pruner.
pub async fn serve(addr: SocketAddr, store: DiscoveryStore) -> std::io::Result<()> {
    {
        let store = store.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                store.prune();
            }
        });
    }
    let app = router(store);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "discovery (pkarr relay) listening");
    axum::serve(listener, app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_keeps_newest_and_prunes() {
        // Build two signed packets for the same key with increasing timestamps.
        let kp = pkarr::Keypair::random();
        let z32 = kp.public_key().to_string();
        let older = SignedPacket::builder().sign(&kp).unwrap();
        // A later packet (newer timestamp) for the same key.
        let newer = SignedPacket::builder().sign(&kp).unwrap();
        let (older, newer) = if older.timestamp() <= newer.timestamp() {
            (older, newer)
        } else {
            (newer, older)
        };

        let store = DiscoveryStore::new();
        store.put(z32.clone(), newer.clone());
        store.put(z32.clone(), older.clone()); // older must NOT overwrite newer
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(&z32).unwrap().timestamp(), newer.timestamp(), "newest wins");

        // A record stored long ago is pruned.
        store.inner.write().get_mut(&z32).unwrap().stored =
            std::time::Instant::now() - RECORD_TTL - Duration::from_secs(1);
        store.prune();
        assert_eq!(store.len(), 0, "expired record pruned");
    }

    /// End-to-end over real HTTP: PUT a signed record, GET it back, reject a bad key.
    /// Exercises the `:z32` route param — a literal-path regression would 404 here.
    #[tokio::test]
    async fn relay_round_trip_over_http() {
        let store = DiscoveryStore::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(store.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let kp = pkarr::Keypair::random();
        let z32 = kp.public_key().to_string();
        let pkt = SignedPacket::builder().sign(&kp).unwrap();
        let base = format!("http://{addr}");
        let cli = reqwest::Client::new();

        let put = cli
            .put(format!("{base}/{z32}"))
            .body(pkt.to_relay_payload().to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), 200, "PUT signed record accepted");
        assert_eq!(store.len(), 1, "record stored");

        let got = cli.get(format!("{base}/{z32}")).send().await.unwrap();
        assert_eq!(got.status(), 200, "GET returns stored record");
        let body = got.bytes().await.unwrap();
        let parsed = SignedPacket::from_relay_payload(&kp.public_key(), &body).unwrap();
        assert_eq!(parsed.timestamp(), pkt.timestamp(), "round-tripped record matches");

        let bad = cli
            .put(format!("{base}/notavalidz32key"))
            .body(vec![1u8, 2, 3])
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400, "invalid key rejected");
    }
}
