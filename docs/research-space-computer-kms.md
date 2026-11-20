# Space Computer / Orbitport KMS as non-custodial storage for sensitive tenant data

Research memo, 2026-11-20. Basis: the two named guides plus the vendor's complete
published docs text (`https://docs.spacecomputer.io/llms-full.txt`, 133,418 chars),
the concepts pages, the landing page, and this repository's own secret-handling code.
Research only: no production change, no push, no roll.

## Verdict

**No — not today, and not in the sense the question needs.** Three independent
blockers, each sufficient on its own:

1. **Custodial by construction.** Keys are created inside the provider's backend by
   remote JSON-RPC (`kms.createKey`); "Neither method exposes private key material."
   Every encrypt, decrypt, sign, wrap and rotate is a provider-side RPC. The tenant
   holds a bearer token and key ids (`kms:<alias>`), not key material. Tenant
   isolation is "enforced by the backend key-store policy" and, for PATs, "the
   gateway obtains the authoritative tenant from the account service" — an
   authorization boundary, not a cryptographic one. There is no documented import,
   private-key export, key deletion, HSM identity, backup or recovery path.
   `generateDataKey` returns a plaintext DEK locally, but the wrapping master key
   stays with the provider, so even envelope encryption has a custodial root.

2. **The vendor forbids this use.** The KMS guide: "Do **not** use it to protect
   production data, sign production transactions, or hold keys you cannot afford to
   lose. Use it for prototypes, internal tooling, and exploration only -- and assume
   any keys you create today may be rotated, invalidated, or deleted before general
   availability." And: "The Orbitport KMS is **experimental**. Interfaces, key
   formats, and on-the-wire shapes can change without notice, and operational
   guarantees (durability, availability, key recoverability) are **not** in place
   yet." The landing page labels it "Experimental key management."

3. **The orbital guarantees are roadmap.** "The current KMS deployment and the
   orbital roadmap are distinct." / "the KMS you can call today does not run on a
   satellite yet." / "These roadmap capabilities should not be assumed to be part of
   a current KMS deployment." / "SpaceTEE is part of SpaceComputer's roadmap. It is
   not yet a live, generally available service." The trust ramp is Phase 1
   (attested terrestrial TEEs), Phase 2 (threshold crypto: "No single participant,
   SpaceComputer included, holds enough key material to sign or decrypt
   unilaterally"), Phase 3 (SpaceKMS, keys born in orbit). Phase 2 is the one that
   would change the custody answer, and it is not shipped.

## Terminology

"Non-custodial" in key management means the data owner holds the key material, the
operator cannot decrypt or sign unilaterally, and the owner can leave with the keys.
It is a statement about who can act, not how well the hardware is guarded. Space
Computer argues the *custody ladder* (file on disk -> TPM -> HSM -> secure element
-> secure element plus TEE) and physical inaccessibility: "Orbital hardware security
modules on satellite nodes provide leak-proof key custody", "Secrets are physically
unreachable and immune to side-channel extraction". That describes a stronger
custodian, not the absence of one.

Across the complete published docs text, `non-custodial`, `self-custody`, `BYOK`
and `customer-managed` each appear **0** times. The one use of "trustless" concerns
space-debris resilience, not custody. The docs never make the claim.

The credential class described by the authentication guide is itself an
illustration: a customer gets a pre-issued bearer token or PAT, not a private key.
Possession of a bearer token is what custodial delegation looks like. The supplied
blob was not decoded; see Handling notes. Per the authentication guide,
"A local `valid: true` is not a signature, permission, or revocation check"; the
gateway validates and the account service enforces access. There is no
client-held private key, no OAuth exchange, no DPoP/mTLS binding.

## Attestation is a claim, not a caller-verifiable proof

The KMS guide links to "the reasoning behind a KMS that runs on attested (and
**eventually** orbital) hardware". No attestation quote, measurement, verified-boot
chain, enclave identity or seal/unseal semantics is exposed by the API.
`getCapabilities` lists schemes (`TRANSIT`, `ETHEREUM`); it does not prove which
hardware handled a given ciphertext. Every reply is a `ServiceResult` whose
`metadata.timestamp` is a **client-side finish time**, not a signed hardware
assertion. The guide says: "Backend implementation does not establish that a
feature is enabled on a particular deployment."

Space Fabric describes the intended design in detail: dual secure elements (NXP
SE050 and Tropic Square TROPIC01) co-signing every attestation, SEAP / Proof of
Execution Triangulation, a Certificate of Authorization once enough geographically
distributed ground stations endorse a challenge. That is a serious design. It is
written about hardware that is not the service callable today. The telemetry
tutorial says of its own signature: "It does not validate the sensors, establish
freshness, or prevent someone replaying a previously signed report."

## Marketing outruns the how-to

The landing page uses present tense for the whole stack: "A tamper-proof,
satellite-based platform delivering trusted execution, cryptographic services, and
confidential compute from orbit", "Physically isolated from terrestrial threats,
resilient to side-channel attacks, and resistant to state-level adversaries",
"Compute over encrypted data with forward secrecy guarantees". The how-to describes
a terrestrial experimental beta with an explicit do-not-use-for-production warning.
An evaluation built on the headline would materially overestimate the guarantee.

## Fit against this platform's invariants

