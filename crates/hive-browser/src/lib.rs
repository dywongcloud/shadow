//! hive-browser — a SHADW mesh node that runs inside a browser tab.
//!
//! This is the wasm32 networking scaffold (PRD `bn-impl-crate-scaffold`). It
//! boots a real iroh `Endpoint` over the browser's relay-only WebSocket
//! transport, publishes/resolves identity through a pkarr relay, and accepts
//! inbound connections on its OWN ALPN (`hive/browser/0`) — the dedicated,
//! never-trusted surface the design (`docs/browser-node-proposal.md` §2.8)
//! keeps structurally disjoint from the fleet control plane. There are NO
//! gossip/join/raw arms here by construction; the handler only serves the
//! small op set in `hive-browser-proto` (echo plus edge-function invoke today),
//! which is enough to prove that a browser tab is a real bidirectional iroh peer
//! without granting it any fleet-node authority.
//!
//! Everything the browser cannot do (UDP, hole-punching) is absent by the same
//! compile-time cfg iroh itself uses; all traffic rides a relay WebSocket and
//! stays end-to-end encrypted (the relay cannot decrypt it).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use iroh::{Endpoint, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
use serde::Serialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// The registered edge-function invoke handler: a JS callback
/// `(codeDigest: string, requestJson: string) => Promise<string>` that resolves
/// the digest to a LOCALLY pinned artifact. Executable source never crosses the
/// wire. `Rc<RefCell<>>`, not `Arc<Mutex<>>` — wasm32 in a browser tab is
/// single-threaded, and `js_sys::Function` isn't `Send` anyway.
type InvokeHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Trusted local asset reader:
/// `(digest: string, offset: number, maxLen: number) => Promise<{total, bytes}>`.
type AssetHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Exact execution scopes granted to each TLS-authenticated iroh endpoint.
/// Empty at boot and shared (not snapshotted) across connection/stream tasks so
/// revocation applies to existing pooled QUIC connections.
type InvokerGrants = Rc<RefCell<BTreeMap<EndpointId, BTreeSet<String>>>>;
type AssetGrants = Rc<RefCell<BTreeMap<EndpointId, BTreeSet<String>>>>;

/// The browser node's dedicated ALPN, plus the rest of the wire contract.
/// Intentionally distinct from `hive/tunnel/0` (the fleet data/control ALPN) so
/// a browser peer can never be one byte from a control-plane stream mode — the
/// fleet-side accept path dispatches per-ALPN and this one has no privileged
/// arms.
///
/// Everything here comes from `hive-browser-proto`, which `crates/hive-p2p`
/// also depends on: this crate being wasm32-only and workspace-excluded does
/// not prevent a path dependency, so there is no second copy of the ALPN, the
/// frame cap, the op bytes, or the framing helpers to keep in sync.
pub use hive_browser_proto::BROWSER_ALPN;

use hive_browser_proto::{
    check_len, encode_asset_get, encode_asset_reply, encode_invoke, encode_reply, encode_request,
    reset as proto_reset, split_asset_get, split_asset_reply, split_invoke, valid_blake3_digest,
    valid_function_digest, Op, ASSET_CHUNK_MAX,
};

#[wasm_bindgen(start)]
pub fn on_load() {
    console_error_panic_hook::set_once();
    // Best-effort structured logs to the devtools console; harmless if it fails.
    let _ = std::panic::catch_unwind(|| tracing_wasm::set_as_global_default());
}

/// Canonical content address shared by function artifacts and asset bytes.
#[wasm_bindgen(js_name = blake3Hex)]
pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
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
    /// See [`InvokeHandler`]; `None` until `setInvokeHandler` is called.
    invoke: InvokeHandler,
    /// Demand-side asset reader; serving remains disabled until explicitly set.
    asset: AssetHandler,
    /// Boot-empty, typed endpoint → pinned-code-digest execution scopes.
    grants: InvokerGrants,
    /// Separate endpoint → asset-digest scopes. A function grant can never read
    /// an asset accidentally, even when both identifiers are 64 hex bytes.
    asset_grants: AssetGrants,
    /// Synchronous idempotency flag for the async close path.
    closed: Rc<Cell<bool>>,
}

