# Changelog

## 2026-09-02 — Tencent exposure ticket: two forgotten `http.server` hand-offs served a directory listing to the internet for a week; the fleet firewall's peer roster was 13 of 22

Tencent's scanner reported "Index directory traversal" on
`43.166.206.175:28126` and `:28127` (fc-virginia, ins-7d6a22bf) on
2026-08-27. Both were `python3 -m http.server 2812x --bind 0.0.0.0`
processes started on 2026-08-26 (11:12 and 12:43) by two-second
non-interactive root SSH sessions — an automated hand-off of freshly built
hive-cloud binaries between nodes (`/root/xfer4/v2/hc`, `/root/xfer5/lb/hc`,
plus a listener-less `/root/xfer3/s9/hc`; ELF builds, no key material) —
and left running. They sat inside TCP 20000-29999, which the platform
security group (`sg-gshd147x` in na-ashburn, `sg-2jhu3loa` in
na-siliconvalley) opens to `0.0.0.0/0` for published container ports, so the
listing was world-readable for a week and nothing on the node noticed.
Resolution: both processes killed and all three trees removed; closure
witnessed from a Tencent vantage (fc-frankfurt: connection refused) and a
non-Tencent one (fc-phoenix: timeout — Tencent's edge drops SYNs to closed
SG-open ports from outside sources, so an outside timeout never proves a
filter; probe from inside the provider). A 22-node sweep found no other
ad-hoc server.

What the same pass found and fixed alongside:

- **`scripts/hive-lockdown.sh` was enforcing a 13-host peer roster on every
  node** (21 via its iptables chain, fc-phoenix via nft). The nine hosts
  added since the last full prerequisites run (fr, phx, tokyo, seoul, va4,
  va5, sj3-5) were strangers to every other node's 8787/50052/50100-50999,
  which silently dropped their HTTP admin dispatch onto the iroh path.
  `gen-hive-lockdown.sh` regenerated the line to 22, the three lockdown
  tasks in `roles/prerequisites` now carry a `lockdown` tag so a roster
  change rolls alone (`ansible-playbook playbooks/site.yml --tags lockdown`,
  22/22 ok), and fc-virginia's stale second copy (a 5-peer nft table beside
  its 13-peer iptables chain, stricter than the live one) was deleted.
  Witness: fc-frankfurt and fc-phoenix now connect to va/sj/bkk:8787 where
  they timed out before.
- **An nft `socket cgroupv2` gate on the published range was tried and
  rejected by measurement**: on fc-virginia (6.12.33-pvm+, nft 1.1.1) it
  accepted 2 of 37 SYNs to hive-cloud's own `:20008` and dropped the rest —
  a listener SYN has no socket attached when the match runs. It ran in a
  throwaway table on a spare port and was removed; the record is in
  AGENTS.md so nobody re-tries it on a live port.
- **`crates/hive-cloud/src/listener_audit.rs` (new, every node, every
  `HIVE_LISTENER_AUDIT_SECS` = 300 s):** reads `/proc/net/tcp{,6}`, keeps
  wildcard LISTEN sockets inside the audited range
  (`HIVE_LISTENER_AUDIT_PORTS`, default 20000-29999), drops the ones this
  process owns (socket inode against `/proc/self/fd`, never a port list),
  resolves the rest to pid/cmdline/cgroup, WARNs every pass while they
  exist, and on the control-plane leader opens ONE deduplicated Major
  incident per (port, pid). Detection only — the platform never kills a
  process it did not start. `GET /v1/host/listeners` (operator, node-local
  like `/v1/dns/stats`) serves the last report; `supported: false` on a
  host without procfs means "not audited", never "clean".
- **`scripts/audit-public-listeners.sh` (new):** the fleet view — every
  wildcard TCP listener per node that is not a platform daemon, plus the
  lockdown branch and peer count each node actually enforces; exits 1 on
  any finding so it can gate a roll. Its first run flagged 34 `next-server`
  processes on fc-sanjose-cvm-2 and 10 on fc-sanjose bound to `*:3xxxx-4xxxx`:
  every one an orphaned host-spawned mock cell from before a hive-node
  restart (`KillMode=process` keeps them alive, nothing adopts or reaps
  them), cwd on a deleted checkout, zero connections, 5.4 GB RSS on cvm-2.
  Reaped by hand (34 on cvm-2, the older-than-hive-cloud ones on sj);
  the structural fix (a host-cell pid ledger reaped at boot, loopback bind
  for host spawns) is PRD `mock-cells-orphaned-on-restart` /
  `sec-host-spawned-functions-wildcard-bind`.
- **Security-group audit (read-only, `DescribeSecurityGroupPolicies` across
  eight regions):** bkk, hk, tokyo, seoul and sp each carry an ALL-ports
  `0.0.0.0/0` group beside the platform group, so the host lockdown is the
  only guard on those five; the platform group itself opens 8786/8787,
  5432/6379, 9000-9001 and 53 world-wide. Recorded as PRD
  `sec-sg-all-ports-open-groups` for an operator decision — an SG mistake
  locks out SSH, so it is not automated here.

Rules that fall out (AGENTS.md "Host firewall & public listeners"): never
bind an ad-hoc server to `0.0.0.0` on a fleet host (`scp`/`rsync`, or
`127.0.0.1` behind an ssh tunnel); run the audit script after any
hand-provisioning session and before a roll; the lockdown roster is
generated, never typed.

## 2026-09-02 — The relational mirror was dead fleet-wide: the index walk ran on every request path and never once completed

On every node — the leader, nine followers, nodes 71 s after boot — every
admin SQL query took exactly 10.00 s and answered `relation "X" does not
exist` for EVERY table, `pg_catalog.pg_class` counted 0 and
`information_schema.tables` was empty, while `GET /v1/admin/sql/tables` kept
listing all 16 (`known_tables()` is a hardcoded list). The mirror loop failed
the same way (`sync_teams upsert failed … relation "teams" does not exist`,
`ALTER TABLE project_teams add deleted_ms failed`), and zero relational
writes had succeeded since boot on the leader or on fc-virginia. Not a
missing DDL statement, not a schema-qualification mismatch: guardian-db's SQL
catalog is one document in the document-store index, and that index was
EMPTY. `relational::session()` ran the full namespace walk
(`storage().refresh()` → `get_many` over the whole `hive` namespace, on
guardian stores of 7.6 GB on sj and 57 GB on va) under `SQL_OP_TIMEOUT` =
10 s before EVERY operation, so the walk was cancelled every time, never
completed even once per process, the catalog document was never known, and
every `Session` loaded `Catalog::new()`. Worse, `CREATE TABLE IF NOT EXISTS`
against that empty catalog "succeeds" and persists a fresh catalog over the
real one — the whole-document read-modify-write `ensure_table_exists`'s doc
already warns about.

Fix (`crates/hive-cloud/src/relational.rs`, `vendor/guardian-db`): the walk
runs in ONE background task (`spawn_index_refresher`) with a bound sized to
the store (`HIVE_RELATIONAL_INDEX_BUILD_SECS`, 900 s) and re-walks on a timer
(`HIVE_RELATIONAL_REFRESH_SECS`, 120 s) behind the document store's live
sync; completing once flips `INDEX_READY` and logs `relational: index built`
with the elapsed time and key count. `session()` waits up to 10 s for
readiness and never walks. `init_schema`, `ensure_table_exists` and
`backfill_billing_normalize` refuse DDL until the index is real;
`run_readonly_query` answers an explicit not-ready error instead of "does not
exist"; `team_for_project` requires a walk completed within two intervals.
Vendored: `refresh_doc_index` builds the replacement key set off the lock and
swaps it in atomically (`DocumentStoreIndex::replace_hash_only`, keeping
fetched values whose hash is unchanged), so a completed refresh is never
observable half-built and a cancelled one leaves the previous index in
service; `GuardianRelationalStorage::index_len` exposes the key count. Verify
on a rolled node: `pg_class` ≥ 16 and a sub-second `SELECT count(*) FROM
project_teams`; an unrolled node stays at 0 / 10.00 s. If the walk cannot
complete inside its bound on a node, the journal now says so once per
attempt (`index walk exceeded its bound`) and nothing writes the catalog.

The first canary of that fix found the second half. The index built in
69 ms on fc-frankfurt and 40 ms on fc-virginia (3,783 keys — the walk was
never slow; the per-session refreshes had been cancelling each other), the
refresh timeouts went from 56,000 per boot to zero, and the mirror's schema
reconcile still logged `relation "project_teams" does not exist` ten times
in the same millisecond. `GuardianRelationalStorage::{get, scan}` read
through `index().get_bytes`, a cache-only lookup that answers `None` for
every key a cold-start walk registered by hash and nothing has read yet; the
catalog is exactly such a key on every boot, so the engine loaded an empty
catalog on a fully built index. Both now fall through to the store's async
`get`, which fetches the value lazily and caches it. And `init_schema`, the
only path that creates the catalog and tables, was a single boot-time
attempt that returned silently when GuardianDB was not open yet and never
ran again; it retries every 15 s within the index-build bound and logs
`relational: schema bring-up complete` with the key count. Note for
anyone probing a follower: `POST /v1/admin/sql/query` is a mutation-shaped
request and the admin ingress forwards it to the control-plane leader, so a
follower's own relational state is witnessed from its journal (the
bring-up line, zero `does not exist`), not from its admin SQL answer.

