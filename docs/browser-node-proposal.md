<!--
Origin: this proposal was produced by a 32-agent research + adversarial-verification
workflow (2026-07-31) studying the primary sources named in section 0, plus live
in-browser measurement (Chrome 150, docs/browser-node-measurements.json). Every
load-bearing claim survived an independent refutation pass; the corrections that
pass produced are folded into the body and flagged inline. The completeness critic's
16 gaps are reproduced verbatim in Appendix A and each is now tracked as / folded
into a PRD row (see the map at the end of section 4).
-->

## 0. Sources studied (primary, cited inline throughout)

- iroh docs: protocols/using-quic, languages/wasm-browser, protocols/blobs,
  protocols/automerge (+ writing-a-protocol, connect-two-endpoints); cross-checked
  against docs.rs pins for iroh 1.0.2/1.0.3 and noq 1.0.1/1.1.1, and the
  n0-computer/iroh-examples browser-echo / browser-blobs skeletons.
- lagonapp/lagon packages/js-runtime/src/runtime (archived, AGPL-3.0 — studied as
  an ABI spec, never for literal reuse).
- macaly/almostnode (MIT — fork-and-own candidate for the node-compat layer).
- denoland/deno core/runtime.rs (architecture reference for the ops/extensions/event-loop
  shape, not the engine).
- Substrate research: quickjs-emscripten, StarlingMonkey, sandboxed-iframe site isolation;
  sqlite.org WASM + OPFS, vlcn/superfly cr-sqlite, @automerge/automerge(-repo) + samod;
  WebCrypto Ed25519, Chromium tab-lifecycle/throttling sources, iroh-relay AccessControl.

---

# SHADW Browser Node — Design Proposal

A browser tab as a first-class (but low-trust, low-durability) SHADW mesh peer: it joins the
iroh mesh through our relays, runs tenant edge functions, hosts node-style apps, replicates
sqlite/CRDT state and static assets, and serves itself offline. Every claim below survived an
adversarial verification pass against primary sources (docs.rs pins, upstream source bytes,
live registry metadata); corrections from that pass are folded in and flagged where they
change code we would otherwise have written wrong.

---

## 1. Premise corrections (state these plainly before any design talk)

1. **There is no V8/Deno/Node engine to run in wasm.** The engine-in-wasm options are QuickJS
   (interpreter, 0.5 MB) and StarlingMonkey (10.5–12.2 MB wasm, WASI-0.2/wasmtime-targeted,
   zero documented browser support — rejected). The substitute is the **browser itself**:
   primary execution substrate is a **cross-site sandboxed iframe** (`sandbox="allow-scripts"`
   only, host document on a separate registrable domain → opaque origin + its own renderer
   process under Chrome Site Isolation), with tenant code inside a Worker in that frame, and
   **quickjs-emscripten RELEASE_SYNC nested inside the same frame** when deterministic
   CPU/memory metering is required. What we take from deno_core is its *architecture*
   (ops, extensions, event-loop predicate), not its engine.

2. **Browser networking is relay-only, permanently in practice.** Browsers cannot send UDP;
   iroh's wasm build rides a WebSocket (`ws_stream_wasm`) to a relay for *all* traffic
   (still E2E-encrypted; the relay cannot decrypt). Two hard consequences: (a) our plain
   `http://:3340` relays are **unusable from any https page** (mixed content blocks `ws://`)
   — relays need TLS (`wss://`) before a single browser node can join; (b) every byte a
   browser exchanges with any peer is metered fleet relay traffic.

3. **Lagon is dead and radioactive for literal reuse.** Archived (last push 2024-05-29),
   repo-wide AGPL-3.0 (network copyleft would infect the combined work). Its value is the
   *inventory and ABI shape* — a two-object host boundary (`LagonSync`/`LagonAsync`) plus one
   `masterHandler` request envelope — treated as a spec for clean-room reimplementation. Its
   code is also non-conformant where it matters (string-only fetch bodies destroy binary,
   NUL-truncating TextDecoder, regex URL, crypto that hands raw key bytes to guest JS and
   returns the same buffer as both halves of an "asymmetric" pair). Never port those.

4. **Browser-as-CDN-edge economics are upside down.** Relay-only means a browser serving 1 GB
   to any other peer costs ~2 GB of fleet relay traffic (in + out) *after* we spent ~1 GB
   seeding it — ~3x fleet bytes vs the fleet node just serving directly (worst case 4x if
   sender and receiver home to different relays). The browser asset store is therefore
   **demand-side only**: offline self-serve, repeat-view egress avoidance, ingest dedup for
   browser-originated content, zero-cost serving to apps hosted in the same browser. It
   counts toward replication factor **zero** and is never placed in another consumer's
   serving set.

5. **The iroh docs site previews an unreleased API — code against docs.rs for our pin.**
   Verified against iroh 1.0.2/1.0.3 and noq 1.0.1/1.1.1: `accept_bi().await` yields
   `Result<(SendStream, RecvStream), ConnectionError>` and `accept_uni().await` yields
   `Result<RecvStream, ConnectionError>` — **no `Option`**; the graceful shutdown signal is
   the `Err` arm (`ConnectionError::ApplicationClosed`/`LocallyClosed`), the exact shape the
   docs page says not to use. Likewise `SendStream::stopped()` resolves
   `Result<Option<VarInt>, StoppedError>` (`Ok(Some(code))` = peer stopped, `Ok(None)` = peer
   read to completion — handle both), `RecvStream::stop(code)` is synchronous, and the
   docs-page "stop resolves with the reset code" behavior actually belongs to
   `received_reset()`.

---

## 2. Architecture

### 2.1 Networking crate (`crates/hive-browser`, wasm32-unknown-unknown)

The recipe is the official `browser-echo`/`browser-blobs` skeleton, not the docs page — the
docs' own dependency line (`default-features = false`, no features) silently drops `tls-ring`
and cfg's out every endpoint preset (`N0`/`Minimal` are gated on `with_crypto_provider`).

```toml
# crates/hive-browser/Cargo.toml
[dependencies]
iroh        = { version = "1", default-features = false, features = ["tls-ring"] }  # pin-compatible with workspace iroh 1.0.2
iroh-blobs  = { version = "0.103", default-features = false }   # MemStore/get/provider only; fs-store & rpc are native-only
getrandom   = { version = "0.3", features = ["wasm_js"] }
wasm-bindgen = "=0.2.122"          # CLI version must match this pin exactly
wasm-bindgen-futures = "0.4"
n0-future   = "0.3"                # task::spawn instead of tokio::spawn (no tokio runtime on wasm)
tokio       = { version = "1.43", default-features = false, features = ["sync"] }
tokio-stream = { version = "0.1.17", default-features = false, features = ["sync"] }
```

