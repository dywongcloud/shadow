# Marketplace settlement profiles

Marketplace settlement has two deliberately separate profiles:

- Base Mainnet is the default production profile. It remains chain ID `8453`
  (`0x2105`) and uses THEO at
  `0xebE516a20238F79DC20b07eaD6768e08891Ed309`. Its existing
  `HIVE_MARKETPLACE_ATOMIC_SPLIT_AUDITED=1` gate and
  `base-theo-atomic-split-v1` configuration reference are unchanged.
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
HIVE_MARKETPLACE_TESTNET_ATOMIC_SPLIT_CONTRACT=0x...      # audited AtomicSplit deployment on chain 785
HIVE_MARKETPLACE_TESTNET_FEE_RECIPIENT=0x...
HIVE_MARKETPLACE_TESTNET_ATOMIC_SPLIT_AUDITED=1
HIVE_MARKETPLACE_TESTNET_CONFIGURATION_REFERENCE=<approved testnet configuration reference>
```

All three addresses must be non-zero EVM addresses. Before this profile is
published or an intent is created, the service proves that the fixed RPC is
chain 785, that the configured THEO and AtomicSplit addresses contain code,
and that THEO's ERC-20 `symbol()` is exactly `THEO`. Any failure leaves
settlement unavailable. Every intent snapshots the complete profile, including
network, chain ID, RPC, explorer, contracts, event signature, confirmation
policy, fee, and configuration reference.

Receipt verification uses that snapshot. It requires the receipt's chain ID,
AtomicSplit contract, `Settlement` event signature, THEO token, buyer, gross
amount, provider amount, fee recipient, fee amount, order reference, and
500-bps value to all match. It waits for two included blocks before accepting
the settlement.

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
   the configured Autheo Testnet AtomicSplit contract. It must emit
   `Settlement(bytes32,address,address,uint256,address,uint256,address,uint256,uint256)`
   with every field equal to the intent snapshot.
5. Submit the resulting transaction hash to the authenticated verification
   endpoint. It may return `awaiting_confirmations` until two blocks include
   it; retrying the same hash must become `verified`.
6. Submit the allocation with the verified intent. Confirm scheduling remains
   bound to the one advertised canonical node.

The normal HMAC authentication, nonce replay protection, idempotency records,
strict listing DTO projection, and one-node allocation binding apply
unchanged to both profiles.
