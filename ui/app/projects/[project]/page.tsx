"use client";

import { useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import {
  Github,
  RotateCcw,
  RefreshCw,
  Trash2,
  ExternalLink,
  Code2,
  Terminal,
  Check,
  ChevronRight,
  Activity,
  Loader2,
} from "lucide-react";
import { Card, Button, Badge, Triangle, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type Deployment, type Overview } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function ProjectDetail({ params }: { params: { project: string } }) {
  const name = decodeURIComponent(params.project);
  const router = useRouter();
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  const { data: ov } = usePoll<Overview>("/v1/overview", 4000);
  const [tab, setTab] = useState<"overview" | "deployments">("overview");
  const [busy, setBusy] = useState("");

  const mine = (deps ?? []).filter((d) => d.project === name);
  const prod = mine.find((d) => d.production) ?? mine[0];
  const rollbackTarget = mine.find((d) => !d.production); // newest non-prod build

  async function promote(id: string) {
    setBusy(id);
    try { await apiSend("POST", `/v1/deployments/${id}/promote`); await refresh(); }
    catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  async function redeploy() {
    setBusy("redeploy");
    try {
      const r = await apiSend<{ build_id: string }>("POST", `/v1/projects/${encodeURIComponent(name)}/redeploy`);
      router.push(`/deploy/${r.build_id}`);
    } catch (e) { alert(String(e)); setBusy(""); }
  }
  async function removeDeployment(id: string) {
    if (!confirm(`Delete deployment ${id}?`)) return;
    setBusy(id);
    try { await apiSend("DELETE", `/v1/deployments/${id}`); await refresh(); }
    catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  async function deleteProject() {
    if (!confirm(`Delete the entire "${name}" project and ALL its deployments? This cannot be undone.`)) return;
    setBusy("project");
    try { await apiSend("DELETE", `/v1/projects/${encodeURIComponent(name)}`); router.push("/projects"); }
    catch (e) { alert(String(e)); setBusy(""); }
  }

  const checklist = [
    { label: "Connect Git Repository", done: !!prod?.git },
    { label: "Add Custom Domain", done: false },
    { label: "Preview Deployment", done: mine.length > 0 },
    { label: "Enable Web Analytics", done: true },
    { label: "Enable Speed Insights", done: false },
    { label: "Enable Firewall", done: ov?.waf_managed ?? false },
  ];
  const doneCount = checklist.filter((c) => c.done).length;

  return (
    <div>
      {/* Project header */}
      <div className="mb-6 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <Triangle />
          <div>
            <div className="flex items-center gap-2">
              <span className="h-2 w-2 rounded-full bg-green" />
              <h1 className="text-xl font-semibold">{name}</h1>
            </div>
            {prod && (
              <a className="text-sm text-link hover:underline" href={`http://${prod.alias}:8787/`}>
                {prod.alias} <ExternalLink className="inline h-3 w-3" />
              </a>
            )}
          </div>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" onClick={redeploy} disabled={busy === "redeploy" || !prod?.git}>
            {busy === "redeploy" ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />} Redeploy
          </Button>
          <Button variant="outline" onClick={() => rollbackTarget && promote(rollbackTarget.id)} disabled={!rollbackTarget || !!busy}>
            <RotateCcw className="h-4 w-4" /> Instant Rollback
          </Button>
          <Button variant="danger" onClick={deleteProject} disabled={busy === "project"}>
            {busy === "project" ? <Loader2 className="h-4 w-4 animate-spin" /> : <Trash2 className="h-4 w-4" />} Delete
          </Button>
          <a href={prod ? `http://${prod.alias}:8787/` : "#"} target="_blank" rel="noreferrer">
            <Button>Visit</Button>
          </a>
        </div>
      </div>

      {/* sub tabs */}
      <div className="mb-6 flex items-center gap-1 border-b border-border">
        {(["overview", "deployments"] as const).map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`relative px-3 py-2 text-sm capitalize ${tab === t ? "text-fg" : "text-secondary hover:text-fg"}`}
          >
            {t}
            {tab === t && <span className="absolute inset-x-2 -bottom-px h-0.5 rounded-full bg-fg" />}
          </button>
        ))}
        <Link
          href={`/projects/${encodeURIComponent(name)}/logs`}
          className="relative px-3 py-2 text-sm text-secondary hover:text-fg"
        >
          Logs
        </Link>
        <Link
          href={`/projects/${encodeURIComponent(name)}/settings`}
          className="relative px-3 py-2 text-sm text-secondary hover:text-fg"
        >
          Settings
        </Link>
      </div>

      {tab === "overview" ? (
        <>
          {/* Production Deployment */}
          <Card className="mb-6 p-0">
            <div className="flex items-center justify-between border-b border-border px-5 py-3">
              <span className="font-medium">Production Deployment</span>
              <div className="flex gap-2">
                <Button variant="outline" onClick={redeploy} disabled={busy === "redeploy" || !prod?.git}>
                  {busy === "redeploy" ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />} Redeploy
                </Button>
                <Button variant="outline" onClick={() => rollbackTarget && promote(rollbackTarget.id)} disabled={!rollbackTarget || !!busy}>
                  <RotateCcw className="h-4 w-4" /> Instant Rollback
                </Button>
              </div>
            </div>
            <div className="grid grid-cols-1 gap-6 p-5 md:grid-cols-2">
              <div className="flex h-56 items-center justify-center rounded-lg border border-border bg-gradient-to-br from-orange-100/60 to-card dark:from-orange-500/10 dark:to-card">
                <Triangle className="h-12 w-12" />
              </div>
              <div className="flex flex-col gap-4 text-sm">
                <div>
                  <div className="text-muted">Deployment</div>
                  <div className="font-mono">{prod?.id ?? "—"}-{name}.localhost</div>
                </div>
                <div>
                  <div className="text-muted">Domains</div>
                  <a className="text-link hover:underline" href={prod ? `http://${prod.alias}:8787/` : "#"}>
                    {prod?.alias ?? "—"} <ExternalLink className="inline h-3 w-3" />
                  </a>
                </div>
                <div className="flex gap-12">
                  <div>
                    <div className="text-muted">Status</div>
                    <div className="flex items-center gap-1.5">
                      <span className="h-2 w-2 rounded-full bg-green" />
                      {prod ? prod.state.charAt(0).toUpperCase() + prod.state.slice(1) : "—"}
                    </div>
                  </div>
                  <div>
                    <div className="text-muted">Created</div>
                    <div>{prod ? `${timeAgo(prod.created_at_ms)} ago by ${prod.creator}` : "—"}</div>
                  </div>
                </div>
                <div>
                  <div className="text-muted">Source</div>
                  {prod?.git ? (
                    <div className="font-mono text-xs">
                      <div className="flex items-center gap-1"><Github className="h-3.5 w-3.5" /> {prod.git.branch}</div>
                      <div className="text-secondary">{prod.git.commit} {prod.git.commit_message}</div>
                    </div>
                  ) : (
                    <div className="flex items-center gap-1.5 font-mono text-xs"><Terminal className="h-3.5 w-3.5" /> hive deploy</div>
                  )}
                </div>
              </div>
            </div>
          </Card>

          {/* Lower cards */}
          <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
            <Card className="p-5">
              <div className="mb-4 flex items-center justify-between">
                <span className="font-medium">Production Checklist</span>
                <Badge>{doneCount}/{checklist.length}</Badge>
              </div>
              <div className="flex flex-col gap-2">
                {checklist.map((c) => (
                  <div key={c.label} className="flex items-center gap-2.5 rounded-lg border border-border px-3 py-2 text-sm">
                    {c.done ? (
                      <Check className="h-4 w-4 text-green" />
                    ) : (
                      <span className="h-4 w-4 rounded-full border border-border-strong" />
                    )}
                    <span className={c.done ? "text-secondary line-through" : ""}>{c.label}</span>
                  </div>
                ))}
              </div>
            </Card>

            <Card className="p-5">
              <div className="mb-4 flex items-center justify-between">
                <span className="font-medium">Observability</span>
                <ChevronRight className="h-4 w-4 text-muted" />
              </div>
              <Metric label="Requests" value={ov?.requests ?? 0} />
              <Metric label="Function Invocations" value={ov?.instances ? (ov.requests) : 0} />
              <Metric label="Blocked (firewall)" value={ov?.blocked ?? 0} />
              <Metric label="Cache hit ratio" value={`${Math.round((ov?.cdn.hit_ratio ?? 0) * 100)}%`} />
            </Card>

            <Card className="flex flex-col items-center justify-center gap-3 p-5 text-center">
              <Activity className="h-8 w-8 text-muted" />
              <div className="font-medium">Analytics</div>
              <p className="text-sm text-secondary">Track visitors and page views across regions.</p>
              <Button variant="outline">Enable Analytics</Button>
            </Card>
          </div>
        </>
      ) : (
        <Table>
          <thead>
            <tr><Th>Deployment</Th><Th>Status</Th><Th>Environment</Th><Th>Source</Th><Th>Created</Th><Th>By</Th><Th></Th></tr>
          </thead>
          <tbody>
            {mine.map((d) => (
              <tr key={d.id}>
                <Td className="font-mono text-xs">{d.id}</Td>
                <Td>
                  <span className="inline-flex items-center gap-1.5">
                    <span className={`h-2 w-2 rounded-full ${d.state === "ready" ? "bg-green" : d.state === "building" ? "bg-amber-400" : "bg-red-400"}`} />
                    {d.state}
                  </span>
                </Td>
                <Td>{d.production ? <Badge>Production</Badge> : <Badge tone="blue">Preview</Badge>}</Td>
                <Td className="font-mono text-xs text-secondary">
                  {d.git ? (
                    <span className="inline-flex items-center gap-1"><Code2 className="h-3.5 w-3.5" /> {d.git.branch} {d.git.commit}</span>
                  ) : (
                    <span className="inline-flex items-center gap-1"><Terminal className="h-3.5 w-3.5" /> hive deploy</span>
                  )}
                </Td>
                <Td className="text-secondary">{timeAgo(d.created_at_ms)} ago</Td>
                <Td className="text-secondary">{d.creator}</Td>
                <Td>
                  <div className="flex items-center justify-end gap-1">
                    {busy === d.id ? (
                      <Loader2 className="h-4 w-4 animate-spin text-muted" />
                    ) : (
                      <>
                        {!d.production && (
                          <button title="Promote to production" onClick={() => promote(d.id)} className="flex h-7 w-7 items-center justify-center rounded-md text-secondary hover:bg-subtle hover:text-fg"><RotateCcw className="h-3.5 w-3.5" /></button>
                        )}
                        <button title="Delete deployment" onClick={() => removeDeployment(d.id)} className="flex h-7 w-7 items-center justify-center rounded-md text-secondary hover:bg-subtle hover:text-red-500"><Trash2 className="h-3.5 w-3.5" /></button>
                      </>
                    )}
                  </div>
                </Td>
              </tr>
            ))}
            {!mine.length && <tr><Td className="text-secondary">No deployments.</Td></tr>}
          </tbody>
        </Table>
      )}
    </div>
  );
}

function Metric({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="mb-3 flex items-center justify-between">
      <span className="text-sm text-secondary">{label}</span>
      <span className="font-semibold tabular-nums">{value}</span>
    </div>
  );
}