```toml
# crates/hive-browser/.cargo/config.toml
[target.wasm32-unknown-unknown]
rustflags = ['--cfg', 'getrandom_backend="wasm_js"']   # harmless-but-required while any getrandom 0.3.x is in tree
```

Build pipeline (wasm-bindgen CLI, not wasm-pack):
`cargo build --target=wasm32-unknown-unknown --release`
→ `wasm-bindgen --weak-refs --target=web`
→ `wasm-opt --enable-bulk-memory --enable-nontrapping-float-to-int -Os`.
Adopt upstream iroh-blobs' CI leak guard verbatim:
`! wasm-tools print --skeleton hive_browser.wasm | grep 'import "env"'`.

Endpoint construction: **not** `presets::N0` — a custom builder wiring
`PkarrPublisher::builder(<shadw pkarr relay URL>)` + `PkarrResolver::builder(...)` (both
constructors verified present in 1.0.2; in the browser both publish and resolve go over
HTTPS, so that pkarr relay must be HTTPS-reachable), relay map pointed at our `https://`
RelayUrls (iroh derives `wss://…/relay` internally; callers must not pass a WebSocket URL),
`.alpns()` for the inbound ALPNs, `.secret_key()` from §2.2. Inbound accept via
`Router::builder(endpoint).accept(ALPN, handler).spawn()` — the ProtocolHandler path is
shared, un-cfg'd code proven in the browser by n0's own examples.

Stream discipline (binds verbatim in the browser; relay transport still carries QUIC):
- Every `SendStream` ends in exactly one of `finish()` (synchronous) or `reset(code)`;
  an unfinished stream hangs the peer's `read_to_end` forever.
- Every `read_to_end` carries a `MAX_*_SIZE` cap — this is also the memory-safety line for a
  tab hosting other tenants' traffic.
- `open_bi()` is lazy: **writer speaks first** on every opened stream, or the accept side
  never sees it.
- Last-reader closes; the other side awaits `closed()`; long-lived trunks race protocol logic
  against `conn.closed()`. A tab can vanish without running teardown, so the **fleet side of
  every browser-facing protocol must treat the ~30 s idle-timeout path as a normal exit** —
  and the browser-ALPN idle timeout must be raised to ≥2× the worst main-thread wake gap
  (>2 min, see §2.7).
- No datagrams, ever — iroh's own docs call datagrams-for-realtime misguided (still
  congestion-controlled and acked), and the browser path is a WebSocket anyway. Stream-per-item
  with `stop`/`reset` staleness aborts and stream-ID ordering, the same pattern AGENTS.md
  already records for UDP-over-mesh.
- 0.5-RTT: a ProtocolHandler can be handed request bytes before the client handshake
  completes; **no side-effectful op (function invoke with writes, sqlite mutation, asset
  mutation, billing) runs on first-flight data** — gate non-idempotent work on handshake
  completion.
- Reconnect/backoff is entirely ours (docs are silent): one long-lived reused connection,
  multiplexed streams; a reconnect costs relay re-establishment.

### 2.2 Identity & key persistence

iroh's identity intake is raw-bytes-only (`SecretKey::from_bytes(&[u8;32])` →
`Builder::secret_key`; no signer hook exists — verified across the whole Builder surface), so
a non-extractable WebCrypto Ed25519 key can never feed iroh. All Ed25519 stays in wasm
(dalek); WebCrypto is only the **wrapping layer**:

- **Generate** (first boot): `SecretKey::generate()` in wasm (proven on wasm32); JS creates a
  **non-extractable AES-GCM-256 wrapping key** WK via `subtle.generateKey`.
- **Store**: one IndexedDB record `{wk: CryptoKey (structured clone), iv, ct =
  AES-GCM(WK, iv, seed, aad="shadw-node-id-v1"), endpoint_id, created_ms}` in a single IDB
  transaction. Use `subtle.encrypt` on the 32 raw bytes, **not** `wrapKey` (wrapKey requires
  an extractable key and BCD shows no ed25519 wrapKey support). Never localStorage. Zero the
  JS-side buffer after encrypt.
- **Load**: decrypt → copy into wasm linear memory → `from_bytes` → bind → zero the staging
  buffers (dalek 3.x `SigningKey` zeroizes on drop).
- **Rotate as a first-class flow**: EndpointId *is* the pubkey, so rotation = new seed + new
  WK under `id:"next"`, a `{old_id, new_id, ts}` handover signed by the OLD key submitted to
  the control plane, atomic pointer flip, delete after fleet ack. Cheap — rotate on suspected
  XSS and consider N-day auto-rotation.
- **Honesty**: this stops plaintext-at-rest and off-origin replay of scraped blobs. It does
  **not** stop live same-origin XSS (which can call decrypt and read exported wasm memory),
  and the W3C spec explicitly disclaims at-rest protection of the CryptoKey itself. The real
  XSS lever is CSP (`script-src 'self' 'wasm-unsafe-eval'`, no inline/eval) plus identity
  handling confined to the worker. Consequence for the platform: **browser EndpointIds are
  low-privilege and capability-gated server-side, always** (§2.8).

### 2.3 Function runtime substrate (primary + fallback)

**Primary — cross-site sandboxed iframe.** `sandbox="allow-scripts"` and *never*
`allow-same-origin` (which on a shared sandbox domain would also give every tenant the same
real origin); host document served from a **separate registrable domain** (site = eTLD+1 —
Chrome's process boundary is per-site). Yields opaque origin (SOP always fails; no
cookies/storage), a dedicated sandboxed renderer process (defends even compromised renderers,
Chrome 77+), full-JIT throughput and the whole web API surface at zero bundle cost. Tenant
code runs in a Worker inside the frame so `worker.terminate()` is the hard kill (the iframe
substrate has no interrupt/memory-limit API). All capabilities are brokered from the trusted
page over postMessage/MessageChannel with transferable ArrayBuffers; the iroh connection and
all keys stay in the trusted host context. node-ish app hosting rides this same substrate.

