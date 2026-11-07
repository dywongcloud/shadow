# Per-account TOTP 2FA

Status: **implemented (source in the working tree).** No passing test run, browser witness, CI
result or deployment is claimed here. Present tense; every claim cites `path:line`. Line numbers
are hints that drift as the tree moves. History lives in `CHANGELOG.md`; this page states only what
must be true of the code now.

## Citation drift

`path:line` numbers are observed at a point in time and drift as the tree moves; SYMBOLS are
authoritative. These six symbol-to-location mappings are re-verified against the current tree:

- `RULES` -> `crates/hive-cloud/src/roles.rs:72`
- `mint_token` -> `crates/hive-cloud/src/admin.rs:594`
- `step_up_ok` -> `crates/hive-cloud/src/auth.rs:294`
- `TotpRow` -> `crates/hive-cloud/src/totp.rs:75`
- `REGISTRY` -> `crates/hive-cloud/src/store_sync.rs:131`
- `ct_eq` -> `crates/hive-cloud/src/admin.rs:440`

## Current state

The feature source is present in the working tree at the moment of writing. A filename scan of the
source tree returns exactly one TOTP file, `crates/hive-cloud/src/totp.rs`, and no 2FA-named
source file; the only 2FA-named artifact is this document. An exhaustive literal scan of
`crates/hive-cloud/src` locates four registered 2FA routes
(`crates/hive-cloud/src/admin.rs:161`, `:165`, `:169`, `:173`): `GET /v1/account/2fa/status`,
`POST /v1/account/2fa/enroll`, `POST /v1/account/2fa/confirm` and
`POST /v1/account/2fa/disable`, each with a viewer-floor `RULES` row
(`crates/hive-cloud/src/roles.rs:83-86`). The `totp` store is registered in
`REGISTRY` (`crates/hive-cloud/src/store_sync.rs:512-524`) and named in `MERGE_STORES`
(`crates/hive-cloud/src/store_sync.rs:530-537`); its snapshot field is
`crates/hive-cloud/src/persist.rs:99-103` and its in-process handle is
`crates/hive-cloud/src/state.rs:354-355`. `POST /v1/token` remains registered at
`crates/hive-cloud/src/admin.rs:159`, and `hive-cloud --totp-selftest` is a diagnostic entry
point (`crates/hive-cloud/src/main.rs:508-511`), not a test file. An exhaustive scoped scan for
`two_factor` returns zero matches; the only `2fa` hit outside the feature files is the
`%2Fa` URL-encoding fragment in a comment (`crates/hive-cloud/src/gossip.rs:1877`). This is a
source-tree observation, not a claim that any run has passed.

## What the feature is, and what optional means

`crates/hive-cloud/src/totp.rs:1-23` defines optional per-account TOTP (RFC 6238) two-factor
authentication. Optional is a property of the ROW, not of the platform: one `TotpRow` exists per
account, keyed by the verified `Claims.sub`, and every new field carries `#[serde(default)]`
(`crates/hive-cloud/src/totp.rs:71-107`). `otp_required` starts false and is set only by a
compare-and-set confirm (`crates/hive-cloud/src/totp.rs:302-361`), so an account that has never
enrolled keeps minting through the unchanged single human mint path
(`crates/hive-cloud/src/totp.rs:236-241`, `crates/hive-cloud/src/admin.rs:590`).

The identity key is `Claims.sub`, the cryptographically-bound JWT subject
(`crates/hive-cloud/src/auth.rs:26-45`); the dashboard derives it from the verified Clerk session
and never from client input (`ui/app/api/token/route.ts`). It is never the tenant, the email, a
SAML NameID, or the spoofable `x-hive-team` header (`crates/hive-cloud/src/totp.rs:10-11`,
`crates/hive-cloud/src/auth.rs:500-504`). A mint can assert ANY tenant, so tenant is not an
identity; email resolves the `platform_admin` operator signal
(`crates/hive-cloud/src/auth.rs:39-44`) and SAML ACS is a separate surface
(`crates/hive-cloud/src/roles.rs:123`). `caller` refuses an empty sub, `__untagged__`,
`key:` API-key subjects, fleet service identities and queue clients
(`crates/hive-cloud/src/totp.rs:663-678`). `UNTAGGED_TENANT` is the fail-closed sentinel
`"__untagged__"` (`crates/hive-cloud/src/admin.rs:3395`,
`crates/hive-cloud/src/persist.rs:20`): a lost tag resolves to an invisible namespace rather than
to the owner's live tenant.