The third canary found the last piece. Bring-up now ran on both nodes
(index built in 14–23 ms, zero refresh timeouts, zero `does not exist`) and
two tables per node — `teams`, `billing_ledger` — still reported unverified:
`ensure_table_exists` verified with `SELECT 1 FROM t LIMIT 1`, which scans
every row, and on a cold index each remotely written row is a peer fetch
of hundreds of milliseconds, so the scan blew the 10 s statement budget
while the CREATE itself had succeeded. Verification now reads
`information_schema.tables`, synthesized from the catalog document with no
row access; after the walk flips readiness the refresher warms every row
value in one pass through the store's lazy `query`
(`GuardianRelationalStorage::warm_values`, logged with count and
duration), and nothing waits on it; the bring-up summary counts the tables
it gave up on and logs at ERROR when that count is non-zero instead of
claiming every table verified. Measured on the fourth canary
(fc-virginia): index built in 14 ms, bring-up complete with every table
verified in the catalog and zero errors within a second of start, and the
warm pass materialized 3,759 documents in 416.9 s — about 111 ms per row,
the peer-fetch cost the old per-row verification was paying inside a 10 s
budget — so a cold node's mirror is fully warm seven minutes after boot and
its catalog and schema are correct from the first second. The control-plane
leader after the fleet roll: index built in 4 ms, bring-up complete with
every table verified, `pg_class` = 28 and real row counts in milliseconds
where it answered 0 and "does not exist" after 10 s an hour earlier; its
warm pass took 1,607 s for 1,935 documents (about 830 ms per row on the
busiest node) and once it landed the statement timeouts stopped entirely.
Expect the `ALTER TABLE project_teams` reconcile to retry with a 10 s
timeout on every mirror tick until the warm pass ends; it converges on its
own and is not the pre-fix "namespace wedged" class its message still names.

## 2026-09-02 — hive-node never exited inside systemd's stop timeout: a post-barrier persist() parked the tokio driver

Measured over 75 fleet stops: `hive-node` took 78.4 s (+78.3..78.6 s) to exit
on its happy path, was SIGKILLed on 34 of the 56 stops on 90 s nodes, and on
the TencentOS hosts (bkk, hk, cvmsj1/2, gpusj1-3) was SIGKILLed at +5 s, before
the listener drain ended and before the platform-state flush ran. The graceful
sequence in `main.rs` (SIGTERM → 15 s listener drain → runtime-artifact drain
→ `persist::flush_blocking` → `mark_clean_exit` → `guardian::shutdown` →
`ep.close()` → exit) spent 15 s in an unconditional drain sleep with zero
connections open and 60 s in `guardian::shutdown`'s first wait, which never
succeeded: the GuardianDB writer commits one generation per 40–75 s. The
SIGKILLs had one cause. After `flush_blocking` closed persistence admission,
any later `persist()` (185 call sites; usually `main.rs`'s follower-sync
adopt right after "store follower-sync: adopted the leader's snapshot", the
metrics loop, or a mirrored gossip write) reached `admit_generation`'s
condvar wait — a std wait on a tokio worker thread, by design "until process
exit". When that worker was the one holding tokio's IO/timer driver no other
worker re-took it, every timer and socket on the runtime stopped (thread
snapshots: 0 in epoll, all 131 in futex, 0 CPU, one worker on a foreign
futex, journal silent for 74 s), the 60 s guardian timeout and the 90 s
in-runtime hard deadline stopped with them, and systemd SIGKILLed at 90 s.
Two smaller defects rode along: `restart_audit::spawn`'s 20 s heartbeat
rewrote the marker with `clean_exit=false` after `mark_clean_exit` had
stamped it at +15 s, so every self-exiting restart was reported UNCLEAN; and
the TencentOS hosts inherit `DefaultTimeoutStopSec=5s` from
`/etc/systemd/system.conf` because the unit template set no `TimeoutStopSec`.

Five fixes. `persist::admit_generation` returns `None` once admission is
closed and never waits — `persist()` is a no-op and `persist_durable()`
returns `false` (fail closed), with one WARN per process. The hard deadline
is a `std::thread` sleeping on the OS clock (`hive-shutdown-deadline`),
default `HIVE_SHUTDOWN_DEADLINE_SECS` 90 → 75 so it stays under systemd's
stop timeout. The listener drain polls `Handle::connection_count()` every
250 ms and ends as soon as it reads zero instead of sleeping the whole 15 s
grace. `HIVE_GUARDIAN_SHUTDOWN_TIMEOUT_MS` defaults to 10 s for each of the
three sequential waits. `mark_clean_exit` latches an `AtomicBool` the
heartbeat checks before every marker write. `hive-node.service.j2` sets
`TimeoutStopSec=90s` explicitly. Expected graceful path after the roll:
~30 s, exiting on its own before either deadline.

A sixth, found while canarying the five: on fc-virginia the old binary was
SIGKILLed 1.2 s after SIGTERM, not at 90 s, and the journal explained it —
`hive-node.service: A process of this unit has been killed by the OOM
killer`, twice a second apart. The unit's cgroup holds every child hive-cloud
spawns (tenant builds, conmon, litebox runners, firecracker) under the
node's memguard `MemoryMax` (31.5 GiB on va), and systemd's default
`OOMPolicy=stop` turns ONE OOM-killed child into a stop of the whole node —
the SIGTERM hive-cloud logged right after the OOM line proves today's victim
was a child, not hive-cloud (12 such OOM-stop events in 24 h on va). The
cgroup's 35 lifetime kernel OOM kills are a different case: every one was
hive-cloud itself at 26.8 GB anon-rss on 2026-08-28, the KV balloon fixed
since. `hive-node.service.j2` now sets `OOMPolicy=continue` — a child's own
failure path reports it and the control plane stays up; hive-cloud itself
being the victim still exits and restarts under `Restart=always` exactly as
before.

## 2026-09-02 — Leader relay drops: iroh's relay actor read its stream only when it had nothing to send

The control-plane leader logged `iroh::socket::transports::relay::actor:
Dropping received relay packet: no available capacity` 458,427 times in 24 h
(journald suppressed 6.0 M more), 227,635 in the worst hour and 6,932 in the
worst second, while followers logged 227 (fr) and 0 (va). The line is NOT
the embedded relay server (which has zero clients on every node): it is
iroh's client-side `ActiveRelayActor` inside hive-cloud. Both of its loops
are `select! { biased; … }` and polled the outbound queue before
`client_stream.next()`, so the one node that SENDS on relay paths at volume
— the leader, serving wholesale store_sync pulls, rosters and gossip to its
relay-only peers — never read its relay stream while it had something to
send. The relay server's 2 s write timeout then reset it (303 `Connection
reset by peer` on the leader against 47 and 35 on followers), and the
released backlog decoded in one burst (2,000–4,500 frames inside 100 ms,
inter-drop gaps under 50 µs) into the per-endpoint 512-slot receive channel
the QUIC driver drains a batch at a time. Vendored patch
(`vendor/iroh/CHANGES.md`, "read before send"): the read arm precedes the send
arm in both selects (`biased` kept: stop, priority inbox and timeouts stay
first; a received message is a non-blocking `try_send`, so reads cannot
starve sends), and the channel is 4096 deep. Unchanged upstream in 1.1.0.
Rolled as a canary to fr and va with a `/v1/mesh` assertion before the fleet.
Open beside it (PRD `leader-udp-11204-rebind-addrinuse`): the leader's pinned
iroh UDP socket fails to rebind with `AddrInUse` a few times an hour and stays
closed for minutes; a 12-minute holder watch ruled out a persistent foreign
holder, the in-process candidate is still unidentified.

## 2026-09-01 — Sandbox terminals no longer disconnect instantly: the pty runner's guest tar outlived by its waiter

Every dashboard sandbox terminal closed the moment it opened. Reproduced
against the leader's own listener and through the public round-robin host:
the websocket upgrade succeeded (101) and the server immediately sent
`Error: No such file or directory (os error 2)` followed by
`{"type":"exited","exit_code":1}`. Standalone, the same runner with the same
tar runs an interactive shell on a pty fine, so the defect was in how
`LiteboxBackend::exec_pty` spawned it: it built the shell tar and its
descriptor-bound alias (`/proc/self/fd/N/initial-files.tar`), spawned the
runner, and returned — dropping both guards at the end of the function.
Their Drop impls unlink the alias name and its scratch directory, and the
runner's own open of `--initial-files` a few milliseconds later found nothing
(the inherited directory fd still existed; the entry did not). `exec_command`
moves both guards into its waiter task and drops them after the runner exits,
which is why one-shot commands worked and shells never did.
`crates/hive-backend/src/litebox.rs`: the pty waiter now owns both guards
exactly like the exec waiter.

With that fixed the shell died a second way, with no output and status 21.
Litebox has no job-control tty ioctls (`tcsetpgrp` answers ENOSYS, -38): a
shell whose stderr IS the pty goes interactive by itself, enters job-control
setup, and the runner exits `exit_group(277)` before a prompt byte —
measured with the staged tar for `/bin/sh`, `+m`, `--norc +m` and `-i +m`
alike, while `/bin/sh -i` with stderr off the pty prints its prompt, runs
input and exits cleanly, warning once "cannot set terminal process group
(-38)" / "no job control in this shell". `exec_pty` now keeps stdin/stdout on
the pty, gives the runner a stderr pipe pumped into the same terminal stream
(those two one-line warnings filtered), and passes `-i`. Job control inside
a sandbox shell stays a litebox gap (PRD `litebox-guest-exec-pty-support`).
The pump forwards raw chunks with LF normalized to CRLF, never whole lines:
an interactive shell writes its prompt to stderr with no trailing newline,
and a line reader held it forever (the roll-5 cut: 101 upgrade, runner
alive, zero frames). Measured on that cut before the chunked pump rolled,
with a websocket client that typed without waiting for a prompt: `echo`,
`id` (`uid=1000`) and `exit` (`{"exit_code":0,"type":"exited"}`) all
round-tripped within 100 ms, and the four held prompts arrived as ONE frame
(`sh-5.2$ sh-5.2$ sh-5.2$ sh-5.2$ exit`) only when `exit` finally supplied
the newline — the terminal was interactive but blank, which is the whole
defect the chunked pump removes.

With the chunked pump live (witnessed green on the leader's listener AND
through `https://api.shadw.cloud`: 101, prompt, `TERM_OK_42`,
`{"exit_code":0,"type":"exited"}`) two smaller defects showed in the frames
and are fixed in the same file:

