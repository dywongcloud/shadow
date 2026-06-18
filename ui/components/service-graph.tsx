"use client";

import { useEffect, useRef, useState, useCallback } from "react";
import ReactFlow, {
  Background, BackgroundVariant, Controls, Handle, Position, MiniMap,
  useNodesState, useEdgesState, type Node, type Edge, type ReactFlowInstance,
} from "reactflow";
import "reactflow/dist/style.css";
import Link from "next/link";
import { useTheme } from "next-themes";
import {
  Shield, Globe, Boxes, Database as DbIcon, Terminal, ScrollText, Settings2,
  Container, FileCode, Zap, Lock, Plus, HardDrive, Server,
} from "lucide-react";
import {
  usePoll, apiSend, type Deployment, type Overview, type Database, type SecureLink,
  type FunctionStats, type ProjectSettings,
} from "@/lib/api";

interface Cdn { hits: number; entries: number }
interface Waf { managed: boolean }
interface RateLimit { enabled: boolean }

const REGION_LABEL: Record<string, string> = {
  iad1: "Washington, D.C., USA (us-east-1)",
  sfo1: "San Francisco, USA (us-west-1)",
  fra1: "Frankfurt, Germany (eu-central-1)",
  hnd1: "Tokyo, Japan (ap-northeast-1)",
};

const PROVIDER_PNG = new Set(["upstash", "redis", "aws", "supabase", "nile", "motherduck", "convex", "drizzle", "turso", "prisma", "postgres"]);
function providerImg(provider?: string): string | null {
  const id = (provider || "").toLowerCase().replace(/\s.*/, "");
  return PROVIDER_PNG.has(id) ? `/providers/${id}.png` : null;
}

const handleStyle = { width: 8, height: 8, background: "var(--rf-handle, #888)", border: "2px solid hsl(var(--card))" };

/* ---- db / env helpers ---- */
type AddKind = "postgres" | "redis" | "blob";

/** The conventional env var name a given db kind populates. */
function envKeyForKind(kind?: string): string | null {
  switch (kind) {
    case "postgres": return "DATABASE_URL";
    case "redis": return "REDIS_URL";
    case "blob": return "BLOB_READ_WRITE_TOKEN";
    case "vector": return "VECTOR_URL";
    case "queue": return "QUEUE_URL";
    default: return null;
  }
}

/** Build a connection string defensively from a Database.connection map. */
function connString(db: Database): string {
  const c = db.connection || {};
  if (c.url) return c.url;
  if (c.token) return c.token; // e.g. blob read/write token
  const host = c.host, port = c.port, user = c.user ?? c.username, pass = c.password ?? c.pass, name = c.database ?? c.db;
  if (host) {
    const scheme = db.kind === "redis" ? "redis" : db.kind === "postgres" ? "postgresql" : "db";
    const auth = user ? `${user}${pass ? `:${pass}` : ""}@` : "";
    const portPart = port ? `:${port}` : "";
    const dbPart = name ? `/${name}` : "";
    return `${scheme}://${auth}${host}${portPart}${dbPart}`;
  }
  // Nothing usable — reference the db by id so the platform can resolve it.
  return `@${db.id}`;
}

/* ---- custom nodes ---- */
function IngressNode({ data }: { data: any }) {
  return (
    <div className="w-[230px] rounded-xl border border-border bg-card p-3 shadow-card">
      <div className="mb-2 flex items-center gap-2 border-b border-border pb-2 text-sm font-semibold">
        <span className="flex h-5 w-5 items-center justify-center rounded bg-fg text-bg">
          <svg width="9" height="8" viewBox="0 0 24 22" aria-hidden><path d="M12 0 L24 22 L0 22 Z" fill="currentColor" /></svg>
        </span>
        OpenEdge Ingress
      </div>
      <StatRow icon={<Shield className="h-3.5 w-3.5" />} label="DDoS protection" on={data.ddos} />
      <StatRow icon={<Globe className="h-3.5 w-3.5" />} label="CDN" on={data.cdn} />
      <StatRow icon={<Zap className="h-3.5 w-3.5" />} label="Edge caching" on={data.cdn} />
      <Handle type="source" id="out" position={Position.Right} style={handleStyle} />
    </div>
  );
}