`crates/hive-cloud/src/secrets.rs` opens with "Key management (self-hosted, no
external KMS)": ChaCha20-Poly1305 AEAD under a fleet-shared `HIVE_SECRET_KEY`,
values sealed as `enc:v1:` before touching disk or the replicated snapshot. Key
resolution fails closed (a malformed env key, unreadable `secret.key`, or a key that
cannot be persisted exits the process); `HIVE_SECRET_KEY_OLD` keeps old ciphertexts
readable across rotation; `try_decrypt` is the honest `Option` while `decrypt` is a
passthrough. `scripts/audit-secret-keys.sh` is the pre-roll check.

Moving that to a remote KMS would invert several load-bearing rules:

| Existing rule | What a remote KMS introduces |
| --- | --- |
| Boot fails closed on local key failure | Boot depends on a third-party gateway, a live bearer token and available credits. `INSUFFICIENT_CREDITS` (402) and `ACCOUNT_UNAVAILABLE` (503) become boot failures; the SDK "does not acquire, refresh, or persist a configured token". |
| The serving runtime never stalls | A request-path decrypt becomes an RPC with a 30,000 ms per-attempt timeout, no retry by default, and non-idempotent `createKey` / `sign`. |
| Availability is not intermittent | LEO passes are 5-15 minute windows with hours between them; the docs say applications "should be designed to tolerate periods where the enclave is unreachable." |
| A credit balance does not decide data access | A credit hold precedes every call, including capability discovery; "Settlement and release are best-effort." |
| Access-deciding tokens use `try_decrypt`, never passthrough `decrypt` | Another long-lived bearer credential to seal, rotate and mask; `put_env` already force-masks credential-shaped values via `looks_like_secret`. |
| Capability is PROBED and ADVERTISED, never assumed | An attestation claim with no caller-verifiable quote bound to the result. |
| Technical evidence is never an attestation or a compliance claim | `docs/soc2-evidence-report.md` prints that boundary on every report; `docs/compliance-controls.md` records AT-1/AT-2 as UNMET. A vendor quote closes neither. |

Structural limit: this platform injects plaintext secrets into tenant processes
after `env_clear()`. Any design where the platform must hand a process usable
plaintext has custody at that instant. Full non-custody is achievable for data the
platform only stores (tenant-side encryption with a tenant-held KEK), not for
runtime-injected credentials. Adding a third-party KMS to that path adds a
custodian without removing one. Any integration would also need `internal_client`
(no redirect) for the fixed `https://op.spacecomputer.io`, and an optional TS field
with an exact fallback, since dashboard and backend deploy independently.

## What would change this verdict

All six, not a subset:

1. Phase 2 threshold cryptography ships **with the tenant as one of the independent
   share holders**, not just multiple parties chosen by the operator.
2. Caller-verifiable attestation per operation: a signed measurement bound to the
   specific RPC result, with a published verifier and pinned hardware identities.
3. GA durability, availability and key recoverability; a frozen wire format; no
   unilateral key rotation, invalidation or deletion.
4. An exit path: key import, export or verifiable destruction, plus documented
   backup and recovery.
5. Credits off the decrypt path; billing state must not decide whether tenant data
   is readable.
6. Independent audit of the deployed system, not a design description — still
   bounded by the rule that code supplies evidence, never an attestation or a
   compliance opinion.

## Alternatives that fit today

Keep the self-hosted AEAD and derive per-tenant keys from `HIVE_SECRET_KEY` with
tenant and project as context, preserving `try_decrypt` and the
`HIVE_SECRET_KEY_OLD` rotation path. That removes the third party entirely. For
data the platform only stores, use tenant-side envelope encryption with a
tenant-held wrapping key and store ciphertext only. Both remain custodial to the
operator wherever plaintext must be injected — the honest ceiling for a PaaS — but
neither adds a new custodian, a credit meter, or an intermittent satellite link.

## Handling notes

- The base64 account credential supplied with the request was **not decoded,
  printed, stored or transmitted**, and no authenticated call was made to
  `op.spacecomputer.io`. Because it now exists in a conversation transcript, treat
  it as exposed: revoke or rotate it at `accounts.spacecomputer.io`. Local clearing
  is insufficient — the authentication guide says `clearToken()` "does not clear a
  configured `accessToken`, request a new token, or revoke a token on the server".
- Two multi-agent adversarial passes were launched to try to refute this verdict.
  Both stalled on a saturated local gm store lane and returned no findings; they
  were stopped, not counted as corroboration. Everything here rests on direct
  reading of the primary sources plus this repository's `secrets.rs`,
  `project_settings.rs`, `docs/compliance-controls.md` and
  `docs/soc2-evidence-report.md`.
- No code was changed, nothing was committed by this research pass, and no roll was
  performed.

## Sources

- https://docs.spacecomputer.io/docs/how-to/kms
- https://docs.spacecomputer.io/docs/how-to/authentication
- https://docs.spacecomputer.io/docs
- https://docs.spacecomputer.io/docs/concepts
- https://docs.spacecomputer.io/docs/concepts/orbital-kms
- https://docs.spacecomputer.io/docs/concepts/space-fabric
- https://docs.spacecomputer.io/docs/concepts/orbitport-architecture
- https://docs.spacecomputer.io/docs/concepts/spacetee
- https://docs.spacecomputer.io/llms-full.txt
- https://spacecomputer.io/
