import { NextResponse } from "next/server";

export const dynamic = "force-dynamic";
export const revalidate = 0;

const FALLBACK_PAIR = "0x182be47742b81777055d69c50e5c9d2fe803e938";
const STALE_MS = 5 * 60_000;
const MAX_STALE_MS = 15 * 60_000;
let lastKnown: Record<string, unknown> | null = null;

type Pair = {
  chainId?: unknown;
  pairAddress?: unknown;
  priceUsd?: unknown;
  priceChange?: { h24?: unknown };
  liquidity?: { usd?: unknown };
  url?: unknown;
};

function number(value: unknown): number | null {
  const parsed = typeof value === "number" ? value : typeof value === "string" ? Number(value) : NaN;
  return Number.isFinite(parsed) ? parsed : null;
}

/** The Marketplace Dexscreener pair contract, normalized without deriving any payment amount. */
export async function GET() {
  const pair = (process.env.THEO_MARKET_PAIR_ADDRESS || FALLBACK_PAIR).trim().toLowerCase();
  if (!/^0x[a-f0-9]{40}$/.test(pair)) {
    return NextResponse.json({ available: false, reason: "market-reference misconfigured" }, { status: 503 });
  }
  try {
    const response = await fetch(`https://api.dexscreener.com/latest/dex/pairs/base/${pair}`, {
      headers: { accept: "application/json" },
      cache: "no-store",
      signal: AbortSignal.timeout(5_000),
    });
    const body = await response.json().catch(() => null) as { pairs?: unknown } | null;
    const candidate = Array.isArray(body?.pairs) ? body.pairs.find((item) => {
      const p = item as Pair;
      return p?.chainId === "base" && typeof p.pairAddress === "string" && p.pairAddress.toLowerCase() === pair;
    }) as Pair | undefined : undefined;
    const price_usd = number(candidate?.priceUsd);
    if (!response.ok || !candidate || price_usd === null || price_usd <= 0) throw new Error("malformed market response");
    const fetched_at_ms = Date.now();
    const reference = {
      available: true,
      price_usd,
      change_24h_pct: number(candidate.priceChange?.h24),
      liquidity_usd: number(candidate.liquidity?.usd),
      source: "Dexscreener · Base / Hydrex",
      source_url: typeof candidate.url === "string" ? candidate.url : null,
      fetched_at_ms,
      stale: false,
      display_only: true,
    };
    lastKnown = reference;
    return NextResponse.json(reference, { headers: { "cache-control": "no-store" } });
  } catch {
    const fetched = typeof lastKnown?.fetched_at_ms === "number" ? lastKnown.fetched_at_ms : 0;
    if (lastKnown && Date.now() - fetched <= MAX_STALE_MS) {
      return NextResponse.json({ ...lastKnown, stale: Date.now() - fetched > STALE_MS }, { headers: { "cache-control": "no-store" } });
    }
    return NextResponse.json({ available: false, reason: "market reference unavailable" }, { status: 503 });
  }
}
