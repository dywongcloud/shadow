import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsServer } from "@/lib/ops-data";
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
  const dbs = await fetchOpsServer<Database[]>("/v1/admin/databases").catch(() => null);
  return <AdminDatabasesClient initialDbs={dbs} />;
}
