import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsResult } from "@/lib/ops-data";
import type { Incident } from "@/lib/api";
import { IncidentsClient } from "./incidents-client";
import { OPEN_LIMIT, OPEN_PATH, RESOLVED_PAGE, resolvedPath } from "@/lib/incident-paths";

export default function IncidentsPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <IncidentsData />
    </Suspense>
  );
}

async function IncidentsData() {
  // Open incidents (capped) plus the first page of resolved history — never
  // the whole list (16,612 rows / 10.5 MB on 2026-10-01).
  const [open, resolved] = await Promise.all([
    fetchOpsResult<Incident[]>(OPEN_PATH),
    fetchOpsResult<Incident[]>(resolvedPath(0)),
  ]);
  // Cap here too: a backend that ignores the paging params (an older build)
  // must never make this page embed an unbounded list in its HTML again.
  return (
    <IncidentsClient
      initialOpen={open.data ? open.data.filter((i) => i.status !== "resolved").slice(0, OPEN_LIMIT) : null}
      initialResolved={resolved.data ? resolved.data.filter((i) => i.status === "resolved").slice(0, RESOLVED_PAGE) : null}
      initialOpenError={open.error}
      initialResolvedError={resolved.error}
    />
  );
}
