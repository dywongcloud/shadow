import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsResult } from "@/lib/ops-data";
import type { Event } from "@/lib/api";
import { AdminAuditClient, type AuditEvent } from "./audit-client";

export default function AuditPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <AdminAuditData />
    </Suspense>
  );
}

async function AdminAuditData() {
  const { data: events, error } = await fetchOpsResult<(Event & { project: string })[]>("/v1/admin/audit");
  return <AdminAuditClient initialEvents={events as AuditEvent[] | null} initialError={error} />;
}
