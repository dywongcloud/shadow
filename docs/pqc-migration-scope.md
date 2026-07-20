# RFC: Post-Quantum Cryptography Migration Scope for the Hive Mesh

- **Status**: Draft for review (research deliverable; no code changes made)
- **Date**: 2026-07-20
- **Scope**: every iroh-anchored cryptographic identity, signature, MAC, and key file in `/Users/dylanwong/fluid/hive`; PQC readiness of the locked dependency stack; a phased, mixed-fleet-safe migration plan
- **Inputs**: three research passes — (1) crypto-surface inventory of the codebase, (2) dependency-side PQC readiness analysis (verified against in-lock crate sources), (3) migration-strategy design (threat model, dual-sign wire design, size/perf math). Cross-report factual conflicts were re-verified against source and resolved in Appendix C.
- **Key locked versions** (`Cargo.lock`, verified): iroh 1.0.2, iroh-relay 1.0.2, rustls 0.23.40, ring 0.17.14, aws-lc-rs 1.17.0, pkarr 3.10.0, ed25519-dalek 2.2.0 + 3.0.0-rc.0, vendored guardian-db 0.18.0.

---

## 1. Executive summary

The hive mesh's entire trust fabric hangs off a single classical primitive: **the ed25519 public key that *is* the iroh NodeId**. It authenticates every QUIC connection (TLS 1.3 raw-public-key handshake inside iroh), anchors the peer-trust allowlist, binds the signed-gossip trailer, channel-binds the STREAM_JOIN HMAC proof, keys pkarr discovery records, and authorizes ~104 mesh RPC dispatch arms in `gossip.rs`. Key exchange is classical X25519 via the ring provider — **zero post-quantum protection today**.

The quantum risk decomposes into three buckets with very different urgency:

1. **Harvest-now-decrypt-later (HNDL) — bleeding now.** Recorded mesh traffic (tenant env secrets, store_sync contents, TLS private-key bundles via `acme.rs:461-486`, deploy payloads) becomes retroactively decryptable the day a cryptographically relevant quantum computer (CRQC) exists. This is fixed by hybrid key exchange — and it is **nearly config-only**: hive's exact locked versions (iroh 1.0.2 + rustls 0.23.40 + aws-lc-rs 1.17.0, already compiled into the binary) clear every bar for X25519MLKEM768. **Phase 0 closes the only retroactive exposure at roughly one engineer-week of cost, with automatic classical fallback making it flag-day-free.**

2. **Signature forgery — exploitable only at CRQC time, but with a deadline that arrives earlier.** A PQ key binding recorded while ed25519 is still unbroken is trustworthy; one established after is not. So **PQ key enrollment must happen well before the horizon**, even though PQ signature *enforcement* can wait. Phase 1 adds ML-DSA-44 enrollment plus a dual-signed gossip v2 — entirely in hive-owned code, reusing the staged sign/log/enforce rollout machinery the fleet already exercised once for ed25519 gossip signing.

3. **Transport identity — structurally unfixable locally.** iroh's NodeId is definitionally a 32-byte ed25519 key, hardwired through TLS RPK verification, relay handshake frames, tickets, discovery, and pkarr's 1000-byte/fixed-offset wire format. A local fork is a 2-4 engineer-month initial effort with permanent divergence cost, racing an upstream (n0) team that has explicitly scoped the same migration and is waiting on industry consensus. **Phase 2 recommendation: do not fork.** Phase 1's cross-attested ed25519↔ML-DSA binding converts "CRQC impersonates any node" into "CRQC must also break ML-DSA" for the control plane, which contains the blast radius until upstream ships PQ identity.

Recommended sequencing: **Phase 0 now** (primary + secondary TLS legs), **Phase 1a (enrollment) on the next binary train**, Phase 1b-1d behind per-peer capability gating, **Phase 2 tracked upstream**. Symmetric surfaces (STREAM_JOIN HMAC, artifact HMACs, webhook MACs, ChaCha20-Poly1305 at-rest sealing) are already quantum-resistant and need no PQ work — only two classical caveats (secret entropy, deterministic join proof) folded into Phase 1.

---

## 2. Threat model (condensed)

| Threat | Applies | Mechanism | Fix | Phase |
|---|---|---|---|---|
| HNDL traffic recording | **Today** | X25519-only TLS 1.3 key exchange (ring provider) on all iroh QUIC | Hybrid X25519MLKEM768 KEM | 0 |
| ed25519 signature/identity forgery | At CRQC | Shor recovers the seed from the public NodeId → attacker owns transport identity, passes trust set, satisfies join-proof channel binding, forges gossip trailers — **total collapse is coupled** because the trust anchor IS the ed25519 key | PQ enrollment + dual-sign + ratchet (control plane); upstream PQ identity (data plane) | 1, 2 |
| Symmetric MAC/AEAD breakage | Not credible | Grover gives ≥2^128 effective on HMAC-SHA256 / ChaCha20-Poly1305, and parallelizes poorly under depth limits | None needed; entropy audit only | 1c |
| Post-CRQC rogue PQ enrollment | At CRQC | Attacker with a Shor'd ed25519 key enrolls their *own* ML-DSA key | Enroll early; first-seen pinning + ratchet | 1a/1d |

Critical asymmetry: **the KEM problem accumulates damage every day; the signature problem has a cliff.** Phase ordering follows directly.

---

## 3. Complete surface inventory

