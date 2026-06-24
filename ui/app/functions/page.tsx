"use client";

import { Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type FunctionStats } from "@/lib/api";

export default function FunctionsPage() {
  const { data: fns } = usePoll<FunctionStats[]>("/v1/functions", 2000);
  return (
    <div>
      <PageHeader title="Functions" desc="Fluid compute — instances multiplex concurrent requests, scale to zero, and bill Active CPU + memory (not idle wall-time)" />
      <Table>
        <thead>
          <tr><Th>Function</Th><Th>Instances</Th><Th>In-flight</Th><Th>Concurrency</Th><Th>Requests</Th><Th>Active CPU</Th><Th>Memory</Th><Th>Fluid savings</Th><Th>Active-CPU savings</Th></tr>
        </thead>
        <tbody>
          {(fns ?? []).map((f) => (
            <tr key={f.key}>
              <Td className="font-mono text-xs">{f.key}</Td>
              <Td className="tabular-nums">{f.instances}</Td>
              <Td className="tabular-nums">{f.inflight}</Td>
              <Td className="tabular-nums">{f.max_concurrency}</Td>
              <Td className="tabular-nums">{f.requests}</Td>
              <Td className="tabular-nums">{((f.active_cpu_ms ?? 0) / 1000).toFixed(1)}s</Td>
              <Td className="tabular-nums">{(f.memory_gb_hrs ?? 0).toFixed(3)} GB-hr</Td>
              <Td><Badge tone={f.savings_pct > 0.4 ? "green" : "default"}>{Math.round(f.savings_pct * 100)}%</Badge></Td>
              <Td><Badge tone={(f.active_cpu_savings_pct ?? 0) > 0.4 ? "green" : "default"}>{Math.round((f.active_cpu_savings_pct ?? 0) * 100)}%</Badge></Td>
            </tr>
          ))}
          {!fns?.length && <tr><Td className="text-muted">No functions deployed yet.</Td></tr>}
        </tbody>
      </Table>
    </div>
  );
}
