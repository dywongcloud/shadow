"use client";

import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { Github, GitBranch, Loader2, Plus, Trash2, ChevronDown } from "lucide-react";
import { Button, Input } from "@/components/ui";
import { apiDeployViaServerRoute, currentTeam } from "@/lib/api";
import { toast } from "@/components/toast";
import { addPendingBuild } from "@/lib/pending-builds";

type Env = "production" | "preview";

/**
 * Vercel-style "Create Deployment" modal for a project's Deployments tab. Lets
 * the user create a fresh deployment from a branch / commit / URL (defaulting to
 * the project's connected repo) plus optional environment variables, instead of
 * being bounced to the new-project import screen. Submits to the same
 * `/v1/git/deploy` endpoint with the project name pinned, so it lands as a new
 * deployment of THIS project.
 */
export function CreateDeploymentModal({
  project,
  repoUrl,
  branch,
  onClose,
  onDone,
}: {
  project: string;
  /** The project's connected repo URL (for display + default source). */
  repoUrl: string;
  /** Default branch (shown as a chip; overridable via the source field). */
  branch?: string;
  onClose: () => void;
  /** Called once the build has started, with its build id + chosen environment. */
  onDone?: (buildId: string, env: Env) => void;
}) {
  // Source defaults to the connected repo; the user can paste a branch/commit/URL.
  const [source, setSource] = useState(repoUrl || "");
  const [envOpen, setEnvOpen] = useState(false);
  const [rows, setRows] = useState<{ k: string; v: string }[]>([{ k: "", v: "" }]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape" && !busy) onClose(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  // Expand a pasted `.env` blob (KEY=VALUE lines) into rows.
  function onPaste(e: React.ClipboardEvent, idx: number) {
    const text = e.clipboardData.getData("text");
    if (!text.includes("\n") && !text.includes("=")) return;
    e.preventDefault();
    const parsed = text
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith("#") && l.includes("="))
      .map((l) => {
        const i = l.indexOf("=");
        return { k: l.slice(0, i).trim(), v: l.slice(i + 1).trim().replace(/^["']|["']$/g, "") };
      });
    if (!parsed.length) return;
    setRows((cur) => {
      const next = [...cur];
      next.splice(idx, 1, ...parsed);
      return next.filter((r) => r.k || r.v).concat({ k: "", v: "" });
    });
  }

  function buildEnv(): Record<string, string> {
    const out: Record<string, string> = {};
    for (const r of rows) if (r.k.trim()) out[r.k.trim()] = r.v;
    return out;
  }

  async function create(target: Env) {
    setBusy(true);
    setError("");
    try {
      // A pasted full git URL overrides the source repo; otherwise treat the
      // field as a branch/commit ref against the connected repo.
      const isUrl = /^(https?:\/\/|git@)/.test(source.trim());
      const repo_url = isUrl ? source.trim() : repoUrl;
      const ref = isUrl ? branch : source.trim();
      // A pasted full (40-hex) or short (7-40-hex) commit SHA must go through
      // the backend's `commit` field (GitDeployRequest.commit), NOT `branch` —
      // sending it as `branch` fed `git clone --branch <sha>`, which fails for
      // a bare commit hash (a shallow clone's `--branch` needs an advertised
      // ref, not an arbitrary commit) and silently fell back to the connected
      // repo's default branch tip instead of the requested exact commit. A
      // short SHA (<40 hex) is excluded from the "full SHA" branch label below
      // only in the sense that both send through `commit` identically here;
      // the length distinction only matters for display, not for this request.
      const isSha = /^[0-9a-f]{7,40}$/i.test(ref || "");
      const env = buildEnv();
      // Via the server route so a PRIVATE github repo gets the user's GitHub token
      // attached server-side (never in the browser) for the clone.
      const r = await apiDeployViaServerRoute<{ build_id: string }>("/api/git/deploy", {
        repo_url,
        branch: isSha ? undefined : ref || undefined,
        commit: isSha ? ref : undefined,
        project,
        target,
        production: target === "production",
        env: Object.keys(env).length ? env : undefined,
        // This is a new deployment of an EXISTING project (we're on its page), not
        // a new-project create — keep the name verbatim, never 409 / suffix.
        redeploy: true,
      });
      // Persist the in-flight build so its "Building" row survives navigation/reload.
      addPendingBuild({ id: r.build_id, project, team: currentTeam(), env: target });
      toast("New Deployment Created");
      onDone?.(r.build_id, target);
      onClose();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  }

  if (typeof document === "undefined") return null;

  return createPortal(
    <div
      className="fixed inset-0 z-[200] flex items-start justify-center overflow-y-auto bg-black/50 p-4 pt-[10vh] backdrop-blur-sm"
      onMouseDown={(e) => { if (e.target === e.currentTarget && !busy) onClose(); }}
    >
      <div role="dialog" aria-modal="true" aria-label="Create Deployment" className="w-full max-w-lg rounded-2xl border border-border bg-card shadow-pop">
        <div className="p-6 sm:p-7">
          <h2 className="text-xl font-semibold tracking-tight">Create Deployment</h2>

          {/* Connected repo */}
          <div className="mt-5 flex items-center gap-3">
            <Github className="h-7 w-7 text-secondary" />
            <div className="min-w-0">
              <div className="truncate font-medium">{repoUrl ? repoUrl.replace(/^https?:\/\/(www\.)?github\.com\//, "").replace(/\.git$/, "") : project}</div>
              <div className="text-xs text-secondary">Connected repository</div>
            </div>
          </div>

          <p className="mt-5 text-sm text-secondary">
            Paste a branch, commit, or repository URL to create a new deployment of <span className="font-medium text-fg">{project}</span>.
          </p>

          <label className="mb-1.5 mt-4 block text-sm text-secondary">Branch, Commit, or URL</label>
          <Input value={source} onChange={(e) => setSource(e.target.value)} placeholder={repoUrl || "https://github.com/owner/repo"} />
          {branch && (
            <div className="mt-2 inline-flex items-center gap-1.5 rounded-md border border-border px-2 py-1 text-xs">
              <GitBranch className="h-3.5 w-3.5" /> <span className="font-mono">{branch}</span>
            </div>
          )}

          {/* Environment variables (collapsible) */}
          <button
            type="button"
            onClick={() => setEnvOpen((o) => !o)}
            className="mt-5 flex w-full items-center justify-between rounded-lg border border-border px-3.5 py-2.5 text-sm hover:border-border-strong"
          >
            <span className="font-medium">Environment Variables</span>
            <ChevronDown className={`h-4 w-4 text-muted transition-transform ${envOpen ? "rotate-180" : ""}`} />
          </button>
          {envOpen && (
            <div className="mt-2 space-y-2">
              {rows.map((row, i) => (
                <div key={i} className="flex items-center gap-2">
                  <Input className="flex-1 font-mono text-xs" placeholder="KEY" value={row.k}
                    onPaste={(e) => onPaste(e, i)}
                    onChange={(e) => setRows((c) => c.map((r, j) => (j === i ? { ...r, k: e.target.value } : r)))} />
                  <Input className="flex-1 font-mono text-xs" placeholder="value" value={row.v}
                    onChange={(e) => setRows((c) => c.map((r, j) => (j === i ? { ...r, v: e.target.value } : r)))} />
                  <button type="button" className="text-muted hover:text-fg" onClick={() => setRows((c) => c.filter((_, j) => j !== i).concat(c.length === 1 ? [{ k: "", v: "" }] : []))}>
                    <Trash2 className="h-4 w-4" />
                  </button>
                </div>
              ))}
              <Button variant="outline" onClick={() => setRows((c) => [...c, { k: "", v: "" }])}>
                <Plus className="h-3.5 w-3.5" /> Add
              </Button>
              <p className="text-xs text-muted">Tip: paste a .env file into a KEY field to import many at once.</p>
            </div>
          )}

          {error && <p className="mt-4 text-sm text-red-600 dark:text-red-400">{error}</p>}
        </div>

        <div className="flex items-center justify-between gap-3 border-t border-border px-6 py-4 sm:px-7">
          <Button variant="outline" onClick={onClose} disabled={busy}>Cancel</Button>
          <div className="flex gap-2">
            <Button variant="outline" onClick={() => create("preview")} disabled={busy || !source.trim()}>
              {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : "Create Preview"}
            </Button>
            <Button onClick={() => create("production")} disabled={busy || !source.trim()} className="bg-fg text-bg">
              {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : "Create Deployment"}
            </Button>
          </div>
        </div>
      </div>
    </div>,
    document.body
  );
}
