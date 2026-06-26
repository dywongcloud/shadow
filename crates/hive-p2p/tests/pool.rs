//! PeerPool integration tests — prove the cross-node path reuses ONE iroh QUIC
//! connection per peer and opens a NEW stream per request.
//!
//! Modeled on `src/bin/p2p_demo.rs`: bind two endpoints, serve tunnels on B over a
//! local HTTP echo "function", and drive requests from A through the pool.
//!
//! These bind real iroh endpoints; if binding fails (e.g. fully offline CI) the
//! tests skip rather than fail.

use std::sync::Arc;
use std::time::Duration;

/// Serialize these real-iroh integration tests: 8 endpoint pairs connecting at once
/// saturate connect/holepunch setup and blow the per-request timeouts (flaky under
/// concurrent `cargo test`). Each test grabs this process-global lock so they run
/// one-at-a-time regardless of `--test-threads`. Poison ignored (a panicking test
/// must not wedge the rest).
static NET_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn net_serial() -> std::sync::MutexGuard<'static, ()> {
    NET_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Minimal HTTP echo server (the "function" node B serves over the tunnel).
async fn spawn_function() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = match l.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let mut acc = Vec::new();
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => acc.extend_from_slice(&buf[..n]),
                    }
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let path = String::from_utf8_lossy(&acc)
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let body = format!("{{\"served_over\":\"iroh-p2p\",\"path\":\"{path}\"}}");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    addr
}

/// Bind node B (serving tunnels to the echo fn) + node A (the dialer + pool).
/// Returns `(pool, node_b_id, addr_b_json)`, or `None` if iroh can't bind.
async fn setup() -> Option<(Arc<hive_p2p::PeerPool>, String, String)> {
    let function = spawn_function().await;
    let ep_b = match hive_p2p::bind().await {
        Ok(ep) => ep,
        Err(_) => return None,
    };
    let node_b_id = ep_b.id().to_string();
    let addr_b_json = hive_p2p::addr_json(&ep_b)?;
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, function, 100, None, None));

    let ep_a = hive_p2p::bind().await.ok()?;
    let pool = hive_p2p::PeerPool::new(ep_a);
    Some((pool, node_b_id, addr_b_json))
}

async fn one(pool: &hive_p2p::PeerPool, node_id: &str, addr_json: &str, path: &str) -> (u16, String) {
    let tr = tokio::time::timeout(
        Duration::from_secs(20),
        pool.request(node_id, addr_json, "GET", path, &[], b""),
    )
    .await
    .expect("request timed out")
    .expect("request failed");
    (tr.status, String::from_utf8_lossy(&tr.body).to_string())
}

/// Two SEQUENTIAL requests reuse the trunk: opened == 1, reused == 1.
#[tokio::test]
async fn sequential_requests_reuse_one_connection() {
    let _serial = net_serial();
    let Some((pool, id, addr)) = setup().await else {
        eprintln!("skipping: iroh could not bind");
        return;
    };

    let (s1, b1) = one(&pool, &id, &addr, "/a").await;
    assert_eq!(s1, 200);
    assert!(b1.contains("iroh-p2p"), "body: {b1}");

    let (s2, b2) = one(&pool, &id, &addr, "/b").await;
    assert_eq!(s2, 200);
    assert!(b2.contains("iroh-p2p"), "body: {b2}");

    let (opened, reused) = pool.stats();
    assert_eq!(opened, 1, "second request must reuse the trunk, not re-dial");
    assert_eq!(reused, 1, "exactly one reuse");
}

/// 16 CONCURRENT requests ride one connection (16 independent streams): opened == 1.
/// The trunk is warmed first so the count is deterministic — the pool intentionally
/// does NOT single-flight first-contact, so concurrent cold dials could double-dial.
#[tokio::test]
async fn concurrent_requests_share_one_connection() {
    let _serial = net_serial();
    let Some((pool, id, addr)) = setup().await else {
        eprintln!("skipping: iroh could not bind");
        return;
    };

    // Warm the trunk (opened = 1).
    let (s, _) = one(&pool, &id, &addr, "/warm").await;
    assert_eq!(s, 200);

    // 16 concurrent requests — each opens its own bi stream over the SAME connection.
    let mut handles = Vec::new();
    for i in 0..16 {
        let pool = pool.clone();
        let id = id.clone();
        let addr = addr.clone();
        handles.push(tokio::spawn(async move { one(&pool, &id, &addr, &format!("/c/{i}")).await }));
    }
    let mut ok = 0;
    for h in handles {
        let (status, body) = h.await.expect("task panicked");
        assert_eq!(status, 200);
        assert!(body.contains("iroh-p2p"));
        ok += 1;
    }
    assert_eq!(ok, 16, "all 16 concurrent requests succeed");

    let (opened, reused) = pool.stats();
    assert_eq!(opened, 1, "16 concurrent streams must share ONE connection");
    assert_eq!(reused, 16, "all 16 reused the warm trunk (no lock serialization stalls)");
}

