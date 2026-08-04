//! Live-witness harness for `bn-impl-protocol-handler`: bind a real hive-p2p
//! endpoint (BOTH ALPNs, per the crate's own `bind_full`), print its
//! `addr_json`, and serve `hive/tunnel/0` + `hive/browser/0` until killed.
//! Not part of any production binary — a throwaway proof that the FLEET
//! accept-loop code (not just hive-browser's own wasm accept loop) really
//! answers `hive/browser/0` echo requests from a real browser tab.
//!
//! Usage (serve):
//! `HIVE_RELAY_URLS=http://<relay-ip>:3340 cargo run --example browser_echo_native`
//!
//! Usage (invoke a browser): set `HIVE_BROWSER_ADDR`, `HIVE_BROWSER_DIGEST`,
//! and `HIVE_BROWSER_REQUEST`; the process prints the exact reply and exits.

use hive_browser_proto::{encode_request, Op};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key_path = std::env::var("HIVE_BROWSER_CLIENT_KEY")
        .ok()
        .map(std::path::PathBuf::from);
    let ep = hive_p2p::bind_full(key_path, &[], &[], false).await?;
    // `ep.addr()` right after bind() serializes whatever's registered so far —
    // for a fresh endpoint that's local network interface candidates only; the
    // relay entry lands asynchronously once the home-relay connection comes
    // up. A browser peer cannot dial raw IP transport at all (compiled out;
    // relay-only), so printing addr_json before `online()` resolves hands out
    // an EndpointAddr with real-but-useless entries and no relay hint at all —
    // exactly the shape that made the browser's connect() time out here.
    ep.online().await;
    println!("NATIVE_ID:{}", ep.id());
    if std::env::var("HIVE_BROWSER_PRINT_ID").as_deref() == Ok("1") {
        return Ok(());
    }
    if let (Ok(addr), Ok(mode)) = (
        std::env::var("HIVE_BROWSER_RAW_ADDR"),
        std::env::var("HIVE_BROWSER_RAW_MODE"),
    ) {
        let addr: iroh::EndpointAddr = serde_json::from_str(&addr)?;
        let conn = ep.connect(addr, hive_p2p::BROWSER_ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let bytes = match mode.as_str() {
            "trailing" => {
                let mut frame = encode_request(Op::Echo, b"x");
                frame.push(0xa5);
                frame
            }
            "prefix-truncated" => vec![2, 0],
            "body-truncated" | "body-stall" => {
                let mut frame = 3u32.to_le_bytes().to_vec();
                frame.extend_from_slice(&[Op::Echo.as_byte(), b'x']);
                frame
            }
            _ => anyhow::bail!("unknown HIVE_BROWSER_RAW_MODE {mode}"),
        };
        send.write_all(&bytes).await?;
        if mode != "body-stall" {
            send.finish()?;
        }
        let stopped = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            send.stopped(),
        )
        .await??;
        let reset = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            recv.received_reset(),
        )
        .await??;
        println!(
            "BROWSER_RAW_STOPPED_CODE:{}",
            stopped.map(|code| code.into_inner()).unwrap_or_default()
        );
        println!(
            "BROWSER_RAW_RESET_CODE:{}",
            reset.map(|code| code.into_inner()).unwrap_or_default()
        );
        return Ok(());
    }
    if let (Ok(addrs), Ok(digest), Ok(request)) = (
        std::env::var("HIVE_BROWSER_ADDRS"),
        std::env::var("HIVE_BROWSER_DIGEST"),
        std::env::var("HIVE_BROWSER_REQUEST"),
    ) {
        let addrs: Vec<String> = serde_json::from_str(&addrs)?;
        let pool = hive_p2p::BrowserPool::new(ep);
        for addr in &addrs {
            let endpoint_id = hive_p2p::endpoint_id_from_addr_json(addr)
                .ok_or_else(|| anyhow::anyhow!("HIVE_BROWSER_ADDRS entry has no endpoint id"))?;
            pool.invoke(&endpoint_id, addr, &digest, &request).await?;
        }
        println!("BROWSER_MULTI_INVOKE_COUNT:{}", addrs.len());
        println!("BROWSER_MULTI_TRUNKS:{}", pool.trunk_count().await);
        println!("BROWSER_POOL_STATS:{}", serde_json::to_string(&pool.stats())?);
        return Ok(());
    }
    if let (Ok(addr), Ok(digest), Ok(request)) = (
        std::env::var("HIVE_BROWSER_ADDR"),
        std::env::var("HIVE_BROWSER_DIGEST"),
        std::env::var("HIVE_BROWSER_REQUEST"),
    ) {
        let endpoint_id = hive_p2p::endpoint_id_from_addr_json(&addr)
            .ok_or_else(|| anyhow::anyhow!("HIVE_BROWSER_ADDR has no endpoint id"))?;
        let pool = hive_p2p::BrowserPool::new(ep);
        if let Ok(unrelated) = std::env::var("HIVE_BROWSER_CLOSE_UNRELATED_DURING_DIAL") {
            let invoke_pool = pool.clone();
            let invoke_endpoint = endpoint_id.clone();
            let invoke_addr = addr.clone();
            let invoke_digest = digest.clone();
            let invoke_request = request.clone();
            let invoke = tokio::spawn(async move {
                invoke_pool
                    .invoke(
                        &invoke_endpoint,
                        &invoke_addr,
                        &invoke_digest,
                        &invoke_request,
                    )
                    .await
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while pool.stats().dial_attempts_total == 0 {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("browser invoke did not enter dial");
                }
                tokio::task::yield_now().await;
            }
            pool.close_endpoint(&unrelated).await;
            let reply = invoke.await??;
            println!("BROWSER_UNRELATED_CLOSE_REPLY:{}", String::from_utf8_lossy(&reply));
            println!("BROWSER_POOL_STATS:{}", serde_json::to_string(&pool.stats())?);
            return Ok(());
        }
        if std::env::var("HIVE_BROWSER_CLOSE_DURING_DIAL").as_deref() == Ok("1") {
            let invoke_pool = pool.clone();
            let invoke_endpoint = endpoint_id.clone();
            let invoke_addr = addr.clone();
            let invoke_digest = digest.clone();
            let invoke_request = request.clone();
            let invoke = tokio::spawn(async move {
                invoke_pool
                    .invoke(
                        &invoke_endpoint,
                        &invoke_addr,
                        &invoke_digest,
                        &invoke_request,
                    )
                    .await
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while pool.stats().dial_attempts_total == 0 {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("browser invoke did not enter dial");
                }
                tokio::task::yield_now().await;
            }
            pool.close_endpoint(&endpoint_id).await;
            let result = invoke.await?;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let trunks = pool.trunk_count().await;
            let stats = pool.stats();
            println!("BROWSER_CLOSE_RACE_TRUNKS:{trunks}");
            println!("BROWSER_POOL_STATS:{}", serde_json::to_string(&stats)?);
            match result {
                Err(error) if !error.sent && trunks == 0 => {
                    println!("BROWSER_CLOSE_RACE_FENCED:{}", error.message);
                    return Ok(());
                }
                Err(error) => anyhow::bail!(
                    "close race did not fence cleanly (sent={}, trunks={}): {}",
                    error.sent,
                    trunks,
                    error.message
                ),
                Ok(_) => anyhow::bail!("close race unexpectedly completed invoke"),
            }
        }
        let concurrency = std::env::var("HIVE_BROWSER_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1);
        let started = std::time::Instant::now();
        let mut invokes = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let pool = pool.clone();
            let endpoint_id = endpoint_id.clone();
            let addr = addr.clone();
            let digest = digest.clone();
            let request = request.clone();
            invokes.push(tokio::spawn(async move {
                pool.invoke(&endpoint_id, &addr, &digest, &request).await
            }));
        }
        if let Some(abort_after_ms) = std::env::var("HIVE_BROWSER_ABORT_AFTER_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while pool.stats().invoke_attempts_total < concurrency as u64 {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("not every browser invoke started before abort");
                }
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(abort_after_ms)).await;
            for invoke in &invokes {
                invoke.abort();
            }
            for invoke in invokes {
                let _ = invoke.await;
            }
            let reply = pool
                .invoke(&endpoint_id, &addr, &digest, &request)
                .await?;
            println!("BROWSER_ABORT_RECOVERY_REPLY:{}", String::from_utf8_lossy(&reply));
            println!("BROWSER_ABORT_RECOVERY_TRUNKS:{}", pool.trunk_count().await);
            println!("BROWSER_POOL_STATS:{}", serde_json::to_string(&pool.stats())?);
            return Ok(());
        }
        let mut replies = Vec::with_capacity(concurrency);
        let mut errors = Vec::new();
        for invoke in invokes {
            match invoke.await? {
                Ok(reply) => replies.push(reply),
                Err(error) => errors.push(error),
            }
        }
        let stats = pool.stats();
        println!("BROWSER_INVOKE_ELAPSED_MS:{}", started.elapsed().as_millis());
        println!("BROWSER_POOL_STATS:{}", serde_json::to_string(&stats)?);
        if std::env::var("HIVE_BROWSER_EXPECT_ERROR").as_deref() == Ok("1") {
            println!("BROWSER_INVOKE_ERROR_COUNT:{}", errors.len());
            println!(
                "BROWSER_INVOKE_ERROR_SENT_COUNT:{}",
                errors.iter().filter(|error| error.sent).count()
            );
            println!(
                "BROWSER_INVOKE_ERROR:{}",
                errors.first().map(|error| error.message.as_str()).unwrap_or("")
            );
            if replies.is_empty() && errors.len() == concurrency {
                pool.close_endpoint(&endpoint_id).await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                return Ok(());
            }
            anyhow::bail!(
                "expected {} browser errors, got {} errors and {} replies",
                concurrency,
                errors.len(),
                replies.len()
            );
        }
        if let Some(error) = errors.into_iter().next() {
            return Err(error.into());
        }
        println!("BROWSER_INVOKE_COUNT:{}", replies.len());
        println!(
            "BROWSER_INVOKE_REPLY:{}",
            replies
                .first()
                .map(|reply| String::from_utf8_lossy(reply).into_owned())
                .unwrap_or_default()
        );
        return Ok(());
    }
    println!(
        "NATIVE_ADDR_JSON:{}",
        hive_p2p::addr_json(&ep).unwrap_or_default()
    );
    // serve_tunnels(local_http, ...) needs SOME local_http target for HIVE_ALPN
    // traffic; irrelevant here since only hive/browser/0 is exercised.
    hive_p2p::serve_tunnels(ep, "http://127.0.0.1:1".into(), 32, None, None).await;
    Ok(())
}
