import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsServer } from "@/lib/ops-data";
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
  const events = await fetchOpsServer<(Event & { project: string })[]>("/v1/admin/audit").catch(() => null);
  return <AdminAuditClient initialEvents={events as AuditEvent[] | null} />;
}