/// Killing the trunk forces an in-call re-dial on the next request: opened == 2.
#[tokio::test]
async fn killed_trunk_redials() {
    let _serial = net_serial();
    let Some((pool, id, addr)) = setup().await else {
        eprintln!("skipping: iroh could not bind");
        return;
    };

    let (s1, _) = one(&pool, &id, &addr, "/first").await;
    assert_eq!(s1, 200);
    assert_eq!(pool.stats().0, 1, "first request opens the trunk");

    // Deterministically kill the cached trunk.
    pool.close_peer(&id).await;

    // Next request must re-dial a fresh connection.
    let (s2, b2) = one(&pool, &id, &addr, "/second").await;
    assert_eq!(s2, 200);
    assert!(b2.contains("iroh-p2p"), "body: {b2}");
    assert_eq!(pool.stats().0, 2, "a killed trunk re-dials (opened == 2)");
}

/// Stronger re-dial proof: sever the LIVE connection but leave it CACHED, so the
/// re-dial is driven by the pool DETECTING the dead trunk (close_reason() /
/// open_bi failure) — not by an empty cache. `opened` can only advance via a real
/// `ep.connect`, and the 200 + body prove a genuine round-trip over the NEW
/// connection, so the counter can't be faked.
#[tokio::test]
async fn severed_cached_trunk_is_detected_and_redialed() {
    let _serial = net_serial();
    let Some((pool, id, addr)) = setup().await else {
        eprintln!("skipping: iroh could not bind");
        return;
    };

    let (s1, _) = one(&pool, &id, &addr, "/first").await;
    assert_eq!(s1, 200);
    let (opened0, reused0) = pool.stats();
    assert_eq!(opened0, 1, "first request opens the trunk");
    assert_eq!(reused0, 0);

    // Close the underlying QUIC connection but KEEP it in the pool's map. The
    // cached handle now reports closed (shared Arc state), proving real severance.
    let was_cached_and_closed = pool.sever_peer(&id).await;
    assert!(was_cached_and_closed, "a live trunk was cached and is now severed");

    // The next request finds a cached-but-dead trunk → must re-dial in-call.
    let (s2, b2) = one(&pool, &id, &addr, "/after-sever").await;
    assert_eq!(s2, 200);
    assert!(b2.contains("iroh-p2p"), "round-trip over the NEW connection: {b2}");
    assert_eq!(pool.stats().0, 2, "severed-but-cached trunk is detected dead and re-dialed (opened == 2)");
}

/// #20 peer trust: a peer whose iroh identity is NOT in the trust set is rejected —
/// its requests fail (the server drops the connection before serving any stream).
#[tokio::test]
async fn untrusted_peer_is_rejected() {
    let _serial = net_serial();
    let function = spawn_function().await;
    let ep_b = match hive_p2p::bind().await { Ok(e) => e, Err(_) => { eprintln!("skip: no iroh"); return; } };
    let addr_b = hive_p2p::addr_json(&ep_b).unwrap();
    let id_b = ep_b.id().to_string();
    // Empty trust set → B trusts no one.
    let trust: hive_p2p::TrustSet = Arc::new(std::sync::RwLock::new(std::collections::HashSet::new()));
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, function, 100, Some(trust), None));

    let ep_a = hive_p2p::bind().await.unwrap();
    let pool = hive_p2p::PeerPool::new(ep_a);
    let res = tokio::time::timeout(
        Duration::from_secs(8),
        pool.request(&id_b, &addr_b, "GET", "/x", &[], b""),
    ).await;
    let ok = matches!(res, Ok(Ok(_)));
    assert!(!ok, "untrusted peer must NOT be served");
}

