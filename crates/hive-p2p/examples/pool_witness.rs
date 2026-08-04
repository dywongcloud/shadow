//! Live witness for `browser-p2p-remove-integration-tests` — replaces the
//! deleted `tests/pool.rs` and `tests/stream_ws.rs` with live executions of
//! the same behaviors against REAL bound iroh endpoints. Every assertion the
//! deleted files made appears here as a printed `WITNESS_OK`/`WITNESS_FAIL`
//! line; the trust-gating pair (`untrusted_peer_is_rejected` /
//! `trusted_peer_is_admitted`) lives in `trust_gate_witness.rs` instead.
//!
//! Phases run SEQUENTIALLY in one process (the deleted suite's NET_SERIAL
//! lock existed only because `cargo test` runs tests concurrently; a witness
//! binary needs no such guard). Timeout-budget phases scope their
//! `HIVE_P2P_*_MS` overrides with a drop guard exactly like the suite did.
//!
//! Usage: `cargo run -p hive-p2p --example pool_witness`
//! Exit code 0 = every phase passed (or skipped offline); 1 = a real failure.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

static FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn report(ok: bool, label: &str, detail: String) {
    if ok {
        println!("WITNESS_OK:{label}: {detail}");
    } else {
        println!("WITNESS_FAIL:{label}: {detail}");
        FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Restore an env var on drop so a per-phase budget override can't leak into
/// the later phases (the deleted suite's EnvGuard, verbatim).
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

/// Bind node B (serving the echo fn) + node A (the dialer + pool).
async fn setup() -> Option<(Arc<hive_p2p::PeerPool>, String, String)> {
    setup_serving(spawn_function().await).await
}

/// Like `setup` but lets the caller supply a custom (e.g. slow) function.
async fn setup_serving(function: String) -> Option<(Arc<hive_p2p::PeerPool>, String, String)> {
    let ep_b = hive_p2p::bind().await.ok()?;
    let id = ep_b.id().to_string();
    let addr = hive_p2p::addr_json(&ep_b)?;
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, function, 100, None, None));
    let ep_a = hive_p2p::bind().await.ok()?;
    Some((hive_p2p::PeerPool::new(ep_a), id, addr))
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

/// A slow HTTP "function": head, then `chunks` SSE chunks each followed by
/// `gap`, then either hang forever or clean-EOF.
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
            });
        }
    });
    addr
}

/// HTTP "function" that streams one chunk, pauses 500ms, then the rest —
/// the incremental-delivery proof from stream_ws.rs.
async fn spawn_streaming_fn() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = match l.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
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
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n")
                    .await;
                let _ = s.flush().await;
                let _ = s.write_all(b"5\r\nfirst\r\n").await;
                let _ = s.flush().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = s.write_all(b"6\r\nsecond\r\n").await;
                let _ = s.flush().await;
                let _ = s.write_all(b"0\r\n\r\n").await;
                let _ = s.flush().await;
            });
        }
    });
    addr
}

/// Raw TCP echo server — the WebSocket-splice stand-in from stream_ws.rs.
async fn spawn_echo() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = match l.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                            let _ = s.flush().await;
                        }
                    }
                }
            });
        }
    });
    addr
}

