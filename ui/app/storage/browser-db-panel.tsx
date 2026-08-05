"use client";

import { useEffect, useMemo, useState } from "react";
import { Database, Plus, X, Loader2, ChevronDown, ChevronRight, Trash2, Globe2, Users } from "lucide-react";
import { Card, Badge, Button, Input, Switch } from "@/components/ui";
import {
  apiGet, apiSend, usePoll, type Deployment, type ProjectSettings,
  type BrowserDbPolicy, type BrowserDbTable, type BrowserDbStatusResponse,
  BROWSER_DB_MAX_BYTES_MAX, BROWSER_DB_VALUE_MAX_BYTES_MAX,
} from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { RedeployModal } from "@/components/redeploy-modal";
import { BrowserDbLiveStatus } from "@/components/browser-db-live-status";
import { SchemaTableEditor } from "./browser-db-schema-builder";

const MB = 1024 * 1024;
const KB = 1024;

function emptyPolicy(): BrowserDbPolicy {
  return { max_bytes: 64 * MB, max_value_bytes: 1 * MB, public_read: false, schema: [{ name: "items", ddl: "" }] };
}

/**
 * "Deploy a replicated SQLite database" — the Storages page's UI opt-in for
 * `docs/browser-db-contract.md`. Writes ONLY `ProjectSettings::browser_db`
 * (`PUT /v1/projects/:project/browser-db`, `crates/hive-cloud/src/admin.rs`)
 * — a dashboard-managed mirror the build merges into the manifest when
 * fluid.json itself declares no block (`git.rs`, the `FunctionSettings::gpu`
 * OR precedent). A redeploy is required for it to actually reach a Ready
 * deployment, so saving offers the existing `RedeployModal` right after.
 */
