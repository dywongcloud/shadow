"use client";

import * as React from "react";
import { useMemo, useRef, useState } from "react";
import { Braces, Code2, ListFilter, Play, Plus, Save, Search, X } from "lucide-react";
import { Badge, Button, Card, Input } from "@/components/ui";
import type { DbKind } from "@/lib/api";

export type BrowserKind = DbKind | "browser_sqlite";

/** One REAL reply, already adapted to a table. Every field is copied or measured; none is invented. */
export interface BrowserTable {
  ok: boolean;
  columns: string[];
  rows: unknown[][];
  truncated?: boolean;
  durationMs?: number;
  engine?: string;
  tier?: string;
  op?: string;
  message?: string | null;
}

export interface DbQueryBuilderProps {
  kind: BrowserKind;
  hint: string;
  capabilities: { write: boolean; destructive: boolean; note?: string };
  mayRun: boolean;
  mayDestructive: boolean;
  runHint?: string;
  /** Real table names, discovered from the reply or the deployment schema. */
  tables?: string[];
  /** Executes ONE real command. The only write surface the browser ever calls. */
  runCommand: (input: string, write: boolean, destructive: boolean) => Promise<BrowserTable>;
  /** A real key-addressed write route. Absent = no Save control is rendered. */
  saveValue?: (key: string, text: string) => Promise<BrowserTable>;
  saveRoute?: string;
  title?: string;
}

const P = {
  indigo: "#6366f1",
  cyan: "#22d3ee",
  emerald: "#34d399",
  amber: "#fbbf24",
  rose: "#fb7185",
  violet: "#a78bfa",
};

const SQL_KINDS: BrowserKind[] = ["sqlite", "postgres", "supabase", "browser_sqlite"];
const SQL_OPS = ["=", "!=", "<", "<=", ">", ">=", "LIKE", "IS NULL", "IS NOT NULL"];
const IDENT = /^[A-Za-z_][A-Za-z0-9_]*$/;

type Condition = { field: string; op: string; value: string };
type View = {
  id: string;
  name: string;
  mode: "builder" | "code";
  code: string;
  table: string;
  limit: string;
  conditions: Condition[];
  cursor: string;
  match: string;
  count: string;
  vector: string;
  search: string;
  typeFilter: string;
  commands: string[];
  responses: BrowserTable[];
  error: string;
  busy: boolean;
  selectedKey: string | null;
  detail: BrowserTable | null;
  typeLabel: string | null;
  ttlLabel: string | null;
  draft: string;
  saving: boolean;
  note: string;
};

type Entry = {
  key: string;
  value: unknown;
  type: string;
  bytes: number;
  score: number | null;
  raw: unknown[];
  table: BrowserTable;
};

function isSql(kind: BrowserKind): boolean {
  return SQL_KINDS.includes(kind);
}

function byteLen(s: string): number {
  return new TextEncoder().encode(s).length;
}

function typeOf(v: unknown): string {
  if (v === null || v === undefined) return "NULL";
  if (Array.isArray(v)) return "ARRAY";
  if (typeof v === "object") return "OBJECT";
  if (typeof v === "number") return "NUMBER";
  if (typeof v === "boolean") return "BOOLEAN";
  return "STRING";
}

/** Pretty-prints a value that is already JSON; otherwise keeps the real bytes verbatim. */
function display(v: unknown): string {
  if (typeof v === "string") {
    try {
      return JSON.stringify(JSON.parse(v), null, 2);
    } catch {
      return v;
    }
  }
  if (v === null || v === undefined) return "";
  return JSON.stringify(v, null, 2);
}

function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function clean(e: unknown): string {
  const s = e instanceof Error ? e.message : String(e);
  return s.replace(/^Error:\s*/, "");
}

function entriesFromTable(table: BrowserTable, kind: BrowserKind): Entry[] {
  const out: Entry[] = [];
  const scoreIdx = table.columns.findIndex((c) => c.toLowerCase() === "score");
  const bytesIdx = table.columns.findIndex((c) => c.toLowerCase() === "bytes");
  // A Redis SCAN page is [cursor, [keys...]]: the nested array is the REAL key list and
  // the scalar cursor is not a key. In that shape, scalar-only rows are metadata, never keys.
  const scanShape = kind === "redis" && table.rows.some((r) => r.some((c) => Array.isArray(c)));
  for (const row of table.rows) {
    const nested = kind === "redis" ? (row.filter((c) => Array.isArray(c)) as unknown[][]) : [];
    if (scanShape && nested.length === 0) continue;
    if (nested.length > 0) {
      for (const arr of nested) {
        for (const item of arr) {
          if (item === null || item === undefined) continue;
          const key = String(item);
          out.push({ key, value: item, type: typeOf(item), bytes: byteLen(key), score: null, raw: [item], table });
        }
      }
      continue;
    }
    const first = row[0];
    const key = first === null || first === undefined ? "" : String(first);
    const value = row.length === 1 ? first : Object.fromEntries(table.columns.map((c, i) => [c, row[i] ?? null]));
    const score = scoreIdx >= 0 ? num(row[scoreIdx]) : null;
    const measured = bytesIdx >= 0 ? num(row[bytesIdx]) : null;
    out.push({
      key,
      value,
      type: typeOf(value),
      bytes: measured === null ? byteLen(JSON.stringify(value)) : measured,
      score,
      raw: row,
      table,
    });
  }
  return out;
}

