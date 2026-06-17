"use client";

import { useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { ArrowLeft, Copy, Eye, EyeOff, Trash2, Check } from "lucide-react";
import { Card, Badge, Button, PageHeader } from "@/components/ui";
import { apiGet, apiSend, usePoll, type Database } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function DatabaseDetail({ params }: { params: { id: string } }) {
  const id = params.id;
  const router = useRouter();
  const { data: db } = usePoll<Database>(`/v1/databases/${id}`, 3000);
  const [revealed, setRevealed] = useState<Record<string, string> | null>(null);
  const [copied, setCopied] = useState("");

  async function reveal() {
    const full = await apiGet<Database>(`/v1/databases/${id}/credentials`);
    setRevealed(full.connection);
  }
  function copy(k: string, v: string) {
    navigator.clipboard.writeText(v);
    setCopied(k);
    setTimeout(() => setCopied(""), 1200);
  }
  async function destroy() {
    if (!confirm(`Delete ${db?.name}? This tears down the backing service.`)) return;
    await apiSend("DELETE", `/v1/databases/${id}`);
    router.push("/storage");
  }

  if (!db) return <div className="text-sm text-secondary">Loading…</div>;
  const conn = revealed ?? db.connection;

  return (
    <div>
      <Link href="/storage" className="mb-4 inline-flex items-center gap-1 text-sm text-secondary hover:text-fg"><ArrowLeft className="h-4 w-4" /> Storage</Link>
      <PageHeader
        title={db.name}
        desc={`${db.kind.toUpperCase()} · ${db.region} · ${db.project || "no project"}`}
        action={
          <div className="flex items-center gap-2">
            {db.status === "ready" ? <Badge tone="green">Ready{db.mode === "simulated" ? " · simulated" : " · live"}</Badge> : <Badge tone="amber">{db.status}</Badge>}
            <Button variant="danger" onClick={destroy}><Trash2 className="h-4 w-4" /> Delete</Button>
          </div>
        }
      />

      <div className="mb-6 grid grid-cols-2 gap-4 sm:grid-cols-4">
        <Mini label="Type" value={db.kind} />
        <Mini label="Region" value={db.region} />
        <Mini label="Mode" value={db.mode} />
        <Mini label="Created" value={`${timeAgo(db.created_ms)} ago`} />
      </div>

      <Card>
        <div className="mb-4 flex items-center justify-between">
          <h3 className="text-sm font-semibold">Connection</h3>
          <Button variant="outline" onClick={revealed ? () => setRevealed(null) : reveal}>
            {revealed ? <><EyeOff className="h-4 w-4" /> Hide</> : <><Eye className="h-4 w-4" /> Reveal secrets</>}
          </Button>
        </div>
        <div className="divide-y divide-border">
          {Object.entries(conn).map(([k, v]) => (
            <div key={k} className="flex items-center gap-3 py-2.5">
              <div className="w-48 shrink-0 font-mono text-xs text-secondary">{k}</div>
              <div className="min-w-0 flex-1 truncate font-mono text-xs text-fg">{v}</div>
              <button onClick={() => copy(k, v)} className="shrink-0 text-muted hover:text-fg">
                {copied === k ? <Check className="h-3.5 w-3.5 text-green" /> : <Copy className="h-3.5 w-3.5" />}
              </button>
            </div>
          ))}
        </div>
        {db.container && <p className="mt-3 text-xs text-muted">Backed by container <code className="font-mono">{db.container}</code></p>}
      </Card>

      {db.note && <Card className="mt-4 text-sm text-secondary">{db.note}</Card>}
    </div>
  );
}

function Mini({ label, value }: { label: string; value: string }) {
  return (
    <Card className="flex flex-col gap-1">
      <span className="text-xs font-medium uppercase tracking-wide text-muted">{label}</span>
      <span className="truncate text-sm font-medium text-fg">{value}</span>
    </Card>
  );
}
