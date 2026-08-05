"use client";

// browser-run-node-target-picker: the authenticated target picker for the
// /run-node page. Lists what this node can OPTIONALLY attach itself to in the
// current tenant (derived from the replicated deployment descriptor metadata
// the /cloud deployments endpoint already serves — local with leader/owner
// fallback handled server-side): browser-eligible functions with their runtime
// limits, and deployments whose only browser contribution is a replicated
// database. Artifact source and digests are never shown and never asked for.
//
// browser-auto-serve-eligible-set: this picker is no longer how work REACHES a
// node. The first, default option is automatic — the fleet derives this
// tenant's whole browser-eligible set server-side and refreshes it on every
// admission renewal, so a function deployed after the node started begins being
// served without anyone touching this page. What remains here is (a) an
// explicit "capacity only" refusal and (b) pinning ONE function or database on
// purpose, which is exactly as narrow as it looks.
//
// browser-node-optional-serve-target: this picker is still NOT a gate — every
// option, including an empty list, starts a working node. Start never depends
// on anything chosen here.
//
// Keyboard: a plain radio group, so arrow keys move between options and Tab
// enters/leaves the group — no custom key handling to get wrong. The whole
// card is the <label>, which keeps the click/tap target large on mobile.

import { Database, RadioTower, Radio, Sparkles } from "lucide-react";
import type { ExcludedDeployment, RunNodeTarget } from "@/lib/run-node-targets";

export interface TargetSelection {
  deployment: string;
  fn: string;
}

/** What the picker can be set to: the automatic eligible set (the default —
 *  the fleet decides, live), capacity only, or ONE deliberately pinned target. */
export type PickerSelection = TargetSelection | "auto" | "none";

export function targetKey(t: TargetSelection): string {
  return `${t.deployment}\u0000${t.fn}`;
}

const AUTO_KEY = "\u0000auto";
const NONE_KEY = "\u0000none";

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

function formatTimeout(ms: number): string {
  return ms % 1000 === 0 ? `${ms / 1000}s` : `${ms}ms`;
}

function selectionKey(sel: PickerSelection): string {
  return sel === "auto" ? AUTO_KEY : sel === "none" ? NONE_KEY : targetKey(sel);
}

export function TargetPicker({
  targets,
  excluded,
  loading,
  fetchError,
  selected,
  onSelect,
  disabled,
}: {
  targets: RunNodeTarget[];
  /** Deployments contributing no selectable target because they aren't ready,
   *  or expose neither a browser-eligible function nor a browser database —
   *  each with the REASON. A bare count told the one person who cared least
   *  ("something is excluded") and the one who cared most ("which of my 14, and
   *  why?") exactly the same nothing, and its single blanket sentence was wrong
   *  for every deployment that simply predates eligibility evaluation. */
  excluded: ExcludedDeployment[];
  loading: boolean;
  fetchError: string | null;
  /** `"auto"` = the automatic eligible set (the default), `"none"` = the
   *  explicit "serve nothing" choice, an object = one pinned target. All three
   *  are real, supported ways to run a node — never an unfinished form. */
  selected: PickerSelection;
  onSelect: (sel: PickerSelection) => void;
  /** True while the node is running — the attached target is fixed for the
   *  node's lifetime, exactly like the free-form inputs it replaces were. */
  disabled: boolean;
}) {
  const selectedKey = selectionKey(selected);
  const eligible = targets.filter((t) => t.kind === "function").length;

  return (
    <div>
      <div role="radiogroup" aria-label="What this node serves" className="grid gap-2">
        <Option
          optionKey={AUTO_KEY}
          checked={selectedKey === AUTO_KEY}
          disabled={disabled}
          onSelect={() => onSelect("auto")}
          icon={<Sparkles className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />}
          title="Everything this team can run in a browser"
          detail={
            <>
              {/* The count is what the picker can SEE right now; the node is
                  admitted for whatever the fleet resolves at admission time and
                  re-resolves on every renewal, so this never claims to be the
                  authoritative set — it describes it. */}
              {loading && !fetchError
                ? "Checking what this team has…"
                : eligible === 0
                  ? "Nothing is browser-eligible in this team yet — the node still joins the mesh and starts serving the moment something is, with no restart."
                  : `${eligible} browser-eligible function${eligible === 1 ? "" : "s"} right now. New deployments are picked up automatically, within a minute, without restarting the node.`}
            </>
          }
        />
        <Option
          optionKey={NONE_KEY}
          checked={selectedKey === NONE_KEY}
          disabled={disabled}
          onSelect={() => onSelect("none")}
          icon={<Radio className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />}
          title="Nothing — just contribute capacity"
          detail="Joins the mesh, holds a relay identity and appears on the constellation map. Serves no function and replicates no database."
        />
        {targets.length > 0 && (
          <p className="mt-1 text-[11px] text-muted">
            Or pin exactly one thing — this node then serves only that, and nothing new is added to it
            automatically.
          </p>
        )}
        {targets.map((t) => {
          const key = targetKey(t);
          return t.kind === "function" ? (
            <Option
              key={key}
              optionKey={key}
              checked={selectedKey === key}
              disabled={disabled}
              onSelect={() => onSelect({ deployment: t.deployment, fn: t.fn })}
              icon={<RadioTower className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />}
              title={
                <>
                  {t.project} <span className="font-normal text-secondary">/ {t.fn}</span>
                </>
              }
              mono={t.deployment}
              detail={
                <>
                  {t.mode} · timeout {formatTimeout(t.timeoutMs)} · memory {formatBytes(t.memoryBytes)} · bundle{" "}
                  {formatBytes(t.sourceBytes)}
                  {t.allowedOps.length > 0 &&
                    ` · ${t.allowedOps.length} host op${t.allowedOps.length === 1 ? "" : "s"}`}
                </>
              }
            />
          ) : (
            <Option
              key={key}
              optionKey={key}
              checked={selectedKey === key}
              disabled={disabled}
              onSelect={() => onSelect({ deployment: t.deployment, fn: t.fn })}
              icon={<Database className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />}
              title={
                <>
                  {t.project} <span className="font-normal text-secondary">/ database only</span>
                </>
              }
              mono={t.deployment}
              detail={
                <>
                  replicates this project&apos;s database — no function to serve · up to {formatBytes(t.maxBytes)}
                  {t.tables.length > 0 &&
                    ` · ${t.tables.length} table${t.tables.length === 1 ? "" : "s"}`}
                  {t.publicRead && " · public read"}
                </>
              }
            />
          );
        })}
      </div>
      {loading && <p className="mt-2 text-[11px] text-muted">Loading this team&apos;s deployments…</p>}
      {fetchError && (
        <div role="alert" className="mt-2 rounded-md bg-red-500/10 p-2 text-xs text-red-500">
          Couldn&apos;t load this team&apos;s deployments: {fetchError}. Retrying automatically — you can start the
          node without a target in the meantime.
        </div>
      )}
      {!loading && !fetchError && targets.length === 0 && (
        <div className="mt-2 rounded-md border border-dashed border-border-strong p-3 text-xs leading-relaxed text-secondary">
          <p className="font-medium text-fg">Nothing in this team can run in a browser yet.</p>
          <p className="mt-1">
            That doesn&apos;t stop you running a node — it just runs with no serve lane until something is
            eligible, and then it picks that up{" "}
            <span className="font-medium text-fg">on its own, without restarting</span>. A function becomes
            eligible automatically once it&apos;s a JS or Bun request→response handler that exports{" "}
            <code className="font-mono">module.exports</code> (or ships a <code className="font-mono">browser.js</code>{" "}
            handler) and uses no Node/Bun-only APIs. Containers, long-running servers (Next.js, Express),
            TypeScript, and Python/Go functions can&apos;t execute in a browser engine at all. A project with a
            fluid.json <code className="font-mono">browser_db</code> block also appears here, database-only.
          </p>
          <ExcludedList excluded={excluded} className="mt-2" />
        </div>
      )}
      {!loading && !fetchError && targets.length > 0 && (
        <ExcludedList excluded={excluded} className="mt-2" />
      )}
    </div>
  );
}

