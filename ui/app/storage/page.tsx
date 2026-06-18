"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import {
  Database as DbIcon, Plus, X, Loader2, HardDrive, ListOrdered, Boxes, Zap, Radio, Wifi,
  ChevronRight, ArrowLeft, Search, ExternalLink, Gauge,
} from "lucide-react";
import { Card, Badge, Button, Input, PageHeader, Table, Th, Td } from "@/components/ui";
import { BrandIcon, BlobIcon, nativePng } from "@/components/provider-icons";
import { apiSend, usePoll, type Database, type DbKind } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

const KIND_ICON: Record<DbKind, React.ReactNode> = {
  postgres: <DbIcon className="h-5 w-5" />,
  redis: <Zap className="h-5 w-5" />,
  blob: <HardDrive className="h-5 w-5" />,
  queue: <ListOrdered className="h-5 w-5" />,
  vector: <Boxes className="h-5 w-5" />,
  pubsub: <Radio className="h-5 w-5" />,
  realtime: <Wifi className="h-5 w-5" />,
};

const KIND_NAME: Record<DbKind, string> = {
  postgres: "Postgres", redis: "Redis", blob: "Blob", queue: "Queue",
  vector: "Vector", pubsub: "Pub/Sub", realtime: "Realtime",
};

function kindBadge(kind: DbKind) {
  const tone = { postgres: "blue", redis: "red", blob: "amber", queue: "green", vector: "default", pubsub: "blue", realtime: "green" } as const;
  return <Badge tone={tone[kind] ?? "default"}>{KIND_NAME[kind] ?? kind}</Badge>;
}

// OpenEdge-native primitives.
const NATIVE: { kind: DbKind; name: string; desc: string }[] = [
  { kind: "postgres", name: "Postgres", desc: "Managed Postgres — instant, branchable." },
  { kind: "redis", name: "Redis", desc: "Durable key-value, TCP + REST." },
  { kind: "blob", name: "Blob", desc: "Fast S3-compatible object storage." },
  { kind: "queue", name: "Queue", desc: "Durable FIFO message queue." },
  { kind: "vector", name: "Vector", desc: "Embeddings index with cosine search." },
  { kind: "pubsub", name: "Pub/Sub", desc: "Topic-based fan-out (RabbitMQ-style)." },
  { kind: "realtime", name: "Realtime", desc: "Secure WebSocket streaming channels." },
];

// Marketplace providers → backed by a native kind.
interface Provider { id: string; name: string; desc: string; kind: DbKind; color: string }
const MARKETPLACE: Provider[] = [
  { id: "neon", name: "Neon", desc: "Serverless Postgres", kind: "postgres", color: "#00e599" },
  { id: "upstash", name: "Upstash", desc: "Serverless DB (Redis, Vector, Queue, Search)", kind: "redis", color: "#00c98d" },
  { id: "redis", name: "Redis", desc: "Official Redis", kind: "redis", color: "#d82c20" },
  { id: "aws", name: "AWS", desc: "Serverless, reliable, secure AWS services", kind: "blob", color: "#ff9900" },
  { id: "supabase", name: "Supabase", desc: "Postgres backend", kind: "postgres", color: "#3ecf8e" },
  { id: "nile", name: "Nile", desc: "Postgres re-engineered for B2B", kind: "postgres", color: "#5b8def" },
  { id: "motherduck", name: "MotherDuck", desc: "Analytics database", kind: "postgres", color: "#ff7a45" },
  { id: "convex", name: "Convex", desc: "Reactive database", kind: "postgres", color: "#f3336b" },
  { id: "prisma", name: "Prisma Postgres", desc: "Instant Serverless Postgres", kind: "postgres", color: "#5a67d8" },
  { id: "turso", name: "Turso", desc: "Serverless SQLite", kind: "postgres", color: "#4ff8d2" },
  { id: "drizzle", name: "Drizzle", desc: "TypeScript ORM (Postgres)", kind: "postgres", color: "#c5f74f" },
  { id: "mongodb", name: "MongoDB Atlas", desc: "Database for developers", kind: "blob", color: "#13aa52" },
];

