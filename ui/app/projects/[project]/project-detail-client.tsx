"use client";

import { Suspense, useEffect, useState, use } from "react";
import Link from "next/link";
import Image from "next/image";
import { useRouter, useSearchParams } from "next/navigation";
import {
  Github,
  RotateCcw,
  RefreshCw,
  Plus,
  Trash2,
  ExternalLink,
  Code2,
  Terminal,
  Check,
  ChevronRight,
  Activity,
  Loader2,
  CircleX,
} from "lucide-react";
import dynamic from "next/dynamic";
import { Card, Button, Badge, Triangle, Table, Th, Td } from "@/components/ui";
import { WfConsoleFrame } from "@/components/wf-console-frame";
import { DeploymentResources } from "@/components/deployment-resources";
import { DeploymentRowMenu } from "@/components/deployment-menu";
import { RedeployModal } from "@/components/redeploy-modal";
import { CreateDeploymentModal } from "@/components/create-deployment-modal";
import { AttachedDomains } from "@/components/attached-domains";

const ServiceGraph = dynamic(
  () => import("@/components/service-graph").then((m) => m.ServiceGraph),
  {
    ssr: false,
    loading: () => (
      <div className="flex h-[600px] items-center justify-center rounded-xl border border-border bg-bg">
        <Loader2 className="h-5 w-5 animate-spin text-muted" />
      </div>
    ),
  }
);
import { apiGet, apiSend, usePoll, currentTeam, type Deployment, type Metrics, type Overview } from "@/lib/api";
import { usePendingBuilds, removePendingBuild, mergePending, addPendingBuild } from "@/lib/pending-builds";
import { SUMMARY_PATH, type ProjectSummary } from "@/lib/projects";
import { timeAgo } from "@/lib/utils";
import { deploymentUrl, deploymentHost, openDeployment, zkEnabled, deploymentSelfAlias } from "@/lib/deploy-url";
import { RawPortConnections } from "@/components/raw-port-connections";

const PROJECT_METRICS_WINDOW_MINUTES = 180;

