# fluid-hive vendored patch: iroh 1.0.2

Base: `iroh` 1.0.2 verbatim from crates.io (checksum
`5fca9b4b462c343ff88fc0af4096c186f939b602a0bc08723536ef2c31c93971`).

## Patch: bound `pending_open_paths` (upstream #4390)

**File:** `src/socket/remote_map/remote_state.rs`, inside
`RemoteState::open_path_on_conn`.

**Problem.** `pending_open_paths: VecDeque<transports::FourTuple>` accumulates
one entry per failed `open_path` attempt (`PathError::RemoteCidsExhausted` /
`PathError::MaxPathIdReached`), with no dedup and no cap. Both failure modes
are properties of the connection, not of a moment in time, so a connection
that stays exhausted accumulates a duplicate entry on every subsequent
holepunch/addr-update trigger, forever.

**Measured, 2026-08-08, production:** jemalloc heap profile on a live fleet
node showed 71,680 MiB — 99.8% of live heap — in one stack:

```
alloc::raw_vec::RawVecInner::finish_grow
alloc::raw_vec::RawVec::grow_one
alloc::collections::vec_deque::VecDeque::grow
iroh::socket::remote_map::remote_state::State::open_path_on_conn
iroh::socket::remote_map::remote_state::RemoteStateActor::open_path_on_all_conns
```

**Fix.** Before `push_back`: skip if `open_addr` is already queued (the
existing entry is retried on the same 333ms tick — no legitimate case needs a
second copy), and refuse to grow past `MAX_PENDING_OPEN_PATHS = 256` (well
above any real multi-path scenario) — a dropped duplicate past the cap costs
nothing beyond waiting for the next trigger instead of this one.

**Why not fix it at the fleet layer instead.** A fleet-side mitigation
(filtering which addresses may reach `Endpoint::connect`) was attempted first
and reverted the same day after it partitioned the mesh — filtering dial
CANDIDATES is not equivalent to bounding a RETRY QUEUE. Only the latter cannot
change which peers are reachable, so it's the version kept.

**Upstream status:** still unbounded in 1.0.3. Two community fix PRs
(#4398, #4414) were closed unmerged by their own authors — a caution about
this exact fix's shape, not just maintainer bandwidth. Carry this patch
deliberately; re-diff against each iroh version bump before updating the pin.

## Patch: read before send in `ActiveRelayActor` (no upstream issue yet; present unchanged in 1.1.0)

**Files:** `src/socket/transports/relay/actor.rs` (`run_connected`,
`run_sending`), `src/socket/transports/relay.rs` (`RelayTransport::new`).

**Problem.** Both actor loops are `tokio::select! { biased; ... }` and polled
the outbound side (`relay_datagrams_send.recv_many`, respectively the
in-flight `sending_fut`) BEFORE `client_stream.next()`. A node with sustained
outbound relay traffic therefore never read its relay TCP stream while it had
something to send. Two things follow: the relay server's 2 s write timeout
(`SERVER_WRITE_TIMEOUT`, iroh-relay `defaults.rs`) resets the connection, and
when the stream is finally read the kernel backlog decodes in one burst into
`relay_datagram_recv_tx` — a `mpsc::channel(512)` shared by every relay actor
of the endpoint and drained `BATCH_SIZE` frames per QUIC-driver poll — so
`handle_relay_msg`'s `try_send` fails and logs `Dropping received relay
packet: no available capacity`, one dropped QUIC packet per line.

**Measured, 2026-09-01, production (control-plane leader fc-sanjose):**
458,427 such lines in 24 h (journald additionally suppressed 6.0 M lines),
227,635 in the worst hour, 6,932 in the worst second; bursts of 2,000–4,500
drops inside 100 ms with inter-drop gaps under 50 µs (a released backlog, not
a stream); 303 server-side `Connection reset by peer` on the leader against
47 and 35 on two followers; fr 227 drops, va 0. The leader is the one node
that SENDS on relay paths at volume (wholesale store_sync pulls, rosters and
gossip to its relay-only peers), which is why followers never show it.

**Fix.** In both selects the `client_stream.next()` arm now precedes the send
arm (`biased` is kept: stop token, priority inbox and timeouts stay first).
Handling a received message is a non-blocking `try_send`, so reads cannot
starve sends the way sends starved reads. `relay_datagram_recv` is sized
512 → 4096 to absorb a backlog of the measured shape (worst case ≈5 MB per
endpoint) instead of dropping it.

**Rollout rule.** This changes mesh-transport scheduling: canary on one or
two nodes with a `/v1/mesh` assertion before any fleet-wide roll (AGENTS.md,
"Bringing a node into the mesh").