function ServiceNode({ data }: { data: any }) {
  const Icon = data.kind === "container" ? Container : data.kind === "static" ? FileCode : Globe;
  return (
    <div className="w-[320px] rounded-xl border border-border bg-card p-3 shadow-card">
      <div className="mb-2 flex items-center gap-2 text-sm font-semibold"><Boxes className="h-4 w-4" /> {data.project}</div>
      <div className="rounded-lg border border-border">
        <div className="flex items-center justify-between border-b border-border px-3 py-2 text-sm">
          <span className="flex items-center gap-2 font-medium"><Icon className="h-4 w-4" /> {data.procLabel}</span>
          <span className="flex items-center gap-1.5 text-xs">
            <span className={`h-2 w-2 rounded-full ${data.state === "ready" ? "bg-green" : data.state === "building" ? "bg-amber-400" : "bg-red-400"}`} />
            {data.state ? data.state[0].toUpperCase() + data.state.slice(1) : "—"}
          </span>
        </div>
        <KV k="Start command" v={data.startCmd} />
        <KV k="Resources" v={data.resources} />
        <KV k="Instances" v={String(data.instances)} />
        <KV k="Domain" v={data.domain} mono />
      </div>
      <div className="mt-2 flex items-center justify-around text-xs text-secondary">
        <Link href={data.logsHref} className="flex items-center gap-1.5 hover:text-fg"><ScrollText className="h-3.5 w-3.5" /> Logs</Link>
        <Link href="/sandbox" className="flex items-center gap-1.5 hover:text-fg"><Terminal className="h-3.5 w-3.5" /> Terminal</Link>
        <Link href={data.settingsHref} className="flex items-center gap-1.5 hover:text-fg"><Settings2 className="h-3.5 w-3.5" /> Settings</Link>
      </div>
      <Handle type="target" id="in" position={Position.Left} style={handleStyle} />
      <Handle type="source" id="svc-out" position={Position.Right} style={handleStyle} />
    </div>
  );
}

function ResourceNode({ data }: { data: any }) {
  return (
    <div className="flex w-[220px] items-center gap-2 rounded-xl border border-border bg-card px-3 py-2.5 shadow-card">
      {data.img ? (
        // eslint-disable-next-line @next/next/no-img-element
        <span className="flex h-7 w-7 items-center justify-center overflow-hidden rounded-md bg-white/5"><img src={data.img} alt="" className="h-5 w-5 object-contain" /></span>
      ) : data.tunnel ? (
        <Lock className="h-4 w-4 text-muted" />
      ) : (
        <DbIcon className="h-4 w-4 text-muted" />
      )}
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm font-medium">{data.name}</div>
        <div className="truncate text-[11px] text-muted">{data.sub}</div>
      </div>
      {data.envKey ? (
        <span
          title={`Populates ${data.envKey}`}
          className="shrink-0 rounded border border-border bg-bg px-1.5 py-0.5 font-mono text-[9px] leading-tight text-secondary"
        >
          {data.envKey}
        </span>
      ) : null}
      <Handle type="target" id="in" position={Position.Left} style={handleStyle} />
    </div>
  );
}

const ADDER_OPTIONS: { kind: AddKind; label: string; provider: string; suffix: string; icon: React.ReactNode }[] = [
  { kind: "postgres", label: "Database", provider: "OpenEdge Postgres", suffix: "db", icon: <DbIcon className="h-4 w-4" /> },
  { kind: "redis", label: "Cache", provider: "OpenEdge KV", suffix: "cache", icon: <Server className="h-4 w-4" /> },
  { kind: "blob", label: "Blob store", provider: "OpenEdge Blob", suffix: "blob", icon: <HardDrive className="h-4 w-4" /> },
];

