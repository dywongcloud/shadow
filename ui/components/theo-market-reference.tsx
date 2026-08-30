"use client";

import { useEffect, useState } from "react";

type MarketReference = {
  available: boolean;
  price_usd?: number;
  change_24h_pct?: number | null;
  liquidity_usd?: number | null;
  source?: string;
  source_url?: string | null;
  fetched_at_ms?: number;
  stale?: boolean;
  reason?: string;
};

export function TheoMarketReference({ compact = false }: { compact?: boolean }) {
  const [data, setData] = useState<MarketReference | null>(null);
  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const response = await fetch("/api/market-reference/theo", { cache: "no-store" });
        const value = await response.json() as MarketReference;
        if (!cancelled) setData(value);
      } catch {
        if (!cancelled) setData({ available: false });
      }
    };
    void load();
    const timer = window.setInterval(load, 60_000);
    return () => { cancelled = true; window.clearInterval(timer); };
  }, []);
  if (!data?.available) {
    return <p className="text-xs text-muted">THEO/USD display-only reference unavailable. Payments remain denominated in THEO.</p>;
  }
  const change = data.change_24h_pct;
  return (
    <div className={compact ? "text-xs text-muted" : "rounded-lg border border-border bg-subtle/30 p-3 text-sm"}>
      <span className="font-medium text-fg">THEO/USD ${data.price_usd?.toLocaleString(undefined, { maximumSignificantDigits: 6 })}</span>
      {change !== null && change !== undefined && <span className={`ml-2 ${change < 0 ? "text-red-500" : "text-emerald-600 dark:text-emerald-400"}`}>{change >= 0 ? "+" : ""}{change.toFixed(2)}% 24h</span>}
      {!compact && (
        <div className="mt-1 text-xs text-secondary">
          Display-only USD reference — never used to calculate a charge.
          {data.liquidity_usd !== null && data.liquidity_usd !== undefined && <> · Liquidity ${data.liquidity_usd.toLocaleString()}</>}
          {data.source_url ? <a className="ml-1 text-link hover:underline" href={data.source_url} target="_blank" rel="noreferrer">{data.source}</a> : <> · {data.source}</>}
          {data.stale && <span className="ml-1 text-amber-600">· stale</span>}
        </div>
      )}
    </div>
  );
}

/** Secondary display only. The primary amount always remains THEO, including
 * when the reference is stale or unavailable. */
export function TheoUsdEstimate({ amountTheo }: { amountTheo: number }) {
  const [data, setData] = useState<MarketReference | null>(null);
  useEffect(() => {
    let cancelled = false;
    fetch("/api/market-reference/theo", { cache: "no-store" })
      .then((response) => response.json())
      .then((value: MarketReference) => { if (!cancelled) setData(value); })
      .catch(() => { if (!cancelled) setData({ available: false }); });
    return () => { cancelled = true; };
  }, []);
  if (!data?.available || !data.price_usd) {
    return <p className="text-xs text-muted">USD reference only — display-only USD estimate unavailable.</p>;
  }
  const estimate = amountTheo * data.price_usd;
  return (
    <p className={`text-xs ${data.stale ? "text-amber-600" : "text-muted"}`}>
      USD reference only — display-only USD estimate: ${estimate.toLocaleString(undefined, { maximumFractionDigits: 2 })}
      {data.stale ? " (market reference stale)" : ""}
    </p>
  );
}
