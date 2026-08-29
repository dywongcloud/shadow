# DevHub Marketplace placement-policy consumer

Marketplace-backed Git deployments enter through `POST /api/marketplace/deploy`.
It is a server-only Next.js route: it reads the verified Clerk session, obtains
the `autheo-marketplace-v1` Clerk JWT template, and calls:

```text
GET {NEXT_PUBLIC_MARKETPLACE_URL}/v1/marketplace/orders/{marketplace_order_id}/placement-policy
Authorization: Bearer <Clerk JWT>
```

The Marketplace request never receives DevHub's `hive_jwt`. Browser input may
name an order and ordinary Git build inputs only; it cannot supply a tenant,
buyer, provider, role, policy, authorization header, or Marketplace token.

The buyer tenant is derived from the same Clerk session:

- active organization: `clerk:org:<org_id>`
- otherwise: `clerk:user:<user_id>`

DevHub must reject a placement policy when `buyer_tenant_id` does not exactly match the tenant derived from its Clerk-authenticated request.

## Policy contract and failures

The current accepted contract is `contract_version: 1` with a positive
`policy_version`. A policy must contain exactly the v1 fields:
`marketplace_order_id`, `buyer_tenant_id`, `status`, `revocation_state`,
`valid_from`, `valid_until`, `approved_node_ids`, `provider_id`, `listing_id`,
`region`, `resources`, and `commercial`.

`status` must be `active`, `revocation_state` must be `not_revoked`, and the
current time must be within the inclusive-start/exclusive-end validity window.
Node IDs must be non-empty, unique registry identifiers. Resources require
non-negative `vcpu`, `memory_mb`, and `disk_gb`; commercial terms require an
uppercase ISO-4217 `currency` and non-negative integer `price_cents`.

Unknown versions, malformed fields, private-address/credential/claim/control
metadata, order mismatch, tenant mismatch, inactive/suspended/revoked policy,
and expired policy are terminal rejections. Only Marketplace network and `5xx`
responses are retried, using bounded exponential backoff.

## Immutable deployment input

Before Hive begins the deployment pipeline, DevHub sends a canonical
`MarketplacePlacementSnapshot` containing the complete validated policy JSON,
retrieval time, requested order ID, derived buyer tenant, and approved node
IDs. Hive carries that snapshot through fanout and stores it in the deployment
record; it is never returned by deployment-list APIs and later stages do not
refetch Marketplace or accept a replacement.

Hive treats `approved_node_ids` as a hard allowlist against its authoritative
live node registry. Each candidate must still pass current health,
reachability, isolation/capability, and capacity checks. If no approved node
passes, the deployment fails with `MARKETPLACE_PLACEMENT_UNAVAILABLE`; Hive
does not fall back to an unapproved local node.

## DevHub UI and local verification

Developers start a Marketplace deployment from **New Project → Deploy from
Marketplace** or from a project's **Marketplace** action. The form accepts
only an order ID and ordinary build inputs: HTTPS repository URL, branch,
project name, repository root, production/preview target, build-cache choice,
and environment variables. It has no inputs for tenant or buyer identity,
providers, roles, JWTs, placement policy JSON, or approved nodes.

On submit, DevHub calls only `POST /api/marketplace/deploy`. That server route
obtains the `autheo-marketplace-v1` token from the real Clerk session, derives
the tenant, retrieves the policy server-side, validates it, and forwards the
immutable snapshot to Hive. It never forwards `hive_jwt` to Marketplace and
never returns the Clerk token, policy snapshot, private node data, credentials,
or control-plane metadata to the browser.

The UI reports policy authorization, missing policy, tenant mismatch, expired,
revoked, suspended, malformed, unsupported-version, and no-eligible-node
failures in actionable language. A successfully accepted request joins the
ordinary build-status page and deployment polling flow.

### Minimal local workflow

1. In one terminal, start Hive:

   ```bash
   cargo run -p hive-cloud -- --admin 127.0.0.1:8786 --listen 127.0.0.1:8787
   ```
2. Create/use a real Clerk development instance. Configure DevHub's
   `NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY` and `CLERK_SECRET_KEY`, then create a
   JWT template named exactly `autheo-marketplace-v1`.
3. In a second terminal, start the isolated local Marketplace fixture:

   ```bash
   cd ui && npm run dev:marketplace
   ```

   It listens on `http://127.0.0.1:4010`, exposes
   `GET /v1/marketplace/orders/{order_id}/placement-policy` and validates the
   Clerk token's issuer, audience, signature, and expiry using Clerk JWKS.
4. Copy `ui/.env.example` to `ui/.env.local`, set
   `NEXT_PUBLIC_MARKETPLACE_URL=http://localhost:4010` for the isolated fixture
   (the normal local Marketplace default is `http://localhost:3000`), then set the non-secret URLs and your
   Clerk keys, then in a third terminal run:

   ```bash
   cd ui && npm run dev
   ```

5. Sign in through Clerk, open the Marketplace deployment form, and submit an
   order whose policy contains the v1 fields described above.

The local `HIVE_DEV_MINT`/DevHub dev-mint behavior is not Clerk and does not
exercise Marketplace authentication. Do not use static bearer tokens,
unsigned tokens, fake Clerk claims, M2M credentials, or a Marketplace auth
bypass for this flow.

For manual verification, the fixture accepts `fixture-valid` and the
selectable order IDs `fixture-unauthorized`, `fixture-missing`,
`fixture-tenant-mismatch`, `fixture-expired`, `fixture-revoked`,
`fixture-suspended`, `fixture-malformed`, `fixture-unsupported`, and
`fixture-no-eligible-nodes`. It validates authentication before selecting any
case. This fixture is intentionally isolated and has no static bearer token,
M2M credential, fake Clerk claim, unsigned-token path, expired-token path, or
auth bypass.
