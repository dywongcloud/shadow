import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { resolveTeam, fetchMetricsSummary, fetchSpeedInsightsSummary } from "@/lib/observability-data";
import { ObservabilityClient } from "./observability-client";

const SEEDED_METRICS_MINUTES = 720;
const SEEDED_SPEED_MINUTES = 10_080;
const SEEDED_SPEED_DEVICE = "desktop";

// Server shell (mirroring app/admin/page.tsx): it renders at once while the data streams in through the Suspense boundary below, and it decides the active tab
// from `searchParams` (a Promise in Next 16) BEFORE rendering so only that tab's data is prefetched, seeded with the client's default window; deciding it here
// rather than from `window.location` inside the client component is also what avoids the hydration mismatch.
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
  const [metrics, speed] = await Promise.all([
    initialTab === "overview" ? fetchMetricsSummary(team, SEEDED_METRICS_MINUTES).catch(() => null) : Promise.resolve(null),
    initialTab === "speed"
      ? fetchSpeedInsightsSummary(team, SEEDED_SPEED_MINUTES, SEEDED_SPEED_DEVICE).catch(() => null)
      : Promise.resolve(null),
  ]);
  return <ObservabilityClient initialTab={initialTab} initialMetrics={metrics} initialSpeedInsights={speed} />;
}
