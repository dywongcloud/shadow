"use client";

import { useState } from "react";
import Link from "next/link";
import { Webhook as WebhookIcon, Check, X, Trash2, ArrowLeft } from "lucide-react";
import { Badge, Button, Card } from "@/components/ui";
import { apiSend, usePoll, type Webhook, type Delivery, type Deployment } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

// Grouped event catalog (mirrors Vercel's Add Webhooks picker).
const GROUPS: { title: string; events: { label: string; slug: string }[] }[] = [
  { title: "Deployment Events", events: [
    { label: "Deployment Created", slug: "deployment.created" },
    { label: "Deployment Build Requested", slug: "deployment.build-requested" },
    { label: "Deployment Error", slug: "deployment.error" },
    { label: "Deployment Canceled", slug: "deployment.canceled" },
    { label: "Deployment Succeeded", slug: "deployment.succeeded" },
    { label: "Deployment Promoted", slug: "deployment.promoted" },
    { label: "Deployment Rollback", slug: "deployment.rollback" },
  ]},
  { title: "Project Events", events: [
    { label: "Project Env Variable Created", slug: "project.env-variable.created" },
    { label: "Project Env Variable Updated", slug: "project.env-variable.updated" },
    { label: "Project Env Variable Deleted", slug: "project.env-variable.deleted" },
    { label: "Project Removed", slug: "project.removed" },
    { label: "Project Renamed", slug: "project.renamed" },
  ]},
  { title: "Domain Events", events: [
    { label: "Domain DNS Records Changed", slug: "domain.dns.records.changed" },
    { label: "Domain Added", slug: "domain.added" },
    { label: "Domain Certificate Add", slug: "domain.certificate.add" },
    { label: "Domain Certificate Renew", slug: "domain.certificate.renew" },
  ]},
  { title: "Project Domain Events", events: [
    { label: "Project Domain Created", slug: "project.domain.created" },
    { label: "Project Domain Verified", slug: "project.domain.verified" },
    { label: "Project Domain Deleted", slug: "project.domain.deleted" },
  ]},
  { title: "Feature Flag Events", events: [
    { label: "Flag Created", slug: "flag.created" },
    { label: "Flag Updated", slug: "flag.updated" },
    { label: "Flag Deleted", slug: "flag.deleted" },
  ]},
  { title: "Check Events", events: [
    { label: "Deployment Checks Failed", slug: "deployment.checks.failed" },
    { label: "Deployment Checks Succeeded", slug: "deployment.checks.succeeded" },
  ]},
  { title: "Observability Events", events: [
    { label: "Alerts Triggered", slug: "alerts.triggered" },
  ]},
  { title: "Project Created", events: [
    { label: "Project Created", slug: "project.created" },
  ]},
  { title: "Firewall Events", events: [
    { label: "Attack Detected", slug: "firewall.attack" },
  ]},
];

