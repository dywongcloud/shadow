import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsResult } from "@/lib/ops-data";
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
    fetchOpsResult<NodeInfo[]>("/v1/nodes"),
    fetchOpsResult<AdminOverview>("/v1/admin/overview"),
  ]);
  return (
    <AdminNodesClient
      initialNodes={nodes.data}
      initialOverview={ov.data}
      initialNodesError={nodes.error}
      initialOverviewError={ov.error}
    />
  );
}
