import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsResult } from "@/lib/ops-data";
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
  const { data: teams, error } = await fetchOpsResult<Team[]>("/v1/teams");
  return <AdminTeamsClient initialTeams={teams} initialError={error} />;
}
