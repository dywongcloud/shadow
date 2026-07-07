# Constraints — hive platform (gm decision-arbiter)

## Verification
- No synthetic test files/suites. Verify by driving the REAL fleet: `exec_js`/Bash
  against live nodes (SSH key `~/Documents/billing.pem`, hosts bkk/va/va2/va3/sj),
  `browser` against the live dashboard, real `curl` to `*.shadw.app` / `/cloud/*`.
- A fix is closed only when its live invariant is witnessed (200 response, correct
  count, scoped logs, no error toast) — never by "the code looks right".

## Build/deploy invariants
- Rust workspace builds with `cargo build --release -p hive-cloud --features zkauth`.
  `zkauth` MUST be on for every node (plain build strips preview auth → 404).
- GLIBC: Virginia (Rocky 10) binary needs GLIBC_2.39 and CRASH-LOOPS on Bangkok
  (TencentOS). Bangkok MUST build natively (it has cargo, 96 cores, ~75s). va2/va3/sj
  accept the VA binary. Binary paths: bkk=`/root/hive/target/release/`, others=`/root/fc-target/release/`.
- Replace a running binary with `mv -f` then `systemctl restart hive-node`; never cp-over-running.
- Only the Mac reaches all nodes over SSH; fleet nodes cannot SSH each other.

## Correctness posture
- Reads are distributed: every node answers from gossip-replicated state. A fleet-wide
  count/list MUST aggregate `peer_deployments` + gossiped stores, never a single node's
  local `gw.list()`. Mutations serialize through the elected control-plane leader.
- Multi-tenant isolation is load-bearing: never widen a tenant filter to fix a count;
  aggregate the RIGHT tenant's data across nodes instead.
- Durability: a hosting fix must survive `systemctl restart` and node reboot with no
  manual step (no hand-freeing IPs, no manual container rm).

## Client edits
- Any `ui/**` `.ts/.tsx/.css` edit is gated on a `browser` witness of its invariant
  before COMPLETE. The `/cloud/:path*` rewrite target is baked into
  `.next/routes-manifest.json` at build — a plain server restart does NOT pick up a new
  HIVE_ADMIN; rebuild or patch the manifest + `launchctl kickstart -k dev.shadw.dashboard`.

## Hygiene
- No graphical/decorative Unicode in code (arrows, bullets, checks) — ASCII only.
- Commit + push at end of session; porcelain-clean gate before push.
