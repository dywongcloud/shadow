"use client";

import { useState } from "react";
import { Card, Badge, Button, Input, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

interface WorkflowDef { id: string; name: string; steps: { name: string; deployment: string; path: string }[] }
interface StepRun { name: string; status: string; output: string }
interface WorkflowRun { id: string; def_id: string; status: string; steps: StepRun[]; started_ms: number }

export default function WorkflowsPage() {
  const { data: defs, refresh } = usePoll<WorkflowDef[]>("/v1/workflows", 3000);
  const { data: runs } = usePoll<WorkflowRun[]>("/v1/workflows/runs", 1500);
  const [name, setName] = useState("pipeline");
  const [deployment, setDeployment] = useState("hello");
  const [steps, setSteps] = useState("/api/hello,/api/cached");

  async function define() {
    const def: WorkflowDef = {
      id: `wf-${Date.now()}`,
      name,
      steps: steps.split(",").map((p, i) => ({ name: `step${i + 1}`, deployment, path: p.trim() })),
    };
    await apiSend("POST", "/v1/workflows", def);
    refresh();
  }

  function tone(s: string) {
    return s === "succeeded" ? "green" : s === "failed" ? "red" : "amber";
  }

  return (
    <div>
      <PageHeader title="Workflows" desc="Durable, multi-step orchestration over your functions" />
      <Card className="mb-4">
        <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
          <Input placeholder="name" value={name} onChange={(e) => setName(e.target.value)} />
          <Input placeholder="deployment" value={deployment} onChange={(e) => setDeployment(e.target.value)} />
          <Input placeholder="step paths, comma-separated" value={steps} onChange={(e) => setSteps(e.target.value)} />
        </div>
        <div className="mt-3"><Button onClick={define}>Define workflow</Button></div>
      </Card>

      <div className="mb-2 text-sm font-medium text-fg">Definitions</div>
      <Table>
        <thead><tr><Th>Name</Th><Th>Steps</Th><Th></Th></tr></thead>
        <tbody>
          {(defs ?? []).map((d) => (
            <tr key={d.id}>
              <Td>{d.name} <span className="font-mono text-xs text-muted">{d.id}</span></Td>
              <Td className="font-mono text-xs text-muted">{d.steps.map((s) => s.path).join(" → ")}</Td>
              <Td><Button onClick={() => apiSend("POST", `/v1/workflows/${d.id}/run`)}>Run</Button></Td>
            </tr>
          ))}
          {!defs?.length && <tr><Td className="text-muted">No workflows defined.</Td></tr>}
        </tbody>
      </Table>

      <div className="mb-2 mt-6 text-sm font-medium text-fg">Runs</div>
      <Table>
        <thead><tr><Th>Run</Th><Th>Status</Th><Th>Steps</Th><Th>Started</Th></tr></thead>
        <tbody>
          {(runs ?? []).map((r) => (
            <tr key={r.id}>
              <Td className="font-mono text-xs">{r.id}</Td>
              <Td><Badge tone={tone(r.status) as any}>{r.status}</Badge></Td>
              <Td className="text-xs">
                {r.steps.map((s, i) => (
                  <span key={i} className="mr-1"><Badge tone={tone(s.status) as any}>{s.name}</Badge></span>
                ))}
              </Td>
              <Td className="text-muted">{timeAgo(r.started_ms)}</Td>
            </tr>
          ))}
          {!runs?.length && <tr><Td className="text-muted">No runs yet.</Td></tr>}
        </tbody>
      </Table>
    </div>
  );
}
