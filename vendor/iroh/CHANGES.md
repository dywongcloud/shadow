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
