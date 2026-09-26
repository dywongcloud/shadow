"use client";

import { useEffect, useState } from "react";
import { CheckCircle2, AlertTriangle, AlertOctagon, Activity, Clock, ChevronDown } from "lucide-react";
import { usePoll, type Incident, type Severity, type IncidentStatus } from "@/lib/api";

// A standalone, public, statuspage-style board. No dashboard chrome (the nav /
// footer hide on /status). Polls the global incidents feed in real time.

const SEV_RANK: Record<Severity, number> = { minor: 1, major: 2, critical: 3 };

const STATUS_LABEL: Record<IncidentStatus, string> = {
  investigating: "Investigating",
  identified: "Identified",
  monitoring: "Monitoring",
  resolved: "Resolved",
};

const SEV_STYLE: Record<Severity, { dot: string; text: string; chip: string }> = {
  minor: { dot: "bg-amber-400", text: "text-amber-500", chip: "border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400" },
  major: { dot: "bg-orange-500", text: "text-orange-500", chip: "border-orange-500/30 bg-orange-500/10 text-orange-600 dark:text-orange-400" },
  critical: { dot: "bg-red-500", text: "text-red-500", chip: "border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-400" },
};

// The platform surfaces shown in the components strip.
const COMPONENTS = ["Edge Network", "Builds & Deployments", "Fluid Compute", "Dashboard", "Admin API", "Storage"];