**Fallback (nested, for metered tiers) — quickjs-emscripten RELEASE_SYNC** (0.5 MB wasm),
run *inside* the same iframe/Worker so its unaudited-engine blast radius is capped by the
process boundary. It is the only substrate with deterministic metering: `setMemoryLimit`,
`setMaxStackSize`, `setInterruptHandler`/`shouldInterruptAfterDeadline`. Architecture copied
from deno_core, with the two verified corrections applied:

- **Ops are the sole host boundary**: numbered async ops (iroh streams, sqlite, assets,
  fetch), completions **batched into one guest call per tick** + `executePendingJobs` — the
  JS↔wasm boundary has no fast-call path, so ops are coarse (statement/batch level, buffers
  through linear memory), never chatty.
- Extensions bundle ops + JS globals shim + per-context state; permissions are host-side
  per-context data checked inside ops, invisible to the guest. **Correction honored:** Deno's
  disabled-op stubs *silently succeed* — we write our own **throwing/rejecting** stubs so
  permission denial fails loudly.
- Event loop: externally driven with the `is_pending` predicate (refed ops ∥ module eval ∥
  tick), giving deterministic **unresolved-promise hang errors** — the browser-node analog of
  request-cancellation-is-a-failure-mode; leases terminate honestly.
- Module loading: prefetch the whole module graph from replicated assets host-side, then a
  plain **synchronous** module loader — no Asyncify build (2x size, slower, single suspension)
  unless dynamic `import()` of unfetched code becomes real.
- **Correction honored:** quickjs-emscripten does **not** surface
  `JS_SetHostPromiseRejectionTracker` (the option is an unimplemented `TODO`-typed
  placeholder) — unhandled-rejection capture is a patched wasm build or userland
  approximation; budget it as integration work.
- No snapshots exist: cold-start budget = Cache-Storage-cached compiled wasm + a warm
  template context; per-tenant context creation is cheap in QuickJS.
- One QuickJS context per tenant function; the trust boundary is the wasm instance/Worker,
  not the context (contexts share linear memory).

The web-globals shim layer replicates the Lagon *shape* (one sync + one async host object,
`masterHandler` `{i,m,h,b} → {b,h,s} | stream-pull`) as postMessage stubs (iframe) or host
imports (QuickJS) — with bodies widened to `Uint8Array` both directions, native platform
globals preferred wherever they exist, and opaque WebCrypto key handles instead of Lagon's
raw-bytes CryptoKey.

### 2.4 Node-compat layer

The implementation is a **clean-room, deliberately small compatibility lane** in
`crates/hive-browser/www/node-compat.js`; no Lagon or almostnode source is copied.
`BrowserNodeCompat.pin()` wraps one local CommonJS source string as the same pinned
BLAKE3 artifact the function runtime already serves, so inbound requests use the
existing `{method,path,headers,body}` invoke path and executable source never crosses
the wire.

The supported surface is explicit:

- `require("express")`: `use/get/post/put/patch/delete/all`, exact or `:param`-shaped
  routes, query parsing, and `status/set/json/send/end` responses;
- `require("http")` / `node:http`: `createServer(listener)` as an inbound request
  adapter; `listen()` throws because browsers cannot bind sockets;
- `require("fs")` / `node:fs`: callback `readFile` and `fs.promises.readFile` against
  a per-artifact virtual read-only mount;
- `require("path")` / `node:path`: `join`, `basename`, and `dirname` over virtual
  paths; and a `Uint8Array`-backed minimal `Buffer.from/isBuffer` surface.

Filesystem reads cross numbered host op 16. The trusted runner supplies the current
artifact digest to the op handler; the handler selects that digest's mount and ignores
any guest-selected namespace, so one granted app cannot name another app's files.
Paths are normalized inside the virtual root and reject NUL/traversal. Pure route/path
shims remain in the guest because they convey no authority; all external capability
continues to cross the existing numbered op table.

Honest limits: no npm installer, package graph, ESM transform, native addon,
`better-sqlite3`, outbound `net`, real listening socket, streaming response, WebSocket
upgrade, Node process globals, or broad Buffer compatibility. `listen()` and unknown
modules fail loudly rather than pretending to work. This lane targets small
Express-style request handlers; broader package compatibility is admitted only when a
real app witnesses the missing API and the implementation can preserve the same
sandbox/op boundary.

### 2.5 sqlite + CRDT stack

- **Engine**: official sqlite.org WASM in a dedicated worker on the **opfs-sahpool** VFS —
  fastest OPFS option, no COOP/COEP headers (which would constrain the page hosting the relay
  WebSocket). Single-connection by design, so:
- **Single-writer discipline**: one DB worker per origin elected with
  `navigator.locks.request('shadw-db', …)` (exclusive); other tabs proxy SQL to the holder.
  Fallback where true multi-connection is unavoidable: `opfs-wl` (SQLite 3.53.0+, released),
  gated on `Atomics.waitAsync` detection.
- **Replication is cr-sqlite CRRs, not automerge-wrapped tables and not a statement
  journal.** Statement journals don't commute (that's SQLSync's rebase model — needs an
  authoritative sequencer, wrong for a partition-tolerant mesh); automerge-per-table doubles
  storage and merges blind to relational constraints. cr-sqlite: per-column LWW + causal-length
  deletes, delta sync as plain rows through `crsql_changes` keyed on `db_version`/`site_id` —
  production-proven by Fly.io's Corrosion ("7.5 million rows globally"). **Vendor the
  superfly/cr-sqlite fork** (active, 2026-07-26; upstream vlcn is dormant and the npm wasm
  package is stale at 0.16.0/2023) — its per-site_id serial db_versions are exactly the
  gap-detection shape `spawn_anti_entropy_loop` already uses. Statically link into a custom
  SQLite wasm build; fleet nodes load the same extension natively, so browser and fleet
  exchange identical `crsql_changes` row sets over iroh bi-streams.