- The first frame was a bare `\r\n`: the pump swallowed the CRLF BEFORE a
  dropped job-control warning, so the newline after the LAST warning leaked
  and every terminal opened with a blank line above its prompt. A dropped
  line now owns the newline that follows it.
- The next prompt overtook the previous command's output (`sh-5.2$ echo
  …\r\nsh-5.2$ TERM_OK_42`): stderr (pipe) and stdout (pty) were two
  channels read by two tasks. The shell tar now stages `/usr/share/hive/shellrc`
  (`exec 2>&1`) and the runner is spawned with `ENV` naming it — the shell
  still STARTS with stderr off the pty (the arrangement that keeps the
  runner alive through job-control init) and then moves it onto the pty
  itself, so prompt, output and command stderr are one ordered stream with
  the pty's own CRLF translation. Linux-gated code, so `cargo check` ran on
  fc-virginia, not only locally.
- Rolled, that cut fixed both (first frame `sh-5.2$ `, then `TERM_OK_42`,
  then the next prompt, on one channel) and failed at `exit` with runner
  exit 101: `thread 'main' panicked at litebox/src/fs/layered.rs:683:67:
  called Result::unwrap() on an Err value: NoWritePerms`. The rc file's tar
  entry had implicitly created `/root` in the guest, not writable by the
  guest uid, and the interactive shell's history write at exit hit a
  permission check litebox unwraps instead of answering EACCES — where a
  MISSING `/root` (every earlier cut) failed that write silently. The rc now
  lives at `/usr/share/hive/shellrc`, nothing under `$HOME`, and `HISTFILE`
  is cleared so the shell never writes history. The panic itself is a
  litebox defect (PRD `litebox-layered-fs-nowriteperms-panic`).
- Final witness on the control-plane leader after the fleet roll
  (2026-09-02, exe `6c64268dd375`), on its own listener and through
  `https://api.shadw.cloud`: 101, first frame `sh-5.2$ `, `TERM_OK_42`
  echoed, `exit` → `{"exit_code":0,"type":"exited"}`, VERDICT PASS both
  ways; the probe's frames read prompt, output, prompt in order, and `id`
  answers `uid=1000`. What stays open is litebox's own job control (no
  `tcsetpgrp`), which the shell reports once at startup and the pump drops.

## 2026-09-01 — Deploy dispatch falls back past an unreachable node; peers keep their real home relay; a missing main entry fails the BUILD, not the node

Two production failures reported together, root-caused as two different
projects (`crates/hive-cloud/src/git.rs`, `schedule.rs`, `gossip.rs`;
AGENTS.md "Deploys"):

- **`express` (regions frankfurt, tokyo): `Placement → fc-frankfurt`,
  `deploy dispatch failed — iroh: no reply`, `Could not reach any target`.**
  Eight consecutive builds over 75 minutes failed the same way, each in
  5–15 s (the dial failed; the 20 s budget never elapsed), while ten capable
  nodes sat idle: the leader had marked fc-tokyo unhealthy, so `place()`
  returned ONE candidate and the coordinator gave up after it. Only three of
  the eight fell inside any restart window. fr builds fine (3/3 ready in
  48 h) — "builds failing on frankfurt" were dispatches that never arrived.
  The leader↔fr iroh pair is chronically sick for a proven reason:
  `gossip::relay_hinted_addr` replaced fr's real home relay
  (`https://fc-virginia.relay.shadw.app:3343/`, carried in its `iroh_addr`)
  with its embedded `http://162.62.83.144:3341`, and every Tencent `:3341` is
  a TCP timeout from every vantage (security group) — so a cached-hint dial
  had exactly one path, direct UDP, and when that flapped it timed out at 5 s
  and fell into discovery (1,014 such timeouts in 24 h).
  - `schedule::dispatch_fallbacks`: an ordered list of capable + reachable
    remote candidates (configured regions first in request order, then
    distance/load/free disk/name), excluding tried targets and self.
  - `git::run_build`'s pure-remote branch walks that list when nothing ran
    (`HIVE_DEPLOY_DISPATCH_FALLBACK_MAX`, default 3; each attempt bounded by
    the existing 15 s HTTP + 2×20 s iroh budgets), logs
    `→ fallback i/n: <node> (<region>) — <unreachable> could not be reached…`,
    stops when any node ran, keeps a container-lease holder sacrosanct, and
    ends with `⚠ Deployed on X because Y could not be reached`. The iroh
    failure text now carries measured elapsed ms and distinguishes
    dial-failed from budget-elapsed. `Could not reach any target` is only
    ever printed after the whole list is exhausted.
  - `relay_hinted_addr` keeps the peer's own home relay when its address
    carries one and steers only relay-less addrs. This is a dial-side mesh
    change and is rolled canary-first (`--limit fr,va`) with a `/v1/mesh`
    assertion before fan-out, per the retain_dialable lesson.
  - Characterized, not fixed (PRD `leader-relay-actor-saturation-transport-wedge`):
    the leader logged 459,062 `Dropping received relay packet: no available
    capacity` in 24 h and the transport-wedge signature 60×, through two
    restarts.
- **`shoomoo2` (github fatbearsk/serverless-clawdbot@xstate) on
  fc-sanjose-3: `main entry "/workspace/server.js" is missing from the
  immutable application archive`.** The repository has no `server.js` (a
  Next.js 16 app; `scripts.build = "next build"`), and a `fluid.json` lane
  runs ZERO install/build commands (`produce_manifest` returns
  `Manifest::from_json` untouched), so `start_cmd: ["node","server.js"]`
  can never have worked. The platform defect was how it surfaced: at the
  post-registration readiness launch, as `NodeBackendUnavailable` — a NODE
  fault charged to the pool for a tenant file that does not exist.
  - `git.rs` `preflight_direct_entries`: before sealing, every Node/Bun
    function whose argv is a plain `node <entry>` / `bun <entry>` must have
    that entry in the checkout; the refusal names the function, the entry,
    the exact path examined, the sealed root's listing, package.json
    `scripts.start`/`scripts.build`/`main`, the deciding field
    (`fluid.json functions[N].start_cmd` vs build-derived), the lane rule and
    the three fixes. Verified not to false-positive on the platform's own
    launch shapes (FDI Next dist-bin, SvelteKit `node build`, exported-app
    launcher, Build Output v3 launcher, `bun run --bun start`, `npm start`).
  - `litebox.rs` `validate_archive_main_entry`: a `Not found in archive` tar
    miss is `DEPLOYMENT_START_FAILED` with a message naming the entry and the
    fields, never `NODE_BACKEND_UNAVAILABLE`; other tar failures keep the
    launch refusal.
  - The tenant fix is on the user's side: remove `fluid.json` (framework
    detection then runs install + `next build` and derives the start command
    from `scripts.start`) or commit a self-contained server. Honoring
    install/build commands inside the fluid.json lane is PRD
    `fluid-json-lane-honor-build-commands`.

Live witnesses after the roll (canary `--limit fr,va`, then all 22 hosts, both
glibc lanes; fr's first 45 s on the new binary showed zero cached-hint dial
timeouts against ten in a comparable span before):

- `POST /v1/projects/express/redeploy` on the new leader → `dpl-cf08186c74`:
  `Placement: region-aware scheduler → fc-frankfurt, fc-tokyo`,
  `→ fc-frankfurt: dispatching deploy (via iroh)`, `[fc-frankfurt] Running
  build`, `Sealed runtime artifact 1023774e301e`, `✓ fc-frankfurt: deployment
  ready` (tokyo likewise), state `ready`, aliased to express.shadw.app. The
  leader opened a trunk to fr within seconds of booting and logged 2
  cached-hint timeouts in 3 minutes where it had logged 45 per 5 minutes.
  The fallback walk was not needed on that run and is therefore not yet
  exercised live.