Consolidated from the file:line-level inventory (report 1), re-verified where reports disagreed. "Math in" = where the actual cryptographic computation executes; policy/wire-format columns note what hive owns regardless.

### 3.1 Mesh transport and identity (hive-p2p + iroh)

| # | Surface | Location | Primitive | Math in | Hive-owned parts |
|---|---|---|---|---|---|
| T1 | QUIC connection identity (`conn.remote_id()`), all dial paths | `crates/hive-p2p/src/lib.rs:1617-1620`, `:706, 752, 1540` | TLS 1.3, RFC 7250 raw public key, ed25519 hardwired (`SignatureScheme::ED25519` pinned in iroh's resolver/verifier); X25519 ECDHE via ring | iroh (iroh-quinn/noq + rustls + ed25519-dalek) | consumption of the resulting id only |
| T2 | Endpoint bind — **no crypto provider set** | `lib.rs:1333-1385` (`bind_full`); iroh `presets.rs:66-74` picks ring | provider selection | iroh | Phase 0 insertion point: `Builder::crypto_provider()` (iroh `endpoint.rs:761`) |
| T3 | Persisted mesh identity | `lib.rs:1507-1530` (`load_or_create_secret`), path `$HIVE_DATA/iroh_secret.key` via `hive-cloud/src/main.rs:234` | 32 B ed25519 seed, 0600; **corrupt file silently regenerated = new identity** | keygen in iroh | file handling, perms, regeneration policy |
| T4 | Peer-trust set (connection admission) | `lib.rs:415-421`, `:1626-1631`; seeded `state.rs:539-548` from `HIVE_TRUSTED_NODE_IDS`; admin pre-registration `admin.rs:3458-3475` (**64-hex validated**, `admin.rs:3466`) | none — the ed25519 pubkey *string* is the anchor; authenticity from T1 | — | entire policy; fails closed on empty set |
| T5 | Bootstrap seeds | `lib.rs:1259-1301`, `$HIVE_DATA/bootstrap_peers`, `HIVE_BOOTSTRAP_PEERS` | 64-hex node ids as first-contact trust anchors | — | parsing, trust decision |

### 3.2 Signed gossip (hive's own protocol — fully hive-owned wire format and policy)

| # | Surface | Location | Primitive | Math in | Hive-owned parts |
|---|---|---|---|---|---|
| G1 | Domain + preimage | `lib.rs:97` (`GOSSIP_SIG_DOMAIN = b"hive-gossip-v1"`), `:277-287` (`gossip_sig_preimage`: domain ‖ method ‖ len(path) ‖ path ‖ len(body) ‖ body ‖ ts_ms, u32-BE lengths) | domain-separated message construction | — | **everything** |
| G2 | Trailer sign/verify | `:291-298` (`sign_gossip`), `:303-347` (`verify_gossip`) — fixed **104 B** `[32B pk][8B ts][64B sig]`, hard `read_exact` at `:1762` | ed25519 | iroh-base → ed25519-dalek | trailer layout; ±300 s two-sided replay window; **signer==QUIC-remote binding** (`:338-344`) |
| G3 | Rollout machinery | `:223-239` (`HIVE_GOSSIP_SIGN`, `HIVE_GOSSIP_VERIFY` off/log/enforce, ts window), `:243-273` (`VerifyStats` → `/v1/relay`) | — | — | everything; **sign+enforce live fleet-wide** per `docs/LIVE_INFRA_RUNBOOK.md:146-149`, though compile-time defaults are sign-off/verify-log |
| G4 | Send/receive paths | `:1114-1125` (send), `:1733-1826` (`serve_gossip`: verify → second-layer trust-membership check `:1781-1795`) | ed25519 | iroh | wire protocol, policy |
| G5 | Mutation authorization | `crates/hive-cloud/src/gossip.rs:666-668` (`mesh_mutation_authorized`), `:689-704`, dispatch `:107-538` (~104 arms: deploy, project delete, TLS bundle, store mirrors, leases…) | policy over verified identity | — | everything — **the blast radius if signatures fall** |
| G6 | Mesh tenant delegation | `gossip.rs:582-619` (`?tok=` HS256 JWT via `auth.rs:68,82`), `:628-637` (operator-claims synthesis from P2P admission alone) | HMAC-SHA256 (JWT) | jsonwebtoken | trust-anchor decisions; documented residual: shared `HIVE_JWT_SECRET` lets any compromised trusted node mint any tenant |
| G7 | Unsigned legacy + data-plane streams | `STREAM_GOSSIP=0x02`, `STREAM_TUNNEL/RAW/RAW_TARGET` (`lib.rs:1661-1677`) | none — connection-level trust only | — | by design; falls with transport identity (Phase 2) |

### 3.3 Join / admission

| # | Surface | Location | Primitive | Math in | Notes |
|---|---|---|---|---|---|
| J1 | STREAM_JOIN framing + admission semantics | `lib.rs:56-64`, `:1158-1217`, `:1570-1660`, `:1684-1704` | — | — | only stream an untrusted conn may use; remote_id never body-derived |
| J2 | Join proof (server) | `main.rs:508-550` — `hmac_sha256_hex(HIVE_JWT_SECRET, remote_id)`; NodeInfo-identity match `:529-535`; **non-constant-time compare `:521`** | HMAC-SHA256 (**hand-rolled RFC 2104**, `admin.rs:3383-3410`, over dep sha2) | ours + sha2 | deterministic (no nonce/ts); channel-bound to ed25519 QUIC id; quantum-resistant as-is |
| J3 | Join proof (client) | `main.rs:1655-1688` | same | same | |
| J4 | Transitive trust growth | `main.rs:1700-1708` — every roster entry's EndpointId inserted into the trust set | — | — | trust closure: any trusted peer extends trust fleet-wide |

### 3.4 Discovery, relay, DNS

| # | Surface | Location | Primitive | Math in | Notes |
|---|---|---|---|---|---|
| D1 | pkarr publish/resolve (Plane B client side) | `lib.rs:1369-1382` | ed25519-signed pkarr records, same endpoint key | iroh + pkarr | |
| D2 | Seer pkarr relay (server) | `crates/hive-cloud/src/discovery.rs:97-110` (`SignedPacket::from_relay_payload`), replay `:63-73` | ed25519 verify; self-verifying records | pkarr 3.10.0 | **structurally dead for ML-DSA**: 1000-byte packet cap, fixed 32 B-key/64 B-sig offsets (`signed_packet.rs:294-305`), BEP-44 DHT hardcodes ed25519 |
| D3 | Relay client auth + embedded relay server | `main.rs:284-335` (`:3341`, plain HTTP, no TLS/ACL — **open relay**); standalone binaries bkk/va/sj `:3340` | ed25519 challenge sig, fixed 64 B wire fields (`iroh-relay-1.0.2/protos/handshake.rs`) | iroh-relay | relayed payloads stay e2e QUIC-encrypted |
| D4 | Seer DNS (Plane A) | `crates/hive-cloud/src/dnsserver.rs` | **none** — no DNSSEC, no pkarr | — | integrity chains entirely to mesh surfaces above |

### 3.5 GuardianDB (second + third identities per node)

| # | Surface | Location | Primitive | Math in | Notes |
|---|---|---|---|---|---|
| Gd1 | Guardian mesh identity | `vendor/guardian-db/src/p2p/network/core/mod.rs:518-555`; `$HIVE_DATA/guardian/iroh/node_secret.key` | 32 B ed25519 seed — **no 0600 chmod** (`:548`) | iroh | separate EndpointId from mesh (mixing caused a live retry-storm) |
| Gd2 | Guardian "main keypair" | `vendor/.../key_synchronizer.rs:144-229`, redb keystore | ed25519-dalek | vendored dep | |
| Gd3 | iroh-docs entry signing, BLAKE3 content addressing | consumed via `guardian.rs:228, 512-574` | ed25519 + BLAKE3 | deps | replication trust chains to mesh admission via `seed_peer` (`guardian.rs:256-272`) |

### 3.6 Adjacent surfaces riding the mesh

| # | Surface | Location | Primitive | Math in | PQ status |
|---|---|---|---|---|---|
| A1 | TLS wildcard private-key distribution | `acme.rs:461-486` (served **decrypted**), re-sealed at receiver `:490-520` | confidentiality = the iroh QUIC channel | — | **top HNDL asset**; fixed by Phase 0 |
| A2 | At-rest sealing | `secrets.rs:19, 61-77`, `$HIVE_DATA/secret.key` | ChaCha20-Poly1305 (ring) | ring | quantum-resistant |
| A3 | Build-artifact integrity | `git.rs:3009-3080` — SHA-256 digest + HMAC-SHA256 sig, **constant-time verify**, key `HIVE_ARTIFACT_SECRET` or derived `HMAC(HIVE_JWT_SECRET, "hive-artifact-signing-v1")` | HMAC | hmac/sha2 crates | quantum-resistant |
| A4 | Webhook verification | `admin.rs:3352-3358` | HMAC-SHA256 (hand-rolled) | ours + sha2 | quantum-resistant |
| A5 | zkauth ring signatures | `crates/hive-zkauth/src/lib.rs:30-160+` — LSAG over Ristretto255, hive-implemented | curve25519-dalek + Sha512 | ours + dalek | classical; discrete-log-based → falls at CRQC; out of scope here but flagged (§9 Q12) |
| A6 | JWT auth (`?tok=` mesh delegation) | `auth.rs:68, 82` | HS256 | jsonwebtoken | quantum-resistant (symmetric), entropy-bound |

### 3.7 Persisted key files (fleet node)

| Path | Content | Perms | Writer |
|---|---|---|---|
| `$HIVE_DATA/iroh_secret.key` | 32 B ed25519 seed — mesh identity | 0600 | `hive-p2p/lib.rs:1507-1530` |
| `$HIVE_DATA/guardian/iroh/node_secret.key` | 32 B ed25519 seed — guardian identity | **default umask** | vendored guardian-db |
| `$HIVE_DATA/guardian/iroh/keystore` (redb) | guardian main keypair + synced keys | redb | vendored guardian-db |
| `$HIVE_DATA/secret.key` | 32 B ChaCha20-Poly1305 at-rest key | 0600 | `secrets.rs:61-77` |
| `$HIVE_DATA/peer_iroh.json`, `peer_guardian_addr.json`, `bootstrap_peers` | identity-bearing routing caches / seed anchors (not secret) | — | `persist.rs:117, 146`; `main.rs:239-240` |

Phase 1 adds one file: the ML-DSA seed, written with the same 0600 `load_or_create_secret` pattern.

---

## 4. Classification: config-only vs our-code vs upstream-fork

| Class | What | Evidence | Cost character |
|---|---|---|---|
| **(a) Config-only** (Cargo features / one builder call) | Hybrid **X25519MLKEM768** KEM on: mesh QUIC, guardian-db QUIC, relay-client TLS, reqwest fallback legs, dashboard TLS. All required machinery is in the locked graph: rustls 0.23.40 has `X25519MLKEM768` unconditionally in the aws-lc-rs provider's kx list with `prefer-post-quantum` controlling priority (`rustls-0.23.40/src/crypto/aws_lc_rs/mod.rs:240-267`); aws-lc-rs 1.17.0 already compiled in; iroh ≥0.98 ships PQ handshakes behind `tls-aws-lc-rs`; noq handles multi-datagram PQ ClientHellos. **Blocker to avoid**: hive builds iroh with default `tls-ring`, and `vendor/guardian-db/Cargo.toml` declares iroh *with default features* — feature unification re-enables ring, and iroh's preset then prefers ring. The robust route is the public `Builder::crypto_provider()` hook (iroh 1.0.2 `endpoint.rs:761`), immune to unification. | report 2 §3.2, §5(a); in-lock source verification | days |
| **(b) Our-code-only** (no dependency changes) | (1) Gossip **dual-sign v2** — preimage, trailer, stream modes, verify policy, rollout flags, stats are all hive code (`hive-p2p/lib.rs`); (2) **PQ key enrollment** in NodeInfo/roster/STREAM_JOIN (all hive plumbing, incl. the join handler's NodeInfo upsert `main.rs:543`); (3) **JOIN v2** ts-binding + secret rotation; (4) **PQ-signed discovery records** via iroh 1.0's public `address_lookup` extension point + a new non-pkarr Seer format (HTTPS JSON, no 1000-byte cap); (5) downgrade **ratchet** + `enforce-pq` verify mode; (6) contingency app-layer AEAD over secret-bearing bodies (HKDF from `HIVE_JWT_SECRET` — symmetric ⇒ HNDL-immune independent of TLS). ML-DSA implementation source: aws-lc-rs `unstable` feature (mutually exclusive with `fips`) or the standalone RustCrypto `ml-dsa` crate — decision open (§9 Q2). | report 2 §5(b), report 3 §3 | weeks |
| **(c) Upstream-fork required** (recommend: don't) | **PQ transport identity**: NodeId type, TLS RPK resolver/verifier (ed25519 pinned), relay handshake fixed-width frames, tickets/`EndpointAddr`/gossip-membership serialization, iroh-dns naming — plus the NodeId type ripple through iroh-gossip/docs/blobs via vendored guardian-db. **pkarr/mainline DHT is not forkable in any meaningful sense** (BEP-44 hardcodes ed25519; sig alone is 2.4× the whole record budget) — it requires protocol replacement. rustls-webpki 0.103.13 already ships ML-DSA verification algs (`aws-lc-rs-unstable`) and rustls defines `SignatureScheme::ML_DSA_44/65/87` code points, so the *TLS layer* of a fork is tractable — the identity model is what isn't. Upstream (n0) has publicly stated it has scoped non-ed25519 EndpointIds and is waiting on industry consensus (draft-ietf-tls-mldsa, composite sigs). | report 2 §4, §5(c) | months + permanent divergence |

---

## 5. The identity = NodeId structural problem

**iroh's NodeId *is* the 32-byte ed25519 public key** (`iroh-base-1.0.2/src/key.rs:30`: `PublicKey(CompressedEdwardsY)`). This is not an implementation detail — it is the load-bearing assumption of the entire stack, and it is why Phase 2 cannot be done incrementally in hive code:

1. **TLS RPK binding**: the expected NodeId is byte-compared against the SPKI in the peer's raw-public-key "certificate"; `ED25519` is the only signature scheme the resolver/verifier accept. Identity verification and key possession are the *same operation*.
2. **Relay handshake**: challenge signatures ride in wire-fixed `[u8; 64]` fields next to 32-byte keys (`iroh-relay-1.0.2/protos/handshake.rs`).
3. **Discovery**: the pkarr record *key* is the pubkey and the DNS *name* is its z-base-32 (52 chars). Verification requires only the name — the key IS the name.
4. **Serialization everywhere**: tickets, `EndpointAddr`, gossip membership, iroh-docs authors — 32-byte or 52-char slots throughout.
5. **Hive's own config**: `HIVE_TRUSTED_NODE_IDS` / `HIVE_PEER_TRUST` (64-hex ids), `HIVE_BOOTSTRAP_PEERS` (`<64-hex>[@ip:port…][|relay]`), gossip trailer signer field, Seer store keys, `peer_iroh.json`.

**The size wall.** ML-DSA-44: pk 1,312 B (41× ed25519), sig 2,420 B (38×). A 1.3 KB key cannot *be* a 32-byte NodeId, and z32(1312 B) ≈ 2,100 chars vs DNS's 63-byte label limit. Identity must become a **fingerprint = hash(SPKI)** — which **inverts the trust flow**: today a verifier needs only the NodeId; with fingerprints every verifier must first *obtain* the full key. In-band in TLS that's fine (+~4 KB/side handshake, tolerable over QUIC, and the webpki/rustls verify plumbing already exists in the locked versions). Out-of-band — discovery records, gossip trailers, trust-set entries — it demands a new key-distribution/caching subsystem. pkarr is structurally dead: the signature alone is 2.4× the whole 1000-byte BEP-44 budget, and the mainline DHT itself verifies ed25519.

**The coupling consequence (why Phase 1 exists).** Because the trust anchor is the ed25519 key, a CRQC collapses everything *simultaneously*: Shor the public NodeId → own the QUIC identity → pass the peer-trust check → satisfy the join proof's channel binding → forge gossip trailers. Message-level PQ signatures are only meaningful if the trust anchor itself gains a PQ component **enrolled while ed25519 is still honest**. That is exactly what Phase 1's cross-attested binding provides: after `enforce-pq`, a forged-ed25519 "peer X" still cannot produce X's enrolled ML-DSA signature, so the control plane (all ~104 gossip dispatch arms) survives even though the raw transport identity does not. The data-plane streams (tunnels/raw), which authorize on transport identity alone, remain Phase 2 territory.

**Why not fork.** The fork inventory (iroh-base, iroh tls/endpoint/tickets, iroh-relay both ends, discovery replaced outright, NodeId ripple through the vendored guardian-db's iroh-gossip/docs/blobs) is 2-4 engineer-months of initial work racing an upstream team that has already scoped the same migration — with every peer needing lockstep upgrade or a dual-stack identity scheme, which is precisely the "industry consensus" problem upstream is waiting out. Only noq (QUIC itself) is identity-agnostic. A hive fork would likely be throwaway work superseded by a breaking upstream migration.

---

## 6. Phased migration plan

Rollout doctrine (all phases): two Linux glibc build groups (bkk/hk vs va/va2/va3/sj) + LA nodes; binaries sha256-verified and `.old`-backed before swap; every phase mixed-fleet-safe, observable via `/v1/relay` counters, and independently revertible. This mirrors the migration the fleet already executed once (unsigned → `HIVE_GOSSIP_SIGN=1` → `HIVE_GOSSIP_VERIFY=enforce`), with one deliberate improvement: **per-peer capability gating instead of global flag-day flips**, removing the coordination-only fragility the code itself flags at `hive-p2p/lib.rs:92`.

### Phase 0 — Hybrid KEM (kill HNDL). Effort: ~1-2 engineer-weeks total.

| Step | Work | Effort |
|---|---|---|
| 0.1 | In `bind_full` (`hive-p2p/lib.rs:~1342`): `.crypto_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))`; enable rustls `prefer-post-quantum` so X25519MLKEM768 is the client's *first* keyshare (without it PQ is negotiable but not offered). Optionally also flip iroh to `tls-aws-lc-rs` in `crates/hive-p2p/Cargo.toml` and de-default `vendor/guardian-db/Cargo.toml`'s iroh features — but the builder hook is the unification-proof mechanism; treat the feature flips as hygiene | 1-2 days |
| 0.2 | Same provider override on the guardian-db endpoint (`vendor/.../core/mod.rs:592` builds its own iroh endpoint — vendored, hive-owned) | 0.5-1 day |
| 0.3 | Verification: qlog/SSLKEYLOGFILE confirmation that X25519MLKEM768 negotiates node↔node; mixed-fleet test (upgraded↔old falls back to X25519 automatically — TLS group negotiation makes this **flag-day-free**); handshake-size regression check (~+2.3 KB once per pooled trunk; amortized ≈0 by the H4 trunk warmer) | 1-2 days |
| 0.4 | Fleet roll: two glibc groups + LA nodes, staggered | 2-3 days elapsed |
| 0.5 | Secondary TLS legs: reqwest HTTP-gossip/admin-forward fallback, standalone iroh-relay binaries on bkk/va/sj (rebuild with `tls-aws-lc-rs`; note relayed traffic's inner QUIC session is what actually matters for HNDL), axum-server dashboard TLS | 2-3 days |

Risk: near-zero. No wire-protocol change, no new key material, no env staging. This is the rare case where the most urgent item is also the cheapest (urgency×feasibility score 25/25 in the strategy analysis).

### Phase 1 — PQ enrollment + dual-signed gossip (control-plane PQ authenticity). Effort: ~4-6 engineer-weeks code + 2-4 weeks staged observation.

**1a. PQ key enrollment (the piece with the invisible deadline) — ~1 week.**
Extend `NodeInfo` (already gossiped every 5 s roster round and upserted at STREAM_JOIN admission) with:
- `pq_pub`: ML-DSA-44 pk (1,312 B raw ≈ 1,750 B base64 — noise vs roster JSON);
- `pq_attest_ed = Ed25519.sign(id_key, "hive-pq-bind-v1" ‖ eid ‖ mldsa_pk)`;
- `pq_attest_pq = ML-DSA.sign(mldsa_sk, ctx="hive-pq-bind-v1", eid ‖ ed_pk)`.

Both directions are required: ed→pq stops a rogue PQ key being bound to your identity; pq→ed stops your PQ key being adopted under another identity. Seed persists beside the iroh seed (0600, same `load_or_create_secret` pattern). Verifiers pin first-seen bindings (TOFU + ratchet) — sound because rosters already flow over enforce-mode signed gossip. STREAM_JOIN is the enrollment ceremony for new nodes.

**1b. Dual-sign v2 wire + capability-gated send — ~1.5 weeks.**
The 104-byte `read_exact` (`lib.rs:1762`) means v1 receivers cannot tolerate a longer trailer, and an old binary routes unknown mode bytes to the tunnel arm — so v2 is a **new stream mode `STREAM_GOSSIP_SIGNED_V2 = 0x06`** with a length-prefixed, suite-agile trailer:

```
[u32 trailer_len][u8 suite_id=0x01 (Ed25519+ML-DSA-44)]
[32B ed25519 pk][8B ts_ms][64B ed25519 sig][2420B ML-DSA-44 sig]   = 2,529 B
```

Both signatures cover the **v2 domain** preimage `"hive-gossip-v2" ‖ suite_id ‖ method ‖ len(path) ‖ path ‖ len(body) ‖ body ‖ ts_ms` (ML-DSA additionally uses FIPS 204 ctx). The ed25519 sig using the v2 domain is the anti-downgrade combiner: a v2 message cannot be stripped to a valid v1, and a captured v1 sig cannot be promoted into v2 (standard "AND" hybrid; unforgeable if *either* scheme holds once enforce-pq requires both). ML-DSA pk is **not** carried per-message — roster enrollment (1a) is the lookup, which is what makes the ratchet possible. Replay window and signer==QUIC-remote binding carry over unchanged. Sender emits 0x06 **iff the peer's freshest NodeInfo advertises `pq_pub`** (capability learned within one 5 s round), else 0x03 — no flag-day; `HIVE_GOSSIP_SIGN_V2=off` exists only as an abort valve. New `VerifyStats` counters: `v2_ok / v2_bad_mldsa / v2_missing_enrollment / v1_from_v2_capable`.

**1c. JOIN v2 + secret hygiene — ~1-2 days, batched into the same binary.**
Add `ts_ms` to the join-proof HMAC preimage (reusing the 300 s window) to close the CRQC proof-replay path (today's proof is a per-endpoint constant, replay-safe only via the *classical* channel binding); enroll `pq_pub` at admission. Audit/rotate `HIVE_JWT_SECRET` to ≥32 CSPRNG bytes (the HMAC's 2^128 quantum floor holds only at full key entropy — this is a *classical* exposure today). Fix the non-constant-time proof compare (`main.rs:521`) while in the file.

**1d. Ratchet + `enforce-pq` — ~3-5 days code, armed but dormant.**
Persist a per-peer v2-seen bit beside the trust set: once a receiver verifies any v2 message from endpoint X, it permanently refuses v1 from X (TLS fallback-SCSV logic; converges one gossip round after each peer's upgrade). Add `HIVE_GOSSIP_VERIFY=enforce-pq` as a 4th mode (parser at `lib.rs:227-233` extends cleanly): reject v1 from ratcheted peers, then from everyone. This closes the residual migration-window downgrade (a CRQC-armed attacker forging *fresh* v1 to not-yet-ratcheted receivers).

**Phase 1 budget check** (fleet N≈8-10; 7 signed endpoints × (N−1)/5 s ≈ 12.6 msg/s; cap-worst N=64 → 89.6 msg/s):
- Bytes: 2,529 B trailer = **0.015 %** of the 16 MiB `GOSSIP_MAX_FRAME`; ≈32 KB/s baseline (≈227 KB/s worst) — noise vs the 5 s roster JSON.
- CPU: ML-DSA-44 sign ~110 µs median / verify ~40 µs → **≈0.26 % of one core** (≈1.9 % worst; portable non-AVX2 impls 3-5× slower, still <1 % at fleet size).
- Latency: +~150 µs per request vs a 5 s cadence — invisible.
- Rejected alternatives: SLH-DSA-128s signing saturates ~2 cores at this cadence (128f = 17 KiB sigs); Falcon-512 has the best wire numbers but draft-status (FIPS 206) + constant-time floating-point Gaussian-sampling risk across a heterogeneous two-glibc fleet; ML-DSA-65 buys margin the control plane doesn't need — the suite byte makes any of these a later config change.
- Deliberate non-goal: **responses stay unsigned** (as in v1, `lib.rs:1821-1825`) — response authenticity rests on transport identity, which stays classical until Phase 2; signing responses would double cost for a leg that falls with the transport anyway.

### Phase 2 — Transport identity migration. Effort: tracking ≈0; fork ≈2-4 engineer-months + permanent tax. **Recommendation: track, don't fork.**

| Option | Work | Effort | Verdict |
|---|---|---|---|
| 2A **Track upstream** (recommended) | Monitor n0's non-ed25519 EndpointId work (blocked on industry consensus: draft-ietf-tls-mldsa, composite sigs); keep Phase 1 attestation as the containment layer; plan the fleet-wide breaking migration when upstream ships (new trust-set format, bootstrap-peer format, ratchet interplay) | ~0 ongoing; migration planning ~2-3 weeks when triggered | Gossip authenticity survives CRQC via Phase 1; only tunnels/raw/data-plane authz remains transport-bound |
| 2B **Local fork** | iroh-base NodeId→fingerprint types + serde/z32; iroh tls resolver/verifier (new SignatureScheme, SPKI-fingerprint check); endpoint/tickets/`address_lookup`; iroh-relay handshake frames both ends + 3 standalone relay deployments; discovery replaced outright (pkarr unfixable) + Seer reimplemented; NodeId ripple through vendored guardian-db (gossip/docs/blobs); dual-stack or lockstep fleet migration | 2-4 engineer-months initial; est. 1-2 engineer-weeks per upstream release thereafter (rebase risk against an actively moving 1.x line) | Not recommended: high divergence cost racing work upstream has already scoped; likely throwaway |
| 2C **PQ discovery records** (independent, our-code) | Parallel non-pkarr Seer format via the public `address_lookup` extension point: fingerprint-keyed HTTPS JSON, ML-DSA-signed, carries full key (~3.8 KB), relay/HTTPS-only (no DHT) | ~1-2 weeks | Optional hardening; protects discovery-record integrity against CRQC but cannot change TLS identity — sequence after Phase 1 if at all |

**Contingency (any phase)**: if Phase 0 were blocked upstream, app-layer AEAD over secret-bearing gossip bodies (HKDF from `HIVE_JWT_SECRET` → AEAD on deploy-fanout env payloads, store_sync, TLS bundles) is fully symmetric and therefore HNDL-immune independent of TLS (~1 week). `git.rs:3028-3041` already demonstrates the one-key-one-purpose derivation pattern. Not needed if Phase 0 lands.

### Consolidated ranking (urgency × feasibility, from the strategy analysis)

| Rank | Item | Score | Phase |
|---|---|---|---|
| 1 | Hybrid KEM on iroh transport | 25 | 0 |
| 2 | KEM on secondary TLS legs | 16 | 0.5 |
| 3 | PQ enrollment + cross-attested binding | 16 | 1a |
| 4 | Dual-sign v2 (capability-gated) | 12 | 1b |
| 5 | JOIN v2 + secret entropy | 12 | 1c |
| 6 | Ratchet + enforce-pq | 6 | 1d |
| 7 | App-layer AEAD contingency | 6 | — |
| 8 | PQ transport identity | 2 | 2 |

---

## 7. Residual risks and pre-existing findings (read-only observations; not fixed by this RFC)

1. **Non-constant-time join-proof compare** (`main.rs:521`) — the one MAC comparison in the tree that isn't constant-time (contrast `git.rs:3069-3078`); acknowledged in-code; fold the fix into Phase 1c.
2. **Transitive trust closure + shared join secret** (`main.rs:1700-1708` + fleet-wide `HIVE_JWT_SECRET`) — one compromised node or leaked secret admits arbitrary new identities fleet-wide; per-node signing keys named as the fix in `gossip.rs:588-590`. Phase 1 enrollment does not fix this by itself (§9 Q7).
3. **Compile-time defaults are sign-off / verify-log** (`lib.rs:223-233`) — enforcement is runtime posture (live per the runbook), not a compile-time guarantee.
4. **Guardian identity file lacks 0600** (`vendor/.../core/mod.rs:548`).
5. **Embedded relay is open, plain-HTTP, no ACL** (`main.rs:302-305`) — abuse/bandwidth exposure, not confidentiality.
6. **Corrupt key file silently mints a new identity** (`lib.rs:1513`; guardian analog) — interacts with the Phase 1d ratchet (§9 Q9).
7. **Unsigned data-plane streams** authorize on connection-level trust alone (by design) — the leg only Phase 2 can harden.

---

## 8. Open questions

1. **CRQC horizon / enforcement trigger.** What planning horizon does the project adopt (NIST's 2030-2035 deprecation window? earlier?), and what concrete signal flips `enforce-pq` (Phase 1d) from armed to active?
2. **ML-DSA implementation source.** aws-lc-rs 1.17.0 gates ML-DSA behind `unstable` (mutually exclusive with `fips`) vs the standalone RustCrypto `ml-dsa` crate. Supply-chain posture, API-stability, and any future FIPS requirement (which would *exclude* aws-lc-rs-unstable) decide this. Is FIPS compliance ever a requirement for hive?
3. **Suite confirmation.** ML-DSA-44 (cat-2) chosen over -65 for the control plane; Falcon-512 revisit when FIPS 206 finalizes? The suite-id byte makes this cheap to change — but who owns the decision record?
4. **Unsigned responses.** Phase 1 deliberately leaves gossip *responses* transport-authenticated only. Accepted as a documented asymmetry until Phase 2, or does any response (e.g., TLS-bundle fetch, store_sync payloads) warrant message-level signing sooner?
5. **GuardianDB PQ posture.** Does the guardian identity (separate EndpointId + separate main keypair) get its own Phase 1 enrollment, or does guardian replication continue to chain trust entirely to mesh admission? Does the vendored fork's endpoint builder expose the provider hook cleanly for Phase 0.2?
6. **Discovery hardening timing.** Build the PQ Seer record format (2C) proactively, or accept that discovery-record forgery post-CRQC is contained by the trust set + Phase 1 attestation until upstream identity lands?
7. **Join-secret architecture.** Do we move to per-node join secrets/credentials (fixing residual-risk #2) as part of Phase 1c, or is that a separate workstream? The shared-secret design also underlies mesh tenant delegation (`gossip.rs:582-619`).
8. **`HIVE_JWT_SECRET` entropy today.** Is the current fleet secret ≥32 CSPRNG bytes? (If human-chosen, that — not any quantum property — is the current binding security level of join admission and artifact signing.)
9. **Ratchet vs re-key.** A node whose key file corrupts silently mints a new identity (finding #6). Under the Phase 1d ratchet, a re-keyed node is a *new* peer requiring re-enrollment — is the operational runbook for deliberate re-keying (and for pruning ratchet state of retired identities) acceptable?
10. **Trust-set format growth.** `HIVE_TRUSTED_NODE_IDS` gains a PQ-fingerprint column in Phase 1 — config-file format, admin-endpoint (`mesh_admit`) signature, and `/v1/overview` reporting all need versioning. Who consumes these downstream (dashboards, runbooks)?
11. **Upstream engagement.** Should hive engage n0 directly (issue/RFC participation) to influence or get early visibility into the EndpointId migration, given the fleet's dependence?
12. **zkauth post-quantum.** The LSAG ring signatures (Ristretto255) are discrete-log-based and fall at CRQC. Out of this RFC's mesh scope — but the roster replicates over gossip and gates preview protection. Separate workstream?
13. **Verification tooling.** Phase 0 acceptance needs automated proof (qlog assertion in CI or a `/v1/relay`-style negotiated-group counter) that PQ KEM is actually negotiated fleet-wide, not just enabled — do we build the counter?

---

## Appendix A — Crypto/trust environment variables

`HIVE_TRUSTED_NODE_IDS` (state.rs:542) · `HIVE_PEER_TRUST` (main.rs:487) · `HIVE_GOSSIP_SIGN` (lib.rs:224) · `HIVE_GOSSIP_VERIFY` (lib.rs:228; default log) · `HIVE_GOSSIP_TS_WINDOW_SECS` (lib.rs:238; 300 s) · `HIVE_JWT_SECRET` (join proof, delegation tokens, artifact-key derivation) · `HIVE_ARTIFACT_SECRET` (git.rs:3029) · `HIVE_BOOTSTRAP_PEERS` (main.rs:239) · `HIVE_DISCOVERY_DNS` / `HIVE_DISCOVERY_ADDR` / `HIVE_SEER_ADDR` / `HIVE_DISCOVERY_N0` (main.rs:249-255, 642-644) · `HIVE_RELAY_URLS` (lib.rs:1392 + vendored fork) · `HIVE_OWN_RELAY_PORT` (main.rs:296). Phase 1 adds: `HIVE_GOSSIP_SIGN_V2` (abort valve), `enforce-pq` as a `HIVE_GOSSIP_VERIFY` value.

## Appendix B — Phase → file touch map (for estimation sanity)

| Phase | Files touched |
|---|---|
| 0 | `crates/hive-p2p/src/lib.rs` (bind_full), `crates/hive-p2p/Cargo.toml`, `crates/hive-cloud/Cargo.toml`, `vendor/guardian-db/Cargo.toml` + endpoint builder, relay-binary build scripts |
| 1a | `hive-edge/src/region.rs` (NodeInfo), `hive-p2p/src/lib.rs` (seed persistence), `hive-cloud/src/main.rs` (join enrollment), roster/gossip serde |
| 1b | `hive-p2p/src/lib.rs` (mode 0x06, preimage v2, trailer, capability gating, VerifyStats), `hive-cloud/src/main.rs` (sync_one_peer send path), tests (`tests/pool.rs` pattern) |
| 1c | `hive-cloud/src/main.rs` (join proof both sides), `admin.rs` (constant-time compare), ops secret rotation |
| 1d | `hive-p2p/src/lib.rs` (verify mode, ratchet state), `persist.rs` (ratchet file), `state.rs`/`admin.rs` (trust-set PQ column) |
| 2 | none locally under 2A; see §6 fork inventory under 2B |

## Appendix C — Discrepancies between the three source reports, resolved against source

1. **Gossip frame cap**: 16 MiB (`GOSSIP_MAX_FRAME = 16*1024*1024`, `lib.rs:100`, re-verified) — not "10 MB" (that figure is the zip-deploy body cap). Size math in §6 uses 16 MiB.
2. **Trust-set ID encoding**: 64-hex ed25519 pubkeys (validated `id.len() != 64 || !ascii_hexdigit` at `admin.rs:3466`, re-verified) — not z-base-32. z32 appears only in pkarr record names / DNS labels.
3. **Trailer read anchor**: `read_exact` of the 104-byte trailer is at `lib.rs:1762` (re-verified; reports cited 1760-1763 and 1761).
4. **dnsserver.rs**: contains **no** cryptographic surface (no pkarr, no DNSSEC) — one report's task premise said otherwise; the inventory pass corrected it and the correction stands.
5. **Locked versions** re-verified in `Cargo.lock`: iroh 1.0.2, rustls 0.23.40, aws-lc-rs 1.17.0, ring 0.17.14, pkarr 3.10.0.

## Appendix D — External citations (from the dependency analysis)

- Iroh post-quantum key exchange (iroh blog, May 19 2026): hybrid X25519MLKEM768 since iroh 0.98 behind `tls-aws-lc-rs`; ed25519 EndpointIds acknowledged as the remaining gap; upstream waiting on industry consensus for identity migration.
- rustls manual (cryptography defaults) + release notes: PQ KX in-core since 0.23.22; `prefer-post-quantum` default since 0.23.27.
- aws-lc-rs releases + issue #773: ML-DSA behind `unstable`, mutually exclusive with `fips` (verified in `aws-lc-rs-1.17.0/src/lib.rs:283-288`).
- BEP-0044 (BitTorrent mutable items): 1000-byte value limit, ed25519 — the structural bound on pkarr.
- draft-ietf-tls-mldsa: TLS `SignatureScheme::ML_DSA_44/65/87` code points 0x0904-0x0906 (present in `rustls-0.23.40/src/enums.rs:518-520`).
- FIPS 204 (ML-DSA), FIPS 205 (SLH-DSA), FIPS 206 draft (FN-DSA/Falcon) — parameter/size/perf figures in §6.
