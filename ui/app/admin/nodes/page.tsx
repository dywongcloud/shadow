import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsServer } from "@/lib/ops-data";
import type { NodeInfo, AdminOverview } from "@/lib/api";
import { AdminNodesClient } from "./nodes-client";

export default function AdminNodesPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <AdminNodesData />
    </Suspense>
  );
}

async function AdminNodesData() {
  const [nodes, ov] = await Promise.all([
    fetchOpsServer<NodeInfo[]>("/v1/nodes").catch(() => null),
    fetchOpsServer<AdminOverview>("/v1/admin/overview").catch(() => null),
  ]);
  return <AdminNodesClient initialNodes={nodes} initialOverview={ov} />;
}
