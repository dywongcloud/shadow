"use client";

import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { Loader2, Plus, ShieldCheck, Trash2 } from "lucide-react";
import { Button, Input } from "@/components/ui";
import { apiDeployViaServerRoute, currentTeam } from "@/lib/api";
import { addPendingBuild } from "@/lib/pending-builds";

type Target = "production" | "preview";
type Row = { key: string; value: string };

const ERROR_MESSAGES: Record<string, string> = {
  NO_SESSION: "Sign in with Clerk before submitting a Marketplace deployment.",
  MARKETPLACE_JWT_UNAVAILABLE: "DevHub could not obtain the Clerk token template “autheo-marketplace-v1”. Ask an administrator to create or enable that template.",
  MARKETPLACE_CONFIG: "Marketplace is not configured correctly. Check MARKETPLACE_URL in DevHub.",
  MARKETPLACE_UNAVAILABLE: "Marketplace is temporarily unavailable. Verify its URL and try again.",
  MARKETPLACE_HTTP_401: "Marketplace did not authorize this Clerk session.",
  MARKETPLACE_HTTP_403: "Marketplace did not authorize this order for your current tenant.",
  MARKETPLACE_HTTP_404: "Marketplace could not find that order or placement policy.",
  MARKETPLACE_HTTP_410: "The Marketplace order or placement policy is no longer available.",
  ORDER_MISMATCH: "The placement policy belongs to a different Marketplace order.",
  TENANT_MISMATCH: "The placement policy buyer does not match your current Clerk organization or personal account.",
  POLICY_INACTIVE: "This Marketplace placement policy is not active (it may be suspended).",
  POLICY_REVOKED: "This Marketplace placement policy has been revoked.",
  POLICY_EXPIRED: "This Marketplace placement policy is expired or not active yet.",
  INCOMPATIBLE_VERSION: "This Marketplace placement policy uses an unsupported version.",
  MALFORMED_POLICY: "Marketplace returned an invalid placement policy. Contact the Marketplace provider.",
  MARKETPLACE_PLACEMENT_UNAVAILABLE: "No Marketplace-approved node is currently eligible. Approved nodes must still be healthy, reachable, capable, and have capacity.",
};

function errorMessage(error: unknown): string {
  const raw = String(error);
  for (const [code, message] of Object.entries(ERROR_MESSAGES)) if (raw.includes(code)) return message;
  return raw || "Marketplace deployment could not be started.";
}

function envFrom(rows: Row[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const row of rows) if (row.key.trim()) out[row.key.trim()] = row.value;
  return out;
}

