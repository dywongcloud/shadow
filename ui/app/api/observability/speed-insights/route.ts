import { NextRequest, NextResponse } from "next/server";
import { resolveTeam, fetchSpeedInsightsSummary, BackendFetchError } from "@/lib/observability-data";

/**
 * CACHED Speed Insights summary — sibling to ../summary/route.ts, same policy
 * rationale (see that file's doc comment for the full "why"), separate
 * contract: RUM aggregates over the multi-day windows Speed Insights actually
 * offers, not the request-count windows Observability offers. Conflating the
 * two allow-lists is exactly the drift this route avoids by owning its own.
 */
/** Same values as summary/route.ts — a multi-day RUM aggregate moves even
 *  slower than a request-count window, so this is if anything conservative. */
const S_MAXAGE = 30;
const SWR = 300;

/** Matches ui/components/speed-insights.tsx's RANGE_MINUTES exactly:
 *  Last 24 Hours / Last 7 Days / Last 30 Days. This route's own allow-list —
 *  never merged with summary/route.ts's — because the two are genuinely
 *  different query shapes over the same backend. */
const ALLOWED_MINUTES = new Set([1440, 10080, 43200]);
const ALLOWED_DEVICES = new Set(["desktop", "mobile"]);

export async function GET(req: NextRequest) {
  const rawMinutes = req.nextUrl.searchParams.get("minutes");
  const minutes = Number(rawMinutes ?? 10080);
  if (!Number.isInteger(minutes) || !ALLOWED_MINUTES.has(minutes)) {
    return NextResponse.json(
      { ok: false, error: `unsupported window "${rawMinutes}" — allowed: ${[...ALLOWED_MINUTES].join(", ")}` },
      { status: 400 }
    );
  }

  const rawDevice = req.nextUrl.searchParams.get("device");
  // Reject rather than forward, same posture as the minutes check: a crafted
  // device value must not reach the backend unvalidated or fragment the cache
  // key into an unbounded set of keys.
  if (rawDevice != null && !ALLOWED_DEVICES.has(rawDevice)) {
    return NextResponse.json(
      { ok: false, error: `unsupported device "${rawDevice}" — allowed: desktop, mobile` },
      { status: 400 }
    );
  }
  const device = rawDevice as "desktop" | "mobile" | undefined;

  const team = await resolveTeam(req.headers.get("x-hive-team"));

  let data: unknown;
  try {
    data = await fetchSpeedInsightsSummary(team, minutes, device);
  } catch (e: unknown) {
    const status = e instanceof BackendFetchError ? e.status : 502;
    const message = e instanceof Error ? e.message : String(e);
    return NextResponse.json({ ok: false, error: message }, { status, headers: { "cache-control": "no-store" } });
  }

  return NextResponse.json(
    { ok: true, minutes, device: device ?? null, team, generatedAt: new Date().toISOString(), data },
    {
      headers: {
        // NEVER `public`: tenant data. See summary/route.ts's doc comment.
        "cache-control": `private, s-maxage=${S_MAXAGE}, stale-while-revalidate=${SWR}`,
        vary: "x-hive-team, cookie",
      },
    }
  );
}
