"use client";

import { useState } from "react";
import { Plus, X, Loader2 } from "lucide-react";
import { Button, Input, Switch } from "@/components/ui";
import {
  apiSend, usePoll,
  type BrowserDbPolicy, type BrowserDbTable, type BrowserDbStatusResponse,
  BROWSER_DB_MAX_BYTES_MAX, BROWSER_DB_VALUE_MAX_BYTES_MAX,
} from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { BrowserDbLiveStatus } from "@/components/browser-db-live-status";
import { SchemaTableEditor } from "./browser-db-schema-builder";

export const MB = 1024 * 1024;
export const KB = 1024;

export function emptyPolicy(): BrowserDbPolicy {
  return { max_bytes: 64 * MB, max_value_bytes: 1 * MB, public_read: false, schema: [{ name: "items", ddl: "" }] };
}

export function formatBytes(n: number): string {
  if (n >= MB) return `${(n / MB).toFixed(1)} MB`;
  if (n >= KB) return `${(n / KB).toFixed(1)} KB`;
  return `${n} B`;
}

/** The SQLite half of the Storage page's create/edit flow: the form only (a browser-replicated database is listed by the unified table like every other, see `db-model.ts`).
 *  Writes ONLY `ProjectSettings::browser_db` (`PUT /v1/projects/:project/browser-db`), the dashboard-managed mirror the build merges into the manifest when fluid.json declares no
 *  block; a redeploy is required for it to reach a Ready deployment, which the caller offers right after saving. */
export function BrowserDbConfigForm({
  projects,
  project,
  onProjectChange,
  allowProjectPick,
  policy,
  onPolicyChange,
  /** True when the LIVE spec comes from a repo-authored fluid.json block. */
  repoAuthored,
}: {
  projects: string[];
  project: string;
  onProjectChange: (p: string) => void;
  allowProjectPick: boolean;
  policy: BrowserDbPolicy;
  onPolicyChange: (p: BrowserDbPolicy) => void;
  repoAuthored?: boolean;
}) {
  const maxMb = Math.max(1, Math.round((policy.max_bytes || 64 * MB) / MB));
  const maxValueKb = Math.max(1, Math.round((policy.max_value_bytes || 1 * MB) / KB));

  function setTable(i: number, t: BrowserDbTable) {
    onPolicyChange({ ...policy, schema: policy.schema.map((cur, j) => (j === i ? t : cur)) });
  }
  function removeTable(i: number) {
    onPolicyChange({ ...policy, schema: policy.schema.filter((_, j) => j !== i) });
  }
  function addTable() {
    onPolicyChange({ ...policy, schema: [...policy.schema, { name: `table_${policy.schema.length + 1}`, ddl: "" }] });
  }

  return (
    <div className="min-w-0 w-full space-y-5">
      <div className="min-w-0 w-full">
        <label className="mb-1 block text-xs font-medium text-secondary">Project</label>
        {allowProjectPick ? (
          <select
            value={project}
            onChange={(e) => onProjectChange(e.target.value)}
            className="min-w-0 w-full rounded-md border border-border bg-bg px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-border"
          >
            <option value="" disabled>Select a project…</option>
            {projects.map((p) => <option key={p} value={p}>{p}</option>)}
          </select>
        ) : (
          <div className="min-w-0 w-full break-all rounded-md border border-border bg-subtle px-3 py-2 text-sm">{project}</div>
        )}
        <p className="mt-1 text-[11px] text-muted">
          The database IS the project — one replicated SQLite database per project, and its data survives redeploys.
        </p>
      </div>

      {repoAuthored && (
        <p className="rounded-lg border border-amber-500/40 bg-amber-500/5 p-3 text-xs text-amber-600 dark:text-amber-400">
          This project&apos;s live spec comes from a <code className="break-all font-mono">browser_db</code> block in its
          fluid.json. An explicit repo block always wins over what you save here — edit fluid.json and push, or
          remove the block to let this dashboard config apply.
        </p>
      )}

      <div>
        <div className="mb-1.5 flex min-w-0 w-full flex-col items-stretch gap-2 sm:flex-row sm:items-center sm:justify-between">
          <label className="min-w-0 w-full text-xs font-medium text-secondary sm:w-auto">Schema</label>
          <button onClick={addTable} type="button" className="flex min-w-0 w-full items-center justify-center gap-1 text-xs text-link hover:underline sm:w-auto">
            <Plus className="h-3 w-3" /> Add table
          </button>
        </div>
        <div className="flex min-w-0 w-full flex-col gap-2">
          {policy.schema.map((t, i) => (
            <SchemaTableEditor key={i} table={t} onChange={(next) => setTable(i, next)} onRemove={() => removeTable(i)} />
          ))}
          {policy.schema.length === 0 && <p className="text-xs text-muted">Add at least one table.</p>}
        </div>
      </div>

      <div className="grid min-w-0 w-full grid-cols-1 gap-3 sm:grid-cols-2">
        <div className="min-w-0 w-full">
          <label className="mb-1 block break-words text-xs font-medium text-secondary">Max database size (MB)</label>
          <Input
            type="number"
            min={1}
            max={BROWSER_DB_MAX_BYTES_MAX / MB}
            value={maxMb}
            onChange={(e) => onPolicyChange({ ...policy, max_bytes: Math.max(1, parseInt(e.target.value || "1", 10)) * MB })}
            className="min-w-0 w-full"
          />
          <p className="mt-1 min-w-0 w-full break-words text-[11px] text-muted">Ceiling {BROWSER_DB_MAX_BYTES_MAX / MB} MB — enforced on every browser&apos;s OPFS copy and every fleet replica.</p>
        </div>
        <div className="min-w-0 w-full">
          <label className="mb-1 block break-words text-xs font-medium text-secondary">Max single value (KB)</label>
          <Input
            type="number"
            min={1}
            max={BROWSER_DB_VALUE_MAX_BYTES_MAX / KB}
            value={maxValueKb}
            onChange={(e) => onPolicyChange({ ...policy, max_value_bytes: Math.max(1, parseInt(e.target.value || "1", 10)) * KB })}
            className="min-w-0 w-full"
          />
          <p className="mt-1 min-w-0 w-full break-words text-[11px] text-muted">Ceiling {BROWSER_DB_VALUE_MAX_BYTES_MAX / KB} KB per changed value.</p>
        </div>
      </div>

      <div className="flex min-w-0 w-full flex-col items-stretch gap-3 rounded-lg border border-border p-3 sm:flex-row sm:items-center">
        <div className="flex w-full sm:w-auto"><Switch checked={policy.public_read} onChange={(v) => onPolicyChange({ ...policy, public_read: v })} label="Public read" /></div>
        <div className="min-w-0 w-full break-words sm:w-auto sm:flex-1">
          <div className="text-sm font-medium">Allow anonymous read-only replicas</div>
          <div className="text-xs text-secondary">Off (default): only signed-in team members&apos; browsers get a grant. On: any visiting browser gets a read-only replica too — never write access.</div>
        </div>
      </div>
    </div>
  );
}

