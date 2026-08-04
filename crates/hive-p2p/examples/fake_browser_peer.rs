//! Live-witness harness for `bn-invoke-http-envelope-validation` /
//! `bn-invoke-target-lifecycle`: a real hive-p2p endpoint that accepts
//! `hive/browser/0` connections and answers `Op::Invoke` with a caller-
//! controlled reply body, so fluid-gateway's REAL `browser_response`
//! envelope validation (crates/fluid-gateway/src/lib.rs) can be exercised
//! end-to-end against real bytes on the wire instead of a mock. Not part of
//! any production binary — same category as `browser_echo_native.rs`.
//!
//! Usage: `HIVE_FAKE_BROWSER_REPLY_FILE=/path/to/bytes cargo run --example
//! fake_browser_peer` — prints `FAKE_BROWSER_ADDR_JSON:<json>` then serves
//! forever, replying to every Op::Invoke with the exact bytes in that file
//! (read once, so the caller can rewrite it between requests to change
//! behavior on the next accepted stream isn't needed — one process per
//! reply shape, matching how the actual test drives this: start, invoke
//! once, kill, restart with a different reply file for the next case).
//!
//! Set `HIVE_FAKE_BROWSER_DROP_AFTER_READ=1` to read and log the complete
//! request, then reset the stream without writing a response. This is the
//! red-capable live witness for `BROWSER_EXECUTION_UNCERTAIN`: the platform
//! has proof the mutation bytes reached browser-owned execution but no proof
//! whether that execution committed before the response path failed.