## Why the gate lives inside mint_token, before auth::issue

`POST /v1/token` is registered once (`crates/hive-cloud/src/admin.rs:159`) and is the single
human JWT mint. The 2FA gate sits inside that handler immediately before `auth::issue` signs a
token (`crates/hive-cloud/src/admin.rs:616-649`, `crates/hive-cloud/src/totp.rs:3-6`). Three
properties of the surrounding code make any other placement a complete bypass:

- `POST /v1/token` is an `is_open_route`, so `require_auth` skips the mutation gate and the
  role matrix for it (`crates/hive-cloud/src/auth.rs:534-539`, `crates/hive-cloud/src/main.rs:2188`).
- `admin_ingress` serves `POST /v1/token` locally and explicitly exempts it from leader
  forwarding (`crates/hive-cloud/src/main.rs:2213-2215`), so an owner-forward never sees the mint.
- `roles::gate` returns `Ok(())` immediately for `platform_admin` and fleet identities
  (`crates/hive-cloud/src/roles.rs:316`), and a new `RULES` row cannot change that
  short-circuit. The self-service 2FA `RULES` rows are viewer-floor and the source says outright
  that the OTP gate is never a rule (`crates/hive-cloud/src/roles.rs:80-86`). The dashboard mirror
  of the matrix only decides whether to offer a control; the backend is the authority
  (`ui/lib/team-role.ts:5-8`).

A handler-local check misses the mint, and a node-local store is a complete bypass because public
hosts are round-robin: a node that has not merged the row answers as though the account never
enrolled (`crates/hive-cloud/src/store_sync.rs:512-524`,
`crates/hive-cloud/src/store_follower.rs:157-162`).

Because an already-issued token also has to be re-checked on every request, the second half of the
gate is at `auth::principal`, the ONE principal resolution shared by `require_auth` and
`admin_ingress` (`crates/hive-cloud/src/auth.rs:408-410`, `:524`;
`crates/hive-cloud/src/main.rs:2190-2195`). Both ingress paths call `step_up_ok` on the resolved
claims (`crates/hive-cloud/src/main.rs:2203-2209`), so a forwarded mutation cannot skip the
re-check. `require_auth` inserts the verified `Claims` into request extensions, never
client-settable (`crates/hive-cloud/src/auth.rs:500-504`, `:518-527`).

## State is replicated, never node-local-only

Every row (sealed secrets, one-shot challenges, replay steps, lock counters, tombstones) rides
`store_sync::REGISTRY` as the `totp` `SyncedStore`
(`crates/hive-cloud/src/store_sync.rs:131`, `:512-524`) and is named in `MERGE_STORES`
(`crates/hive-cloud/src/store_sync.rs:530-537`), so adoption is a per-row merge, never a wholesale
replace (`crates/hive-cloud/src/totp.rs:143-187`, `:206-230`). A wholesale replace is admissible
only from an owner every node agrees on: the chain HEAD or the identity election, and never a
`Fallback` (`crates/hive-cloud/src/cluster.rs:46-55`,
`crates/hive-cloud/src/store_follower.rs:138-147`, `:175-176`).

The snapshot field is `PlatformSnapshot.totp` behind `#[serde(default)]`
(`crates/hive-cloud/src/persist.rs:99-103`, `:23-141`). This is load-bearing: `persist::load`
deserializes with `unwrap_or_default` (`crates/hive-cloud/src/persist.rs:242-244`,
`:100-101`), so a serde-REQUIRED key that an older reader does not know silently yields an entirely
empty snapshot and wipes ALL state. A serde-required key is never removed from a persisted struct in
one release. The in-process store is `CloudState.totp`
(`crates/hive-cloud/src/state.rs:354-355`).

