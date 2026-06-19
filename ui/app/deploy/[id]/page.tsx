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
import { deploymentUrl, deploymentHost } from "@/lib/deploy-url";

function fmtTime(ms: number) {
  const d = new Date(ms);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

// Wrap occurrences of `q` (case-insensitive) in the given string with a <mark>.
function withFindMarks(text: string, q: string): React.ReactNode {
  if (!q) return text;
  const needle = q.toLowerCase();
  const hay = text.toLowerCase();
  const parts: React.ReactNode[] = [];
  let from = 0;
  let idx = hay.indexOf(needle, from);
  if (idx === -1) return text;
  let key = 0;
  while (idx !== -1) {
    if (idx > from) parts.push(text.slice(from, idx));
    parts.push(
      <mark key={`m${key++}`} className="bg-amber-300/40 text-fg rounded px-0.5">
        {text.slice(idx, idx + q.length)}
      </mark>,
    );
    from = idx + q.length;
    idx = hay.indexOf(needle, from);
  }
  if (from < text.length) parts.push(text.slice(from));
  return <>{parts}</>;
}

// Highlight URLs and *.localhost aliases inside a chunk of text, then apply find-marks.
function highlightInline(text: string, q: string): React.ReactNode {
  const re = /(https?:\/\/[^\s]+|[a-z0-9_-]+\.localhost)/gi;
  const parts: React.ReactNode[] = [];
  let last = 0;
  let key = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) parts.push(withFindMarks(text.slice(last, m.index), q));
    const token = m[0];
    const isUrl = /^https?:\/\//i.test(token);
    parts.push(
      <span key={`t${key++}`} className={isUrl ? "text-link" : "text-emerald-400"}>
        {withFindMarks(token, q)}
      </span>,
    );
    last = m.index + token.length;
  }
  if (last < text.length) parts.push(withFindMarks(text.slice(last), q));
  return <>{parts}</>;
}

// Determine the base text color class for a log line by its content. First match wins.
function lineColor(line: string): string {
  const t = line.trim();
  const lower = t.toLowerCase();

  // npm/pnpm/yarn progress noise.
  if (
    /^(npm|pnpm|yarn) /.test(lower) ||
    lower.startsWith("added ") ||
    lower.includes("packages in") ||
    lower.includes("found 0 vulnerabilities")
  ) {
    return "text-muted";
  }

  // Error / failure.
  if (
    lower.includes("error") ||
    lower.includes("err!") ||
    lower.includes("✗") ||
    lower.includes("failed") ||
    lower.includes("build failed")
  ) {
    return "text-red-500";
  }

  // Warning.
  if (lower.includes("warning") || lower.includes("warn") || lower.includes("deprecated")) {
    return "text-amber-500";
  }

  // Success / ready.
  if (
    lower.includes("ready") ||
    lower.includes("completed") ||
    lower.includes("deployment ready") ||
    lower.includes("✓") ||
    lower.includes("success") ||
    lower.includes("compiled successfully") ||
    lower.includes("image built")
  ) {
    return "text-emerald-500";
  }

  // Detected-framework / info lines.
  if (/^(detected|cloning|mapped|loaded|uploading|running build in|build machine)/i.test(t)) {
    return "text-sky-400";
  }

  return "text-fg";
}

// Render a single log line with content-aware syntax highlighting.
function HighlightLine({ line, q }: { line: string; q: string }) {
  // Preserve the backend's leading indentation (some lines are prefixed with two spaces).
  const indentMatch = line.match(/^(\s*)/);
  const indent = indentMatch ? indentMatch[1] : "";
  const body = line.slice(indent.length);
  const trimmed = body.trim();

  // Command lines: `Running "npm install"`, or lines starting with `$` / `> `.
  const running = trimmed.match(/^Running\s+"(.+)"\s*$/);
  if (running) {
    return (
      <span className="whitespace-pre-wrap break-all text-fg">
        {indent}
        <span className="text-muted">Running </span>
        <span className="text-fg font-medium">&quot;{withFindMarks(running[1], q)}&quot;</span>
      </span>
    );
  }
  if (/^\$ /.test(trimmed) || /^> /.test(trimmed)) {
    return (
      <span className="whitespace-pre-wrap break-all text-fg font-medium">
        {indent}
        {highlightInline(body, q)}
      </span>
    );
  }

  return (
    <span className={cn("whitespace-pre-wrap break-all", lineColor(body))}>
      {indent}
      {highlightInline(body, q)}
    </span>
  );
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
                    <HighlightLine line={l.line} q={q} />
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
          <div className="flex min-w-0 items-center gap-2 text-sm text-secondary">
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
            <div className="flex shrink-0 gap-2">
              <a href={deploymentUrl(build.alias)} target="_blank" rel="noreferrer">
                <Button variant="outline" className="whitespace-nowrap">
                  Visit <ExternalLink className="h-3.5 w-3.5" />
                </Button>
              </a>
              <Link href={`/projects/${encodeURIComponent(build.project)}`}>
                <Button className="whitespace-nowrap">Continue to Project</Button>
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
          Tip: open <span className="font-mono">{deploymentHost(build?.alias)}</span>, or continue to the project for logs &amp; settings.
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
