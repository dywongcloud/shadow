"use client";

import { useState } from "react";
import Link from "next/link";
import {
  Plus, Search, Trash2, ArrowRight, Route as RouteIcon, Tag, Image as ImageIcon,
  AlertTriangle, Check, X,
} from "lucide-react";
import { Badge, Button, Card, PageHeader } from "@/components/ui";
import { apiSend, usePoll } from "@/lib/api";
import { cn } from "@/lib/utils";

interface Routing {
  redirects: { source: string; destination: string; status: number }[];
  rewrites: { source: string; destination: string }[];
}
interface Cdn { hits: number; misses: number; stale: number; entries: number; hit_ratio: number }

export default function CdnPage() {
  return (
    <div className="space-y-12">
      <PageHeader title="CDN" desc="Edge routing, redirects, and cache controls — applied instantly without a new deployment." />
      <RoutingSection />
      <RedirectsSection />
      <CachesSection />
    </div>
  );
}

/* ----------------------------- Routing Rules ----------------------------- */
function RoutingSection() {
  const { data, refresh } = usePoll<Routing>("/v1/routing", 4000);
  const [q, setQ] = useState("");
  const rules = [
    ...(data?.redirects ?? []).map((r) => ({ kind: "Redirect" as const, ...r })),
    ...(data?.rewrites ?? []).map((r) => ({ kind: "Rewrite" as const, status: 0, ...r })),
  ].filter((r) => !q || r.source.includes(q) || r.destination.includes(q));

  async function remove(kind: string, source: string) {
    const path = kind === "Redirect" ? "/v1/routing/redirects/delete" : "/v1/routing/rewrites/delete";
    await apiSend("POST", path, { source });
    refresh();
  }

  return (
    <section>
      <SectionHead title="Routing Rules" desc="Rules that execute at the edge and override deployment config." count={`${rules.length} of 100`}
        action={<Link href="/cdn/routing/new"><Button><Plus className="h-4 w-4" /> Create Route</Button></Link>} />
      <div className="relative mb-3">
        <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
        <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search by path…"
          className="w-full rounded-lg border border-border bg-card py-2.5 pl-9 pr-3 text-sm focus:outline-none focus:ring-2 focus:ring-border" />
      </div>
      {rules.length ? (
        <Card className="p-0">
          {rules.map((r, i) => (
            <div key={i} className="flex items-center justify-between border-b border-border px-4 py-3 text-sm last:border-0">
              <div className="flex items-center gap-3">
                <Badge tone={r.kind === "Redirect" ? "amber" : "blue"}>{r.kind}{r.kind === "Redirect" ? ` ${r.status}` : ""}</Badge>
                <span className="font-mono text-xs">{r.source}</span>
                <ArrowRight className="h-3.5 w-3.5 text-muted" />
                <span className="font-mono text-xs text-secondary">{r.destination}</span>
              </div>
              <button onClick={() => remove(r.kind, r.source)} className="text-muted hover:text-red-500"><Trash2 className="h-4 w-4" /></button>
            </div>
          ))}
        </Card>
      ) : (
        <Card className="flex flex-col items-center gap-3 py-14 text-center">
          <span className="flex h-12 w-12 items-center justify-center rounded-xl border border-border"><RouteIcon className="h-5 w-5 text-muted" /></span>
          <div className="text-base font-semibold">No Routing Rules</div>
          <p className="max-w-sm text-sm text-secondary">Create a route to define your traffic rules.</p>
          <Link href="/cdn/routing/new"><Button>Create Route</Button></Link>
        </Card>
      )}
    </section>
  );
}

/* ------------------------------- Redirects ------------------------------- */
function RedirectsSection() {
  const { data, refresh } = usePoll<Routing>("/v1/routing", 4000);
  const [q, setQ] = useState("");
  const [open, setOpen] = useState(false);
  const list = (data?.redirects ?? []).filter((r) => !q || r.source.includes(q) || r.destination.includes(q));

  async function remove(source: string) {
    await apiSend("POST", "/v1/routing/redirects/delete", { source });
    refresh();
  }

  return (
    <section>
      <SectionHead title="Redirects" desc="Available instantly after the change — no new deployment needed."
        action={<Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Create Redirect</Button>} />
      <div className="relative mb-3">
        <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
        <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search redirects…"
          className="w-full rounded-lg border border-border bg-card py-2.5 pl-9 pr-3 text-sm focus:outline-none focus:ring-2 focus:ring-border" />
      </div>
      {list.length ? (
        <Card className="p-0">
          {list.map((r, i) => (
            <div key={i} className="flex items-center justify-between border-b border-border px-4 py-3 text-sm last:border-0">
              <div className="flex items-center gap-3">
                <Badge tone="amber">{r.status}</Badge>
                <span className="font-mono text-xs">{r.source}</span>
                <ArrowRight className="h-3.5 w-3.5 text-muted" />
                <span className="font-mono text-xs text-secondary">{r.destination}</span>
              </div>
              <button onClick={() => remove(r.source)} className="text-muted hover:text-red-500"><Trash2 className="h-4 w-4" /></button>
            </div>
          ))}
        </Card>
      ) : (
        <Card className="flex flex-col items-center gap-2 py-14 text-center">
          <div className="text-base font-semibold">No redirects</div>
          <p className="max-w-xs text-sm text-secondary">Create redirects so every path can always be accessed.</p>
          <Button variant="outline" onClick={() => setOpen(true)}>Create redirects</Button>
        </Card>
      )}
      {open && <CreateRedirect onClose={() => setOpen(false)} onCreated={() => { setOpen(false); refresh(); }} />}
    </section>
  );
}

