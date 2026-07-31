//! hive-browser — a SHADW mesh node that runs inside a browser tab.
//!
//! This is the wasm32 networking scaffold (PRD `bn-impl-crate-scaffold`). It
//! boots a real iroh `Endpoint` over the browser's relay-only WebSocket
//! transport, publishes/resolves identity through a pkarr relay, and accepts
//! inbound connections on its OWN ALPN (`hive/browser/0`) — the dedicated,
//! never-trusted surface the design (`docs/browser-node-proposal.md` §2.8)
//! keeps structurally disjoint from the fleet control plane. There are NO
//! gossip/join/raw arms here by construction; the only thing this handler does
//! is a length-prefixed request/response echo, which is enough to prove that a
//! browser tab is a real bidirectional iroh peer.
//!
//! Everything the browser cannot do (UDP, hole-punching) is absent by the same
//! compile-time cfg iroh itself uses; all traffic rides a relay WebSocket and
//! stays end-to-end encrypted (the relay cannot decrypt it).

use std::sync::Arc;

use iroh::{Endpoint, RelayMap, RelayMode, RelayUrl, SecretKey};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// The browser node's dedicated ALPN. Intentionally distinct from
/// `hive/tunnel/0` (the fleet data/control ALPN) so a browser peer can never
/// be one byte from a control-plane stream mode — the fleet-side accept path
/// dispatches per-ALPN and this one has no privileged arms.
pub const BROWSER_ALPN: &[u8] = b"hive/browser/0";

/// Cap on a single echoed request frame — also the memory-safety line for a tab
/// serving other peers' traffic (an unbounded `read_to_end` is a DoS lever).
const MAX_FRAME: usize = 1 << 20; // 1 MiB

#[wasm_bindgen(start)]
pub fn on_load() {
    console_error_panic_hook::set_once();
    // Best-effort structured logs to the devtools console; harmless if it fails.
    let _ = std::panic::catch_unwind(|| tracing_wasm::set_as_global_default());
}

/// A live browser mesh node. Holds the bound endpoint and the spawned accept
/// loop for its lifetime; dropping it tears the endpoint down.
#[wasm_bindgen]
pub struct BrowserNode {
    ep: Endpoint,
    /// The relay URL this node was booted against (reported in status; the live
    /// home-relay is negotiated asynchronously and not needed for display).
    relay: String,
    /// Count of echo requests served so far — surfaced to the page as a liveness
    /// signal that the inbound accept path actually fired.
    served: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Serialize)]
struct Status {
    node_id: String,
    relay: String,
    served: u64,
    addr_json: String,
}

#[wasm_bindgen]
impl BrowserNode {
    /// Boot a node: bind an endpoint against `relay_url` (must be `wss://` from
    /// an https page — plain `ws://` is mixed-content-blocked), optionally wire
    /// a pkarr `discovery_url` for publish+resolve, and restore identity from
    /// `secret_hex` (32-byte ed25519 seed, hex) if given — else generate a fresh
    /// one. Spawns the `hive/browser/0` accept loop before returning.
    #[wasm_bindgen]
    pub async fn boot(
        relay_url: String,
        discovery_url: Option<String>,
        secret_hex: Option<String>,
    ) -> Result<BrowserNode, JsError> {
        let relay: RelayUrl = relay_url
            .parse()
            .map_err(|e| JsError::new(&format!("bad relay url {relay_url:?}: {e}")))?;
        let map = RelayMap::from_iter([relay.clone()]);

        // Minimal preset (no n0 pkarr/DNS baked in) + our own relay map, so the
        // browser only ever talks to the relay we hand it.
        let mut builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(RelayMode::Custom(map))
            .alpns(vec![BROWSER_ALPN.to_vec()]);

        // Identity: restore a persisted seed if the page gave us one, else the
        // caller is expected to read `secret back out` via a later export. A
        // stable seed ⇒ a stable EndpointId across reloads.
        if let Some(hex) = secret_hex.as_deref().filter(|h| !h.is_empty()) {
            let raw = hex_to_32(hex)
                .ok_or_else(|| JsError::new("secret_hex must be 64 hex chars (32 bytes)"))?;
            builder = builder.secret_key(SecretKey::from_bytes(&raw));
        }

        if let Some(url) = discovery_url.as_deref().filter(|u| !u.is_empty()) {
            let u = url::Url::parse(url)
                .map_err(|e| JsError::new(&format!("bad discovery url {url:?}: {e}")))?;
            builder = builder
                .address_lookup(iroh::address_lookup::PkarrPublisher::builder(u.clone()))
                .address_lookup(iroh::address_lookup::PkarrResolver::builder(u));
        }

        let ep = builder
            .bind()
            .await
            .map_err(|e| JsError::new(&format!("endpoint bind failed: {e}")))?;

        let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
        spawn_accept_loop(ep.clone(), served.clone());

        Ok(BrowserNode {
            ep,
            relay: relay.to_string(),
            served,
        })
    }