- The same repo and branch deployed fresh under a throwaway project
  (`gm-preflight-witness`, deleted afterwards) → `dpl-0f4ab200cf` state
  `error` with `Launch preflight failed for function "web" (functions[0]):
  main entry "server.js" — chosen by fluid.json functions[0].start_cmd =
  ["node", "server.js"] — does not exist in the deployment tree about to be
  sealed … package.json declares scripts.start = "next start", scripts.build
  = "next build". A fluid.json deployment ships the repository checkout
  AS-IS …` and no `Sealed runtime artifact`, `NodeBackendUnavailable` or
  `tar:` line.

## 2026-09-01 — Sandbox execs get a supervisor (deadline, kill, terminate sweep); litebox fork-child corruption root-caused

The first live proof after the fleet roll below created a real Litebox sandbox
on the leader (`owner_node: fc-sanjose`, `provider: platform`, no "simulated"
note — the thing the whole day was about) and then hung on its first command:
`POST …/commands` timed out at 60 s with zero bytes, `DELETE` answered 200,
and the `litebox-runner` for that sandbox was still alive seven minutes later
at 99.9% CPU until killed by hand.

Root cause, established standalone with the exact staged tar rather than
inferred (`crates/hive-backend/src/litebox.rs`, `SHELL_GUEST_OPTIONAL_PROGRAMS`
doc; AGENTS.md "Litebox"): the litebox `fork()` emulation gives a child whose
glibc state still points into the parent's mapping. Fork + immediate `exec`
works; a pipeline/subshell child aborts (`malloc(): unaligned tcache chunk
detected`), a child that touches stdio aborts (`glibc detected an invalid
stdio handle`), and two consecutive not-found commands make the second child
spin forever. The repro was handed to the litebox fork's owning session
(their Task #51); nothing here changes fork semantics.

What changed on this side:

- `sandboxes_platform::run_command` no longer drains the exec inline. A new
  `supervise_exec` task drains BOTH blocking and detached runs, so a dropped
  request future (client timeout, closed tab, the leader→owner forward's
  budget) can no longer orphan a running guest with its record stuck at
  `running`. It enforces a deadline — `HIVE_SANDBOX_RUN_MS` − 10 s for
  blocking runs (one knob with the forward budget, owner finalizes first), the
  sandbox's `timeout_ms` capped by `HIVE_SANDBOX_EXEC_MAX_MS` (1 h) for
  detached — and on expiry kills the guest process group via `kill_exec`,
  gives the waiter 3 s to report `ExecDone`, then closes the record `killed`
  with `[hive] command exceeded its N ms deadline …` on its stderr.
- `LiteboxBackend::terminate` kills every live exec and interactive-shell
  process group of the cell before tearing down its TUN; `exec_pty` runners
  now get their own process group and are tracked in `execs` under the
  session id, so a shell dies with its sandbox too.
- `SHELL_GUEST_OPTIONAL_PROGRAMS`: `uname`, `id`, `sed`, `awk`, `sort`,
  `xargs`, `tar`, … staged when the host has them (absent = skipped, never an
  exec failure). Under the fork defect a not-found command is the WORST case,
  so this is hang-avoidance as much as convenience.
- The `LiteboxExec` doc no longer claims guest forks are host forks: on the
  fleet's runner build the "child" is a second thread of the runner (gdb,
  `pgrep -P` empty); the group kill stays correct either way.

Fleet state this entry ships into: the roll below completed
`BACKEND VERIFIED: 22/22`; the leader fc-sanjose restarted as
`isolation backend: Litebox` with the 27-id trust list loaded and its 218
tenant containers surviving the restart (`KillMode=process`).

## 2026-09-01 — Sandboxes fail closed and route to a real owner; Linux mock nodes become Litebox

Commit `59357cdac9` carries ALL of this under a subject that names only its
Tencent hunk — the `git_commit` verb ignored its `paths` argument and staged
the whole tree (PRD row `gm-git-commit-ignores-paths`). This entry is the
honest description of that commit. It is the replay of local `138815cd20`
onto `dff7bf99c3` (cross-node terminal forwarding over the mesh,
`mesh_shell.rs`); the merge keeps that forwarding and points it at
`SandboxRecord::owner_node` — a non-owner node now tunnels the interactive
shell to the real owner over the existing `RawTarget` mesh surface and sends
`wrong_node` only when no mesh path to the owner exists.

- **Sandbox creation never returns a simulated success again.**
  `PlatformSandboxProvider::create_sandbox` fails CLOSED: a provisioning
  error is a typed `EngineUnavailable` naming the node and its backend, no
  record is persisted, and a Drop guard releases the half-provisioned cell.
  The previous path stored `status=failed` + note and returned `Ok` — the
  "this node has no real isolation backend … sandboxes are simulated here"
  records (e.g. `sbx_52b6a2da81324dc0`) came from the control-plane leader
  itself, which runs `MockBackend`.
- **The leader is a router, not a fallback provisioner.** `POST
  …/sandboxes` mints the id, then delegates to healthy exec-capable peers
  (`backend ∈ {firecracker, litebox}`, same region first, name order) with
  bounded per-peer and total budgets over the existing owner-hop transport
  (HTTP admin, else the gossip `POST` arm); exhaustion is a typed 503
  `SANDBOX_NO_CAPABLE_NODE` listing every candidate tried. Records carry
  `owner_node`; stop/delete/run/kill ride one typed owner RPC
  (`/v1/internal/sandboxes/owner-op`) that re-derives authority on the owner
  and never re-proxies; reads proxy to the owner. Internal hops require the
  fleet token or a tenant-bound `mesh-internal` JWT; `/v1/internal/sandboxes/`
  is exempt from both leader gates.
- **`LiteboxBackend` can run sandbox commands.** `exec_command`/`kill_exec`
  implemented on the same guest mechanism as `exec_pty` (fresh runner on the
  cell's TUN, guest tar with the program's `ldd` closure, separate
  stdout/stderr `ExecOutput` streams, exactly one `ExecDone`, `killpg` by exec
  id).
- **The pinned AnEntrypoint Litebox runner now works on RHEL-family hosts.**
  `roles/litebox/files/networking.patch` gained a third hunk: uid 0 gets
  `CAP_DAC_OVERRIDE` semantics in the guest in-memory FS. Without it the
  runner panicked at `lib.rs:293 NoWritePerms` because `/usr/bin` is 0555.
  With it `hive-cloud --litebox-probe` PASSES on fc-tokyo, fc-seoul,
  fc-virginia-4/5 (runner `f970bfe70ac86d4e`, byte-identical),
  fc-sanjose-cvm-1/2 (`cb20b4e9f3cf03f5`) and fc-sanjose; the first six run
  `backend=litebox` in production, the leader flips on its next restart.
  Details in `PATCHES.md`; `hosts.ini.example` documents the `[litebox]` group
  and the per-host `litebox_verified` mark; the AGENTS.md Litebox section is
  drained to pointers.
- **Tencent dedicated IPv4.** `AllocateAddresses` tags use `{Key, Value}`;
  `TagKey`/`TagValue` is the filter type and was rejected with
  `UnknownParameter: Tags.0.TagKey` before any address was purchased.

## (pending) — Browser functions run against a real Node API

A browser function may now be written the way a Node function is written. The
build-time scan that rejected `require(`, `import(`, `process.`, `Buffer`,
`__dirname`, `__filename`, `Deno.`, `Bun.` and (in quickjs mode) `fetch(` is
GONE — globally, for every function — and the substrate supplies the surface
those tokens name instead of refusing the source that used them.

