# Marketplace settlement profiles

Marketplace settlement has two deliberately separate profiles:

- Base Mainnet's legacy AtomicSplit profile is historical-only. Existing
  snapshots continue to decode and verify with its legacy event ABI, but it
  cannot issue a new payment intent.
- Autheo Testnet is available only when
  `HIVE_MARKETPLACE_SETTLEMENT_PROFILE=autheo-testnet-v1`. It is never
  selected by an RPC URL, chain ID, or any generic environment variable.

The split prevents test configuration from repointing a production process.
Testnet settings are ignored unless the exact test-only profile name is set;
they never alter Base Mainnet's token, chain, RPC, explorer, fee, or audited
contract gate.

## Autheo Testnet v1

The test profile is fixed to:

| Field | Value |
| --- | --- |
| Network | Autheo Testnet |
| Chain ID | `785` (`0x311`) |
| Currency | THEO |
| RPC | `https://testnet-rpc1.autheo.com` |
| Explorer | `https://testnet-explorer.autheo.com` |
| Fee | `500` bps |
| Confirmations | 2 included blocks |

Enable it only with all of these values:

```text
HIVE_MARKETPLACE_SETTLEMENT_PROFILE=autheo-testnet-v1
HIVE_MARKETPLACE_TESTNET_THEO_TOKEN=0x...                 # deployed THEO ERC-20 on chain 785
HIVE_MARKETPLACE_TESTNET_ATOMIC_SPLIT_CONTRACT=0x...      # approved MarketplaceAtomicFeeSplit V2 deployment
HIVE_MARKETPLACE_TESTNET_FEE_RECIPIENT=0x...
HIVE_MARKETPLACE_TESTNET_ATOMIC_SPLIT_AUDITED=1
HIVE_MARKETPLACE_TESTNET_CONFIGURATION_REFERENCE=<approved V2 configuration reference>
```

All three addresses must be non-zero EVM addresses. Before this profile is
published or an intent is created, the service proves that the fixed RPC is
chain 785, that the configured THEO and AtomicSplit addresses contain code,
and that THEO's ERC-20 `symbol()` is exactly `THEO`. Any failure leaves
settlement unavailable. The explicit audited flag, V2 contract version,
active status, and non-empty configuration reference are all required to
create an intent; contract existence never implies approval. Every intent
snapshots the complete profile, including network, chain ID, RPC identity,
explorer, contracts, event signature, confirmation policy, fee,
configuration reference, version/status, buyer wallet, order reference, and
provider/deployment identity.

## MarketplaceAtomicFeeSplit V2 receipt semantics

V2 emits:

```solidity
Settlement(
  bytes32 settlementKey, bytes32 orderReference, address payer, address token,
  address providerRecipient, uint256 grossAmount, uint256 providerAmount,
  address feeRecipient, uint256 feeAmount, uint256 feeBps
)
```

The topic is derived at runtime from
`keccak256(bytes("Settlement(bytes32,bytes32,address,address,address,uint256,uint256,address,uint256,uint256)"))`;
DevHub never relies on a copied topic hash.

DevHub derives the expected cryptographic replay identity using Solidity
`abi.encode` (not packed encoding):

```text
keccak256(abi.encode(
  orderReference, token, payer, providerRecipient, feeRecipient, grossAmount,
  chainId, settlementContract
))
```

The following deterministic Solidity ABI vector is retained as an integration
cross-check (each address byte is repeated for readability):

| Input | Value |
| --- | --- |
| orderReference | `0x1111111111111111111111111111111111111111111111111111111111111111` |
| token | `0x2222222222222222222222222222222222222222` |
| payer | `0x3333333333333333333333333333333333333333` |
| providerRecipient | `0x4444444444444444444444444444444444444444` |
| feeRecipient | `0x5555555555555555555555555555555555555555` |
| grossAmount | `123456789012345678901234567890` |
| chainId | `785` |
| settlementContract | `0x6666666666666666666666666666666666666666` |
| settlementKey | `0xd554bf2b8901281e557b84a1daa7b1fa690f74d0168be4ea76eecf8a3ad4b6f5` |

Receipt verification uses only the immutable intent snapshot. It selects logs
by exact contract and topic, then verifies the settlement key, order reference,
payer, token, provider recipient, gross/provider/fee amounts, fee recipient,
and 500-bps policy. It recomputes `feeAmount = floor(grossAmount * 500 / 10000)`
and `providerAmount = grossAmount - feeAmount`, requires a successful receipt,
and waits for two included blocks. A payment is replay-identified by
settlement key plus its bound transaction hash and log index; order reference
remains a business identifier only. Repeating verification of the same bound,
valid transaction is idempotent; a different transaction hash is rejected.

Historical V1 intents retain their captured contract/snapshot and legacy ABI
for read/receipt verification only. New intents never fall back to V1.

## Settlement lifecycle

```text
Marketplace payment intent
  -> immutable V2 settlement snapshot
  -> buyer submits MarketplaceAtomicFeeSplit V2 transaction
  -> DevHub reads successful receipt
  -> exact Settlement event decoded
  -> settlementKey independently derived and compared
  -> all immutable fields and 500-bps arithmetic verified
  -> two confirmations
  -> payment = verified
  -> allocation submission enabled
```

## Manual end-to-end test

1. In an isolated non-production process, set the six variables above and the
   ordinary Marketplace HMAC key configuration. Do not alter any Base Mainnet
   variables.
2. Request the authenticated Marketplace settlement configuration and confirm
   it reports `autheo-testnet-v1`, chain `785`, `0x311`, the fixed Autheo RPC
   and explorer, and `500` bps.
3. Request an authenticated payment intent for a currently advertised
   deployment. Save its exact settlement snapshot, order reference, buyer,
   gross amount, provider amount, fee amount, and recipient addresses.
4. From the intent's buyer address, submit a buyer-authorized transaction to
   the configured V2 contract. It must emit the V2 `Settlement` event with
   settlement key and every other field equal to the immutable intent snapshot.
5. Submit the resulting transaction hash to the authenticated verification
   endpoint. It may return `awaiting_confirmations` until two blocks include
   it; retrying the same hash must become `verified`.
6. Submit the allocation with the verified intent. Confirm scheduling remains
   bound to the one advertised canonical node.

The normal HMAC authentication, nonce replay protection, idempotency records,
strict listing DTO projection, and one-node allocation binding apply
unchanged to both profiles.
