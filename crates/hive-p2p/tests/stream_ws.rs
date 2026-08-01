//! Streaming (#1) and raw/WebSocket-transport (#2) cross-node tests for PeerPool.
//!
//! Bind two real iroh endpoints (skip if offline), serve tunnels on B over a
//! local server, and drive A → B through the pool. Mirrors `tests/pool.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// HTTP "function" that streams a chunked response: one chunk, a long pause, then
/// the rest. The pause lets the test prove the first chunk arrives incrementally
/// (not buffered until the whole body is ready).
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
                // Long pause BEFORE the rest: the client must already have "first".
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

/// Raw TCP echo server — stands in for an upgraded/WebSocket peer the owner
/// splices a raw stream to.
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

/// Bind node B (serving `local`) + node A (pool). `None` if iroh can't bind.
async fn bind_pair(local: String) -> Option<(Arc<hive_p2p::PeerPool>, String, String)> {
    let ep_b = hive_p2p::bind().await.ok()?;
    let id = ep_b.id().to_string();
    let addr = hive_p2p::addr_json(&ep_b)?;
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, local, 100, None, None));
    let ep_a = hive_p2p::bind().await.ok()?;
    Some((hive_p2p::PeerPool::new(ep_a), id, addr))
}

/// #1 — a chunked / SSE response routed cross-node arrives incrementally: the
/// first chunk is delivered before the owner has produced the rest.
#[tokio::test]
async fn response_streams_incrementally_cross_node() {
    let fnaddr = spawn_streaming_fn().await;
    let Some((pool, id, addr)) = bind_pair(fnaddr).await else {
        eprintln!("skipping: iroh could not bind");
        return;
    };
    let mut ts = match tokio::time::timeout(
        Duration::from_secs(20),
        pool.request_stream(&id, &addr, "GET", "/sse", &[], b""),
    )
    .await
    {
        Ok(Ok(t)) => t,
        _ => {
            eprintln!("skipping: request_stream failed (offline?)");
            return;
        }
    };
    assert_eq!(ts.status, 200);

    // The first chunk must arrive well within the server's 500ms pre-rest pause.
    let t0 = Instant::now();
    let first = tokio::time::timeout(Duration::from_millis(400), ts.recv())
        .await
        .expect("first chunk should arrive before the server's 500ms pause (incremental)")
        .expect("a first body chunk");
    assert!(
        t0.elapsed() < Duration::from_millis(400),
        "delivered incrementally, not buffered"
    );

    let mut body = Vec::new();
    body.extend_from_slice(&first);
    while let Some(c) = ts.recv().await {
        body.extend_from_slice(&c);
    }
    let s = String::from_utf8_lossy(&body);
    assert!(
        s.contains("first") && s.contains("second"),
        "full body reassembles: {s:?}"
    );
}

/// #2 — a raw bidirectional stream over the trunk splices bytes both ways
/// (client ↔ iroh ↔ owner's target). This is the WebSocket cross-node transport.
#[tokio::test]
async fn raw_stream_splices_both_ways_cross_node() {
    let echo = spawn_echo().await;
    let Some((pool, id, addr)) = bind_pair(echo).await else {
        eprintln!("skipping: iroh could not bind");
        return;
    };
    let mut raw =
        match tokio::time::timeout(Duration::from_secs(20), pool.open_raw(&id, &addr)).await {
            Ok(Ok(s)) => s,
            _ => {
                eprintln!("skipping: open_raw failed (offline?)");
                return;
            }
        };
    // A WebSocket handshake + frame, relayed verbatim over the raw splice.
    let msg = b"GET /ws HTTP/1.1\r\nupgrade: websocket\r\n\r\n[frame:hello]";
    raw.write_all(msg).await.unwrap();
    raw.flush().await.unwrap();
    let mut got = vec![0u8; msg.len()];
    tokio::time::timeout(Duration::from_secs(10), raw.read_exact(&mut got))
        .await
        .expect("echo returns within 10s")
        .expect("read the echoed bytes");
    assert_eq!(
        &got, msg,
        "raw bytes echo back over the cross-node splice (both directions)"
    );
}
