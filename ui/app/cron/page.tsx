"use client";

import { useState } from "react";
import { Card, Badge, Button, Input, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type CronJob } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function CronPage() {
  const { data: jobs, refresh } = usePoll<CronJob[]>("/v1/cron", 2000);
  const [name, setName] = useState("");
  const [schedule, setSchedule] = useState("0 * * * * *");
  const [deployment, setDeployment] = useState("hello");
  const [path, setPath] = useState("/api/cron");
  const [err, setErr] = useState("");

  async function add() {
    setErr("");
    try {
      await apiSend("POST", "/v1/cron", {
        id: `cron-${Date.now()}`, name: name || "job", schedule, deployment, path,
        enabled: true, runs: 0,
      });
      setName("");
      refresh();
    } catch (e) { setErr(String(e)); }
  }

  return (
    <div>
      <PageHeader title="Cron Jobs" desc="Scheduled function invocations (6-field cron: sec min hour dom mon dow)" />
      <Card className="mb-4">
        <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
          <Input placeholder="name" value={name} onChange={(e) => setName(e.target.value)} />
          <Input placeholder="schedule" value={schedule} onChange={(e) => setSchedule(e.target.value)} />
          <Input placeholder="deployment (host)" value={deployment} onChange={(e) => setDeployment(e.target.value)} />
          <Input placeholder="path" value={path} onChange={(e) => setPath(e.target.value)} />
        </div>
        <div className="mt-3 flex items-center gap-3">
          <Button onClick={add}>Schedule job</Button>
          {err ? <span className="text-xs text-red-400">{err}</span> : null}
        </div>
      </Card>

      <Table>
        <thead><tr><Th>Name</Th><Th>Schedule</Th><Th>Target</Th><Th>Runs</Th><Th>Last</Th><Th></Th></tr></thead>
        <tbody>
          {(jobs ?? []).map((j) => (
            <tr key={j.id}>
              <Td>{j.name} {j.enabled ? <Badge tone="green">on</Badge> : <Badge>off</Badge>}{j.source === "vercel.json" && <Badge tone="blue">vercel.json</Badge>}</Td>
              <Td className="font-mono text-xs">{j.schedule}</Td>
              <Td className="font-mono text-xs text-muted">{j.deployment}{j.path}</Td>
              <Td className="tabular-nums">{j.runs}</Td>
              <Td className="text-muted">{j.last_run_ms ? timeAgo(j.last_run_ms) : "—"}</Td>
              <Td><Button variant="danger" onClick={async () => { await apiSend("DELETE", `/v1/cron/${j.id}`); refresh(); }}>Delete</Button></Td>
            </tr>
          ))}
          {!jobs?.length && <tr><Td className="text-muted">No cron jobs yet.</Td></tr>}
        </tbody>
      </Table>
    </div>
  );
}
