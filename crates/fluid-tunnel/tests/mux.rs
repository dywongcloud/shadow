//! Deterministic test: many concurrent requests multiplexed over ONE tunnel.

use std::sync::Arc;
use std::time::Duration;

use fluid_tunnel::{TunnelClient, TunnelServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A tiny HTTP/1.1 server: replies 200 with a body echoing the request path.
/// Honors `Connection: close` (closes after responding).
async fn spawn_http_echo() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match l.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                // Read request head (until \r\n\r\n); ignore body.
                let mut acc = Vec::new();
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&acc);
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                // Simulate a little async work.
                tokio::time::sleep(Duration::from_millis(5)).await;
                let body = format!("hello {path}");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    addr
}

/// An HTTP/1.1 echo server that reflects per-request identity back: it parses the
/// request line, the `x-ctx` request header, and the FULL body (by content-length),
/// then replies with `x-ctx: <value>` AND a body of `"<path>|<x-ctx>|<body>"`. This
/// lets a test detect ANY cross-request contamination — a swapped response, a
/// mis-routed header, or an interleaved/corrupted body — because each field is
/// uniquely keyed to the request that sent it.
async fn spawn_ctx_echo() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match l.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut acc: Vec<u8> = Vec::new();
                // Read until end of headers.
                let head_end = loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    acc.extend_from_slice(&buf[..n]);
                    if let Some(p) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&acc[..head_end]).to_string();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let hval = |name: &str| -> Option<String> {
                    head.lines()
                        .find(|l| l.to_ascii_lowercase().starts_with(&format!("{name}:")))
                        .map(|l| l[l.find(':').unwrap() + 1..].trim().to_string())
                };
                let ctx = hval("x-ctx").unwrap_or_default();
                let clen: usize = hval("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                // Read the remaining body bytes.
                let mut body = acc[head_end..].to_vec();
                while body.len() < clen {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    body.extend_from_slice(&buf[..n]);
                }
                // Small randomized-ish delay (by body length) to force interleaving.
                tokio::time::sleep(Duration::from_millis((clen % 7) as u64)).await;
                let body_str = String::from_utf8_lossy(&body);
                let out = format!("{path}|{ctx}|{body_str}");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-ctx: {ctx}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    out.len(),
                    out
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    addr
}

/// #17 context-leak runtime test: many concurrent requests share ONE tunnel (the
/// in-instance concurrency model). Each carries a unique path, `x-ctx` header, and
/// a unique variable-length body. Every response must reflect EXACTLY its own
/// request's path/header/body — proving the stream-id demux never leaks one
/// request's context into another's response under heavy interleaving.
#[tokio::test]
async fn concurrent_requests_do_not_leak_context() {
    let http_addr = spawn_ctx_echo().await;

    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tunnel_addr = l.local_addr().unwrap();
    let local = http_addr.clone();
    tokio::spawn(async move {
        let (server_conn, _) = l.accept().await.unwrap();
        TunnelServer::serve(server_conn, local, 1000).await;
    });
    let client_conn = TcpStream::connect(tunnel_addr).await.unwrap();
    let client = Arc::new(TunnelClient::new(client_conn));

    let mut handles = Vec::new();
    for i in 0..150u32 {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let path = format!("/r/{i}");
            let ctx = format!("ctx-{i:08x}");
            // Unique, variable-length body so a swap/corruption can't accidentally match.
            let body = format!("body-{i}-").repeat((i % 37 + 1) as usize);
            let resp = tokio::time::timeout(
                Duration::from_secs(15),
                c.request(
                    "POST",
                    &path,
                    vec![("x-ctx".into(), ctx.clone())],
                    body.as_bytes(),
                ),
            )
            .await
            .expect("request timed out")
            .expect("request failed");
            assert_eq!(resp.status, 200);
            // Response header must carry THIS request's ctx, not another's.
            let got_ctx = resp
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("x-ctx"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            assert_eq!(got_ctx, ctx, "response x-ctx header leaked across requests");
            let mut got = Vec::new();
            let mut rx = resp.body;
            while let Some(chunk) = rx.recv().await {
                got.extend_from_slice(&chunk);
            }
            assert_eq!(
                String::from_utf8_lossy(&got),
                format!("{path}|{ctx}|{body}"),
                "response body leaked/corrupted across concurrent requests"
            );
            i
        }));
    }
    let mut done = 0;
    for h in handles {
        h.await.expect("task panicked");
        done += 1;
    }
    assert_eq!(done, 150);
}

