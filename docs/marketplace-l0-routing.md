# Marketplace and DevHub placement boundary

Marketplace is a commercial/control-plane client of DevHub (`hive-cloud`),
not an L0 mesh peer. It never receives node private keys, peer objects, relay
credentials, trunks, tenant workloads, `hive_jwt` tokens, or private topology.

## Approved API contract

DevHub calls Marketplace for placement through exactly one endpoint:

```text
GET /v1/marketplace/orders/{marketplace_order_id}/placement-policy
Authorization: Bearer <Clerk JWT from autheo-marketplace-v1>
```

The call is made only by DevHub's server-side deployment route. It mints the
Clerk JWT from a verified session with the `autheo-marketplace-v1` template;
it never sends `hive_jwt`, `HIVE_INTERNAL_TOKEN`, a custom DevHub/Hive M2M
header, or any browser-provided authorization value to Marketplace.

Marketplace calls DevHub only through these private service-to-service routes:

* `GET /v1/marketplace/l0/deployments`
* `POST /v1/marketplace/payment-intents`
* `POST /v1/marketplace/payments/verify`
* `POST /v1/marketplace/l0/allocations`

Those routes remain behind the existing timestamped five-header HMAC gate.
The verifier signs the exact raw request body using the canonical method,
path, timestamp, nonce, and SHA-256 digest; it preserves durable nonce replay
protection, idempotency behavior, and existing error semantics. JWT and
authentication bypasses are not accepted on this router.

## Placement-policy handling

Before Hive/Rust deployment begins, DevHub:

1. derives the buyer tenant from the verified Clerk session;
2. fetches and strictly validates the Marketplace policy;
3. rejects tenant/order mismatches, unknown contract or policy versions,
   inactive, expired, or revoked policies, malformed fields, and sensitive or
   topology-bearing fields;
4. snapshots the validated policy and its approved node IDs immutably into the
   deployment request; and
5. requires Hive to validate that the approved IDs still name healthy,
   reachable, capable nodes with sufficient live capacity.

Marketplace policy data is untrusted until those checks complete. Hive never
falls back to an unapproved node and does not refetch or replace the snapshot
during later build or fanout stages. Browser clients receive neither the Clerk
JWT nor the policy snapshot, node addresses, credentials, control-plane
metadata, or private infrastructure details.

`GET /v1/admin/marketplace` is a separate, operator-authorized Hive Admin
diagnostic route. It is not a Marketplace integration endpoint and remains on
the private/loopback Admin listener; port 8786, Swagger, and Marketplace
private routes must not be exposed through public ingress.

## Intentionally deferred work

Marketplace allocation status/read APIs, callbacks, and DevHub/Hive usage
ingestion are not part of the current contract. `POST /usage-records` is
buyer-authenticated, not a DevHub/Hive ingestion endpoint.

If allocation callbacks, usage ingestion, or node-verifier completion are
needed later, they require a separately reviewed Marketplace contract defining
authentication, tenant binding, idempotency and replay handling, payload
sanitization, error semantics, and operational ownership. Do not add
speculative endpoints.