- **`crates/hive-browser/www/node-runtime.js` (new).** Installed in the guest
  immediately before the artifact function: a CommonJS `require` over the Node
  builtin set (`path`, `events`, `util`, `stream`, `string_decoder`,
  `querystring`, `url`, `os`, `assert`, `crypto`, `fs`, `http`/`https`,
  `buffer`, `process`, `timers`, `perf_hooks`, `vm`, `module`, `async_hooks`,
  `zlib`, `v8`, …) plus `express`, a real `Buffer` (a `Uint8Array` subclass),
  `process`, `console`, timers, `URL`/`URLSearchParams`,
  `TextEncoder`/`TextDecoder`, `atob`/`btoa`, `performance`,
  `structuredClone`, `AbortController`, `__dirname`/`__filename`. The QuickJS
  guest had NONE of these — measured against the shipped bundle, its global
  object is bare ECMAScript, so the old rejection message ("use
  Uint8Array/TextEncoder, both exist in QuickJS") named an API that was not
  there.
- **Not almostnode.** The obvious candidate is a 16 MB browser-hosted Node
  emulator that owns module resolution, an npm client and dev servers, executes
  with host-realm `eval`, assigns `globalThis.process`, and references
  `window`/`document`/`navigator`/`localStorage`/`Worker`/service workers
  throughout its built bundle — none of which exist in a QuickJS guest, and its
  own README rates `net`/`tls`/`dns`/`dgram`/`cluster`/`vm`/`v8` "stubs only"
  and tells you to run untrusted code in a separately-deployed cross-origin
  iframe. What it genuinely provides for a sandboxed guest is implemented
  directly here, against this substrate's real primitives.
- **Both calling conventions, one reconciliation point.** The emitted wrapper
  passes the handler a `req` that is a SUPERSET of the platform request
  descriptor and a `res` that is a superset of `ops`, so
  `(request, ops) => ({status, body})` keeps working byte-identically while
  `(req, res) => res.json(...)`, an Express app, and
  `http.createServer(...)` + `server.listen(PORT)` all work as written.
  `bridge.settle(out)` decides: an explicit non-`res` return value wins,
  otherwise the response written on `res` is used (including the
  `return res.json(x)` shape, which returns `res`, not `undefined`).
- **Unsupported stays LOUD, at the honest boundary.** `net`, `tls`, `dns`,
  `dgram`, `cluster`, `child_process`, `worker_threads`, `zlib`, outbound
  `http.request`, `crypto.randomBytes`/`randomUUID` (there is no CSPRNG in the
  guest, and Math.random is not one), a relative `require('./x')` and any npm
  dependency each throw a NAMED error at the call — never a silent no-op. The
  refusal moved from build time to the exact line that cannot work, so a
  handler that never takes that branch is no longer blocked from deploying.
- **`process.env` is EMPTY** (`NODE_ENV`, `HIVE_BROWSER_NODE` only) and is never
  populated from the host: project env and secrets still never ship to a
  donor's browser. That was the real reason `process.` was banned; it is now
  enforced where it belongs.
- **Unchanged on purpose:** the canonical policy digest (both implementations,
  byte-identical, no new mode and no new host op), admission/capability
  derivation, tenant ownership checks, and pin()'s size + BLAKE3 + policy-digest
  verification of artifact bytes. The Node runtime wraps AROUND verified source;
  it never rewrites it and grants nothing `allowed_ops` did not already grant.
- **Still rejected at build:** static `import`/`export` STATEMENTS. The artifact
  is evaluated as one function expression, where they are a hard SyntaxError —
  permitting them would only defer the failure into every donor's browser, at
  boot, for every request. CommonJS is the supported form.
- Published: `ui/scripts/sync-browser-node.mjs` now copies `node-runtime.js`
  into `ui/public/browser-node/` — it is a STATIC import of
  worker-function-runtime.js, so omitting it would break the SharedWorker's
  whole module graph on the fleet while every local check stayed green.

## (pending) — Browser nodes are dispatched to, not hand-fed

A running browser node now receives whatever browser-eligible work its tenant
has, automatically, instead of serving the one artifact a human picked at
start. The picker survives as a deliberate override, not a gate.

- **The admission capability is a SET.** `browser_admission::validate_request`
  resolves the donor's whole eligible set server-side
  (`browser_artifacts::eligible_for_tenant`: every browser-eligible function of
  every Ready deployment under the AUTHENTICATED tenant, from the same two
  replicated sources `descriptor_for` already reads, deterministically ordered
  production-then-newest and capped by `HIVE_BROWSER_AUTO_SERVE_MAX`, default
  16). It is re-derived on every renewal, so a function deployed after the node
  started is served within one lease tick — no restart, no re-pick. The
  capability block gains `artifacts[]` and keeps mirroring its first entry into
  the flat fields a pre-upgrade worker reads.
- **`serve_mode` is a request, not a capability.** `"auto"` asks the server to
  derive the set; absent (a pre-upgrade worker) or anything else still means
  serve nothing, so a rollout never starts serving code on a donor that did not
  ask for it. Naming a `deployment` overrides auto and pins exactly that one —
  today's behaviour, unchanged.
- **One endpoint, many routes.** `fluid_gateway::set_browser_targets` replaces
  an endpoint's complete registration set atomically (one entry per function
  key, each validated exactly as before, hard-capped at
  `MAX_BROWSER_TARGETS_PER_ENDPOINT`); `upsert_browser_target` is now its
  one-element wrapper. `routing_identity_changed` became a superset test: a
  pure addition no longer tears down the browser's QUIC trunk and constellation
  presence every time anyone in the tenant deploys.
- **The scalar admission triple is now a compat view only** and is kept a
  coherent member of the set — never a deployment-A/function-of-B mixture,
  which a pre-`serves` follower would have registered as a route that could
  execute the wrong digest for a same-named function. In auto mode with no
  database pin it is empty, so an old follower routes nothing rather than
  something wrong.
- **Databases are not auto-picked among several.** A browser holds one replica,
  so `browser_db::auto_db_deployment_for_tenant` grants only when the tenant has
  exactly one project carrying a `browser_db` block; with two or more the picker
  chooses or the node runs without one.
- **Worker (HOST_ABI v4).** `normalizeCapability`/`reconcileCapability` handle
  the descriptor set: pin every authorized artifact (both BLAKE3 digests
  recomputed locally, cache-first), then revoke stale digests/callers before
  granting replacements. A per-artifact failure no longer discards the rest —
  it lands in `status.functions.failed` and retries next renewal; only a total
  failure takes the old backoff path. `status.functions.serving[]` names what is
  actually pinned, so /run-node reports the real set instead of a count.

## (pending) — Durable deployment roots + concurrent same-project zip deploys

Two deployment-lifecycle fixes in the git/zip build path, both live-witnessed
on a local `HIVE_FORCE_MOCK=1` node:

- **Deployment roots are durable (`$HIVE_DATA/deploys`, not `/tmp`).** The mock
  backend serves files straight from a deployment's recorded `root` for its
  whole life, but `git::deploy_root()` was `$TMPDIR/hive-deploys` — a host
  reboot wiped the checkout while the replicated deployment RECORD survived,
  so the node 404'd `DEPLOYMENT_NOT_FOUND` for a deployment it believed it had
  (root-caused live: dan.shadw.app, 2026-08-03). New checkouts and retained
  zip sources now live under `$HIVE_DATA/deploys`; boot restore is unchanged
  (records carry absolute paths), so pre-upgrade `/tmp` roots keep serving
  until their host reboots, and every root-scanning reader
  (`newest_deploy_dir`, `gc_build_dirs`, `purge_project_source_dirs`,
  retained-source lookup) falls back to the legacy `/tmp` root — a pre-upgrade
  zip project still redeploys from its `/tmp`-retained archive and
  re-materializes into the durable root (witnessed). The project-name path
  component is now `sanitize_tag`'d everywhere a checkout dir is built
  (tenant-controlled text is never a path component verbatim), with
  raw-name prefix fallback (`git::checkout_prefixes`) so pre-sanitization
  checkouts remain visible to redeploy/GC/purge.
- **Concurrent same-project builds no longer share one checkout dir.**
  `run_build` named the checkout `<project>-<now_ms()>`, and two concurrent
  builds (both woken by the same timer tick from the synchronized 350ms
  pre-build sleep) land on the SAME millisecond far more often than intuition
  says — one shared dir, two racing `unzip -o` processes, and the loser dies
  with exit 50 "cannot create …: No such file or directory" (reproduced both
  through the API and with bare `unzip`; the witness hit it 3x and had to
  serialize deploys). The checkout dir, the "Building…" placeholder dir, the
  extract temp zip, and the build-cache temp tar now all carry the build id;
  the retained `<tag>.src.zip` write is tmp+rename (two concurrent writers
  could previously tear it mid-read by a redeploy). Two concurrent zip
  deploys of one project now BOTH reach `ready` (witnessed 3/3 race-window
  hits, including one pair with byte-identical ms stamps); the alias resolves
  to the later finisher, matching Vercel's concurrent-deploy model. When the
  two requests instead serialize on the project-name check, the loser still
  gets the pre-existing loud 409 — retryable, never a silent strand.

## (pending) — Browser↔fleet live CRR exchange (browser_db databases replicate for real)

A project that opts in via a fluid.json top-level `browser_db` block now gets a
REAL replicated database, not just a contract: browsers holding a live,
server-derived grant sync divergent writes both ways against a per-project
fleet replica over one new `Op::CrrSync` op on the existing `hive/browser/0`
ALPN, riding the repaired hive-crsql seam (per-origin-site durable watermarks,
HCB1 canonical batches, gap/replay, transactional apply). Each request/reply
pair is a full bidirectional anti-entropy round: the request carries the
sender's watermarks (the responder's export selector) plus its push batches;
the reply carries a typed apply status (ok / sync-gap / quota-exceeded /
value-too-large / read-only / batch-refused), the responder's post-apply
watermarks (the acknowledgement the sender persists as its push cursor), and
the responder's bounded export — `more` means re-request, and the freshly
applied watermarks ARE the resume cursor. Both directions of initiation work:
browser-dialed rounds (wasm `crrSyncOn` → hive-p2p `serve_browser_conn` →
`browser_db::sync_round`) and fleet-dialed pulls (`BrowserPool::crr_sync`,
exposed for operators as `POST /v1/browser/dbs/sync/:endpoint_id`, metadata
only — DB bytes never leave the CRR protocol). Grants ride the admission: a
server-derived `db` capability block (`tenant, project, access, max_bytes,
max_value_bytes, db_file, schema, sync_peers, expires_ms`) plus a replicated
`BrowserAdmission.db` grant the exchange re-checks on EVERY request against
its own admission view AND the live descriptor (Public scope additionally
requires the live spec's `public_read`; foreign-tenant/unknown/block-removed
are the identical refusal). Caps enforce with typed refusal + whole-batch
rollback, never truncation; `BrowserDbPolicy.schema` (`{name, ddl}`) is how
both halves derive schema cr-sqlite doesn't replicate. Replica files are
platform-templated (`$HIVE_DATA/browser-dbs/hive-browserdb-{sanitize_tag(
project)}.db`), GC'd with the browser_artifacts blast-radius guards and a
30-day inert grace. Witnessed live on a real two-process setup (local
hive-cloud with enforced JWT + embedded relay, real Chrome tab running the
sqlite DedicatedWorker against OPFS): bidirectional convergence both ways
through the wire op, fleet-initiated pull, reload resuming from durable
watermarks with zero re-push, hand-crafted gap refused typed with no partial
state, oversized/quota pushes refused typed with rollback, revocation cutting
replication + OPFS wipe, and the pre-change-binary refusal contrast
(`HIVE_BROWSER_DB_LISTEN=0` → NO_HANDLER). See
`docs/browser-db-contract.md` and AGENTS.md's browser-replicated-databases
section.

