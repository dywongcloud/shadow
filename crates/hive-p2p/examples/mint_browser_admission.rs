//! One-shot live-witness helper for bn-p2p-revocation-latency: mints a real
//! admissible `POST /v1/browser/admissions` body (real ed25519 keypair, real
//! proof-of-possession signature over `"{endpoint_id}:{challenge_ms}"`,
//! signed exactly like `hive_browser::BrowserNode::signAdmission` /
//! `run-node-worker.js`'s `admitOnce`) and prints it as ready-to-curl JSON so
//! a real leader-side revoke -> follower fan-out round trip can be exercised
//! against a real local hive-cloud pair without a browser. Not part of any
//! production binary — same category as `fake_browser_peer.rs`.
//!
//! Usage: `cargo run --example mint_browser_admission` — prints one JSON line.

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ep = hive_p2p::bind_full(None, &[], &[], false).await?;
    ep.online().await;
    let addr_json = hive_p2p::addr_json(&ep).expect("addr_json");
    let endpoint_id = hive_p2p::endpoint_id_from_addr_json(&addr_json).expect("endpoint_id");
    let challenge_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let message = format!("{endpoint_id}:{challenge_ms}");
    let sig = ep.secret_key().sign(message.as_bytes());
    let signature = to_hex(&sig.to_bytes());

    let body = serde_json::json!({
        "endpoint_id": endpoint_id,
        "addr_json": addr_json,
        "deployment": std::env::var("MINT_DEPLOYMENT").unwrap_or_default(),
        "function": std::env::var("MINT_FUNCTION").unwrap_or_default(),
        "digest": std::env::var("MINT_DIGEST").unwrap_or_default(),
        "lease_secs": 300,
        "scope": "team",
        "protocol_version": hive_browser_proto::BROWSER_PROTOCOL_VERSION,
        "challenge_ms": challenge_ms,
        "signature": signature,
    });
    println!("MINT_ADMISSION_JSON:{body}");
    println!("MINT_ENDPOINT_ID:{endpoint_id}");
    Ok(())
}
