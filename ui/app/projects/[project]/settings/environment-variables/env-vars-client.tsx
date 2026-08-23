"use client";

import { useEffect, useState, use } from "react";
import { Search, Lock, Plus, Trash2 } from "lucide-react";
import { Card, Button, Input, Badge, Switch } from "@/components/ui";
import { apiGet, apiSend, type EnvVar, type ProjectSettings } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export function EnvVarsPage({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const [vars, setVars] = useState<EnvVar[]>([]);
  const [q, setQ] = useState("");
  const [adding, setAdding] = useState(false);
  const [loadErr, setLoadErr] = useState("");

  // new-var form
  const [k, setK] = useState("");
  const [v, setV] = useState("");
  const [target, setTarget] = useState("production");
  const [scope, setScope] = useState<"runtime" | "build" | "all">("runtime");
  const [sensitive, setSensitive] = useState(true);
  // Save-path error + busy state: the server answer to a failed write (401
  // session expired, 403 wrong workspace, network) used to be swallowed by an
  // unhandled rejection, so every failure looked identical — "nothing
  // happens". Surface the actual error text next to the Save button.
  const [saveErr, setSaveErr] = useState("");
  const [busy, setBusy] = useState(false);

  // Inline EDIT state: which key is expanded + its draft value/target. Sensitive
  // values are never returned by the API (masked to ""), so the editor shows an
  // empty field — entering a new value REPLACES the stored one.
  const [editing, setEditing] = useState<string | null>(null);
  const [editVal, setEditVal] = useState("");
  const [editTarget, setEditTarget] = useState("production");
  const [editScope, setEditScope] = useState<"runtime" | "build" | "all">("runtime");

  async function load() {
    try {
      const s = await apiGet<ProjectSettings>(`/v1/projects/${project}/settings`);
      setVars(s.env ?? []);
      setLoadErr("");
    } catch (e) {
      // Surface the failure instead of silently rendering an empty list — a
      // swallowed error here looked like "my variables disappeared".
      setLoadErr(String(e));
    }
  }
  useEffect(() => { load(); }, [project]);

  async function add() {
    if (!k || busy) return;
    setBusy(true);
    setSaveErr("");
    try {
      await apiSend("POST", `/v1/projects/${project}/env`, {
        key: k.trim(), value: v, target, scope, sensitive, updated_ms: 0,
      });
      setK(""); setV(""); setScope("runtime"); setSensitive(true); setAdding(false);
      load();
    } catch (e) {
      setSaveErr(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  }
  async function remove(key: string) {
    setSaveErr("");
    try {
      await apiSend("DELETE", `/v1/projects/${project}/env/${encodeURIComponent(key)}`);
      load();
    } catch (e) {
      setSaveErr(String(e).replace(/^Error:\s*/, ""));
    }
  }
  function openEdit(e: EnvVar) {
    setEditing(e.key);
    setEditVal(e.sensitive ? "" : e.value); // sensitive values are server-masked
    setEditTarget(e.target || "production");
    setEditScope(e.scope || "runtime");
  }
  async function saveEdit(e: EnvVar) {
    // Backend semantics: an empty value on an existing key KEEPS the stored
    // value (secrets are never echoed back to the client), so leaving the field
    // blank on a sensitive var means "unchanged".
    if (busy) return;
    setBusy(true);
    setSaveErr("");
    try {
      await apiSend("POST", `/v1/projects/${project}/env`, {
        key: e.key, value: editVal, target: editTarget, scope: editScope, sensitive: e.sensitive, updated_ms: 0,
      });
      setEditing(null);
      load();
    } catch (err) {
      setSaveErr(String(err).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
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
            <div>
              <label className="mb-1.5 block text-xs font-medium text-secondary">Available during</label>
              <select
                value={scope}
                onChange={(e) => setScope(e.target.value as "runtime" | "build" | "all")}
                className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
              >
                <option value="runtime">Runtime only (default)</option>
                <option value="build">Build only</option>
                <option value="all">Build and runtime</option>
              </select>
            </div>
            <label className="flex items-center gap-3 self-end pb-2 text-sm text-secondary">
              <Switch checked={sensitive} onChange={setSensitive} label="Sensitive" /> Sensitive (encrypted, hidden after save)
            </label>
          </div>
          <div className="mt-4 flex items-center gap-2">
            <Button onClick={add} disabled={busy}>{busy ? "Saving…" : "Save"}</Button>
            <Button variant="ghost" onClick={() => setAdding(false)}>Cancel</Button>
            {saveErr && <span className="text-sm text-red-500">{saveErr}</span>}
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
      {loadErr && (
        <div className="mb-4 rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-500">
          Could not load variables: {loadErr}{" "}
          <button className="underline" onClick={load}>Retry</button>
        </div>
      )}
      <Card className="p-0">
        {filtered.map((e, i) => (
          <div key={e.key + i} className="border-b border-border last:border-0">
            <div
              className="flex cursor-pointer items-center gap-4 px-4 py-3.5 hover:bg-subtle/40"
              onClick={() => (editing === e.key ? setEditing(null) : openEdit(e))}
            >
              <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full border border-border text-muted">
                <Lock className="h-3.5 w-3.5" />
              </span>
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  <span className="truncate font-mono text-sm">{e.key}</span>
                  {e.sensitive && <Badge>Sensitive</Badge>}
                </div>
                <div className="text-xs capitalize text-secondary">
                  {e.target} · {(e.scope || "runtime").replace("all", "build + runtime")}
                  {/* Value preview: plaintext vars show their value; sensitive stay hidden. */}
                  {!e.sensitive && e.value && (
                    <span className="ml-2 font-mono normal-case text-muted">= {e.value.length > 42 ? e.value.slice(0, 42) + "…" : e.value}</span>
                  )}
                  {e.sensitive && <span className="ml-2 font-mono text-muted">= ••••••••</span>}
                </div>
              </div>
              <span className="hidden text-xs text-secondary sm:inline">
                {e.updated_ms ? `Updated ${timeAgo(e.updated_ms)} ago` : "Added"}
              </span>
              <span className="text-xs text-secondary underline decoration-dotted underline-offset-2">Edit</span>
              <button
                onClick={(ev) => { ev.stopPropagation(); remove(e.key); }}
                className="text-muted hover:text-red-500"
                aria-label={`Delete ${e.key}`}
              >
                <Trash2 className="h-4 w-4" />
              </button>
            </div>
            {editing === e.key && (
              <div className="border-t border-border bg-subtle/30 px-4 py-3">
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                  <div>
                    <label className="mb-1.5 block text-xs font-medium text-secondary">Value</label>
                    <Input
                      value={editVal}
                      placeholder={e.sensitive ? "hidden — enter a new value to replace" : "value"}
                      onChange={(ev) => setEditVal(ev.target.value)}
                      className="font-mono text-xs"
                    />
                    {e.sensitive && (
                      <p className="mt-1 text-xs text-muted">Sensitive values are never shown. Leave blank to keep the current value.</p>
                    )}
                  </div>
                  <div>
                    <label className="mb-1.5 block text-xs font-medium text-secondary">Environment</label>
                    <select
                      value={editTarget}
                      onChange={(ev) => setEditTarget(ev.target.value)}
                      className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
                    >
                      <option value="production">Production</option>
                      <option value="preview">Preview</option>
                      <option value="development">Development</option>
                      <option value="all">All Environments</option>
                    </select>
                  </div>
                  <div>
                    <label className="mb-1.5 block text-xs font-medium text-secondary">Available during</label>
                    <select
                      value={editScope}
                      onChange={(ev) => setEditScope(ev.target.value as "runtime" | "build" | "all")}
                      className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
                    >
                      <option value="runtime">Runtime only (default)</option>
                      <option value="build">Build only</option>
                      <option value="all">Build and runtime</option>
                    </select>
                  </div>
                </div>
                <div className="mt-3 flex items-center gap-2">
                  <Button onClick={() => saveEdit(e)} disabled={busy}>{busy ? "Saving…" : "Save"}</Button>
                  <Button variant="ghost" onClick={() => setEditing(null)}>Cancel</Button>
                  {saveErr && <span className="text-sm text-red-500">{saveErr}</span>}
                </div>
              </div>
            )}
          </div>
        ))}
        {!filtered.length && !loadErr && (
          <div className="px-4 py-12 text-center text-sm text-secondary">
            No environment variables yet. Click <span className="font-medium text-fg">Add Environment Variable</span> to store secrets securely.
          </div>
        )}
      </Card>
    </div>
  );
}
