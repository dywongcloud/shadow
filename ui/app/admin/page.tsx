import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsResult } from "@/lib/ops-data";
import type { AdminOverview, Metrics, Incident } from "@/lib/api";
import { AdminOverviewClient } from "./overview-client";
import { OPEN_INCIDENTS_LIMIT, OPEN_INCIDENTS_PATH } from "@/lib/incident-paths";

// Server shell: renders instantly (no per-request dynamic API used here), while
// the actual ops data streams in via the Suspense boundary below -- avoids the
// old pure-client page's blank-then-fetch round trip on first load.
export default function AdminOverviewPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <AdminOverviewData />
    </Suspense>
  );
}

async function AdminOverviewData() {
  // Always fresh (no-store) -- live operational data, never cached. Fetched in
  // parallel server-side so the FIRST paint already has real numbers instead of
  // the client having to make its own round trip after hydration. A failed
  // server-side fetch (e.g. backend momentarily unreachable) falls back to null
  // rather than crashing the Suspense boundary -- the client component's own
  // useOpsPoll takes over and recovers on its next tick, matching the previous
  // pure-client page's tolerant behavior (fields render "—" until data arrives).
  // Only OPEN incidents, capped: the unpaged list reached 16,612 rows /
  // 10.5 MB (2026-10-01) and was embedded in this page's HTML on every load.
  const [ov, metrics, incidents] = await Promise.all([
    fetchOpsResult<AdminOverview>("/v1/admin/overview"),
    fetchOpsResult<Metrics>("/v1/metrics?minutes=60"),
    fetchOpsResult<Incident[]>(OPEN_INCIDENTS_PATH),
  ]);
  // Cap here too: a backend that ignores the paging params (an older build)
  // must never make this page embed an unbounded list in its HTML again.
  const open = incidents.data ? incidents.data.filter((i) => i.status !== "resolved").slice(0, OPEN_INCIDENTS_LIMIT) : null;
  return (
    <AdminOverviewClient
      initialOverview={ov.data}
      initialMetrics={metrics.data}
      initialIncidents={open}
      initialError={ov.error ?? metrics.error ?? incidents.error ?? null}
    />
  );
}
