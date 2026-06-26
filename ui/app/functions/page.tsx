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
          <tr><Th>Function</Th><Th>Instances</Th><Th>In-flight</Th><Th>Concurrency</Th><Th>Scale-outs</Th><Th>NACKs</Th><Th>Recycled</Th><Th>Requests</Th><Th>Active CPU</Th><Th>Memory</Th><Th>Fluid savings</Th><Th>Active-CPU savings</Th></tr>
        </thead>
        <tbody>
          {(fns ?? []).map((f) => (
            <tr key={f.key}>
              <Td className="font-mono text-xs">{f.key}</Td>
              <Td className="tabular-nums">{f.instances}</Td>
              <Td className="tabular-nums">{f.inflight}</Td>
              <Td className="tabular-nums">{f.safe_concurrency ?? f.max_concurrency}{(f.safe_concurrency ?? f.max_concurrency) !== f.max_concurrency ? ` / ${f.max_concurrency}` : ""}</Td>
              <Td className="tabular-nums">{f.scale_out_total ?? 0}</Td>
              <Td className="tabular-nums"><span title={`concurrency: ${f.nack_concurrency ?? 0} · quota: ${f.nack_quota ?? 0}`}>{f.nack_total ?? 0}</span></Td>
              <Td className="tabular-nums"><span title={`last cold start — provision ${f.last_provision_ms ?? 0}ms · runtime init ${f.last_runtime_init_ms ?? 0}ms`}>{f.recycled ?? 0}</span></Td>
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