impl Drop for BrowserNode {
    fn drop(&mut self) {
        // wasm-bindgen's generated `free()` cannot await, but it must not leave
        // the accept loop/relay connection alive forever. Clear capabilities
        // synchronously and begin best-effort shutdown. Callers needing a
        // witnessed drain use the explicit async `close()` before `free()`.
        self.invoke.borrow_mut().take();
        self.asset.borrow_mut().take();
        self.grants.borrow_mut().clear();
        self.asset_grants.borrow_mut().clear();
        if !self.closed.replace(true) {
            let ep = self.ep.clone();
            n0_future::task::spawn(async move { ep.close().await });
        }
    }
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

        // `bind()` resolving does NOT mean this node is dialable. The relay
        // entry that a browser peer needs — the ONLY transport it has, since a
        // browser cannot dial raw IP — is registered asynchronously once the
        // home-relay connection comes up, so `ep.addr()` before that returns an
        // EndpointAddr with an EMPTY addrs list. Handing that out looks like a
        // valid address and fails much later at the DIALER with "No addressing
        // information available", blaming the caller for this node's
        // un-readiness. `browser_echo_native.rs` hit exactly this and carries
        // the same `online()` call for the same reason.
        ep.online().await;

        let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let invoke: InvokeHandler = Rc::new(RefCell::new(None));
        let asset: AssetHandler = Rc::new(RefCell::new(None));
        let grants: InvokerGrants = Rc::new(RefCell::new(BTreeMap::new()));
        let asset_grants: AssetGrants = Rc::new(RefCell::new(BTreeMap::new()));
        let closed = Rc::new(Cell::new(false));
        spawn_accept_loop(
            ep.clone(),
            served.clone(),
            invoke.clone(),
            asset.clone(),
            grants.clone(),
            asset_grants.clone(),
        );