function CreateRedirect({ onClose, onCreated }: { onClose: () => void; onCreated: () => void }) {
  const [source, setSource] = useState("");
  const [dest, setDest] = useState("");
  const [status, setStatus] = useState("308");
  async function create() {
    if (!source || !dest) return;
    try { await apiSend("POST", "/v1/routing/redirects", { source, destination: dest, status: Number(status) }); onCreated(); }
    catch (e) { alert(String(e)); }
  }
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <div className="w-full max-w-md rounded-xl border border-border bg-card p-6 shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="mb-4 flex items-center justify-between"><h2 className="text-lg font-semibold">Create Redirect</h2><button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button></div>
        <div className="space-y-3">
          <input value={source} onChange={(e) => setSource(e.target.value)} placeholder="/old-path" className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none" />
          <input value={dest} onChange={(e) => setDest(e.target.value)} placeholder="/new-path" className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none" />
          <select value={status} onChange={(e) => setStatus(e.target.value)} className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none">
            <option value="308">308 Permanent</option><option value="307">307 Temporary</option><option value="301">301 Moved</option><option value="302">302 Found</option>
          </select>
        </div>
        <div className="mt-6 flex justify-end gap-2"><Button variant="outline" onClick={onClose}>Cancel</Button><Button onClick={create}>Create</Button></div>
      </div>
    </div>
  );
}

/* -------------------------------- Caches --------------------------------- */
type Scope = "tag" | "image" | "all" | "";
function CachesSection() {
  const { data: cdn, refresh } = usePoll<Cdn>("/v1/cdn", 4000);
  const [scope, setScope] = useState<Scope>("");
  const [done, setDone] = useState(false);

  async function purge() {
    if (!scope) return;
    try { await apiSend("POST", "/v1/cdn/purge", { scope }); setDone(true); setTimeout(() => setDone(false), 2000); refresh(); }
    catch (e) { alert(String(e)); }
  }

  return (
    <section>
      <SectionHead title="Caches" desc="Pages, API responses, and assets cached in regions close to users." />
      <div className="mb-4 grid grid-cols-2 gap-3 sm:grid-cols-4">
        <Stat label="Cache HITs" value={cdn?.hits ?? 0} />
        <Stat label="MISSes" value={cdn?.misses ?? 0} />
        <Stat label="Entries" value={cdn?.entries ?? 0} />
        <Stat label="Hit ratio" value={`${Math.round((cdn?.hit_ratio ?? 0) * 100)}%`} />
      </div>
      <Card>
        <h3 className="text-base font-semibold">Purge cache</h3>
        <p className="mt-1 text-sm text-secondary">Force fresh data on the next request.</p>
        <div className="mb-4 mt-5 text-sm font-medium">Which content do you want to purge?</div>
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
          <Choice icon={<Tag className="h-4 w-4" />} title="Cache Tag" desc="Responses with a specific user-defined tag." active={scope === "tag"} onClick={() => setScope("tag")} />
          <Choice icon={<ImageIcon className="h-4 w-4" />} title="Source Image" desc="Optimized images for a source image URL." active={scope === "image"} onClick={() => setScope("image")} />
          <Choice icon={<AlertTriangle className="h-4 w-4 text-amber-500" />} title="All content" desc="Purge everything for this project." active={scope === "all"} onClick={() => setScope("all")} />
        </div>
        <div className="mt-6 flex justify-end"><Button onClick={purge} disabled={!scope}>{done ? <><Check className="h-4 w-4" /> Purged</> : "Purge"}</Button></div>
      </Card>
    </section>
  );
}

/* ------------------------------- helpers --------------------------------- */
function SectionHead({ title, desc, count, action }: { title: string; desc: string; count?: string; action?: React.ReactNode }) {
  return (
    <div className="mb-4 flex items-end justify-between gap-3">
      <div>
        <h2 className="text-xl font-semibold tracking-tight">{title}</h2>
        <p className="mt-1 text-sm text-secondary">{desc}{count ? ` · ${count}` : ""}</p>
      </div>
      {action}
    </div>
  );
}
function Stat({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <Card className="flex flex-col gap-1 p-4">
      <span className="text-[11px] uppercase tracking-wide text-muted">{label}</span>
      <span className="text-xl font-semibold tabular-nums">{value}</span>
    </Card>
  );
}
function Choice({ icon, title, desc, active, onClick }: { icon: React.ReactNode; title: string; desc: string; active: boolean; onClick: () => void }) {
  return (
    <button onClick={onClick} className={cn("flex flex-col items-start gap-2 rounded-lg border p-4 text-left transition-colors", active ? "border-fg bg-subtle" : "border-border hover:bg-subtle")}>
      <div className="flex w-full items-center justify-between">
        <span className="flex items-center gap-2 text-sm font-medium">{icon} {title}</span>
        <span className={cn("h-4 w-4 rounded-full border", active ? "border-fg bg-fg" : "border-border-strong")} />
      </div>
      <span className="text-xs text-secondary">{desc}</span>
    </button>
  );
}