export function MarketplaceDeploymentModal({ project, onClose, onDone }: {
  project?: string;
  onClose: () => void;
  onDone?: () => void;
}) {
  const [orderId, setOrderId] = useState("");
  const [repoUrl, setRepoUrl] = useState("");
  const [branch, setBranch] = useState("main");
  const [projectName, setProjectName] = useState(project ?? "");
  const [rootDir, setRootDir] = useState("");
  const [target, setTarget] = useState<Target>("production");
  const [useCache, setUseCache] = useState(true);
  const [rows, setRows] = useState<Row[]>([{ key: "", value: "" }]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape" && !busy) onClose(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  async function submit() {
    setBusy(true);
    setError("");
    try {
      const result = await apiDeployViaServerRoute<{ build_id: string }>("/api/marketplace/deploy", {
        marketplace_order_id: orderId.trim(),
        repo_url: repoUrl.trim(),
        branch: branch.trim() || undefined,
        project: projectName.trim() || undefined,
        root_dir: rootDir.trim() || undefined,
        target,
        use_cache: useCache,
        redeploy: !!project,
        env: envFrom(rows),
      });
      addPendingBuild({
        id: result.build_id,
        project: projectName.trim() || project || "marketplace-deployment",
        team: currentTeam(),
        env: target,
      });
      onDone?.();
      onClose();
    } catch (cause) {
      setError(errorMessage(cause));
      setBusy(false);
    }
  }

  if (typeof document === "undefined") return null;
  return createPortal(
    <div className="fixed inset-0 z-[200] flex items-start justify-center overflow-y-auto bg-black/50 p-4 pt-[7vh] backdrop-blur-sm" onMouseDown={(event) => event.target === event.currentTarget && !busy && onClose()}>
      <div role="dialog" aria-modal="true" aria-label="Deploy from Marketplace" className="w-full max-w-2xl rounded-2xl border border-border bg-card shadow-pop">
        <div className="p-6 sm:p-7">
          <div className="flex items-start gap-3">
            <ShieldCheck className="mt-0.5 h-6 w-6 text-green" />
            <div><h2 className="text-xl font-semibold tracking-tight">Deploy from Marketplace</h2><p className="mt-1 text-sm text-secondary">DevHub retrieves and snapshots the Marketplace policy server-side. Node approval and tenant identity are never editable here.</p></div>
          </div>
          <div className="mt-6 grid gap-4 sm:grid-cols-2">
            <Field label="Marketplace order ID"><Input value={orderId} maxLength={256} required placeholder="order_…" onChange={(e) => setOrderId(e.target.value)} /></Field>
            <Field label="Project name"><Input value={projectName} maxLength={128} required={!!project} disabled={!!project} placeholder="my-project" onChange={(e) => setProjectName(e.target.value)} /></Field>
            <Field label="Repository URL" wide><Input value={repoUrl} maxLength={2048} required placeholder="https://github.com/owner/repo.git" onChange={(e) => setRepoUrl(e.target.value)} /></Field>
            <Field label="Branch"><Input value={branch} maxLength={256} placeholder="main" onChange={(e) => setBranch(e.target.value)} /></Field>
            <Field label="Root directory"><Input value={rootDir} maxLength={512} placeholder="apps/web (optional)" onChange={(e) => setRootDir(e.target.value)} /></Field>
            <Field label="Target"><select value={target} onChange={(e) => setTarget(e.target.value as Target)} className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm"><option value="production">Production</option><option value="preview">Preview</option></select></Field>
            <label className="flex items-center gap-2 self-end pb-2 text-sm text-secondary"><input type="checkbox" checked={useCache} onChange={(e) => setUseCache(e.target.checked)} /> Use build cache</label>
          </div>
          <div className="mt-5 rounded-lg border border-border p-3">
            <div className="mb-2 text-sm font-medium">Environment variables</div>
            <p className="mb-3 text-xs text-muted">Values follow DevHub&apos;s existing secret-handling path; Marketplace credentials and tokens are never accepted.</p>
            <div className="space-y-2">{rows.map((row, index) => <div key={index} className="flex gap-2"><Input className="font-mono text-xs" placeholder="KEY" value={row.key} onChange={(e) => setRows((all) => all.map((item, i) => i === index ? { ...item, key: e.target.value } : item))} /><Input className="font-mono text-xs" placeholder="value" value={row.value} onChange={(e) => setRows((all) => all.map((item, i) => i === index ? { ...item, value: e.target.value } : item))} /><button type="button" aria-label="Remove variable" className="text-muted hover:text-fg" onClick={() => setRows((all) => all.length === 1 ? [{ key: "", value: "" }] : all.filter((_, i) => i !== index))}><Trash2 className="h-4 w-4" /></button></div>)}</div>
            <Button variant="outline" className="mt-3" onClick={() => setRows((all) => [...all, { key: "", value: "" }])}><Plus className="h-3.5 w-3.5" /> Add variable</Button>
          </div>
          {error && <p className="mt-4 rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-400">{error}</p>}
        </div>
        <div className="flex justify-end gap-3 border-t border-border px-6 py-4 sm:px-7"><Button variant="outline" onClick={onClose} disabled={busy}>Cancel</Button><Button onClick={submit} disabled={busy || !orderId.trim() || !repoUrl.trim() || (!project && !projectName.trim())}>{busy ? <><Loader2 className="h-4 w-4 animate-spin" /> Submitting…</> : "Submit Marketplace deployment"}</Button></div>
      </div>
    </div>,
    document.body,
  );
}

function Field({ label, children, wide }: { label: string; children: React.ReactNode; wide?: boolean }) {
  return <label className={wide ? "sm:col-span-2" : ""}><span className="mb-1.5 block text-sm text-secondary">{label}</span>{children}</label>;
}