- **Automerge stays for genuinely document-shaped state** (asset manifests, config docs,
  presence): `@automerge/automerge` 3.4.0 via the `/slim` entry + `initializeWasm(wasmUrl)`
  loading the 3.86 MB wasm (815 KB brotli, measured) from **our own replicated asset store**,
  no CDN. `@automerge/automerge-repo` **pinned at 2.5.6** (a 2.6.0-subduction prerelease line
  signals network-layer churn). The iroh NetworkAdapter is ~150 lines: cbor-x-encoded
  `Message`s as `[u32 len][bytes]` frames on one bi-stream, reusing the websocket package's
  `join`/`peer` handshake (ProtocolV1 = "1") so the fleet side interops via **samod 0.12.3**
  (wire-compatible by design, transport-agnostic Dialer/Acceptor around an iroh stream; keep
  samod fleet-side only — wasm support unverified). Map iroh NodeId 64-hex directly to PeerId
  so identity is transport-attested. Storage adapter: 5 methods over
  `[documentId, "snapshot"|"incremental", hash]` chunks → one OPFS file per chunk or one
  sqlite `(key TEXT PRIMARY KEY, data BLOB)` table. "sqlite replicated via automerge" does
  not exist in the wild and stays rejected in favor of cr-sqlite.
- **Durability posture**: at boot call `navigator.storage.persist()` + `estimate()` and
  gossip both as node metadata (unknown = unknown, never full). Eviction is whole-origin,
  all-technologies-at-once (assets + automerge + sqlite die together); Safari wipes
  script-written storage after 7 days without interaction. A wiped browser peer is safe by
  construction — CRR re-hydrates from the mesh from `db_version 0`.

### 2.6 Asset store & serving

- **Addressing**: one wasm-exported BLAKE3 implementation produces the canonical
  64-lowercase-hex identifier for both function source and asset bytes. The browser
  does not depend on a slow or incompatible JavaScript BLAKE3 package. Every ingest
  and completed peer pull re-hashes before persistence.
- **Durability**: `BrowserAssetStore` writes bytes plus MIME metadata to an
  origin-private `hive-browser-assets-v1` OPFS directory. A per-digest Web Lock is
  mandatory, so concurrent tabs serialize replacement/removal instead of racing.
  `createWritable().close()` is the commit boundary; a failed writer aborts loudly.
- **Serving**: the trusted host mirrors pinned responses into the
  `hive-browser-assets-v1` Cache Storage namespace. `asset-sw.js` owns only
  `/__hive_asset/<digest>` and serves GET/HEAD plus one RFC byte range with real 206,
  `Content-Range`, `Accept-Ranges`, and 416 for malformed/unsatisfiable ranges. OPFS
  remains the durable source; a cache miss is rehydrated and re-verified by the page.
  The opaque tenant iframe does not register a service worker — host ops transfer
  asset bytes into the sandbox when a function needs them.
- **Peer pull**: `Op::AssetGet` on `hive/browser/0` requests
  `[digest, offset, max_len]`; replies carry `[total_len, chunk]`. The shared 1 MiB
  frame cap leaves eight bytes for total length, so arbitrary assets transfer in
  bounded chunks. Every chunk re-checks an exact `(TLS EndpointId, digest)` grant;
  revocation therefore stops an existing pooled connection on its next range.
- **Placement**: this remains a demand-side mirror + browser-originated ingest buffer.
  Browser copies contribute replication factor zero and are never advertised as a
  general CDN serving set. Browser-to-browser pulls are explicit scoped cache fills,
  not opportunistic placement.

### 2.7 Tab-lifetime placement

- **The iroh endpoint lives in a SharedWorker, never on the main thread.** Worker timers are
  unthrottled by construction (`kDedicatedWorkerThrottling` disabled by default; SharedWorker
  threads never get budget pools); the SharedWorker survives any one tab's
  navigation/bfcache/refresh; one identity + one relay connection shared across N tabs;
  support is now universal (Chrome Android 148+, Safari 16+, Firefox 29+). Sqlite, automerge,
  and the asset CAS live in workers too — tabs are pure UI over MessagePort.
- **Fallback tier-2**: dedicated Worker in a single anchor tab where SharedWorker is missing
  — still unthrottled, dies with its page.
- **Service worker is disqualified as connection host** (30 s idle kill, 5 min event
  ceiling): cache-serving + "reopen the node" push nudge only.
- **Disconnect is the normal path**: Chrome 149 force-fails main-thread WebSockets on bfcache
  entry ("Page entered Back-Forward Cache."); freezing kills everything. One reconnect state
  machine in the worker: exponential backoff 1 s→60 s with jitter; idempotent rejoin (relay
  handshake → pkarr republish → admission renew → automerge/CRR resync). Triggers are
  event-driven only — socket close/error, `visibilitychange→visible`, `online`,
  `pageshow(persisted)`, `resume` — deduped through the one machine (double-connect is the
  documented footgun).
- **Relay drives liveness**: WS protocol-level ping/pong is answered by the browser's network
  service with no JS wakeup — our relays ping browser clients server-side; iroh/QUIC idle
  timeouts for browser connections tolerate >2 min gaps so a tier-2 fallback isn't killed by
  the 1/min hidden-tab timer budget.
- **Freeze shield (desktop)**: every SHADW page holds a `navigator.locks` lock while the node
  is active — `kHoldingWebLock` is a CannotFreezeReason (WebSocket and WebTransport are
  **not** on that list). Deliberate inversion of the lifecycle doc's "release locks on
  freeze" advice. No inaudible-audio hacks.
- **Android is best-effort by design** (`kStopInBackground` freezes pages ~1 min after
  hiding regardless of connections): checkpoint aggressively, resync on resume, and message
  it as "node pauses in background".

### 2.8 Mesh admission & trust tier

Browser peers are a **third trust class, structurally disjoint from fleet trust**:

- **Own ALPN `hive/browser/0`** with its own accept path (iroh dispatches per-ALPN;
  reject early with an explicit `Connection::close(code)`). Today all five stream modes ride
  `hive/tunnel/0` behind one mode byte and one 64-hex TrustSet — a browser id in that set
  would be one byte from control-plane gossip. The browser handler contains **no dispatch
  arms** for GOSSIP/GOSSIP_SIGNED/JOIN/RAW/RAW_TARGET — unreachable by construction. Browser
  ids are **never inserted into TrustSet**, so even a mis-routed connection hits the existing
  untrusted-peer drop and the signed-gossip signer check. `hive/tunnel/0` changes not at all.
- **Admission** copies the hot-join template into a separate store: platform JWT →
  `POST /v1/mesh/browser-admission` on the leader → signed record
  `{eid, team, scopes[], issued_ms, exp_ms (30–60 min, renewed over the live connection),
  nonce}` bound to the TLS-handshake-authenticated endpoint id, written to a **replicated**
  `BrowserAdmissions` store (never node-local — the browser dials an arbitrary node; the
  round-robin rule applies), returned with the relay URL set.
