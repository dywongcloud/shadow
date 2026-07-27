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
    // Deliberately NOT `assert_eq!(opened0, 1)`. Under real machine load the
    // first request can legitimately dial more than once (timeout + retry), and
    // this test then failed intermittently on a healthy code path — witnessed
    // once during a run whose only source change was in an unrelated crate, and
    // green 16/16 on an immediate re-run. What this test actually exists to
    // prove is the DELTA below: a severed-but-cached trunk is detected dead and
    // re-dialed exactly once more. Anchoring on the delta keeps that guarantee
    // while removing the load-sensitivity.
    assert!(opened0 >= 1, "first request opens at least one trunk (got {opened0})");
    assert_eq!(reused0, 0, "nothing to reuse yet");

    // Close the underlying QUIC connection but KEEP it in the pool's map. The
    // cached handle now reports closed (shared Arc state), proving real severance.
    let was_cached_and_closed = pool.sever_peer(&id).await;
    assert!(was_cached_and_closed, "a live trunk was cached and is now severed");

    // The next request finds a cached-but-dead trunk → must re-dial in-call.
    let (s2, b2) = one(&pool, &id, &addr, "/after-sever").await;
    assert_eq!(s2, 200);
    assert!(b2.contains("iroh-p2p"), "round-trip over the NEW connection: {b2}");
    assert_eq!(
        pool.stats().0,
        opened0 + 1,
        "severed-but-cached trunk is detected dead and re-dialed exactly once more (opened {} -> {})",
        opened0,
        pool.stats().0
    );
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
    let handler: hive_p2p::GossipHandler = Arc::new(|method, path, body, _signer| {
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

/// SIGNED gossip round-trip (web3 trustlessness): with `HIVE_GOSSIP_SIGN=1` the
/// request rides STREAM_GOSSIP_SIGNED, the receiver VERIFIES the ed25519 signature
/// (bound to the QUIC remote identity), and the handler sees the verified signer id.
#[tokio::test]
async fn signed_gossip_verifies_and_passes_signer() {
    let _serial = net_serial();
    std::env::set_var("HIVE_GOSSIP_SIGN", "1");
    std::env::set_var("HIVE_GOSSIP_VERIFY", "enforce");
    let ep_b = match hive_p2p::bind().await { Ok(e) => e, Err(_) => { eprintln!("skip: no iroh"); return; } };
    let addr_b = hive_p2p::addr_json(&ep_b).unwrap();
    let id_b = ep_b.id().to_string();
    // Handler echoes the VERIFIED signer id so the test can assert verification ran.
    let handler: hive_p2p::GossipHandler = Arc::new(|_m, _p, _b, signer| {
        Box::pin(async move { signer.unwrap_or_else(|| "UNSIGNED".into()).into_bytes() })
    });
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, "127.0.0.1:1".into(), 100, None, Some(handler)));

    let ep_a = hive_p2p::bind().await.unwrap();
    let id_a = ep_a.id().to_string();
    let pool = hive_p2p::PeerPool::new(ep_a);
    let resp = tokio::time::timeout(
        Duration::from_secs(20),
        pool.gossip_request(&id_b, &addr_b, hive_p2p::GOSSIP_POST, "/v1/nodes", b"signed"),
    ).await.expect("timed out").expect("signed gossip failed");
    // Even in ENFORCE mode the request is served, and the verified signer is the
    // sender's cryptographic identity (not just any string). This equality is the
    // end-to-end proof: the handler only receives Some(signer) after a VALID
    // signature bound to the QUIC remote identity.
    assert_eq!(String::from_utf8_lossy(&resp), id_a, "handler must see the verified signer id");
    // Counters are process-global and other suite tests gossip concurrently, so
    // only assert the monotonic signal (at least our request verified).
    let (ok, ..) = hive_p2p::verify_stats();
    assert!(ok >= 1, "signed_ok counted");
    std::env::remove_var("HIVE_GOSSIP_SIGN");
    std::env::remove_var("HIVE_GOSSIP_VERIFY");
}