function AdderNode({ data }: { data: any }) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  return (
    <div className="relative w-[220px]">
      <button
        type="button"
        disabled={busy}
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center justify-center gap-2 rounded-xl border border-dashed border-border-strong bg-card/40 px-3 py-3 text-sm font-medium text-muted transition-colors hover:border-fg hover:text-fg disabled:opacity-60"
      >
        <Plus className="h-4 w-4" /> {busy ? "Adding…" : "Add a Service"}
      </button>
      <Handle type="target" id="in" position={Position.Left} style={handleStyle} />
      {open ? (
        <div className="absolute left-0 top-full z-20 mt-1 w-full overflow-hidden rounded-lg border border-border bg-card text-sm shadow-card">
          {ADDER_OPTIONS.map((opt) => (
            <button
              key={opt.kind}
              type="button"
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                setOpen(false);
                try {
                  await data.onAdd(opt.kind, opt.provider, opt.suffix);
                } finally {
                  setBusy(false);
                }
              }}
              className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-bg disabled:opacity-60"
            >
              <span className="text-muted">{opt.icon}</span> {opt.label}
            </button>
          ))}
          <Link
            href={`/projects/${encodeURIComponent(data.project)}/storage`}
            className="flex w-full items-center gap-2 border-t border-border px-3 py-2 text-left text-secondary hover:bg-bg"
          >
            <Lock className="h-4 w-4 text-muted" /> Connect existing
          </Link>
        </div>
      ) : null}
    </div>
  );
}

const nodeTypes = { ingress: IngressNode, service: ServiceNode, resource: ResourceNode, adder: AdderNode };

