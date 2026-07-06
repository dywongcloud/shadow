"use client";

import Link from "next/link";
import { Card, Badge, Table, Th, Td } from "@/components/ui";
import { useOpsPoll, type Database, type DbKind } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

const kindTone: Record<DbKind, "blue" | "red" | "amber" | "green" | "default"> = {
  postgres: "blue", redis: "red", blob: "amber", queue: "green", vector: "default", pubsub: "blue", realtime: "green",
};

export default function AdminDatabasesPage() {
  // Platform-owner view → ALL databases across every tenant (global endpoint).
  const { data: dbs } = useOpsPoll<Database[]>("/v1/admin/databases", 3000);
  const active = (dbs ?? []).filter((d) => d.status === "ready").length;
  const live = (dbs ?? []).filter((d) => d.mode === "live").length;
  const byKind = (dbs ?? []).reduce<Record<string, number>>((a, d) => { a[d.kind] = (a[d.kind] ?? 0) + 1; return a; }, {});

  return (
    <div>
      <div className="mb-6">
        <h1 className="text-2xl font-semibold tracking-tight">Database Fleet</h1>
        <p className="mt-1 text-sm text-secondary">{dbs?.length ?? 0} provisioned · {active} active · {live} live · across all teams and projects.</p>
      </div>

      <div className="mb-6 flex flex-wrap gap-2">
        {Object.entries(byKind).map(([k, n]) => (
          <Badge key={k} tone={kindTone[k as DbKind] ?? "default"}>{k}: {n}</Badge>
        ))}
      </div>

      <Table>
        <thead><tr><Th>Name</Th><Th>Type</Th><Th>Project</Th><Th>Team</Th><Th>Region</Th><Th>Mode</Th><Th>Status</Th><Th>Age</Th></tr></thead>
        <tbody>
          {(dbs ?? []).map((d) => (
            <tr key={d.id} className="hover:bg-subtle">
              <Td className="font-medium"><Link href={`/storage/${d.id}`}>{d.name}</Link></Td>
              <Td><Badge tone={kindTone[d.kind]}>{d.kind}</Badge></Td>
              <Td className="text-secondary">{d.project || "—"}</Td>
              <Td className="text-secondary">{d.team || "—"}</Td>
              <Td><Badge tone="blue">{d.region}</Badge></Td>
              <Td className="text-secondary">{d.mode}</Td>
              <Td><Badge tone={d.status === "ready" ? "green" : d.status === "error" ? "red" : "amber"}>{d.status}</Badge></Td>
              <Td className="text-secondary">{timeAgo(d.created_ms)}</Td>
            </tr>
          ))}
        </tbody>
      </Table>
      {!dbs?.length && <Card className="mt-4 py-12 text-center text-sm text-secondary">No databases provisioned.</Card>}
    </div>
  );
}
