# DevHub L0 capacity-allocation contract

Marketplace calls DevHub through the five `/v1/marketplace/*` endpoints using
an HMAC key configured by the operator. DevHub treats Marketplace as a
server-to-server client, never as a mesh peer or a source of provider
recipients, node bindings, settlement configuration, or buyer transactions.

## Required configuration

```text
HIVE_MARKETPLACE_HMAC_KEYS=<key-id>:<secret>[,<key-id>:<secret>...]
HIVE_MARKETPLACE_PROVIDER_RECIPIENTS=<provider-id>=<0x-address>[,...]
HIVE_MARKETPLACE_ATOMIC_SPLIT_AUDITED=1
HIVE_MARKETPLACE_ATOMIC_SPLIT_CONTRACT=<0x-address>
HIVE_MARKETPLACE_FEE_RECIPIENT=<0x-address>
HIVE_MARKETPLACE_SETTLEMENT_CONFIGURATION_REFERENCE=base-theo-atomic-split-v1
THEO_RPC_URL=https://...
```

`HIVE_MARKETPLACE_ATOMIC_SPLIT_AUDITED` is a deliberate mainnet approval
gate. Do not set it, or point any of these values at a testnet contract, until
the corresponding chain/configuration version has been reviewed. This rollout
accepts only Base (8453), THEO
`0xebE516a20238F79DC20b07eaD6768e08891Ed309`, 18 decimals, a 500-bps fee,
two confirmations, and the named configuration reference.

Each request signs the canonical UTF-8 string:

```text
METHOD\nPATH\nTIMESTAMP\nNONCE\nSHA256_HEX(BODY)
```

with `HMAC-SHA256`. It sends the key ID, timestamp, nonce, body digest, and
signature in `X-Marketplace-Key-Id`, `X-Marketplace-Timestamp`,
`X-Marketplace-Nonce`, `X-Marketplace-Content-SHA256`, and
`X-Marketplace-Signature`. Writes additionally require `Idempotency-Key`.
Nonces are durable and single-use; secrets, headers, and bodies must not be
logged.

## Settlement and allocation

Discovery records are short-lived (60 seconds), contain only the public safe
schema, and derive a provider recipient solely from the operator-owned
provider registry. Payment intents snapshot the selected deployment binding,
buyer address, Base/THEO settlement configuration, and exact split:
`fee = floor(gross / 20)` and `provider = gross - fee`. Amounts remain decimal
atomic integers through the full EVM `uint256` range.

DevHub does not sign, construct, relay, or broadcast a buyer transaction.
Verification independently reads the Base receipt and accepts only a successful
receipt with sufficient confirmations and a `Settlement` log emitted by the
configured atomic-split contract. The log's signature and every immutable
intent field are matched before an allocation can be submitted.

Immediately before scheduling, DevHub derives the listing anew from its live
registry and rejects expired, revoked/unhealthy, capacity-exhausted, or
provider/node-mismatched placements. The scheduler receives a one-node
allowlist; it never substitutes another node.
