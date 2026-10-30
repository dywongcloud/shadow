//! One-shot helper: mints a real admissible `POST /v1/browser/admissions` body (real ed25519
//! keypair, real proof-of-possession over `"{endpoint_id}:{challenge_ms}"`) as ready-to-curl JSON.
//! `MINT_SECRET_HEX` mints for an EXISTING browser identity, `MINT_RELAY_URL` supplies the relay hint
//! for the synthesized `addr_json`. `cargo run --example mint_browser_admission`.

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (endpoint_id, addr_json, secret) = match std::env::var("MINT_SECRET_HEX") {
        Ok(hex) => {
            let Some(seed) = from_hex(&hex) else {
                anyhow::bail!("MINT_SECRET_HEX must be 64 hex chars (32 bytes)");
            };
            let secret = iroh::SecretKey::from_bytes(&seed);
            let endpoint_id = secret.public().to_string();
            let relay = std::env::var("MINT_RELAY_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3341".to_string());
            let url: iroh::RelayUrl = relay.parse()?;
            let addr =
                iroh::EndpointAddr::from_parts(secret.public(), [iroh::TransportAddr::Relay(url)]);
            let addr_json = serde_json::to_string(&addr)?;
            (endpoint_id, addr_json, secret)
        }
        Err(_) => {
            let ep = hive_p2p::bind_full(None, &[], &[], false).await?;
            ep.online().await;
            let addr_json = hive_p2p::addr_json(&ep).expect("addr_json");
            let endpoint_id =
                hive_p2p::endpoint_id_from_addr_json(&addr_json).expect("endpoint_id");
            let secret = ep.secret_key().clone();
            (endpoint_id, addr_json, secret)
        }
    };
    let challenge_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let message = format!("{endpoint_id}:{challenge_ms}");
    let sig = secret.sign(message.as_bytes());
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
