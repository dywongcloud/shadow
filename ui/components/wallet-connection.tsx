"use client";

import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";
import { ChevronDown, Loader2, Wallet } from "lucide-react";
import { Button } from "@/components/ui";
import {
  connectWallet,
  provider,
  shortAddress,
  walletAvailable,
  walletChainId,
  walletProviderNames,
  type TheoNetwork,
  type WalletProviderId,
} from "@/lib/theo-wallet";
import { apiGet } from "@/lib/api";

type WalletSession = {
  address: string;
  chainId: number | null;
  providerId: WalletProviderId | null;
  connect: (providerId: WalletProviderId) => Promise<void>;
  disconnect: () => void;
};

const WalletContext = createContext<WalletSession | null>(null);

export function WalletProvider({ children }: { children: React.ReactNode }) {
  const [address, setAddress] = useState("");
  const [chainId, setChainId] = useState<number | null>(null);
  const [providerId, setProviderId] = useState<WalletProviderId | null>(null);

  const disconnect = useCallback(() => {
    setAddress("");
    setChainId(null);
    setProviderId(null);
  }, []);

  const connect = useCallback(async (id: WalletProviderId) => {
    const nextAddress = await connectWallet(id);
    setAddress(nextAddress);
    setChainId(await walletChainId(id));
    setProviderId(id);
  }, []);

  useEffect(() => {
    if (!providerId) return;
    const p = provider(providerId);
    if (!p?.on) return;
    const accountsChanged = (...args: unknown[]) => {
      const accounts = args[0];
      const next = Array.isArray(accounts) && typeof accounts[0] === "string" ? accounts[0] : "";
      if (/^0x[a-fA-F0-9]{40}$/.test(next)) setAddress(next);
      else disconnect();
    };
    const chainChanged = (...args: unknown[]) => {
      const value = args[0];
      setChainId(typeof value === "string" ? Number.parseInt(value, 16) : null);
    };
    p.on("accountsChanged", accountsChanged);
    p.on("chainChanged", chainChanged);
    p.on("disconnect", disconnect);
    return () => {
      p.removeListener?.("accountsChanged", accountsChanged);
      p.removeListener?.("chainChanged", chainChanged);
      p.removeListener?.("disconnect", disconnect);
    };
  }, [disconnect, providerId]);

  const value = useMemo(() => ({ address, chainId, providerId, connect, disconnect }), [address, chainId, providerId, connect, disconnect]);
  return <WalletContext.Provider value={value}>{children}</WalletContext.Provider>;
}

export function useWallet() {
  const value = useContext(WalletContext);
  if (!value) throw new Error("WalletProvider is missing.");
  return value;
}

export function WalletConnectionButton() {
  const wallet = useWallet();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState<WalletProviderId | null>(null);
  const [network, setNetwork] = useState<TheoNetwork | null>(null);
  const [message, setMessage] = useState("");

  useEffect(() => {
    apiGet<TheoNetwork>("/v1/billing/wallet-config").then(setNetwork).catch(() => setNetwork(null));
  }, []);

  async function select(id: WalletProviderId) {
    setMessage("");
    if (!walletAvailable(id)) {
      setMessage(`${walletProviderNames[id]} extension or app is required. Install or unlock it, then try again.`);
      return;
    }
    setBusy(id);
    try {
      await wallet.connect(id);
      setOpen(false);
    } catch (error) {
      const raw = error instanceof Error ? error.message : String(error);
      setMessage(/reject|denied|4001/i.test(raw) ? "Wallet connection was rejected." : raw);
    } finally {
      setBusy(null);
    }
  }

  const networkLabel = network && wallet.chainId === network.chain_id ? network.chain_name : wallet.chainId === null ? "Network unknown" : `Wrong network (${wallet.chainId})`;
  return (
    <div className="relative">
      <Button variant="outline" className="hidden sm:inline-flex" onClick={() => setOpen((value) => !value)} aria-expanded={open} aria-haspopup="menu">
        <Wallet className="h-3.5 w-3.5" /> {wallet.address ? shortAddress(wallet.address) : "Connect wallet"} <ChevronDown className="h-3.5 w-3.5" />
      </Button>
      {open && (
        <div role="menu" className="absolute right-0 z-50 mt-2 w-80 rounded-lg border border-border bg-card p-2 shadow-pop">
          {wallet.address ? (
            <div className="p-3">
              <div className="font-mono text-sm font-medium">{shortAddress(wallet.address)}</div>
              <p className={`mt-1 text-xs ${network && wallet.chainId !== network.chain_id ? "text-amber-600" : "text-secondary"}`}>{networkLabel}</p>
              <Button variant="ghost" className="mt-3 w-full justify-center" onClick={() => { wallet.disconnect(); setOpen(false); }}>Disconnect</Button>
            </div>
          ) : (
            <>
              <p className="px-3 pb-2 pt-2 text-xs text-muted">Choose a browser wallet. Private keys and recovery phrases never leave your wallet.</p>
              {(["metamask", "coinbase", "keplr"] as WalletProviderId[]).map((id) => (
                <button key={id} role="menuitem" disabled={busy !== null} onClick={() => void select(id)} className="flex w-full items-center justify-between rounded-md px-3 py-2.5 text-left text-sm hover:bg-subtle disabled:opacity-50">
                  <span className="font-medium">{walletProviderNames[id]}</span>
                  {busy === id ? <Loader2 className="h-4 w-4 animate-spin" /> : walletAvailable(id) ? <span className="text-xs text-emerald-600">Available</span> : <span className="text-xs text-muted">Extension required</span>}
                </button>
              ))}
              {message && <p role="alert" className="px-3 pb-2 pt-2 text-xs text-amber-600">{message}</p>}
            </>
          )}
        </div>
      )}
    </div>
  );
}