/// Signed gossip with a trust set configured that DOES include the sender's
/// identity: verification must still succeed and the handler must still see
/// the signer (regression test for the added signer-membership check inside
/// `serve_gossip` — it must not regress the already-trusted case). NOTE: an
/// UNTRUSTED signer can't be independently exercised through this public
/// entry point today — `serve_tunnels` already drops the QUIC connection for
/// an untrusted peer at admission time (before any stream/signature is even
/// read), and `verify_gossip`'s own transport-binding invariant requires
/// `signer == remote_id`, so by construction the signer inside `serve_gossip`
/// is always the SAME identity already checked at connection admission. The
/// membership check added there is real, correct defense-in-depth (protects
/// against any future code path that reuses a connection/stream without
/// re-checking trust), but today it is provably redundant with the
/// connection-level check whenever a trust set is configured — closing the
/// DEFAULT-OFF gap (trust unconfigured) is tracked separately as an
/// infra-level decision, not a code change.
#[tokio::test]
async fn signed_gossip_with_trust_set_including_sender_still_verifies() {
    let _serial = net_serial();
    std::env::set_var("HIVE_GOSSIP_SIGN", "1");
    std::env::set_var("HIVE_GOSSIP_VERIFY", "enforce");
    let ep_b = match hive_p2p::bind().await { Ok(e) => e, Err(_) => { eprintln!("skip: no iroh"); return; } };
    let addr_b = hive_p2p::addr_json(&ep_b).unwrap();
    let id_b = ep_b.id().to_string();
    let handler: hive_p2p::GossipHandler = Arc::new(|_m, _p, _b, signer| {
        Box::pin(async move { signer.unwrap_or_else(|| "UNSIGNED".into()).into_bytes() })
    });

    let ep_a = hive_p2p::bind().await.unwrap();
    let id_a = ep_a.id().to_string();

    // Trust set includes the sender (id_a) — the legitimate, "hardened and
    // correctly configured" case.
    let trust: hive_p2p::TrustSet = Arc::new(std::sync::RwLock::new(std::collections::HashSet::from([id_a.clone()])));
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, "127.0.0.1:1".into(), 100, Some(trust), Some(handler)));

    let pool = hive_p2p::PeerPool::new(ep_a);
    let resp = tokio::time::timeout(
        Duration::from_secs(20),
        pool.gossip_request(&id_b, &addr_b, hive_p2p::GOSSIP_POST, "/v1/nodes", b"signed"),
    ).await.expect("timed out").expect("signed gossip from a trusted peer must succeed");
    assert_eq!(String::from_utf8_lossy(&resp), id_a, "handler must still see the verified signer id when trusted");

    std::env::remove_var("HIVE_GOSSIP_SIGN");
    std::env::remove_var("HIVE_GOSSIP_VERIFY");
}