export function ServiceGraph({ project, prod }: { project: string; prod: Deployment | undefined }) {
  const { resolvedTheme } = useTheme();
  const { data: ov } = usePoll<Overview>("/v1/overview", 6000);
  const { data: cdn } = usePoll<Cdn>("/v1/cdn", 8000);
  const { data: waf } = usePoll<Waf>("/v1/waf", 10000);
  const { data: rl } = usePoll<RateLimit>("/v1/ratelimit", 10000);
  const { data: dbs, refresh: refreshDbs } = usePoll<Database[]>(`/v1/projects/${encodeURIComponent(project)}/databases`, 6000);
  const { data: links } = usePoll<SecureLink[]>("/v1/securelinks", 6000);
  const { data: fnstats } = usePoll<FunctionStats[]>("/v1/functions", 6000);
  const { data: settings } = usePoll<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`, 10000);

  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);
  const lastKey = useRef("");
  const rf = useRef<ReactFlowInstance | null>(null);

  const kind = prod?.kind ?? "static";
  const ddos = !!(rl?.enabled || waf?.managed);
  const cdnActive = (cdn?.entries ?? 0) > 0 || (cdn?.hits ?? 0) > 0;
  const myFns = (fnstats ?? []).filter((f) => prod && f.key.startsWith(prod.id));
  const instances = myFns.reduce((a, f) => a + (f.instances || 0), 0) || (kind === "static" ? 0 : 1);
  const memory = settings?.functions?.memory_mib ?? 512;
  const conns = [
    ...(dbs ?? []).map((d) => ({ id: d.id, name: d.name, sub: `${d.provider || d.kind}`, img: providerImg(d.provider), tunnel: false, envKey: d.kind === "blob" ? envKeyForKind("blob") : envKeyForKind(d.kind) })),
    ...(links ?? []).filter((l) => l.project === project).map((l) => ({ id: l.id, name: l.target, sub: "Secure tunnel", img: null as string | null, tunnel: true, envKey: l.env_var || null })),
  ];

  /** Provision a database and seed the matching project env var, then refresh. */
  const onAdd = useCallback(async (kind: AddKind, provider: string, suffix: string) => {
    try {
      const name = `${project}-${suffix}`;
      const db = await apiSend<Database>("POST", "/v1/databases", { name, project, kind, provider });
      const envKey = envKeyForKind(kind);
      if (envKey) {
        const value = connString(db);
        await apiSend("POST", `/v1/projects/${encodeURIComponent(project)}/env`, {
          key: envKey, value, target: "production", sensitive: true,
        });
      }
    } finally {
      // Let the new resource node appear without waiting for the 6s poll.
      refreshDbs();
    }
  }, [project, refreshDbs]);

  const key = JSON.stringify([prod?.id, prod?.state, kind, instances, memory, ddos, cdnActive, conns.map((c) => c.id)]);

  useEffect(() => {
    if (key === lastKey.current) return;
    lastKey.current = key;

    const procLabel = kind === "container" ? "Container" : kind === "static" ? "Static Site" : "Web Process";
    const ns: Node[] = [
      { id: "ingress", type: "ingress", position: { x: 0, y: 140 }, data: { ddos, cdn: cdnActive } },
      {
        id: "service", type: "service", position: { x: 320, y: 110 },
        data: {
          project, kind, procLabel, state: prod?.state,
          startCmd: kind === "container" ? "podman run" : kind === "static" ? "—" : "npm start",
          resources: kind === "static" ? "Edge" : `${memory} MB`,
          instances, domain: prod?.alias ?? "—",
          logsHref: `/projects/${encodeURIComponent(project)}/logs`,
          settingsHref: `/projects/${encodeURIComponent(project)}/settings`,
        },
      },
      ...conns.map((c, i) => ({
        id: `res-${c.id}`, type: "resource" as const,
        position: { x: 760, y: i * 92 },
        data: { name: c.name, sub: c.sub, img: c.img, tunnel: c.tunnel, envKey: c.envKey },
      })),
      {
        id: "adder", type: "adder" as const,
        position: { x: 760, y: conns.length * 92 + (conns.length ? 8 : 0) },
        data: { project, onAdd },
      },
    ];
    const es: Edge[] = [
      { id: "e-ingress", source: "ingress", sourceHandle: "out", target: "service", targetHandle: "in", animated: true, style: { stroke: "#f5a623", strokeWidth: 2 } },
      ...conns.map((c) => ({
        id: `e-${c.id}`, source: "service", sourceHandle: "svc-out", target: `res-${c.id}`, targetHandle: "in",
        animated: true, style: { stroke: "hsl(var(--border-strong))", strokeWidth: 2 },
      })),
      {
        id: "e-adder", source: "service", sourceHandle: "svc-out", target: "adder", targetHandle: "in",
        animated: true, style: { stroke: "hsl(var(--border-strong))", strokeWidth: 1.5, strokeDasharray: "5 5" },
      },
    ];
    setNodes(ns);
    setEdges(es);
    // Refit once the (async-loaded) nodes are in place.
    setTimeout(() => rf.current?.fitView({ padding: 0.2, duration: 300 }), 80);
  }, [key, project, kind, prod, instances, memory, ddos, cdnActive, conns, onAdd, setNodes, setEdges]);

  const dark = resolvedTheme === "dark";

  return (
    <div className="relative h-[600px] w-full overflow-hidden rounded-xl border border-border bg-bg">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onInit={(i) => { rf.current = i; setTimeout(() => i.fitView({ padding: 0.2 }), 60); }}
        nodeTypes={nodeTypes}
        fitView
        fitViewOptions={{ padding: 0.2 }}
        minZoom={0.3}
        maxZoom={1.75}
        proOptions={{ hideAttribution: true }}
        defaultEdgeOptions={{ animated: true }}
      >
        <Background variant={BackgroundVariant.Dots} gap={22} size={1} color={dark ? "#151515" : "#eee"} />
        <Controls showInteractive={false} className="!border-border !bg-card" />
        <MiniMap pannable zoomable className="!border !border-border !bg-card" maskColor={dark ? "rgba(0,0,0,0.4)" : "rgba(255,255,255,0.5)"} />
      </ReactFlow>
      <div className="pointer-events-none absolute bottom-3 right-3 z-10 rounded-md border border-border bg-card/90 px-2 py-1 text-xs text-muted">
        {REGION_LABEL[ov?.region ?? ""] ?? ov?.region ?? "—"}
      </div>
    </div>
  );
}

function StatRow({ icon, label, on }: { icon: React.ReactNode; label: string; on: boolean }) {
  return (
    <div className="flex items-center justify-between py-1 text-xs">
      <span className="flex items-center gap-1.5 text-secondary">{icon}{label}</span>
      <span className={`flex items-center gap-1 ${on ? "text-green" : "text-muted"}`}>
        <span className={`h-1.5 w-1.5 rounded-full ${on ? "bg-green" : "bg-border-strong"}`} />{on ? "Active" : "Inactive"}
      </span>
    </div>
  );
}
function KV({ k, v, mono }: { k: string; v: string; mono?: boolean }) {
  return (
    <div className="flex items-center justify-between gap-3 border-b border-border px-3 py-1.5 text-xs last:border-0">
      <span className="text-secondary">{k}</span>
      <span className={`truncate ${mono ? "font-mono" : ""}`}>{v}</span>
    </div>
  );
}
