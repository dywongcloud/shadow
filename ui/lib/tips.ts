/* Public tip address. One EVM address is used on every network listed;
 * there is no per-chain address and no secret here. The address is a
 * display constant: never interpolate it into a URL, HTML string or Location. */
export const TIPS_ADDRESS = "0x7874258AcBcE8ADF90693B2ad6b0A9481A0AB1CD";

/** Networks the operator accepts tips on, in the order they were given. */
export const TIPS_NETWORKS = [
  "Ethereum",
  "Optimism",
  "Base",
  "Arbitrum",
  "Polygon",
  "Avalanche",
  "Monad",
  "ZKsync",
] as const;