- **Exactly four scoped surfaces**, all team-scoped by the record: (a) tenant-gateway HTTP
  for the team's own deployments (no privilege above an internet client); (b) automerge/CRR
  sync for docs/DBs the team owns; (c) content-addressed blob get/put in team namespaces;
  (d) team pubsub/presence. Native→browser dials verify the fleet dialer against a signed
  fleet-id attestation included in the admission response — never a roster fetch.
- **Two-level revocation** (Tailscale's lesson: revoking the credential does not deauthorize
  the node): tombstone the replicated record **and** every node immediately closes live
  `hive/browser/0` connections for that eid (libp2p `disallow_peer` semantics); third layer:
  deny the eid at the embedded relays via `AccessControl` — for a relay-only peer that is a
  fleet-wide kill switch. Records GC 30–60 min after last disconnect (ephemeral-node model)
  off the relay's `OnDisconnectGuard` signal.
- **Caps at the node accept path** (relay `accept_conn_limit` is a documented no-op):
  per-eid concurrent connections (2–3), lower `max_concurrent_bidi_streams` on the browser
  ALPN, per-eid inflight-stream and byte budgets metered to the team for billing, per-team
  admission-record count caps at mint.

### 2.9 Relay prerequisites (fleet-side, blocking)

1. **TLS.** `wss://` on every relay browsers will touch (direct certs or fronting) — without
   this nothing else in this document runs from an https page.
2. **Close the open relay.** The embedded relay currently runs the `AllowAll` default —
   every fleet node is an open public relay. Set `relay_cfg.access = Arc::new(ShadwRelayAccess)`
   (custom `DynAccessControl`): allow fleet trust-set ids unconditionally; otherwise require a
   valid SHADW-minted token via `request.auth_token()`. Verification must be local/offline —
   the hook blocks registration.