function fmtAbs(ms: number) {
  return new Date(ms).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
}
function fmtAgo(ms: number) {
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

export default function StatusPage() {
  const { data } = usePoll<Incident[]>("/v1/incidents", 5000);
  const [now, setNow] = useState(0);
  useEffect(() => {
    // Date.now() is client-only and time-varying -- cannot be computed during
    // SSR/lazy-init without a hydration mismatch; the `now ? ... : ""` render
    // guard covers the gap until this effect first fires.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setNow(Date.now());
    const id = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(id);
  }, [data]);

  const incidents = data ?? [];
  const active = incidents.filter((i) => i.status !== "resolved").sort((a, b) => b.updated_ms - a.updated_ms);
  const past = incidents.filter((i) => i.status === "resolved").sort((a, b) => b.updated_ms - a.updated_ms);

  const worst = active.reduce<Severity | null>((acc, i) => (!acc || SEV_RANK[i.severity] > SEV_RANK[acc] ? i.severity : acc), null);
  const affectedComponents = new Set(active.flatMap((i) => i.affected));

  const banner = !worst
    ? { icon: <CheckCircle2 className="h-7 w-7" />, title: "All Systems Operational", cls: "border-green/30 bg-green/10 text-green" }
    : worst === "critical"
    ? { icon: <AlertOctagon className="h-7 w-7" />, title: "Major System Outage", cls: "border-red-500/30 bg-red-500/10 text-red-500" }
    : worst === "major"
    ? { icon: <AlertTriangle className="h-7 w-7" />, title: "Partial System Outage", cls: "border-orange-500/30 bg-orange-500/10 text-orange-500" }
    : { icon: <AlertTriangle className="h-7 w-7" />, title: "Degraded Performance", cls: "border-amber-500/30 bg-amber-500/10 text-amber-500" };

  return (
    <div className="min-h-screen bg-bg text-fg">
      <div className="mx-auto max-w-3xl px-5 py-10 sm:py-14">
        {/* Header */}
        <div className="mb-8 flex items-center justify-between">
          <div className="flex items-center gap-2.5">
            <span className="flex h-8 w-8 items-center justify-center rounded-lg bg-fg text-bg">
              <svg width="13" height="11" viewBox="0 0 24 22" aria-hidden><path d="M12 0 L24 22 L0 22 Z" fill="currentColor" /></svg>
            </span>
            <div>
              <div className="text-base font-semibold leading-tight">DevHub Status</div>
              <div className="text-xs text-muted">Real-time platform health</div>
            </div>
          </div>
          <div className="flex items-center gap-1.5 text-xs text-muted">
            <span className="relative flex h-2 w-2">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-green opacity-60" />
              <span className="relative inline-flex h-2 w-2 rounded-full bg-green" />
            </span>
            Live{now ? ` · ${new Date(now).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })}` : ""}
          </div>
        </div>

        {/* Overall banner */}
        <div className={`mb-8 flex items-center gap-4 rounded-xl border p-5 ${banner.cls}`}>
          {banner.icon}
          <div>
            <div className="text-lg font-semibold">{banner.title}</div>
            <div className="text-sm opacity-80">
              {active.length ? `${active.length} active incident${active.length === 1 ? "" : "s"}` : "All services are running normally."}
            </div>
          </div>
        </div>

        {/* Components */}
        <div className="mb-10 overflow-hidden rounded-xl border border-border bg-card">
          <div className="border-b border-border px-4 py-2.5 text-sm font-medium">Components</div>
          {COMPONENTS.map((c) => {
            const down = affectedComponents.has(c);
            return (
              <div key={c} className="flex items-center justify-between border-b border-border px-4 py-3 text-sm last:border-0">
                <span>{c}</span>
                <span className={`flex items-center gap-1.5 text-xs font-medium ${down ? "text-amber-500" : "text-green"}`}>
                  <span className={`h-2 w-2 rounded-full ${down ? "bg-amber-400" : "bg-green"}`} />
                  {down ? "Degraded" : "Operational"}
                </span>
              </div>
            );
          })}
        </div>

        {/* Active incidents */}
        {active.length > 0 && (
          <section className="mb-10">
            <h2 className="mb-3 flex items-center gap-2 text-sm font-semibold uppercase tracking-wide text-secondary">
              <Activity className="h-4 w-4" /> Active Incidents
            </h2>
            <div className="space-y-4">
              {active.map((i) => <IncidentCard key={i.id} incident={i} active />)}
            </div>
          </section>
        )}

        {/* Past incidents */}
        <section>
          <h2 className="mb-3 flex items-center gap-2 text-sm font-semibold uppercase tracking-wide text-secondary">
            <Clock className="h-4 w-4" /> Incident History
          </h2>
          {past.length === 0 ? (
            <div className="rounded-xl border border-border bg-card px-4 py-10 text-center text-sm text-muted">
              No incidents reported. {active.length === 0 ? "All clear. 🎉" : ""}
            </div>
          ) : (
            <div className="space-y-4">
              {past.slice(0, 20).map((i) => <IncidentCard key={i.id} incident={i} />)}
            </div>
          )}
        </section>

        <div className="mt-12 text-center text-xs text-muted">
          Powered by DevHub · This page refreshes automatically.
        </div>
      </div>
    </div>
  );
}

function IncidentCard({ incident, active }: { incident: Incident; active?: boolean }) {
  const [open, setOpen] = useState(!!active);
  const sev = SEV_STYLE[incident.severity];
  const updates = [...incident.updates].sort((a, b) => b.ts_ms - a.ts_ms);

  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-start justify-between gap-3 px-4 py-3.5 text-left">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <span className={`h-2 w-2 shrink-0 rounded-full ${sev.dot}`} />
            <span className="font-semibold">{incident.title}</span>
            <span className={`rounded-full border px-2 py-0.5 text-[11px] font-medium capitalize ${sev.chip}`}>{incident.severity}</span>
            <span className="rounded-full border border-border bg-subtle px-2 py-0.5 text-[11px] font-medium text-secondary">
              {STATUS_LABEL[incident.status]}
            </span>
          </div>
          <div className="mt-1 text-xs text-muted">
            {fmtAbs(incident.created_ms)}
            {incident.affected.length ? ` · Affects: ${incident.affected.join(", ")}` : ""}
          </div>
        </div>
        <ChevronDown className={`mt-1 h-4 w-4 shrink-0 text-muted transition-transform ${open ? "rotate-180" : ""}`} />
      </button>

      {open && (
        <div className="border-t border-border px-4 py-3">
          {updates.length === 0 ? (
            <div className="text-sm text-muted">No updates posted yet.</div>
          ) : (
            <ol className="relative ml-1 space-y-4 border-l border-border pl-5">
              {updates.map((u, idx) => (
                <li key={idx} className="relative">
                  <span className="absolute -left-[1.45rem] top-1 flex h-3 w-3 items-center justify-center">
                    <span className={`h-2 w-2 rounded-full ${idx === 0 ? "bg-fg" : "bg-border-strong"}`} />
                  </span>
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-medium">{STATUS_LABEL[u.status]}</span>
                    <span className="text-xs text-muted">{fmtAgo(u.ts_ms)} · {fmtAbs(u.ts_ms)}</span>
                  </div>
                  <p className="mt-0.5 text-sm text-secondary">{u.message}</p>
                </li>
              ))}
            </ol>
          )}
        </div>
      )}
    </div>
  );
}
