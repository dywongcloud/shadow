import { NextRequest, NextResponse } from "next/server";
import { resolveTeam, fetchMetricsSummary, BackendFetchError } from "@/lib/observability-data";

/**
 * CACHED observability summary — the server-side seam Observability and Speed
 * Insights were missing.
 *
 * Why this exists at all. The Observability page is `"use client"` and polls
 * `/cloud/v1/metrics?minutes=N` every 5s through `apiGet`, which sets
 * `cache: "no-store"`. That is correct for a LIVE TAIL and wrong for everything
 * else: it means the first paint blocks on a client round trip to the control
 * plane, every viewer of the same tenant re-asks for identical data, and there
 * is no HTTP layer anywhere to attach a cache policy to — so "add ISR" had
 * nothing to attach to either. A client component cannot carry `revalidate`,
 * so no amount of editing the page could have produced one.
 *
 * What this adds, and what it deliberately does NOT change. This route serves
 * the SUMMARY view (the initial, cacheable payload) with a real cache policy;
 * the live tail keeps its uncached 5s poll untouched, because a metric a user
 * is watching tick must not be served stale. The two are different products
 * sharing a data source, and conflating them is how a "cache everything" pass
 * makes a live dashboard lie.
 *
 * The policy:
 *   * `s-maxage=30` — a shared cache may serve this for 30s. Matches the poll
 *     interval's order of magnitude, so the cached summary is never
 *     meaningfully older than what a live viewer already sees.
 *   * `stale-while-revalidate=300` — for 5 minutes past expiry a viewer gets
 *     the stale summary INSTANTLY while the refresh happens behind them. This
 *     is the property that matters on a control-plane page: metrics are the
 *     first thing an operator opens during an incident, which is exactly when
 *     the backend is slowest, and a blank chart that eventually fills is worse
 *     than a 45-second-old one that renders now.
 *   * `private` — this is TENANT data. It must never enter a shared CDN cache
 *     keyed without the tenant; `private` confines it to the browser cache and
 *     Next's own per-request data cache.
 *
 * Tenanting is load-bearing. The cache key includes the resolved tenant AND the
 * window, because `/v1/metrics` answers differently per team; a cache keyed on
 * the URL alone would serve one tenant's metrics to another. That is why the
 * tenant is resolved server-side here rather than trusted from a header the
 * client could set.
 */

// Node runtime: `backend()` reads server-only config and talks to the local
// admin socket, neither of which exists on the edge runtime.
export const runtime = "nodejs";

/** Seconds a shared cache may serve this summary without revalidating. */
const S_MAXAGE = 30;
/** Seconds past expiry a stale summary may still be served instantly. */
const SWR = 300;

/** Windows the UI offers. Anything else is rejected rather than forwarded, so a
 *  crafted `minutes` cannot turn this into an unbounded backend query — and so
 *  the cache cannot be fragmented into unbounded distinct keys by a caller. */
// 720 (12h) is the OBSERVABILITY PAGE'S OWN DEFAULT range
// (ui/app/observability/observability-client.tsx's `useState(720)`) — it was
// missing here, which meant the page's default view would 400 against this
// route the moment anything called it with no explicit `minutes` override.
// Found by cross-referencing this allow-list against its one real consumer,
// not by inspection of this file alone.
const ALLOWED_MINUTES = new Set([15, 60, 360, 720, 1440, 10080]);

export async function GET(req: NextRequest) {
  const raw = req.nextUrl.searchParams.get("minutes");
  const minutes = Number(raw ?? 60);
  if (!Number.isInteger(minutes) || !ALLOWED_MINUTES.has(minutes)) {
    return NextResponse.json(
      { ok: false, error: `unsupported window "${raw}" — allowed: ${[...ALLOWED_MINUTES].join(", ")}` },
      { status: 400 }
    );
  }

  const team = await resolveTeam(req.headers.get("x-hive-team"));

  let data: unknown;
  try {
    data = await fetchMetricsSummary(team, minutes);
  } catch (e: unknown) {
    // A summary is a CONVENIENCE over the live poll, which still runs. Failing
    // it must not blank the page, so report the fault honestly and let the
    // client fall back to polling rather than throwing a 500 at the shell.
    const status = e instanceof BackendFetchError ? e.status : 502;
    const message = e instanceof Error ? e.message : String(e);
    return NextResponse.json({ ok: false, error: message }, { status, headers: { "cache-control": "no-store" } });
  }

  return NextResponse.json(
    { ok: true, minutes, team, generatedAt: new Date().toISOString(), data },
    {
      headers: {
        // NEVER `public`: tenant data. See the doc comment above.
        "cache-control": `private, s-maxage=${S_MAXAGE}, stale-while-revalidate=${SWR}`,
        // Make the tenant part of the cache identity explicit to any
        // intermediary, so a proxy cannot key on the URL alone.
        vary: "x-hive-team, cookie",
      },
    }
  );
}
