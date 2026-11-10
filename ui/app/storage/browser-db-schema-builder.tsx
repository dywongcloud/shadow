"use client";

import { useState } from "react";
import { Plus, Trash2, Code2, Table2 } from "lucide-react";
import { Button, Input } from "@/components/ui";
import { cn } from "@/lib/utils";
import type { BrowserDbTable } from "@/lib/api";

/** One column in the friendly table builder; only the generated DDL goes on the wire (`fluid_core::BrowserDbTable` is `{ name, ddl }`). */
export type ColumnDraft = { name: string; type: "TEXT" | "INTEGER" | "REAL" | "BLOB"; pk: boolean };

export function ddlFromColumns(tableName: string, columns: ColumnDraft[]): string {
  const cols = columns
    .filter((c) => c.name.trim())
    .map((c) => `${c.name.trim()} ${c.type}${c.pk ? " PRIMARY KEY" : ""}`);
  if (cols.length === 0) return "";
  return `CREATE TABLE IF NOT EXISTS ${tableName.trim()} (${cols.join(", ")})`;
}

const defaultColumns = (): ColumnDraft[] => [
  { name: "id", type: "INTEGER", pk: true },
  { name: "value", type: "TEXT", pk: false },
];

function markSoleKey(columns: ColumnDraft[], keyIndex: number, checked: boolean): ColumnDraft[] {
  return columns.map((c, j) => ({ ...c, pk: j === keyIndex && checked }));
}

/** One table row in the schema builder: a column editor (default) or a raw-DDL textarea for what columns cannot express (foreign keys, `CHECK`, `UNIQUE`, generated
 *  columns); switching to raw keeps the DDL the column builder generated so far. */
export function SchemaTableEditor({
  table,
  onChange,
  onRemove,
}: {
  table: BrowserDbTable;
  onChange: (t: BrowserDbTable) => void;
  onRemove: () => void;
}) {
  const [raw, setRaw] = useState(false);
  const [columns, setColumns] = useState<ColumnDraft[]>(defaultColumns());

  function setColumnsAndDdl(next: ColumnDraft[]) {
    setColumns(next);
    onChange({ ...table, ddl: ddlFromColumns(table.name, next) });
  }

  return (
    <div className="min-w-0 w-full rounded-lg border border-border p-3">
      <div className="mb-2 flex min-w-0 w-full flex-col items-stretch gap-2 sm:flex-row sm:items-center">
        <Input
          value={table.name}
          onChange={(e) => {
            const name = e.target.value.replace(/[^A-Za-z0-9_]/g, "");
            onChange({ ...table, name, ddl: raw ? table.ddl : ddlFromColumns(name, columns) });
          }}
          placeholder="table_name"
          className="min-w-0 w-full break-all font-mono sm:max-w-[220px]"
        />
        <button
          type="button"
          onClick={() => setRaw((r) => !r)}
          className="flex min-w-0 w-full items-center justify-center gap-1 rounded-md border border-border px-2 py-1 text-xs text-secondary hover:bg-subtle sm:ml-auto sm:w-auto"
          title={raw ? "Switch to column builder" : "Edit raw DDL"}
        >
          {raw ? <Table2 className="h-3.5 w-3.5" /> : <Code2 className="h-3.5 w-3.5" />}
          {raw ? "Columns" : "Raw DDL"}
        </button>
        <button type="button" onClick={onRemove} className="flex min-w-0 w-full items-center justify-center text-muted hover:text-red-500 sm:w-auto" title="Remove table">
          <Trash2 className="h-4 w-4" />
        </button>
      </div>

      {raw ? (
        <textarea
          value={table.ddl}
          onChange={(e) => onChange({ ...table, ddl: e.target.value })}
          rows={3}
          placeholder="CREATE TABLE IF NOT EXISTS my_table (id INTEGER PRIMARY KEY, value TEXT)"
          className="min-w-0 w-full break-all rounded-md border border-border bg-card px-3 py-2 font-mono text-xs focus:outline-none focus:ring-2 focus:ring-border"
        />
      ) : (
        <div className="flex min-w-0 w-full flex-col gap-1.5">
          {columns.map((c, i) => (
            <div key={i} className="flex min-w-0 w-full flex-col items-stretch gap-1.5 sm:flex-row sm:items-center">
              <Input
                value={c.name}
                onChange={(e) => {
                  const next = columns.slice();
                  next[i] = { ...c, name: e.target.value.replace(/[^A-Za-z0-9_]/g, "") };
                  setColumnsAndDdl(next);
                }}
                placeholder="column_name"
                className="min-w-0 w-full flex-1 break-all font-mono"
              />
              <select
                value={c.type}
                onChange={(e) => {
                  const next = columns.slice();
                  next[i] = { ...c, type: e.target.value as ColumnDraft["type"] };
                  setColumnsAndDdl(next);
                }}
                className="min-w-0 w-full rounded-md border border-border bg-card px-2 py-2 text-xs focus:outline-none focus:ring-2 focus:ring-border sm:w-auto"
              >
                {(["TEXT", "INTEGER", "REAL", "BLOB"] as const).map((t) => (
                  <option key={t} value={t}>{t}</option>
                ))}
              </select>
              <label className={cn("flex min-w-0 w-full items-center justify-center gap-1 rounded-md border px-2 py-1.5 text-xs sm:w-auto", c.pk ? "border-link text-link" : "border-border text-muted")}>
                <input
                  type="checkbox"
                  checked={c.pk}
                  onChange={(e) => setColumnsAndDdl(markSoleKey(columns, i, e.target.checked))}
                  className="h-3 w-3"
                />
                PK
              </label>
              <button
                type="button"
                onClick={() => setColumnsAndDdl(columns.filter((_, j) => j !== i))}
                className="flex min-w-0 w-full items-center justify-center text-muted hover:text-red-500 sm:w-auto"
                disabled={columns.length <= 1}
              >
                <Trash2 className="h-3.5 w-3.5" />
              </button>
            </div>
          ))}
          <button
            type="button"
            onClick={() => setColumnsAndDdl([...columns, { name: "", type: "TEXT", pk: false }])}
            className="mt-1 flex min-w-0 w-full items-center justify-center gap-1 text-xs text-link hover:underline sm:w-auto"
          >
            <Plus className="h-3 w-3" /> Add column
          </button>
          {table.ddl && <div className="mt-1.5 min-w-0 w-full break-all font-mono text-[11px] text-muted" title={table.ddl}>{table.ddl}</div>}
        </div>
      )}
    </div>
  );
}
