import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsResult } from "@/lib/ops-data";
import type { AdminOverview, Metrics, Incident } from "@/lib/api";
import { AdminOverviewClient } from "./overview-client";
import { OPEN_INCIDENTS_LIMIT, OPEN_INCIDENTS_PATH } from "@/lib/incident-paths";

export default function AdminOverviewPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <AdminOverviewData />
    </Suspense>
  );
}

async function AdminOverviewData() {
  const [ov, metrics, incidents] = await Promise.all([
    fetchOpsResult<AdminOverview>("/v1/admin/overview"),
    fetchOpsResult<Metrics>("/v1/metrics?minutes=60"),
    fetchOpsResult<Incident[]>(OPEN_INCIDENTS_PATH),
  ]);
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
