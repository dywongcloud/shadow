"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronDown, ChevronRight, Copy, Search, Loader2, CircleX, TriangleAlert } from "lucide-react";
import { fmtTime, HighlightLine } from "@/lib/log-format";
import type { Build } from "@/lib/api";

/**
 * Vercel-style collapsible Build Logs panel: line count + error/warning tallies,
 * find-in-logs, timestamped + syntax-highlighted lines. Auto-scrolls while the
 * build is still running. Reused by the deployment detail page.
 */
export function BuildLogs({ build, defaultOpen = true }: { build: Build | null; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  const [q, setQ] = useState("");
  const [now, setNow] = useState(Date.now());
  const logRef = useRef<HTMLDivElement>(null);

  const state = build?.state ?? "queued";
  const building = state === "building" || state === "queued";
  const errored = state === "error";

  // Tick a clock while building so the elapsed counter advances live.
  useEffect(() => {
    if (!building) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [building]);

  const elapsed = build ? Math.max(0, Math.round(((build.finished_ms ?? now) - build.started_ms) / 1000)) : 0;

  const allLines = build?.lines ?? [];
  const lines = useMemo(
    () => (q ? allLines.filter((l) => l.line.toLowerCase().includes(q.toLowerCase())) : allLines),
    [allLines, q],
  );
  const { errors, warnings } = useMemo(() => {
    let e = 0;
    let w = 0;
    for (const l of allLines) {
      const t = l.line.toLowerCase();
      if (t.includes("error") || t.includes("err!") || t.includes("✗") || t.includes("failed")) e++;
      else if (t.includes("warn") || t.includes("deprecated")) w++;
    }
    return { errors: e, warnings: w };
  }, [allLines]);

  useEffect(() => {
    if (building && logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
  }, [allLines.length, building]);

  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center justify-between px-5 py-3.5">
        <span className="flex items-center gap-2 text-sm font-semibold">
          {open ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
          Build Logs
        </span>
        <span className="flex items-center gap-3 text-xs text-secondary">
          {elapsed}s
          {building ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : errored ? (
            <span className="flex h-5 w-5 items-center justify-center rounded-full bg-red-500 text-white">
              <CircleX className="h-3.5 w-3.5" />
            </span>
          ) : null}
        </span>
      </button>

      {open && (
        <div className="border-t border-border">
          <div className="flex items-center gap-3 px-4 py-2">
            <button
              className="text-muted hover:text-fg"
              onClick={() => navigator.clipboard?.writeText(allLines.map((l) => l.line).join("\n"))}
              title="Copy logs"
            >
              <Copy className="h-4 w-4" />
            </button>
            <span className="text-xs text-secondary">{allLines.length} lines</span>
            {errors > 0 && (
              <span className="flex items-center gap-1 text-xs text-red-500">
                <CircleX className="h-3.5 w-3.5" /> {errors}
              </span>
            )}
            {warnings > 0 && (
              <span className="flex items-center gap-1 text-xs text-amber-500">
                <TriangleAlert className="h-3.5 w-3.5" /> {warnings}
              </span>
            )}
            <div className="relative ml-auto w-56">
              <Search className="absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted" />
              <input
                value={q}
                onChange={(e) => setQ(e.target.value)}
                placeholder="Find in logs"
                className="w-full rounded-md border border-border bg-bg py-1 pl-8 pr-2 text-xs focus:border-border-strong focus:outline-none"
              />
            </div>
          </div>
          <div ref={logRef} className="max-h-[28rem] overflow-auto border-t border-border bg-subtle/40 px-4 py-3 font-mono text-xs">
            {lines.map((l, i) => (
              <div key={i} className="flex gap-3 py-px leading-relaxed">
                <span className="shrink-0 select-none text-muted">{fmtTime(l.ts_ms)}</span>
                <HighlightLine line={l.line} q={q} />
              </div>
            ))}
            {!lines.length && (
              <div className="py-4 text-muted">{building ? "Waiting for logs…" : q ? "No matching lines." : "No logs."}</div>
            )}
            {building && (
              <div className="flex gap-3 py-px text-muted">
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
