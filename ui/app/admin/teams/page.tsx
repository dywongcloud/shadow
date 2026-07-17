import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsServer } from "@/lib/ops-data";
import type { Team } from "@/lib/api";
import { AdminTeamsClient } from "./teams-client";

export default function AdminTeamsPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <AdminTeamsData />
    </Suspense>
  );
}

async function AdminTeamsData() {
  const teams = await fetchOpsServer<Team[]>("/v1/teams").catch(() => null);
  return <AdminTeamsClient initialTeams={teams} />;
}