export function ProjectDetail({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  return (
    <Suspense fallback={null}>
      <ProjectDetailInner params={params} />
    </Suspense>
  );
}

function ProjectDetailInner({ params }: { params: { project: string } }) {
  const name = decodeURIComponent(params.project);
  const router = useRouter();
  const searchParams = useSearchParams();
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  const { data: ov } = usePoll<Overview>("/v1/overview", 4000);
  const { data: summaryRows, refresh: refreshSummary } = usePoll<ProjectSummary[]>(SUMMARY_PATH, 5000);
  const summary = (summaryRows ?? []).find((row) => row.project === name) ?? null;
  const { data: pm } = usePoll<Metrics>(`/v1/metrics?minutes=${PROJECT_METRICS_WINDOW_MINUTES}&project=${encodeURIComponent(name)}`, 8000);
  const tabParam = searchParams.get("tab");
  const tab: "overview" | "graph" | "workflows" | "resources" | "deployments" =
    tabParam === "graph" || tabParam === "deployments" || tabParam === "workflows" || tabParam === "resources"
      ? tabParam
      : "overview";
  const setTab = (t: "overview" | "graph" | "workflows" | "resources" | "deployments") =>
    router.replace(`/projects/${encodeURIComponent(name)}?tab=${t}`, { scroll: false });
  const [busy, setBusy] = useState("");
  const [redeployFor, setRedeployFor] = useState<Deployment | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [retryError, setRetryError] = useState("");
  const rawPending = usePendingBuilds({ project: name });

  const mine = (deps ?? []).filter((d) => d.project === name);
  const pendingRows = mergePending(rawPending, mine);

  const pendingCount = pendingRows.length;
  useEffect(() => {
    refresh();
    refreshSummary();
  }, [pendingCount, refresh, refreshSummary]);
  const prod = mine.find((d) => d.production) ?? mine[0];
  const prodAlias = prod ? (deploymentSelfAlias(prod) || prod.alias) : undefined;
  const rollbackTarget = mine.find((d) => !d.production);
  const hasNoDeployment = deps !== null && mine.length === 0;
  const lastBuild = summary?.last_build ?? null;
  const building = summary?.building ?? null;
  const retrySource = lastBuild?.repo_url && !lastBuild.repo_url.startsWith("upload://") && !lastBuild.repo_url.startsWith("image://")
    ? lastBuild.repo_url
    : summary?.repo_url && !summary.repo_url.startsWith("upload://") && !summary.repo_url.startsWith("image://")
      ? summary.repo_url
      : "";

  async function promote(id: string) {
    setBusy(id);
    try { await apiSend("POST", `/v1/deployments/${id}/promote`); await refresh(); }
    catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  async function removeDeployment(id: string) {
    if (!confirm(`Delete deployment ${id}?`)) return;
    setBusy(id);
    try { await apiSend("DELETE", `/v1/deployments/${id}`); await refresh(); }
    catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  /** Cancels an IN-FLIGHT build by its BUILD id (the key `/deploy/:id` and `/v1/builds/:id` use). A real deployment row's failure is
   *  surfaced and `refresh()` re-reads the server state; an optimistic localStorage row may not be server-registered yet (a 404 is
   *  expected), and its local entry is dropped either way so a "Building" row cannot outlive its cancel. */
  async function cancelBuild(id: string, optimistic: boolean) {
    if (!confirm(`Cancel the in-progress build ${id}?`)) return;
    setBusy(id);
    try {
      await apiSend("POST", `/v1/builds/${encodeURIComponent(id)}/cancel`);
    } catch (e) {
      if (!optimistic) { alert(String(e)); setBusy(""); return; }
    }
    if (optimistic) removePendingBuild(id);
    try { await refresh(); } finally { setBusy(""); }
  }
  async function deleteProject() {
    if (!confirm(`Delete the entire "${name}" project and ALL its deployments? This cannot be undone.`)) return;
    setBusy("project");
    try { await apiSend("DELETE", `/v1/projects/${encodeURIComponent(name)}`); router.push("/projects"); }
    catch (e) { alert(String(e)); setBusy(""); }
  }
  /** Rebuild a project that has no deployment from its last build's source
   *  (the backend's redeploy falls back to that source for exactly this case). */
  async function retryBuild() {
    setBusy("retry");
    setRetryError("");
    try {
      const r = await apiSend<{ build_id: string }>("POST", `/v1/projects/${encodeURIComponent(name)}/redeploy`, { use_cache: false, target: "production" });
      addPendingBuild({ id: r.build_id, project: name, team: currentTeam(), env: "production" });
      router.push(`/deploy/${encodeURIComponent(r.build_id)}`);
    } catch (e) {
      setRetryError(String(e));
      setBusy("");
    }
  }

  const checklist = [
    { label: "Connect Git Repository", done: !!prod?.git || !!retrySource },
    { label: "Add Custom Domain", done: false },
    { label: "Preview Deployment", done: mine.length > 0 },
    { label: "Enable Web Analytics", done: true },
    { label: "Enable Speed Insights", done: false },
    { label: "Enable Firewall", done: ov?.waf_managed ?? false },
  ];
  const doneCount = checklist.filter((c) => c.done).length;
  const headerDot = prod
    ? prod.state === "ready" ? "bg-green" : prod.state === "error" ? "bg-red-400" : "bg-amber-400"
    : building ? "bg-amber-400" : lastBuild?.state === "error" ? "bg-red-400" : "bg-muted";

  return (
    <div>
      <div className="mb-6 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <Triangle />
          <div>
            <div className="flex items-center gap-2">
              <span className={`h-2 w-2 rounded-full ${headerDot}`} />
              <h1 className="text-xl font-semibold">{name}</h1>
            </div>
            {prod && (
              <a className="text-sm text-link hover:underline" href={deploymentUrl(prodAlias, prod.region_code)} target="_blank" rel="noreferrer">
                {deploymentHost(prodAlias, prod.region_code)} <ExternalLink className="inline h-3 w-3" />
              </a>
            )}
            {!prod && hasNoDeployment && (
              <div className="text-sm text-muted">
                {building ? "First build running…" : lastBuild ? `Last build ${lastBuild.state} — no deployment yet` : "No deployments yet"}
              </div>
            )}
          </div>
        </div>
        <div className="flex gap-2">
          {hasNoDeployment ? (
            <Button variant="outline" onClick={retryBuild} disabled={!retrySource || !!busy || !!building}>
              {busy === "retry" ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />} Retry build
            </Button>
          ) : (
            <Button variant="outline" onClick={() => prod && setRedeployFor(prod)} disabled={!prod?.git}>
              {busy === "redeploy" ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />} Redeploy
            </Button>
          )}
          <Button variant="outline" onClick={() => rollbackTarget && promote(rollbackTarget.id)} disabled={!rollbackTarget || !!busy}>
            <RotateCcw className="h-4 w-4" /> Instant Rollback
          </Button>
          <Button variant="danger" onClick={deleteProject} disabled={busy === "project"}>
            {busy === "project" ? <Loader2 className="h-4 w-4 animate-spin" /> : <Trash2 className="h-4 w-4" />} Delete
          </Button>
          {prod && (
            <a
              href={deploymentUrl(prodAlias, prod?.region_code)}
              target="_blank"
              rel="noreferrer"
              onClick={(e) => {
                if (zkEnabled && prod) {
                  e.preventDefault();
                  openDeployment(prodAlias || prod.alias, name, prod.region_code);
                }
              }}
            >
              <Button>Visit</Button>
            </a>
          )}
        </div>
      </div>

      <div className="mb-6" />

      {tab === "graph" ? (
        <ServiceGraph project={name} prod={prod} />
      ) : tab === "workflows" ? (
        <WfConsoleFrame project={name} />
      ) : tab === "resources" ? (
        <DeploymentResources deploymentId={prod?.id} />
      ) : tab === "overview" ? (
        <>
          {hasNoDeployment ? (
            <EmptyProjectCard
              name={name}
              summary={summary}
              retrySource={retrySource}
              busy={busy}
              retryError={retryError}
              onRetry={retryBuild}
              onDelete={deleteProject}
            />
          ) : (
          <Card className="mb-6 p-0">
            <div className="flex items-center justify-between border-b border-border px-5 py-3">
              <span className="font-medium">Production Deployment</span>
              <div className="flex gap-2">
                <Button variant="outline" onClick={() => prod && setRedeployFor(prod)} disabled={!prod?.git}>
                  <RefreshCw className="h-4 w-4" /> Redeploy
                </Button>
                <Button variant="outline" onClick={() => rollbackTarget && promote(rollbackTarget.id)} disabled={!rollbackTarget || !!busy}>
                  <RotateCcw className="h-4 w-4" /> Instant Rollback
                </Button>
              </div>
            </div>
            <div className="grid grid-cols-1 gap-6 p-5 md:grid-cols-2">
              <DeploymentPreview project={name} prod={prod} />
              <div className="flex flex-col gap-4 text-sm">
                <div>
                  <div className="text-muted">Deployment</div>
                  <div className="font-mono">{prod?.id ?? "—"}-{name}.localhost</div>
                </div>
                <div>
                  <div className="text-muted">Domains</div>
                  <div className="flex flex-wrap items-center gap-2">
                    <a className="text-link hover:underline" href={deploymentUrl(prodAlias, prod?.region_code)} target="_blank" rel="noreferrer">
                      {prod ? deploymentHost(prodAlias, prod.region_code) : "—"} <ExternalLink className="inline h-3 w-3" />
                    </a>
                    <AttachedDomains project={name} />
                  </div>
                </div>
                {prod && prod.raw_ports && prod.raw_ports.length > 0 && (
                  <div>
                    <div className="text-muted">Raw Connections</div>
                    <RawPortConnections deployment={prod} />
                  </div>
                )}
                <div className="flex gap-12">
                  <div>
                    <div className="text-muted">Status</div>
                    <div className="flex items-center gap-1.5">
                      <span className={`h-2 w-2 rounded-full ${prod?.state === "ready" ? "bg-green" : prod?.state === "error" ? "bg-red-400" : "bg-amber-400"}`} />
                      {prod ? prod.state.charAt(0).toUpperCase() + prod.state.slice(1) : "—"}
                    </div>
                  </div>
                  <div>
                    <div className="text-muted">Created</div>
                    <div>{prod ? `${timeAgo(prod.created_at_ms)} by ${prod.creator}` : "—"}</div>
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
          )}

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
              <Metric label="Requests" value={pm?.totals?.requests ?? 0} />
              <Metric label="Errors" value={pm?.totals?.errors ?? 0} />
              <Metric label="Blocked (firewall)" value={pm?.totals?.blocked ?? 0} />
              <Metric label="Cache hit ratio" value={`${Math.round((pm?.totals?.cache_hit_ratio ?? 0) * 100)}%`} />
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
        <div className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <span className="text-sm text-secondary">{mine.length + pendingRows.length} deployment{mine.length + pendingRows.length === 1 ? "" : "s"}</span>
          <div className="flex gap-2">
            <Button onClick={() => setCreateOpen(true)}><Plus className="h-4 w-4" /> New Deployment</Button>
          </div>
        </div>
        <Table>
          <thead>
            <tr><Th className="px-2">Deployment</Th><Th className="px-2">Status</Th><Th className="px-2">Environment</Th><Th className="px-2 hidden md:table-cell">Source</Th><Th className="px-2 hidden sm:table-cell">Created</Th><Th className="px-2 hidden lg:table-cell">By</Th><Th className="px-2"></Th></tr>
          </thead>
          <tbody>
            {pendingRows.map((p) => (
              <tr key={p.id} className="animate-pulse">
                <Td className="px-2 font-mono text-xs">
                  <a className="text-link hover:underline" href={`/deploy/${p.id}`}>{p.id}</a>
                </Td>
                <Td className="px-2">
                  <span className="inline-flex items-center gap-1.5">
                    <Loader2 className="h-3.5 w-3.5 animate-spin text-amber-400" /> building
                  </span>
                </Td>
                <Td className="px-2">{p.env === "production" ? <Badge>Production</Badge> : <Badge tone="blue">Preview</Badge>}</Td>
                <Td className="px-2 font-mono text-xs text-secondary hidden md:table-cell">
                  <span className="inline-flex items-center gap-1"><RefreshCw className="h-3.5 w-3.5" /> redeploy</span>
                </Td>
                <Td className="px-2 text-secondary hidden sm:table-cell">just now</Td>
                <Td className="px-2 text-secondary hidden lg:table-cell">you</Td>
                <Td className="px-2">
                  <DeploymentRowMenu
                    canRollback={false}
                    canRedeploy={false}
                    canCancel
                    busy={busy === p.id}
                    onRollback={() => {}}
                    onRedeploy={() => {}}
                    onCancel={() => cancelBuild(p.id, true)}
                  />
                </Td>
              </tr>
            ))}
            {mine.map((d) => (
              <tr
                key={d.id}
                onClick={() => router.push(`/deployments/${encodeURIComponent(d.id)}`)}
                className="cursor-pointer transition-colors hover:bg-subtle/50"
                title="View deployment details"
              >
                <Td className="px-2 font-mono text-xs">
                  <a className="text-link hover:underline" href={deploymentUrl(d.production ? d.alias : (d.commit_alias || d.branch_alias || d.id_alias || d.alias), d.region_code)} target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()}>{d.id}</a>
                </Td>
                <Td className="px-2">
                  <span className="inline-flex items-center gap-1.5">
                    <span className={`h-2 w-2 rounded-full ${d.state === "ready" ? "bg-green" : d.state === "building" ? "bg-amber-400" : "bg-red-400"}`} />
                    {d.state}
                  </span>
                </Td>
                <Td className="px-2">
                  {(() => {
                    const env = d.target || (d.production ? "production" : "preview");
                    return (
                      <span className="inline-flex items-center gap-1.5">
                        {env === "production" ? <Badge>Production</Badge> : <Badge tone="blue">Preview</Badge>}
                        {d.production && <span title="Currently promoted to production" className="h-1.5 w-1.5 rounded-full bg-green" />}
                      </span>
                    );
                  })()}
                </Td>
                <Td className="px-2 font-mono text-xs text-secondary hidden md:table-cell">
                  {d.git ? (
                    <span className="inline-flex items-center gap-1"><Code2 className="h-3.5 w-3.5" /> {d.git.branch} {d.git.commit}</span>
                  ) : (
                    <span className="inline-flex items-center gap-1"><Terminal className="h-3.5 w-3.5" /> hive deploy</span>
                  )}
                </Td>
                <Td className="px-2 text-secondary hidden sm:table-cell">{timeAgo(d.created_at_ms)}</Td>
                <Td className="px-2 text-secondary hidden lg:table-cell">{d.creator}</Td>
                <Td className="px-2">
                  <div onClick={(e) => e.stopPropagation()}>
                    <DeploymentRowMenu
                      canRollback={!d.production}
                      canRedeploy={!!d.git}
                      canCancel={d.state === "building" || d.state === "queued"}
                      busy={busy === d.id}
                      onRollback={() => promote(d.id)}
                      onRedeploy={() => setRedeployFor(d)}
                      onCancel={() => cancelBuild(d.id, false)}
                      onDelete={() => removeDeployment(d.id)}
                    />
                  </div>
                </Td>
              </tr>
            ))}
            {!mine.length && !pendingRows.length && (
              <tr>
                <Td className="text-secondary">
                  {lastBuild
                    ? <>No deployments. The last build (<a className="text-link hover:underline" href={`/deploy/${encodeURIComponent(lastBuild.id)}`}>{lastBuild.id}</a>) ended {lastBuild.state}{lastBuild.error ? `: ${lastBuild.error.split("\n")[0]}` : ""}.</>
                    : "No deployments."}
                </Td>
              </tr>
            )}
          </tbody>
        </Table>
        </div>
      )}

      {redeployFor && (
        <RedeployModal
          deployment={redeployFor}
          prodAlias={prod?.alias || `${name}.localhost`}
          onClose={() => setRedeployFor(null)}
          onDone={() => {
            setTab("deployments");
            refresh();
          }}
        />
      )}
      {createOpen && (
        <CreateDeploymentModal
          project={name}
          repoUrl={prod?.git?.repo_url ?? retrySource}
          branch={prod?.git?.branch ?? lastBuild?.branch}
          onClose={() => setCreateOpen(false)}
          onDone={() => {
            setTab("deployments");
            refresh();
          }}
        />
      )}
    </div>
  );
}

/** The overview card for a project that has no deployment: why (the last
 *  build's outcome and error), and the two things the user can do about it. */
function EmptyProjectCard({ name, summary, retrySource, busy, retryError, onRetry, onDelete }: {
  name: string;
  summary: ProjectSummary | null;
  retrySource: string;
  busy: string;
  retryError: string;
  onRetry: () => void;
  onDelete: () => void;
}) {
  const last = summary?.last_build ?? null;
  const building = summary?.building ?? null;
  const settings = `/projects/${encodeURIComponent(name)}/settings`;
  return (
    <Card className="mb-6 p-0">
      <div className="flex items-center justify-between border-b border-border px-5 py-3">
        <span className="font-medium">No deployment yet</span>
        <div className="flex gap-2">
          <Button variant="outline" onClick={onRetry} disabled={!retrySource || !!busy || !!building}>
            {busy === "retry" ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />} Retry build
          </Button>
          <Button variant="danger" onClick={onDelete} disabled={busy === "project"}>
            <Trash2 className="h-4 w-4" /> Delete project
          </Button>
        </div>
      </div>
      <div className="flex flex-col gap-4 p-5 text-sm">
        {building ? (
          <div className="flex items-center gap-2">
            <Loader2 className="h-4 w-4 animate-spin text-amber-400" />
            First build <a className="font-mono text-link hover:underline" href={`/deploy/${encodeURIComponent(building.id)}`}>{building.id}</a> running since {timeAgo(building.started_ms)}.
          </div>
        ) : last ? (
          <>
            <div className="flex items-center gap-2">
              <CircleX className={`h-4 w-4 ${last.state === "error" ? "text-red-500" : "text-muted"}`} />
              Build <a className="font-mono text-link hover:underline" href={`/deploy/${encodeURIComponent(last.id)}`}>{last.id}</a> ended <span className="font-medium">{last.state}</span> {timeAgo(last.finished_ms || last.started_ms)}
              {last.state === "ready" && " without registering a deployment"}
            </div>
            {last.error && (
              <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded-md border border-border bg-subtle/50 p-3 font-mono text-xs text-red-500">{last.error}</pre>
            )}
            <div className="text-xs text-muted">
              Build logs may have expired on the node that ran them; the error above is kept with the project.
            </div>
          </>
        ) : summary ? (
          <div className="text-secondary">This project exists but has never built. Import its repository again under this name, or delete it.</div>
        ) : (
          <div className="flex items-center gap-2 text-secondary"><Loader2 className="h-4 w-4 animate-spin" /> Looking up the project…</div>
        )}
        {retryError && <div className="text-red-500">{retryError}</div>}
        <div className="flex flex-wrap gap-4 text-xs">
          {retrySource ? (
            <span className="inline-flex items-center gap-1 text-secondary"><Github className="h-3.5 w-3.5" /> {retrySource.replace(/^https?:\/\/(www\.)?github\.com\//, "").replace(/\.git$/, "")}{last?.branch ? ` @ ${last.branch}` : ""}{summary?.root_dir ? ` · ${summary.root_dir}` : ""}</span>
          ) : (
            <span className="text-secondary">No git source recorded — retry by importing the repository again under this name.</span>
          )}
          <Link href={`${settings}/environment-variables`} className="text-link hover:underline">Environment variables</Link>
          <Link href={`${settings}/build`} className="text-link hover:underline">Build settings</Link>
          <Link href={`${settings}/git`} className="text-link hover:underline">Git</Link>
        </div>
      </div>
    </Card>
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

interface Preview {
  kind: "image" | "json" | "text" | "none";
  url?: string;
  body?: string;
  status?: number;
  content_type?: string;
  alias?: string;
}

/** Production deployment preview: a live screenshot of the site for frontends,
 *  or the JSON/text response for backend services. */
function DeploymentPreview({ project, prod }: { project: string; prod: Deployment | undefined }) {
  const [pv, setPv] = useState<Preview | null>(null);
  const [imgError, setImgError] = useState(false);

  const identity = `${project}:${prod?.id ?? ""}`;
  const [prevIdentity, setPrevIdentity] = useState(identity);
  if (identity !== prevIdentity) {
    setPrevIdentity(identity);
    setImgError(false);
    setPv(null);
  }

  useEffect(() => {
    let stop = false;
    if (!prod) return;
    apiGet<Preview>(`/v1/projects/${encodeURIComponent(project)}/preview`)
      .then((d) => { if (!stop) setPv(d); })
      .catch(() => { if (!stop) setPv({ kind: "none" }); });
    return () => { stop = true; };
  }, [project, prod]);

  const box = "relative h-56 overflow-hidden rounded-lg border border-border";

  if (!prod || !pv) {
    return (
      <div className={`${box} flex items-center justify-center bg-gradient-to-br from-orange-100/60 to-card dark:from-orange-500/10 dark:to-card`}>
        <Loader2 className="h-5 w-5 animate-spin text-muted" />
      </div>
    );
  }

  if (pv.kind === "image" && pv.url && !imgError) {
    return (
      <a href={deploymentUrl(deploymentSelfAlias(prod) || prod.alias, prod.region_code)} target="_blank" rel="noreferrer" className={`${box} block bg-bg group`}>
        <Image
          src={`/cloud${pv.url}`}
          alt={`${project} preview`}
          fill
          sizes="(max-width: 640px) 100vw, 400px"
          className="object-cover object-top transition-transform group-hover:scale-[1.02]"
          onError={() => setImgError(true)}
        />
        <span className="absolute bottom-2 left-2 rounded-md border border-border bg-card/90 px-2 py-0.5 text-[11px] text-secondary backdrop-blur">
          {pv.alias}
        </span>
      </a>
    );
  }

  if (pv.kind === "json" || pv.kind === "text") {
    let body = pv.body ?? "";
    if (pv.kind === "json") {
      try { body = JSON.stringify(JSON.parse(body), null, 2); } catch {}
    }
    return (
      <div className={`${box} bg-[#0b0b0b]`}>
        <div className="flex items-center justify-between border-b border-border bg-card/40 px-3 py-1.5 text-[11px]">
          <span className="flex items-center gap-1.5 text-secondary"><Terminal className="h-3 w-3" /> Service response</span>
          <span className="font-mono text-muted">{pv.content_type?.split(";")[0] || (pv.kind === "json" ? "application/json" : "text/plain")}{pv.status ? ` · ${pv.status}` : ""}</span>
        </div>
        <pre className="no-scrollbar h-[calc(100%-30px)] overflow-auto p-3 font-mono text-[11px] leading-relaxed text-emerald-300/90">{body || "(empty response)"}</pre>
      </div>
    );
  }

  return (
    <div className={`${box} flex items-center justify-center bg-gradient-to-br from-orange-100/60 to-card dark:from-orange-500/10 dark:to-card`}>
      <Triangle className="h-12 w-12" />
    </div>
  );
}
