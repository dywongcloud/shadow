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
    if let (Ok(addr), Ok(digest), Ok(request)) = (
        std::env::var("HIVE_BROWSER_ADDR"),
        std::env::var("HIVE_BROWSER_DIGEST"),
        std::env::var("HIVE_BROWSER_REQUEST"),
    ) {
        let endpoint_id = hive_p2p::endpoint_id_from_addr_json(&addr)
            .ok_or_else(|| anyhow::anyhow!("HIVE_BROWSER_ADDR has no endpoint id"))?;
        let pool = hive_p2p::BrowserPool::new(ep);
        match pool.invoke(&endpoint_id, &addr, &digest, &request).await {
            Ok(reply) => {
                println!("BROWSER_INVOKE_REPLY:{}", String::from_utf8_lossy(&reply));
                return Ok(());
            }
            Err(error) => anyhow::bail!("{error}"),
        }
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
