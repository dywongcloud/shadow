"use client";

import { useEffect, useMemo, useState } from "react";
import dynamic from "next/dynamic";
import { Search, Database, RefreshCw, ChevronRight, Code2, Plus, Trash2, Save, Pencil, Table2 } from "lucide-react";
import { opsGet, opsSend } from "@/lib/api";

interface Collection { name: string; count: number; editable?: boolean }
interface CollectionsResp { collections: Collection[]; store: string }
interface RowsResp { collection: string; total: number; matched: number; rows: any[]; editable?: boolean }

type BrowserView = "documents" | "tables";

// The "Tables (PostgreSQL)" view (its own state/effects/SQL client) is lazy
// loaded -- most Data Browser visits stay on the default Documents view, so
// this chunk (and its bundled query/results UI) is only fetched when a user
// actually switches to it.
const SqlTablesView = dynamic(() => import("./sql-tables-view"), {
  ssr: false,
  loading: () => <div className="py-10 text-center text-sm text-secondary">Loading…</div>,
});

const DELETABLE_TYPED = new Set(["deployments", "databases", "secure_links", "webhooks"]);
interface Namespace { namespace: string; projects: number; deployments: number; databases: number; api_keys: number; webhooks: number }

const LABELS: Record<string, string> = {
  deployments: "Deployments",
  projects: "Projects",
  teams: "Teams",
  databases: "Databases",
  secure_links: "Secure Links",
  api_keys: "API Keys",
  builds: "Builds",
  workflow_defs: "Workflow Defs",
  workflow_runs: "Workflow Runs",
  incidents: "Incidents",
  webhooks: "Webhooks",
  events: "Events",
};

