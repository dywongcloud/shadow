"use client";

import { useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { RotateCcw, RefreshCw, Trash2, Loader2, Plus, CircleX } from "lucide-react";
import { Badge, Button, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, cancelBuild, usePoll, type Deployment } from "@/lib/api";
import { usePendingBuilds, removePendingBuild } from "@/lib/pending-builds";
import { timeAgo } from "@/lib/utils";
import { deploymentUrl, deploymentHost } from "@/lib/deploy-url";
import { RedeployModal } from "@/components/redeploy-modal";

export default function DeploymentsPage() {
  const router = useRouter();
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  // In-flight builds from the persistent store — survive navigation/reload.
  const pending = usePendingBuilds();
  const [busy, setBusy] = useState<string>("");
  // The deployment whose Redeploy modal is open (null = closed).
  const [redeployFor, setRedeployFor] = useState<Deployment | null>(null);

  async function promote(d: Deployment) {
    setBusy(d.id);
    try {
      await apiSend("POST", `/v1/deployments/${d.id}/promote`);
      await refresh();
    } catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  async function remove(d: Deployment) {
    if (!confirm(`Delete deployment ${d.id}? This cannot be undone.`)) return;
    setBusy(d.id);
    try {
      await apiSend("DELETE", `/v1/deployments/${d.id}`);
      await refresh();
    } catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  // Stops the ACTUAL server-side build process (git clone / npm install /
  // build command), not just a client-side no-op — see `POST
  // /v1/builds/:id/cancel`. `p.id` IS the build id (see `PendingBuild`'s doc).
  async function cancelPending(buildId: string) {
    setBusy(buildId);
    try {
      await cancelBuild(buildId);
      removePendingBuild(buildId); // now terminal — drop the pulsing row
    } catch (e) {
      alert(String(e)); // request itself failed — keep the row, let the user retry
    } finally {
      setBusy("");
    }
  }

  return (
    <div>
      <PageHeader
        title="Deployments"
        desc="Every deployment gets an immutable URL. Promote any build to production instantly (rollback), redeploy from git, or delete."
        action={<Link href="/new"><Button><Plus className="h-4 w-4" /> New Deployment</Button></Link>}
      />
      <Table>
        <thead><tr><Th>Project</Th><Th className="hidden lg:table-cell">Deployment</Th><Th>Status</Th><Th>URL</Th><Th className="hidden md:table-cell">Source</Th><Th className="hidden sm:table-cell">Created</Th><Th></Th></tr></thead>
        <tbody>
          {/* Persistent optimistic rows for in-flight builds (survive navigation). */}
          {pending.map((p) => {
            const cancelling = busy === p.id;
            return (
              <tr key={p.id} className="animate-pulse">
                <Td className="font-medium">{p.project}</Td>
                <Td className="font-mono text-xs text-muted hidden lg:table-cell">
                  <a className="text-link hover:underline" href={`/deploy/${p.id}`}>{p.id}</a>
                </Td>
                <Td>{p.env === "production" ? <Badge tone="green">Production</Badge> : <Badge>Preview</Badge>}</Td>
                <Td className="text-xs text-muted">
                  <span className="inline-flex items-center gap-1.5">
                    <Loader2 className="h-3.5 w-3.5 animate-spin text-amber-400" /> building
                  </span>
                </Td>
                <Td className="text-xs text-secondary hidden md:table-cell">
                  <span className="inline-flex items-center gap-1"><RefreshCw className="h-3.5 w-3.5" /> deploying</span>
                </Td>
                <Td className="text-muted hidden sm:table-cell">just now</Td>
                <Td>
                  <div className="flex items-center justify-end gap-1">
                    {cancelling ? (
                      <Loader2 className="h-4 w-4 animate-spin text-muted" />
                    ) : (
                      <IconBtn title="Cancel deployment" danger onClick={() => cancelPending(p.id)}>
                        <CircleX className="h-3.5 w-3.5" />
                      </IconBtn>
                    )}
                  </div>
                </Td>
              </tr>
            );
          })}
          {(deps ?? []).map((d) => {
            const working = busy === d.id;
            return (
              <tr
                key={d.id}
                onClick={() => router.push(`/deployments/${encodeURIComponent(d.id)}`)}
                className="cursor-pointer transition-colors hover:bg-subtle/50"
                title="View deployment details"
              >
                <Td className="font-medium">{d.project}</Td>
                <Td className="font-mono text-xs text-muted hidden lg:table-cell">{d.id}</Td>
                <Td>
                  {/* Environment is the immutable build target; the green dot marks
                      the deployment currently promoted to the production domain. */}
                  {(() => {
                    const env = d.target || (d.production ? "production" : "preview");
                    return (
                      <span className="inline-flex items-center gap-1.5">
                        {env === "production" ? <Badge tone="green">Production</Badge> : <Badge>Preview</Badge>}
                        {d.production && <span title="Currently promoted to production" className="h-1.5 w-1.5 rounded-full bg-green" />}
                      </span>
                    );
                  })()}
                </Td>
                {/* The row currently promoted to production links via the project
                    production alias (e.g. my-app.<domain>), so production opens at
                    its real production URL. Every other row links to its OWN
                    immutable URL (the per-deployment id, or commit URL), so a
                    preview opens that preview — not production. */}
                <Td className="font-mono text-xs">
                  {/* Prefer the SHARED, mesh-routable commit alias for previews; the
                      per-node id alias (dpl-<id>) only resolves on its own host node
                      under multi-region fanout, so it 404s through the pooled ingress. */}
                  {(() => { const self = d.production ? d.alias : (d.commit_alias || d.branch_alias || d.id_alias || d.alias);
                    return <a className="text-link hover:underline" href={deploymentUrl(self)} target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()}>{deploymentHost(self)}</a>; })()}
                </Td>
                <Td className="text-xs text-secondary hidden md:table-cell">
                  {d.git ? <span className="font-mono">{d.git.branch}@{d.git.commit || "—"}</span> : <span className="text-muted">CLI</span>}
                </Td>
                <Td className="text-muted hidden sm:table-cell">{timeAgo(d.created_at_ms)}</Td>
                <Td>
                  <div className="flex items-center justify-end gap-1" onClick={(e) => e.stopPropagation()}>
                    {working ? (
                      <Loader2 className="h-4 w-4 animate-spin text-muted" />
                    ) : (
                      <>
                        {!d.production && (
                          <IconBtn title="Promote to production (rollback)" onClick={() => promote(d)}><RotateCcw className="h-3.5 w-3.5" /></IconBtn>
                        )}
                        {d.git && (
                          <IconBtn title="Redeploy from git (new deployment)" onClick={() => setRedeployFor(d)}><RefreshCw className="h-3.5 w-3.5" /></IconBtn>
                        )}
                        <IconBtn title="Delete deployment" danger onClick={() => remove(d)}><Trash2 className="h-3.5 w-3.5" /></IconBtn>
                      </>
                    )}
                  </div>
                </Td>
              </tr>
            );
          })}
          {deps === null && !pending.length && <tr><Td className="text-muted">Loading deployments…</Td></tr>}
          {deps !== null && !deps.length && !pending.length && <tr><Td className="text-muted">No deployments yet — <a className="text-link hover:underline" href="/new">create one</a>.</Td></tr>}
        </tbody>
      </Table>

      {redeployFor && (
        <RedeployModal
          deployment={redeployFor}
          prodAlias={`${redeployFor.project}.localhost`}
          onClose={() => setRedeployFor(null)}
          onDone={() => refresh()}
        />
      )}
    </div>
  );
}

function IconBtn({ children, title, onClick, danger }: { children: React.ReactNode; title: string; onClick: () => void; danger?: boolean }) {
  return (
    <button
      title={title}
      onClick={onClick}
      className={`flex h-7 w-7 items-center justify-center rounded-md text-secondary transition-colors hover:bg-subtle ${danger ? "hover:text-red-500" : "hover:text-fg"}`}
    >
      {children}
    </button>
  );
}