## (pending) — Auto-deploy webhook-less git projects by polling the tracked branch

`git push` silently never deployed for any project imported as a plain public
repo URL (or whose owner never completed the GitHub connection): those have
`git_ci == None` and no webhook, so GitHub never notifies the platform and the
sole auto-deploy trigger (`admin::git_webhook`) never fires — with no visible
error (no failed delivery, because no webhook object exists). This was the exact
break for `shoomoo` (public repo `fatbearsk/serverless-clawdbot`, branch
`xstate`: deployed `664328b` while HEAD was `e80e18c2`). Added
`git::spawn_git_poll_reconcile`, a leader-only reconciler that polls each
git-sourced project's tracked-branch HEAD with `git ls-remote` (host-agnostic,
no GitHub REST rate limit, no credential for a public repo) and starts the same
build the webhook would whenever HEAD has advanced past the deployed commit.
Per-project SHA dedup (`CloudState::git_poll_seen`, seeded from the deployed
commit) makes a push deploy exactly once and never lets the poller and a real
webhook double-fire. Witnessed live: on rollout the leader auto-deployed six
previously-stuck webhook-less projects, each exactly once, and left already-
current projects (including the just-caught-up `shoomoo`) untouched.

## (pending) — Fix GitOps sync silently reading the wrong tenant + unbounded hang

Production GitOps sync intermittently showed "GitOps sync failed / Failed to
fetch" with a Retry button, even for tenants with a genuinely linked repo. The
prior "missing or invalid bearer token" fix (below) covered every *mutation*
through `gitops-server.ts`'s shared `backend()` helper, but left its *reads*
unauthenticated: `ui/app/api/gitops/sync/route.ts`'s own `/v1/gitops` link
lookup and all 10 concurrent `backend()` calls inside `buildOrgArtifacts`
(`/v1/teams/:team`, `/v1/gitops/projects`, `/v1/overview`, etc.) omitted the
caller's platform JWT. Under JWT enforcement, `admin.rs`'s `tenant()` resolves
any request with no claims to `ANON_TENANT` rather than the real caller — so
the link lookup silently read the (empty) link for `"__anon__"` and returned
`{skipped:true, reason:"no-config-repo"}` even when the tenant had GitOps
linked, and the `/v1/teams/:team` read hit `require_team`'s 403. `backend()`
now threads `authToken` through every read too, and `buildOrgArtifacts` takes
an `authToken` parameter forwarded by both of its callers
(`api/gitops/sync`, `api/gitops/init`) so every request in the chain resolves
to the real tenant.

Separately, `backend()` had no timeout: a single hung backend or GitHub API
call anywhere in `buildOrgArtifacts`/`commitFiles` could stall the whole
`/api/gitops/sync` request indefinitely, which is the shape of failure that
eventually surfaces client-side as a raw "Failed to fetch" rather than a clean
HTTP error. `backend()` now wraps its `fetch()` in an `AbortController` with a
20s timeout, matching `lib/api.ts`'s `fetchWithTimeout` default, so a hang
degrades to a retryable HTTP timeout that `gitops.tsx`'s existing retry/
backoff logic already handles. Witnessed: `cargo check -p hive-cloud` and
`npm run build` (ui/) both compile clean with these changes.

## (pending) — Fix build status stuck at "0 lines, Waiting for logs…" forever

A fresh deployment's progress page (`/deploy/[id]`) could get stuck showing
"Deployment started 0s ago…" with 0 log lines forever, even though the build
had genuinely started (and often finished) on the backend. Root cause: a
deploy mutation (`POST /v1/git/deploy`) is always forwarded to the current
control-plane leader by `admin_ingress`, so a fresh build frequently lives
only on the leader's in-memory `BuildStore` — but status *reads*
(`GET /v1/builds/:id`) are served best-effort local and are never forwarded.
A dashboard poll landing on a different (non-leader) fleet node than the one
that ran the build had no way to find it, and 404'd on every single poll
forever. `build_get` now mirrors the fallback its sibling `deployment_build`
already had: on a local miss, if this node isn't the control-plane leader, it
proxies the read to the leader via `fetch_from_host` before giving up with a
genuine 404. Witnessed: reproduced the exact stuck state live (a build
present on one fleet node, invisible via `GET` from a peer node), confirmed
the fix compiles clean and the genuinely-unknown-id case is unaffected.

## (pending) — Fix "missing or invalid bearer token" on private-repo deploys

Deploying (or redeploying) a repo through the dashboard failed with
`missing or invalid bearer token`. The deploy is POSTed to a same-origin Next
server route (`/api/git/deploy`) — not straight to the backend — so the route can
attach the user's GitHub clone token server-side. But the shared `backend()`
helper forwarded only `x-hive-team`, never the user's platform JWT: unlike the
`/cloud` rewrite proxy, a server→backend `fetch()` doesn't carry the browser's
`hive_jwt` cookie, so the backend's `require_auth` rejected the POST. `backend()`
now forwards the caller's platform token (via `authTokenFrom(req)`, which reads
the httpOnly `hive_jwt` cookie or an incoming `Authorization: Bearer`), and every
server-route mutation that goes through it — deploy, redeploy, and the GitOps
init/sync/integrations-link writes (the same latent class bug) — passes it
through. The GitHub *clone* token (`git_token`) is a separate credential and is
untouched; both are server-side only. Witnessed: the deploy that returned 401
now returns a `build_id`, and the no-token path is unchanged.

The same audit found the marketplace toolkit-index cache (`/api/composio/toolkits`)
silently failing its `guardian-db` write for the same reason (operator-gated
admin-data endpoint, no token) — so under enforcement it never persisted and
re-fetched Composio's full 1,000+ catalog on every Integrations load. It now
mints a short-lived `_global` operator token (server-side, via the internal
token + owner email) for the read and write, so the index caches (witnessed:
consecutive loads now serve `source: guardian-db`).

## (pending) — First-party GitHub App OAuth (private repos & orgs for projects/deployments)

Private organization repos still couldn't be used for projects and deployments:
the Composio-managed OAuth app can't grant org access (no `read:org`, and orgs
with OAuth-app restrictions had to approve a third-party app). The platform now
has its OWN GitHub App (org-level permissions) wired end-to-end. `POST
/api/github/connect` returns the first-party
`github.com/login/oauth/authorize` URL (HMAC-signed state, CSRF-bound to a nonce
cookie); GitHub redirects to the registered `/oauth/github/callback`, which
exchanges the code and seals the user token into an AES-256-GCM-encrypted
httpOnly cookie — the token is never stored server-side, never readable by the
browser, and auto-refreshes (expiring-token Apps) or is cleared on refresh
failure so status stays honest. A new `lib/github` facade fronts every GitHub
operation (repos, orgs, org repos, repo creation, webhooks, Actions variables,
GitOps commits, and the deploy/redeploy clone token) preferring the App token
with direct GitHub REST calls, and falling back to the existing Composio
connection unchanged — existing users are unaffected, and with neither
configured everything degrades exactly as before. Org access is now an App
*installation*: the Integrations GitHub card links "install the GitHub App on
the organization" (from the user's installation `app_slug`), and an
org-not-installed 403 threads the org's installations-settings URL through the
existing restricted/approve UI. Disconnect revokes the grant on GitHub and
clears the cookie. Witnessed live: credential validity, all callback error
paths, cookie crypto roundtrip + tamper rejection, expired-token refresh
failure clearing, Composio fallback, and the secret appearing in zero tracked
files.

## (pending) — Responsive lazy-loading, ISR, Speed Insights, image optimization & LLM SEO

The dashboard shipped none of the Vercel-style performance/discovery practices in
a way that was actually present on mobile: the root layout was `force-dynamic`, so
**every** route rendered dynamically (zero ISR, even the public marketing/docs
pages), most routes had no loading skeleton, images were raw `<img>` (no
responsive/modern-format optimization), the Speed Insights view had no in-app
collector, and there were no SEO artifacts for search or AI crawlers. This applies
all of them across the board, responsively.

