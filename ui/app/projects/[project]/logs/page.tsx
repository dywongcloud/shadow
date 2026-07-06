"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { Search, Play, RotateCw, X, ChevronDown, MapPin, ShieldCheck, Zap } from "lucide-react";
import { Triangle, Badge } from "@/components/ui";
import { apiGet, type Event } from "@/lib/api";
import { cn } from "@/lib/utils";

function statusTone(s: number) {
  if (s === 0) return "text-secondary";
  if (s >= 500) return "text-red-500";
  if (s >= 400) return "text-amber-500";
  if (s >= 300) return "text-blue-500";
  return "text-green";
}
function fmtTime(ms: number) {
  const d = new Date(ms);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${d.toLocaleString("en-US", { month: "short", day: "2-digit" }).toUpperCase()} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 2).slice(0, 2)}`;
}

export default function ProjectLogs({ params }: { params: { project: string } }) {
  const project = decodeURIComponent(params.project);
  const [events, setEvents] = useState<Event[]>([]);
  const [q, setQ] = useState("");
  const [live, setLive] = useState(true);
  const [sel, setSel] = useState<Event | null>(null);
  const [range, setRange] = useState("Last 30 minutes");

  async function load() {
    try {
      const e = await apiGet<Event[]>(`/v1/logs?limit=300&project=${encodeURIComponent(project)}${q ? `&q=${encodeURIComponent(q)}` : ""}`);
      setEvents(e);
    } catch {}
  }
  useEffect(() => {
    load();
    if (!live) return;
    const id = setInterval(load, 1500);
    return () => clearInterval(id);
  }, [project, q, live]);

  const counts = {
    warning: events.filter((e) => e.action.includes("stale") || e.action === "rewrite").length,
    error: events.filter((e) => e.status >= 500 || e.action.includes("deny") || e.action.includes("block")).length,
    fatal: 0,
  };

  return (
    <div>
      {/* project header */}
      <div className="mb-4 flex items-center gap-3">
        <Triangle />
        <div className="flex items-center gap-2">
          <span className="h-2 w-2 rounded-full bg-green" />
          <Link href={`/projects/${encodeURIComponent(project)}`} className="text-lg font-semibold hover:underline">{project}</Link>
          <span className="text-muted">/</span>
          <span className="text-lg font-semibold">Logs</span>
        </div>
      </div>
      {/* Project sub-tabs now live in the top nav (breadcrumb-tabs model). */}

      <div className="grid grid-cols-1 gap-0 overflow-hidden rounded-xl border border-border lg:grid-cols-[200px_1fr]">
        {/* Filters sidebar */}
        <aside className="border-b border-border bg-subtle/40 p-4 lg:border-b-0 lg:border-r">
          <div className="mb-3 flex items-center justify-between">
            <span className="text-sm font-semibold">Filters</span>
            <button onClick={() => { setQ(""); }} className="text-xs text-secondary hover:text-fg">Reset</button>
          </div>
          <div className="mb-4">
            <div className="mb-1.5 text-xs font-medium text-secondary">Timeline</div>
            <select value={range} onChange={(e) => setRange(e.target.value)} className="w-full rounded-md border border-border bg-card px-2 py-1.5 text-xs focus:outline-none">
              <option>Last 30 minutes</option><option>Last hour</option><option>Last 24 hours</option>
            </select>
          </div>
          <div className="mb-2 text-xs font-medium text-secondary">Console Level</div>
          <Level label="Warning" n={counts.warning} />
          <Level label="Error" n={counts.error} />
          <Level label="Fatal" n={counts.fatal} />
          <div className="mt-3 flex flex-col gap-1.5 text-xs text-secondary">
            {["Resource", "Environment", "Route", "Status Code", "Host", "Request Method", "Cache"].map((f) => (
              <button key={f} className="flex items-center justify-between py-1 hover:text-fg"><span>{f}</span><ChevronDown className="h-3 w-3" /></button>
            ))}
          </div>
        </aside>

        {/* Main */}
        <div className="min-w-0">
          {/* toolbar */}
          <div className="flex items-center gap-2 border-b border-border p-3">
            <div className="relative flex-1">
              <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
              <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search logs…" className="w-full rounded-md border border-border bg-card py-1.5 pl-9 pr-2 text-sm focus:outline-none" />
            </div>
            <button onClick={() => setLive(!live)} className={cn("flex items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-sm", live ? "border-green/40 text-green" : "border-border text-secondary")}>
              <Play className="h-3.5 w-3.5" /> {live ? "Live" : "Paused"}
            </button>
            <button onClick={load} className="rounded-md border border-border p-1.5 text-secondary hover:bg-subtle"><RotateCw className="h-4 w-4" /></button>
          </div>

          <div className="flex">
            {/* table */}
            <div className="min-w-0 flex-1 overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="text-xs uppercase tracking-wide text-muted">
                    <th className="px-3 py-2 text-left font-medium">Time</th>
                    <th className="px-3 py-2 text-left font-medium">Status</th>
                    <th className="px-3 py-2 text-left font-medium">Host</th>
                    <th className="px-3 py-2 text-left font-medium">Request</th>
                  </tr>
                </thead>
                <tbody className="font-mono text-xs">
                  {events.map((e, i) => (
                    <tr key={i} onClick={() => setSel(e)} className={cn("cursor-pointer border-t border-border hover:bg-subtle", sel === e && "bg-subtle")}>
                      <td className="whitespace-nowrap px-3 py-2 text-secondary">{fmtTime(e.ts_ms)}</td>
                      <td className={cn("px-3 py-2", statusTone(e.status))}>{e.method} {e.status || "---"}</td>
                      <td className="max-w-[200px] truncate px-3 py-2 text-secondary">{e.host}</td>
                      <td className="max-w-[280px] truncate px-3 py-2">{e.path}</td>
                    </tr>
                  ))}
                  {!events.length && <tr><td colSpan={4} className="px-3 py-10 text-center text-sm text-secondary">No logs yet — hit your deployment to generate traffic.</td></tr>}
                </tbody>
              </table>
            </div>

            {/* detail panel */}
            {sel && (
              <div className="w-80 shrink-0 border-l border-border p-4">
                <div className="mb-3 flex items-center justify-between">
                  <span className="font-mono text-xs">{sel.method} {sel.path}</span>
                  <button onClick={() => setSel(null)} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
                </div>
                <Field label="Request ID" value={`${Math.random().toString(36).slice(2, 8)}-${sel.ts_ms}`} mono />
                <Field label="Path" value={sel.path} mono />
                <Field label="Host" value={sel.host} mono />
                <Field label="Status" value={String(sel.status || "—")} />
                <div className="mt-3 flex items-center gap-1.5 text-xs text-secondary">
                  <MapPin className="h-3.5 w-3.5" /> Received in {regionLabel(sel.region)} ({sel.region})
                </div>
                <div className="mt-4 rounded-lg border border-border p-3">
                  <div className="mb-1.5 flex items-center gap-1.5 text-sm font-medium"><ShieldCheck className="h-4 w-4" /> Firewall</div>
                  <Badge tone={sel.action.includes("deny") || sel.action.includes("block") ? "red" : "green"}>
                    {sel.action.includes("deny") || sel.action.includes("block") ? "Blocked" : "Allowed"}
                  </Badge>
                </div>
                <div className="mt-3 rounded-lg border border-border p-3">
                  <div className="mb-2 flex items-center gap-1.5 text-sm font-medium"><Zap className="h-4 w-4" /> Function Invocation</div>
                  <Field label="Action" value={sel.action} />
                  {sel.detail ? <Field label="Detail" value={sel.detail} mono /> : null}
                  <Field label="Cache" value={sel.action.includes("cache") ? sel.action.replace("cache-", "").toUpperCase() : "MISS"} />
                </div>
                <div className="mt-3 text-xs text-secondary">
                  <div className="mb-1 font-medium text-fg">Deployment Information</div>
                  <Field label="Project" value={project} />
                  <Field label="Environment" value="production" />
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function Level({ label, n }: { label: string; n: number }) {
  return (
    <label className="flex items-center justify-between py-1 text-xs text-secondary">
      <span className="flex items-center gap-2"><input type="checkbox" className="h-3.5 w-3.5 accent-fg" /> {label}</span>
      <span className="rounded-full bg-card px-1.5 text-[10px]">{n}</span>
    </label>
  );
}
function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="mb-2">
      <div className="text-[11px] uppercase tracking-wide text-muted">{label}</div>
      <div className={cn("truncate text-xs", mono && "font-mono")}>{value}</div>
    </div>
  );
}
function regionLabel(r: string) {
  return { iad1: "Washington, D.C., USA", sfo1: "San Francisco, USA", fra1: "Frankfurt, Germany" }[r] || r;
}