Rows carry a strictly monotonic per-row `updated_ms`
(`crates/hive-cloud/src/totp.rs:88`, `:130-134`, `:170-171`), following the ProjectSettings
precedent (`crates/hive-cloud/src/project_settings.rs:51`, `:465-480`). Disable writes a
PERMANENT tombstone, force-clears secret material, and a tombstoned row dominates any non-deleted
row that is not strictly newer (`crates/hive-cloud/src/totp.rs:103-106`, `:143-187`,
`:477-496`); revoked API-key ids are permanent and survive a newer row that forgot them
(`crates/hive-cloud/src/totp.rs:97-100`, `:175-183`). Snapshots route through
`serde_json::Value` plus `enc_sorted` so nested challenge maps are byte-identical on every node
(`crates/hive-cloud/src/store_sync.rs:514-516`, `crates/hive-cloud/src/totp.rs:136-141`).

## Cryptography and verification

The primitive is RFC 6238 / RFC 4226 HOTP: a 30 s step, 6 digits, HMAC-SHA1, dynamic truncation, and
a plus-or-minus one step tolerance (`crates/hive-cloud/src/totp.rs:41-44`, `:574-611`).
`verify_code` returns the ACCEPTED step and requires `step > last_used_step`, the replicated
replay defense (`crates/hive-cloud/src/totp.rs:590-611`, `:90`). Every secret and MAC comparison
is constant-time: TOTP codes through `admin::ct_eq` and recovery-code hashes through Argon2id
`verify_password` (`crates/hive-cloud/src/totp.rs:605`, `:631-641`,
`crates/hive-cloud/src/admin.rs:436`).

The 20-byte secret is sealed with `secrets::encrypt`
(`crates/hive-cloud/src/totp.rs:277-300`). That function FAILS OPEN: it returns plaintext
unchanged on an empty input, an already-encrypted input, an RNG failure, or a seal failure
(`crates/hive-cloud/src/secrets.rs:284-303`), so enrollment asserts `secrets::is_encrypted` and
refuses otherwise (`crates/hive-cloud/src/totp.rs:283-286`,
`crates/hive-cloud/src/secrets.rs:275-277`). Stored secrets are read ONLY with
`secrets::try_decrypt`, the honest `Option` (`crates/hive-cloud/src/totp.rs:613-618`,
`crates/hive-cloud/src/secrets.rs:103-121`); `secrets::decrypt` is a passthrough that returns
its input unchanged on any failure and never signals an unopenable ciphertext
(`crates/hive-cloud/src/secrets.rs:316-338`). `enc:v1:` is the seal prefix
(`crates/hive-cloud/src/secrets.rs:30`).

Recovery codes are one-use and stored only as Argon2id PHC hashes with their own random salt
(`crates/hive-cloud/src/totp.rs:324-340`, `:620-629`, `:466-470`). A login challenge is
server-side, single-use by a compare-and-set on any terminal outcome, bound to `sub`, and expires
after five minutes (`crates/hive-cloud/src/totp.rs:58-69`, `:363-392`, `:394-475`). The
dedicated per-sub verification throttle is charged on EVERY verification including wrong codes,
before any code comparison (`crates/hive-cloud/src/totp.rs:52-54`, `:200-204`, `:394-407`);
the mint limiter alone is not enough (`crates/hive-cloud/src/admin.rs:468`, `:599-614`). No
secret, code, or challenge id enters a URL, `Location`, HTML, log line, or argv; they travel only
in an authenticated JSON body (`crates/hive-cloud/src/totp.rs:20-21`, `:686-687`).

## Flows

- **Status.** `GET /v1/account/2fa/status` reports only the caller's own enrollment flag over the
  authenticated JSON body, scoped to the verified sub; a read never reveals the secret, recovery
  codes or challenge ids (`crates/hive-cloud/src/admin.rs:161`,
  `crates/hive-cloud/src/roles.rs:83`, `crates/hive-cloud/src/totp.rs:687-697`).
- **Enrollment.** `POST /v1/account/2fa/enroll` requires an asserted human identity and returns the
  base32 secret ONCE over the authenticated JSON body with digits, period, and algorithm, while
  `otp_required` stays false (`crates/hive-cloud/src/admin.rs:160-163`,
  `crates/hive-cloud/src/totp.rs:277-300`, `:686-714`).
