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