3. **Token design fits the wasm constraint**: browsers cannot set WS headers, so the token
   rides `?token=` in the upgrade URL and **will appear in relay logs** — mint short-TTL,
   signed, offline-verifiable tokens (claims: account, expiry, optionally the browser pubkey
   so a leaked token can't be replayed from another key). Client side is stock
   `RelayConfig::with_auth_token` (native header / wasm query param, tested upstream in 1.0.2).
4. **Per-account connection bookkeeping** in the same AccessControl (`ConnectionId` count in
   `on_connect`, decrement in `on_disconnect` — guard-based, fires on drop); eviction via
   `server.relay_service().clients().disconnect(eid, None)`. This is what makes the
   already-armed per-connection 16 MiB/s `ClientRateLimit` a per-account bound.
5. **What the hook cannot do**: it fires post-TLS+WS-upgrade+crypto-handshake and
   `ClientRequest` has no remote address — per-IP/connection-rate flood defense lives in
   front (nftables hashlimit on the relay ports) or via the sanctioned `RelayService::new`
   embedding seam. Don't bump iroh-relay for any of this (1.0.3 adds nothing relay-side).
6. **Standalone transition-era `:3340` binaries** (bkk/va/sj): audit and either configure
   access (the stock binary has **five** modes — `everyone` (default), `allowlist`,
   `denylist`, `http`, `shared_token`; **allowlist over fleet ids is the natural fit**) or
   decommission — otherwise they remain a parallel open door. (Implementation note: the http
   mode's checker header is actually `X-Iroh-NodeId`, despite docs claiming
   `X-Iroh-Endpoint-Id`.)

---

## 3. Security model

Defense in depth, innermost to outermost:

1. **Process boundary**: tenant code in a cross-site sandboxed iframe (opaque origin, own
   sandboxed renderer process; Site Isolation defends against Spectre-class reads and
   compromised renderers). Invariant: **never `allow-same-origin`** on a tenant frame; the
   sandbox host is its own registrable domain. Hard kill = `worker.terminate()`.
2. **Engine limits** (metered tiers): QuickJS memory/stack/interrupt quotas inside the same
   frame; the unaudited engine's escape lands in an opaque-origin process holding nothing.
3. **Capability ops**: the only door out of a guest is the op table; permissions are
   host-side per-context data; denied ops **throw** (our stubs, not Deno's silent no-ops);
   `read_to_end` size caps on every stream; no side effects on 0.5-RTT data.
4. **Key custody**: seed wrapped by a non-extractable AES-GCM key in IndexedDB; honest threat
   model — protects at-rest and off-origin, not live XSS; CSP + worker confinement are the
   XSS levers; rotation is cheap and designed-in; browser ids are low-privilege everywhere
   server-side.
5. **Trust tier**: browser ids never in TrustSet; control-plane surfaces
   (gossip::dispatch arms, JOIN, RAW splice — an SSRF primitive if reachable) unreachable by
   construction on the browser ALPN; four team-scoped surfaces only; short-TTL replicated
   admission records bound to the TLS-proven id.
6. **Revocation**: record tombstone + immediate connection teardown + relay-level deny (hard
   kill for a relay-only peer).
7. **Relay gating**: token-gated admission, per-account connection caps, per-connection
   ingress rate limit; per-IP defense at the OS/front layer; open-relay default closed
   fleet-wide, embedded and standalone both.
8. **Tenant-data invariants inherited from AGENTS.md**: tenant strings never become
   host/mount/dataset names; the same boundary-validation rule applies to VFS paths, CAS
   keys, and doc ids in every new browser-facing surface.

---

## 4. Implementation plan (ordered, mapped to PRD rows)

Relay TLS (§2.9.1) is a fleet-side prerequisite tracked with `bn-impl-mesh-admission` but
must land before `bn-verify-e2e` can run at all.

1. **`bn-impl-crate-scaffold`** — `crates/hive-browser` per §2.1: Cargo.toml/config.toml
   exactly as specified, wasm-bindgen build pipeline, `wasm-tools` import-leak CI guard,
   endpoint builder with custom pkarr wiring, echo-class ProtocolHandler proving inbound
   accept in a real browser. Exit: wasm artifact connects to a wss relay and completes a
   bi-stream round-trip with a fleet node.
2. **`bn-impl-key-persistence`** — §2.2 wrap-at-rest scheme, boot/load/zeroize path, rotation
   flow incl. signed handover; stable EndpointId across reloads witnessed in IndexedDB +
   `/v1/…` echo.
3. **`bn-impl-protocol-handler`** — `hive/browser/0` accept path on the fleet side (no
   gossip/join/raw arms) + browser-side Router registration; stream discipline (finish/reset,
   size caps, writer-first, idle-timeout tolerance) encoded here once and reused by every
   later surface.
4. **`bn-impl-function-runtime`** — §2.3: sandbox domain + iframe/Worker substrate,
   postMessage broker, ops table with batched completions, throwing denied-op stubs, pending
   predicate + unresolved-promise termination; QuickJS metered variant nested behind the same
   ops ABI.
5. **`bn-impl-node-compat`** — §2.4: clean-room CommonJS request wrapper inside the
   existing sandbox, explicit Express/http/fs/path subset, artifact-scoped VFS host op,
   and loud unsupported-module/listen boundaries.
6. **`bn-impl-sqlite-automerge`** — §2.5: sqlite wasm + sahpool worker, Web Locks writer
   election, vendored superfly/cr-sqlite wasm link, `crsql_changes` exchange over the browser
   ALPN, fleet-side native extension load; automerge slim + iroh NetworkAdapter + samod
   fleet peer for doc-shaped state.
7. **`bn-impl-asset-store`** — §2.6: shared wasm BLAKE3, OPFS durable CAS,
   Cache-backed service-worker GET/HEAD/206/416 serving, and chunked exact-digest
   AssetGet with per-endpoint grants and final re-verification.
8. **`bn-impl-mesh-admission`** — §2.8 + §2.9: admission mint endpoint + replicated
   BrowserAdmissions store, two-level revocation + relay AccessControl deny, embedded-relay
   access closed, standalone relay audit, per-eid caps, relay token issuance in the session
   flow.
9. **`bn-impl-ui-page`** — the node page itself: SharedWorker topology (§2.7), reconnect
   state machine, Web-Lock freeze shield, CSP, persist() prompt UX, status surface
   (connection, storage, admission TTL, tier).
10. **`bn-verify-e2e`** — no-mocks witness per repo rule: a real browser (Chrome + Safari at
    minimum) joins via a wss relay, deploys/serves a function to a fleet-originated request,
    syncs a cr-sqlite row both directions, replicates + range-serves a video asset offline,
    survives tab close/reopen with the same identity, and is revoked live (connection drops +
    relay denies within one sweep). Contrast test: an unadmitted endpoint id is refused at
    relay and ALPN.

---

## 5. Open questions requiring LIVE measurement (not more reading)

1. **Renderer-process reality of opaque-origin sandboxed iframes** per browser
   (`chrome://process-internals`, Fission, Safari) with our actual sandbox domain — Chrome's
   guarantee is per-site; same-site sandboxed frames may share the parent process.
2. **Iframe cold-start + postMessage RPC throughput** (transferables) on
   Chrome/Firefox/Safari, desktop + Android — no published numbers exist for our shape.
3. **QuickJS-wasm throughput class** on representative edge functions (folklore says 10–50x
   vs JIT), per-context creation time, and limit-hit behavior (memory-limit overrun,
   interrupt firing); also JSPI shipping status — broad JSPI availability would obsolete the
   Asyncify tradeoff entirely.
4. **JS↔wasm op-call overhead crossover** — the batch size at which batching stops paying,
   in 2026 browsers.
5. **MemStore/OPFS ceilings**: wasm linear-memory limits vs realistic asset sets;
   `Blob.slice()` laziness on >2 GB OPFS-backed Files in Safari/Firefox; whether current
   Chrome bypasses the SW for media-element range *continuation* requests (needs a `<video>`
   seek test).
6. **SharedWorker timer fidelity over hours** on hidden desktop tabs incl. OS sleep/wake;
   whether a page-held Web Lock blocks Android's renderer-side freeze (source suggests it
   does **not**); Finch-enabled freezing (`kInfiniteTabsFreezing`, battery-saver,
   `kFreezeSharedWorker`) on 2026 stable despite tree defaults; Safari/Firefox lifetime
   behavior wholesale (the investigation was Chromium-only).
7. **Relay accounting**: one hop or two when browser and consumer home to different relays
   (3x vs 4x economics); relay-side WS ping cadence vs a frozen renderer's network-service
   pong behavior.
8. **iroh 1.0.2 wiring checks before coding**: embedded-relay `AccessControl` exposure
   through the `RelayConfig` path main.rs already uses; browser-side dial with a
   caller-chosen ALPN over ws_stream_wasm; `conn.alpn()` shape in the hand-rolled accept
   loop; per-connection vs per-endpoint `client_rx` bucket granularity under
   reconnect-deactivation.
9. **Storage grants in the field**: persist() grant rates (Chrome heuristics vs Firefox
   prompt), Safari 7-day eviction vs home-screen-installed pages, and real quota floors on
   older Safari still in circulation.
10. **cr-sqlite build proof**: superfly fork compiled against SQLite 3.53+ wasm and loaded
    alongside the iroh wasm module in one page — asserted feasible from vlcn's js/ recipe,
    never build-tested; plus the row/column-granularity atomicity caveat (a peer can observe
    half of another peer's transaction) verified against docs before any multi-row invariant
    relies on it.

---

## Appendix A — Completeness critic: verified gaps (verbatim)

Produced by an adversarial completeness pass against the actual tree. Each gap is
now either its own PRD row or folded into an existing one (see section 4 map).

Verified against the actual tree before critiquing: `hive-p2p/src/lib.rs` (single-ALPN `.alpns(vec![HIVE_ALPN])` at line 1487, hand-rolled accept loop at 1751 — no `Router`/`ProtocolHandler` fleet-side), `hive-cloud/src/main.rs` embedded-relay wiring (lines 360–406, `ServerConfig::default()`, no `access` field touched, `HIVE_RELAY_CLIENT_BPS` per-connection only), `billing.rs` `RateCard` (no byte/relay meter exists). Gaps below are ordered by how hard the implementation trips.

```markdown
- id: gap-fn-routing-pillar
  gap: "No design anywhere for how a request FINDS a browser-hosted function: no announce/registration of 'eid serves function F', no LB across N tabs of one team, no failover to fleet when the tab froze mid-request, no integration with lease.rs/decide_lease/circuit machinery, and no answer to whether PUBLIC ingress can reach a browser app at all (gateway backhaul over hive/browser/0?) or only mesh-internal callers."
  why: "bn-verify-e2e step 10 literally requires 'serves a function to a fleet-originated request' — the witness plan references a subsystem no PRD row builds. A frozen tab holding a live lease also re-creates the exact CAPACITY_EXHAUSTED-vs-CIRCUIT_OPEN misattribution AGENTS.md documents."
  close: "New PRD row (bn-impl-invoke-routing) before bn-impl-mesh-admission: serving-set registration rides the admission record or a scoped announce op; fleet gateway treats browser targets as a lease backend with its own circuit class and a hard fleet-fallback; explicitly decide public-ingress-reachable vs team-internal-only."

- id: gap-sandbox-vs-sw-offline-contradiction
  gap: "§2.3 and §2.6 are mutually exclusive as written: an opaque-origin sandboxed iframe (no allow-same-origin) CANNOT register a service worker, and the parent origin's SW does not intercept a cross-site iframe's subresource fetches. So the SW-206-Range serving path cannot feed tenant apps running in the sandbox substrate."
  why: "'Serves itself offline' and 'hosts node-style apps' both route through this hole; discovered at bn-impl-asset-store integration time it forces a substrate redesign."
  close: "Decide the app-hosting origin model now: (a) per-tenant real subdomains on the sandbox eTLD+1 with their own SWs (weaker isolation, needs wildcard cert + origin-allocation scheme), or (b) assets pushed into the frame via postMessage/transferables + blob: URLs (no SW inside), keeping the SW only for the trusted host origin. Write the chosen model into §2.3/§2.6 and the e2e witness."

- id: gap-fleet-accept-loop-not-router
  gap: "Fleet side has no Router: hive-p2p binds exactly one ALPN and serve_tunnels() is a hand-rolled accept loop with a shared max_inbound_conns semaphore. Adding hive/browser/0 means extending .alpns(), dispatching on conn.alpn() inside that loop, and — critically — a SEPARATE accept/concurrency budget so admitted-browser floods can't starve fleet trunk accepts."
  why: "§4.3 assumes a register-a-handler shape that doesn't exist here; the shared semaphore makes the browser ALPN a DoS lever against control-plane trunks even with per-eid caps."
  close: "Scope bn-impl-protocol-handler to include the serve_tunnels refactor with per-ALPN semaphores; verify conn.alpn() availability on iroh 1.0.2's accepted connection before coding (currently buried in open-Q8, needs to be a blocking pre-check)."

- id: gap-relay-byte-metering-hole
  gap: "Browser↔browser traffic relayed peer-to-peer never touches any node's ALPN accept path — §2.8's 'per-eid byte budgets metered at the node accept path' misses it entirely. Relay-side there is only the per-connection 16 MiB/s ClientRateLimit; §2.9.4 counts connections, not bytes, and whether iroh-relay 1.0.2 exposes per-client byte counters at all is unverified."
  why: "Two admitted browsers = a free E2E-encrypted TURN service on fleet bandwidth; also unbillable, and RateCard has no byte/relay meter to bill it against even if counted."
  close: "Verify iroh-relay per-client accounting hooks (or the RelayService::new embedding seam) before committing the admission design; add a relay-bytes meter to RateCard + MetricsStore; if per-client byte counts are unobtainable, cap browser↔browser via admission scopes (fleet-terminated flows only)."

- id: gap-browser-metadata-transport-contradiction
  gap: "§2.5 says the browser 'gossips' persist()/estimate() as node metadata, but §2.8 makes gossip unreachable by construction on the browser ALPN. No sanctioned transport for browser status exists, and nothing states that browser eids are excluded from /v1/nodes, schedule.rs candidate sets, dns lb_records, spawn_health_loop probing, relay-hint selection, and anti-entropy."
  why: "A browser eid leaking into NodeInfo-consuming paths gets health-probed over iroh (garbage per-observer verdicts), considered for placement, or published into geo-DNS — each an incident class AGENTS.md already documents for real nodes."
  close: "Define a status op on the browser ALPN (piggyback admission renewal); add an explicit invariant + witness to bn-impl-mesh-admission: browser eids never enter NodeInfo/registry paths — grep-level assertion plus a live /v1/nodes check with a connected browser."

- id: gap-admission-replication-race-and-renewal-write
  gap: "Mint is leader-forwarded (fine), but the browser dials an arbitrary node milliseconds later — BrowserAdmissions replication lag → spurious rejection has no retry/grace design. Renewal 'over the live connection' lands on a non-leader node and is a WRITE (extends exp_ms) — per the round-robin rule it must leader-forward or fanout, unstated. Revocation SLA (tombstone propagation bound) has no number and no witness."
  why: "This is exactly the node-local-state-behind-round-robin failure class the repo has hit four times; the e2e row tests revoke but not the mint→first-dial race or renewal-through-non-leader."
  close: "Specify: accept-side re-check with one leader-proxy fallback on miss (build on fetch_from_host pattern); renewal forwards to leader; revocation witness measures tombstone-to-connection-teardown latency across ≥2 nodes."

- id: gap-admission-session-decoupling
  gap: "Admission lifetime is decoupled from the platform session: renewal authz is unspecified (JWT re-presented, or does a live connection self-renew forever after logout?), and team-membership removal or user deprovisioning doesn't feed the revocation path."
  why: "A fired employee's still-open tab keeps a valid, self-renewing mesh credential for their ex-team's DBs/blobs until someone notices — the exact Tailscale lesson §2.8 cites, one layer up."
  close: "Renewal requires a fresh platform-session proof each time (not just possession of the live connection); wire team-membership mutation events into the tombstone path; add both to the revocation witness."

- id: gap-tenant-gateway-ingress-path
  gap: "Scope (a) 'tenant-gateway HTTP, no privilege above an internet client' has no mechanism: which fleet code path does a browser-originated mesh request enter? If it enters the internal trunk-serving path (built for fleet-originated, already-trusted traffic) it may bypass public-ingress auth/rate-limiting; nothing prevents Host/target smuggling toward another team's deployment or loopback admin surfaces (mesh_raw-style SSRF one layer up)."
  why: "'Same privilege as an internet client' is an assertion, not a design — the two ingress paths in edge.rs/admin_ingress were never built to receive tenant-controlled traffic over the mesh."
  close: "Route scope-(a) requests through the SAME handler chain as public ingress with the team from the admission record pinned as the ONLY reachable namespace; witness with a browser peer attempting cross-team and 127.0.0.1-targeted requests."

- id: gap-blob-namespace-acl-missing
  gap: "'Content-addressed blob get/put in team namespaces' — stock BlobsProtocol serves any blob whose hash the requester knows; hash-as-capability fails here because hashes leak through team-readable manifests, and iroh-blobs has no namespace concept. No fleet-side ACL layer is designed."
  why: "Any admitted browser can pull any tenant's blob fleet-wide by hash — cross-tenant data exposure through the front door of §2.8's 'four scoped surfaces'."
  close: "Design a per-team hash-set gate wrapping the provider on the browser ALPN (custom protocol handler consulting a team→hash-set index) before bn-impl-asset-store; add a cross-team blob-fetch refusal to the e2e contrast tests."

- id: gap-relay-tls-provisioning-mechanics
  gap: "'TLS on every relay' has no mechanics: embedded relays are per-node on :3341 — they need stable DNS names (Seer? ns-<node> pattern?), certs (acme.rs integration; N per-node certs vs LE duplicate-cert windows the reconciler treats as incidents), and cert rotation on nodes that also serve wss to browsers. Same for the HTTPS pkarr relay §2.1 requires — no row provisions it."
  why: "This is the blocking prerequisite for everything, and it intersects two systems with documented sharp edges (ACME rate-limit incidents, Vercel DNS record-conflict invariants). 'Direct certs or fronting' is a fork, not a plan."
  close: "Dedicated PRD row: pick fronting-vs-direct, enumerate relay hostnames + cert issuance path through the existing ACME machinery, include the pkarr relay, and gate bn-impl-crate-scaffold's exit criterion on a browser connecting to a REAL fleet wss relay, not a dev one."

- id: gap-pkarr-publish-decision
  gap: "Whether the browser should publish to pkarr at all is undecided: publishing puts short-lived browser eids into shared fleet discovery (pollution + eid enumeration surface); NOT publishing means fleet→browser dials must come exclusively from the relay URL in the admission record — which §2.8 half-implies but never states."
  why: "PkarrPublisher wired by default (§2.1) makes the leak the default; discovery pollution and 'who can enumerate browser peers' are both trust-tier questions."
  close: "Decide: browser builds resolver-only, no publisher; fleet dials browsers via admission-record relay URL. One-line change now, an eid-enumeration surface later."

- id: gap-crsqlite-fleet-side-unexamined
  gap: "'Fleet nodes load the same extension natively' assumes a fleet-side sqlite runtime that doesn't exist in hive-cloud (state rides GuardianDB/iroh-docs + persist snapshots; no rusqlite in the tree). Fleet-side cr-sqlite means: new native dep across BOTH glibc groups, per-tenant DB files on fleet disks (new storage surface — placement disk floor, GC keep-set, retention policy all re-apply), and a sync-scheduler for which fleet nodes replicate which tenant DBs."
  why: "Half of §2.5's replication story is a fleet feature with zero design and zero PRD row; the browser half is unshippable without it."
  close: "New row (bn-impl-fleet-crr-peer) covering the rusqlite+extension build for both glibc groups, tenant-DB placement/GC per the storage invariants, and replication-factor policy; same treatment for the samod automerge peer (where do fleet-side doc bytes persist, who bills them)."

- id: gap-sahpool-worker-topology-unverified
  gap: "opfs-sahpool needs createSyncAccessHandle, which is exposed in DEDICATED workers only — a SharedWorker cannot host it. §2.7 says sqlite 'lives in workers' next to the SharedWorker endpoint, but a dedicated worker is tab-owned unless spawned as a nested worker of the SharedWorker — and nested-worker-in-SharedWorker support in Safari is unverified. Also unaddressed: sahpool pre-allocates a fixed pool capacity (growth beyond it errors), and no budget/eviction priority exists among CAS + sqlite + automerge sharing one origin quota (quota pressure kills the sqlite writer mid-CRR-transaction first)."
  why: "The single-writer election design silently assumes a worker topology that may not exist on one of the two witness browsers; quota exhaustion is the browser analog of the fc-sanjose disk-full incident."
  close: "Add to §5 live measurements: nested dedicated worker under SharedWorker on Safari/Firefox; specify sahpool capacity sizing + a CAS-first eviction policy driven by estimate() headroom."

- id: gap-billing-model-absent
  gap: "No metering/billing design for anything browser-side: relay bytes (see gap-relay-byte-metering-hole), fleet-side CRR/automerge storage, admission-record churn, or browser-executed function invocations (does the tenant pay less for donating compute? does fluid_ms even apply when the fleet ran nothing?). RateCard has no applicable meter."
  why: "'Metered to the team for billing' appears once with no mechanism; economics is a stated pillar (§1.4) with no accounting substrate."
  close: "Add rate-card entries + the counting chokepoint (per AGENTS.md: count at the one chokepoint, not per-caller — the browser-ALPN accept path and relay AccessControl are the two chokepoints) as part of bn-impl-mesh-admission."

- id: gap-e2e-witness-holes
  gap: "bn-verify-e2e never witnesses the adversarial half: an ADMITTED-but-hostile browser flooding streams to exhaust the shared accept semaphore, cross-team blob/DB/gateway access attempts, admission replay from a second eid, revocation-latency bound, quota-exhaustion behavior, or Firefox at all; Android 'best-effort' claims (§2.7) have zero witness plan despite shaping the messaging."
  why: "Every contrast test listed is unadmitted-vs-admitted; the trust tier's actual claims are about what an ADMITTED low-trust peer cannot do."
  close: "Extend bn-verify-e2e with an abuse suite (admitted-peer flood, cross-team probes, revoke-latency measurement) and add Firefox + one real Android device to the browser matrix."

- id: gap-version-skew-protocol-evolution
  gap: "Tabs live for days across fleet rollouts; no versioning story for the browser ALPN wire formats, the ops ABI, or the wasm bundle (forced-refresh nudge, min-supported-version gate at admission)."
  why: "First protocol change after launch strands a population of stale tabs mid-handshake with undiagnosable failures — the fleet controls its own rollout order, but not the tabs'."
  close: "Version the ALPN payloads from day one (version field in the admission handshake, reject-below-floor with an explicit close code the UI turns into a refresh prompt); cheap now, a migration later."
```

The two that invalidate written sections (not just add work): `gap-sandbox-vs-sw-offline-contradiction` (§2.3 vs §2.6 are incompatible as specified) and `gap-browser-metadata-transport-contradiction` (§2.5 vs §2.8). The biggest unbuilt pillar with a witness already depending on it is `gap-fn-routing-pillar`.