- **ISR**: the root `force-dynamic` is removed. It was only ever needed for the
  home route's landing↔dashboard flip, which (like all auth chrome) is
  client-side — no page reads server auth (`auth()`/`cookies()`/`headers()`), so
  it was safe to lift. The home route keeps `force-dynamic` via a thin server
  shell (`page.tsx` → `home-client.tsx`) so Clerk SSR still avoids the flash;
  every public marketing + docs page is now `force-static` + `revalidate: 3600`
  (real ISR, `○`/`◐` in the build), and the per-user dashboard pages render a fast
  static shell that fetches data client-side.
- **Lazy loading**: a responsive `loading.tsx` skeleton (`PageSkeleton`) now
  covers every route (66 added), so navigation shows an instant, layout-stable
  placeholder on mobile; the heaviest client components (React-Flow graphs, Tremor
  charts, the command bar) stay `next/dynamic`-split.
- **Image Optimization**: all 25 raw `<img>` became `next/image` (responsive
  `srcset`/`sizes`, AVIF/WebP via `next.config` `images`, lazy by default,
  `priority` only on the two LCP logos) — arbitrary external-host avatars/logos
  stay hardened lazy `<img>`. Cumulative Layout Shift measured 0.
- **Speed Insights**: a self-contained Core Web Vitals beacon (`VitalsBeacon`) in
  the root layout collects FCP/LCP/CLS/INP/TTFB on every device and posts to the
  vitals sink, so the Observability → Speed Insights view reflects the dashboard's
  own real-user performance.
- **SEO for LLMs / AI search**: added `robots.ts` (welcomes GPTBot, PerplexityBot,
  ClaudeBot, Google-Extended, …), `sitemap.ts`, `llms.txt`, schema.org JSON-LD
  (Organization/WebSite/SoftwareApplication), a dynamic OpenGraph image, and
  unique per-page `metadata` (title/description/OG/canonical) with one `<h1>` per
  public page.
- **Responsive**: an explicit `width=device-width, initial-scale=1,
  viewport-fit=cover` viewport (zoom left enabled for accessibility). Verified no
  horizontal overflow at 375 / 768 / 1280 px.

## (pending) — GitHub connection management (scopes, orgs, reconnect, disconnect)

Users who connected GitHub for a private org or repo had no way to see what the
connection actually granted, re-authorize with adjusted scopes, request
organization approval, or disconnect and reconnect — and the platform made two
silent mistakes: it swallowed GitHub's org OAuth-app-restriction `403`s (a
restricted org's repo list just came back empty), and it treated
revoked-but-still-`ACTIVE` Composio connections as connected, showing a false
green. The Integrations page's GitHub "Configure" and "Disconnect" buttons were
dead no-ops.