export default function WebhooksPage() {
  const { data: hooks, refresh } = usePoll<Webhook[]>("/v1/webhooks", 5000);
  const { data: deliveries } = usePoll<Delivery[]>("/v1/webhooks/deliveries", 5000);
  const { data: deps } = usePoll<Deployment[]>("/deployments", 8000);
  const projectNames = Array.from(new Set((deps ?? []).map((d) => d.project))).sort();

  const [scope, setScope] = useState<"all" | "specific">("all");
  const [projects, setProjects] = useState<string[]>([]);
  const [events, setEvents] = useState<string[]>([]);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);

  function toggle<T>(arr: T[], v: T, set: (a: T[]) => void) {
    set(arr.includes(v) ? arr.filter((x) => x !== v) : [...arr, v]);
  }

  async function create() {
    if (!url.trim() || events.length === 0) { alert("Pick at least one event and an endpoint URL."); return; }
    setBusy(true);
    try {
      await apiSend("POST", "/v1/webhooks", {
        url,
        events,
        projects: scope === "all" ? [] : projects,
      });
      setUrl(""); setEvents([]); setProjects([]); setScope("all");
      refresh();
    } catch (e) { alert(String(e)); } finally { setBusy(false); }
  }

  async function remove(id: string) {
    await apiSend("DELETE", `/v1/webhooks/${id}`);
    refresh();
  }

  return (
    <div>
      <Link href="/settings" className="mb-3 inline-flex items-center gap-1.5 text-sm text-link hover:underline"><ArrowLeft className="h-4 w-4" /> Team Settings</Link>
      <h1 className="mb-6 text-2xl font-semibold tracking-tight">Add Webhooks</h1>

      <Card className="p-6">
        {/* Projects */}
        <div className="mb-2 text-sm font-semibold">Projects</div>
        <p className="mb-3 text-sm text-secondary">Choose which projects should send events to the specified endpoint.</p>
        <div className="mb-6 flex gap-6 text-sm">
          <label className="flex cursor-pointer items-center gap-2">
            <input type="radio" checked={scope === "all"} onChange={() => setScope("all")} /> All Projects
          </label>
          <label className="flex cursor-pointer items-center gap-2">
            <input type="radio" checked={scope === "specific"} onChange={() => setScope("specific")} /> Specific Projects
          </label>
        </div>
        {scope === "specific" && (
          <div className="mb-6 flex flex-wrap gap-1.5">
            {projectNames.length ? projectNames.map((p) => (
              <button key={p} onClick={() => toggle(projects, p, setProjects)}
                className={`rounded-full border px-2.5 py-1 text-xs ${projects.includes(p) ? "border-fg bg-fg text-bg" : "border-border text-secondary hover:bg-subtle"}`}>
                {p}
              </button>
            )) : <span className="text-xs text-muted">No projects yet.</span>}
          </div>
        )}

        {/* Events */}
        <div className="mb-1 text-sm font-semibold">Events</div>
        <p className="mb-4 text-sm text-secondary">Select the events the webhook will listen to. At least one must be selected.</p>
        <div className="grid grid-cols-1 gap-x-8 gap-y-6 rounded-lg border border-border p-5 sm:grid-cols-2 lg:grid-cols-3">
          {GROUPS.map((g) => (
            <div key={g.title}>
              <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">{g.title}</div>
              <div className="flex flex-col gap-2.5">
                {g.events.map((ev) => (
                  <label key={ev.slug} className="flex cursor-pointer items-start gap-2">
                    <input type="checkbox" className="mt-0.5" checked={events.includes(ev.slug)} onChange={() => toggle(events, ev.slug, setEvents)} />
                    <span>
                      <span className="block text-sm leading-tight">{ev.label}</span>
                      <span className="block font-mono text-[11px] text-muted">{ev.slug}</span>
                    </span>
                  </label>
                ))}
              </div>
            </div>
          ))}
        </div>

        {/* Endpoint */}
        <div className="mb-1 mt-6 text-sm font-semibold">Endpoint</div>
        <p className="mb-2 text-sm text-secondary">Webhook events will be sent as a <code className="rounded bg-subtle px-1 font-mono text-xs">POST</code> request to this URL.</p>
        <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://my-app.com/webhooks"
          className="w-full rounded-md border border-border bg-card px-3 py-2.5 text-sm focus:outline-none focus:ring-2 focus:ring-border" />

        <div className="mt-6 flex items-center justify-between border-t border-border pt-4">
          <span className="text-sm text-secondary">{events.length} event(s) · {scope === "all" ? "all projects" : `${projects.length} project(s)`}</span>
          <Button onClick={create} disabled={busy}>Create Webhook</Button>
        </div>
      </Card>

      {/* Existing webhooks */}
      {!!hooks?.length && (
        <div className="mt-8 space-y-3">
          <h2 className="text-lg font-semibold">Active webhooks</h2>
          {hooks.map((h) => (
            <div key={h.id} className="flex items-start justify-between gap-3 rounded-lg border border-border bg-card p-4">
              <div className="min-w-0">
                <div className="flex items-center gap-2 truncate font-mono text-sm"><WebhookIcon className="h-4 w-4 shrink-0 text-muted" /> {h.url}</div>
                <div className="mt-1.5 flex flex-wrap gap-1">
                  <Badge tone="blue">{h.project === "*" ? "all projects" : h.project}</Badge>
                  {h.events.length ? h.events.slice(0, 6).map((e) => <Badge key={e}>{e}</Badge>) : <Badge>all events</Badge>}
                  {h.events.length > 6 ? <Badge>+{h.events.length - 6}</Badge> : null}
                </div>
                <div className="mt-2 font-mono text-[11px] text-muted">secret: {h.secret}</div>
              </div>
              <button onClick={() => remove(h.id)} className="shrink-0 text-muted hover:text-red-500"><Trash2 className="h-4 w-4" /></button>
            </div>
          ))}
        </div>
      )}

      {/* Deliveries */}
      {!!deliveries?.length && (
        <div className="mt-8 space-y-2">
          <h2 className="text-lg font-semibold">Recent deliveries</h2>
          <div className="divide-y divide-border rounded-lg border border-border bg-card">
            {deliveries.slice(0, 12).map((d) => (
              <div key={d.id} className="flex items-center gap-3 px-4 py-2 text-sm">
                {d.ok ? <Check className="h-4 w-4 text-green" /> : <X className="h-4 w-4 text-red-500" />}
                <span className="font-mono text-xs">{d.event}</span>
                <span className="min-w-0 flex-1 truncate text-xs text-secondary">{d.url}</span>
                <Badge tone={d.ok ? "green" : "red"}>{d.status || "ERR"}</Badge>
                <span className="text-xs text-muted">{timeAgo(d.ts_ms)} ago</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