        Ok(BrowserNode {
            ep,
            relay: relay.to_string(),
            served,
            invoke,
            asset,
            grants,
            asset_grants,
            closed,
        })
    }

    /// The node's cryptographic identity (64-hex EndpointId = its ed25519
    /// public key). Stable across reloads iff booted from the same seed.
    #[wasm_bindgen(js_name = nodeId)]
    pub fn node_id(&self) -> String {
        self.ep.id().to_string()
    }

    /// The raw 32-byte ed25519 seed, as 64 hex chars — the ONLY way key
    /// material leaves this module. Per `docs/browser-node-proposal.md` §2.2,
    /// this exists solely so the caller can wrap it with a non-extractable
    /// WebCrypto key before it ever touches durable storage; the wasm module
    /// itself never persists anything. JS strings cannot be zeroed after use
    /// (no mutable-buffer access), so the caller must encrypt this value
    /// immediately on receipt and never log or store it bare.
    #[wasm_bindgen(js_name = secretHex)]
    pub fn secret_hex(&self) -> String {
        bytes_to_hex(&self.ep.secret_key().to_bytes())
    }

    /// Serialized `EndpointAddr` (id + relay/transport hints) a peer needs to
    /// dial this browser node.
    ///
    /// THROWS rather than returning an address with no transport hints. `boot`
    /// awaits `online()` so this should not happen, but an address carrying
    /// only an id is undialable, and silently returning one moves the failure
    /// to a different machine's dial attempt where the real cause is invisible.
    /// Fail here, where the node that is not ready can actually be named.
    #[wasm_bindgen(js_name = addrJson)]
    pub fn addr_json(&self) -> Result<String, JsError> {
        self.ensure_open()?;
        let addr = self.ep.addr();
        if addr.addrs.is_empty() {
            return Err(JsError::new(
                "node has no transport hints yet (not online) — refusing to hand out an undialable addr",
            ));
        }
        serde_json::to_string(&addr).map_err(|e| JsError::new(&format!("addr serialize: {e}")))
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
            // Status is a display surface, not a dial surface: an un-ready node
            // should still render its id/relay/counters rather than throwing the
            // whole status blob away, so the undialable case degrades to an
            // empty string HERE only. `addrJson` itself still refuses.
            addr_json: self.addr_json().unwrap_or_default(),
        })
        .unwrap_or_default()
    }

    /// Outbound test: dial `peer_addr_json` on `hive/browser/0`, send `msg` as
    /// an [`Op::Echo`] request, and return the echoed reply. Proves the browser
    /// node's OUTBOUND path (browser → relay → peer) in addition to the
    /// accept loop's inbound path.
    #[wasm_bindgen(js_name = echoTo)]
    pub async fn echo_to(&self, peer_addr_json: String, msg: String) -> Result<String, JsError> {
        let reply = self
            .request(peer_addr_json, Op::Echo, msg.as_bytes())
            .await?;
        String::from_utf8(reply).map_err(|e| JsError::new(&format!("reply not utf8: {e}")))
    }

    /// Register the trusted resolver called for every authorized
    /// [`Op::Invoke`] request. `handler` is
    /// `(codeDigest: string, requestJson: string) => Promise<string>` and must
    /// resolve `codeDigest` to a LOCALLY pinned artifact; executable source is
    /// never accepted from a peer. Installing a handler grants nobody.
    #[wasm_bindgen(js_name = setInvokeHandler)]
    pub fn set_invoke_handler(&self, handler: js_sys::Function) -> Result<(), JsError> {
        self.ensure_open()?;
        if !handler.is_function() {
            return Err(JsError::new("invoke handler must be a function"));
        }
        *self.invoke.borrow_mut() = Some(handler);
        Ok(())
    }

    /// Register the trusted local reader used by [`Op::AssetGet`]. Registration
    /// grants nobody; each caller still needs an exact endpoint/digest scope.
    #[wasm_bindgen(js_name = setAssetHandler)]
    pub fn set_asset_handler(&self, handler: js_sys::Function) -> Result<(), JsError> {
        self.ensure_open()?;
        if !handler.is_function() {
            return Err(JsError::new("asset handler must be a function"));
        }
        *self.asset.borrow_mut() = Some(handler);
        Ok(())
    }

    /// Grant one authenticated endpoint permission to pull one exact asset.
    #[wasm_bindgen(js_name = grantAsset)]
    pub fn grant_asset(&self, endpoint_id: String, digest: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        validate_asset_digest(&digest)?;
        Ok(self
            .asset_grants
            .borrow_mut()
            .entry(endpoint_id)
            .or_default()
            .insert(digest))
    }

    /// Revoke one endpoint/asset capability; pooled connections re-read it on
    /// every chunk, so revocation also stops a transfer already in progress.
    #[wasm_bindgen(js_name = revokeAsset)]
    pub fn revoke_asset(&self, endpoint_id: String, digest: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        validate_asset_digest(&digest)?;
        let mut grants = self.asset_grants.borrow_mut();
        let Some(scopes) = grants.get_mut(&endpoint_id) else {
            return Ok(false);
        };
        let removed = scopes.remove(&digest);
        if scopes.is_empty() {
            grants.remove(&endpoint_id);
        }
        Ok(removed)
    }

    /// Grant one TLS-authenticated iroh endpoint permission to invoke exactly
    /// one pinned code digest. The boot-empty map is the execution boundary;
    /// future platform admission is its sole production writer.
    #[wasm_bindgen(js_name = grantInvoker)]
    pub fn grant_invoker(&self, endpoint_id: String, code_digest: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        validate_code_digest(&code_digest)?;
        Ok(self
            .grants
            .borrow_mut()
            .entry(endpoint_id)
            .or_default()
            .insert(code_digest))
    }

    /// Revoke one exact endpoint/digest scope. Idempotent: a valid but absent
    /// scope returns `false`; malformed IDs/digests throw without mutation.
    /// Existing connections re-read this map for every invoke stream.
    #[wasm_bindgen(js_name = revokeInvoker)]
    pub fn revoke_invoker(
        &self,
        endpoint_id: String,
        code_digest: String,
    ) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        validate_code_digest(&code_digest)?;
        let mut grants = self.grants.borrow_mut();
        let Some(scopes) = grants.get_mut(&endpoint_id) else {
            return Ok(false);
        };
        let removed = scopes.remove(&code_digest);
        if scopes.is_empty() {
            grants.remove(&endpoint_id);
        }
        Ok(removed)
    }

    /// Clear every execution capability and gracefully close the iroh endpoint.
    /// Idempotent and awaitable; unlike wasm-bindgen's generated `free()`, this
    /// waits for QUIC close notifications. An invocation already inside a JS
    /// Promise may finish; no not-yet-started invocation can begin after grants
    /// are cleared.
    #[wasm_bindgen(js_name = close)]
    pub async fn close(&self) {
        self.invoke.borrow_mut().take();
        self.asset.borrow_mut().take();
        self.grants.borrow_mut().clear();
        self.asset_grants.borrow_mut().clear();
        if !self.closed.replace(true) {
            self.ep.close().await;
        } else {
            self.ep.closed().await;
        }
    }

    fn ensure_open(&self) -> Result<(), JsError> {
        if self.closed.get() || self.ep.is_closed() {
            return Err(JsError::new("browser node is closed"));
        }
        Ok(())
    }

    /// Outbound: dial `peer_addr_json` and ask it to invoke the locally pinned
    /// artifact named by `code_digest` against `request_json` (a Lagon-shaped
    /// `{method,path,headers,body}` envelope), returning raw response bytes as a
    /// UTF-8 string. No executable source crosses the wire.
    #[wasm_bindgen(js_name = invokeOn)]
    pub async fn invoke_on(
        &self,
        peer_addr_json: String,
        code_digest: String,
        request_json: String,
    ) -> Result<String, JsError> {
        self.ensure_open()?;
        let payload = encode_invoke(&code_digest, &request_json)
            .map_err(|e| JsError::new(&format!("bad invoke: {e}")))?;
        let reply = self.request(peer_addr_json, Op::Invoke, &payload).await?;
        String::from_utf8(reply).map_err(|e| JsError::new(&format!("reply not utf8: {e}")))
    }

    /// Pull a complete BLAKE3-addressed asset in bounded chunks. Every reply
    /// repeats the immutable total length; the final assembled bytes are hashed
    /// before any caller can persist them.
    #[wasm_bindgen(js_name = assetOn)]
    pub async fn asset_on(
        &self,
        peer_addr_json: String,
        digest: String,
    ) -> Result<js_sys::Uint8Array, JsError> {
        self.ensure_open()?;
        validate_asset_digest(&digest)?;
        let mut out = Vec::new();
        let mut total = None;
        loop {
            let payload = encode_asset_get(&digest, out.len() as u64, ASSET_CHUNK_MAX)
                .map_err(|e| JsError::new(&format!("bad asset request: {e}")))?;
            let reply = self
                .request(peer_addr_json.clone(), Op::AssetGet, &payload)
                .await?;
            let (reply_total, chunk) = split_asset_reply(&reply)
                .map_err(|e| JsError::new(&format!("bad asset reply: {e}")))?;
            if total.replace(reply_total).is_some_and(|known| known != reply_total) {
                return Err(JsError::new("asset length changed during transfer"));
            }
            let next = out.len().checked_add(chunk.len()).ok_or_else(|| {
                JsError::new("asset length overflow")
            })?;
            if next as u64 > reply_total || (chunk.is_empty() && next as u64 != reply_total) {
                return Err(JsError::new("asset peer returned an inconsistent range"));
            }
            out.extend_from_slice(chunk);
            if out.len() as u64 == reply_total {
                break;
            }
        }
        if blake3::hash(&out).to_hex().as_str() != digest {
            return Err(JsError::new("asset BLAKE3 mismatch"));
        }
        Ok(js_sys::Uint8Array::from(out.as_slice()))
    }

    /// Shared request/response plumbing for both public methods above: dial,
    /// send `[u32 total_len][op][op_payload]`, read back `[u32 len][bytes]`.
    async fn request(
        &self,
        peer_addr_json: String,
        op: Op,
        op_payload: &[u8],
    ) -> Result<Vec<u8>, JsError> {
        self.ensure_open()?;
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
        // One write of one pre-built frame, not three writes of its pieces: the
        // header/op/payload split is the protocol's business, not this call's.
        // Writer speaks first — open_bi is lazy, the peer never sees the stream
        // until bytes arrive.
        send.write_all(&encode_request(op, op_payload))
            .await
            .map_err(|e| JsError::new(&format!("write request: {e}")))?;
        send.finish()
            .map_err(|e| JsError::new(&format!("finish: {e}")))?;
        // The peer replies with plain [u32 len][bytes] — no op byte on the way
        // back, since the caller already knows what it asked for.
        let mut lenb = [0u8; 4];
        recv.read_exact(&mut lenb)
            .await
            .map_err(|e| JsError::new(&format!("read reply len: {e}")))?;
        let len = check_len(lenb).map_err(|e| {
            conn.close(e.reset_code().into(), b"bad reply frame");
            JsError::new(&format!("reply frame: {e}"))
        })?;
        let mut reply = vec![0u8; len];
        recv.read_exact(&mut reply)
            .await
            .map_err(|e| JsError::new(&format!("read reply body: {e}")))?;
        conn.close(0u32.into(), b"done");
        Ok(reply)
    }
}

