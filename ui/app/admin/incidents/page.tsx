import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsServer } from "@/lib/ops-data";
import type { Incident } from "@/lib/api";
import { IncidentsClient } from "./incidents-client";

export default function IncidentsPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <IncidentsData />
    </Suspense>
  );
}

async function IncidentsData() {
  const incidents = await fetchOpsServer<Incident[]>("/v1/incidents").catch(() => null);
  return <IncidentsClient initialIncidents={incidents} />;
}
