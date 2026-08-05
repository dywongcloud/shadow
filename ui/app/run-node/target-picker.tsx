"use client";

// browser-run-node-target-picker: the authenticated serve-target picker for
// the /run-node page. Lists ONLY ready, browser-capable functions in the
// current tenant (derived from the replicated deployment descriptor metadata
// the /cloud deployments endpoint already serves — local with leader/owner
// fallback handled server-side), with each target's runtime limits. Artifact
// source and digests are never shown and never asked for.
//
// Keyboard: a plain radio group, so arrow keys move between targets and Tab
// enters/leaves the group — no custom key handling to get wrong. The whole
// card is the <label>, which keeps the click/tap target large on mobile.

import { RadioTower } from "lucide-react";
import type { BrowserTarget } from "@/lib/run-node-targets";

export interface TargetSelection {
  deployment: string;
  fn: string;
}

export function targetKey(t: TargetSelection): string {
  return `${t.deployment}${t.fn}`;
}

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

function formatTimeout(ms: number): string {
  return ms % 1000 === 0 ? `${ms / 1000}s` : `${ms}ms`;
}

export function TargetPicker({
  targets,
  excludedCount,
  loading,
  fetchError,
  selected,
  onSelect,
  disabled,
}: {
  targets: BrowserTarget[];
  /** Deployments not shown because they aren't ready or have no
   *  browser-eligible function — surfaced as a footnote count so a non-empty
   *  tenant doesn't look identical to an empty one. */
  excludedCount: number;
  loading: boolean;
  fetchError: string | null;
  selected: TargetSelection | null;
  onSelect: (sel: TargetSelection) => void;
  /** True while the node is running — the served target is fixed for the
   *  node's lifetime, exactly like the free-form inputs it replaces were. */
  disabled: boolean;
}) {
  if (loading) {
    return <p className="text-sm text-secondary">Loading this team&apos;s deployments…</p>;
  }
  if (fetchError) {
    return (
      <div role="alert" className="rounded-md bg-red-500/10 p-2 text-xs text-red-500">
        Couldn&apos;t load this team&apos;s deployments: {fetchError}. Retrying automatically.
      </div>
    );
  }
  if (targets.length === 0) {
    return (
      <div className="rounded-md border border-dashed border-border-strong p-3 text-xs leading-relaxed text-secondary">
        <p className="font-medium text-fg">No browser-eligible functions in this team yet.</p>
        <p className="mt-1">
          A function runs here <span className="font-medium text-fg">automatically</span> once it&apos;s
          browser-eligible — a JS or Bun request→response handler that exports{" "}
          <code className="font-mono">module.exports</code> (or ships a <code className="font-mono">browser.js</code>{" "}
          handler) and uses no Node/Bun-only APIs. No <code className="font-mono">fluid.json</code> opt-in is
          required. Containers, long-running servers (Next.js, Express), TypeScript, and Python/Go functions
          can&apos;t run in a browser and are skipped.
          {excludedCount > 0 &&
            ` (${excludedCount} deployment${excludedCount === 1 ? "" : "s"} not shown — not ready, or not browser-eligible.)`}
        </p>
      </div>
    );
  }

  return (
    <div>
      <div role="radiogroup" aria-label="Serve target" className="grid gap-2">
        {targets.map((t) => {
          const key = targetKey(t);
          const checked = selected !== null && targetKey(selected) === key;
          return (
            <label
              key={key}
              className={`flex min-h-11 cursor-pointer items-start gap-3 rounded-md border p-3 transition-colors ${
                checked ? "border-fg bg-subtle" : "border-border hover:border-border-strong"
              } ${disabled ? "cursor-not-allowed opacity-60" : ""} has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-fg has-[:focus-visible]:ring-offset-1`}
            >
              <input
                type="radio"
                name="run-node-target"
                value={key}
                checked={checked}
                disabled={disabled}
                onChange={() => onSelect({ deployment: t.deployment, fn: t.fn })}
                className="mt-0.5 shrink-0 accent-fg"
              />
              <span className="min-w-0 flex-1">
                <span className="flex items-center gap-1.5 text-sm font-medium text-fg">
                  <RadioTower className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                  <span className="truncate">
                    {t.project} <span className="font-normal text-secondary">/ {t.fn}</span>
                  </span>
                </span>
                <span className="mt-0.5 block truncate font-mono text-[11px] text-muted">{t.deployment}</span>
                <span className="mt-1 block text-[11px] leading-relaxed text-secondary">
                  {t.mode} · timeout {formatTimeout(t.timeoutMs)} · memory {formatBytes(t.memoryBytes)} · bundle{" "}
                  {formatBytes(t.sourceBytes)}
                  {t.allowedOps.length > 0 && ` · ${t.allowedOps.length} host op${t.allowedOps.length === 1 ? "" : "s"}`}
                </span>
              </span>
            </label>
          );
        })}
      </div>
      {excludedCount > 0 && (
        <p className="mt-2 text-[11px] text-muted">
          {excludedCount} deployment{excludedCount === 1 ? "" : "s"} not shown — not ready, or not browser-eligible
          (container, long-running server, TypeScript, or uses Node/Bun APIs).
        </p>
      )}
    </div>
  );
}
