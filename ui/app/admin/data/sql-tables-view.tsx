"use client";

import { useEffect, useState } from "react";
import { RefreshCw, Table2, Play } from "lucide-react";
import { opsGet, opsSend } from "@/lib/api";

// ---- "View as PostgreSQL" — the relational mirror, real typed tables instead
// of the document-collection JSON view. Read-only: no edit/create/delete
// affordances exist here at all (enforced server-side too — see admin.rs).
// Extracted into its own lazy-loaded chunk (see page.tsx's next/dynamic import):
// most Data Browser visits stay on the default Documents view, so this SQL
// client (its own state/effects, syntax-highlighted query box, results table)
// is pure unused weight in the initial bundle for the common case.
interface SqlColumn { name: string; type: string }
interface SqlTable { name: string; columns: SqlColumn[] }
interface SqlTablesResp { tables: SqlTable[] }
interface SqlQueryResp { fields: string[]; rows: any[][] }

function cell(v: any): string {
  if (v == null) return "—";
  if (typeof v === "object") return Array.isArray(v) ? `[${v.length}]` : "{…}";
  const s = String(v);
  return s.length > 60 ? s.slice(0, 57) + "…" : s;
}

export default function SqlTablesView() {
  const [sqlTables, setSqlTables] = useState<SqlTable[] | null>(null);
  const [activeTable, setActiveTable] = useState<string>("");
  const [sqlText, setSqlText] = useState("");
  const [sqlResult, setSqlResult] = useState<SqlQueryResp | null>(null);
  const [sqlLoading, setSqlLoading] = useState(false);
  const [sqlErr, setSqlErr] = useState("");

  function loadSqlTables() {
    opsGet<SqlTablesResp>("/v1/admin/sql/tables")
      .then((d) => {
        setSqlTables(d.tables || []);
        if (!activeTable && d.tables?.length) {
          setActiveTable(d.tables[0].name);
          setSqlText(`SELECT * FROM ${d.tables[0].name} LIMIT 200`);
        }
      })
      .catch((e) => setSqlErr(String(e)));
  }
  // Load the table list once, on mount (this component only mounts when the
  // Tables view is actually opened -- see page.tsx's conditional dynamic import).
  useEffect(() => { loadSqlTables(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  function selectTable(name: string) {
    setActiveTable(name);
    setSqlText(`SELECT * FROM ${name} LIMIT 200`);
    setSqlErr("");
    runSqlQuery(`SELECT * FROM ${name} LIMIT 200`);
  }

  async function runSqlQuery(sqlOverride?: string) {
    const sql = (sqlOverride ?? sqlText).trim();
    if (!sql) return;
    setSqlLoading(true);
    setSqlErr("");
    try {
      const res = await opsSend<SqlQueryResp>("POST", "/v1/admin/sql/query", { sql });
      setSqlResult(res);
    } catch (e) {
      setSqlErr(String(e));
      setSqlResult(null);
    } finally {
      setSqlLoading(false);
    }
  }
  // Run the default query as soon as the table list arrives with a selection.
  useEffect(() => { if (activeTable && !sqlResult && !sqlLoading) runSqlQuery(); }, [activeTable]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <>
      <div className="mb-3 flex justify-end">
        <button onClick={() => { loadSqlTables(); if (activeTable) runSqlQuery(); }} className="flex items-center gap-1.5 rounded-md border border-border-strong px-3 py-1.5 text-sm hover:bg-subtle">
          <RefreshCw className="h-3.5 w-3.5" /> Refresh
        </button>
      </div>
      <div className="grid grid-cols-1 gap-5 lg:grid-cols-[220px_1fr]">
        {/* Tables list */}
        <aside className="flex flex-col gap-1">
          {(sqlTables ?? []).map((t) => (
            <button
              key={t.name}
              onClick={() => selectTable(t.name)}
              className={`flex items-center gap-2 rounded-md px-3 py-2 text-sm transition-colors ${activeTable === t.name ? "bg-card font-medium text-fg shadow-card" : "text-secondary hover:bg-card hover:text-fg"}`}
            >
              <Table2 className="h-3.5 w-3.5" /> {t.name}
            </button>
          ))}
          {sqlTables === null && <div className="px-3 py-2 text-sm text-muted">Loading…</div>}
          {sqlTables?.length === 0 && <div className="px-3 py-2 text-sm text-muted">No relational tables.</div>}
        </aside>

        {/* Query + results */}
        <div className="min-w-0">
          <form onSubmit={(e) => { e.preventDefault(); runSqlQuery(); }} className="mb-3 flex items-start gap-2">
            <textarea
              value={sqlText}
              onChange={(e) => setSqlText(e.target.value)}
              spellCheck={false}
              rows={2}
              placeholder="SELECT * FROM project_teams LIMIT 200"
              className="flex-1 resize-none rounded-md border border-border bg-card p-2.5 font-mono text-xs leading-relaxed focus:border-border-strong focus:outline-none"
            />
            <button type="submit" disabled={sqlLoading} className="flex shrink-0 items-center gap-1.5 rounded-md bg-accent px-3 py-2.5 text-sm font-medium text-accent-fg hover:opacity-90 disabled:opacity-50">
              <Play className="h-3.5 w-3.5" /> Run
            </button>
          </form>
          <p className="mb-2 text-xs text-muted">Read-only — SELECT only. Editing/creating/deleting relational data isn&apos;t exposed here; use the normal Documents view or the project/billing UIs.</p>

          {sqlErr && <p className="mb-2 text-xs text-red-500">{sqlErr}</p>}

          <div className="overflow-x-auto rounded-xl border border-border bg-card">
            <table className="w-full text-sm">
              <thead>
                <tr>
                  {(sqlResult?.fields ?? []).map((f) => (
                    <th key={f} className="whitespace-nowrap border-b border-border px-3 py-2 text-left text-xs font-medium uppercase tracking-wide text-muted">{f}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {(sqlResult?.rows ?? []).map((row, i) => (
                  <tr key={i} className="hover:bg-subtle/50">
                    {row.map((v, j) => (
                      <td key={j} className="whitespace-nowrap border-b border-border px-3 py-2 font-mono text-xs text-fg">{cell(v)}</td>
                    ))}
                  </tr>
                ))}
                {sqlResult && sqlResult.rows.length === 0 && (
                  <tr><td colSpan={Math.max(1, sqlResult.fields.length)} className="px-3 py-10 text-center text-sm text-secondary">No rows.</td></tr>
                )}
                {sqlLoading && !sqlResult && (
                  <tr><td className="px-3 py-10 text-center text-sm text-secondary">Loading…</td></tr>
                )}
              </tbody>
            </table>
          </div>
        </div>
      </div>
    </>
  );
}
