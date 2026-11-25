# Network swarm transport: what shipped, what is a HOLD

Status: 2026-11-24. Local commits only: no push, no roll, no new dependency,
no `Cargo.lock` change, no test file. A roll needs the operator's word.

## What the adversarial swarm established

Two workflows ran against the request. The research swarm
(`wf_754ef243-7f8`, six confirmed results) and a lean synthesis
(`wf_884c852f-bd4`, three refuters plus a judge) produced these votes:

1. **The premise "the network swarm is unbuilt" is FALSE.** `inference.rs`
   already elects a rendezvous coordinator, plans same-region members by RTT,
   builds `<public_ip>:50052` addresses and launches `llama-server --rpc`, and
   injects `HIVE_INFERENCE_URL`. Upstream jitllm's ROADMAP calling network
   swarm "Not started" describes UPSTREAM, not this tree.
2. **A drop-in TCP-to-QUIC tunnel shim is REFUTED** on six concrete failure
   modes: capability probe returns `None`; a "tensor transport" with no caller
   is a phantom; splicing a relay-reachable endpoint into UNAUTHENTICATED host
   `:50052` weakens auth; a new ALPN needs its own inbound budget and
   `ConnClass`; `copy_bidirectional`/`select!` splicing is forbidden; iroh's
   30 s QUIC idle timeout kills an idle RPC link during the 3600 s model load;
   `EADDRINUSE` against `raw_ports.rs`' reserved `:50052`.
3. **RDMA is NOT shippable on this host.** `Cargo.lock` has 728 packages and
   zero `rdma`/`ibverbs`/`infiniband`/`roce`/`mlx5`/`thunderbolt` tokens.
   `/usr/bin/ibv_devinfo` exists but prints "No IB devices found";
   `/dev/infiniband`, `/sys/class/infiniband` and a libibverbs dylib are absent.
   macOS `bridge0`/`llw0` are IP/Ethernet Thunderbolt Bridge interfaces, NOT
   verbs devices. No live GPU/RDMA fleet node, no reviewed base, no operator
   word for a roll.
4. My own seed had two janks, both caught by refuters: `bind_full` at `:3570`
   and `dial` at `:3827` are line numbers, not ports; and
   `scripts/hive-lockdown.sh:33-41` is a comment plus the `LOCKED_PORTS`
   declaration, while enforcement is at nft `:91-92` / iptables `:128-129`.
   `PRIVATE_PEERS` are admitted only in the strict table (`:162`, `:178`).
5. **A genuine bug surfaced**: `member_rtts` accepted a FRESH gossip
   `latency_ms` without ever connecting to `:50052`, so a member whose
   llama-rpc was stopped still counted its VRAM toward coverage, appeared in
   `--rpc`, and parked the endpoint for 3600 s while `HIVE_INFERENCE_URL` was
   already injected fleet-wide. A 2 s probe miss was also cached as
   `RTT_UNKNOWN` for 300 s, refusing a recovered pool for five minutes.
6. The first proposed slice was killed by the synthesis: filtering
   `RTT_UNKNOWN` could not catch the dead member (fresh gossip latency
   bypasses the probe), URL injection had no readiness proof, and a loopback
   probe could print PASS with zero completed QUIC dials.

## What shipped

### A real swarm channel over the production QUIC trunk

`crates/hive-p2p/src/swarm.rs` (new) is a PROBED, bounded primitive, with no
phantom: every public symbol has a live caller in `--swarm-probe`.

- Strict LENGTH-PREFIXED frames (`<u32 BE len><u8 tag><payload>`), a
  `FRAME_MAX_BYTES` bound, and a `SwarmError` covering truncation, oversize,
  unexpected tag, I/O and timeout. A bare join is not injective.
- Affine typestate `SwarmChannel<Handshaking|Open|Closed>`: every transition
  consumes `self`, so a state cannot be replayed. `InflightSlot` is released by
  a `Drop` guard on every exit, including a dropped future.
- `probe_capability()` reads real device nodes and returns `None` = NOT
  CAPABLE. macOS Thunderbolt Bridge is reported as IP-only and is NEVER
  admitted as an RDMA transport. No verbs FFI, no assumed capability.
- `bind_probe_endpoint()` uses the production `HIVE_ALPN` and the production
  hybrid PQ crypto provider with relay and address lookup disabled and a
  loopback `:0` bind. It does NOT read `HIVE_IROH_PORT`, so two endpoints in
  one process cannot collide.
- There is NO new ALPN and NO new `ConnClass`: adding either without an accept
  arm and its own budget is precisely the phantom the swarm refuted.

