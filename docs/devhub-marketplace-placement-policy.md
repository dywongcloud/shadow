# DevHub Marketplace placement-policy consumer

Marketplace-backed Git deployments enter through `POST /api/marketplace/deploy`.
It is a server-only Next.js route: it reads the verified Clerk session, obtains
the `autheo-marketplace-v1` Clerk JWT template, and calls:

```text
GET {MARKETPLACE_URL}/v1/marketplace/orders/{marketplace_order_id}/placement-policy
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
