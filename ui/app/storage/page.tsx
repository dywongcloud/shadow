"use client";

import { useState } from "react";
import Link from "next/link";
import { Database as DbIcon, Plus, X, Loader2, HardDrive, ListOrdered, Boxes, Zap, Radio, Wifi } from "lucide-react";
import { Card, Badge, Button, Input, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type Database, type DbKind } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

const KINDS: { kind: DbKind; name: string; tag: string; desc: string; icon: React.ReactNode }[] = [
  { kind: "postgres", name: "Postgres", tag: "Serverless SQL", desc: "Managed Postgres — instant, branchable, Prisma-ready.", icon: <DbIcon className="h-5 w-5" /> },
  { kind: "redis", name: "Redis", tag: "Key-Value", desc: "Durable Redis with TCP + REST, Upstash-style.", icon: <Zap className="h-5 w-5" /> },
  { kind: "blob", name: "Blob", tag: "Object Storage", desc: "S3-compatible object storage for files & assets.", icon: <HardDrive className="h-5 w-5" /> },
  { kind: "queue", name: "Queue", tag: "Messaging", desc: "Durable FIFO message queue for background jobs.", icon: <ListOrdered className="h-5 w-5" /> },
  { kind: "vector", name: "Vector", tag: "AI / Embeddings", desc: "Vector index with cosine search for AI apps.", icon: <Boxes className="h-5 w-5" /> },
  { kind: "pubsub", name: "Pub/Sub", tag: "Messaging", desc: "Topic-based publish/subscribe fan-out (RabbitMQ-style).", icon: <Radio className="h-5 w-5" /> },
  { kind: "realtime", name: "Realtime", tag: "Streaming", desc: "Secure WebSocket channels for live, bidirectional apps.", icon: <Wifi className="h-5 w-5" /> },
];

const KIND_META = Object.fromEntries(KINDS.map((k) => [k.kind, k]));

function kindBadge(kind: DbKind) {
  const tone = { postgres: "blue", redis: "red", blob: "amber", queue: "green", vector: "default", pubsub: "blue", realtime: "green" } as const;
  return <Badge tone={tone[kind] ?? "default"}>{KIND_META[kind]?.name ?? kind}</Badge>;
}

export default function StoragePage() {
  const { data: dbs, refresh } = usePoll<Database[]>("/v1/databases", 3000);
  const [open, setOpen] = useState(false);

  return (
    <div>
      <PageHeader
        title="Storage"
        desc="Serverless databases provisioned per project — Postgres, Redis, Blob, Queue and Vector. Pay for what you use."
        action={<Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Create Database</Button>}
      />

      {!dbs?.length ? (
        <Card className="flex flex-col items-center gap-3 py-16 text-center">
          <DbIcon className="h-8 w-8 text-muted" />
          <div className="text-sm font-medium">No databases yet</div>
          <p className="max-w-sm text-sm text-secondary">Provision a serverless database and connect it to a project. Credentials are generated automatically.</p>
          <Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Create Database</Button>
        </Card>
      ) : (
        <Table>
          <thead><tr><Th>Name</Th><Th>Type</Th><Th>Project</Th><Th>Region</Th><Th>Status</Th><Th>Created</Th></tr></thead>
          <tbody>
            {dbs.map((d) => (
              <tr key={d.id} className="cursor-pointer hover:bg-subtle">
                <Td className="font-medium">
                  <Link href={`/storage/${d.id}`} className="flex items-center gap-2">
                    <span className="text-muted">{KIND_META[d.kind]?.icon}</span>{d.name}
                  </Link>
                </Td>
                <Td>{kindBadge(d.kind)}</Td>
                <Td className="text-secondary">{d.project || "—"}</Td>
                <Td><Badge tone="blue">{d.region}</Badge></Td>
                <Td><StatusBadge status={d.status} mode={d.mode} /></Td>
                <Td className="text-secondary">{timeAgo(d.created_ms)} ago</Td>
              </tr>
            ))}
          </tbody>
        </Table>
      )}

      {open && <CreateModal onClose={() => setOpen(false)} onCreated={() => { setOpen(false); refresh(); }} />}
    </div>
  );
}

function StatusBadge({ status, mode }: { status: string; mode: string }) {
  if (status === "provisioning") return <Badge tone="amber"><Loader2 className="h-3 w-3 animate-spin" /> Provisioning</Badge>;
  if (status === "error") return <Badge tone="red">Error</Badge>;
  return <Badge tone="green">Ready{mode === "simulated" ? " · sim" : ""}</Badge>;
}

function CreateModal({ onClose, onCreated }: { onClose: () => void; onCreated: () => void }) {
  const [kind, setKind] = useState<DbKind>("postgres");
  const [name, setName] = useState("");
  const [project, setProject] = useState("");
  const [region, setRegion] = useState("iad1");
  const [busy, setBusy] = useState(false);

  async function create() {
    setBusy(true);
    try {
      await apiSend("POST", "/v1/databases", {
        name: name || `${kind}-db`,
        project: project || "default",
        kind,
        region,
      });
      onCreated();
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <div className="w-full max-w-lg rounded-xl border border-border bg-card p-6 shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="mb-4 flex items-center justify-between">
          <h2 className="text-lg font-semibold">Create Database</h2>
          <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
        </div>
        <div className="mb-4 grid grid-cols-2 gap-2 sm:grid-cols-3">
          {KINDS.map((k) => (
            <button
              key={k.kind}
              onClick={() => setKind(k.kind)}
              className={`flex flex-col items-start gap-1 rounded-lg border p-3 text-left transition-colors ${kind === k.kind ? "border-fg bg-subtle" : "border-border hover:bg-subtle"}`}
            >
              <span className="text-muted">{k.icon}</span>
              <span className="text-sm font-medium">{k.name}</span>
              <span className="text-[11px] text-muted">{k.tag}</span>
            </button>
          ))}
        </div>
        <p className="mb-4 text-xs text-secondary">{KIND_META[kind]?.desc}</p>
        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Name</label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder={`${kind}-db`} />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Project</label>
              <Input value={project} onChange={(e) => setProject(e.target.value)} placeholder="my-project" />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Region</label>
              <Input value={region} onChange={(e) => setRegion(e.target.value)} placeholder="iad1" />
            </div>
          </div>
        </div>
        <div className="mt-6 flex justify-end gap-2">
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button onClick={create} disabled={busy}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />} Create</Button>
        </div>
      </div>
    </div>
  );
}