/// Spawn the `hive/browser/0` accept loop. One connection → many bi streams;
/// each stream is a `[u32 len][op][payload]` request dispatched on its op byte.
/// No gossip, no join, no trust arms — this ALPN has no privileged surface, and
/// growing one belongs behind admission (`bn-impl-mesh-admission`), not here.
///
/// Every refusal is loud and carries a distinct `hive-browser-proto` reset code.
/// There is deliberately no "unknown op falls back to echo" arm: that would
/// answer a future protocol version's request with its own raw bytes and look
/// like success to the caller.
fn spawn_accept_loop(
    ep: Endpoint,
    served: Arc<std::sync::atomic::AtomicU64>,
    invoke: InvokeHandler,
    asset: AssetHandler,
    grants: InvokerGrants,
    asset_grants: AssetGrants,
) {
    n0_future::task::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let served = served.clone();
            let invoke = invoke.clone();
            let asset = asset.clone();
            let grants = grants.clone();
            let asset_grants = asset_grants.clone();
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
                    conn.close(proto_reset::UNEXPECTED_ALPN.into(), b"unexpected alpn");
                    return;
                }
                let remote_id = conn.remote_id();
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let served = served.clone();
                    let invoke = invoke.clone();
                    let asset = asset.clone();
                    let grants = grants.clone();
                    let asset_grants = asset_grants.clone();
                    n0_future::task::spawn(async move {
                        let mut lenb = [0u8; 4];
                        if recv.read_exact(&mut lenb).await.is_err() {
                            return;
                        }
                        let len = match check_len(lenb) {
                            Ok(n) => n,
                            Err(e) => {
                                tracing::warn!(error = %e, "browser stream refused");
                                let _ = send.reset(e.reset_code().into());
                                return;
                            }
                        };
                        if len == 0 {
                            let _ = send.reset(proto_reset::MALFORMED_PAYLOAD.into());
                            return;
                        }
                        // Read the op BEFORE allocating the caller-controlled
                        // remainder. A completely ungranted endpoint gets one
                        // cheap FORBIDDEN reset instead of a 1 MiB allocation.
                        let mut opb = [0u8; 1];
                        if recv.read_exact(&mut opb).await.is_err() {
                            return;
                        }
                        let op = match Op::from_byte(opb[0]) {
                            Ok(op) => op,
                            Err(e) => {
                                tracing::warn!(error = %e, remote = %remote_id, "browser stream refused");
                                let _ = send.reset(e.reset_code().into());
                                return;
                            }
                        };
                        let endpoint_granted = match op {
                            Op::Echo => true,
                            Op::Invoke => grants.borrow().contains_key(&remote_id),
                            Op::AssetGet => asset_grants.borrow().contains_key(&remote_id),
                        };
                        if !endpoint_granted {
                            tracing::warn!(remote = %remote_id, ?op, "browser operation forbidden");
                            let _ = send.reset(proto_reset::FORBIDDEN.into());
                            return;
                        }
                        let mut payload = vec![0u8; len - 1];
                        if recv.read_exact(&mut payload).await.is_err() {
                            return;
                        }
                        let reply = match op {
                            Op::Echo => payload,
                            Op::Invoke => {
                                match run_invoke(&invoke, &grants, remote_id, &payload).await {
                                    Ok(bytes) => bytes,
                                    Err(code) => {
                                        let _ = send.reset(code.into());
                                        return;
                                    }
                                }
                            }
                            Op::AssetGet => {
                                match run_asset(
                                    &asset,
                                    &asset_grants,
                                    remote_id,
                                    &payload,
                                )
                                .await
                                {
                                    Ok(bytes) => bytes,
                                    Err(code) => {
                                        let _ = send.reset(code.into());
                                        return;
                                    }
                                }
                            }
                        };
                        if send.write_all(&encode_reply(&reply)).await.is_ok() {
                            let _ = send.finish();
                            served.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    });
                }
            });
        }
    });
}

