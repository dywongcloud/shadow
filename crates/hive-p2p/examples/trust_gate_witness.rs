//! Live witness for `browser-p2p-remove-standing-tests` — replaces the
//! deleted `#[cfg(test)] mod trust_tests` from `src/lib.rs` with live
//! executions of the same behaviors at the REAL trust boundary:
//!
//!   1. `peer_trust_allowlist_admits_only_known_ids`
//!      → a REAL iroh peer whose identity is NOT in the serving node's trust
//!        set is refused (connection dropped before any stream is served),
//!        while a peer whose identity IS listed is served a real 200 over the
//!        same listener. The trust set is read live per connection, exactly
//!        like production's `HIVE_PEER_TRUST=1` path.
//!   2. `persistent_secret_is_stable_across_loads`
//!      → `bind_full` with a key file twice yields the SAME endpoint id; a
//!        malformed key file is regenerated without a panic.
//!   3. `endpoint_id_from_garbage_is_none` / `parse_bootstrap_seeds_forms`
//!      → the REAL parsers, executed live: garbage addr_json yields no id;
//!        a seed CSV keeps the three valid forms and drops garbage.
//!
//! Usage: `cargo run -p hive-p2p --example trust_gate_witness`
//! Exit code 0 = every witness line passed; 1 = at least one failed.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Minimal HTTP echo server (the "function" the trusted peer is served).
async fn spawn_function() -> anyhow::Result<String> {
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
                let body = "{\"served_over\":\"iroh-p2p\"}";
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

fn report(ok: bool, label: &str, detail: &str, failures: &mut usize) {
    if ok {
        println!("WITNESS_OK:{label}: {detail}");
    } else {
        println!("WITNESS_FAIL:{label}: {detail}");
        *failures += 1;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut failures = 0usize;

    // ---- Witness 1: the trust gate over real QUIC connections ----
    let function = spawn_function().await?;
    let ep_b = hive_p2p::bind().await?;
    let id_b = ep_b.id().to_string();
    let addr_b = hive_p2p::addr_json(&ep_b)
        .ok_or_else(|| anyhow::anyhow!("server addr_json unavailable"))?;
    // Empty trust set, NO join handler: fail-closed, every non-JOIN stream
    // refused — the exact `HIVE_PEER_TRUST=1` shape (minus the fleet env).
    let trust: hive_p2p::TrustSet =
        Arc::new(std::sync::RwLock::new(std::collections::HashSet::new()));
    tokio::spawn(hive_p2p::serve_tunnels(
        ep_b,
        function,
        100,
        Some(trust.clone()),
        None,
    ));

    // An UNTRUSTED peer: its request must never be served. The server drops
    // the connection at admission; the client observes a failure, never a 200.
    let ep_untrusted = hive_p2p::bind().await?;
    let id_untrusted = ep_untrusted.id().to_string();
    let pool_untrusted = hive_p2p::PeerPool::new(ep_untrusted);
    let refused = tokio::time::timeout(
        Duration::from_secs(20),
        pool_untrusted.request(&id_b, &addr_b, "GET", "/x", &[], b""),
    )
    .await;
    let refused_ok = !matches!(refused, Ok(Ok(_)));
    report(
        refused_ok,
        "untrusted-peer-refused",
        &format!(
            "peer {id_untrusted} not in trust set was not served (outcome: {})",
            match &refused {
                Ok(Ok(_)) => "SERVED — gate broken".to_string(),
                Ok(Err(e)) => format!("request error: {e}"),
                Err(_) => "request timed out unserved".to_string(),
            }
        ),
        &mut failures,
    );

    // A TRUSTED peer: insert its REAL iroh identity into the live trust set,
    // then the same request path returns a real 200 from the echo function.
    let ep_trusted = hive_p2p::bind().await?;
    let id_trusted = ep_trusted.id().to_string();
    trust.write().unwrap().insert(id_trusted.clone());
    let pool_trusted = hive_p2p::PeerPool::new(ep_trusted);
    let served = tokio::time::timeout(
        Duration::from_secs(20),
        pool_trusted.request(&id_b, &addr_b, "GET", "/ok", &[], b""),
    )
    .await;
    match served {
        Ok(Ok(resp)) => report(
            resp.status == 200 && String::from_utf8_lossy(&resp.body).contains("iroh-p2p"),
            "trusted-peer-served",
            &format!(
                "peer {id_trusted} in trust set served status={}",
                resp.status
            ),
            &mut failures,
        ),
        other => report(
            false,
            "trusted-peer-served",
            &format!(
                "trusted peer was NOT served: {}",
                match other {
                    Ok(Err(e)) => format!("request error: {e}"),
                    Err(_) => "timed out".to_string(),
                    Ok(Ok(_)) => unreachable!(),
                }
            ),
            &mut failures,
        ),
    }

    // ---- Witness 2: persistent identity is stable across binds; malformed
    // key material is regenerated, never a panic ----
    let dir = std::env::temp_dir().join(format!("hive-trust-witness-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let key_path = dir.join("iroh_secret.key");
    let ep1 = hive_p2p::bind_full(Some(key_path.clone()), &[], &[], false).await?;
    let id1 = ep1.id().to_string();
    let written = key_path.exists();
    drop(ep1);
    let ep2 = hive_p2p::bind_full(Some(key_path.clone()), &[], &[], false).await?;
    let id2 = ep2.id().to_string();
    drop(ep2);
    report(
        written && id1 == id2,
        "persistent-key-stable",
        &format!("key file written={written}, id stable across binds: {id1}"),
        &mut failures,
    );
    std::fs::write(&key_path, b"short")?;
    let ep3 = hive_p2p::bind_full(Some(key_path.clone()), &[], &[], false).await?;
    let id3 = ep3.id().to_string();
    drop(ep3);
    report(
        id3 != id1,
        "malformed-key-regenerated",
        "a malformed key file regenerated a fresh identity without a panic",
        &mut failures,
    );
    let _ = std::fs::remove_dir_all(&dir);

    // ---- Witness 3: the REAL parsers, executed live ----
    let garbage = hive_p2p::endpoint_id_from_addr_json("not json");
    let empty_obj = hive_p2p::endpoint_id_from_addr_json("{}");
    report(
        garbage.is_none() && empty_obj.is_none(),
        "garbage-addr-yields-no-id",
        "endpoint_id_from_addr_json(\"not json\"/\"{}\") = None",
        &mut failures,
    );
    let csv = format!(
        "{id_b} , {id_b}@1.2.3.4:9000+5.6.7.8:9001 , {id_b}|https://relay.example/ , not-a-key , "
    );
    let seeds = hive_p2p::parse_bootstrap_seeds(&csv);
    let seeds_ok = seeds.len() == 3
        && seeds.iter().all(|s| s.node_id == id_b)
        && seeds.iter().all(|s| {
            hive_p2p::endpoint_id_from_addr_json(&s.addr_json).as_deref() == Some(id_b.as_str())
        })
        && seeds[1].addr_json.contains("1.2.3.4")
        && seeds[1].addr_json.contains("5.6.7.8")
        && hive_p2p::parse_bootstrap_seeds("").is_empty();
    report(
        seeds_ok,
        "bootstrap-seed-forms",
        &format!(
            "3 valid seed forms parsed (bare / @addrs / |relay), garbage dropped, empty csv empty"
        ),
        &mut failures,
    );

    if failures == 0 {
        println!("WITNESS_OK:ALL: trust gate refuses untrusted peers, serves trusted peers, identity and parsers behave");
        Ok(())
    } else {
        println!("WITNESS_FAIL:{failures} check(s) failed");
        std::process::exit(1);
    }
}
