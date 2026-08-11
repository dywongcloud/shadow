import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { resolveTeam, fetchMetricsSummary, fetchSpeedInsightsSummary } from "@/lib/observability-data";
import { ObservabilityClient } from "./observability-client";

// Server shell, mirroring app/admin/page.tsx's precedent: renders instantly,
// while the actual observability data streams in via the Suspense boundary
// below -- the old pure-client page was "use client" from line 1 with no
// server prefetch at all, so first paint was always a blank shell until the
// client's own poll landed.
//
// This one additionally needs `searchParams` (Next 16: a Promise) to decide
// which tab is active BEFORE rendering, so the correct tab's data — and only
// that tab's — is prefetched. Deciding the tab here (not inside the client
// component from `window.location`) is also what fixes a real hydration bug:
// see observability-client.tsx's comment on the old useState initializer.
export default async function ObservabilityPage({
  searchParams,
}: {
  searchParams: Promise<{ tab?: string }>;
}) {
  const sp = await searchParams;
  const initialTab: "overview" | "speed" = sp.tab === "speed-insights" ? "speed" : "overview";
  return (
    <Suspense fallback={<PageSkeleton />}>
      <ObservabilityData initialTab={initialTab} />
    </Suspense>
  );
}

async function ObservabilityData({ initialTab }: { initialTab: "overview" | "speed" }) {
  const team = await resolveTeam(null);
  // DEFAULT_RANGE_MINUTES=720 here MUST match observability-client.tsx's
  // useState(DEFAULT_RANGE_MINUTES); DEFAULT_SPEED_MINUTES=10_080 + device=
  // "desktop" here MUST match speed-insights.tsx's useState("Last 7
  // Days")/useState("Desktop") defaults. A mismatch would not break anything
  // (usePoll's own tick would just fetch a different window a moment later
  // and replace the seed) but would defeat the point of seeding — first paint
  // would show data for a window the UI doesn't claim to be showing.
  //
  // Only the ACTIVE tab is prefetched — a deliberate scope decision extending
  // the existing lazy-loading discipline (next/dynamic already defers each
  // tab's chart bundle) from code to data: the inactive tab's fleet-fanned-out
  // query stays deferred until the user actually switches to it, exactly like
  // its JS chunk already was.
  const [metrics, speed] = await Promise.all([
    initialTab === "overview" ? fetchMetricsSummary(team, 720).catch(() => null) : Promise.resolve(null),
    initialTab === "speed"
      ? fetchSpeedInsightsSummary(team, 10_080, "desktop").catch(() => null)
      : Promise.resolve(null),
  ]);
  return <ObservabilityClient initialTab={initialTab} initialMetrics={metrics} initialSpeedInsights={speed} />;
}
