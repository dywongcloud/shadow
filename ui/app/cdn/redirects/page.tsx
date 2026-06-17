"use client";

import { useState } from "react";
import { Plus, Search, Trash2, ArrowRight, X } from "lucide-react";
import { Badge, Button, Card } from "@/components/ui";
import { apiSend, usePoll } from "@/lib/api";
import { cn } from "@/lib/utils";

interface Routing {
  redirects: { source: string; destination: string; status: number }[];
}

export default function RedirectsPage() {
  const { data, refresh } = usePoll<Routing>("/v1/routing", 4000);
  const [env, setEnv] = useState<"Production" | "Staging">("Production");
  const [q, setQ] = useState("");
  const [open, setOpen] = useState(false);

  const list = (data?.redirects ?? []).filter((r) => !q || r.source.includes(q) || r.destination.includes(q));

  async function remove(source: string) {
    await apiSend("POST", "/v1/routing/redirects/delete", { source });
    refresh();
  }

  return (
    <div>
      <h1 className="text-2xl font-semibold tracking-tight">Redirects</h1>
      <p className="mt-1 text-sm text-secondary">
        Manage redirects for your project. Redirects configured here are available instantly after the change without the need for a new deployment.
      </p>

      <div className="mt-6 flex items-center gap-2">
        <div className="flex rounded-md border border-border p-0.5">
          {(["Production", "Staging"] as const).map((e) => (
            <button key={e} onClick={() => setEnv(e)}
              className={cn("rounded px-3 py-1 text-sm", env === e ? "bg-subtle font-medium text-fg" : "text-secondary")}>{e}</button>
          ))}
        </div>
        <div className="relative flex-1">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search redirects…"
            className="w-full rounded-lg border border-border bg-card py-2.5 pl-9 pr-3 text-sm focus:outline-none focus:ring-2 focus:ring-border" />
        </div>
        <Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Create Redirect</Button>
      </div>

      {list.length ? (
        <Card className="mt-4 p-0">
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
        <Card className="mt-4 flex flex-col items-center gap-3 py-16 text-center">
          <div className="text-base font-semibold">No redirects</div>
          <p className="max-w-xs text-sm text-secondary">Create redirects in your project to make sure every path can always be accessed.</p>
          <Button variant="outline" onClick={() => setOpen(true)}>Create redirects</Button>
        </Card>
      )}

      {open && <CreateRedirect onClose={() => setOpen(false)} onCreated={() => { setOpen(false); refresh(); }} />}
    </div>
  );
}

function CreateRedirect({ onClose, onCreated }: { onClose: () => void; onCreated: () => void }) {
  const [source, setSource] = useState("");
  const [dest, setDest] = useState("");
  const [status, setStatus] = useState("308");
  const [busy, setBusy] = useState(false);

  async function create() {
    if (!source || !dest) return;
    setBusy(true);
    try {
      await apiSend("POST", "/v1/routing/redirects", { source, destination: dest, status: Number(status) });
      onCreated();
    } catch (e) { alert(String(e)); setBusy(false); }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <div className="w-full max-w-md rounded-xl border border-border bg-card p-6 shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="mb-4 flex items-center justify-between">
          <h2 className="text-lg font-semibold">Create Redirect</h2>
          <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
        </div>
        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Source path</label>
            <input value={source} onChange={(e) => setSource(e.target.value)} placeholder="/old-path" className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none" />
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Destination</label>
            <input value={dest} onChange={(e) => setDest(e.target.value)} placeholder="/new-path" className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none" />
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Status</label>
            <select value={status} onChange={(e) => setStatus(e.target.value)} className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none">
              <option value="308">308 Permanent</option><option value="307">307 Temporary</option>
              <option value="301">301 Moved</option><option value="302">302 Found</option>
            </select>
          </div>
        </div>
        <div className="mt-6 flex justify-end gap-2">
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button onClick={create} disabled={busy}>Create</Button>
        </div>
      </div>
    </div>
  );
}
