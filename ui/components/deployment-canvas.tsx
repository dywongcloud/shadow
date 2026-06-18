"use client";

import Link from "next/link";
import {
  Network, Shield, Globe, Boxes, Database as DbIcon, Terminal, ScrollText, Settings2,
  ChevronsUpDown, Plus, Container, FileCode, Zap, MapPin,
} from "lucide-react";
import {
  usePoll, type Deployment, type Overview, type Database, type SecureLink,
  type FunctionStats, type ProjectSettings,
} from "@/lib/api";

interface Cdn { hits: number; misses: number; entries: number; hit_ratio: number }
interface Waf { managed: boolean }
interface RateLimit { enabled: boolean }

const REGION_LABEL: Record<string, string> = {
  iad1: "Washington, D.C., USA (us-east-1)",
  sfo1: "San Francisco, USA (us-west-1)",
  fra1: "Frankfurt, Germany (eu-central-1)",
  hnd1: "Tokyo, Japan (ap-northeast-1)",
};

export function DeploymentCanvas({ project, prod }: { project: string; prod: Deployment | undefined }) {
  const { data: ov } = usePoll<Overview>("/v1/overview", 5000);
  const { data: cdn } = usePoll<Cdn>("/v1/cdn", 6000);
  const { data: waf } = usePoll<Waf>("/v1/waf", 8000);
  const { data: rl } = usePoll<RateLimit>("/v1/ratelimit", 8000);
  const { data: dbs } = usePoll<Database[]>(`/v1/projects/${encodeURIComponent(project)}/databases`, 6000);
  const { data: links } = usePoll<SecureLink[]>("/v1/securelinks", 6000);
  const { data: fnstats } = usePoll<FunctionStats[]>("/v1/functions", 5000);
  const { data: settings } = usePoll<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`, 8000);

  const kind = prod?.kind ?? "static";
  const isContainer = kind === "container";
  const isStatic = kind === "static";
  const procIcon = isContainer ? <Container className="h-4 w-4" /> : isStatic ? <FileCode className="h-4 w-4" /> : <Globe className="h-4 w-4" />;
  const procLabel = isContainer ? "Container" : isStatic ? "Static Site" : "Web Process";

  // Instances for this deployment's functions.
  const myFns = (fnstats ?? []).filter((f) => prod && f.key.startsWith(prod.id));
  const instances = myFns.reduce((a, f) => a + (f.instances || 0), 0) || (isStatic ? 0 : 1);
  const memory = settings?.functions?.memory_mib ?? 512;

  const conns = [
    ...(dbs ?? []).map((d) => ({ id: d.id, name: d.name, kind: d.kind })),
    ...(links ?? []).filter((l) => l.project === project).map((l) => ({ id: l.id, name: l.target, kind: "tunnel" })),
  ];

  const ddos = rl?.enabled || waf?.managed;
  const cdnActive = (cdn?.entries ?? 0) > 0 || (cdn?.hits ?? 0) > 0;

  return (
    <div
      className="relative overflow-hidden rounded-xl border border-border bg-bg p-6"
      style={{
        backgroundImage:
          "radial-gradient(circle, hsl(var(--foreground) / 0.08) 1px, transparent 1px)",
        backgroundSize: "18px 18px",
      }}
    >
      <div className="flex flex-col items-stretch gap-6 lg:flex-row lg:items-center lg:justify-between">
        {/* Networking / Ingress */}
        <div className="w-full lg:w-[260px]">
          <div className="mb-2 flex items-center gap-2 text-sm font-medium"><Network className="h-4 w-4" /> Networking</div>
          <div className="rounded-xl border border-border bg-card p-3 shadow-card">
            <div className="mb-2 flex items-center gap-2 border-b border-border pb-2 text-sm font-semibold">
              <span className="flex h-5 w-5 items-center justify-center rounded bg-fg text-bg">
                <svg width="9" height="8" viewBox="0 0 24 22" aria-hidden><path d="M12 0 L24 22 L0 22 Z" fill="currentColor" /></svg>
              </span>
              OpenEdge
            </div>
            <Row label={<><Shield className="h-3.5 w-3.5" /> DDoS protection</>} on={!!ddos} />
            <Row label={<><Globe className="h-3.5 w-3.5" /> CDN</>} on={cdnActive} />
            <Row label={<><Zap className="h-3.5 w-3.5" /> Edge caching</>} on={cdnActive} />
            <Link href="/firewall" className="mt-2 flex items-center justify-center gap-1.5 border-t border-border pt-2 text-xs text-secondary hover:text-fg">
              <Settings2 className="h-3.5 w-3.5" /> Settings
            </Link>
          </div>
        </div>

        <Connector label={isStatic ? "https" : "8787"} active />

        {/* Service / process */}
        <div className="w-full lg:flex-1 lg:max-w-[380px]">
          <div className="rounded-xl border border-border bg-card p-3 shadow-card">
            <div className="mb-2 flex items-center justify-between text-sm font-semibold">
              <span className="flex items-center gap-2"><Boxes className="h-4 w-4" /> {project}</span>
              <ChevronsUpDown className="h-3.5 w-3.5 text-muted" />
            </div>
            <div className="rounded-lg border border-border">
              <div className="flex items-center justify-between border-b border-border px-3 py-2 text-sm">
                <span className="flex items-center gap-2 font-medium">{procIcon} {procLabel}</span>
                <span className="flex items-center gap-1.5 text-xs">
                  <span className={`h-2 w-2 rounded-full ${prod?.state === "ready" ? "bg-green" : prod?.state === "building" ? "bg-amber-400" : "bg-red-400"}`} />
                  {prod ? prod.state.charAt(0).toUpperCase() + prod.state.slice(1) : "—"}
                </span>
              </div>
              <KV k="Start command" v={isContainer ? "podman run" : isStatic ? "—" : "npm start"} />
              <KV k="Resources" v={isStatic ? "Edge" : `${memory} MB`} />
              <KV k="Instances" v={String(instances)} />
              <KV k="Domain" v={prod?.alias ?? "—"} mono />
            </div>
            <div className="mt-2 flex items-center justify-around text-xs text-secondary">
              <Link href={`/projects/${encodeURIComponent(project)}/logs`} className="flex items-center gap-1.5 hover:text-fg"><ScrollText className="h-3.5 w-3.5" /> View logs</Link>
              <Link href="/sandbox" className="flex items-center gap-1.5 hover:text-fg"><Terminal className="h-3.5 w-3.5" /> Web terminal</Link>
              <Link href={`/projects/${encodeURIComponent(project)}/settings`} className="flex items-center gap-1.5 hover:text-fg"><Settings2 className="h-3.5 w-3.5" /> Settings</Link>
            </div>
          </div>
        </div>

        <Connector label="" active={conns.length > 0} dashed />

        {/* Connected services */}
        <div className="w-full lg:w-[260px]">
          <div className="rounded-xl border border-border bg-card p-3 shadow-card">
            <div className="mb-2 flex items-center gap-2 text-sm font-medium"><Boxes className="h-4 w-4" /> Connected services</div>
            <div className="flex flex-col gap-1.5">
              {conns.length ? conns.map((c) => (
                <div key={c.id} className="flex items-center gap-2 rounded-lg border border-border px-3 py-2 text-sm">
                  {c.kind === "tunnel" ? <Network className="h-3.5 w-3.5 text-muted" /> : <DbIcon className="h-3.5 w-3.5 text-muted" />}
                  <span className="truncate">{c.name}</span>
                </div>
              )) : <div className="rounded-lg border border-dashed border-border px-3 py-3 text-center text-xs text-muted">No connected services</div>}
            </div>
            <Link href="/storage?browse=1" className="mt-2 flex items-center justify-center gap-1.5 pt-1 text-xs font-medium text-link hover:underline">
              <Plus className="h-3.5 w-3.5" /> Add internal connection
            </Link>
          </div>
        </div>
      </div>

      <div className="mt-4 flex items-center justify-end gap-1.5 text-xs text-muted">
        <MapPin className="h-3.5 w-3.5" /> {REGION_LABEL[ov?.region ?? ""] ?? ov?.region ?? "—"}
      </div>
    </div>
  );
}

function Row({ label, on }: { label: React.ReactNode; on: boolean }) {
  return (
    <div className="flex items-center justify-between py-1 text-xs">
      <span className="flex items-center gap-1.5 text-secondary">{label}</span>
      <span className={`flex items-center gap-1 ${on ? "text-green" : "text-muted"}`}>
        <span className={`h-1.5 w-1.5 rounded-full ${on ? "bg-green" : "bg-border-strong"}`} />
        {on ? "Active" : "Inactive"}
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

function Connector({ label, active, dashed }: { label: string; active: boolean; dashed?: boolean }) {
  return (
    <div className="relative flex min-w-[40px] flex-1 items-center justify-center lg:max-w-[120px]">
      <div className={`h-px w-full border-t-2 border-dashed ${active ? "border-[#f5a623]" : "border-border-strong"} ${dashed ? "border-border-strong" : ""}`} />
      {label && (
        <span className="absolute rounded-md border border-border bg-card px-2 py-0.5 font-mono text-[11px] text-secondary">{label}</span>
      )}
    </div>
  );
}
