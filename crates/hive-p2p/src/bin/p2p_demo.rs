//! Demo: route a Fluid function request between two iroh nodes, peer-to-peer.
//!
//! Node B hosts a "function" and serves the tunnel protocol over iroh.
//! Node A dials B *by node id* and sends a request through a TunnelClient.
//! Proves the whole serving stack works over a P2P QUIC connection.

use std::time::Duration;

use anyhow::Result;
use fluid_tunnel::TunnelClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Minimal HTTP echo server (the "function").
async fn spawn_function() -> Result<String> {
    let l = TcpListener::bind("127.0.0.1:0").await?;
    let addr = l.local_addr()?.to_string();
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
    Ok(addr)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber_init();

    // ---- Node B (the instance host) ----
    let function = spawn_function().await?;
    let ep_b = hive_p2p::bind().await?;
    let node_b_id = ep_b.id();
    let addr_b = ep_b.addr();
    println!("node B id: {node_b_id}");
    let max_conc = 100;
    tokio::spawn(hive_p2p::serve_tunnels(
        ep_b, function, max_conc, None, None,
    ));

    // ---- Node A (the gateway) dials B by endpoint id, over P2P ----
    let ep_a = hive_p2p::bind().await?;
    println!("node A id: {}", ep_a.id());
    println!("dialing node B peer-to-peer over iroh...");
    let stream = hive_p2p::dial(&ep_a, addr_b).await?;
    let client = TunnelClient::new(stream);

    // Fire a handful of concurrent requests over the single P2P tunnel.
    let client = std::sync::Arc::new(client);
    let mut handles = Vec::new();
    for i in 0..5 {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let resp = tokio::time::timeout(
                Duration::from_secs(20),
                c.request("GET", &format!("/p2p/{i}"), vec![], b"", Duration::from_secs(20)),
            )
            .await
            .expect("timeout")
            .expect("request failed");
            let mut body = Vec::new();
            let mut rx = resp.body;
            while let Some(chunk) = rx.recv().await {
                body.extend_from_slice(&chunk);
            }
            (resp.status, String::from_utf8_lossy(&body).to_string())
        }));
    }
    for (i, h) in handles.into_iter().enumerate() {
        let (status, body) = h.await?;
        println!("  request {i}: status={status} body={body}");
        assert_eq!(status, 200);
        assert!(body.contains("iroh-p2p"));
    }
    println!("OK: 5 requests routed over an iroh P2P tunnel");
    Ok(())
}

fn tracing_subscriber_init() {
    // Best-effort; ignore if already set.
    let _ = std::panic::catch_unwind(|| {});
}
