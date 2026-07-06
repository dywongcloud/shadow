// Shared build-log rendering: timestamp formatting + content-aware syntax
// highlighting for a single log line. Used by the deploy build page and the
// deployment detail page so both render logs identically.

import React from "react";
import { cn } from "@/lib/utils";

/** `HH:MM:SS.mmm` for a log line's epoch-ms timestamp. */
export function fmtTime(ms: number): string {
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
  if (
    /^(npm|pnpm|yarn) /.test(lower) ||
    lower.startsWith("added ") ||
    lower.includes("packages in") ||
    lower.includes("found 0 vulnerabilities")
  ) {
    return "text-muted";
  }
  if (
    lower.includes("error") ||
    lower.includes("err!") ||
    lower.includes("✗") ||
    lower.includes("failed") ||
    lower.includes("build failed")
  ) {
    return "text-red-500";
  }
  if (lower.includes("warning") || lower.includes("warn") || lower.includes("deprecated")) {
    return "text-amber-500";
  }
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
  if (/^(detected|cloning|mapped|loaded|uploading|running build in|build machine)/i.test(t)) {
    return "text-sky-400";
  }
  return "text-fg";
}

/** Render a single log line with content-aware syntax highlighting + find marks. */
export function HighlightLine({ line, q }: { line: string; q: string }) {
  const indentMatch = line.match(/^(\s*)/);
  const indent = indentMatch ? indentMatch[1] : "";
  const body = line.slice(indent.length);
  const trimmed = body.trim();

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