/** AND over several real SCAN pages: a key survives only if every real reply returned it. */
function intersect(lists: Entry[][]): Entry[] {
  if (lists.length === 0) return [];
  return lists[0].filter((e) => lists.slice(1).every((l) => l.some((x) => x.key === e.key)));
}

function sqlLiteral(v: string): string {
  const s = v.trim();
  if (/^-?\d+(?:\.\d+)?$/.test(s)) return s;
  return "'" + s.replace(/'/g, "''") + "'";
}

/** Compiles the builder's conditions into commands the console REALLY accepts. No invented grammar. */
function compose(view: View, kind: BrowserKind): { commands: string[]; error: string; note: string } {
  if (isSql(kind)) {
    const t = view.table.trim();
    if (!IDENT.test(t)) return { commands: [], error: "Enter a table name (letters, digits or underscores).", note: "" };
    const limit = Math.min(200, Math.max(1, Number.parseInt(view.limit, 10) || 50));
    const parts: string[] = [];
    for (const c of view.conditions) {
      const f = c.field.trim();
      if (!IDENT.test(f)) return { commands: [], error: "Each condition needs a column name (letters, digits or underscores).", note: "" };
      if (c.op === "IS NULL" || c.op === "IS NOT NULL") {
        parts.push(f + " " + c.op);
        continue;
      }
      if (!c.value.trim()) return { commands: [], error: "Each condition needs a value.", note: "" };
      parts.push(f + " " + c.op + " " + sqlLiteral(c.value));
    }
    const q = "SELECT * FROM " + t + (parts.length ? " WHERE " + parts.join(" AND ") : "") + " LIMIT " + String(limit);
    return { commands: [q], error: "", note: "One real SQL statement; the server decides its tier from the text." };
  }
  if (kind === "redis") {
    const cursor = /^\d+$/.test(view.cursor.trim()) ? view.cursor.trim() : "0";
    const count = Math.min(1000, Math.max(1, Number.parseInt(view.count, 10) || 100));
    const patterns = view.conditions.map((c) => c.value.trim() || view.match.trim() || "*");
    const list = patterns.length ? patterns : [view.match.trim() || "*"];
    const commands = list.map((m) => "SCAN " + cursor + " MATCH " + m + " COUNT " + String(count));
    return {
      commands,
      error: "",
      note: list.length > 1
        ? "AND = intersection of the keys returned by " + String(list.length) + " real SCAN pages."
        : "One real SCAN page; the reply carries the next cursor.",
    };
  }
  if (kind === "queue") {
    const n = Math.min(200, Math.max(1, Number.parseInt(view.count, 10) || 10));
    return { commands: ["PEEK " + String(n)], error: "", note: "PEEK reads messages without consuming them." };
  }
  if (kind === "vector") {
    let parsed: unknown;
    try {
      parsed = JSON.parse(view.vector.trim());
    } catch {
      return { commands: [], error: "Enter a JSON vector, e.g. [0.1,0.2].", note: "" };
    }
    if (!Array.isArray(parsed) || parsed.length === 0 || parsed.some((x) => typeof x !== "number" || !Number.isFinite(x))) {
      return { commands: [], error: "The vector must be a non-empty JSON array of finite numbers.", note: "" };
    }
    return { commands: ["QUERY " + JSON.stringify(parsed)], error: "", note: "QUERY returns real ids, scores and metadata." };
  }
  if (kind === "blob") return { commands: ["LIST"], error: "", note: "LIST returns the real keys in this bucket." };
  return { commands: ["INFO"], error: "", note: "INFO is the only command this console offers for this kind." };
}

/** The console's tokenizer groups with quotes and has no escapes: a key holding BOTH
 *  quote characters cannot be addressed by a command at all, so Get/Save refuse it. */
function quoteArg(s: string): string | null {
  if (!s.includes("'")) return "'" + s + "'";
  if (!s.includes('"')) return '"' + s + '"';
  return null;
}

function valueCommand(kind: BrowserKind, key: string): string | null {
  const q = quoteArg(key);
  if (!q) return null;
  if (kind === "redis" || kind === "blob") return "GET " + q;
  return null;
}

function valueText(kind: BrowserKind, table: BrowserTable): string {
  const row = table.rows[0];
  if (!row) return "";
  let cell: unknown = row[0];
  if (kind === "blob") {
    const i = table.columns.findIndex((c) => c.toLowerCase() === "value");
    if (i >= 0) cell = row[i];
  }
  return display(cell);
}

function scalarText(table: BrowserTable): string {
  const cell = table.rows[0]?.[0];
  if (cell === null || cell === undefined) return "—";
  if (typeof cell === "string" || typeof cell === "number") return String(cell);
  return JSON.stringify(cell);
}

function newView(id: string, name: string, kind: BrowserKind, tables: string[]): View {
  return {
    id,
    name,
    mode: "builder",
    code: "",
    table: tables[0] ?? "",
    limit: "50",
    conditions: [],
    cursor: "0",
    match: "*",
    count: kind === "queue" ? "10" : "100",
    vector: "",
    search: "",
    typeFilter: "all",
    commands: [],
    responses: [],
    error: "",
    busy: false,
    selectedKey: null,
    detail: null,
    typeLabel: null,
    ttlLabel: null,
    draft: "",
    saving: false,
    note: "",
  };
}

/** Syntax tokens for the value pane. Colours are flat strokes over a dark editor surface,
 *  never a blur: keys violet, strings emerald, numbers amber, booleans rose, null cyan. */
const JSON_TOKEN = /("(?:\\.|[^"\\])*")(\s*:)?|\b(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)\b|\b(true|false)\b|\b(null)\b|([{}[\],:])|(\s+)|([^\s]+)/g;

