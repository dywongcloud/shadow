import { NextRequest, NextResponse } from "next/server";
import { resolveTeam, fetchSpeedInsightsSummary, BackendFetchError } from "@/lib/observability-data";

// Cached Speed Insights summary, sibling of ../summary/route.ts (same cache policy, see its header): RUM aggregates over the multi-day windows Speed Insights
// offers, with its own allow-lists so the two query shapes cannot drift together.

/** Same values as summary/route.ts (a multi-day RUM aggregate moves even slower, so these are conservative). */
const S_MAXAGE = 30;
const SWR = 300;

/** The windows components/speed-insights.tsx offers (its `RANGE_MINUTES`: 24 h / 7 d / 30 d), kept apart from summary/route.ts's allow-list. */
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
        "cache-control": `private, s-maxage=${S_MAXAGE}, stale-while-revalidate=${SWR}`,
        vary: "x-hive-team, cookie",
      },
    }
  );
}
