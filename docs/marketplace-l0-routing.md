# Marketplace L0 node advertisements and allocations

Marketplace is a commercial/control-plane client of DevHub (`hive-cloud`), not
an L0 mesh peer. It cannot receive or control node private keys, peer objects,
relay credentials, trunks, tenant workloads, or `hive_jwt` tokens. Settlement
may be on-chain; routing is always off-chain and enforced by DevHub.

## Trust boundary and configuration

Every Marketplace call uses an HMAC request tag in `x-marketplace-key`, computed
as lowercase hex `HMAC-SHA256(HIVE_MARKETPLACE_API_KEY,
"hive-marketplace-v1")`. This credential is distinct from DevHub browser/API
authentication. DevHub never forwards a Marketplace bearer or `hive_jwt`
between systems.

The backend is the source of truth for live node health, connectedness,
isolation backend, runtime capability, GPU, disk, and placement. Required
backend configuration is:

```text
HIVE_MARKETPLACE_API_KEY
HIVE_MARKETPLACE_ADVERTISEMENT_SECRET
THEO_CHAIN_ID
THEO_TOKEN_ADDRESS
THEO_TREASURY_ADDRESS
THEO_TOKEN_DECIMALS
THEO_REQUIRED_CONFIRMATIONS
HIVE_MARKETPLACE_SETTLEMENT_VERIFY_URL
HIVE_MARKETPLACE_SETTLEMENT_API_KEY
```

`HIVE_MARKETPLACE_PRICING_TERMS` is optional metadata. The settlement verifier
is an existing configured authority; it must return a server-verified result
and must not accept browser transaction data. It is the source of truth for
contract/order state. Chain, token contract, treasury, decimals, and
confirmation policy reuse the existing DevHub THEO billing configuration above.

## API

All endpoints are server-to-server and require `x-marketplace-key`.

### `GET /v1/marketplace/nodes`

Returns only live healthy connected nodes which pass DevHub's production
isolation and disk/memory eligibility checks. No address, peer, relay, key, or
operator fields are returned.

```json
{
  "advertisement_id":"adv_...",
  "issued_at_ms":0,
  "expires_at_ms":0,
  "attestation":"base64url(payload).base64url(hmac)",
  "settlement":{"chain_id":"...","token_contract":"...","treasury":"...","decimals":18,"required_confirmations":12},
  "nodes":[{"node_id":"stable-registry-id","region":"...","capabilities":{"backend":"firecracker","cpu_cores":8,"memory_mb":32768,"gpu_count":0,"gpu_model":null,"gpu_vram_mb":0},"available_capacity":{"disk_free_gb":100,"gpu_free_mb":null},"supported_runtimes":["wasmer"],"pricing":{"currency":"THEO","terms_reference":""}}]
}
```

Advertisements expire after 60 seconds. An allocation must present the exact
signed attestation, cannot approve a node absent from it, and may not outlive
the attestation.

### `POST /v1/marketplace/allocations`

```json
{
  "marketplace_order_id":"order_123",
  "tenant_id":"clerk:org:org_123",
  "resources":{"vcpu":2,"memory_mb":4096,"disk_gb":20,"gpu":false,"runtime":"wasmer","region":"virginia"},
  "approved_node_ids":["stable-registry-id"],
  "theo_amount":"1250000000000000000",
  "expires_at_ms":1730000000000,
  "contract_reference":"0x...",
  "advertisement_id":"adv_...",
  "advertisement_attestation":"..."
}
```

The service validates expiry, uniqueness, and advertised-node membership, then
calls the configured settlement verifier. The verifier must return
`confirmed: true`, matching order, contract reference, amount, chain, token,
treasury, decimals, and at least the configured confirmation count. Repeating
the same order with the same tenant and contract is idempotent; a conflicting
binding is rejected.

### `POST /v1/marketplace/allocations/{order_id}/route`

```json
{"deployment": {"repo_url":"https://github.com/acme/app.git","project":"app"}}
```

DevHub validates the allocation and settlement again, rejects expired/revoked
allocations, and checks that at least one approved node remains eligible. It
then injects an immutable `MarketplacePlacementSnapshot` itself and invokes the
existing deployment scheduler. The scheduler retains the approved-node
allowlist and dispatches remote work with the existing gossip/PeerPool path.
`no_fanout`, fanout-secondary, and project-incarnation values in the submitted
deployment are overwritten server-side and cannot bypass placement. There is no
unapproved fallback. Existing region, health, GPU, runtime, disk,
capacity and isolation filters remain authoritative.

### `POST /v1/marketplace/allocations/{order_id}/fulfill`

This has no request body. It re-verifies configured settlement state before
marking an already-routed allocation fulfilled. It never trusts client
transaction data.

`GET /v1/marketplace/allocations` and
`GET /v1/marketplace/allocations/{order_id}` provide operator/service
visibility. All advertisement issuance, allocation accept/reject, routing, and
fulfillment transitions are append-only DevHub audit records.

DevHub operators can inspect the same sanitized eligible-node view and all
accepted allocation records at `GET /v1/admin/marketplace`; it is protected by
the normal DevHub platform-operator authorization and does not require or
reveal the Marketplace service credential.

## Operational behavior

Settlement verifier network failures are `502`; missing required configuration
is `503`; pending, failed, mismatched, expired, revoked, or stale
advertisements return a conflict and do not route. Allocation records replicate
from the DevHub control-plane leader to followers for safe round-robin reads.
An explicit revocation feed is still required in production before a Marketplace
order can be revoked before its expiry; this initial API intentionally fails
closed on expiry and settlement state but does not invent a custody, price-feed,
or chain-indexing subsystem.