/** The per-deployment "why isn't mine here?" disclosure. Collapsed by default
 *  (it is a diagnostic, not the primary flow) but every reason is one click
 *  away and none of them is invented in this file: a ready deployment's reason
 *  is the BUILD's own verdict, stamped on the manifest and gossiped, so the
 *  sentence shown here is the sentence the build log printed. */
function ExcludedList({ excluded, className }: { excluded: ExcludedDeployment[]; className?: string }) {
  if (excluded.length === 0) return null;
  return (
    <details className={`${className ?? ""} text-[11px] text-muted`}>
      <summary className="min-h-11 cursor-pointer py-3 hover:text-secondary">
        {excluded.length} deployment{excluded.length === 1 ? "" : "s"} not shown — why?
      </summary>
      <ul className="mt-1 grid gap-1.5 border-l border-border pl-3">
        {excluded.map((e) => (
          <li key={e.deployment}>
            <span className="font-medium text-secondary">{e.project}</span>{" "}
            <span className="font-mono text-[10px]">{e.deployment}</span>
            <span className="mt-0.5 block leading-relaxed">{e.reason}</span>
          </li>
        ))}
      </ul>
    </details>
  );
}

function Option({
  optionKey,
  checked,
  disabled,
  onSelect,
  icon,
  title,
  mono,
  detail,
}: {
  optionKey: string;
  checked: boolean;
  disabled: boolean;
  onSelect: () => void;
  icon: React.ReactNode;
  title: React.ReactNode;
  mono?: string;
  detail: React.ReactNode;
}) {
  return (
    <label
      className={`flex min-h-11 cursor-pointer items-start gap-3 rounded-md border p-3 transition-colors ${
        checked ? "border-fg bg-subtle" : "border-border hover:border-border-strong"
      } ${disabled ? "cursor-not-allowed opacity-60" : ""} has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-fg has-[:focus-visible]:ring-offset-1`}
    >
      <input
        type="radio"
        name="run-node-target"
        value={optionKey}
        checked={checked}
        disabled={disabled}
        onChange={onSelect}
        className="mt-0.5 shrink-0 accent-fg"
      />
      <span className="min-w-0 flex-1">
        <span className="flex items-center gap-1.5 text-sm font-medium text-fg">
          {icon}
          <span className="truncate">{title}</span>
        </span>
        {mono && <span className="mt-0.5 block truncate font-mono text-[11px] text-muted">{mono}</span>}
        <span className="mt-1 block text-[11px] leading-relaxed text-secondary">{detail}</span>
      </span>
    </label>
  );
}
