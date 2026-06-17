"use client";

import { useState } from "react";
import { Siren, Plus, X } from "lucide-react";
import { Card, Badge, Button, Input } from "@/components/ui";
import { apiSend, usePoll, type Incident, type Severity, type IncidentStatus } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

const STATUSES: IncidentStatus[] = ["investigating", "identified", "monitoring", "resolved"];

export default function IncidentsPage() {
  const { data: incidents, refresh } = usePoll<Incident[]>("/v1/incidents", 4000);
  const [open, setOpen] = useState(false);

  return (
    <div>
      <div className="mb-6 flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Incidents</h1>
          <p className="mt-1 text-sm text-secondary">Track and communicate operational incidents across the platform.</p>
        </div>
        <Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Declare Incident</Button>
      </div>

      <div className="space-y-4">
        {(incidents ?? []).map((i) => (
          <IncidentCard key={i.id} inc={i} onChange={refresh} />
        ))}
        {!incidents?.length && (
          <Card className="flex flex-col items-center gap-2 py-16 text-center">
            <Siren className="h-8 w-8 text-muted" />
            <div className="text-sm font-medium">No incidents</div>
            <p className="text-sm text-secondary">When something breaks, declare an incident to track the response.</p>
          </Card>
        )}
      </div>

      {open && <DeclareModal onClose={() => setOpen(false)} onCreated={() => { setOpen(false); refresh(); }} />}
    </div>
  );
}

function sevTone(s: Severity) {
  return s === "critical" ? "red" : s === "major" ? "amber" : "blue";
}
function statusTone(s: IncidentStatus) {
  return s === "resolved" ? "green" : s === "monitoring" ? "blue" : "amber";
}

function IncidentCard({ inc, onChange }: { inc: Incident; onChange: () => void }) {
  const [status, setStatus] = useState<IncidentStatus>(inc.status);
  const [msg, setMsg] = useState("");

  async function post() {
    if (!msg.trim()) return;
    await apiSend("POST", `/v1/incidents/${inc.id}/updates`, { status, message: msg });
    setMsg("");
    onChange();
  }

  return (
    <Card>
      <div className="mb-3 flex items-start justify-between gap-3">
        <div className="flex items-center gap-2">
          <Badge tone={sevTone(inc.severity)}>{inc.severity}</Badge>
          <h3 className="text-base font-semibold">{inc.title}</h3>
        </div>
        <Badge tone={statusTone(inc.status)}>{inc.status}</Badge>
      </div>
      {!!inc.affected.length && (
        <div className="mb-3 flex flex-wrap gap-1">
          {inc.affected.map((a) => <Badge key={a}>{a}</Badge>)}
        </div>
      )}

      <div className="relative space-y-3 border-l border-border pl-4">
        {[...inc.updates].reverse().map((u, idx) => (
          <div key={idx} className="relative">
            <span className="absolute -left-[21px] top-1 h-2.5 w-2.5 rounded-full border-2 border-card bg-border-strong" />
            <div className="flex items-center gap-2">
              <span className="text-xs font-medium capitalize">{u.status}</span>
              <span className="text-xs text-muted">{timeAgo(u.ts_ms)} ago</span>
            </div>
            <p className="text-sm text-secondary">{u.message}</p>
          </div>
        ))}
      </div>

      {inc.status !== "resolved" && (
        <div className="mt-4 flex flex-col gap-2 border-t border-border pt-4 sm:flex-row sm:items-center">
          <select
            value={status}
            onChange={(e) => setStatus(e.target.value as IncidentStatus)}
            className="rounded-md border border-border bg-card px-3 py-2 text-sm capitalize text-fg focus:outline-none"
          >
            {STATUSES.map((s) => <option key={s} value={s}>{s}</option>)}
          </select>
          <Input value={msg} onChange={(e) => setMsg(e.target.value)} placeholder="Post an update…" className="flex-1" />
          <Button onClick={post}>Post update</Button>
        </div>
      )}
    </Card>
  );
}

function DeclareModal({ onClose, onCreated }: { onClose: () => void; onCreated: () => void }) {
  const [title, setTitle] = useState("");
  const [severity, setSeverity] = useState<Severity>("minor");
  const [affected, setAffected] = useState("");
  const [message, setMessage] = useState("");

  async function declare() {
    if (!title.trim()) return;
    await apiSend("POST", "/v1/incidents", {
      title,
      severity,
      affected: affected.split(",").map((s) => s.trim()).filter(Boolean),
      message,
    });
    onCreated();
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <div className="w-full max-w-lg rounded-xl border border-border bg-card p-6 shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="mb-4 flex items-center justify-between">
          <h2 className="text-lg font-semibold">Declare Incident</h2>
          <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
        </div>
        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Title</label>
            <Input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Elevated 5xx in iad1" autoFocus />
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Severity</label>
            <select
              value={severity}
              onChange={(e) => setSeverity(e.target.value as Severity)}
              className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm capitalize text-fg focus:outline-none"
            >
              <option value="minor">Minor</option>
              <option value="major">Major</option>
              <option value="critical">Critical</option>
            </select>
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Affected (comma-separated)</label>
            <Input value={affected} onChange={(e) => setAffected(e.target.value)} placeholder="iad1, CDN" />
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Initial update</label>
            <Input value={message} onChange={(e) => setMessage(e.target.value)} placeholder="We are investigating…" />
          </div>
        </div>
        <div className="mt-6 flex justify-end gap-2">
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button onClick={declare}><Siren className="h-4 w-4" /> Declare</Button>
        </div>
      </div>
    </div>
  );
}
