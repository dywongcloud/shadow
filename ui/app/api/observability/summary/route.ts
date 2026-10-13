import { NextRequest, NextResponse } from "next/server";
import { resolveTeam, fetchMetricsSummary, BackendFetchError } from "@/lib/observability-data";

// Cached observability SUMMARY (the cacheable first-paint payload; the live tail keeps its uncached 5 s poll, which must never be served stale).
// `private` keeps tenant data out of shared caches and `vary: x-hive-team, cookie` makes the tenant part of the cache identity; the tenant is
// resolved server-side, never trusted from a client header, because `/v1/metrics` answers per team and a URL-only key would serve one tenant's
// metrics to another.

/** Seconds a shared cache may serve this summary without revalidating. */
const S_MAXAGE = 30;
/** Seconds past expiry a stale summary may still be served instantly. */
const SWR = 300;

/** Windows the UI offers: anything else is rejected, so a crafted `minutes` can neither become an unbounded backend query nor fragment the cache
 *  into unbounded keys. */
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
    const status = e instanceof BackendFetchError ? e.status : 502;
    const message = e instanceof Error ? e.message : String(e);
    return NextResponse.json({ ok: false, error: message }, { status, headers: { "cache-control": "no-store" } });
  }

  return NextResponse.json(
    { ok: true, minutes, team, generatedAt: new Date().toISOString(), data },
    {
      headers: {
        "cache-control": `private, s-maxage=${S_MAXAGE}, stale-while-revalidate=${SWR}`,
        vary: "x-hive-team, cookie",
      },
    }
  );
}
