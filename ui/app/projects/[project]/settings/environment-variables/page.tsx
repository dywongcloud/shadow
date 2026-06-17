"use client";

import { useEffect, useState } from "react";
import { Search, Lock, Plus, Trash2 } from "lucide-react";
import { Card, Button, Input, Badge, Switch } from "@/components/ui";
import { apiGet, apiSend, type EnvVar, type ProjectSettings } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function EnvVarsPage({ params }: { params: { project: string } }) {
  const project = decodeURIComponent(params.project);
  const [vars, setVars] = useState<EnvVar[]>([]);
  const [q, setQ] = useState("");
  const [adding, setAdding] = useState(false);

  // new-var form
  const [k, setK] = useState("");
  const [v, setV] = useState("");
  const [target, setTarget] = useState("production");
  const [sensitive, setSensitive] = useState(true);

  async function load() {
    const s = await apiGet<ProjectSettings>(`/v1/projects/${project}/settings`);
    setVars(s.env);
  }
  useEffect(() => { load(); }, [project]);

  async function add() {
    if (!k) return;
    await apiSend("POST", `/v1/projects/${project}/env`, {
      key: k.trim(), value: v, target, sensitive, updated_ms: 0,
    });
    setK(""); setV(""); setSensitive(true); setAdding(false);
    load();
  }
  async function remove(key: string) {
    await apiSend("DELETE", `/v1/projects/${project}/env/${encodeURIComponent(key)}`);
    load();
  }

  const filtered = vars.filter((e) => e.key.toLowerCase().includes(q.toLowerCase()));

  return (
    <div>
      <div className="mb-6 flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h2 className="text-2xl font-semibold tracking-tight">Environment Variables</h2>
          <p className="mt-1.5 text-sm text-secondary">Store API keys, tokens, and config securely.</p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline">Link Shared Variable</Button>
          <Button onClick={() => setAdding((a) => !a)}><Plus className="h-4 w-4" /> Add Environment Variable</Button>
        </div>
      </div>

      {adding && (
        <Card className="mb-4">
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <div>
              <label className="mb-1.5 block text-xs font-medium text-secondary">Key</label>
              <Input placeholder="EXAMPLE_NAME" value={k} onChange={(e) => setK(e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, "_"))} />
            </div>
            <div>
              <label className="mb-1.5 block text-xs font-medium text-secondary">Value</label>
              <Input placeholder="value" value={v} onChange={(e) => setV(e.target.value)} />
            </div>
            <div>
              <label className="mb-1.5 block text-xs font-medium text-secondary">Environment</label>
              <select
                value={target}
                onChange={(e) => setTarget(e.target.value)}
                className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
              >
                <option value="production">Production</option>
                <option value="preview">Preview</option>
                <option value="development">Development</option>
                <option value="all">All Environments</option>
              </select>
            </div>
            <label className="flex items-center gap-3 self-end pb-2 text-sm text-secondary">
              <Switch checked={sensitive} onChange={setSensitive} label="Sensitive" /> Sensitive (encrypted, hidden after save)
            </label>
          </div>
          <div className="mt-4 flex gap-2">
            <Button onClick={add}>Save</Button>
            <Button variant="ghost" onClick={() => setAdding(false)}>Cancel</Button>
          </div>
        </Card>
      )}

      <div className="mb-4 flex flex-wrap items-center gap-2">
        <div className="relative min-w-[220px] flex-1">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <Input placeholder="Search variables" value={q} onChange={(e) => setQ(e.target.value)} className="pl-9" />
        </div>
        <select className="rounded-md border border-border bg-card px-3 py-2 text-sm text-secondary focus:outline-none">
          <option>All Environments</option><option>Production</option><option>Preview</option>
        </select>
        <select className="rounded-md border border-border bg-card px-3 py-2 text-sm text-secondary focus:outline-none">
          <option>All Variables</option><option>Sensitive</option><option>Plaintext</option>
        </select>
        <select className="rounded-md border border-border bg-card px-3 py-2 text-sm text-secondary focus:outline-none">
          <option>Last Updated</option><option>Name</option>
        </select>
      </div>

      <Card className="p-0">
        {filtered.map((e, i) => (
          <div key={e.key + i} className="flex items-center gap-4 border-b border-border px-4 py-3.5 last:border-0">
            <span className="flex h-8 w-8 items-center justify-center rounded-full border border-border text-muted">
              <Lock className="h-3.5 w-3.5" />
            </span>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span className="truncate font-mono text-sm">{e.key}</span>
                {e.sensitive && <Badge>Sensitive</Badge>}
              </div>
              <div className="text-xs capitalize text-secondary">{e.target}</div>
            </div>
            <span className="hidden text-xs text-secondary sm:inline">
              {e.updated_ms ? `Updated ${timeAgo(e.updated_ms)} ago` : "Added"}
            </span>
            <button onClick={() => remove(e.key)} className="text-muted hover:text-red-500">
              <Trash2 className="h-4 w-4" />
            </button>
          </div>
        ))}
        {!filtered.length && (
          <div className="px-4 py-12 text-center text-sm text-secondary">
            No environment variables yet. Click <span className="font-medium text-fg">Add Environment Variable</span> to store secrets securely.
          </div>
        )}
      </Card>
    </div>
  );
}
