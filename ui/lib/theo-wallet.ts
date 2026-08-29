"use client";

export type Eip1193Provider = {
  request(args: { method: string; params?: unknown[] }): Promise<unknown>;
};

declare global {
  interface Window {
    ethereum?: Eip1193Provider;
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
};

export function provider(): Eip1193Provider | null {
  return typeof window !== "undefined" ? window.ethereum ?? null : null;
}

export async function connectWallet(): Promise<string> {
  const p = provider();
  if (!p) throw new Error("No EIP-1193 wallet was found. Install or unlock a compatible wallet.");
  const accounts = await p.request({ method: "eth_requestAccounts" });
  const address = Array.isArray(accounts) && typeof accounts[0] === "string" ? accounts[0] : "";
  if (!/^0x[a-fA-F0-9]{40}$/.test(address)) throw new Error("Wallet did not provide an account.");
  return address;
}

export async function walletChainId(): Promise<number | null> {
  const p = provider();
  if (!p) return null;
  const chain = await p.request({ method: "eth_chainId" });
  if (typeof chain !== "string") return null;
  const value = Number.parseInt(chain, 16);
  return Number.isSafeInteger(value) ? value : null;
}

export async function switchNetwork(network: TheoNetwork): Promise<void> {
  const p = provider();
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

export async function sendTheoTransfer(network: TheoNetwork, from: string, amountAtomic: string): Promise<string> {
  const p = provider();
  if (!p) throw new Error("No EIP-1193 wallet was found.");
  const result = await p.request({
    method: "eth_sendTransaction",
    params: [{ from, to: network.token_address, data: transferCalldata(network.treasury_address, amountAtomic), value: "0x0" }],
  });
  if (typeof result !== "string" || !/^0x[a-fA-F0-9]{64}$/.test(result)) throw new Error("Wallet did not return a transaction hash.");
  return result;
}