export default function StoragePage() {
  const { data: dbs, refresh } = usePoll<Database[]>("/v1/databases", 3000);
  const [open, setOpen] = useState(false);
  // Deep-link: /storage?browse opens the Browse Storage panel directly.
  useEffect(() => {
    if (typeof window !== "undefined" && new URLSearchParams(window.location.search).has("browse")) {
      setOpen(true);
    }
  }, []);

  return (
    <div className="pb-24">
      <PageHeader
        title="Storage"
        desc="Serverless databases provisioned per project — connect them to your projects."
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
          <thead><tr><Th>Name</Th><Th>Provider</Th><Th>Type</Th><Th>Project</Th><Th>Region</Th><Th>Status</Th><Th>Created</Th></tr></thead>
          <tbody>
            {dbs.map((d) => (
              <tr key={d.id} className="cursor-pointer hover:bg-subtle">
                <Td className="font-medium">
                  <Link href={`/storage/${d.id}`} className="flex items-center gap-2">
                    <span className="text-muted">{KIND_ICON[d.kind]}</span>{d.name}
                  </Link>
                </Td>
                <Td className="text-secondary">{d.provider || KIND_NAME[d.kind]}</Td>
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

      {open && <BrowseStorage onClose={() => setOpen(false)} onCreated={() => { setOpen(false); refresh(); }} />}
    </div>
  );
}

function StatusBadge({ status, mode }: { status: string; mode: string }) {
  if (status === "provisioning") return <Badge tone="amber"><Loader2 className="h-3 w-3 animate-spin" /> Provisioning</Badge>;
  if (status === "error") return <Badge tone="red">Error</Badge>;
  return <Badge tone="green">Ready{mode === "simulated" ? " · sim" : ""}</Badge>;
}


/** Vercel-style "Browse Storage" right-side slide-over. */
function BrowseStorage({ onClose, onCreated }: { onClose: () => void; onCreated: () => void }) {
  const [q, setQ] = useState("");
  const [sel, setSel] = useState<{ kind: DbKind; provider: string } | null>(null);
  const [step, setStep] = useState<"browse" | "configure">("browse");
  // configure step
  const [name, setName] = useState("");
  const [project, setProject] = useState("");
  const [region, setRegion] = useState("iad1");
  const [busy, setBusy] = useState(false);

  const ql = q.toLowerCase();
  const native = NATIVE.filter((n) => !ql || n.name.toLowerCase().includes(ql) || n.desc.toLowerCase().includes(ql));
  const market = MARKETPLACE.filter((m) => !ql || m.name.toLowerCase().includes(ql) || m.desc.toLowerCase().includes(ql));

  async function create() {
    if (!sel) return;
    setBusy(true);
    try {
      await apiSend("POST", "/v1/databases", {
        name: name || `${sel.provider.toLowerCase().replace(/[^a-z0-9]+/g, "-")}-db`,
        project: project || "default",
        kind: sel.kind,
        region,
        provider: sel.provider,
      });
      onCreated();
    } catch (e) { alert(String(e)); setBusy(false); }
  }

  return (
    <div className="fixed inset-0 z-50 flex justify-end bg-black/40" onClick={onClose}>
      <div className="flex h-full w-full max-w-xl flex-col border-l border-border bg-card shadow-pop" onClick={(e) => e.stopPropagation()}>
        {/* header */}
        <div className="flex items-start justify-between border-b border-border p-6">
          <div>
            <h2 className="text-xl font-semibold">{step === "browse" ? "Browse Storage" : "Configure"}</h2>
            <p className="mt-1 text-sm text-secondary">
              {step === "browse" ? "Create databases and stores that you can connect to your projects." : `${sel?.provider} · backed by OpenEdge ${sel ? KIND_NAME[sel.kind] : ""}`}
            </p>
          </div>
          <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-5 w-5" /></button>
        </div>

        {/* body */}
        <div className="flex-1 overflow-y-auto p-6">
          {step === "browse" ? (
            <>
              <div className="relative mb-5">
                <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
                <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search provider or database…"
                  className="w-full rounded-lg border border-border bg-bg py-2.5 pl-9 pr-3 text-sm focus:outline-none focus:ring-2 focus:ring-border" />
              </div>

              {!!native.length && (
                <>
                  <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">OpenEdge Native</div>
                  <div className="mb-6 flex flex-col gap-2">
                    {native.map((n) => (
                      <Row key={n.kind}
                        icon={nativePng(n.kind)
                          ? <span className="flex h-9 w-9 items-center justify-center overflow-hidden rounded-lg bg-white/5">{/* eslint-disable-next-line @next/next/no-img-element */}<img src={nativePng(n.kind)!} alt={n.name} className="h-7 w-7 object-contain" /></span>
                          : n.kind === "blob"
                          ? <span className="flex h-9 w-9 items-center justify-center"><BlobIcon /></span>
                          : <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary">{KIND_ICON[n.kind]}</span>}
                        name={n.name} desc={n.desc}
                        active={sel?.provider === n.name}
                        onClick={() => setSel({ kind: n.kind, provider: n.name })}
                      />
                    ))}
                  </div>
                </>
              )}

              {!!market.length && (
                <>
                  <div className="mb-2 flex items-center justify-between">
                    <span className="text-xs font-medium uppercase tracking-wide text-muted">Marketplace Database Providers</span>
                    <span className="flex items-center gap-1 text-xs text-link">Learn more <ExternalLink className="h-3 w-3" /></span>
                  </div>
                  <div className="flex flex-col gap-2">
                    {market.map((m) => (
                      <Row key={m.id}
                        icon={<BrandIcon id={m.id} name={m.name} color={m.color} />}
                        name={m.name} desc={m.desc}
                        active={sel?.provider === m.name}
                        onClick={() => setSel({ kind: m.kind, provider: m.name })}
                      />
                    ))}
                  </div>
                </>
              )}
            </>
          ) : (
            <div className="space-y-4">
              <div className="flex items-center gap-3 rounded-lg border border-border bg-subtle p-3">
                <span className="text-secondary">{sel && KIND_ICON[sel.kind]}</span>
                <div>
                  <div className="text-sm font-medium">{sel?.provider}</div>
                  <div className="text-xs text-secondary">Backed by OpenEdge {sel ? KIND_NAME[sel.kind] : ""}</div>
                </div>
              </div>
              <div>
                <label className="mb-1 block text-xs font-medium text-secondary">Database name</label>
                <Input value={name} onChange={(e) => setName(e.target.value)} placeholder={`${sel?.provider.toLowerCase()}-db`} />
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
              <p className="flex items-center gap-1.5 text-xs text-muted"><Gauge className="h-3.5 w-3.5" /> Credentials are generated automatically and injectable into the project.</p>
            </div>
          )}
        </div>

        {/* footer */}
        <div className="flex items-center justify-between border-t border-border p-4">
          {step === "configure"
            ? <Button variant="ghost" onClick={() => setStep("browse")}><ArrowLeft className="h-4 w-4" /> Back</Button>
            : <span className="text-xs text-muted">{sel ? `Selected: ${sel.provider}` : "Select a database or provider"}</span>}
          <div className="flex gap-2">
            <Button variant="outline" onClick={onClose}>Cancel</Button>
            {step === "browse" ? (
              <Button onClick={() => setStep("configure")} disabled={!sel}>Continue <ChevronRight className="h-4 w-4" /></Button>
            ) : (
              <Button onClick={create} disabled={busy}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />} Create</Button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function Row({ icon, name, desc, active, onClick }: { icon: React.ReactNode; name: string; desc: string; active: boolean; onClick: () => void }) {
  return (
    <button onClick={onClick} className={`flex w-full items-center gap-3 rounded-xl border px-4 py-3 text-left transition-colors ${active ? "border-fg bg-subtle" : "border-border hover:bg-subtle"}`}>
      {icon}
      <div className="min-w-0 flex-1">
        <div className="text-sm font-semibold">{name}</div>
        <div className="truncate text-xs text-secondary">{desc}</div>
      </div>
      <ChevronRight className="h-4 w-4 shrink-0 text-muted" />
    </button>
  );
}
