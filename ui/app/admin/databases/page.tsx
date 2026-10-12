import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsResult } from "@/lib/ops-data";
import type { Database } from "@/lib/api";
import { AdminDatabasesClient } from "./databases-client";

export default function AdminDatabasesPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <AdminDatabasesData />
    </Suspense>
  );
}

async function AdminDatabasesData() {
  // Platform-owner view -> ALL databases across every tenant (global endpoint).
  const { data: dbs, error } = await fetchOpsResult<Database[]>("/v1/admin/databases");
  return <AdminDatabasesClient initialDbs={dbs} initialError={error} />;
}