/// Run one [`Op::Invoke`] payload against the trusted local artifact resolver.
///
/// `Err` is the reset code to refuse the stream with, so each distinguishable
/// failure reaches the caller as a distinct code instead of collapsing into an
/// empty reply. Authorization is re-read at the final synchronous point before
/// `Function::call2`; pooled connections never cache a grant.
///
/// The handler and grant `Rc`s are NOT borrowed across the await. `RefCell` has
/// no async awareness, so keeping either borrow alive over `JsFuture` would
/// panic on concurrent invoke/grant/revoke activity.
async fn run_invoke(
    invoke: &InvokeHandler,
    grants: &InvokerGrants,
    remote_id: EndpointId,
    payload: &[u8],
) -> Result<Vec<u8>, u32> {
    let (code_digest, request_json) = split_invoke(payload).map_err(|e| {
        tracing::warn!(error = %e, remote = %remote_id, "browser invoke: bad payload");
        e.reset_code()
    })?;

    let allowed = grants
        .borrow()
        .get(&remote_id)
        .is_some_and(|scopes| scopes.contains(code_digest));
    if !allowed {
        tracing::warn!(remote = %remote_id, digest = code_digest, "browser invoke forbidden");
        return Err(proto_reset::FORBIDDEN);
    }

    let handler = invoke.borrow().clone();
    let Some(handler) = handler else {
        tracing::warn!("browser invoke: no handler registered (call setInvokeHandler first)");
        return Err(proto_reset::NO_HANDLER);
    };

    let ret = handler
        .call2(
            &JsValue::NULL,
            &JsValue::from_str(code_digest),
            &JsValue::from_str(request_json),
        )
        .map_err(|e| {
            tracing::warn!(error = ?e, "browser invoke: handler threw");
            proto_reset::HANDLER_FAILED
        })?;

    // The handler may be sync or async; `Promise::resolve` normalises both, so
    // a handler returning a plain string is not a silent failure.
    let resolved = JsFuture::from(js_sys::Promise::resolve(&ret))
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, "browser invoke: handler rejected");
            proto_reset::HANDLER_FAILED
        })?;

    let Some(text) = resolved.as_string() else {
        tracing::warn!("browser invoke: handler resolved with a non-string");
        return Err(proto_reset::HANDLER_FAILED);
    };
    Ok(text.into_bytes())
}

