"use client";

import { useState } from "react";
import { Siren, Plus, X, Bot } from "lucide-react";
import { Card, Badge, Button, Input } from "@/components/ui";
import { opsGet, opsSend, useOpsPoll, type Incident, type Severity, type IncidentStatus } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { OPEN_LIMIT, OPEN_PATH, RESOLVED_PAGE, resolvedPath } from "@/lib/incident-paths";
import { OpsFetchError } from "@/components/ops-fetch-error";

const STATUSES: IncidentStatus[] = ["investigating", "identified", "monitoring", "resolved"];
const TIMELINE_PREVIEW_ENTRIES = 5;

export function IncidentsClient({
  initialOpen,
  initialResolved,
  initialOpenError,
  initialResolvedError,
}: {
  initialOpen: Incident[] | null;
  initialResolved: Incident[] | null;
  initialOpenError?: string | null;
  initialResolvedError?: string | null;
}) {
  const { data: open, error: openError, refresh } = useOpsPoll<Incident[]>(OPEN_PATH, 5000, true, initialOpen);
  const openFailure = openError ?? initialOpenError ?? null;
  const resolvedFailure = initialResolvedError ?? null;
  const [resolved, setResolved] = useState<Incident[]>(initialResolved ?? []);
  const [resolvedDone, setResolvedDone] = useState((initialResolved?.length ?? 0) < RESOLVED_PAGE);
  const [loadingMore, setLoadingMore] = useState(false);
  const [declare, setDeclare] = useState(false);

  async function loadMore() {
    setLoadingMore(true);
    try {
      const page = await opsGet<Incident[]>(resolvedPath(resolved.length), { fresh: true });
      setResolved((r) => [...r, ...page.filter((p) => !r.some((x) => x.id === p.id))]);
      if (page.length < RESOLVED_PAGE) setResolvedDone(true);
    } finally {
      setLoadingMore(false);
    }
  }

  async function onChanged() {
    refresh();
    const page = await opsGet<Incident[]>(resolvedPath(0), { fresh: true }).catch(() => null);
    if (page) {
      setResolved(page);
      setResolvedDone(page.length < RESOLVED_PAGE);
    }
  }

  const openRows = (open ?? []).filter((i) => i.status !== "resolved");

  return (
    <div>
      <div className="mb-6 flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Incidents</h1>
          <p className="mt-1 text-sm text-secondary">
            Declared incidents are yours to update and close. Automated ones are observed conditions: they resolve themselves once the platform stops observing them.
          </p>
        </div>
        <Button onClick={() => setDeclare(true)}><Plus className="h-4 w-4" /> Declare Incident</Button>
      </div>

      <section className="mb-8">
        <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-secondary">Open · {openRows.length}{openRows.length >= OPEN_LIMIT ? "+" : ""}</h2>
        <div className="space-y-4">
          {openRows.map((i) => (
            <IncidentCard key={i.id} inc={i} onChange={onChanged} />
          ))}
          {!openFailure && !openRows.length && (
            <Card className="flex flex-col items-center gap-2 py-12 text-center">
              <Siren className="h-8 w-8 text-muted" />
              <div className="text-sm font-medium">No open incidents</div>
              <p className="text-sm text-secondary">When something breaks, declare an incident to track the response.</p>
            </Card>
          )}
          <OpsFetchError error={openFailure} label="open incidents" />
        </div>
      </section>

      <section>
        <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-secondary">Resolved</h2>
        <div className="space-y-4">
          {resolved.map((i) => (
            <IncidentCard key={i.id} inc={i} onChange={onChanged} />
          ))}
          {!resolvedFailure && !resolved.length && (
            <Card className="py-8 text-center text-sm text-muted">No resolved incidents in the retained history.</Card>
          )}
          {!resolvedDone && (
            <Button variant="outline" onClick={loadMore} disabled={loadingMore}>
              {loadingMore ? "Loading…" : `Load ${RESOLVED_PAGE} more`}
            </Button>
          )}
          <OpsFetchError error={resolvedFailure} label="resolved incidents" />
        </div>
      </section>

      {declare && <DeclareModal onClose={() => setDeclare(false)} onCreated={() => { setDeclare(false); refresh(); }} />}
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
  const automated = inc.origin === "automated";
  const [showAll, setShowAll] = useState(false);
  const timeline = [...inc.updates].reverse();
  const shown = showAll ? timeline : timeline.slice(0, TIMELINE_PREVIEW_ENTRIES);

  async function post() {
    if (!msg.trim()) return;
    await opsSend("POST", `/v1/incidents/${inc.id}/updates`, { status, message: msg });
    setMsg("");
    onChange();
  }

  return (
    <Card>
      <div className="mb-3 flex items-start justify-between gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <Badge tone={sevTone(inc.severity)}>{inc.severity}</Badge>
          <h3 className="text-base font-semibold">{inc.title}</h3>
          {automated ? (
            <Badge>
              <Bot className="mr-1 inline h-3 w-3" />
              auto
            </Badge>
          ) : null}
        </div>
        <Badge tone={statusTone(inc.status)}>{inc.status}</Badge>
      </div>
      {!!inc.affected.length && (
        <div className="mb-3 flex flex-wrap gap-1">
          {inc.affected.map((a) => <Badge key={a}>{a}</Badge>)}
        </div>
      )}
      {automated && inc.status !== "resolved" && inc.observed_ms ? (
        <div className="mb-3 text-xs text-muted">
          Condition last observed {timeAgo(inc.observed_ms)}
          {inc.expires_ms ? ` · resolves itself if not observed again by ${new Date(inc.expires_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}` : ""}
        </div>
      ) : null}

      <div className="relative space-y-3 border-l border-border pl-4">
        {shown.map((u, idx) => (
          <div key={idx} className="relative">
            <span className="absolute -left-[21px] top-1 h-2.5 w-2.5 rounded-full border-2 border-card bg-border-strong" />
            <div className="flex items-center gap-2">
              <span className="text-xs font-medium capitalize">{u.status}</span>
              <span className="text-xs text-muted">{timeAgo(u.ts_ms)}</span>
            </div>
            <p className="text-sm text-secondary">{u.message}</p>
          </div>
        ))}
        {timeline.length > shown.length ? (
          <button onClick={() => setShowAll(true)} className="text-xs text-secondary hover:text-fg">
            Show {timeline.length - shown.length} earlier update{timeline.length - shown.length === 1 ? "" : "s"}
          </button>
        ) : null}
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
    await opsSend("POST", "/v1/incidents", {
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
