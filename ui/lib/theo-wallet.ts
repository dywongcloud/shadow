"use client";

export type Eip1193Provider = {
  request(args: { method: string; params?: unknown[] }): Promise<unknown>;
  on?(event: "accountsChanged" | "chainChanged" | "disconnect", listener: (...args: unknown[]) => void): void;
  removeListener?(event: "accountsChanged" | "chainChanged" | "disconnect", listener: (...args: unknown[]) => void): void;
  isMetaMask?: boolean;
  isCoinbaseWallet?: boolean;
  isKeplr?: boolean;
  providers?: Eip1193Provider[];
};

declare global {
  interface Window {
    ethereum?: Eip1193Provider;
    keplr?: { ethereum?: Eip1193Provider };
  }
}

export type TheoNetwork = {
  chain_id: number;
  chain_name: string;
  rpc_url: string;
  explorer_url: string;
  token_address: string;
  treasury_address: string;
  token_decimals: number;
  required_confirmations: number;
};

export type WalletProviderId = "metamask" | "coinbase" | "keplr";

export const walletProviderNames: Record<WalletProviderId, string> = {
  metamask: "MetaMask",
  coinbase: "Coinbase Wallet",
  keplr: "Keplr",
};

function ethereumProviders(): Eip1193Provider[] {
  if (typeof window === "undefined") return [];
  const root = window.ethereum;
  if (!root) return [];
  return root.providers?.length ? root.providers : [root];
}

/** Resolves an injected provider only at the point of use; provider objects
 * never enter browser storage or cross an API boundary. */
export function provider(kind?: WalletProviderId): Eip1193Provider | null {
  const providers = ethereumProviders();
  if (!kind) return providers[0] ?? window.keplr?.ethereum ?? null;
  if (kind === "keplr") return window.keplr?.ethereum ?? providers.find((p) => p.isKeplr) ?? null;
  return providers.find((p) => kind === "metamask" ? p.isMetaMask && !p.isCoinbaseWallet : p.isCoinbaseWallet) ?? null;
}

export function walletAvailable(kind: WalletProviderId): boolean {
  return provider(kind) !== null;
}

export function shortAddress(address: string): string {
  return `${address.slice(0, 6)}…${address.slice(-4)}`;
}

export async function connectWallet(kind: WalletProviderId): Promise<string> {
  const p = provider(kind);
  if (!p) throw new Error(`${walletProviderNames[kind]} extension or app is required. Install it, unlock it, then try again.`);
  const accounts = await p.request({ method: "eth_requestAccounts" });
  const address = Array.isArray(accounts) && typeof accounts[0] === "string" ? accounts[0] : "";
  if (!/^0x[a-fA-F0-9]{40}$/.test(address)) throw new Error("Wallet did not provide an account.");
  return address;
}

export async function walletChainId(kind?: WalletProviderId): Promise<number | null> {
  const p = provider(kind);
  if (!p) return null;
  const chain = await p.request({ method: "eth_chainId" });
  if (typeof chain !== "string") return null;
  const value = Number.parseInt(chain, 16);
  return Number.isSafeInteger(value) ? value : null;
}

export async function switchNetwork(network: TheoNetwork, kind?: WalletProviderId): Promise<void> {
  const p = provider(kind);
  if (!p) throw new Error("No EIP-1193 wallet was found.");
  const chainId = `0x${network.chain_id.toString(16)}`;
  try {
    await p.request({ method: "wallet_switchEthereumChain", params: [{ chainId }] });
  } catch (error) {
    const code = typeof error === "object" && error !== null && "code" in error ? (error as { code?: number }).code : undefined;
    if (code !== 4902) throw error;
    await p.request({
      method: "wallet_addEthereumChain",
      params: [{
        chainId,
        chainName: network.chain_name,
        rpcUrls: [network.rpc_url],
        blockExplorerUrls: network.explorer_url ? [network.explorer_url] : [],
        nativeCurrency: { name: "Autheo", symbol: "THEO", decimals: 18 },
      }],
    });
  }
}

function padAddress(address: string): string {
  if (!/^0x[a-fA-F0-9]{40}$/.test(address)) throw new Error("Invalid configured THEO address.");
  return address.slice(2).toLowerCase().padStart(64, "0");
}

/** Encode an ERC-20 `transfer(address,uint256)`. Amounts are already atomic THEO units. */
export function transferCalldata(to: string, amountAtomic: string): string {
  if (!/^[1-9][0-9]*$/.test(amountAtomic)) throw new Error("Invalid THEO amount.");
  const amount = BigInt(amountAtomic).toString(16).padStart(64, "0");
  return `0xa9059cbb${padAddress(to)}${amount}`;
}

export async function sendTheoTransfer(network: TheoNetwork, from: string, amountAtomic: string, kind?: WalletProviderId): Promise<string> {
  const p = provider(kind);
  if (!p) throw new Error("No EIP-1193 wallet was found.");
  const result = await p.request({
    method: "eth_sendTransaction",
    params: [{ from, to: network.token_address, data: transferCalldata(network.treasury_address, amountAtomic), value: "0x0" }],
  });
  if (typeof result !== "string" || !/^0x[a-fA-F0-9]{64}$/.test(result)) throw new Error("Wallet did not return a transaction hash.");
  return result;
}