function colorize(text: string): React.ReactNode[] {
  const out: React.ReactNode[] = [];
  let match: RegExpExecArray | null;
  let n = 0;
  JSON_TOKEN.lastIndex = 0;
  while ((match = JSON_TOKEN.exec(text)) !== null) {
    n += 1;
    const [full, str, colon, number, bool, nul, punct, space] = match;
    if (str) {
      out.push(<span key={"s" + n} style={{ color: colon ? P.violet : P.emerald }}>{str}</span>);
      if (colon) out.push(<span key={"c" + n} style={{ color: "#94a3b8" }}>{colon}</span>);
    } else if (number) {
      out.push(<span key={"n" + n} style={{ color: P.amber }}>{number}</span>);
    } else if (bool) {
      out.push(<span key={"b" + n} style={{ color: P.rose }}>{bool}</span>);
    } else if (nul) {
      out.push(<span key={"z" + n} style={{ color: P.cyan }}>{nul}</span>);
    } else if (punct) {
      out.push(<span key={"p" + n} style={{ color: "#94a3b8" }}>{punct}</span>);
    } else if (space) {
      out.push(<span key={"w" + n}>{space}</span>);
    } else {
      out.push(<span key={"o" + n} style={{ color: "#cbd5e1" }}>{full}</span>);
    }
  }
  return out;
}

function tint(hex: string, alpha: number): string {
  return "color-mix(in srgb, " + hex + " " + String(Math.round(alpha * 100)) + "%, transparent)";
}

