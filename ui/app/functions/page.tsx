"use client";

import { Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type FunctionStats } from "@/lib/api";

export default function FunctionsPage() {
  const { data: fns } = usePoll<FunctionStats[]>("/v1/functions", 2000);
  return (
    <div>
      <PageHeader title="Functions" desc="Fluid compute — instances multiplex concurrent requests, scale to zero" />
      <Table>
        <thead>
          <tr><Th>Function</Th><Th>Instances</Th><Th>In-flight</Th><Th>Concurrency</Th><Th>Requests</Th><Th>Active-CPU savings</Th></tr>
        </thead>
        <tbody>
          {(fns ?? []).map((f) => (
            <tr key={f.key}>
              <Td className="font-mono text-xs">{f.key}</Td>
              <Td className="tabular-nums">{f.instances}</Td>
              <Td className="tabular-nums">{f.inflight}</Td>
              <Td className="tabular-nums">{f.max_concurrency}</Td>
              <Td className="tabular-nums">{f.requests}</Td>
              <Td><Badge tone={f.savings_pct > 0.4 ? "green" : "default"}>{Math.round(f.savings_pct * 100)}%</Badge></Td>
            </tr>
          ))}
          {!fns?.length && <tr><Td className="text-muted">No functions deployed yet.</Td></tr>}
        </tbody>
      </Table>
    </div>
  );
}