/// Signature primitives: a tampered body/method/timestamp fails verification, the
/// signer must match the transport identity, and stale timestamps are rejected.
#[tokio::test]
async fn gossip_signature_rejects_tamper_replay_and_mismatch() {
    let sk = iroh::SecretKey::generate();
    let id = sk.public().to_string();
    let now: u64 = 1_700_000_000_000;
    let trailer = hive_p2p::sign_gossip(&sk, hive_p2p::GOSSIP_POST, "/v1/x", b"body", now);
    // Valid.
    assert_eq!(
        hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/x", b"body", &id, now).unwrap(),
        id
    );
    // Tampered body / path / method.
    assert!(hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/x", b"evil", &id, now).is_err());
    assert!(hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/y", b"body", &id, now).is_err());
    assert!(hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_GET, "/v1/x", b"body", &id, now).is_err());
    // Replay outside the freshness window.
    assert!(hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/x", b"body", &id, now + 10 * 60 * 1000).is_err());
    // Signer/transport mismatch: valid signature but from a DIFFERENT connection identity.
    let other = iroh::SecretKey::generate().public().to_string();
    assert!(hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/x", b"body", &other, now).is_err());
}

/// Eager trunking: `warm()` (what the background trunk-warmer calls) pre-establishes
/// the trunk, so a later request REUSES it — no cold dial on the request path.
#[tokio::test]
async fn warm_pre_establishes_trunk_reused_by_request() {
    let _serial = net_serial();
    // Generous connect budget: a single `warm()` is one connect attempt, and a real
    // holepunch can exceed the 5s default when the serialized suite has saturated
    // connect setup (in production the warmer just retries next tick — here we want
    // the one warm to be deterministic).
    let _g = EnvGuard::set("HIVE_P2P_CONNECT_MS", "20000");
    let Some((pool, id, addr)) = setup().await else {
        eprintln!("skipping: iroh could not bind");
        return;
    };
    // Proactively warm — establishes the trunk off the request path.
    let ok = tokio::time::timeout(Duration::from_secs(25), pool.warm(&id, &addr))
        .await
        .expect("warm must not hang");
    assert!(ok, "trunk warms");
    assert_eq!(pool.stats().0, 1, "warm opened exactly one trunk");
    assert_eq!(pool.trunk_count().await, 1, "one cached trunk after warm");

    // A subsequent request REUSES the pre-warmed trunk — no new dial.
    let (s, b) = one(&pool, &id, &addr, "/warmed").await;
    assert_eq!(s, 200);
    assert!(b.contains("iroh-p2p"), "body: {b}");
    let (opened, reused) = pool.stats();
    assert_eq!(opened, 1, "request reused the pre-warmed trunk (no cold dial)");
    assert_eq!(reused, 1, "exactly one reuse of the warm trunk");
}

// ---- #H4: bounded iroh data plane -------------------------------------------------

/// Restore an env var on drop so a per-test budget override can't leak into the
/// other (serialized) real-iroh tests and shrink their connect budget.
struct EnvGuard(&'static str, Option<String>);
impl EnvGuard {
    fn set(k: &'static str, v: &str) -> Self {
        let old = std::env::var(k).ok();
        std::env::set_var(k, v);
        EnvGuard(k, old)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

/// Bind node B serving `function` over tunnels + node A with a pool. Like `setup`
/// but lets the caller supply a custom (e.g. slow) function endpoint.
async fn setup_serving(function: String) -> Option<(Arc<hive_p2p::PeerPool>, String, String)> {
    let ep_b = hive_p2p::bind().await.ok()?;
    let id = ep_b.id().to_string();
    let addr = hive_p2p::addr_json(&ep_b)?;
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, function, 100, None, None));
    let ep_a = hive_p2p::bind().await.ok()?;
    Some((hive_p2p::PeerPool::new(ep_a), id, addr))
}

/// A slow HTTP "function": send the head, then `chunks` SSE chunks each followed by
/// `gap`, then either hang forever (`then_hang`) or close (clean EOF).
async fn spawn_slow_function(chunks: usize, gap: Duration, then_hang: bool) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = match l.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            tokio::spawn(async move {
                // Drain request headers.
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
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                if s.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let _ = s.flush().await;
                for i in 0..chunks {
                    if s.write_all(format!("data: {i}\n\n").as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = s.flush().await;
                    tokio::time::sleep(gap).await;
                }
                if then_hang {
                    std::future::pending::<()>().await;
                }
                // else: drop the socket → EOF → clean end of body.
            });
        }
    });
    addr
}

/// Blackhole connect: a peer that is bound but NEVER accepts → `connect`/`open`
/// can't complete. `request()` must return a `DeadPeerTimeout` within the connect
/// budget (× retry), NOT hang, and the timeout must be counted.
#[tokio::test]
async fn blackhole_connect_times_out_and_is_marked_dead() {
    let _serial = net_serial();
    let _g1 = EnvGuard::set("HIVE_P2P_CONNECT_MS", "500");
    let _g2 = EnvGuard::set("HIVE_P2P_OPEN_MS", "500");
    // `acquire`'s discovery-fallback retry (`dial_fresh`, relay-selection-algorithm)
    // adds a SEPARATE budget on top of `HIVE_P2P_CONNECT_MS` for every connect
    // timeout/error against the cached hint — bound it too, or this test's total
    // wall time is (connect budget + the fallback's own 4s DEFAULT) × retry,
    // blowing past the assertions below even though the peer is still correctly
    // detected dead.
    let _g3 = EnvGuard::set("HIVE_P2P_DISCOVERY_MS", "500");
    // Bind B but DO NOT serve/accept — held alive so its addr stays advertised.
    let ep_b = match hive_p2p::bind().await {
        Ok(e) => e,
        Err(_) => { eprintln!("skip: no iroh"); return; }
    };
    let id = ep_b.id().to_string();
    let addr = hive_p2p::addr_json(&ep_b).unwrap();
    let ep_a = hive_p2p::bind().await.unwrap();
    let pool = hive_p2p::PeerPool::new(ep_a);

    let t0 = std::time::Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(8),
        pool.request(&id, &addr, "GET", "/x", &[], b""),
    )
    .await
    .expect("request must NOT hang — the connect budget must bound it");
    let elapsed = t0.elapsed();

    let err = res.err().expect("blackhole connect must error, not succeed");
    assert!(
        err.downcast_ref::<hive_p2p::DeadPeerTimeout>().is_some(),
        "a pre-send (connect/open) timeout is the dead-peer signal: {err}"
    );
    assert!(elapsed < Duration::from_secs(4), "bounded by connect+open budget (×retry), took {elapsed:?}");
    let ts = pool.relay_stats().await.timeouts;
    assert!(
        ts.iter().any(|t| t.node_id == id && (t.phase == "connect" || t.phase == "open")),
        "a connect/open timeout must be counted: {ts:?}"
    );
    drop(ep_b);
}