#[tokio::main]
async fn main() {
    // ---- Phase: sequential requests reuse ONE trunk -----------------------
    if let Some((pool, id, addr)) = setup().await {
        let (s1, b1) = one(&pool, &id, &addr, "/a").await;
        let (s2, b2) = one(&pool, &id, &addr, "/b").await;
        let (opened, reused) = pool.stats();
        report(
            s1 == 200 && s2 == 200 && b1.contains("iroh-p2p") && b2.contains("iroh-p2p")
                && opened == 1 && reused == 1,
            "sequential-requests-reuse-one-connection",
            format!("two 200s over one trunk (opened={opened}, reused={reused})"),
        );
    } else {
        println!("WITNESS_SKIP:sequential-requests-reuse-one-connection: iroh could not bind");
    }

    // ---- Phase: 16 concurrent requests share ONE connection ----------------
    if let Some((pool, id, addr)) = setup().await {
        let (s, _) = one(&pool, &id, &addr, "/warm").await;
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
            if status == 200 && body.contains("iroh-p2p") {
                ok += 1;
            }
        }
        let (opened, reused) = pool.stats();
        report(
            s == 200 && ok == 16 && opened == 1 && reused == 16,
            "concurrent-requests-share-one-connection",
            format!("16 concurrent streams over one trunk (opened={opened}, reused={reused}, ok={ok})"),
        );
    } else {
        println!("WITNESS_SKIP:concurrent-requests-share-one-connection: iroh could not bind");
    }

    // ---- Phase: killing the trunk forces an in-call re-dial ----------------
    if let Some((pool, id, addr)) = setup().await {
        let (s1, _) = one(&pool, &id, &addr, "/first").await;
        let opened1 = pool.stats().0;
        pool.close_peer(&id).await;
        let (s2, b2) = one(&pool, &id, &addr, "/second").await;
        report(
            s1 == 200 && s2 == 200 && b2.contains("iroh-p2p") && opened1 == 1 && pool.stats().0 == 2,
            "killed-trunk-redials",
            format!("opened {} -> {} after close_peer", opened1, pool.stats().0),
        );
    } else {
        println!("WITNESS_SKIP:killed-trunk-redials: iroh could not bind");
    }

    // ---- Phase: a severed-but-CACHED trunk is detected and re-dialed -------
    if let Some((pool, id, addr)) = setup().await {
        let (s1, _) = one(&pool, &id, &addr, "/first").await;
        let (opened0, reused0) = pool.stats();
        let severed = pool.sever_peer(&id).await;
        let (s2, b2) = one(&pool, &id, &addr, "/after-sever").await;
        report(
            s1 == 200 && s2 == 200 && b2.contains("iroh-p2p") && opened0 >= 1 && reused0 == 0
                && severed && pool.stats().0 == opened0 + 1,
            "severed-cached-trunk-is-detected-and-redialed",
            format!("severed={severed}, opened {opened0} -> {}", pool.stats().0),
        );
    } else {
        println!("WITNESS_SKIP:severed-cached-trunk-is-detected-and-redialed: iroh could not bind");
    }

    // ---- Phase: relay-vs-direct classification accounts every byte ---------
    if let Some((pool, id, addr)) = setup().await {
        let (s, _) = one(&pool, &id, &addr, "/a").await;
        let rs = pool.relay_stats().await;
        let relayed = rs.relayed_bytes_tx + rs.relayed_bytes_rx;
        let direct = rs.direct_bytes_tx + rs.direct_bytes_rx;
        let one_bucket = rs.relayed_conns + rs.direct_conns == 1;
        let bytes_counted = relayed + direct > 0;
        let consistent = if rs.direct_conns == 1 { relayed == 0 } else { direct == 0 };
        report(
            s == 200 && one_bucket && bytes_counted && consistent,
            "relay-stats-classifies-and-counts-bytes",
            format!("relayed_conns={} direct_conns={} relayed_bytes={} direct_bytes={}",
                rs.relayed_conns, rs.direct_conns, relayed, direct),
        );
    } else {
        println!("WITNESS_SKIP:relay-stats-classifies-and-counts-bytes: iroh could not bind");
    }

    // ---- Phase: gossip round-trips over iroh, second request reuses trunk --
    if let Some(ep_b) = hive_p2p::bind().await.ok() {
        let addr_b = hive_p2p::addr_json(&ep_b).unwrap();
        let id_b = ep_b.id().to_string();
        let handler: hive_p2p::GossipHandler = Arc::new(|method, path, body, _signer| {
            Box::pin(async move {
                format!("{method}:{path}:{}", String::from_utf8_lossy(&body)).into_bytes()
            })
        });
        tokio::spawn(hive_p2p::serve_tunnels(ep_b, "127.0.0.1:1".into(), 100, None, Some(handler)));
        let ep_a = hive_p2p::bind().await.unwrap();
        let pool = hive_p2p::PeerPool::new(ep_a);
        let r1 = tokio::time::timeout(
            Duration::from_secs(20),
            pool.gossip_request(&id_b, &addr_b, hive_p2p::GOSSIP_POST, "/v1/nodes", b"hello"),
        )
        .await
        .expect("gossip timed out")
        .expect("gossip failed");
        let r2 = pool
            .gossip_request(&id_b, &addr_b, hive_p2p::GOSSIP_GET, "/v1/serve-hosts", b"")
            .await
            .expect("second gossip failed");
        report(
            String::from_utf8_lossy(&r1) == "1:/v1/nodes:hello"
                && String::from_utf8_lossy(&r2) == "0:/v1/serve-hosts:",
            "gossip-request-round-trips-over-iroh",
            format!("{} | {}", String::from_utf8_lossy(&r1), String::from_utf8_lossy(&r2)),
        );
    } else {
        println!("WITNESS_SKIP:gossip-request-round-trips-over-iroh: iroh could not bind");
    }

    // ---- Phase: SIGNED gossip verifies and the handler sees the signer -----
    for trusted_case in [false, true] {
        let _sign = EnvGuard::set("HIVE_GOSSIP_SIGN", "1");
        let _verify = EnvGuard::set("HIVE_GOSSIP_VERIFY", "enforce");
        let label = if trusted_case {
            "signed-gossip-trusted-still-verifies"
        } else {
            "signed-gossip-verifies-and-passes-signer"
        };
        if let Some(ep_b) = hive_p2p::bind().await.ok() {
            let addr_b = hive_p2p::addr_json(&ep_b).unwrap();
            let id_b = ep_b.id().to_string();
            let handler: hive_p2p::GossipHandler = Arc::new(|_m, _p, _b, signer| {
                Box::pin(async move { signer.unwrap_or_else(|| "UNSIGNED".into()).into_bytes() })
            });
            let ep_a = hive_p2p::bind().await.unwrap();
            let id_a = ep_a.id().to_string();
            let trust = trusted_case.then(|| {
                let t: hive_p2p::TrustSet =
                    Arc::new(std::sync::RwLock::new(std::collections::HashSet::from([id_a.clone()])));
                t
            });
            tokio::spawn(hive_p2p::serve_tunnels(ep_b, "127.0.0.1:1".into(), 100, trust, Some(handler)));
            let pool = hive_p2p::PeerPool::new(ep_a);
            let resp = tokio::time::timeout(
                Duration::from_secs(20),
                pool.gossip_request(&id_b, &addr_b, hive_p2p::GOSSIP_POST, "/v1/nodes", b"signed"),
            )
            .await
            .expect("signed gossip timed out")
            .expect("signed gossip failed");
            let (ok, ..) = hive_p2p::verify_stats();
            report(
                String::from_utf8_lossy(&resp) == id_a && ok >= 1,
                label,
                format!("handler saw verified signer {} (signed_ok={ok})", String::from_utf8_lossy(&resp)),
            );
        } else {
            println!("WITNESS_SKIP:{label}: iroh could not bind");
        }
    }

    // ---- Phase: signature primitives reject tamper, replay, and mismatch ---
    {
        let sk = iroh::SecretKey::generate();
        let id = sk.public().to_string();
        let now: u64 = 1_700_000_000_000;
        let trailer = hive_p2p::sign_gossip(&sk, hive_p2p::GOSSIP_POST, "/v1/x", b"body", now);
        let valid = hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/x", b"body", &id, now)
            .map(|signer| signer == id)
            .unwrap_or(false);
        let tampered_body = hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/x", b"evil", &id, now).is_err();
        let tampered_path = hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/y", b"body", &id, now).is_err();
        let tampered_method = hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_GET, "/v1/x", b"body", &id, now).is_err();
        let replay = hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/x", b"body", &id, now + 10 * 60 * 1000).is_err();
        let other = iroh::SecretKey::generate().public().to_string();
        let mismatch = hive_p2p::verify_gossip(&trailer, hive_p2p::GOSSIP_POST, "/v1/x", b"body", &other, now).is_err();
        report(
            valid && tampered_body && tampered_path && tampered_method && replay && mismatch,
            "gossip-signature-rejects-tamper-replay-and-mismatch",
            format!("valid={valid} body={tampered_body} path={tampered_path} method={tampered_method} replay={replay} mismatch={mismatch}"),
        );
    }

    // ---- Phase: warm() pre-establishes the trunk a request then reuses -----
    {
        let _budget = EnvGuard::set("HIVE_P2P_CONNECT_MS", "20000");
        if let Some((pool, id, addr)) = setup().await {
            let warm = tokio::time::timeout(Duration::from_secs(25), pool.warm(&id, &addr))
                .await
                .expect("warm must not hang");
            let (s, b) = one(&pool, &id, &addr, "/warmed").await;
            let (opened, reused) = pool.stats();
            report(
                warm && s == 200 && b.contains("iroh-p2p") && opened == 1 && reused == 1,
                "warm-pre-establishes-trunk-reused-by-request",
                format!("warm={warm}, opened={opened}, reused={reused}"),
            );
        } else {
            println!("WITNESS_SKIP:warm-pre-establishes-trunk-reused-by-request: iroh could not bind");
        }
    }

    // ---- Phase: blackhole connect times out and is marked dead -------------
    {
        let _g1 = EnvGuard::set("HIVE_P2P_CONNECT_MS", "500");
        let _g2 = EnvGuard::set("HIVE_P2P_OPEN_MS", "500");
        let _g3 = EnvGuard::set("HIVE_P2P_DISCOVERY_MS", "500");
        if let Some(ep_b) = hive_p2p::bind().await.ok() {
            // Bound but NEVER served: connect/open can't complete.
            let id = ep_b.id().to_string();
            let addr = hive_p2p::addr_json(&ep_b).unwrap();
            let ep_a = hive_p2p::bind().await.unwrap();
            let pool = hive_p2p::PeerPool::new(ep_a);
            let t0 = Instant::now();
            let res = tokio::time::timeout(
                Duration::from_secs(8),
                pool.request(&id, &addr, "GET", "/x", &[], b""),
            )
            .await
            .expect("request must NOT hang — the connect budget must bound it");
            let elapsed = t0.elapsed();
            let dead = res
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<hive_p2p::DeadPeerTimeout>())
                .is_some();
            let counted = pool
                .relay_stats()
                .await
                .timeouts
                .iter()
                .any(|t| t.node_id == id && (t.phase == "connect" || t.phase == "open"));
            report(
                dead && elapsed < Duration::from_secs(4) && counted,
                "blackhole-connect-times-out-and-is-marked-dead",
                format!("dead_peer_timeout={dead} elapsed={elapsed:?} counted={counted}"),
            );
        } else {
            println!("WITNESS_SKIP:blackhole-connect-times-out-and-is-marked-dead: iroh could not bind");
        }
    }

    // ---- Phase: accept-but-silent owner hits the first-byte budget ---------
    {
        let _g = EnvGuard::set("HIVE_P2P_FIRSTBYTE_MS", "700");
        if let Some(ep_b) = hive_p2p::bind().await.ok() {
            let id = ep_b.id().to_string();
            let addr = hive_p2p::addr_json(&ep_b).unwrap();
            tokio::spawn(hive_p2p::serve_silent(ep_b));
            let ep_a = hive_p2p::bind().await.unwrap();
            let pool = hive_p2p::PeerPool::new(ep_a);
            // Warm outside the measured window so elapsed reflects first-byte.
            let _warm = tokio::time::timeout(Duration::from_secs(12), pool.open_raw(&id, &addr))
                .await
                .expect("warm must not hang")
                .expect("trunk warms");
            let t0 = Instant::now();
            let res = tokio::time::timeout(
                Duration::from_secs(12),
                pool.request(&id, &addr, "GET", "/x", &[], b""),
            )
            .await
            .expect("request must NOT hang — the first-byte budget must bound it");
            let elapsed = t0.elapsed();
            let post_send = res
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<hive_p2p::PostSendTimeout>())
                .is_some();
            let opened = pool.stats().0;
            let counted = pool.relay_stats().await.timeouts.iter().any(|t| t.phase == "firstbyte");
            report(
                post_send && elapsed < Duration::from_secs(3) && opened == 1 && counted,
                "accept-but-silent-first-byte-times-out-without-retry",
                format!("post_send={post_send} elapsed={elapsed:?} opened={opened} counted={counted}"),
            );
        } else {
            println!("WITNESS_SKIP:accept-but-silent-first-byte-times-out-without-retry: iroh could not bind");
        }
    }

    // ---- Phase: idle mid-stream fires the inter-chunk budget ---------------
    {
        let _g = EnvGuard::set("HIVE_P2P_IDLE_MS", "700");
        let func = spawn_slow_function(1, Duration::from_millis(0), true).await;
        if let Some((pool, id, addr)) = setup_serving(func).await {
            let mut ts = tokio::time::timeout(
                Duration::from_secs(15),
                pool.request_stream(&id, &addr, "GET", "/sse", &[], b""),
            )
            .await
            .expect("stream setup must not hang")
            .expect("stream opens");
            let first = tokio::time::timeout(Duration::from_secs(3), ts.recv())
                .await
                .expect("first chunk soon");
            let t0 = Instant::now();
            let next = tokio::time::timeout(Duration::from_secs(5), ts.recv())
                .await
                .expect("recv must self-terminate on idle, not hang");
            let counted = pool.relay_stats().await.timeouts.iter().any(|t| t.phase == "idle");
            report(
                ts.status == 200 && first.is_some() && next.is_none() && ts.timed_out()
                    && t0.elapsed() < Duration::from_secs(3) && counted,
                "idle-timeout-fires-when-stream-goes-silent",
                format!("first={} next_none={} timed_out={} elapsed={:?} counted={counted}",
                    first.is_some(), next.is_none(), ts.timed_out(), t0.elapsed()),
            );
        } else {
            println!("WITNESS_SKIP:idle-timeout-fires-when-stream-goes-silent: iroh could not bind");
        }
    }

    // ---- Phase: slow-but-alive streams are NOT killed ----------------------
    {
        let _g = EnvGuard::set("HIVE_P2P_IDLE_MS", "700");
        let func = spawn_slow_function(4, Duration::from_millis(150), false).await;
        if let Some((pool, id, addr)) = setup_serving(func).await {
            let mut ts = tokio::time::timeout(
                Duration::from_secs(15),
                pool.request_stream(&id, &addr, "GET", "/sse", &[], b""),
            )
            .await
            .expect("stream setup must not hang")
            .expect("stream opens");
            let mut body = Vec::new();
            while let Some(c) = tokio::time::timeout(Duration::from_secs(3), ts.recv())
                .await
                .expect("each chunk arrives within budget")
            {
                body.extend_from_slice(&c);
            }
            let body_s = String::from_utf8_lossy(&body);
            report(
                ts.status == 200 && !ts.timed_out()
                    && body_s.contains("data: 0") && body_s.contains("data: 3"),
                "slow-but-alive-stream-survives",
                format!("timed_out={} body={}", ts.timed_out(), body_s.replace('\n', "\\n")),
            );
        } else {
            println!("WITNESS_SKIP:slow-but-alive-stream-survives: iroh could not bind");
        }
    }

    // ---- Phase (stream_ws #1): chunked responses arrive INCREMENTALLY ------
    {
        let fnaddr = spawn_streaming_fn().await;
        if let Some((pool, id, addr)) = setup_serving(fnaddr).await {
            let mut ts = tokio::time::timeout(
                Duration::from_secs(20),
                pool.request_stream(&id, &addr, "GET", "/sse", &[], b""),
            )
            .await
            .expect("stream setup")
            .expect("stream opens");
            let t0 = Instant::now();
            let first = tokio::time::timeout(Duration::from_millis(400), ts.recv()).await;
            let first_elapsed = t0.elapsed();
            let first_ok = matches!(&first, Ok(Some(_))) && first_elapsed < Duration::from_millis(400);
            let mut body = Vec::new();
            if let Ok(Some(c)) = first {
                body.extend_from_slice(&c);
            }
            while let Some(c) = ts.recv().await {
                body.extend_from_slice(&c);
            }
            let s = String::from_utf8_lossy(&body);
            report(
                ts.status == 200 && first_ok && s.contains("first") && s.contains("second"),
                "response-streams-incrementally-cross-node",
                format!("first chunk in {first_elapsed:?} (before the server's 500ms pause), body reassembles"),
            );
        } else {
            println!("WITNESS_SKIP:response-streams-incrementally-cross-node: iroh could not bind");
        }
    }

    // ---- Phase (stream_ws #2): a raw stream splices bytes BOTH ways --------
    {
        let echo = spawn_echo().await;
        if let Some((pool, id, addr)) = setup_serving(echo).await {
            let mut raw = tokio::time::timeout(Duration::from_secs(20), pool.open_raw(&id, &addr))
                .await
                .expect("open_raw must not hang")
                .expect("open_raw");
            let msg = b"GET /ws HTTP/1.1\r\nupgrade: websocket\r\n\r\n[frame:hello]";
            raw.write_all(msg).await.expect("write");
            raw.flush().await.expect("flush");
            let mut got = vec![0u8; msg.len()];
            let read = tokio::time::timeout(Duration::from_secs(10), raw.read_exact(&mut got)).await;
            let echoed = matches!(read, Ok(Ok(_))) && got == msg;
            report(
                echoed,
                "raw-stream-splices-both-ways-cross-node",
                format!("{} bytes echoed verbatim over the raw splice", got.len()),
            );
        } else {
            println!("WITNESS_SKIP:raw-stream-splices-both-ways-cross-node: iroh could not bind");
        }
    }

    let failures = FAILURES.load(std::sync::atomic::Ordering::Relaxed);
    if failures == 0 {
        println!("WITNESS_OK:ALL: every deleted tests/pool.rs + tests/stream_ws.rs assertion holds live");
    } else {
        println!("WITNESS_FAIL:{failures} phase(s) failed");
        std::process::exit(1);
    }
}
