"use client";

import { useEffect, useMemo, useState } from "react";
import { Search, Database, RefreshCw, ChevronRight, Code2 } from "lucide-react";
import { apiGet } from "@/lib/api";

interface Collection { name: string; count: number }
interface CollectionsResp { collections: Collection[]; store: string }
interface RowsResp { collection: string; total: number; matched: number; rows: any[] }
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

  function loadCollections() {
    apiGet<CollectionsResp>("/v1/admin/data").then(setMeta).catch(() => {});
    apiGet<{ namespaces: Namespace[] }>("/v1/admin/namespaces").then((d) => setNamespaces(d.namespaces || [])).catch(() => {});
  }
  useEffect(() => { loadCollections(); }, []);

  function loadRows(col: string, query: string) {
    setLoading(true);
    setSelected(null);
    apiGet<RowsResp>(`/v1/admin/data/${encodeURIComponent(col)}?q=${encodeURIComponent(query)}&limit=500`)
      .then(setRows)
      .catch(() => setRows({ collection: col, total: 0, matched: 0, rows: [] }))
      .finally(() => setLoading(false));
  }
  useEffect(() => { loadRows(active, ""); setQ(""); }, [active]);

  // Derive table columns from the union of keys across rows (scalars first).
  const columns = useMemo(() => {
    const r = rows?.rows ?? [];
    const keys = new Set<string>();
    for (const row of r.slice(0, 50)) {
      if (row && typeof row === "object") Object.keys(row).forEach((k) => keys.add(k));
    }
    const all = Array.from(keys);
    // Put common id/name fields first.
    const pri = ["id", "name", "project", "slug", "team", "kind", "status", "state"];
    all.sort((a, b) => (pri.indexOf(a) + 1 || 99) - (pri.indexOf(b) + 1 || 99) || a.localeCompare(b));
    return all.slice(0, 7);
  }, [rows]);

  function cell(v: any): string {
    if (v == null) return "—";
    if (typeof v === "object") return Array.isArray(v) ? `[${v.length}]` : "{…}";
    const s = String(v);
    return s.length > 60 ? s.slice(0, 57) + "…" : s;
  }

  return (
    <div className="p-6 sm:p-8">
      <div className="mb-6 flex items-end justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Data Browser</h1>
          <p className="mt-1 text-sm text-secondary">
            Query and explore the platform&apos;s underlying store{meta ? ` · ${meta.store}` : ""}.
          </p>
        </div>
        <button onClick={() => { loadCollections(); loadRows(active, q); }} className="flex items-center gap-1.5 rounded-md border border-border-strong px-3 py-1.5 text-sm hover:bg-subtle">
          <RefreshCw className="h-3.5 w-3.5" /> Refresh
        </button>
      </div>

      {/* Tenant namespaces — the multi-tenant partition of the store. */}
      {namespaces.length > 0 && (
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

      <div className="grid grid-cols-1 gap-5 lg:grid-cols-[220px_1fr]">
        {/* Collections list */}
        <aside className="flex flex-col gap-1">
          {(meta?.collections ?? []).map((c) => (
            <button
              key={c.name}
              onClick={() => setActive(c.name)}
              className={`flex items-center justify-between rounded-md px-3 py-2 text-sm transition-colors ${active === c.name ? "bg-card font-medium text-fg shadow-card" : "text-secondary hover:bg-card hover:text-fg"}`}
            >
              <span className="flex items-center gap-2"><Database className="h-3.5 w-3.5" /> {LABELS[c.name] ?? c.name}</span>
              <span className="rounded-full bg-subtle px-1.5 py-0.5 text-[10px] tabular-nums text-muted">{c.count}</span>
            </button>
          ))}
          {!meta && <div className="px-3 py-2 text-sm text-muted">Loading…</div>}
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

          <div className="mb-2 flex items-center gap-3 text-xs text-muted">
            <Code2 className="h-3.5 w-3.5" />
            {rows ? `${rows.matched} of ${rows.total} rows${rows.matched > rows.rows.length ? ` (showing ${rows.rows.length})` : ""}` : "—"}
            {loading && <RefreshCw className="h-3 w-3 animate-spin" />}
          </div>

          <div className="overflow-x-auto rounded-xl border border-border bg-card">
            <table className="w-full text-sm">
              <thead>
                <tr>
                  {columns.map((c) => (
                    <th key={c} className="whitespace-nowrap border-b border-border px-3 py-2 text-left text-xs font-medium uppercase tracking-wide text-muted">{c}</th>
                  ))}
                  <th className="border-b border-border px-3 py-2"></th>
                </tr>
              </thead>
              <tbody>
                {(rows?.rows ?? []).map((row, i) => (
                  <tr key={i} className="cursor-pointer hover:bg-subtle/50" onClick={() => setSelected(row)}>
                    {columns.map((c) => (
                      <td key={c} className="whitespace-nowrap border-b border-border px-3 py-2 font-mono text-xs text-fg">{cell(row?.[c])}</td>
                    ))}
                    <td className="border-b border-border px-3 py-2 text-right"><ChevronRight className="h-3.5 w-3.5 text-muted" /></td>
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

      {/* Row detail drawer */}
      {selected && (
        <div className="fixed inset-0 z-50 flex justify-end bg-black/40" onClick={() => setSelected(null)}>
          <div className="h-full w-[560px] max-w-[92vw] overflow-y-auto border-l border-border bg-card p-5" onClick={(e) => e.stopPropagation()}>
            <div className="mb-3 flex items-center justify-between">
              <span className="font-semibold">Row · {LABELS[active] ?? active}</span>
              <button onClick={() => setSelected(null)} className="text-muted hover:text-fg">✕</button>
            </div>
            <pre className="overflow-x-auto rounded-lg border border-border bg-subtle/50 p-3 font-mono text-xs leading-relaxed">
              {JSON.stringify(selected, null, 2)}
            </pre>
          </div>
        </div>
      )}
    </div>
  );
}