export default function DataBrowserPage() {
  const [meta, setMeta] = useState<CollectionsResp | null>(null);
  const [namespaces, setNamespaces] = useState<Namespace[]>([]);
  const [active, setActive] = useState<string>("deployments");
  const [rows, setRows] = useState<RowsResp | null>(null);
  const [q, setQ] = useState("");
  const [loading, setLoading] = useState(false);
  const [selected, setSelected] = useState<any | null>(null);
  const [editText, setEditText] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const [newOpen, setNewOpen] = useState(false);
  const [newCollection, setNewCollection] = useState("documents");
  const [newBody, setNewBody] = useState('{\n  "name": "",\n  "value": ""\n}');
  // Inline cell editing (editable collections only).
  const [editCell, setEditCell] = useState<{ id: string; col: string } | null>(null);
  const [cellDraft, setCellDraft] = useState("");
  // "Edit mode": all editable fields become inputs at once, saved together.
  const [editMode, setEditMode] = useState(false);
  const [drafts, setDrafts] = useState<Record<string, Record<string, string>>>({});

  // Documents vs. Tables (PostgreSQL) — the Tables view's own state/effects
  // now live entirely in the lazy-loaded SqlTablesView component.
  const [view, setView] = useState<BrowserView>("documents");

  const editable = !!rows?.editable;
  const deletable = editable || DELETABLE_TYPED.has(active);

  // System/metadata fields are never user-editable (the docstore ignores id /
  // collection / created_ms on patch; tenant + updated_ms are managed too).
  const SYS_FIELDS = new Set(["id", "collection", "tenant", "created_ms", "updated_ms"]);

  function isScalar(v: any): boolean {
    return v == null || typeof v === "string" || typeof v === "number" || typeof v === "boolean";
  }

  function beginCell(row: any, col: string) {
    if (!editable || SYS_FIELDS.has(col) || !isScalar(row?.[col])) return;
    setEditCell({ id: row.id, col });
    setCellDraft(row?.[col] == null ? "" : String(row[col]));
    setErr("");
  }

  /** Coerce the textual draft into a JSON scalar (number/bool/null/string). */
  function coerce(raw: string): any {
    const t = raw.trim();
    if (t === "") return "";
    if (t === "true") return true;
    if (t === "false") return false;
    if (t === "null") return null;
    if (/^-?\d+(\.\d+)?$/.test(t)) return Number(t);
    return raw;
  }

  async function saveCell() {
    if (!editCell) return;
    setBusy(true);
    setErr("");
    try {
      // data_patch MERGES, so sending just this field is non-destructive.
      await opsSend("PUT", `/v1/admin/data/${encodeURIComponent(active)}/${encodeURIComponent(editCell.id)}`, {
        [editCell.col]: coerce(cellDraft),
      });
      setEditCell(null);
      loadRows(active, q);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }

  function loadCollections() {
    opsGet<CollectionsResp>("/v1/admin/data").then(setMeta).catch(() => {});
    opsGet<{ namespaces: Namespace[] }>("/v1/admin/namespaces").then((d) => setNamespaces(d.namespaces || [])).catch(() => {});
  }
  useEffect(() => { loadCollections(); }, []);

  function loadRows(col: string, query: string) {
    setLoading(true);
    setSelected(null);
    opsGet<RowsResp>(`/v1/admin/data/${encodeURIComponent(col)}?q=${encodeURIComponent(query)}&limit=500`)
      .then(setRows)
      .catch(() => setRows({ collection: col, total: 0, matched: 0, rows: [] }))
      .finally(() => setLoading(false));
  }
  // Local editing state (search query, edit mode, drafts, cell selection)
  // resets whenever the collection changes -- but that's a response to the
  // user's OWN table-switch actions (both setActive call sites below), so it
  // belongs at those event sites, not synced from an effect keyed on `active`.
  useEffect(() => { loadRows(active, ""); }, [active]);
  function switchActive(name: string) {
    setQ("");
    setEditMode(false);
    setDrafts({});
    setEditCell(null);
    setActive(name);
  }

  function setDraft(rowId: string, col: string, val: string) {
    setDrafts((d) => ({ ...d, [rowId]: { ...(d[rowId] || {}), [col]: val } }));
  }
  async function saveAll() {
    setBusy(true); setErr("");
    try {
      for (const id of Object.keys(drafts)) {
        const patch: Record<string, any> = {};
        for (const [col, raw] of Object.entries(drafts[id])) patch[col] = coerce(raw);
        if (Object.keys(patch).length) {
          await opsSend("PUT", `/v1/admin/data/${encodeURIComponent(active)}/${encodeURIComponent(id)}`, patch);
        }
      }
      setDrafts({}); setEditMode(false); loadRows(active, q);
    } catch (e) { setErr(String(e)); } finally { setBusy(false); }
  }
  function cancelEdit() { setDrafts({}); setEditMode(false); }
  const draftCount = Object.values(drafts).reduce((n, m) => n + Object.keys(m).length, 0);

  // Derive table columns from the union of keys across rows (scalars first).
  const columns = useMemo(() => {
    const r = rows?.rows ?? [];
    const keys = new Set<string>();
    for (const row of r.slice(0, 50)) {
      if (row && typeof row === "object") Object.keys(row).forEach((k) => keys.add(k));
    }
    const all = Array.from(keys);
    // Identity fields first, then real data fields, then doc metadata last — so
    // user data (e.g. `value`) is never crowded out by created_ms/tenant/etc.
    const pri = ["id", "name", "project", "slug", "team", "kind", "status", "state"];
    const sys = ["collection", "tenant", "created_ms", "updated_ms"];
    const rank = (k: string) => {
      const p = pri.indexOf(k);
      if (p >= 0) return p;
      if (sys.includes(k)) return 90;
      return 50;
    };
    all.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
    return all.slice(0, 8);
  }, [rows]);

  function cell(v: any): string {
    if (v == null) return "—";
    if (typeof v === "object") return Array.isArray(v) ? `[${v.length}]` : "{…}";
    const s = String(v);
    return s.length > 60 ? s.slice(0, 57) + "…" : s;
  }

  function openRow(row: any) {
    setSelected(row);
    setEditText(JSON.stringify(row, null, 2));
    setErr("");
  }

  async function createDoc() {
    setErr("");
    if (!newCollection.trim()) { setErr("Enter a collection name."); return; }
    setBusy(true);
    try {
      const body = JSON.parse(newBody);
      await opsSend("POST", `/v1/admin/data/${encodeURIComponent(newCollection)}`, body);
      setNewOpen(false);
      loadCollections();
      switchActive(newCollection);
      loadRows(newCollection, "");
    } catch (e) { setErr(e instanceof SyntaxError ? "Invalid JSON" : String(e)); }
    finally { setBusy(false); }
  }

  async function saveEdit() {
    if (!selected?.id) return;
    setErr(""); setBusy(true);
    try {
      const body = JSON.parse(editText);
      await opsSend("PUT", `/v1/admin/data/${encodeURIComponent(active)}/${encodeURIComponent(selected.id)}`, body);
      setSelected(null);
      loadRows(active, q);
    } catch (e) { setErr(e instanceof SyntaxError ? "Invalid JSON" : String(e)); }
    finally { setBusy(false); }
  }

  async function deleteRow(id: string) {
    if (!id || !confirm("Delete this entry? This cannot be undone.")) return;
    setBusy(true);
    try {
      await opsSend("DELETE", `/v1/admin/data/${encodeURIComponent(active)}/${encodeURIComponent(id)}`);
      setSelected(null);
      loadCollections();
      loadRows(active, q);
    } catch (e) { setErr(String(e)); }
    finally { setBusy(false); }
  }

  return (
    <div className="p-6 sm:p-8">
      <div className="mb-6 flex items-end justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Data Browser</h1>
          <p className="mt-1 text-sm text-secondary">
            {view === "documents"
              ? <>Query and explore the platform&apos;s underlying store{meta ? ` · ${meta.store}` : ""}.</>
              : <>Read-only view of the relational mirror (guardian-db&apos;s native SQL layer) as real PostgreSQL tables.</>}
          </p>
        </div>
        <div className="flex items-center gap-3">
          <div className="flex rounded-md border border-border-strong p-0.5 text-sm">
            <button
              onClick={() => setView("documents")}
              className={`flex items-center gap-1.5 rounded px-2.5 py-1 ${view === "documents" ? "bg-card font-medium text-fg shadow-card" : "text-secondary hover:text-fg"}`}
            >
              <Code2 className="h-3.5 w-3.5" /> Documents
            </button>
            <button
              onClick={() => setView("tables")}
              className={`flex items-center gap-1.5 rounded px-2.5 py-1 ${view === "tables" ? "bg-card font-medium text-fg shadow-card" : "text-secondary hover:text-fg"}`}
            >
              <Table2 className="h-3.5 w-3.5" /> Tables (PostgreSQL)
            </button>
          </div>
          {view === "documents" && (
            <div className="flex gap-2">
              <button onClick={() => { setNewCollection("documents"); setNewOpen(true); }} className="flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90">
                <Plus className="h-3.5 w-3.5" /> New
              </button>
              <button onClick={() => { loadCollections(); loadRows(active, q); }} className="flex items-center gap-1.5 rounded-md border border-border-strong px-3 py-1.5 text-sm hover:bg-subtle">
                <RefreshCw className="h-3.5 w-3.5" /> Refresh
              </button>
            </div>
          )}
        </div>
      </div>

      {/* Tenant namespaces — the multi-tenant partition of the store. */}
      {view === "documents" && namespaces.length > 0 && (
        <div className="mb-5">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">Tenant namespaces ({namespaces.length})</div>
          <div className="flex flex-wrap gap-2">
            {namespaces.map((n) => (
              <div key={n.namespace} className="flex items-center gap-2 rounded-lg border border-border bg-card px-3 py-1.5 text-xs">
                <span className={`font-mono font-medium ${n.namespace === "_global" ? "text-amber-500" : "text-fg"}`}>{n.namespace}</span>
                <span className="text-muted">{n.projects}p · {n.deployments}d · {n.databases}db</span>
              </div>
            ))}
          </div>
        </div>
      )}

      {view === "tables" ? (
        <SqlTablesView />
      ) : (
      <div className="grid grid-cols-1 gap-5 lg:grid-cols-[220px_1fr]">
        {/* Collections list */}
        <aside className="flex flex-col gap-1">
          {(meta?.collections ?? []).map((c) => (
            <button
              key={c.name}
              onClick={() => switchActive(c.name)}
              className={`flex items-center justify-between rounded-md px-3 py-2 text-sm transition-colors ${active === c.name ? "bg-card font-medium text-fg shadow-card" : "text-secondary hover:bg-card hover:text-fg"}`}
            >
              <span className="flex items-center gap-2"><Database className="h-3.5 w-3.5" /> {LABELS[c.name] ?? c.name}</span>
              <span className="rounded-full bg-subtle px-1.5 py-0.5 text-[10px] tabular-nums text-muted">{c.count}</span>
            </button>
          ))}
          {!meta && <div className="px-3 py-2 text-sm text-muted">Loading…</div>}
          <button
            onClick={() => { setNewCollection(""); setNewBody('{\n  "name": "",\n  "value": ""\n}'); setErr(""); setNewOpen(true); }}
            className="mt-1 flex items-center gap-2 rounded-md border border-dashed border-border-strong px-3 py-2 text-sm text-secondary hover:bg-card hover:text-fg"
          >
            <Plus className="h-3.5 w-3.5" /> New collection
          </button>
        </aside>

        {/* Rows */}
        <div className="min-w-0">
          <form
            onSubmit={(e) => { e.preventDefault(); loadRows(active, q); }}
            className="relative mb-3"
          >
            <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
            <input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder={`Query ${LABELS[active] ?? active}… (substring match across all fields)`}
              className="w-full rounded-md border border-border bg-card py-2 pl-9 pr-3 text-sm focus:border-border-strong focus:outline-none"
            />
          </form>

          <div className="mb-2 flex items-center justify-between gap-3 text-xs text-muted">
            <span className="flex items-center gap-2">
              <Code2 className="h-3.5 w-3.5" />
              {rows ? `${rows.matched} of ${rows.total} rows${rows.matched > rows.rows.length ? ` (showing ${rows.rows.length})` : ""}` : "—"}
              {loading && <RefreshCw className="h-3 w-3 animate-spin" />}
            </span>
            <span className="flex items-center gap-2">
              {editable ? (
                editMode ? (
                  <>
                    <span className="text-amber-500">{draftCount} unsaved field{draftCount === 1 ? "" : "s"}</span>
                    <button onClick={cancelEdit} disabled={busy} className="flex items-center gap-1 rounded-md border border-border-strong px-2 py-1 text-xs hover:bg-subtle">
                      Cancel
                    </button>
                    <button onClick={saveAll} disabled={busy || draftCount === 0} className="flex items-center gap-1 rounded-md bg-accent px-2 py-1 text-xs font-medium text-accent-fg hover:opacity-90 disabled:opacity-50">
                      <Save className="h-3 w-3" /> Save changes
                    </button>
                  </>
                ) : (
                  <>
                    <button onClick={() => { setEditMode(true); setEditCell(null); }} className="flex items-center gap-1 rounded-md border border-border-strong px-2 py-1 text-xs hover:bg-subtle">
                      <Pencil className="h-3 w-3" /> Edit mode
                    </button>
                    <button
                      onClick={() => { setNewCollection(active); setNewBody('{\n  "name": "",\n  "value": ""\n}'); setErr(""); setNewOpen(true); }}
                      className="flex items-center gap-1 rounded-md border border-border-strong px-2 py-1 text-xs hover:bg-subtle"
                    >
                      <Plus className="h-3 w-3" /> Add document
                    </button>
                  </>
                )
              ) : (
                <span className="text-muted">Read-only managed collection</span>
              )}
            </span>
          </div>

          <div className="overflow-x-auto rounded-xl border border-border bg-card">
            <table className="w-full text-sm">
              <thead>
                <tr>
                  {columns.map((c) => (
                    <th key={c} className="whitespace-nowrap border-b border-border px-3 py-2 text-left text-xs font-medium uppercase tracking-wide text-muted">{c}</th>
                  ))}
                  <th className="border-b border-border px-3 py-2 text-right text-xs font-medium uppercase tracking-wide text-muted">Actions</th>
                </tr>
              </thead>
              <tbody>
                {(rows?.rows ?? []).map((row, i) => (
                  <tr key={row?.id ?? i} className="group hover:bg-subtle/50">
                    {columns.map((c) => {
                      const isEditing = editable && editCell?.id === row?.id && editCell?.col === c;
                      const canInline = editable && !SYS_FIELDS.has(c) && isScalar(row?.[c]);
                      // In edit mode, every editable cell is an input bound to drafts.
                      if (editMode && canInline && row?.id) {
                        const cur = drafts[row.id]?.[c];
                        return (
                          <td key={c} className="whitespace-nowrap border-b border-border px-2 py-1.5">
                            <input
                              value={cur ?? (row?.[c] == null ? "" : String(row[c]))}
                              onChange={(e) => setDraft(row.id, c, e.target.value)}
                              className={`w-full min-w-[90px] rounded border bg-bg px-1.5 py-0.5 font-mono text-xs outline-none focus:border-accent ${cur !== undefined ? "border-amber-500/60" : "border-border-strong"}`}
                            />
                          </td>
                        );
                      }
                      return (
                        <td
                          key={c}
                          onClick={() => (editMode ? null : canInline ? beginCell(row, c) : openRow(row))}
                          className={`whitespace-nowrap border-b border-border px-3 py-2 font-mono text-xs text-fg ${!editMode && canInline ? "cursor-text hover:bg-accent/5" : !editMode ? "cursor-pointer" : ""}`}
                        >
                          {isEditing ? (
                            <input
                              autoFocus
                              value={cellDraft}
                              onChange={(e) => setCellDraft(e.target.value)}
                              onClick={(e) => e.stopPropagation()}
                              onBlur={saveCell}
                              onKeyDown={(e) => {
                                if (e.key === "Enter") saveCell();
                                if (e.key === "Escape") setEditCell(null);
                              }}
                              className="w-full min-w-[80px] rounded border border-border-strong bg-bg px-1.5 py-0.5 font-mono text-xs outline-none focus:border-accent"
                            />
                          ) : (
                            cell(row?.[c])
                          )}
                        </td>
                      );
                    })}
                    <td className="border-b border-border px-3 py-2">
                      <div className="flex items-center justify-end gap-1">
                        <button
                          title="View / edit JSON"
                          onClick={(e) => { e.stopPropagation(); openRow(row); }}
                          className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-subtle hover:text-fg"
                        >
                          <ChevronRight className="h-3.5 w-3.5" />
                        </button>
                        {deletable && row?.id ? (
                          <button
                            title="Delete entry"
                            onClick={(e) => { e.stopPropagation(); deleteRow(row.id); }}
                            className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-red-500/10 hover:text-red-500"
                          >
                            <Trash2 className="h-3.5 w-3.5" />
                          </button>
                        ) : null}
                      </div>
                    </td>
                  </tr>
                ))}
                {rows && rows.rows.length === 0 && (
                  <tr><td colSpan={columns.length + 1} className="px-3 py-10 text-center text-sm text-secondary">No rows.</td></tr>
                )}
              </tbody>
            </table>
          </div>
        </div>
      </div>
      )}

      {/* Row detail / edit drawer */}
      {selected && (
        <div className="fixed inset-0 z-50 flex justify-end bg-black/40" onClick={() => setSelected(null)}>
          <div className="flex h-full w-[560px] max-w-[92vw] flex-col border-l border-border bg-card p-5" onClick={(e) => e.stopPropagation()}>
            <div className="mb-3 flex items-center justify-between">
              <span className="font-semibold">{editable ? "Edit row" : "Row"} · {LABELS[active] ?? active}</span>
              <button onClick={() => setSelected(null)} className="text-muted hover:text-fg">✕</button>
            </div>
            {editable ? (
              <textarea
                value={editText}
                onChange={(e) => setEditText(e.target.value)}
                spellCheck={false}
                className="flex-1 resize-none rounded-lg border border-border bg-subtle/40 p-3 font-mono text-xs leading-relaxed focus:border-border-strong focus:outline-none"
              />
            ) : (
              <pre className="flex-1 overflow-auto rounded-lg border border-border bg-subtle/50 p-3 font-mono text-xs leading-relaxed">{JSON.stringify(selected, null, 2)}</pre>
            )}
            {err && <p className="mt-2 text-xs text-red-500">{err}</p>}
            <div className="mt-3 flex items-center justify-between">
              {deletable ? (
                <button onClick={() => deleteRow(selected.id)} disabled={busy} className="flex items-center gap-1.5 rounded-md border border-red-500/40 px-3 py-1.5 text-sm text-red-500 hover:bg-red-500/10">
                  <Trash2 className="h-3.5 w-3.5" /> Delete
                </button>
              ) : <span />}
              {editable && (
                <button onClick={saveEdit} disabled={busy} className="flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90">
                  <Save className="h-3.5 w-3.5" /> Save
                </button>
              )}
            </div>
          </div>
        </div>
      )}

      {/* Create document modal */}
      {newOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={() => setNewOpen(false)}>
          <div className="w-full max-w-lg rounded-xl border border-border bg-card p-5" onClick={(e) => e.stopPropagation()}>
            <div className="mb-3 text-base font-semibold">New document</div>
            <label className="mb-1 block text-xs text-secondary">Collection</label>
            <input value={newCollection} placeholder="collection-name" onChange={(e) => setNewCollection(e.target.value.replace(/[^a-z0-9_-]/gi, "_"))} className="mb-3 w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none" />
            <label className="mb-1 block text-xs text-secondary">JSON body</label>
            <textarea value={newBody} onChange={(e) => setNewBody(e.target.value)} spellCheck={false} rows={8} className="w-full resize-none rounded-md border border-border bg-subtle/40 p-3 font-mono text-xs focus:border-border-strong focus:outline-none" />
            {err && <p className="mt-2 text-xs text-red-500">{err}</p>}
            <p className="mt-2 text-xs text-muted">Documents are stored in their own editable collection (managed platform collections stay read-only).</p>
            <div className="mt-4 flex justify-end gap-2">
              <button onClick={() => setNewOpen(false)} className="rounded-md border border-border-strong px-3 py-1.5 text-sm hover:bg-subtle">Cancel</button>
              <button onClick={createDoc} disabled={busy} className="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90">Create</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