### The inference admission failure CLASS, fixed at the shared primitive

`member_rtts` and its stale 300 s cache are removed. `member_links()` runs one
`bounded_round::BoundedRound` per reconcile round and requires a FRESH
successful connect to `<public_ip>:50052` for EVERY candidate, independent of
gossip latency. Gossip latency only ORDERS admitted members; it never admits
one. `Completion`'s drop guard defaults to `MemberLink { reachable: false }`,
so a dropped or panicking probe task fails closed. `plan_members` is a pure
morphism over plain values and refuses unreachable members by name in an
honest placement error. `rpc_port_reachable(ip, port, budget)` is the ONE
shared reachability primitive; gossip latency is not an argument to it.

`HIVE_INFERENCE_URL` is injected only after a live 2xx `/health` proof from the
coordinator's serving socket, via `hive_outbound::internal_client()` (no
proxy, no redirect). When readiness fails, ONLY the exact platform-shaped URL
we injected is withdrawn; a different value is a tenant override and is left
alone. `inference_endpoint_base()` PARSES the gossiped IP into an `IpAddr`
before it can reach a URL, brackets IPv6, and refuses link-local,
unspecified, multicast and broadcast targets. No caller-controlled string is
interpolated into a URL.

No `health::demote` call was added: a stopped llama-rpc is an inference
service fault, not a mesh transport failure, and demoting it would withdraw a
healthy node from DNS and placement on one observer's evidence.

### An operator diagnostic that cannot pass vacuously

`hive-cloud --swarm-probe` (same family as `--dns-probe` / `--dht-probe` /
`--litebox-probe`) binds nothing on a production port, joins no mesh, and
touches no running node. Live output on this host:

```
RESULT frames_round_trip=PASS sizes=5 bound_refused=1 truncated_refused=1 ib_state_parser=PASS
RESULT rpc_connect=PASS live_accepted=1 closed_refused=1 gossip_latency_is_not_an_input=1
RESULT admission=PASS fresh_gossip_latency_with_dead_rpc_refused=1 idempotent=1
RESULT reachable_pool=PASS members=2
RESULT url_gate=PASS ready_200=1 loading_503=0 closed_port=0
RESULT url_parameterisation=PASS v6_bracketed=1 link_local_refused=1 metadata_refused=1
RESULT port_collision=PASS shared_held=1 unique_admitted=1
RESULT rdma_verbs=NOT_CAPABLE typed_refusal=swarm capability not proven: rdma_verbs
RESULT thunderbolt_bridge_ip_only=true (IP interface, never an RDMA transport)
RESULT bind=PASS relay=disabled dht=disabled address_lookup=none
RESULT round=PASS landed=3 bytes=133143 outstanding=1 spent_ms=152 deadline_ms=3000
RESULT straggler=PASS dispatched_round=1 drained_in_round=6 bytes=8192
RESULT kex=PASS hybrid_pq_delta=8 classical_fallback_delta=0 unknown_delta=0
RESULT server=PASS connections=4 served_bytes=141335
RESULT scope=dial_target=loopback_direct_only fleet_public_path=NOT_PROVEN speedup_claim=none
RESULT inflight_slots_released=PASS live=0
SWARM_PROBE=PASS
```

Every gate is a NON-DEFAULT result: three byte-exact framed echoes landed in
this round, the real negotiated hybrid KEX group moved `pq_kex_stats()` by 8,
the server echoed 141335 bytes, one straggler was PARKED (not cancelled) and
landed in a later round, and all in-flight slots were released. Zero completed
dials cannot print PASS. A hard timeout wraps EACH bind. The diagnostic
states its scope: dial targets are loopback direct addresses; it does NOT
prove the fleet's public-IP/`HIVE_IROH_PORT` dialable path, n0/DHT behaviour,
or any QUIC-versus-TCP latency. iroh's own net-report may emit unrelated UDP
even with relay/DHT/address-lookup disabled; that is not a dial path. No
speedup is claimed anywhere.

## HOLD: operator-gated residuals

