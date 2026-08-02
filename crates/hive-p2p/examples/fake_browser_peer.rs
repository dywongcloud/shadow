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

use hive_p2p::BROWSER_ALPN;
use hive_browser_proto::{check_len, split_request};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let reply_path = std::env::var("HIVE_FAKE_BROWSER_REPLY_FILE")
        .expect("HIVE_FAKE_BROWSER_REPLY_FILE must name a file containing the raw reply bytes");
    let reply = std::fs::read(&reply_path)?;

    let ep = hive_p2p::bind_full(None, &[], &[], false).await?;
    ep.online().await;
    println!(
        "FAKE_BROWSER_ADDR_JSON:{}",
        hive_p2p::addr_json(&ep).unwrap_or_default()
    );

    loop {
        eprintln!("fake_browser_peer: waiting for a connection...");
        let conn = match ep.accept().await {
            Some(incoming) => {
                eprintln!("fake_browser_peer: incoming connection, awaiting handshake...");
                match incoming.await {
                    Ok(conn) => {
                        eprintln!("fake_browser_peer: handshake complete, alpn={:?}", conn.alpn());
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
        tokio::spawn(async move {
            eprintln!("fake_browser_peer: entering accept_bi loop");
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                eprintln!("fake_browser_peer: accepted a bi-stream");
                let reply = reply.clone();
                tokio::spawn(async move {
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
                    eprintln!("fake_browser_peer: got op={op:?}, payload_len={}", payload.len());
                    eprintln!(
                        "fake_browser_peer: payload={}",
                        String::from_utf8_lossy(payload)
                    );
                    // Reply verbatim with the configured bytes regardless of op —
                    // the test controls op-specific behavior via which reply file
                    // it starts this process with. encode_reply matches the
                    // real wire format (le length, no op byte on the reply).
                    let framed = hive_browser_proto::encode_reply(&reply);
                    match send.write_all(&framed).await {
                        Ok(()) => eprintln!("fake_browser_peer: wrote {} framed reply bytes", framed.len()),
                        Err(e) => {
                            eprintln!("fake_browser_peer: failed writing reply: {e}");
                            return;
                        }
                    }
                    let _ = send.finish();
                    eprintln!("fake_browser_peer: reply complete");
                });
            }
            eprintln!("fake_browser_peer: accept_bi loop ended (connection closed)");
        });
    }
    Ok(())
}
