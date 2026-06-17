"use client";

import { useState } from "react";
import { Card, Badge, Button, PageHeader } from "@/components/ui";
import { apiSend } from "@/lib/api";

interface SandboxResp {
  job_id: string;
  state: string;
  exit_code: number | null;
  logs: string[];
  duration_ms: number;
}

export default function SandboxPage() {
  const [code, setCode] = useState("echo 'hello from a Hive sandbox'\nuname -a\npython3 -c 'print(2**16)'");
  const [resp, setResp] = useState<SandboxResp | null>(null);
  const [running, setRunning] = useState(false);

  async function run() {
    setRunning(true);
    setResp(null);
    try {
      const commands = code.split("\n").map((l) => l.trim()).filter(Boolean);
      const r = await apiSend<SandboxResp>("POST", "/v1/sandbox", { commands });
      setResp(r);
    } finally {
      setRunning(false);
    }
  }

  return (
    <div>
      <PageHeader title="Sandbox" desc="Run untrusted code in an isolated, single-use cell" />
      <Card className="mb-4">
        <textarea
          value={code}
          onChange={(e) => setCode(e.target.value)}
          spellCheck={false}
          className="h-44 w-full resize-none rounded-md border border-border bg-neutral-950 p-3 font-mono text-sm text-neutral-100 focus:outline-none focus:ring-2 focus:ring-border"
        />
        <div className="mt-3">
          <Button onClick={run} disabled={running}>{running ? "Running…" : "Run in sandbox"}</Button>
        </div>
      </Card>

      {resp && (
        <Card>
          <div className="mb-3 flex items-center gap-3">
            <Badge tone={resp.exit_code === 0 ? "green" : "red"}>{resp.state}</Badge>
            <span className="text-xs text-muted">exit {resp.exit_code} · {resp.duration_ms}ms · {resp.job_id}</span>
          </div>
          <pre className="max-h-96 overflow-auto rounded-md border border-border bg-neutral-950 p-3 font-mono text-xs text-neutral-100">
{resp.logs.join("\n")}
          </pre>
        </Card>
      )}
    </div>
  );
}