export function BrowserDbSection() {
  const { data: deployments } = usePoll<Deployment[]>("/deployments", 15000);
  const projects = useMemo(
    () => Array.from(new Set((deployments ?? []).map((d) => d.project))).sort(),
    [deployments],
  );
  const deploymentsByProject = useMemo(() => {
    const m = new Map<string, Deployment>();
    for (const d of deployments ?? []) {
      const cur = m.get(d.project);
      if (!cur || (d.production && !cur.production) || d.created_at_ms > cur.created_at_ms) m.set(d.project, d);
    }
    return m;
  }, [deployments]);

  const [settings, setSettings] = useState<Record<string, ProjectSettings | undefined>>({});
  const projectsKey = projects.join(",");
  useEffect(() => {
    if (!projects.length) return;
    let cancelled = false;
    (async () => {
      const entries = await Promise.all(
        projects.map(async (p) => {
          try {
            return [p, await apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(p)}/settings`)] as const;
          } catch {
            return [p, undefined] as const;
          }
        }),
      );
      if (!cancelled) setSettings(Object.fromEntries(entries));
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectsKey]);

  const configured = projects.filter((p) => settings[p]?.browser_db);

  const [editing, setEditing] = useState<{ project: string; isNew: boolean } | null>(null);
  const [redeployTarget, setRedeployTarget] = useState<Deployment | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  async function refreshOne(project: string) {
    try {
      const s = await apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`, { fresh: true });
      setSettings((cur) => ({ ...cur, [project]: s }));
    } catch {
      /* leave stale entry — next poll cycle retries */
    }
  }

  async function remove(project: string) {
    if (!confirm(`Remove the browser-replicated database config for "${project}"? Fleet replica data is kept (30-day inert grace) — this only stops new grants and a future deploy from carrying the block.`)) return;
    await apiSend("DELETE", `/v1/projects/${encodeURIComponent(project)}/browser-db`);
    await refreshOne(project);
  }

  return (
    <div className="mt-10">
      <div className="mb-3 flex items-center justify-between">
        <div>
          <h2 className="text-lg font-semibold">Browser-Replicated SQLite</h2>
          <p className="mt-1 max-w-2xl text-sm text-secondary">
            One replicated cr-sqlite database per project. Every browser that admits to the project&apos;s deployment gets a live,
            bidirectionally-synced OPFS copy — no server database to provision, no connection string.
          </p>
        </div>
        <Button onClick={() => setEditing({ project: "", isNew: true })}>
          <Plus className="h-4 w-4" /> Deploy a replicated SQLite database
        </Button>
      </div>

      {configured.length === 0 ? (
        <Card className="flex flex-col items-center gap-2 py-10 text-center">
          <Database className="h-7 w-7 text-muted" />
          <div className="text-sm font-medium">No browser-replicated database yet</div>
          <p className="max-w-sm text-sm text-secondary">
            Opt a project in, define its schema, and redeploy — browsers that run this project&apos;s node start syncing immediately.
          </p>
        </Card>
      ) : (
        <div className="flex flex-col gap-2">
          {configured.map((p) => {
            const policy = settings[p]!.browser_db!;
            const isOpen = expanded === p;
            return (
              <Card key={p} className="p-0">
                <button
                  onClick={() => setExpanded(isOpen ? null : p)}
                  className="flex w-full items-center gap-3 px-4 py-3 text-left"
                >
                  {isOpen ? <ChevronDown className="h-4 w-4 text-muted" /> : <ChevronRight className="h-4 w-4 text-muted" />}
                  <Database className="h-4 w-4 text-muted" />
                  <span className="font-medium">{p}</span>
                  <span className="text-xs text-secondary">
                    {policy.schema.length} table{policy.schema.length === 1 ? "" : "s"}
                  </span>
                  <Badge tone={policy.public_read ? "blue" : "default"}>
                    {policy.public_read ? <span className="flex items-center gap-1"><Globe2 className="h-3 w-3" /> public read</span> : <span className="flex items-center gap-1"><Users className="h-3 w-3" /> team only</span>}
                  </Badge>
                  <span className="ml-auto flex items-center gap-1.5">
                    <span
                      role="button"
                      tabIndex={0}
                      onClick={(e) => { e.stopPropagation(); setEditing({ project: p, isNew: false }); }}
                      className="rounded-md border border-border px-2 py-1 text-xs text-secondary hover:bg-subtle"
                    >
                      Edit
                    </span>
                    {deploymentsByProject.get(p) && (
                      <span
                        role="button"
                        tabIndex={0}
                        onClick={(e) => { e.stopPropagation(); setRedeployTarget(deploymentsByProject.get(p)!); }}
                        className="rounded-md border border-border px-2 py-1 text-xs text-secondary hover:bg-subtle"
                      >
                        Redeploy
                      </span>
                    )}
                    <span
                      role="button"
                      tabIndex={0}
                      onClick={(e) => { e.stopPropagation(); remove(p); }}
                      className="rounded-md border border-border px-2 py-1 text-xs text-muted hover:border-red-500/40 hover:text-red-500"
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                    </span>
                  </span>
                </button>
                {isOpen && <BrowserDbStatusPanel project={p} />}
              </Card>
            );
          })}
        </div>
      )}

      {editing && (
        <BrowserDbConfigModal
          projects={projects}
          initialProject={editing.project}
          allowProjectPick={editing.isNew}
          initial={editing.isNew ? undefined : settings[editing.project]?.browser_db ?? undefined}
          onClose={() => setEditing(null)}
          onSaved={async (project) => {
            setEditing(null);
            await refreshOne(project);
            const dep = deploymentsByProject.get(project);
            if (dep) setRedeployTarget(dep);
          }}
        />
      )}

      {redeployTarget && (
        <RedeployModal
          deployment={redeployTarget}
          prodAlias={`${redeployTarget.project}.localhost`}
          onClose={() => setRedeployTarget(null)}
        />
      )}
    </div>
  );
}

function BrowserDbStatusPanel({ project }: { project: string }) {
  const { data: status } = usePoll<BrowserDbStatusResponse>(
    `/v1/projects/${encodeURIComponent(project)}/browser-db/status`,
    10000,
  );
  return (
    <div className="border-t border-border bg-subtle px-4 py-3">
      {!status ? (
        <div className="flex items-center gap-2 text-xs text-secondary"><Loader2 className="h-3.5 w-3.5 animate-spin" /> Loading status…</div>
      ) : !status.opted_in ? (
        <p className="text-xs text-secondary">Not opted in on this node yet — settings were just saved; redeploy to apply.</p>
      ) : (
        <div className="flex flex-col gap-3">
          <div>
            <div className="mb-1 flex items-center justify-between text-xs text-secondary">
              <span>Replica size</span>
              <span>
                {formatBytes(status.replica.bytes)} / {formatBytes(status.max_bytes)}
              </span>
            </div>
            <div className="h-1.5 w-full overflow-hidden rounded-full bg-border">
              <div
                className="h-full rounded-full bg-fg"
                style={{ width: `${Math.min(100, status.max_bytes ? (status.replica.bytes / status.max_bytes) * 100 : 0)}%` }}
              />
            </div>
          </div>
          <div className="flex flex-wrap gap-x-6 gap-y-1 text-xs text-secondary">
            <span>{status.replica.sites} replicated site{status.replica.sites === 1 ? "" : "s"}</span>
            <span>{status.replica.last_modified_ms ? `last write ${timeAgo(status.replica.last_modified_ms)} ago` : "no writes yet"}</span>
            <span>value cap {formatBytes(status.max_value_bytes)}</span>
            <span>tables: {status.tables.join(", ") || "none"}</span>
          </div>
          {status.notes.length > 0 && (
            <p className="text-xs text-amber-600 dark:text-amber-400">{status.notes.join(" · ")}</p>
          )}
          <BrowserDbLiveStatus project={project} />
        </div>
      )}
    </div>
  );
}

function formatBytes(n: number): string {
  if (n >= MB) return `${(n / MB).toFixed(1)} MB`;
  if (n >= KB) return `${(n / KB).toFixed(1)} KB`;
  return `${n} B`;
}

function BrowserDbConfigModal({
  projects,
  initialProject,
  allowProjectPick,
  initial,
  onClose,
  onSaved,
}: {
  projects: string[];
  initialProject: string;
  allowProjectPick: boolean;
  initial?: BrowserDbPolicy;
  onClose: () => void;
  onSaved: (project: string) => void;
}) {
  const [project, setProject] = useState(initialProject || projects[0] || "");
  const [policy, setPolicy] = useState<BrowserDbPolicy>(initial ?? emptyPolicy());
  const [maxMb, setMaxMb] = useState(Math.max(1, Math.round((initial?.max_bytes || 64 * MB) / MB)));
  const [maxValueKb, setMaxValueKb] = useState(Math.max(1, Math.round((initial?.max_value_bytes || 1 * MB) / KB)));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  function setTable(i: number, t: BrowserDbTable) {
    setPolicy((p) => ({ ...p, schema: p.schema.map((cur, j) => (j === i ? t : cur)) }));
  }
  function removeTable(i: number) {
    setPolicy((p) => ({ ...p, schema: p.schema.filter((_, j) => j !== i) }));
  }
  function addTable() {
    setPolicy((p) => ({ ...p, schema: [...p.schema, { name: `table_${p.schema.length + 1}`, ddl: "" }] }));
  }

  async function save() {
    if (!project.trim()) {
      setError("Choose a project.");
      return;
    }
    const missingDdl = policy.schema.find((t) => !t.ddl.trim());
    if (missingDdl) {
      setError(`Table "${missingDdl.name}" needs at least one column.`);
      return;
    }
    setBusy(true);
    setError("");
    try {
      const body: BrowserDbPolicy = {
        ...policy,
        max_bytes: Math.min(BROWSER_DB_MAX_BYTES_MAX, Math.max(1, maxMb) * MB),
        max_value_bytes: Math.min(BROWSER_DB_VALUE_MAX_BYTES_MAX, Math.max(1, maxValueKb) * KB),
      };
      await apiSend("PUT", `/v1/projects/${encodeURIComponent(project)}/browser-db`, body);
      onSaved(project);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <div className="flex max-h-[85vh] w-full max-w-xl flex-col overflow-hidden rounded-xl border border-border bg-card shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="flex items-center justify-between border-b border-border p-5">
          <h2 className="text-lg font-semibold">Deploy a replicated SQLite database</h2>
          <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-5 w-5" /></button>
        </div>
        <div className="flex-1 space-y-5 overflow-y-auto p-5">
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Project</label>
            {allowProjectPick ? (
              <select
                value={project}
                onChange={(e) => setProject(e.target.value)}
                className="w-full rounded-md border border-border bg-bg px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-border"
              >
                <option value="" disabled>Select a project…</option>
                {projects.map((p) => <option key={p} value={p}>{p}</option>)}
              </select>
            ) : (
              <div className="rounded-md border border-border bg-subtle px-3 py-2 text-sm">{project}</div>
            )}
          </div>

          <div>
            <div className="mb-1.5 flex items-center justify-between">
              <label className="text-xs font-medium text-secondary">Schema</label>
              <button onClick={addTable} type="button" className="flex items-center gap-1 text-xs text-link hover:underline">
                <Plus className="h-3 w-3" /> Add table
              </button>
            </div>
            <div className="flex flex-col gap-2">
              {policy.schema.map((t, i) => (
                <SchemaTableEditor key={i} table={t} onChange={(next) => setTable(i, next)} onRemove={() => removeTable(i)} />
              ))}
              {policy.schema.length === 0 && <p className="text-xs text-muted">Add at least one table.</p>}
            </div>
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Max database size (MB)</label>
              <Input
                type="number"
                min={1}
                max={BROWSER_DB_MAX_BYTES_MAX / MB}
                value={maxMb}
                onChange={(e) => setMaxMb(parseInt(e.target.value || "1", 10))}
              />
              <p className="mt-1 text-[11px] text-muted">Ceiling {BROWSER_DB_MAX_BYTES_MAX / MB} MB — enforced on every browser&apos;s OPFS copy and every fleet replica.</p>
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Max single value (KB)</label>
              <Input
                type="number"
                min={1}
                max={BROWSER_DB_VALUE_MAX_BYTES_MAX / KB}
                value={maxValueKb}
                onChange={(e) => setMaxValueKb(parseInt(e.target.value || "1", 10))}
              />
              <p className="mt-1 text-[11px] text-muted">Ceiling {BROWSER_DB_VALUE_MAX_BYTES_MAX / KB} KB per changed value.</p>
            </div>
          </div>

          <div className="flex items-center gap-3 rounded-lg border border-border p-3">
            <Switch checked={policy.public_read} onChange={(v) => setPolicy((p) => ({ ...p, public_read: v }))} label="Public read" />
            <div>
              <div className="text-sm font-medium">Allow anonymous read-only replicas</div>
              <div className="text-xs text-secondary">Off (default): only signed-in team members&apos; browsers get a grant. On: any visiting browser gets a read-only replica too — never write access.</div>
            </div>
          </div>

          {error && <p className="text-sm text-red-500">{error}</p>}
        </div>
        <div className="flex items-center justify-between border-t border-border p-4">
          <span className="text-xs text-muted">A redeploy is required for changes to take effect.</span>
          <div className="flex gap-2">
            <Button variant="outline" onClick={onClose}>Cancel</Button>
            <Button onClick={save} disabled={busy}>
              {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />} Save
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}
