"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import {
  Loader2,
  Check,
  ChevronDown,
  ChevronRight,
  Clock,
  Copy,
  Search,
  ExternalLink,
  GitBranch,
  CircleX,
} from "lucide-react";
import { Button, Card } from "@/components/ui";
import { apiGet, type Build } from "@/lib/api";
import { cn } from "@/lib/utils";

function fmtTime(ms: number) {
  const d = new Date(ms);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

export default function DeployPage({ params }: { params: { id: string } }) {
  const { id } = params;
  const [build, setBuild] = useState<Build | null>(null);
  const [logsOpen, setLogsOpen] = useState(true);
  const [q, setQ] = useState("");
  const [now, setNow] = useState(Date.now());
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let stop = false;
    async function tick() {
      try {
        const b = await apiGet<Build>(`/v1/builds/${id}`);
        if (!stop) setBuild(b);
        if (!stop && (b.state === "building" || b.state === "queued")) {
          setTimeout(tick, 500);
        }
      } catch {
        if (!stop) setTimeout(tick, 800);
      }
    }
    tick();
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => {
      stop = true;
      clearInterval(t);
    };
  }, [id]);

  // Auto-scroll logs while building.
  useEffect(() => {
    if (build?.state === "building" && logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight;
    }
  }, [build?.lines.length, build?.state]);

  const state = build?.state ?? "queued";
  const elapsed = build ? Math.max(0, Math.round(((build.finished_ms ?? now) - build.started_ms) / 1000)) : 0;
  const lines = (build?.lines ?? []).filter((l) => l.line.toLowerCase().includes(q.toLowerCase()));

  const ready = state === "ready";
  const errored = state === "error";

  return (
    <div className="mx-auto max-w-3xl">
      <Link href="/" className="mb-6 inline-flex items-center gap-2 text-sm text-secondary hover:text-fg">
        ← Back to dashboard
      </Link>

      <Card className="p-0">
        {/* Status header */}
        <div className="flex items-center gap-2 px-5 py-4 sm:px-6">
          {ready ? (
            <Check className="h-4 w-4 text-green" />
          ) : errored ? (
            <CircleX className="h-4 w-4 text-red-500" />
          ) : (
            <Loader2 className="h-4 w-4 animate-spin text-secondary" />
          )}
          <span className="text-sm font-medium">
            {ready ? "Deployment ready" : errored ? "Deployment failed" : `Deployment started ${elapsed}s ago…`}
          </span>
        </div>

        {/* Build Logs */}
        <div className="border-t border-border">
          <button
            onClick={() => setLogsOpen((o) => !o)}
            className="flex w-full items-center justify-between px-5 py-3 sm:px-6"
          >
            <span className="flex items-center gap-2 text-sm font-medium">
              {logsOpen ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
              Build Logs
            </span>
            <span className="flex items-center gap-2 text-xs text-secondary">
              {elapsed}s
              {!ready && !errored && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            </span>
          </button>

          {logsOpen && (
            <div className="border-t border-border">
              {/* toolbar */}
              <div className="flex items-center gap-3 px-4 py-2">
                <button
                  className="text-muted hover:text-fg"
                  onClick={() => navigator.clipboard?.writeText((build?.lines ?? []).map((l) => l.line).join("\n"))}
                  title="Copy logs"
                >
                  <Copy className="h-4 w-4" />
                </button>
                <span className="text-xs text-secondary">{build?.lines.length ?? 0} lines</span>
                <div className="relative ml-auto w-56">
                  <Search className="absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted" />
                  <input
                    value={q}
                    onChange={(e) => setQ(e.target.value)}
                    placeholder="Find in logs"
                    className="w-full rounded-md border border-border bg-card py-1 pl-8 pr-2 text-xs focus:border-border-strong focus:outline-none"
                  />
                </div>
              </div>
              {/* log lines */}
              <div ref={logRef} className="max-h-80 overflow-auto border-t border-border bg-subtle/50 px-4 py-3 font-mono text-xs">
                {lines.map((l, i) => (
                  <div key={i} className="flex gap-3 py-px leading-relaxed">
                    <span className="shrink-0 select-none text-muted">{fmtTime(l.ts_ms)}</span>
                    <span className="whitespace-pre-wrap break-all text-fg">{l.line}</span>
                  </div>
                ))}
                {!lines.length && <div className="py-4 text-muted">Waiting for logs…</div>}
                {!ready && !errored && (
                  <div className="flex gap-3 py-px text-muted">
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  </div>
                )}
              </div>
            </div>
          )}
        </div>

        {/* Pending / done steps */}
        <Step label="Deployment Summary" done={ready} pending={!ready && !errored} />
        <Step label="Assigning Custom Domains" done={ready} pending={!ready && !errored} />

        {/* Footer */}
        <div className="flex flex-col gap-3 border-t border-border px-5 py-3 sm:flex-row sm:items-center sm:justify-between sm:px-6">
          <div className="flex items-center gap-2 text-sm text-secondary">
            {build?.commit_message ? (
              <>
                <GitBranch className="h-3.5 w-3.5" />
                <span className="truncate">{build.commit_message}</span>
                <span className="font-mono text-xs">{build.commit}</span>
              </>
            ) : (
              <span className="font-mono text-xs">{build?.repo_url}</span>
            )}
          </div>
          {ready && build?.alias ? (
            <div className="flex gap-2">
              <a href={`http://${build.alias}:8787/`} target="_blank" rel="noreferrer">
                <Button variant="outline">
                  Visit <ExternalLink className="h-3.5 w-3.5" />
                </Button>
              </a>
              <Link href={`/projects/${encodeURIComponent(build.project)}`}>
                <Button>Continue to Project</Button>
              </Link>
            </div>
          ) : errored ? (
            <Link href="/new"><Button variant="outline">Try again</Button></Link>
          ) : (
            <Button variant="outline" disabled>
              Cancel Deployment
            </Button>
          )}
        </div>
      </Card>

      {ready && (
        <p className="mt-4 text-center text-sm text-secondary">
          Tip: open <span className="font-mono">{build?.alias}</span> on port 8787, or continue to the project for logs & settings.
        </p>
      )}
    </div>
  );
}

function Step({ label, done, pending }: { label: string; done: boolean; pending: boolean }) {
  return (
    <div className="flex items-center gap-2 border-t border-border px-5 py-3 text-sm sm:px-6">
      <ChevronRight className="h-4 w-4 text-muted" />
      <span className={cn(done ? "text-fg" : "text-secondary")}>{label}</span>
      <span className="ml-auto">
        {done ? <Check className="h-4 w-4 text-green" /> : <Clock className="h-4 w-4 text-muted" />}
      </span>
    </div>
  );
}