async fn run_asset(
    handler: &AssetHandler,
    grants: &AssetGrants,
    remote_id: EndpointId,
    payload: &[u8],
) -> Result<Vec<u8>, u32> {
    let (digest, offset, max_len) = split_asset_get(payload).map_err(|e| {
        tracing::warn!(error = %e, remote = %remote_id, "browser asset: bad payload");
        e.reset_code()
    })?;
    let allowed = grants
        .borrow()
        .get(&remote_id)
        .is_some_and(|scopes| scopes.contains(digest));
    if !allowed {
        tracing::warn!(remote = %remote_id, digest, "browser asset forbidden");
        return Err(proto_reset::FORBIDDEN);
    }
    if offset > js_sys::Number::MAX_SAFE_INTEGER as u64 {
        return Err(proto_reset::MALFORMED_PAYLOAD);
    }
    let Some(handler) = handler.borrow().clone() else {
        return Err(proto_reset::NO_HANDLER);
    };
    let value = handler
        .call3(
            &JsValue::NULL,
            &JsValue::from_str(digest),
            &JsValue::from_f64(offset as f64),
            &JsValue::from_f64(max_len as f64),
        )
        .map_err(|_| proto_reset::HANDLER_FAILED)?;
    let resolved = JsFuture::from(js_sys::Promise::resolve(&value))
        .await
        .map_err(|_| proto_reset::HANDLER_FAILED)?;
    let total = js_sys::Reflect::get(&resolved, &JsValue::from_str("total"))
        .ok()
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0)
        .ok_or(proto_reset::HANDLER_FAILED)?;
    if total > js_sys::Number::MAX_SAFE_INTEGER {
        return Err(proto_reset::HANDLER_FAILED);
    }
    let bytes = js_sys::Reflect::get(&resolved, &JsValue::from_str("bytes"))
        .map_err(|_| proto_reset::HANDLER_FAILED)?;
    if bytes.is_null() || bytes.is_undefined() {
        return Err(proto_reset::HANDLER_FAILED);
    }
    let chunk = js_sys::Uint8Array::new(&bytes).to_vec();
    if chunk.len() > max_len || offset.saturating_add(chunk.len() as u64) > total as u64 {
        return Err(proto_reset::HANDLER_FAILED);
    }
    encode_asset_reply(total as u64, &chunk).map_err(|e| e.reset_code())
}

/// Parse the cryptographic endpoint identity used by iroh's completed TLS
/// handshake. Keeping the typed key in the grants map avoids string aliases
/// (hex/base32/case) that could make revocation miss the original grant.
fn parse_endpoint_id(raw: &str) -> Result<EndpointId, JsError> {
    raw.trim()
        .parse::<EndpointId>()
        .map_err(|e| JsError::new(&format!("invalid endpoint id: {e}")))
}

fn validate_code_digest(digest: &str) -> Result<(), JsError> {
    if valid_function_digest(digest) {
        Ok(())
    } else {
        Err(JsError::new(
            "code digest must be exactly 64 lowercase hexadecimal bytes",
        ))
    }
}

fn validate_asset_digest(digest: &str) -> Result<(), JsError> {
    if valid_blake3_digest(digest) {
        Ok(())
    } else {
        Err(JsError::new(
            "asset digest must be exactly 64 lowercase hexadecimal bytes",
        ))
    }
}

/// Render bytes as lowercase hex.
fn bytes_to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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