| Residual | Required gate |
| --- | --- |
| RDMA over Thunderbolt (macOS) | A reviewed third-party base (e.g. Dolphin eXpressWare / SISCI), two Macs and a cable, a REAL `ibv_devinfo` witness, and an operator roll word. macOS Thunderbolt Bridge is IP-only and cannot be made RDMA by a flag. |
| RDMA in a data center | RoCEv2/InfiniBand NICs, `rdma-core`/libibverbs FFI with a `Cargo.lock` change, an ACTIVE port, and a measured on-hardware A/B before any ratio is quoted. |
| QUIC tensor plane | `SWARM_ALPN` with its own `ConnClass`/`InboundBudgets`, a 40 s accept deadline, an established trunk that is never evicted, a bounded splice that copies each direction itself, an idle-timeout plan covering a 3600 s model load, `TcpStream` shim ports reserved in `raw_ports.rs`, and `scripts/hive-lockdown.sh` closing public `:50052`. |
| Replicated endpoint status | An owner proxy (`build_get` + `fetch_from_host`) plus a `gossip::dispatch` arm if URL advertisement must follow a coordinator's in-process status rather than a direct readiness probe. |
| Capability advertisement | A probed `NodeInfo` field and ONE shared `schedule` predicate, gated on real hardware and a roll. Nothing advertises an unprobed capability today. |
| Fleet dialability | A two-node witness over the real `HIVE_PUBLIC_IP`:`HIVE_IROH_PORT` path. The loopback diagnostic deliberately does not claim this. |
| Live children | A reconcile tick that re-plans members for an ALREADY-RUNNING child; today the load watchdog still covers a member that dies mid-load. |

## Second verification round, and the defects it found

A second workflow (`wf_7aa55635-ed1`) attacked the shipped diff with three
independent refuters. All three found real defects; two of them were fixed in
the same turn and the third is now fixed too.

1. **Port-collision bypass of the URL gate (correctness, most serious).**
   `port_for` is `FNV(project) mod 900`, so `proj-9` and `proj-90` both map to
   port 50372. One healthy llama-server on that port returned 2xx for BOTH
   projects, so the dead project inherited another project's inference URL.
   Fixed with `ambiguous_inference_ports()`, a pure morphism computed once per
   leader tick from the desired set: any `(coordinator_ip, port)` shared by
   more than one project is HELD, with a warning and no URL injection. The
   live diagnostic now witnesses the real collision pair:
   `port_collision=PASS shared_held=1 unique_admitted=1`.
2. **Three phantom `SwarmError` variants.** `Timeout`, `UntrustedPeer` and
   `CapabilityMissing` were declared but never constructed, so "every error is
   handled" was false. They now have real constructors with real callers:
   `check_peer()` refuses an identity/ALPN mismatch on both the dial and accept
   paths, `map_timeout()` maps a bind deadline, and `require_rdma()` turns
   NOT CAPABLE into a typed refusal (`rdma_verbs=NOT_CAPABLE typed_refusal=...`).
   The diff audit counts one constructor for each.
3. **Wrong Linux IB sysfs parse.** `"4: ACTIVE\n"` is the real `state` format;
   `eq_ignore_ascii_case("ACTIVE")` would have advertised a real RDMA host as
   NOT CAPABLE. `port_is_active()` now accepts both shapes, witnessed with
   `ib_state_parser=PASS` over five inputs including `4: ACTIVE` and `1: DOWN`.
4. **IPv6 link-local admission.** `inference_endpoint_base()` refused IPv4
   link-local but not `fe80::/10`. Both V4 and V6 link-local, unspecified,
   multicast and broadcast are now refused; a gossiped IP is parsed into
   `IpAddr` and IPv6 is bracketed. Witnessed:
   `url_parameterisation=PASS v6_bracketed=1 link_local_refused=1
   metadata_refused=1`.

The verifier also confirmed, by live execution, what is NOT refuted: no new
dependency (`git diff --numstat` on `Cargo.lock`/`Cargo.toml` is empty), the
probe filters `ProbeOutcome::default()` out, `served_bytes=141335` equals
`133143 + 8192`, and `hybrid_pq_delta=8` comes from the real
`negotiated_key_exchange_group` read off four completed handshakes, not from
the ALPN. It explicitly noted that a fresh `BoundedRound` per tick discards
parked stragglers, which is fail-closed and not a defect.

## Gates run on the final tree

`cargo test --workspace --locked` TEST_EXIT=0 (61 suites);
`scripts/cli-acceptance.sh` 35 passed / 0 failed;
`python3 -I scripts/check-contract-bindings.py --check` RESULT PASS
bindings=34 layers=4 gates=35; `git diff --check` 0;
`./target/debug/hive-cloud --swarm-probe` SWARM_PROBE=PASS;
`sh tmp/r8-witness.sh` WITNESS_EXIT=0 (the jitllm refusal regression:
`refuse_exact=True`, `jit_stub_spawned=False`, `jit_inference_url_in_env=False`).
Added lines carry 0 non-ASCII, 0 unchecked panics, 0 placeholder markers and
0 attribution trailers.

NO_TEST_FILES. No secrets. No push and no roll.