- **Confirm.** `POST /v1/account/2fa/confirm` verifies the first code against the pending secret,
  compares-and-sets `otp_required=true`, returns ten one-use recovery codes ONCE, revokes the
  account's `hive_` API keys, and records their ids permanently
  (`crates/hive-cloud/src/admin.rs:164-167`, `crates/hive-cloud/src/totp.rs:302-361`,
  `:716-744`). Both plaintext values are shown exactly once and never stored in plaintext
  (`crates/hive-cloud/src/totp.rs:277`, `:324-340`, `:353`).
- **Login challenge.** A mint for a 2FA account returns `403` with `otp_required` and a
  `challenge_id` rather than a token (`crates/hive-cloud/src/admin.rs:620-647`); the client
  retries the same mint with `challenge_id` and `otp`, and a successful verification stamps
  `otp_at` into the issued token (`crates/hive-cloud/src/totp.rs:394-475`,
  `crates/hive-cloud/src/auth.rs:22-24`).
- **Step-up.** Every protected mutation on a 2FA account asks `step_up_ok` on a fresh `otp_at`
  within `STEP_UP_WINDOW_SECS` (`crates/hive-cloud/src/totp.rs:55-56`,
  `crates/hive-cloud/src/main.rs:2203-2209`). The store lookup is tri-state and fails CLOSED on an
  unreadable store (`crates/hive-cloud/src/totp.rs:243-250`).
- **Disable.** `POST /v1/account/2fa/disable` re-asserts the step-up gate in the handler itself,
  runs through the same challenge, rate-limit and replay path, then writes a permanent tombstone
  (`crates/hive-cloud/src/admin.rs:168-171`, `crates/hive-cloud/src/totp.rs:477-496`,
  `:746-763`).

**Residual.** Already-issued JWTs are not revoked; they keep living until their own expiry, at most
`MINTED_TOKEN_TTL_SECS = 3600` (`crates/hive-cloud/src/admin.rs:41`, `:579`). Rotating
`HIVE_JWT_SECRET` to invalidate them is explicitly forbidden: `secret()` and `enforced()` read
that one variable (`crates/hive-cloud/src/auth.rs:104-108`), so a rotation invalidates every fleet
token and flips enforcement on any node where it is unset. `HIVE_SECRET_KEY` is never rotated here
either (`crates/hive-cloud/src/totp.rs:19`).

## Non-goals

This feature is not, and does not change:

- Deployment password-gate cookies (`crates/hive-cloud/src/edge.rs:1921`,
  `crates/hive-cloud/src/enterprise.rs:632`): a separate sealed cookie over a stored deployment
  password hash.
- SAML ACS (`crates/hive-cloud/src/roles.rs:123`), which verifies no assertion signature, and SCIM
  (`crates/hive-cloud/src/auth.rs:534` open-route set), authenticated by a per-team bearer.
- zkauth registration and proof routes (`crates/hive-cloud/src/auth.rs:534` open-route set).
- Queue client tokens (`crates/hive-cloud/src/auth.rs:78`,
  `crates/hive-cloud/src/roles.rs:28`) and fleet `service` delegation
  (`crates/hive-cloud/src/auth.rs:72`, `:162-168`): `caller` refuses both
  (`crates/hive-cloud/src/totp.rs:663-678`).
- API keys minted before enrollment: they are revoked at confirm and their ids are permanently
  refused (`crates/hive-cloud/src/totp.rs:97-100`, `:252-262`,
  `crates/hive-cloud/src/auth.rs:375`).
- Org tokens: the `admin`, `member` and `viewer` roles keep their own shorter TTL and the
  dashboard re-mints from the live session (`crates/hive-cloud/src/admin.rs:579`,
  `ui/app/api/token/route.ts:24-25`); 2FA does not mint or extend them.

## Live-execution verification runbook

Verification is live execution against the real binary and a COPY of canonical state; the runbook
writes no test file. On macOS keep every scratch path under `/private/tmp`. Keep the operator's
PATH (Homebrew cargo has no cross std). `HIVE_GUARDIAN_RESTORE=0` keeps GuardianDB from
overwriting the copied snapshot (`crates/hive-cloud/src/guardian.rs:4911-4913`,
`crates/hive-cloud/src/persist.rs:1-4`, `:232-234`).

