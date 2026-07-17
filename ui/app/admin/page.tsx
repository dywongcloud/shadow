import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsServer } from "@/lib/ops-data";
import type { AdminOverview, Metrics, Incident } from "@/lib/api";
import { AdminOverviewClient } from "./overview-client";

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
  const [ov, metrics, incidents] = await Promise.all([
    fetchOpsServer<AdminOverview>("/v1/admin/overview").catch(() => null),
    fetchOpsServer<Metrics>("/v1/metrics?minutes=60").catch(() => null),
    fetchOpsServer<Incident[]>("/v1/incidents").catch(() => null),
  ]);
  return <AdminOverviewClient initialOverview={ov} initialMetrics={metrics} initialIncidents={incidents} />;
}