/** Clamp + PUT. Returns an error string, or "" on success. */
export async function saveBrowserDb(project: string, policy: BrowserDbPolicy): Promise<string> {
  if (!project.trim()) return "Choose a project.";
  const missingDdl = policy.schema.find((t) => !t.ddl.trim());
  if (missingDdl) return `Table "${missingDdl.name}" needs at least one column.`;
  try {
    await apiSend("PUT", `/v1/projects/${encodeURIComponent(project)}/browser-db`, {
      ...policy,
      max_bytes: Math.min(BROWSER_DB_MAX_BYTES_MAX, Math.max(1, policy.max_bytes || MB)),
      max_value_bytes: Math.min(BROWSER_DB_VALUE_MAX_BYTES_MAX, Math.max(1, policy.max_value_bytes || KB)),
    } satisfies BrowserDbPolicy);
    return "";
  } catch (e) {
    return String(e);
  }
}

/** Standalone edit modal (used from the SQLite database's detail page). */
export function BrowserDbConfigModal({
  projects,
  initialProject,
  allowProjectPick,
  initial,
  repoAuthored,
  onClose,
  onSaved,
}: {
  projects: string[];
  initialProject: string;
  allowProjectPick: boolean;
  initial?: BrowserDbPolicy;
  repoAuthored?: boolean;
  onClose: () => void;
  onSaved: (project: string) => void;
}) {
  const [project, setProject] = useState(initialProject || projects[0] || "");
  const [policy, setPolicy] = useState<BrowserDbPolicy>(initial ?? emptyPolicy());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function save() {
    setBusy(true);
    setError("");
    const err = await saveBrowserDb(project, policy);
    if (err) {
      setError(err);
      setBusy(false);
      return;
    }
    onSaved(project);
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <div className="flex min-w-0 max-h-[85vh] w-full max-w-xl flex-col overflow-hidden rounded-xl border border-border bg-card shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="flex min-w-0 w-full flex-col items-stretch gap-3 border-b border-border p-5 sm:flex-row sm:items-center sm:justify-between">
          <h2 className="min-w-0 w-full break-all text-lg font-semibold sm:w-auto">SQLite database</h2>
          <button onClick={onClose} className="flex w-full items-center justify-center text-muted hover:text-fg sm:w-auto"><X className="h-5 w-5" /></button>
        </div>
        <div className="min-w-0 w-full flex-1 overflow-y-auto p-5">
          <BrowserDbConfigForm
            projects={projects}
            project={project}
            onProjectChange={setProject}
            allowProjectPick={allowProjectPick}
            policy={policy}
            onPolicyChange={setPolicy}
            repoAuthored={repoAuthored}
          />
          {error && <p className="mt-4 min-w-0 w-full break-all text-sm text-red-500">{error}</p>}
        </div>
        <div className="flex min-w-0 w-full flex-col items-stretch gap-3 border-t border-border p-4 sm:flex-row sm:items-center sm:justify-between">
          <span className="min-w-0 w-full break-words text-xs text-muted sm:w-auto">A redeploy is required for changes to take effect.</span>
          <div className="flex w-full flex-col gap-2 sm:w-auto sm:flex-row">
            <Button variant="outline" onClick={onClose} className="w-full sm:w-auto">Cancel</Button>
            <Button onClick={save} disabled={busy} className="w-full sm:w-auto">
              {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />} Save
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}

/** This node's live replica figures for one project's SQLite database. */
export function BrowserDbStatusPanel({ project }: { project: string }) {
  const { data: status } = usePoll<BrowserDbStatusResponse>(
    `/v1/projects/${encodeURIComponent(project)}/browser-db/status`,
    10000,
  );
  if (!status) {
    return <div className="flex min-w-0 w-full flex-col gap-2 text-xs text-secondary sm:flex-row sm:items-center"><Loader2 className="h-3.5 w-3.5 animate-spin" /> Loading replica status…</div>;
  }
  if (!status.opted_in) {
    return <p className="min-w-0 w-full break-words text-xs text-secondary">Not opted in on this node yet — settings were just saved; redeploy to apply.</p>;
  }
  return (
    <div className="flex min-w-0 w-full flex-col gap-3">
      <div className="min-w-0 w-full">
        <div className="mb-1 flex min-w-0 w-full flex-col gap-1 text-xs text-secondary sm:flex-row sm:items-center sm:justify-between">
          <span className="min-w-0 w-full sm:w-auto">Replica size (this node)</span>
          <span className="min-w-0 w-full break-all sm:w-auto">{formatBytes(status.replica.bytes)} / {formatBytes(status.max_bytes)}</span>
        </div>
        <div className="h-1.5 min-w-0 w-full overflow-hidden rounded-full bg-border">
          <div
            className="h-full rounded-full bg-fg"
            style={{ width: `${Math.min(100, status.max_bytes ? (status.replica.bytes / status.max_bytes) * 100 : 0)}%` }}
          />
        </div>
      </div>
      <div className="flex min-w-0 w-full flex-col gap-x-6 gap-y-1 text-xs text-secondary sm:flex-row sm:flex-wrap">
        <span className="min-w-0 w-full break-words sm:w-auto">{status.replica.sites} replicated site{status.replica.sites === 1 ? "" : "s"}</span>
        <span className="min-w-0 w-full break-words sm:w-auto">{status.replica.last_modified_ms ? `last write ${timeAgo(status.replica.last_modified_ms)}` : "no writes yet"}</span>
        <span className="min-w-0 w-full break-words sm:w-auto">value cap {formatBytes(status.max_value_bytes)}</span>
        <span className="min-w-0 w-full break-all sm:w-auto">tables: {status.tables.join(", ") || "none"}</span>
      </div>
      {status.notes.length > 0 && (
        <p className="min-w-0 w-full break-all text-xs text-amber-600 dark:text-amber-400">{status.notes.join(" · ")}</p>
      )}
      <BrowserDbLiveStatus project={project} />
    </div>
  );
}
