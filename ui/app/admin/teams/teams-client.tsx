"use client";

import Link from "next/link";
import { Users, ExternalLink } from "lucide-react";
import { Card, Badge, Table, Th, Td } from "@/components/ui";
import { useOpsPoll, type Team } from "@/lib/api";
import { OpsFetchError } from "@/components/ops-fetch-error";
import { timeAgo } from "@/lib/utils";

export function AdminTeamsClient({
  initialTeams,
  initialError,
}: {
  initialTeams: Team[] | null;
  initialError?: string | null;
}) {
  const { data: teams, error } = useOpsPoll<Team[]>("/v1/teams", 4000, true, initialTeams);
  const failure = error ?? initialError ?? null;
  const totalMembers = (teams ?? []).reduce((a, t) => a + t.members.length, 0);

  return (
    <div>
      <div className="mb-6 flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Teams</h1>
          <p className="mt-1 text-sm text-secondary">{failure ? "Unavailable — teams could not be read." : `${teams?.length ?? 0} teams · ${totalMembers} members across the platform.`}</p>
        </div>
        <Link href="/teams" className="inline-flex items-center gap-1.5 rounded-md border border-border-strong px-3 py-1.5 text-sm hover:bg-subtle">
          Manage <ExternalLink className="h-3.5 w-3.5" />
        </Link>
      </div>

      <Table>
        <thead><tr><Th>Team</Th><Th>Slug</Th><Th>Plan</Th><Th>Members</Th><Th>Owner</Th><Th>Created</Th></tr></thead>
        <tbody>
          {(teams ?? []).map((t) => {
            const owner = t.members.find((m) => m.role === "owner");
            return (
              <tr key={t.slug}>
                <Td className="font-medium"><span className="flex items-center gap-2"><Users className="h-3.5 w-3.5 text-muted" />{t.name}</span></Td>
                <Td className="font-mono text-xs text-secondary">{t.slug}</Td>
                <Td><Badge tone="blue">{t.plan}</Badge></Td>
                <Td>{t.members.length}</Td>
                <Td className="text-secondary">{owner?.email ?? "—"}</Td>
                <Td className="text-secondary">{t.created_ms ? `${timeAgo(t.created_ms)}` : "—"}</Td>
              </tr>
            );
          })}
        </tbody>
      </Table>
      <OpsFetchError error={failure} label="teams" />
    </div>
  );
}