/// #14: the tunnel meters request/response bytes and reports them in-band; the
/// client surfaces them via `health()`. Drive known traffic and assert the byte
/// counters move (and exceed the payload bytes, since framing/headers add more).
#[tokio::test]
async fn tunnel_meters_bytes_in_and_out() {
    let http_addr = spawn_http_echo().await;
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tunnel_addr = l.local_addr().unwrap();
    let local = http_addr.clone();
    tokio::spawn(async move {
        let (server_conn, _) = l.accept().await.unwrap();
        TunnelServer::serve(server_conn, local, 1000).await;
    });
    let client_conn = TcpStream::connect(tunnel_addr).await.unwrap();
    let client = Arc::new(TunnelClient::new(client_conn));

    // Send a handful of requests with sizable bodies; drain each response.
    let body = vec![b'z'; 1000];
    for i in 0..10 {
        let resp = client
            .request("POST", &format!("/p/{i}"), vec![], &body)
            .await
            .expect("request failed");
        assert_eq!(resp.status, 200);
        let mut rx = resp.body;
        while rx.recv().await.is_some() {}
    }

    // Metrics tick every 500ms; wait for at least one report after the traffic.
    let mut h = client.health();
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        h = client.health();
        if h.bytes_in > 0 && h.bytes_out > 0 {
            break;
        }
    }
    assert!(h.alive, "tunnel should be alive");
    assert!(
        h.bytes_in >= 10_000,
        "bytes_in should reflect ~10x1000B request bodies, got {}",
        h.bytes_in
    );
    assert!(
        h.bytes_out > 0,
        "bytes_out should be counted, got {}",
        h.bytes_out
    );
    // No backpressure under a fast local consumer.
    assert_eq!(
        h.backpressure_events, 0,
        "no backpressure expected on a drained tunnel"
    );
}

#[tokio::test]
async fn many_concurrent_requests_over_one_tunnel() {
    let http_addr = spawn_http_echo().await;

    // Connect the two tunnel ends over a loopback TCP pair.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tunnel_addr = l.local_addr().unwrap();
    let local = http_addr.clone();
    tokio::spawn(async move {
        let (server_conn, _) = l.accept().await.unwrap();
        TunnelServer::serve(server_conn, local, 1000).await;
    });
    let client_conn = TcpStream::connect(tunnel_addr).await.unwrap();
    let client = Arc::new(TunnelClient::new(client_conn));

    // Fire 200 concurrent requests over the single tunnel.
    let mut handles = Vec::new();
    for i in 0..200 {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let path = format!("/req/{i}");
            let resp = tokio::time::timeout(
                Duration::from_secs(10),
                c.request("GET", &path, vec![], b""),
            )
            .await
            .expect("request timed out")
            .expect("request failed");
            assert_eq!(resp.status, 200);
            // Drain the body.
            let mut body = Vec::new();
            let mut rx = resp.body;
            while let Some(chunk) = rx.recv().await {
                body.extend_from_slice(&chunk);
            }
            assert_eq!(String::from_utf8_lossy(&body), format!("hello {path}"));
            i
        }));
    }

    let mut done = 0;
    for h in handles {
        h.await.expect("task panicked");
        done += 1;
    }
    assert_eq!(done, 200, "all 200 multiplexed requests should complete");
}