use hive_browser_proto::{check_len, split_request};
use hive_p2p::BROWSER_ALPN;

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let reply_path = std::env::var("HIVE_FAKE_BROWSER_REPLY_FILE")
        .expect("HIVE_FAKE_BROWSER_REPLY_FILE must name a file containing the raw reply bytes");
    let reply = std::fs::read(&reply_path)?;
    let drop_after_read = std::env::var("HIVE_FAKE_BROWSER_DROP_AFTER_READ")
        .map(|value| matches!(value.as_str(), "1" | "true"))
        .unwrap_or(false);
    let reply_delay_ms = std::env::var("HIVE_FAKE_BROWSER_REPLY_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let read_delay_ms = std::env::var("HIVE_FAKE_BROWSER_READ_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let reply_mode = std::env::var("HIVE_FAKE_BROWSER_REPLY_MODE")
        .unwrap_or_else(|_| "normal".to_string());

    let ep = hive_p2p::bind_full(None, &[], &[], false).await?;
    ep.online().await;
    let addr_json = hive_p2p::addr_json(&ep).unwrap_or_default();
    let endpoint_id = hive_p2p::endpoint_id_from_addr_json(&addr_json)
        .ok_or_else(|| anyhow::anyhow!("fake browser addr has no endpoint id"))?;
    let challenge_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let message = format!("{endpoint_id}:{challenge_ms}");
    let signature = to_hex(&ep.secret_key().sign(message.as_bytes()).to_bytes());
    println!("FAKE_BROWSER_ADDR_JSON:{addr_json}");
    println!("FAKE_BROWSER_ENDPOINT_ID:{endpoint_id}");
    println!("FAKE_BROWSER_ADMISSION_CHALLENGE_MS:{challenge_ms}");
    println!("FAKE_BROWSER_ADMISSION_SIGNATURE:{signature}");

    loop {
        eprintln!("fake_browser_peer: waiting for a connection...");
        let conn = match ep.accept().await {
            Some(incoming) => {
                eprintln!("fake_browser_peer: incoming connection, awaiting handshake...");
                match incoming.await {
                    Ok(conn) => {
                        eprintln!(
                            "fake_browser_peer: handshake complete, alpn={:?}",
                            conn.alpn()
                        );
                        conn
                    }
                    Err(e) => {
                        eprintln!("fake_browser_peer: handshake failed: {e}");
                        continue;
                    }
                }
            }
            None => {
                eprintln!("fake_browser_peer: endpoint closed, exiting accept loop");
                break;
            }
        };
        if conn.alpn() != BROWSER_ALPN {
            eprintln!("fake_browser_peer: non-browser ALPN, skipping");
            continue;
        }
        let reply = reply.clone();
        let reply_mode = reply_mode.clone();
        tokio::spawn(async move {
            eprintln!("fake_browser_peer: entering accept_bi loop");
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                eprintln!("fake_browser_peer: accepted a bi-stream");
                let reply = reply.clone();
                let reply_mode = reply_mode.clone();
                tokio::spawn(async move {
                    if read_delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(read_delay_ms)).await;
                    }
                    // Wire framing is [u32 LITTLE-endian len][...] per
                    // hive-browser-proto::{check_len,encode_request} -- reusing
                    // the real check_len/split_request here (not a hand-rolled
                    // big-endian parse) is what caught this: a first attempt
                    // with from_be_bytes misread the length prefix entirely
                    // and the stream read failed before ever reaching the op
                    // byte. Live, witnessed bug in the TEST HARNESS, not in
                    // production code (which already uses check_len).
                    let mut lenb = [0u8; 4];
                    if let Err(e) = recv.read_exact(&mut lenb).await {
                        eprintln!("fake_browser_peer: failed reading length prefix: {e}");
                        return;
                    }
                    let len = match check_len(lenb) {
                        Ok(n) => n,
                        Err(e) => {
                            eprintln!("fake_browser_peer: bad length prefix: {e}");
                            return;
                        }
                    };
                    let mut buf = vec![0u8; len];
                    if let Err(e) = recv.read_exact(&mut buf).await {
                        eprintln!("fake_browser_peer: failed reading payload ({len} bytes): {e}");
                        return;
                    }
                    let (op, payload) = match split_request(&buf) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("fake_browser_peer: bad request frame: {e}");
                            return;
                        }
                    };
                    eprintln!(
                        "fake_browser_peer: got op={op:?}, payload_len={}",
                        payload.len()
                    );
                    eprintln!(
                        "fake_browser_peer: payload={}",
                        String::from_utf8_lossy(payload)
                    );
                    let mut trailing = [0u8; 1];
                    match recv.read(&mut trailing).await {
                        Ok(None) => {}
                        Ok(Some(_)) => {
                            eprintln!("fake_browser_peer: request has trailing bytes");
                            let _ = send.reset(hive_browser_proto::reset::MALFORMED_PAYLOAD.into());
                            let _ = recv.stop(hive_browser_proto::reset::MALFORMED_PAYLOAD.into());
                            return;
                        }
                        Err(error) => {
                            eprintln!("fake_browser_peer: request EOF failed: {error}");
                            return;
                        }
                    }
                    if reply_delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(reply_delay_ms)).await;
                    }
                    if drop_after_read {
                        eprintln!("fake_browser_peer: resetting stream after full request read");
                        let _ = send.reset(0u8.into());
                        return;
                    }
                    // Reply verbatim with the configured bytes regardless of op —
                    // the diagnostic controls malformed/truncated variants with
                    // HIVE_FAKE_BROWSER_REPLY_MODE.
                    let framed = hive_browser_proto::encode_reply(&reply);
                    let bytes = match reply_mode.as_str() {
                        "normal" | "trailing" => framed.as_slice(),
                        "prefix-truncated" => &framed[..framed.len().min(2)],
                        "body-truncated" => &framed[..(4 + reply.len() / 2).min(framed.len())],
                        mode => {
                            eprintln!("fake_browser_peer: unknown reply mode {mode}");
                            return;
                        }
                    };
                    match send.write_all(bytes).await {
                        Ok(()) => eprintln!(
                            "fake_browser_peer: wrote {} reply bytes in mode {}",
                            bytes.len(),
                            reply_mode
                        ),
                        Err(e) => {
                            eprintln!("fake_browser_peer: failed writing reply: {e}");
                            return;
                        }
                    }
                    if reply_mode == "trailing" {
                        if let Err(error) = send.write_all(&[0xa5]).await {
                            eprintln!("fake_browser_peer: failed writing trailing byte: {error}");
                            return;
                        }
                    }
                    match send.finish() {
                        Ok(()) => eprintln!("fake_browser_peer: reply complete"),
                        Err(error) => eprintln!("fake_browser_peer: reply finish failed: {error}"),
                    }
                });
            }
            eprintln!("fake_browser_peer: accept_bi loop ended (connection closed)");
        });
    }
    Ok(())
}