/// Accept-but-silent: the owner accepts the connection + stream but never writes a
/// response. The first-byte timeout (post-send) must fire within budget, return an
/// error, and — crucially — NOT redial (opened == 1, reused == 0).
#[tokio::test]
async fn accept_but_silent_first_byte_times_out_without_retry() {
    let _serial = net_serial();
    let _g = EnvGuard::set("HIVE_P2P_FIRSTBYTE_MS", "700");
    let ep_b = match hive_p2p::bind().await {
        Ok(e) => e,
        Err(_) => { eprintln!("skip: no iroh"); return; }
    };
    let id = ep_b.id().to_string();
    let addr = hive_p2p::addr_json(&ep_b).unwrap();
    tokio::spawn(hive_p2p::serve_silent(ep_b));

    let ep_a = hive_p2p::bind().await.unwrap();
    let pool = hive_p2p::PeerPool::new(ep_a);

    // Warm the trunk first (pay the slow real holepunch connect HERE, outside the
    // measured window) so `elapsed` reflects only the first-byte phase. `open_raw`
    // returns after the stream opens — it doesn't await a response — so it succeeds
    // even against a silent owner.
    let _warm = tokio::time::timeout(Duration::from_secs(12), pool.open_raw(&id, &addr))
        .await
        .expect("warm must not hang")
        .expect("trunk warms");
    assert_eq!(pool.stats().0, 1, "exactly one connection opened (the warm)");

    let t0 = std::time::Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(12),
        pool.request(&id, &addr, "GET", "/x", &[], b""),
    )
    .await
    .expect("request must NOT hang — the first-byte budget must bound it");
    let elapsed = t0.elapsed();

    let err = res.err().expect("silent owner must error");
    assert!(
        err.downcast_ref::<hive_p2p::PostSendTimeout>().is_some(),
        "first-byte is post-send (no retry): {err}"
    );
    // Trunk was warm → measured window is just the firstbyte budget (~700ms).
    assert!(elapsed < Duration::from_secs(3), "bounded by the firstbyte budget, took {elapsed:?}");
    // No redial from the post-send failure: still exactly one connection ever opened.
    let (opened, _reused) = pool.stats();
    assert_eq!(opened, 1, "post-send timeout must NOT redial (opened stays 1)");
    let ts = pool.relay_stats().await.timeouts;
    assert!(ts.iter().any(|t| t.phase == "firstbyte"), "firstbyte timeout counted: {ts:?}");
}

/// Idle mid-stream: head + one chunk, then the owner goes silent. The inter-chunk
/// idle timeout must fire (terminate the stream + flag it), within the idle budget.
#[tokio::test]
async fn idle_timeout_fires_when_stream_goes_silent() {
    let _serial = net_serial();
    let _g = EnvGuard::set("HIVE_P2P_IDLE_MS", "700");
    let func = spawn_slow_function(1, Duration::from_millis(0), true).await;
    let Some((pool, id, addr)) = setup_serving(func).await else {
        eprintln!("skip: no iroh");
        return;
    };

    let mut ts = tokio::time::timeout(
        Duration::from_secs(15),
        pool.request_stream(&id, &addr, "GET", "/sse", &[], b""),
    )
    .await
    .expect("stream setup must not hang")
    .expect("stream opens");
    assert_eq!(ts.status, 200);

    let first = tokio::time::timeout(Duration::from_secs(3), ts.recv()).await.expect("first chunk soon");
    assert!(first.is_some(), "the one emitted chunk arrives");

    // The peer is now silent; the next recv must self-terminate on the idle budget.
    let t0 = std::time::Instant::now();
    let next = tokio::time::timeout(Duration::from_secs(5), ts.recv())
        .await
        .expect("recv must self-terminate on idle, not hang");
    assert!(next.is_none(), "idle timeout ends the stream (None)");
    assert!(ts.timed_out(), "ended via idle timeout, not a clean EOF");
    assert!(t0.elapsed() < Duration::from_secs(3), "fired within the idle budget (~700ms): {:?}", t0.elapsed());
    let cnt = pool.relay_stats().await.timeouts;
    assert!(cnt.iter().any(|t| t.phase == "idle"), "idle timeout counted: {cnt:?}");
}

/// Slow-but-alive: chunks every 150ms with a 700ms idle budget. The stream must NOT
/// be killed — only inactivity kills, not slowness — and all chunks are delivered.
#[tokio::test]
async fn slow_but_alive_stream_survives() {
    let _serial = net_serial();
    let _g = EnvGuard::set("HIVE_P2P_IDLE_MS", "700");
    let func = spawn_slow_function(4, Duration::from_millis(150), false).await;
    let Some((pool, id, addr)) = setup_serving(func).await else {
        eprintln!("skip: no iroh");
        return;
    };

    let mut ts = tokio::time::timeout(
        Duration::from_secs(15),
        pool.request_stream(&id, &addr, "GET", "/sse", &[], b""),
    )
    .await
    .expect("stream setup must not hang")
    .expect("stream opens");
    assert_eq!(ts.status, 200);

    let mut body = Vec::new();
    while let Some(c) = tokio::time::timeout(Duration::from_secs(3), ts.recv())
        .await
        .expect("each chunk arrives within budget")
    {
        body.extend_from_slice(&c);
    }
    assert!(!ts.timed_out(), "an active stream (chunk gap < idle) must NOT be killed");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("data: 0") && body.contains("data: 3"), "all chunks delivered: {body}");
}
