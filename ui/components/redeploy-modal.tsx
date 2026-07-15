"use client";

import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  ArrowUpCircle,
  ChevronDown,
  Clock,
  ExternalLink,
  GitBranch,
  GitCommitHorizontal,
  Globe,
  Loader2,
  Check,
} from "lucide-react";
import { Button } from "@/components/ui";
import { apiDeployViaServerRoute, currentTeam, type Deployment } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { deploymentHost } from "@/lib/deploy-url";
import { toast } from "@/components/toast";
import { addPendingBuild } from "@/lib/pending-builds";

type Env = "production" | "preview";

/**
 * Vercel-style Redeploy confirmation modal. Lets the user choose the target
 * environment and whether to reuse the existing build cache, then kicks off a
 * redeploy and surfaces a blue "New Deployment Created" toast.
 */
export function RedeployModal({
  deployment,
  prodAlias,
  onClose,
  onDone,
}: {
  /** The deployment being redeployed (source of the new build). */
  deployment: Deployment;
  /** The project's production-domain alias (for "Assigned domains"). */
  prodAlias: string;
  onClose: () => void;
  /** Called after the redeploy build has started, with its build id + the chosen
   *  environment (so the caller can show an optimistic "Building" row). */
  onDone?: (buildId: string, env: Env) => void;
}) {
  const initialEnv: Env = deployment.target === "preview" || (!deployment.production && deployment.target !== "production") ? "preview" : "production";
  const [env, setEnv] = useState<Env>(initialEnv);
  const [envOpen, setEnvOpen] = useState(false);
  const [useCache, setUseCache] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const cardRef = useRef<HTMLDivElement>(null);

  // Close on Escape (unless mid-redeploy).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  async function redeploy() {
    setBusy(true);
    setError("");
    try {
      // Via the server route so a redeploy of a PRIVATE github repo gets the user's
      // GitHub token attached server-side (never in the browser) for the clone.
      const r = await apiDeployViaServerRoute<{ build_id: string }>(
        `/api/projects/${encodeURIComponent(deployment.project)}/redeploy`,
        { target: env, use_cache: useCache },
      );
      // Persist the in-flight build so its "Building" row survives navigation/reload.
      addPendingBuild({ id: r.build_id, project: deployment.project, team: currentTeam(), env });
      toast("New Deployment Created");
      onDone?.(r.build_id, env);
      onClose();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  }

  const selfHost = deploymentHost(deployment.id_alias || deployment.commit_alias || deployment.alias);
  // Assigned domain reflects the CHOSEN environment: production → the project's
  // production domain; preview → the generated preview URL (branch alias, else the
  // unique deployment alias) — never the production URL for a preview redeploy.
  const previewHost = deploymentHost(
    deployment.branch_alias || deployment.id_alias || deployment.commit_alias || deployment.alias,
  );
  const domainHost = env === "production" ? deploymentHost(prodAlias) : previewHost;
  const commitMsg = deployment.git?.commit_message || "Latest commit";

  if (typeof document === "undefined") return null;

  return createPortal(
    <div
      className="fixed inset-0 z-[200] flex items-start justify-center overflow-y-auto bg-black/50 p-4 pt-[10vh] backdrop-blur-sm"
      onMouseDown={(e) => { if (e.target === e.currentTarget && !busy) onClose(); }}
    >
      <div
        ref={cardRef}
        role="dialog"
        aria-modal="true"
        aria-label="Redeploy"
        className="w-full max-w-lg rounded-2xl border border-border bg-card shadow-pop"
      >
        <div className="p-6 sm:p-7">
          <h2 className="text-xl font-semibold tracking-tight">Redeploy</h2>
          <p className="mt-2 text-sm text-secondary">
            Create a new deployment with the same source code as your current one but with the latest Project Settings.
          </p>

          {/* Choose Environment */}
          <label className="mb-1.5 mt-6 block text-sm text-secondary">Choose Environment</label>
          <div className="relative">
            <button
              type="button"
              onClick={() => setEnvOpen((o) => !o)}
              className="flex w-full items-center justify-between rounded-lg border border-border bg-card px-3.5 py-3 text-sm hover:border-border-strong"
            >
              <span className="flex items-center gap-2.5">
                <ArrowUpCircle className="h-5 w-5 text-secondary" />
                <span className="font-medium capitalize">{env}</span>
              </span>
              <span className="flex items-center gap-2">
                <span className="rounded-md bg-[#0070f3]/10 px-2 py-0.5 text-xs font-medium capitalize text-[#0070f3]">{env}</span>
                <ChevronDown className={`h-4 w-4 text-muted transition-transform ${envOpen ? "rotate-180" : ""}`} />
              </span>
            </button>
            {envOpen && (
              <div className="absolute left-0 right-0 top-full z-10 mt-1.5 overflow-hidden rounded-lg border border-border bg-card py-1 shadow-pop">
                {(["production", "preview"] as Env[]).map((opt) => (
                  <button
                    key={opt}
                    type="button"
                    onClick={() => { setEnv(opt); setEnvOpen(false); }}
                    className="flex w-full items-center justify-between px-3.5 py-2.5 text-left text-sm hover:bg-subtle"
                  >
                    <span className="flex items-center gap-2.5">
                      <ArrowUpCircle className="h-5 w-5 text-secondary" />
                      <span className="font-medium capitalize">{opt}</span>
                    </span>
                    {env === opt && <Check className="h-4 w-4 text-[#0070f3]" />}
                  </button>
                ))}
              </div>
            )}
          </div>

          {/* Current deployment */}
          <div className="mt-3 rounded-lg border border-border p-4">
            <div className="flex items-start gap-3">
              <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full border-[5px] border-[#0070f3]" />
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="truncate font-mono text-sm">{selfHost}</span>
                  <span className="rounded-md bg-[#0070f3]/10 px-2 py-0.5 text-xs font-medium text-[#0070f3]">Current</span>
                </div>
                <div className="mt-2 flex items-center gap-1.5 text-sm text-secondary">
                  <GitBranch className="h-3.5 w-3.5" /> <span className="font-mono text-xs">{deployment.git?.branch || "—"}</span>
                </div>
                <div className="mt-1 flex items-center gap-1.5 text-sm text-secondary">
                  <GitCommitHorizontal className="h-3.5 w-3.5 shrink-0" /> <span className="truncate text-xs">{commitMsg}</span>
                </div>
                <div className="mt-1 flex items-center gap-1.5 text-sm text-secondary">
                  <Clock className="h-3.5 w-3.5" /> <span className="text-xs">{timeAgo(deployment.created_at_ms)} ago</span>
                </div>
              </div>
            </div>
          </div>

          {/* Assigned domains */}
          <div className="mt-4">
            <div className="text-sm text-secondary">Assigned domains:</div>
            <div className="mt-1 flex items-center gap-1.5 text-sm">
              <Globe className="h-3.5 w-3.5 text-muted" /> <span className="font-mono text-xs">{domainHost}</span>
            </div>
          </div>

          {/* Use existing Build Cache */}
          <label className="mt-5 flex cursor-pointer items-center gap-2.5 text-sm">
            <button
              type="button"
              role="checkbox"
              aria-checked={useCache}
              onClick={() => setUseCache((v) => !v)}
              className={`flex h-[18px] w-[18px] shrink-0 items-center justify-center rounded-[5px] border transition-colors ${
                useCache ? "border-fg bg-fg text-bg" : "border-border-strong bg-card"
              }`}
            >
              {useCache && <Check className="h-3.5 w-3.5" strokeWidth={3} />}
            </button>
            <span>
              Use existing Build Cache.{" "}
              <a
                href="https://vercel.com/docs/deployments/troubleshoot-a-build#build-cache"
                target="_blank"
                rel="noreferrer"
                className="text-link hover:underline"
                onClick={(e) => e.stopPropagation()}
              >
                Learn about Build Cache <ExternalLink className="inline h-3 w-3" />
              </a>
            </span>
          </label>

          {error && <p className="mt-4 text-sm text-red-600 dark:text-red-400">{error}</p>}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-between gap-3 border-t border-border px-6 py-4 sm:px-7">
          <Button variant="outline" onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button onClick={redeploy} disabled={busy} className="bg-fg text-bg">
            {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : "Redeploy"}
          </Button>
        </div>
      </div>
    </div>,
    document.body
  );
}
