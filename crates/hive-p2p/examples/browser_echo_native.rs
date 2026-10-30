//! Live-witness harness: bind a real hive-p2p endpoint (both ALPNs via `bind_full`), print its
//! `addr_json`, and serve `hive/tunnel/0` + `hive/browser/0` until killed, or drive one of the
//! browser-node audit scenarios named by its first argument. Serve:
//! `HIVE_RELAY_URLS=http://<relay-ip>:3340 cargo run --example browser_echo_native`; invoke a
//! browser with `HIVE_BROWSER_ADDR`, `HIVE_BROWSER_DIGEST`, `HIVE_BROWSER_REQUEST`.

use hive_browser_proto::{encode_request, Op};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let key_path = std::env::var("HIVE_BROWSER_CLIENT_KEY")
        .ok()
        .map(std::path::PathBuf::from);
    let ep = hive_p2p::bind_full(key_path, &[], &[], false).await?;
    match tokio::time::timeout(std::time::Duration::from_secs(20), ep.online()).await {
        Ok(()) => println!("NATIVE_ONLINE:home-relay"),
        Err(_) => println!("NATIVE_ONLINE:timeout(no home relay selected)"),
    }
    println!("NATIVE_ID:{}", ep.id());
    if std::env::var("HIVE_BROWSER_PRINT_ID").as_deref() == Ok("1") {
        return Ok(());
    }
    if let (Ok(addr), Ok(mode)) = (
        std::env::var("HIVE_BROWSER_RAW_ADDR"),
        std::env::var("HIVE_BROWSER_RAW_MODE"),
    ) {
        let addr: iroh::EndpointAddr = serde_json::from_str(&addr)?;
        let wait_ms = std::env::var("HIVE_BROWSER_RAW_WAIT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(15_000);
        let wait = std::time::Duration::from_millis(wait_ms);
        match mode.as_str() {
            "stop-reading" => {
                let conn = ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?;
                let (mut send, mut recv) = conn.open_bi().await?;
                let frame = if std::env::var("HIVE_BROWSER_RAW_OP").as_deref() == Ok("invoke") {
                    let payload = hive_browser_proto::encode_invoke(&"a".repeat(64), "{}")
                        .expect("64-hex digest is valid");
                    encode_request(Op::Invoke, &payload)
                } else {
                    encode_request(Op::Echo, b"stop-reading-witness")
                };
                send.write_all(&frame).await?;
                send.finish()?;
                recv.stop(5u32.into())?;
                let stopped = tokio::time::timeout(wait, send.stopped()).await??;
                println!(
                    "STOP_READING_STOPPED_CODE:{}",
                    stopped.map(|code| code.into_inner()).unwrap_or_default()
                );
                return Ok(());
            }
            "one-mib-workload" => {
                let streams = std::env::var("HIVE_BROWSER_RAW_WORKLOAD_STREAMS")
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(8);
                let request_path = std::env::var("HIVE_BROWSER_REQUEST_FILE")
                    .expect("workload needs HIVE_BROWSER_REQUEST_FILE");
                let request_json = std::fs::read_to_string(request_path)?;
                let digest =
                    std::env::var("HIVE_BROWSER_DIGEST").unwrap_or_else(|_| "a".repeat(64));
                let payload = hive_browser_proto::encode_invoke(&digest, &request_json)?;
                let frame = encode_request(Op::Invoke, &payload);
                println!("WORKLOAD_FRAME_BYTES:{}", frame.len());
                let conn = ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?;
                let t0 = std::time::Instant::now();
                let mut tasks = Vec::new();
                for index in 0..streams {
                    let conn = conn.clone();
                    let frame = frame.clone();
                    tasks.push(tokio::spawn(async move {
                        let stage_timeout = std::time::Duration::from_secs(90);
                        let result: anyhow::Result<String> = async {
                            let (mut send, mut recv) = conn.open_bi().await?;
                            send.write_all(&frame).await?;
                            send.finish()?;
                            let mut lenb = [0u8; 4];
                            tokio::time::timeout(stage_timeout, recv.read_exact(&mut lenb))
                                .await??;
                            let len = hive_browser_proto::check_len(lenb)?;
                            let mut body = vec![0u8; len];
                            tokio::time::timeout(stage_timeout, recv.read_exact(&mut body))
                                .await??;
                            let mut trailing = [0u8; 1];
                            let eof = tokio::time::timeout(stage_timeout, recv.read(&mut trailing))
                                .await?;
                            match eof {
                                Ok(None) => {}
                                Ok(Some(_)) => anyhow::bail!("reply had trailing bytes"),
                                Err(error) => anyhow::bail!("reply eof read failed: {error}"),
                            }
                            Ok(String::from_utf8_lossy(&body).into_owned())
                        }
                        .await;
                        (index, result)
                    }));
                }
                let mut ok = 0usize;
                for task in tasks {
                    match task.await? {
                        (index, Ok(reply)) => {
                            ok += 1;
                            println!("WORKLOAD_STREAM_{index}_OK:{}", reply.len());
                        }
                        (index, Err(error)) => {
                            println!("WORKLOAD_STREAM_{index}_ERR:{error}");
                        }
                    }
                }
                println!("WORKLOAD_OK:{ok}/{streams}");
                println!("WORKLOAD_ELAPSED_MS:{}", t0.elapsed().as_millis());
                match conn.close_reason() {
                    Some(reason) => println!("WORKLOAD_CONN_CLOSED:{reason:?}"),
                    None => println!("WORKLOAD_CONN_CLOSED:none"),
                }
                return Ok(());
            }
            "echo" => {
                let conn = ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?;
                let body = echo_roundtrip(&conn).await?;
                println!("ECHO_REPLY:{}", String::from_utf8_lossy(&body));
                return Ok(());
            }
            "conn-cap" => {
                let mut conns = Vec::new();
                for _ in 0..4 {
                    conns.push(ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?);
                }
                println!("CONN_CAP_HELD:{}", conns.len());
                let fifth = ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?;
                let reason =
                    tokio::time::timeout(std::time::Duration::from_millis(wait_ms), fifth.closed())
                        .await;
                match reason {
                    Ok(iroh::endpoint::ConnectionError::ApplicationClosed(frame)) => {
                        println!(
                            "CONN_CAP_FIFTH_CLOSE_CODE:{}",
                            frame.error_code.into_inner()
                        );
                        println!(
                            "CONN_CAP_FIFTH_CLOSE_REASON:{}",
                            String::from_utf8_lossy(&frame.reason)
                        );
                    }
                    Ok(other) => println!("CONN_CAP_FIFTH_CLOSE_OTHER:{other:?}"),
                    Err(_) => println!("CONN_CAP_FIFTH_CLOSE:pending"),
                }
                let body = echo_roundtrip(&conns[0]).await?;
                println!(
                    "CONN_CAP_HELD_ECHO_REPLY:{}",
                    String::from_utf8_lossy(&body)
                );
                return Ok(());
            }
            "echo-idle-watch" => {
                let conn = ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?;
                echo_roundtrip(&conn).await?;
                println!("IDLE_WATCH_REPLY:ok");
                let t0 = std::time::Instant::now();
                let reason = conn.closed().await;
                let ms = t0.elapsed().as_millis();
                match reason {
                    iroh::endpoint::ConnectionError::ApplicationClosed(frame) => {
                        println!("IDLE_CLOSE_CODE:{}", frame.error_code.into_inner());
                        println!(
                            "IDLE_CLOSE_REASON:{}",
                            String::from_utf8_lossy(&frame.reason)
                        );
                    }
                    other => println!("IDLE_CLOSE_OTHER:{other:?}"),
                }
                println!("IDLE_CLOSE_MS:{ms}");
                let conn2 = ep.connect(addr, hive_p2p::BROWSER_ALPN).await?;
                echo_roundtrip(&conn2).await?;
                println!("IDLE_RECONNECT_REPLY:ok");
                return Ok(());
            }
            "stall-streams" => {
                let stall_count = std::env::var("HIVE_BROWSER_RAW_STALL_STREAMS")
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(8);
                let hold_ms = std::env::var("HIVE_BROWSER_RAW_STALL_HOLD_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(30_000);
                let conn = ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?;
                let mut held = Vec::new();
                for _ in 0..stall_count {
                    let (mut send, recv) = conn.open_bi().await?;
                    send.write_all(&[3, 0, 0, 0, 0, b'x']).await?;
                    held.push((send, recv));
                }
                println!("STALL_STREAMS_OPEN:{}", held.len());
                if std::env::var("HIVE_BROWSER_RAW_STALL_PROBE").as_deref() == Ok("1") {
                    let conn2 = ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?;
                    let (mut probe_send, mut probe_recv) = conn2.open_bi().await?;
                    let frame = encode_request(Op::Echo, b"probe");
                    probe_send.write_all(&frame).await?;
                    probe_send.finish()?;
                    let stopped = tokio::time::timeout(wait, probe_send.stopped()).await??;
                    let reset = tokio::time::timeout(wait, probe_recv.received_reset()).await??;
                    println!(
                        "STALL_PROBE_STOPPED_CODE:{}",
                        stopped.map(|code| code.into_inner()).unwrap_or_default()
                    );
                    println!(
                        "STALL_PROBE_RESET_CODE:{}",
                        reset.map(|code| code.into_inner()).unwrap_or_default()
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                for (index, (mut send, _recv)) in held.into_iter().enumerate() {
                    let code = tokio::time::timeout(
                        std::time::Duration::from_millis(1_000),
                        send.stopped(),
                    )
                    .await;
                    match code {
                        Ok(Ok(stopped)) => println!(
                            "STALL_STREAM_{index}_STOPPED:{}",
                            stopped.map(|c| c.into_inner()).unwrap_or_default()
                        ),
                        Ok(Err(error)) => println!("STALL_STREAM_{index}_STOPPED_ERR:{error}"),
                        Err(_) => println!("STALL_STREAM_{index}_STOPPED:pending"),
                    }
                    let _ = send.reset(0u32.into());
                }
                println!("STALL_STREAMS_RELEASED");
                tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
                match echo_roundtrip(&conn).await {
                    Ok(body) => {
                        println!("STALL_POST_RELEASE_ECHO:{}", String::from_utf8_lossy(&body))
                    }
                    Err(error) => println!("STALL_POST_RELEASE_ECHO_ERR:{error}"),
                }
                return Ok(());
            }
            "echo-load" => {
                let duration_ms = std::env::var("HIVE_BROWSER_RAW_LOAD_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(12_000);
                let payload_len = std::env::var("HIVE_BROWSER_RAW_LOAD_PAYLOAD")
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(65_536);
                let payload = vec![0x5au8; payload_len];
                let pace_ms = std::env::var("HIVE_BROWSER_RAW_LOAD_PACE_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0);
                let mut conns = Vec::new();
                for _ in 0..4 {
                    conns.push(ep.connect(addr.clone(), hive_p2p::BROWSER_ALPN).await?);
                }
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(duration_ms);
                let mut opened = 0u64;
                let mut tasks = Vec::new();
                while std::time::Instant::now() < deadline {
                    let conn = &conns[(opened as usize) % conns.len()];
                    let stream = match tokio::time::timeout(
                        std::time::Duration::from_millis(2_000),
                        conn.open_bi(),
                    )
                    .await
                    {
                        Ok(Ok(stream)) => stream,
                        _ => continue,
                    };
                    let (mut send, recv) = stream;
                    let frame = encode_request(Op::Echo, &payload);
                    tasks.push(tokio::spawn(async move {
                        let wrote = send.write_all(&frame).await.is_ok() && send.finish().is_ok();
                        let stopped = send.stopped().await;
                        drop(recv);
                        (wrote, stopped.ok().flatten().map(|code| code.into_inner()))
                    }));
                    opened += 1;
                    if pace_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(pace_ms)).await;
                    }
                }
                let mut completed = 0u64;
                let mut overloaded = 0u64;
                let mut other_codes = Vec::new();
                for task in tasks {
                    if let Ok((wrote, code)) = task.await {
                        if wrote {
                            completed += 1;
                        }
                        match code {
                            Some(8) => overloaded += 1,
                            Some(code) => other_codes.push(code),
                            None => {}
                        }
                    }
                }
                println!("ECHO_LOAD_STREAMS_OPENED:{opened}");
                println!("ECHO_LOAD_STREAMS_WROTE:{completed}");
                println!("ECHO_LOAD_OVERLOADED:{overloaded}");
                println!("ECHO_LOAD_OTHER_CODES:{other_codes:?}");
                return Ok(());
            }
            _ => {}
        }
        let conn = ep.connect(addr, hive_p2p::BROWSER_ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let (bytes, finish) = match mode.as_str() {
            "trailing" => {
                let mut frame = encode_request(Op::Echo, b"x");
                frame.push(0xa5);
                (frame, true)
            }
            "prefix-truncated" => (vec![2, 0], true),
            "prefix-only" => (3u32.to_le_bytes().to_vec(), true),
            "op-only-truncated" => {
                let mut frame = 3u32.to_le_bytes().to_vec();
                frame.push(Op::Echo.as_byte());
                (frame, true)
            }
            "zero-len" => (0u32.to_le_bytes().to_vec(), true),
            "oversized" => {
                let mut frame = ((hive_browser_proto::BROWSER_MAX_FRAME + 1) as u32)
                    .to_le_bytes()
                    .to_vec();
                frame.push(Op::Echo.as_byte());
                (frame, true)
            }
            "unknown-op" => {
                let mut frame = 1u32.to_le_bytes().to_vec();
                frame.push(9u8);
                (frame, true)
            }
            "invoke-op" => {
                let payload = hive_browser_proto::encode_invoke(&"0".repeat(64), "{}")
                    .expect("64-hex zero digest is valid");
                (encode_request(Op::Invoke, &payload), true)
            }
            "echo-oversize" => {
                let payload = vec![0x41u8; hive_browser_proto::BROWSER_MAX_ECHO + 1];
                (encode_request(Op::Echo, &payload), true)
            }
            "body-truncated" | "body-stall" => {
                let mut frame = 3u32.to_le_bytes().to_vec();
                frame.extend_from_slice(&[Op::Echo.as_byte(), b'x']);
                (frame, mode != "body-stall")
            }
            _ => anyhow::bail!("unknown HIVE_BROWSER_RAW_MODE {mode}"),
        };
        if let Err(error) = send.write_all(&bytes).await {
            println!("BROWSER_RAW_WRITE_ERROR:{error}");
        }
        if finish {
            let _ = send.finish();
        }
        let stopped = tokio::time::timeout(wait, send.stopped()).await??;
        let reset = tokio::time::timeout(wait, recv.received_reset()).await??;
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
        println!(
            "BROWSER_POOL_STATS:{}",
            serde_json::to_string(&pool.stats())?
        );
        return Ok(());
    }
    let request_from_file = match std::env::var("HIVE_BROWSER_REQUEST_FILE") {
        Ok(path) => Some(std::fs::read_to_string(path)?),
        Err(_) => None,
    };
    let request_var = std::env::var("HIVE_BROWSER_REQUEST")
        .ok()
        .or(request_from_file);
    if let (Ok(addr), Ok(digest), Some(request)) = (
        std::env::var("HIVE_BROWSER_ADDR"),
        std::env::var("HIVE_BROWSER_DIGEST"),
        request_var,
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
            println!(
                "BROWSER_UNRELATED_CLOSE_REPLY:{}",
                String::from_utf8_lossy(&reply)
            );
            println!(
                "BROWSER_POOL_STATS:{}",
                serde_json::to_string(&pool.stats())?
            );
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
            let reply = pool.invoke(&endpoint_id, &addr, &digest, &request).await?;
            println!(
                "BROWSER_ABORT_RECOVERY_REPLY:{}",
                String::from_utf8_lossy(&reply)
            );
            println!("BROWSER_ABORT_RECOVERY_TRUNKS:{}", pool.trunk_count().await);
            println!(
                "BROWSER_POOL_STATS:{}",
                serde_json::to_string(&pool.stats())?
            );
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
        println!(
            "BROWSER_INVOKE_ELAPSED_MS:{}",
            started.elapsed().as_millis()
        );
        println!("BROWSER_POOL_STATS:{}", serde_json::to_string(&stats)?);
        if std::env::var("HIVE_BROWSER_EXPECT_ERROR").as_deref() == Ok("1") {
            println!("BROWSER_INVOKE_ERROR_COUNT:{}", errors.len());
            println!(
                "BROWSER_INVOKE_ERROR_SENT_COUNT:{}",
                errors.iter().filter(|error| error.sent).count()
            );
            println!(
                "BROWSER_INVOKE_ERROR:{}",
                errors
                    .first()
                    .map(|error| error.message.as_str())
                    .unwrap_or("")
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
        if std::env::var("HIVE_BROWSER_PRINT_LEN").as_deref() == Ok("1") {
            for reply in &replies {
                println!("BROWSER_INVOKE_REPLY_LEN:{}", reply.len());
            }
        }
        println!(
            "BROWSER_INVOKE_REPLY:{}",
            replies
                .first()
                .map(|reply| {
                    if std::env::var("HIVE_BROWSER_PRINT_LEN").as_deref() == Ok("1") {
                        format!("<{} bytes>", reply.len())
                    } else {
                        String::from_utf8_lossy(reply).into_owned()
                    }
                })
                .unwrap_or_default()
        );
        return Ok(());
    }
    println!(
        "NATIVE_ADDR_JSON:{}",
        hive_p2p::addr_json(&ep).unwrap_or_default()
    );
    hive_p2p::serve_tunnels(ep, "http://127.0.0.1:1".into(), 32, None, None).await;
    Ok(())
}

/// One complete valid echo request/response on a fresh bi stream of `conn`.
async fn echo_roundtrip(conn: &iroh::endpoint::Connection) -> anyhow::Result<Vec<u8>> {
    let (mut send, mut recv) = conn.open_bi().await?;
    let frame = encode_request(Op::Echo, b"idle-watch");
    send.write_all(&frame).await?;
    send.finish()?;
    let mut lenb = [0u8; 4];
    recv.read_exact(&mut lenb).await?;
    let len = hive_browser_proto::check_len(lenb)?;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(body)
}
