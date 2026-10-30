"use client";

import { useState } from "react";
import { Play, Terminal, TriangleAlert } from "lucide-react";
import { Badge, Button, Card } from "@/components/ui";
import {
  apiSend,
  useCan,
  type Database,
  type DbConsoleResponse,
  type DbKind,
} from "@/lib/api";
import { roleHint } from "@/lib/team-role";
import { toast } from "@/components/toast";

/** Mirrors `crates/hive-cloud/src/db_console.rs` (`capabilities` + `syntax_hint`). The server's own
 *  answer overrides this: the dashboard and the backend deploy independently, so a fresh UI has to
 *  render correctly against a node that predates it — and a node that answers is authoritative. */
const DIALECT: Record<DbKind, { write: boolean; destructive: boolean; hint: string; note?: string }> = {
  sqlite: { write: true, destructive: true, hint: "SELECT * FROM … — one SQL statement, no trailing semicolon" },
  postgres: { write: true, destructive: true, hint: "SELECT * FROM … — one SQL statement, no trailing semicolon" },
  supabase: { write: true, destructive: true, hint: "SELECT * FROM … — Supabase is Postgres underneath" },
  redis: { write: true, destructive: true, hint: "GET mykey · HGETALL myhash · SCAN 0 · INFO — RESP, not SQL" },
  blob: { write: true, destructive: false, hint: "LIST · HEAD mykey · GET mykey · PUT mykey value · DELETE mykey" },
  queue: { write: true, destructive: false, hint: "DEPTH · PEEK 10 · PUSH message · POP" },
  vector: { write: false, destructive: false, hint: "COUNT · QUERY [0.1,0.2,0.3]" },
  pubsub: { write: true, destructive: false, hint: "INFO · PUBLISH message" },
  realtime: { write: false, destructive: false, hint: "INFO" },
};

/** The command console for ONE database instance. Read-only by default: a write has to be switched
 *  on per request, and a destructive command needs a second switch AND a team admin — the server
 *  re-decides the tier from the command text, so these switches only ever narrow, never widen. */
export function DbConsole({ db }: { db: Database }) {
  const [input, setInput] = useState("");
  const [write, setWrite] = useState(false);
  const [destructive, setDestructive] = useState(false);
  const [running, setRunning] = useState(false);
  const [res, setRes] = useState<DbConsoleResponse | null>(null);
  const mayConsole = useCan()("database.console");
  const mayDestructive = useCan()("database.console.destructive");

  const fallback = DIALECT[db.kind] ?? { write: false, destructive: false, hint: "" };
  const caps = res?.capabilities ?? fallback;
  const hint = res?.hint ?? fallback.hint;

  async function run() {
    if (!input.trim()) return;
    setRunning(true);
    try {
      setRes(
        await apiSend<DbConsoleResponse>("POST", `/v1/databases/${db.id}/console`, {
          input,
          write,
          destructive,
        }),
      );
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setRes(null);
      toast(`Command refused: ${message.replace(/^Error:\s*/, "")}`, {});
    } finally {
      setRunning(false);
    }
  }

  return (
    <Card>
      <div className="mb-3 flex items-center gap-2">
        <Terminal className="h-4 w-4 text-secondary" />
        <h3 className="text-sm font-semibold">Console</h3>
        <Badge tone={caps.write ? "default" : "blue"}>{caps.write ? "read + write" : "read-only"}</Badge>
        {res?.scope === "node-local" && <Badge tone="amber">node-local</Badge>}
        {res?.proxied && <Badge tone="green">ran on {res.node}</Badge>}
      </div>

      <form
        className="flex flex-col gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          void run();
        }}
      >
        <div className="flex items-center gap-2">
          <input
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder={hint || "command"}
            spellCheck={false}
            autoComplete="off"
            aria-label={`Command for ${db.name}`}
            className="min-w-0 flex-1 rounded-md border border-border bg-subtle/50 px-3 py-2 font-mono text-xs text-fg placeholder:text-muted focus:border-blue focus:outline-none"
          />
          <span title={mayConsole ? undefined : roleHint("database.console")}>
            <Button type="submit" disabled={running || !mayConsole || !input.trim()}>
              <Play className="h-4 w-4" />
              {running ? "Running…" : "Run"}
            </Button>
          </span>
        </div>

        <div className="flex flex-wrap items-center gap-4 text-xs text-secondary">
          <label className="flex items-center gap-1.5">
            <input
              type="checkbox"
              checked={write}
              disabled={!caps.write}
              onChange={(e) => {
                setWrite(e.target.checked);
                if (!e.target.checked) setDestructive(false);
              }}
            />
            Allow writes
          </label>
          <label className="flex items-center gap-1.5">
            <input
              type="checkbox"
              checked={destructive}
              disabled={!caps.destructive || !write || !mayDestructive}
              onChange={(e) => setDestructive(e.target.checked)}
            />
            Allow destructive
          </label>
          <span className="text-muted">{caps.note ?? ""}</span>
        </div>
        {destructive && (
          <p className="flex items-start gap-1.5 text-xs text-red-500">
            <TriangleAlert className="mt-0.5 h-3 w-3 shrink-0" />
            The next command may destroy data irreversibly. It also needs a team admin.
          </p>
        )}
      </form>

      {res && (
        <div className="mt-4">
          <div className="mb-2 flex flex-wrap items-center gap-2 text-xs text-muted">
            <span>
              tier <code className="font-mono text-fg">{res.tier ?? "read"}</code>
            </span>
            {res.engine && <span>engine <code className="font-mono text-fg">{res.engine}</code></span>}
            <span>{res.row_count ?? res.rows.length} rows</span>
            {typeof res.duration_ms === "number" && <span>{res.duration_ms} ms</span>}
            {res.node && <span>node {res.node}</span>}
            {res.truncated && <span>truncated</span>}
          </div>
          {!res.ok && (
            <p className="mb-2 rounded-md border border-red-500/30 bg-red-500/5 px-3 py-2 text-xs text-red-500">
              {res.message ?? res.error ?? "the engine refused this command"}
            </p>
          )}
          {res.rows.length === 0 ? (
            <p className="text-xs text-muted">
              {res.ok ? "Command ran; it returned no rows." : ""}
            </p>
          ) : (
            <div className="max-h-96 overflow-auto rounded-md border border-border">
              <table className="w-full border-collapse text-left font-mono text-xs">
                <thead className="sticky top-0 bg-subtle">
                  <tr>
                    {(res.columns ?? []).map((c) => (
                      <th key={c} className="border-b border-border px-2 py-1.5 font-medium text-secondary">
                        {c}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {res.rows.map((row, i) => (
                    <tr key={i}>
                      {(row ?? []).map((cell, j) => (
                        <td key={j} className="border-b border-border/50 px-2 py-1 align-top text-fg">
                          {cellOf(cell)}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}

      <p className="mt-3 flex items-start gap-1.5 text-xs text-muted">
        <TriangleAlert className="mt-0.5 h-3 w-3 shrink-0" />
        One command per request, no shell and no host filesystem access. Connection secrets are never
        shown here and are never echoed back by the server.
      </p>
    </Card>
  );
}

function cellOf(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return JSON.stringify(v);
}
