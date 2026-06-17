"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { RotateCcw, RefreshCw, Trash2, Loader2 } from "lucide-react";
import { Badge, Button, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type Deployment } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function DeploymentsPage() {
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  const router = useRouter();
  const [busy, setBusy] = useState<string>("");

  async function promote(d: Deployment) {
    setBusy(d.id);
    try {
      await apiSend("POST", `/v1/deployments/${d.id}/promote`);
      await refresh();
    } catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  async function redeploy(d: Deployment) {
    setBusy(d.id);
    try {
      const r = await apiSend<{ build_id: string }>("POST", `/v1/projects/${encodeURIComponent(d.project)}/redeploy`);
      router.push(`/deploy/${r.build_id}`);
    } catch (e) { alert(String(e)); setBusy(""); }
  }
  async function remove(d: Deployment) {
    if (!confirm(`Delete deployment ${d.id}? This cannot be undone.`)) return;
    setBusy(d.id);
    try {
      await apiSend("DELETE", `/v1/deployments/${d.id}`);
      await refresh();
    } catch (e) { alert(String(e)); } finally { setBusy(""); }
  }

  return (
    <div>
      <PageHeader
        title="Deployments"
        desc="Every deployment gets an immutable URL. Promote any build to production instantly (rollback), redeploy from git, or delete."
      />
      <Table>
        <thead><tr><Th>Project</Th><Th>Deployment</Th><Th>Status</Th><Th>URL</Th><Th>Source</Th><Th>Created</Th><Th></Th></tr></thead>
        <tbody>
          {(deps ?? []).map((d) => {
            const working = busy === d.id;
            return (
              <tr key={d.id}>
                <Td className="font-medium">{d.project}</Td>
                <Td className="font-mono text-xs text-muted">{d.id}</Td>
                <Td>
                  {d.production
                    ? <Badge tone="green"><span className="h-1.5 w-1.5 rounded-full bg-green" /> Production</Badge>
                    : <Badge>Preview</Badge>}
                </Td>
                <Td className="font-mono text-xs"><a className="text-link hover:underline" href={`http://${d.alias}:8787/`} target="_blank" rel="noreferrer">{d.alias}</a></Td>
                <Td className="text-xs text-secondary">
                  {d.git ? <span className="font-mono">{d.git.branch}@{d.git.commit || "—"}</span> : <span className="text-muted">CLI</span>}
                </Td>
                <Td className="text-muted">{timeAgo(d.created_at_ms)}</Td>
                <Td>
                  <div className="flex items-center justify-end gap-1">
                    {working ? (
                      <Loader2 className="h-4 w-4 animate-spin text-muted" />
                    ) : (
                      <>
                        {!d.production && (
                          <IconBtn title="Promote to production (rollback)" onClick={() => promote(d)}><RotateCcw className="h-3.5 w-3.5" /></IconBtn>
                        )}
                        {d.git && (
                          <IconBtn title="Redeploy from git (new deployment)" onClick={() => redeploy(d)}><RefreshCw className="h-3.5 w-3.5" /></IconBtn>
                        )}
                        <IconBtn title="Delete deployment" danger onClick={() => remove(d)}><Trash2 className="h-3.5 w-3.5" /></IconBtn>
                      </>
                    )}
                  </div>
                </Td>
              </tr>
            );
          })}
          {!deps?.length && <tr><Td className="text-muted">No deployments yet — <a className="text-link hover:underline" href="/new">create one</a>.</Td></tr>}
        </tbody>
      </Table>
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