    /// The node's cryptographic identity (64-hex EndpointId = its ed25519
    /// public key). Stable across reloads iff booted from the same seed.
    #[wasm_bindgen(js_name = nodeId)]
    pub fn node_id(&self) -> String {
        self.ep.id().to_string()
    }

    /// Serialized `EndpointAddr` (id + relay/transport hints) a peer needs to
    /// dial this browser node.
    #[wasm_bindgen(js_name = addrJson)]
    pub fn addr_json(&self) -> String {
        serde_json::to_string(&self.ep.addr()).unwrap_or_default()
    }

    /// How many inbound echo requests this node has served — proof the accept
    /// path fired, readable from the page after a peer connects.
    #[wasm_bindgen(js_name = servedCount)]
    pub fn served_count(&self) -> u64 {
        self.served.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// One JSON blob of everything the status UI needs.
    #[wasm_bindgen(js_name = statusJson)]
    pub fn status_json(&self) -> String {
        serde_json::to_string(&Status {
            node_id: self.ep.id().to_string(),
            relay: self.relay.clone(),
            served: self.served_count(),
            addr_json: self.addr_json(),
        })
        .unwrap_or_default()
    }

    /// Outbound test: dial `peer_addr_json` on `hive/browser/0`, send `msg`, and
    /// return the echoed reply. Proves the browser node's OUTBOUND path (browser
    /// → relay → peer) in addition to the accept loop's inbound path.
    #[wasm_bindgen(js_name = echoTo)]
    pub async fn echo_to(&self, peer_addr_json: String, msg: String) -> Result<String, JsError> {
        let addr: iroh::EndpointAddr = serde_json::from_str(&peer_addr_json)
            .map_err(|e| JsError::new(&format!("bad peer addr_json: {e}")))?;
        let conn = self
            .ep
            .connect(addr, BROWSER_ALPN)
            .await
            .map_err(|e| JsError::new(&format!("connect failed: {e}")))?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| JsError::new(&format!("open_bi failed: {e}")))?;
        let bytes = msg.into_bytes();
        let len = (bytes.len() as u32).to_le_bytes();
        // Writer speaks first — open_bi is lazy, the peer never sees the stream
        // until bytes arrive.
        send.write_all(&len)
            .await
            .map_err(|e| JsError::new(&format!("write len: {e}")))?;
        send.write_all(&bytes)
            .await
            .map_err(|e| JsError::new(&format!("write body: {e}")))?;
        send.finish()
            .map_err(|e| JsError::new(&format!("finish: {e}")))?;
        // The peer's accept loop replies in the SAME [u32 len][bytes] framing it
        // reads requests in (see spawn_accept_loop) — read the length prefix
        // explicitly and take exactly that many bytes, mirroring the accept
        // side, rather than read_to_end-ing the whole stream (which would
        // include the 4-byte header as if it were reply text).
        let mut lenb = [0u8; 4];
        recv.read_exact(&mut lenb)
            .await
            .map_err(|e| JsError::new(&format!("read reply len: {e}")))?;
        let len = u32::from_le_bytes(lenb) as usize;
        if len > MAX_FRAME {
            conn.close(2u32.into(), b"reply too large");
            return Err(JsError::new("reply exceeds MAX_FRAME"));
        }
        let mut reply = vec![0u8; len];
        recv.read_exact(&mut reply)
            .await
            .map_err(|e| JsError::new(&format!("read reply body: {e}")))?;
        conn.close(0u32.into(), b"done");
        String::from_utf8(reply).map_err(|e| JsError::new(&format!("reply not utf8: {e}")))
    }
}

/// Spawn the `hive/browser/0` accept loop. One connection → many bi streams;
/// each stream is a `[u32 len][bytes]` request that is echoed straight back.
/// No dispatch, no trust arms, no side effects — the minimal proof of inbound
/// bidirectional traffic on a browser peer.
fn spawn_accept_loop(ep: Endpoint, served: Arc<std::sync::atomic::AtomicU64>) {
    n0_future::task::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let served = served.clone();
            n0_future::task::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "browser accept: handshake failed");
                        return;
                    }
                };
                // Only our own ALPN reaches here (the endpoint advertises just
                // one), but be explicit: refuse anything else loudly.
                if conn.alpn() != BROWSER_ALPN {
                    conn.close(1u32.into(), b"unexpected alpn");
                    return;
                }
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let served = served.clone();
                    n0_future::task::spawn(async move {
                        let mut lenb = [0u8; 4];
                        if recv.read_exact(&mut lenb).await.is_err() {
                            return;
                        }
                        let len = u32::from_le_bytes(lenb) as usize;
                        if len > MAX_FRAME {
                            let _ = send.reset(2u32.into());
                            return;
                        }
                        let mut buf = vec![0u8; len];
                        if recv.read_exact(&mut buf).await.is_err() {
                            return;
                        }
                        // Echo the exact bytes back, length-prefixed the same way.
                        if send.write_all(&lenb).await.is_ok()
                            && send.write_all(&buf).await.is_ok()
                        {
                            let _ = send.finish();
                            served.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    });
                }
            });
        }
    });
}

/// Parse 64 hex chars into 32 bytes; `None` on any malformed input.
fn hex_to_32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}