The connection is now honest and fully manageable. `githubConnectionDetail`
reports `connected` only when the token is BOTH active in Composio AND live
against GitHub (a `GITHUB_GET_THE_AUTHENTICATED_USER` probe unmasks dead-`ACTIVE`
tokens and drops the cached token), and surfaces the granted scope names, the
account login, and whether private-repo (`repo`) and org-enumeration (`read:org`)
access are present — never a token value. New helpers and routes: `githubOrgs` +
`GET /api/github/orgs` (the user's orgs, degrading to `[]` without `read:org`);
`disconnectGithub` + `POST /api/github/disconnect` (deletes every connected
account for the user, so a later reconnect binds fresh); reconnect = disconnect
then re-run OAuth; `githubOrgRepos` now requests `type:all` and returns
`{repos, restricted, approve_url}` on an org OAuth-app restriction, via a shared
`orgApproveUrl` that parses GitHub's 403 body and falls back to the org's
third-party-apps policy page (never an empty link), also used by repo creation;
and `githubAuthConfigId` honors a `HIVE_GITHUB_AUTH_CONFIG_ID` pin, otherwise
requesting `[repo, read:org, workflow]`. The Integrations page gains a real
GitHub card (login, scope badges, accessible orgs, a red reconnect-needed banner
when the token is dead, and wired Disconnect / Reconnect-adjust-access /
Set-up-GitOps buttons); the GitOps modal swaps the free-text org field for a
dropdown with an approve link and a Re-check-access button and can be re-opened
on demand; and New Project merges organization repos into the import list.
Because Composio's managed GitHub app does not grant `read:org`, org
*enumeration* degrades gracefully and only prompts a (non-disruptive) reconnect
when a user actually needs it — private-repo access, which already works via
`repo`, is never interrupted. Verified live on all 7 fleet nodes: enriched
status serves, a dead-`ACTIVE` entity reports `live:false`, `/api/github/orgs`
and `/api/github/disconnect` are registered, and no response carries a token.

## (pending) — Deploy private GitHub repos (inject the connected-GitHub token)

Deploying a private GitHub repo failed with `fatal: could not read Username for
'https://github.com'` — the build's `git clone` ran anonymously and no layer ever
attached a credential. Now the user's connected-GitHub token is plumbed to the
clone: `GitDeployRequest` carries a `git_token` (fetched server-side by new
`/api/git/deploy` + `/api/projects/[project]/redeploy` routes from the user's
Composio GitHub connection, never exposed to the browser), and the backend injects
it as an `x-access-token` clone URL (github.com-only), with the token scrubbed from
clone stderr, an anonymous retry if a stale token is rejected (so public repos are
never broken), actionable no-credential vs rejected-credential error messages, and
the token cleared after the clone. A node-level `GITHUB_TOKEN` still works as a
fallback (e.g. for webhook auto-deploys). Verified live on the fleet; public repos
unaffected.

## (pending) — Usage page defaults to Monthly + lazy charts; fix flaky capacity test

The dashboard Usage view now selects the **Monthly** granularity on first load
(was Daily). The page is split into a server shell (`page.tsx`, carrying an ISR
`revalidate` config) and a client `usage-view.tsx`; the Tremor charts stay
code-split via `next/dynamic` and now render loading skeletons so the deferred
chunk never leaves a layout-shifting gap. (ISR note: the root layout's
`force-dynamic` — required for auth-correct chrome — currently supersedes per-page
static rendering app-wide, so `/usage` still renders dynamically; the ISR config
is kept forward-compatible and, since the page carries no server data, caches
nothing of value regardless.)

Also fixes a flaky control-plane test (`capacity_is_released_after_builds`):
capacity release is eventual-consistent and lagged the job's terminal transition,
so the test's immediate `vcpus_used == 0` assert raced — now it waits for capacity
to drain first (a real leak still fails, deterministically).

## (pending) — GitOps config sync is server-side only (delete in-browser git)

GitOps config-repo mirroring used to fall back to an in-browser isomorphic-git
path (`gitops-local` → `isogit` → `/api/git/cors-proxy`) whenever the server
route skipped (GitHub not connected), and that browser path was unreliable. The
server-side sync (`/api/gitops/sync` → Composio GitHub API), which is the path
that worked, is now the sole mechanism: `triggerGitopsSync` no longer imports or
runs any browser git — on a skipped sync it records a benign "not configured"
status and the Set-up-GitOps onboarding is how a user connects GitHub. The entire
client-side git subsystem is deleted (`ui/lib/gitops-local.ts`, `ui/lib/isogit.ts`,
`ui/app/api/git/cors-proxy`, and the `isomorphic-git` + `@isomorphic-git/lightning-fs`
dependencies), so the dashboard bundle ships zero browser-git code. Verified
server-only across all fleet nodes (`/api/gitops/sync` 200, `/api/git/cors-proxy`
404, zero isomorphic-git in every bundle).

## (pending) — Fix Redeploy 404 for zip-uploaded and image projects

The Redeploy modal (and `shadw projects redeploy`) returned a bare
`POST /v1/projects/<p>/redeploy -> 404` for any project deployed from a ZIP
upload or a prebuilt image. `project_redeploy` resolved the source through
`git_for_project_fleet`, which filters `GitSource::is_real_git()` and so
returns `None` for the synthetic `upload://`/`image://` pseudo-URLs those
deploys stamp into `repo_url` — a git-only redeploy that never worked for the
non-git half of the platform. Now it resolves the newest source unfiltered
(`source_for_project_fleet`) and rebuilds by kind: git re-clones; images
reconstruct their `image_ref`; zip projects rebuild from RETAINED source —
a durable `<project>.src.zip` kept at upload time (GC-safe, since the build-dir
reaper skips non-directory files), falling back to a copy of the prior build's
on-disk checkout. Because placement stickiness is container-lease-only, a zip
redeploy pins to the node holding the source (local `no_fanout` build, else the
redeploy forwards to the host node over the existing fanout transport). Genuine
failures now return a descriptive 4xx body instead of an empty 404. Verified
live end-to-end on the fleet (the reported `rfc-blog-page` redeploy: 404 → 200,
built to Ready, still serving). Also removed a stray root-level `test.js`
integration harness.

## (pending) — Fix admin incidents page; generic leader→follower store replication for the whole node-local divergence class

The admin incidents page didn't load and "create didn't work": `IncidentStore`
was node-local, so incidents created on the control-plane leader (where every
mutation forwards) were invisible to reads that the dashboard's multi-A DNS
landed on any other node. A fleet audit found this was a whole CLASS —
~12 stores (apikeys, webhooks, databases, domains, integrations, gitops, docs,
notifications, identity, enterprise, teams, incidents) took mutations only on
the leader but served reads from the local store. Rather than hand-write a
gossip arm + adoption block per store, added one generic mechanism
(`store_sync::REGISTRY` + a `/v1/store-snapshot/<name>` gossip arm + a follower
loop that adopts each store's snapshot when it changes). Serialization is
canonicalized through `serde_json::Value` (sorted keys) so the byte-compare
change-gate is stable even for HashMap-backed stores with nested maps — a first
cut re-adopted databases/domains/gitops every tick until that landed.
`apikeys` was the severest case: a key minted on the leader now verifies on
every node instead of failing API auth on followers. Audit replication was
live-verified adopting 659 entries onto a follower. Also added
`DELETE /v1/incidents/:id` (was resolve-only). Secret-bearing stores ride the
existing peer-trust-enforced signed mesh.

Edge-enforcement config (WAF rules, redirects/rewrites, bot policy) replicates
too, so every node enforces the leader's config. Cron is the exception — its
jobs are split across nodes (manual jobs on the leader, `vercel.json` jobs on
the building node), so `cron_list` fans out read-only and merges instead
(execution stays per-node, never double-firing).

`securelinks`/`audit` gained `snapshot()`/`load()`; `Team`/`Member`/`Incident`
gained `PartialEq`; `vendor/guardian-db` is now a workspace-excluded path dep so
its own test suite runs standalone.

## (pending) — fc-sanjose-2 bring-up; fix workflows-page 400, teams-mirror corruption, and guardian-db index flakiness

Brought up the fleet's 10th node, fc-sanjose-2 (43.173.78.95): PVM kernel
built from `virt-pvm/linux@pvm-612` (the ansible `pvm_firecracker` role now
clones that base and applies the fsgsbase/rdtscp fallback patches only on
hosts that need them — the patch repo's own `kernel/` dir is a non-buildable
browsing tree), firecracker-next, podman+gVisor, backend, dashboard, embedded
+ standalone relay, guardian replication, and a hardened lockdown (9090
blocked publicly). `hive-lockdown.sh` is now git-tracked and deployed by the
`prerequisites` role, which also disables+masks `firewalld` — a fresh Rocky
10 image ships it active and its restrictive default zone silently blocked
the relay/discovery ports underneath the hive rules. The PVM role gained a
`pvm_already_provisioned` idempotency gate (a re-run on an already-PVM host
used to self-referentially re-configure from the PVM kernel's own config and
break the build).

`GET /v1/workflows/runs?summary=1` returned 400 (the dashboard workflows page
couldn't load runs): axum's `Query` bool deserialization only accepts literal
`true`/`false`. All 7 `Option<bool>` query fields in `admin.rs` now use a
lenient deserializer accepting `1`/`0` (matching the mesh-RPC path's existing
convention); swept every crate — the class is confined to `admin.rs`.

The relational teams mirror lost 3 of 5 real teams fleet-wide: the TeamStore
itself diverges across nodes (mutations land only on the control-plane
leader; followers never merged them), so a brief failover put a stale
stand-in in charge of `sync_teams`, whose delete-reconciliation wiped every
team it had never heard of. Followers now adopt the leader's TeamStore each
mirror tick via a new `/v1/teams/snapshot` mesh arm, and the delete phase is
tombstone-guarded (`updated_ms` staleness) so a live leader's rows can't be
wiped. Separately, guardian-db's document-store index rebuild dropped rows
whose content-blob fetch transiently failed (table counts flickered to 0 on
healthy nodes) — it now falls back to the previously-fetched value. Every
guardian SQL op is also bounded by a 10s timeout (a corrupted first-open used
to hang reads forever with zero signal).

## 88fe215 — Fix multi-tenancy data-visibility/usage-consistency bugs; add relational layer on guardian-db 0.18; add per-node relay + guardian-db anti-entropy

Team Simpfi's `drugs-wtf` project and fleet-wide billing/metrics were reported
missing/inconsistent — `ProjectStore`/`BillingStore` were node-local-only with
no gossip replication. Adds a relational mirror on GuardianDB's native SQL
layer (upgraded 0.17.2 → 0.18.0) plus a fleet-aware fallback in
`gitops_projects`, closing the visibility gap. Separately root-caused the
actual cross-node read failures to a missing `fetch_from_host`
discovery-fallback and missing `gossip::dispatch` routes for the billing
endpoints — fixed both; billing now converges byte-identical fleet-wide.

Implements the requested per-node relay + guardian-db anti-entropy
architecture: every node embeds and gossips its own relay, a relay-selection
algorithm replaces stale connect hints, and a 60s anti-entropy loop detects
and reconciles guardian-db divergence between random peers — live-verified
converging a real fleet divergence.

Found and fixed a live plaintext-secret leak (a real GitHub PAT served
unmasked via project settings) with a credential-shape auto-detector, and
cleaned up 3 abandoned duplicate Simpfi projects.

## a29c4f1 — Docs + a one-time fleet-wide sweep for existing leaked secrets

Ran the new credential-shape detector as a one-time backfill across every
project on every live node (not just future writes) and found two more real,
already-exposed OpenAI API keys (`fatni`, `shoomoo`) beyond the original
GitHub PAT — resealed all of them; a follow-up re-scan confirmed zero
credential-shaped values remain stored as non-sensitive fleet-wide.

## c2bbfce — Add caching + a read-only PostgreSQL/tables view to the admin Data Browser

The Data Browser's collection/row/table reads are now client-cached for 15s
(mutations still bust the cache immediately, unchanged). Adds a Documents |
Tables (PostgreSQL) view toggle backed by the relational layer above: two
new admin endpoints (`GET /v1/admin/sql/tables`, `POST /v1/admin/sql/query`)
enforced SELECT-only server-side, live-verified end-to-end through the real
dashboard including the guardrail actually being exercised via the UI's own
query box. Along the way, confirmed the dashboard's `/ops` proxy forwards to
the current control-plane leader regardless of which node runs the
dashboard process — a new admin endpoint needs the leader deployed first.

## 41459ad — Fix stale launchd labels in shadw-watchdog.sh

The KeepAlive-backstop watchdog (`dev.shadw.watchdog`) still targeted the
pre-rename labels `dev.shadw.node-a`/`dev.shadw.node-b`, so every 30s tick
silently no-op'd after the local nodes were renamed to
`dev.shadw.fc-lax`/`dev.shadw.fc-lax2` — letting fc-lax2 sit crashed (a
third-party `noq`/`noq-proto` panic-in-destructor abort) for 9h13m despite
launchd `KeepAlive=true`. Updated the watchdog's targets to the current
labels; verified live.

## 166ea99 — Fix stale fleet dashboard + a stale va binary; add scripts/deploy-ui-fleet.sh

The Data Browser's Tables (PostgreSQL) view (c2bbfce) was invisible on the
real https://shadw.cloud because that rollout updated only the backend
binary, never the ui/ dashboard (systemd `hive-ui.service`) — every public
node kept serving a pre-feature build. Separately, va was serving a stale
`hive-cloud` binary that predated the SQL routes entirely (a prior rebuild
was never swapped into its live path). Rebuilt+restarted the dashboard on
all 6 public nodes and swapped in va's correct binary; live-verified via
the compiled bundle, the backend routes, and a real browser hit against
shadw.cloud. Adds `scripts/deploy-ui-fleet.sh` so this doesn't regress
silently again.

## f6c9798 — Add teams/team_members/deployments to the admin SQL view + full billing backfill

The SQL (PostgreSQL) view was missing whole surfaces: no teams, users
(team_members), or deployments tables, and `billing_accounts` only held
tenants actively metered on a tick (4 rows while 20+ tenants existed).
Adds three view-only tables plus `spawn_relational_mirror_loop` — teams,
members, and a FULL billing snapshot backfill sync from the control-plane
leader, own-deployments from every node, content-hash-debounced so quiet
ticks write nothing. Fleet-rolled and live-verified: billing_accounts
4 → 23 rows, real teams/members/deployment rows replicated cross-node,
existing tables and the SELECT-only guard unchanged.

## a2af203 — Watchdog: persistent KeepAlive loop; launchd pended-spawn root cause

fc-lax2 crashed again (same upstream `noq` abort) and sat down because the
watchdog LaunchAgent had never fired: launchd on this long-uptime gui domain
reports `pended nondemand spawn = speculative` and indefinitely defers
StartInterval/RunAtLoad spawns — only demand spawns (`kickstart`) run.
Converted the watchdog to a persistent self-looping KeepAlive daemon
(`WATCHDOG_LOOP=1`), re-bootstrapped, and adversarially verified: SIGABRT'd
fc-lax2 and watched the watchdog restore it autonomously in under a minute.
Separately, fc-virginia and fc-virginia-2 are userspace-frozen (ICMP alive,
all service ports dead from every vantage incl. VPC-internal) and need a
Tencent-console reboot — out of reach from this session.