export function DbQueryBuilder(props: DbQueryBuilderProps) {
  const { kind, hint, capabilities, mayRun, mayDestructive, runHint, tables, runCommand, saveValue, saveRoute, title } = props;
  const seq = useRef(1);
  const [views, setViews] = useState<View[]>(() => [newView("index-0", "Index", kind, tables ?? [])]);
  const [activeId, setActiveId] = useState("index-0");
  const [write, setWrite] = useState(false);
  const [destructive, setDestructive] = useState(false);
  const preRef = useRef<HTMLPreElement | null>(null);
  const view = views.find((v) => v.id === activeId) ?? views[0];

  function patch(id: string, p: Partial<View>) {
    setViews((all) => all.map((v) => (v.id === id ? { ...v, ...p } : v)));
  }

  const perResponse = useMemo(() => view.responses.map((t) => entriesFromTable(t, kind)), [view.responses, kind]);
  const entries = useMemo(
    () => (perResponse.length <= 1 ? (perResponse[0] ?? []) : intersect(perResponse)),
    [perResponse],
  );
  const types = useMemo(() => {
    const counts = new Map<string, number>();
    for (const e of entries) counts.set(e.type, (counts.get(e.type) ?? 0) + 1);
    return Array.from(counts.entries()).sort((a, b) => a[0].localeCompare(b[0]));
  }, [entries]);
  const shown = useMemo(() => {
    const q = view.search.trim().toLowerCase();
    return entries.filter(
      (e) => (view.typeFilter === "all" || e.type === view.typeFilter) && (!q || e.key.toLowerCase().includes(q)),
    );
  }, [entries, view.search, view.typeFilter]);
  const selected = entries.find((e) => e.key === view.selectedKey) ?? null;
  const compiled = useMemo(() => compose(view, kind), [view, kind]);
  const raw = view.responses[0] ?? null;

  async function execute(commands: string[], w: boolean, d: boolean) {
    if (!mayRun || commands.length === 0 || view.busy) return;
    const id = view.id;
    patch(id, { busy: true, error: "", note: "", commands });
    const results: BrowserTable[] = [];
    try {
      for (const cmd of commands) results.push(await runCommand(cmd, w, d));
      const failed = results.find((r) => !r.ok);
      patch(id, {
        busy: false,
        responses: results,
        selectedKey: null,
        detail: null,
        draft: "",
        typeLabel: null,
        ttlLabel: null,
        error: failed?.message ? String(failed.message) : "",
        note: String(results.length) + (results.length === 1 ? " real command executed." : " real commands executed; keys shown are their intersection."),
      });
    } catch (e) {
      patch(id, { busy: false, error: clean(e), responses: [], note: "" });
    }
  }

  function runBuilder() {
    if (compiled.error) {
      patch(view.id, { error: compiled.error });
      return;
    }
    void execute(compiled.commands, false, false);
  }

  function runCode() {
    const q = view.code.trim();
    if (!q) {
      patch(view.id, { error: "Enter a command first." });
      return;
    }
    void execute([q], write, destructive);
  }

  function addTab() {
    const id = "index-" + String(seq.current++);
    const name = "Index " + String(seq.current);
    setViews((all) => [...all, newView(id, name, kind, tables ?? [])]);
    setActiveId(id);
  }

  function closeTab(id: string) {
    if (views.length === 1) return;
    const rest = views.filter((v) => v.id !== id);
    setViews(rest);
    if (activeId === id) setActiveId(rest[0].id);
  }

  function addCondition() {
    const c: Condition = { field: isSql(kind) ? "" : "MATCH", op: isSql(kind) ? "=" : "matches", value: "" };
    patch(view.id, { conditions: [...view.conditions, c] });
  }

  function patchCondition(index: number, p: Partial<Condition>) {
    patch(view.id, {
      conditions: view.conditions.map((c, i) => (i === index ? { ...c, ...p } : c)),
    });
  }

  async function selectEntry(entry: Entry) {
    const id = view.id;
    patch(id, { selectedKey: entry.key, detail: null, typeLabel: null, ttlLabel: null, draft: "", error: "", note: "" });
    const cmd = valueCommand(kind, entry.key);
    if (!cmd) {
      patch(id, {
        draft: display(entry.value),
        note: kind === "redis" || kind === "blob"
          ? "This key holds both quote characters; the console's command grammar cannot address it."
          : "Value comes from the real result row.",
      });
      return;
    }
    patch(id, { busy: true });
    try {
      const detail = await runCommand(cmd, false, false);
      let typeLabel: string | null = null;
      let ttlLabel: string | null = null;
      if (kind === "redis") {
        const q = quoteArg(entry.key);
        if (q) {
          const settled = await Promise.allSettled([
            runCommand("TYPE " + q, false, false),
            runCommand("TTL " + q, false, false),
          ]);
          const typeTable = settled[0].status === "fulfilled" ? settled[0].value : null;
          const ttlTable = settled[1].status === "fulfilled" ? settled[1].value : null;
          if (typeTable && typeTable.rows[0]) typeLabel = scalarText(typeTable).toUpperCase();
          if (ttlTable && ttlTable.rows[0]) {
            const t = num(ttlTable.rows[0][0]);
            ttlLabel = t === null ? "—" : t === -2 ? "gone" : t < 0 ? "No" : String(t) + " s";
          }
        }
      }
      patch(id, { busy: false, detail, typeLabel, ttlLabel, draft: valueText(kind, detail) });
    } catch (e) {
      patch(id, { busy: false, error: clean(e) });
    }
  }

  async function save() {
    if (!saveValue || !selected || view.saving) return;
    const id = view.id;
    const text = view.draft;
    patch(id, { saving: true, error: "", note: "" });
    try {
      const result = await saveValue(selected.key, text);
      const cmd = valueCommand(kind, selected.key);
      let readBack: BrowserTable | null = null;
      if (cmd) {
        try {
          readBack = await runCommand(cmd, false, false);
        } catch {
          readBack = null;
        }
      }
      const after = readBack ? valueText(kind, readBack) : null;
      patch(id, {
        saving: false,
        detail: readBack ?? view.detail,
        draft: after ?? text,
        note: after === null
          ? "Write returned " + String(result.rows.length) + " real row(s)."
          : after === text
            ? "Saved and read back unchanged."
            : "Saved; the read-back differs from the submitted text.",
      });
    } catch (e) {
      patch(id, { saving: false, error: clean(e) });
    }
  }

  return (
    <Card className="min-w-0 p-0 shadow-none">
      <div className="min-w-0 border-b border-border px-3 pt-3 sm:px-4">
        <div className="mb-2 flex min-w-0 flex-wrap items-center justify-between gap-2">
          <div className="flex min-w-0 items-center gap-2">
            <Braces className="h-4 w-4 shrink-0" style={{ color: P.indigo }} aria-hidden />
            <h3 className="min-w-0 break-words text-sm font-semibold">{title ?? "Data browser"}</h3>
            <Badge tone={capabilities.write ? "green" : "blue"}>{capabilities.write ? "read + write" : "read-only"}</Badge>
          </div>
          <span className="min-w-0 break-words text-[11px] text-muted">{kind.toUpperCase()} · one real command per run</span>
        </div>

        <div className="flex min-w-0 items-end gap-2">
          <div className="no-scrollbar flex min-w-0 flex-1 items-center gap-1 overflow-x-auto" role="tablist" aria-label="Query views">
            {views.map((v) => (
              <div key={v.id} className="flex shrink-0 items-center gap-1">
                {v.id === activeId ? (
                  <>
                  <Search className="h-3.5 w-3.5 shrink-0" style={{ color: P.emerald }} aria-hidden />
                  <Input
                    aria-label="Query view name"
                    value={v.name}
                    onChange={(e) => patch(v.id, { name: e.target.value })}
                    className="min-h-[44px] w-24 min-w-0 border-transparent bg-transparent px-2 text-xs shadow-none sm:w-32"
                    style={{ boxShadow: "inset 0 -2px 0 " + P.violet }}
                  />
                  </>
                ) : (
                  <button
                    type="button"
                    role="tab"
                    aria-selected={false}
                    onClick={() => setActiveId(v.id)}
                    className="inline-flex min-h-[44px] items-center gap-1.5 rounded-t-md border-b-2 border-transparent px-2 text-xs text-secondary hover:text-fg"
                  >
                    <Search className="h-3.5 w-3.5" style={{ color: P.emerald }} aria-hidden />
                    <span className="max-w-24 truncate">{v.name}</span>
                  </button>
                )}
                {views.length > 1 && v.id === activeId && (
                  <button
                    type="button"
                    aria-label={"Close " + v.name}
                    onClick={() => closeTab(v.id)}
                    className="inline-flex min-h-[44px] min-w-[44px] items-center justify-center text-muted hover:text-fg"
                  >
                    <X className="h-3.5 w-3.5" aria-hidden />
                  </button>
                )}
              </div>
            ))}
          </div>
          <button
            type="button"
            aria-label="New query tab"
            title="New query tab"
            onClick={addTab}
            className="inline-flex min-h-[44px] min-w-[44px] shrink-0 items-center justify-center rounded-md border border-border hover:bg-subtle"
          >
            <Plus className="h-4 w-4" style={{ color: P.cyan }} aria-hidden />
          </button>
        </div>
        <div
          aria-hidden
          className="h-[2px] w-full min-w-0"
          style={{
            background: "linear-gradient(90deg, " + P.indigo + ", " + P.cyan + ", " + P.emerald + ", " + P.amber + ", " + P.rose + ", " + P.violet + ")",
            maskImage: "linear-gradient(90deg, #000 0%, #000 72%, transparent 100%)",
            WebkitMaskImage: "linear-gradient(90deg, #000 0%, #000 72%, transparent 100%)",
          }}
        />
      </div>

      <div className="flex min-w-0 flex-col gap-3 p-3 sm:p-4">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <div className="inline-flex min-h-[44px] items-center gap-1 rounded-md border border-border p-1">
            <button
              type="button"
              aria-pressed={view.mode === "builder"}
              onClick={() => patch(view.id, { mode: "builder" })}
              className="inline-flex min-h-[44px] items-center gap-1.5 rounded px-2 text-xs font-medium"
              style={view.mode === "builder" ? { color: "#fff", background: P.indigo } : { color: "hsl(var(--secondary))" }}
            >
              <ListFilter className="h-3.5 w-3.5" aria-hidden /> Query Builder
            </button>
            <button
              type="button"
              aria-pressed={view.mode === "code"}
              onClick={() => patch(view.id, { mode: "code" })}
              className="inline-flex min-h-[44px] items-center gap-1.5 rounded px-2 text-xs font-medium"
              style={view.mode === "code" ? { color: "#07131a", background: P.cyan } : { color: "hsl(var(--secondary))" }}
            >
              <Code2 className="h-3.5 w-3.5" aria-hidden /> Code Editor
            </button>
          </div>

          <label className="inline-flex min-h-[44px] min-w-0 items-center gap-1.5 text-xs text-secondary">
            <Search className="h-3.5 w-3.5 shrink-0" style={{ color: P.emerald }} aria-hidden />
            <select
              aria-label="Filter by value type"
              value={view.typeFilter}
              onChange={(e) => patch(view.id, { typeFilter: e.target.value })}
              className="min-h-[44px] max-w-[9rem] min-w-0 rounded-md border border-border bg-card px-2 text-xs text-fg"
            >
              <option value="all">All Types ({entries.length})</option>
              {types.map(([t, count]) => (
                <option key={t} value={t}>{t} ({count})</option>
              ))}
            </select>
          </label>

          <Input
            aria-label="Search returned keys"
            placeholder="Search returned keys"
            value={view.search}
            onChange={(e) => patch(view.id, { search: e.target.value })}
            className="min-h-[44px] min-w-0 flex-1 text-xs"
          />
        </div>

        {view.mode === "builder" ? (
          <div className="min-w-0 rounded-lg border border-border p-3" style={{ background: tint(P.indigo, 0.04) }}>
            <div className="mb-2 flex min-w-0 flex-wrap items-center gap-2 text-xs text-secondary">
              <span className="font-medium uppercase tracking-wide" style={{ color: P.indigo }}>Conditions</span>
              <span className="min-w-0 break-words">{compiled.note}</span>
            </div>

            {isSql(kind) ? (
              <div className="grid min-w-0 gap-2 sm:grid-cols-2">
                <label className="flex min-w-0 flex-col gap-1 text-[11px] text-secondary">
                  Table
                  {tables && tables.length > 0 ? (
                    <select
                      aria-label="Table"
                      value={view.table}
                      onChange={(e) => patch(view.id, { table: e.target.value })}
                      className="min-h-[44px] w-full min-w-0 rounded-md border border-border bg-card px-2 text-xs text-fg"
                    >
                      {tables.map((t) => <option key={t} value={t}>{t}</option>)}
                    </select>
                  ) : (
                    <Input
                      aria-label="Table name"
                      value={view.table}
                      onChange={(e) => patch(view.id, { table: e.target.value })}
                      placeholder="table_name"
                      className="min-h-[44px] min-w-0 font-mono text-xs"
                    />
                  )}
                </label>
                <label className="flex min-w-0 flex-col gap-1 text-[11px] text-secondary">
                  Row limit
                  <Input
                    aria-label="Row limit"
                    type="number"
                    min={1}
                    max={200}
                    value={view.limit}
                    onChange={(e) => patch(view.id, { limit: e.target.value })}
                    className="min-h-[44px] min-w-0 font-mono text-xs"
                  />
                </label>
              </div>
            ) : null}

            {kind === "redis" ? (
              <div className="grid min-w-0 gap-2 sm:grid-cols-3">
                <label className="flex min-w-0 flex-col gap-1 text-[11px] text-secondary">
                  Cursor
                  <Input aria-label="Cursor" value={view.cursor} onChange={(e) => patch(view.id, { cursor: e.target.value })} className="min-h-[44px] min-w-0 font-mono text-xs" />
                </label>
                <label className="flex min-w-0 flex-col gap-1 text-[11px] text-secondary">
                  Default MATCH
                  <Input aria-label="Default match pattern" value={view.match} onChange={(e) => patch(view.id, { match: e.target.value })} className="min-h-[44px] min-w-0 font-mono text-xs" />
                </label>
                <label className="flex min-w-0 flex-col gap-1 text-[11px] text-secondary">
                  COUNT
                  <Input aria-label="Scan count" type="number" min={1} max={1000} value={view.count} onChange={(e) => patch(view.id, { count: e.target.value })} className="min-h-[44px] min-w-0 font-mono text-xs" />
                </label>
              </div>
            ) : null}

            {kind === "queue" ? (
              <label className="flex min-w-0 flex-col gap-1 text-[11px] text-secondary">
                PEEK count
                <Input aria-label="Peek count" type="number" min={1} max={200} value={view.count} onChange={(e) => patch(view.id, { count: e.target.value })} className="min-h-[44px] min-w-0 font-mono text-xs" />
              </label>
            ) : null}

            {kind === "vector" ? (
              <label className="flex min-w-0 flex-col gap-1 text-[11px] text-secondary">
                QUERY vector
                <Input aria-label="Query vector" value={view.vector} onChange={(e) => patch(view.id, { vector: e.target.value })} placeholder="[0.1,0.2]" className="min-h-[44px] min-w-0 font-mono text-xs" />
              </label>
            ) : null}

            {(isSql(kind) || kind === "redis") ? (
              <div className="mt-3 flex min-w-0 flex-col gap-2">
                {view.conditions.map((c, i) => (
                  <div key={i} className="flex min-w-0 flex-col gap-2">
                    {i > 0 && <span className="w-fit rounded-full px-2 py-0.5 text-[10px] font-semibold" style={{ color: P.violet, background: tint(P.violet, 0.14) }}>And</span>}
                    <div className="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-center">
                      {isSql(kind) ? (
                        <Input aria-label={"Column " + String(i + 1)} value={c.field} onChange={(e) => patchCondition(i, { field: e.target.value })} placeholder="column" className="min-h-[44px] min-w-0 flex-1 font-mono text-xs" />
                      ) : (
                        <span className="inline-flex min-h-[44px] shrink-0 items-center rounded-md border border-border bg-card px-3 font-mono text-xs text-secondary">MATCH</span>
                      )}
                      {isSql(kind) ? (
                        <select
                          aria-label={"Operator " + String(i + 1)}
                          value={c.op}
                          onChange={(e) => patchCondition(i, { op: e.target.value })}
                          className="min-h-[44px] min-w-0 shrink-0 rounded-md border border-border bg-card px-2 font-mono text-xs text-fg"
                        >
                          {SQL_OPS.map((op) => <option key={op} value={op}>{op}</option>)}
                        </select>
                      ) : (
                        <span className="inline-flex min-h-[44px] shrink-0 items-center px-2 text-xs text-secondary">matches</span>
                      )}
                      {(!isSql(kind) || (c.op !== "IS NULL" && c.op !== "IS NOT NULL")) && (
                        <Input
                          aria-label={"Value " + String(i + 1)}
                          value={c.value}
                          onChange={(e) => patchCondition(i, { value: e.target.value })}
                          placeholder={isSql(kind) ? "value" : "key:*"}
                          className="min-h-[44px] min-w-0 flex-1 font-mono text-xs"
                        />
                      )}
                      <button
                        type="button"
                        aria-label={"Remove condition " + String(i + 1)}
                        onClick={() => patch(view.id, { conditions: view.conditions.filter((_, j) => j !== i) })}
                        className="inline-flex min-h-[44px] min-w-[44px] shrink-0 items-center justify-center rounded-md border border-border text-muted hover:text-fg"
                      >
                        <X className="h-3.5 w-3.5" aria-hidden />
                      </button>
                    </div>
                  </div>
                ))}
                <div className="flex min-w-0 flex-wrap items-center gap-2">
                  <button
                    type="button"
                    aria-label="And, add a condition"
                    onClick={addCondition}
                    className="inline-flex min-h-[44px] items-center gap-1.5 rounded-md px-2 text-xs font-semibold"
                    style={{ color: P.violet, background: tint(P.violet, 0.12), border: "1px solid " + tint(P.violet, 0.35) }}
                  >
                    And <Plus className="h-3.5 w-3.5" aria-hidden />
                  </button>
                  <button
                    type="button"
                    onClick={addCondition}
                    className="inline-flex min-h-[44px] items-center gap-1.5 rounded-md border border-dashed border-border px-3 text-xs text-secondary hover:text-fg"
                  >
                    <Plus className="h-3.5 w-3.5" aria-hidden /> Add a condition
                  </button>
                </div>
              </div>
            ) : (
              <p className="mt-2 min-w-0 break-words text-[11px] text-muted">
                This console offers one command shape for {kind.toUpperCase()}; there is no condition grammar behind it, so no condition rows are shown.
              </p>
            )}

            <pre className="mt-3 max-w-full min-w-0 overflow-x-auto whitespace-pre-wrap break-words rounded-md border border-border bg-card p-2 font-mono text-[11px] text-fg">
              {compiled.commands.join("\n")}
            </pre>
            <div className="mt-3 flex min-w-0 flex-wrap items-center gap-2">
              <span title={mayRun ? undefined : runHint}>
                <Button type="button" onClick={runBuilder} disabled={!mayRun || view.busy || compiled.commands.length === 0} className="min-h-[44px]">
                  <Play className="h-4 w-4" aria-hidden /> {view.busy ? "Running…" : "Run query"}
                </Button>
              </span>
              {view.commands.length > 0 && (
                <Button type="button" variant="outline" onClick={() => void execute(view.commands, false, false)} disabled={!mayRun || view.busy} className="min-h-[44px]">Refresh</Button>
              )}
            </div>
          </div>
        ) : (
          <div className="min-w-0 rounded-lg border border-border p-3" style={{ background: tint(P.cyan, 0.04) }}>
            <label className="mb-1 block text-[11px] font-medium uppercase tracking-wide text-secondary" htmlFor={"code-" + view.id}>Command</label>
            <textarea
              id={"code-" + view.id}
              value={view.code}
              onChange={(e) => patch(view.id, { code: e.target.value })}
              placeholder={hint}
              spellCheck={false}
              rows={3}
              className="block w-full min-w-0 resize-y rounded-md border border-border bg-card p-3 font-mono text-xs leading-relaxed text-fg outline-none focus:border-border-strong"
            />
            <div className="mt-2 flex min-w-0 flex-wrap items-center gap-4 text-xs text-secondary">
              <label className="inline-flex min-h-[44px] items-center gap-2">
                <input type="checkbox" checked={write} disabled={!capabilities.write} onChange={(e) => { setWrite(e.target.checked); if (!e.target.checked) setDestructive(false); }} />
                Allow writes
              </label>
              <label className="inline-flex min-h-[44px] items-center gap-2">
                <input type="checkbox" checked={destructive} disabled={!capabilities.destructive || !write || !mayDestructive} onChange={(e) => setDestructive(e.target.checked)} />
                Allow destructive
              </label>
              <span className="min-w-0 break-words text-muted">{capabilities.note ?? ""}</span>
            </div>
            <div className="mt-2 flex min-w-0 flex-wrap items-center gap-2">
              <span title={mayRun ? undefined : runHint}>
                <Button type="button" onClick={runCode} disabled={!mayRun || view.busy || !view.code.trim()} className="min-h-[44px]">
                  <Play className="h-4 w-4" aria-hidden /> {view.busy ? "Running…" : "Run command"}
                </Button>
              </span>
              {view.commands.length > 0 && (
                <Button type="button" variant="outline" onClick={() => void execute(view.commands, write, destructive)} disabled={!mayRun || view.busy} className="min-h-[44px]">Refresh</Button>
              )}
            </div>
          </div>
        )}

        {view.error && (
          <p role="alert" className="min-w-0 break-words rounded-md border px-3 py-2 text-xs" style={{ color: P.rose, borderColor: tint(P.rose, 0.4), background: tint(P.rose, 0.08) }}>{view.error}</p>
        )}
        {view.note && <p className="min-w-0 break-words text-[11px] text-muted">{view.note}</p>}

        <div className="grid min-w-0 gap-3 lg:grid-cols-2">
          <div className="flex min-w-0 flex-col gap-2">
            <div className="flex min-w-0 items-center justify-between gap-2">
              <span className="text-xs font-medium uppercase tracking-wide text-muted">Keys / rows ({shown.length})</span>
              {raw?.truncated && <Badge tone="amber">truncated</Badge>}
            </div>
            <div className="max-h-72 min-w-0 overflow-y-auto rounded-lg border border-border">
              {view.responses.length === 0 ? (
                <p className="min-w-0 break-words p-3 text-xs text-muted">No rows yet. Run a query to see real data.</p>
              ) : shown.length === 0 ? (
                <p className="min-w-0 break-words p-3 text-xs text-muted">No keys match this filter.</p>
              ) : (
                <ul className="divide-y divide-border">
                  {shown.map((e, i) => (
                    <li key={e.key + "-" + String(i)} className="min-w-0">
                      <button
                        type="button"
                        onClick={() => void selectEntry(e)}
                        aria-pressed={view.selectedKey === e.key}
                        className="flex min-h-[44px] w-full min-w-0 items-center gap-2 px-2 py-1.5 text-left hover:bg-subtle"
                        style={view.selectedKey === e.key ? { background: tint(P.indigo, 0.1), boxShadow: "inset 3px 0 0 " + P.indigo } : undefined}
                      >
                        <span className="min-w-0 flex-1 break-words font-mono text-[11px] text-fg">{e.key || "(empty)"}</span>
                        <span title="Bytes of the displayed key or row, measured from the reply" className="shrink-0 rounded px-1.5 py-0.5 font-mono text-[10px] font-semibold" style={{ color: P.indigo, background: tint(P.indigo, 0.14), border: "1px solid " + tint(P.indigo, 0.3) }}>{e.bytes}</span>
                        <span title="JSON type of the displayed value" className="shrink-0 rounded px-1.5 py-0.5 text-[10px] font-semibold" style={{ color: P.cyan, background: tint(P.cyan, 0.12), border: "1px solid " + tint(P.cyan, 0.3) }}>{e.type}</span>
                        {e.score !== null && (
                          <span title="Score returned by the vector QUERY reply" className="shrink-0 rounded px-1.5 py-0.5 font-mono text-[10px] font-semibold" style={{ color: P.violet, background: tint(P.violet, 0.14) }}>{e.score}</span>
                        )}
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </div>

          <div className="flex min-w-0 flex-col gap-2">
            <div className="flex min-w-0 items-center justify-between gap-2">
              <span className="min-w-0 break-words text-xs font-medium uppercase tracking-wide text-muted">Value</span>
              {selected && <code className="min-w-0 max-w-[10rem] truncate font-mono text-[11px] text-secondary">{selected.key}</code>}
            </div>
            {!selected ? (
              <p className="min-w-0 break-words rounded-lg border border-border p-3 text-xs text-muted">Select a key or row to inspect its real value.</p>
            ) : (
              <>
                <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                  <span className="rounded px-2 py-0.5 text-[10px] font-semibold" style={{ color: P.emerald, background: tint(P.emerald, 0.14), border: "1px solid " + tint(P.emerald, 0.3) }}>{view.typeLabel ?? selected.type}</span>
                  <span className="rounded px-2 py-0.5 text-[10px] font-semibold" style={{ color: P.cyan, background: tint(P.cyan, 0.12) }}>Size: {byteLen(view.draft)} B</span>
                  <span className="rounded px-2 py-0.5 text-[10px] font-semibold" style={{ color: P.amber, background: tint(P.amber, 0.12) }}>Length: {view.draft.length}</span>
                  <span className="rounded px-2 py-0.5 text-[10px] font-semibold" style={{ color: P.rose, background: tint(P.rose, 0.12) }} title={view.ttlLabel === null ? "This console exposes no TTL command for this kind" : undefined}>
                    TTL: {view.ttlLabel ?? "not exposed"}
                  </span>
                </div>
                <div
                  className="relative min-w-0 rounded-lg border border-border"
                  style={{ background: "linear-gradient(135deg, rgba(99,102,241,0.12), rgba(34,211,238,0.05) 45%, #0b1020 100%)" }}
                >
                  <pre
                    ref={preRef}
                    aria-hidden
                    className="pointer-events-none absolute inset-0 m-0 overflow-hidden whitespace-pre-wrap break-words p-3 font-mono text-xs leading-relaxed"
                  >{colorize(view.draft)}{"\n"}</pre>
                  <textarea
                    aria-label="JSON value editor"
                    value={view.draft}
                    onChange={(e) => patch(view.id, { draft: e.target.value })}
                    onScroll={(e) => {
                      if (preRef.current) {
                        preRef.current.scrollTop = e.currentTarget.scrollTop;
                        preRef.current.scrollLeft = e.currentTarget.scrollLeft;
                      }
                    }}
                    spellCheck={false}
                    rows={10}
                    className="relative block max-h-80 w-full min-w-0 resize-y overflow-auto whitespace-pre-wrap break-words bg-transparent p-3 font-mono text-xs leading-relaxed text-transparent outline-none selection:bg-indigo-500/30"
                    style={{ caretColor: P.indigo }}
                  />
                </div>
                <div className="flex min-w-0 flex-wrap items-center gap-2">
                  {saveValue ? (
                    <Button
                      type="button"
                      onClick={() => void save()}
                      disabled={!mayRun || !capabilities.write || view.saving || view.busy}
                      className="min-h-[44px]"
                    >
                      <Save className="h-4 w-4" aria-hidden /> {view.saving ? "Saving…" : "Save"}
                    </Button>
                  ) : null}
                  <span className="min-w-0 break-words text-[11px] text-muted">
                    {saveValue && saveRoute ? "Save writes through " + saveRoute : "This view is read-only: the console has no key-addressed write command for this kind."}
                  </span>
                </div>
              </>
            )}
          </div>
        </div>

        {raw && (
          <div className="min-w-0">
            <div className="mb-1.5 flex min-w-0 flex-wrap items-center gap-2 text-[11px] text-muted">
              {raw.tier && <span>tier <code className="font-mono text-fg">{raw.tier}</code></span>}
              {raw.engine && <span>engine <code className="font-mono text-fg">{raw.engine}</code></span>}
              <span>{raw.rows.length} rows</span>
              {typeof raw.durationMs === "number" && <span>{raw.durationMs} ms</span>}
              {raw.op && <code className="max-w-full min-w-0 break-words font-mono">{raw.op}</code>}
            </div>
            <div className="min-w-0 max-w-full overflow-x-auto rounded-lg border border-border">
              <table className="w-full min-w-[28rem] border-collapse text-left font-mono text-[11px]">
                <thead className="bg-subtle">
                  <tr>{raw.columns.map((c, i) => <th key={c + "-" + String(i)} className="border-b border-border px-2 py-1.5 font-medium text-secondary">{c}</th>)}</tr>
                </thead>
                <tbody>
                  {raw.rows.map((row, i) => (
                    <tr key={i}>
                      {raw.columns.map((c, j) => (
                        <td key={c + "-" + String(j)} className="border-b border-border/50 px-2 py-1 align-top text-fg">
                          <span className="block min-w-0 break-words">{row[j] === null || row[j] === undefined ? "—" : typeof row[j] === "string" ? row[j] : JSON.stringify(row[j])}</span>
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        )}
      </div>
    </Card>
  );
}