/// #20 peer trust: a peer whose identity IS in the trust set is admitted and served
/// normally — enforcement doesn't break legitimate fleet traffic.
#[tokio::test]
async fn trusted_peer_is_admitted() {
    let _serial = net_serial();
    let function = spawn_function().await;
    let ep_b = match hive_p2p::bind().await { Ok(e) => e, Err(_) => { eprintln!("skip: no iroh"); return; } };
    let addr_b = hive_p2p::addr_json(&ep_b).unwrap();
    let id_b = ep_b.id().to_string();
    // Bind A first so we can put its identity in B's trust set.
    let ep_a = hive_p2p::bind().await.unwrap();
    let id_a = ep_a.id().to_string();
    let trust: hive_p2p::TrustSet = Arc::new(std::sync::RwLock::new(std::collections::HashSet::new()));
    trust.write().unwrap().insert(id_a);
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, function, 100, Some(trust), None));

    let pool = hive_p2p::PeerPool::new(ep_a);
    let (status, body) = one(&pool, &id_b, &addr_b, "/ok").await;
    assert_eq!(status, 200);
    assert!(body.contains("iroh-p2p"), "trusted peer served: {body}");
}

/// #23 relay cost accounting: after a request, the trunk is classified into EXACTLY
/// one bucket (relay xor direct) and its bytes are accounted. We don't assert WHICH
/// bucket — iroh's N0 preset connects via a relay first and holepunches to direct
/// asynchronously, so the path at measurement time is timing-dependent; the
/// accounting correctness (mutually-exclusive buckets, bytes counted) is what's
/// deterministic and is what #23 needs.
#[tokio::test]
async fn relay_stats_classifies_and_counts_bytes() {
    let _serial = net_serial();
    let Some((pool, id, addr)) = setup().await else {
        eprintln!("skipping: iroh could not bind");
        return;
    };
    let (s, _) = one(&pool, &id, &addr, "/a").await;
    assert_eq!(s, 200);
    let rs = pool.relay_stats().await;
    assert_eq!(rs.relayed_conns + rs.direct_conns, 1, "exactly one trunk, classified into one bucket");
    let relayed = rs.relayed_bytes_tx + rs.relayed_bytes_rx;
    let direct = rs.direct_bytes_tx + rs.direct_bytes_rx;
    assert!(relayed + direct > 0, "trunk bytes accounted (relay or direct): relayed={relayed} direct={direct}");
    // Bytes land in the SAME bucket the connection was classified into.
    if rs.direct_conns == 1 {
        assert_eq!(relayed, 0, "direct-classified trunk has no relayed bytes");
    } else {
        assert_eq!(direct, 0, "relay-classified trunk has no direct bytes");
    }
}

/// Gossip-over-iroh round-trip: a STREAM_GOSSIP request reaches the handler with
/// the right method/path/body and the framed response comes back intact.
#[tokio::test]
async fn gossip_request_round_trips_over_iroh() {
    let _serial = net_serial();
    let ep_b = match hive_p2p::bind().await { Ok(e) => e, Err(_) => { eprintln!("skip: no iroh"); return; } };
    let addr_b = hive_p2p::addr_json(&ep_b).unwrap();
    let id_b = ep_b.id().to_string();
    // Handler echoes "<method>:<path>:<body>" so we can assert all three round-trip.
    let handler: hive_p2p::GossipHandler = Arc::new(|method, path, body| {
        Box::pin(async move {
            format!("{method}:{path}:{}", String::from_utf8_lossy(&body)).into_bytes()
        })
    });
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, "127.0.0.1:1".into(), 100, None, Some(handler)));

    let ep_a = hive_p2p::bind().await.unwrap();
    let pool = hive_p2p::PeerPool::new(ep_a);
    let resp = tokio::time::timeout(
        // 20s to match the other real-iroh tests' budget — the shorter 10s flaked when
        // the whole suite runs concurrently (8 endpoint pairs contend on connect setup).
        Duration::from_secs(20),
        pool.gossip_request(&id_b, &addr_b, hive_p2p::GOSSIP_POST, "/v1/nodes", b"hello"),
    ).await.expect("timed out").expect("gossip failed");
    assert_eq!(String::from_utf8_lossy(&resp), "1:/v1/nodes:hello");

    // A second gossip request reuses the trunk (no re-dial).
    let r2 = pool.gossip_request(&id_b, &addr_b, hive_p2p::GOSSIP_GET, "/v1/serve-hosts", b"").await.unwrap();
    assert_eq!(String::from_utf8_lossy(&r2), "0:/v1/serve-hosts:");
}