```bash
# Copy, never move, the canonical state; work only on the copy.
mkdir -p /private/tmp/hive-2fa-a /private/tmp/hive-2fa-b
cp "$HIVE_DATA/state.json" /private/tmp/hive-2fa-a/state.json
cp "$HIVE_DATA/state.json" /private/tmp/hive-2fa-b/state.json

# Prove the RFC 6238 primitive with the real binary, no node boot.
hive-cloud --totp-selftest

# Two loopback nodes, one state copy each, distinct admin and mesh ports.
env HIVE_DATA=/private/tmp/hive-2fa-a HIVE_GUARDIAN_RESTORE=0 HIVE_JWT_SECRET=$JWT_A HIVE_INTERNAL_TOKEN=$INTERNAL_A hive-cloud &
env HIVE_DATA=/private/tmp/hive-2fa-b HIVE_GUARDIAN_RESTORE=0 HIVE_JWT_SECRET=$JWT_B HIVE_INTERNAL_TOKEN=$INTERNAL_B hive-cloud &
```

Probe the following over HTTP against node A and record each status and body:

- enroll, then confirm with a valid code; assert the secret and ten recovery codes appear exactly
  once and never again on a second enroll (`crates/hive-cloud/src/totp.rs:686-744`).
- a mint without `challenge_id` returns `403` `otp_required` with a `challenge_id`, and a
  mint with that id plus the code returns a token (`crates/hive-cloud/src/admin.rs:620-647`).
- replay the SAME code and challenge id and assert refusal; step reuse is refused by
  `step > last_used_step` (`crates/hive-cloud/src/totp.rs:590-611`, `:462-473`).
- bypass probes: a mint with no `x-hive-internal`, a forged `x-hive-team`, a
  `platform_admin` claim, a `service` fleet claim, and a `hive_` API key minted before
  enrollment each refuse (`crates/hive-cloud/src/admin.rs:599-614`,
  `crates/hive-cloud/src/roles.rs:80-82`, `crates/hive-cloud/src/totp.rs:663-678`).
- wrong-code probes: charge the throttle on every wrong code and assert the account lock after the
  configured consecutive failures (`crates/hive-cloud/src/totp.rs:45-54`, `:449-460`).
- disable, then assert the tombstone dominates an older replicated row
  (`crates/hive-cloud/src/totp.rs:477-496`).

Replication check: after enrollment on node A, read the same account through node B's admin API and
assert `otp_required` becomes true there; a node that has not merged the row yet is the freshness
case below, not a failure of the merge. Adoption is per-row `MERGE_STORES`
(`crates/hive-cloud/src/store_sync.rs:530-537`,
`crates/hive-cloud/src/store_follower.rs:157-162`).

Workspace gates, run from the repo root:

```bash
cargo test --workspace --locked
TMPDIR=/private/tmp scripts/cli-acceptance.sh
```

`cargo test --workspace --locked` is CI's hard gate; `scripts/cli-acceptance.sh` is the CLI
acceptance surface. Both are run, not authored. The runbook adds no `test/`, `__tests__/`,
`spec/` or `#[cfg(test)]` file, and no browser witness or deployment is claimed here.

## What this does not guarantee

- No protection against a stolen session cookie already in hand. `hive_jwt` is an httpOnly cookie
  whose lifetime is the minted `expires_in` less 30 s (`ui/app/api/token/route.ts:24-25`); a
  bearer holding it is a bearer, and step-up only re-checks on protected mutations.
- No revocation of pre-enrollment tokens. Tokens already issued stay valid until their own `exp`,
  at most 3600 s (`crates/hive-cloud/src/admin.rs:41`, `crates/hive-cloud/src/auth.rs:36-38`);
  disable writes a tombstone, not a token blacklist (`crates/hive-cloud/src/totp.rs:477-496`).
- No phishing resistance. A TOTP code is a short-lived shared secret a user can be tricked into
  relaying; RFC 6238 offers no origin binding (`crates/hive-cloud/src/totp.rs:41-44`).
- No freshness guarantee on a node that has not merged the row yet. Adoption is a per-row merge on
  the store-sync interval, so a just-enrolled account is treated as unenrolled on a lagging node
  until that row arrives (`crates/hive-cloud/src/store_sync.rs:512-524`,
  `crates/hive-cloud/src/store_follower.rs:175-182`, `crates/hive-cloud/src/totp.rs:143-187`).

See also: `docs/README.md`, `docs/contract-verification.md` (runbook format),
`docs/smart-contracts.md`.
