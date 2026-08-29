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
  ShieldCheck,
} from "lucide-react";
import dynamic from "next/dynamic";
import { Card, Button, Badge, Triangle, Table, Th, Td } from "@/components/ui";
import { WfConsoleFrame } from "@/components/wf-console-frame";
import { DeploymentResources } from "@/components/deployment-resources";
import { DeploymentRowMenu } from "@/components/deployment-menu";
import { RedeployModal } from "@/components/redeploy-modal";
import { CreateDeploymentModal } from "@/components/create-deployment-modal";
import { AttachedDomains } from "@/components/attached-domains";
import { MarketplaceDeploymentModal } from "@/components/marketplace-deployment-modal";
import { MarketplaceProjectResources } from "@/components/marketplace-project-resources";

// Lazy-load the React Flow service graph — it's a heavy client bundle, so it's
// only fetched when the Service Graph tab is actually opened.
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
import { apiGet, apiSend, usePoll, type Deployment, type Metrics, type Overview } from "@/lib/api";
import { usePendingBuilds, removePendingBuild, mergePending } from "@/lib/pending-builds";
import { timeAgo } from "@/lib/utils";
import { deploymentUrl, deploymentHost, openDeployment, zkEnabled, deploymentSelfAlias } from "@/lib/deploy-url";
import { RawPortConnections } from "@/components/raw-port-connections";

// The project sub-tabs now live in the TOP NAV (breadcrumb-tabs model, issue 3);
// they drive this page via `?tab=`. The page reads that param REACTIVELY so a tab
// click in the header switches the in-page view without a remount. Wrapped in
// Suspense (useSearchParams requirement) at the export.
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
  // Per-project observability comes from the TENANT-SCOPED metrics endpoint, not
  // /v1/overview: overview's counters (requests/cdn/concurrency) are platform-wide
  // and operator-only — regular team members receive a stripped response without
  // them, which both crashed the old unguarded read and would otherwise pin this
  // card to permanent zeros. /v1/metrics?project= is member-accessible and filtered
  // to this project. (180 is the backend's max window.)
  const { data: pm } = usePoll<Metrics>(`/v1/metrics?minutes=180&project=${encodeURIComponent(name)}`, 8000);
  const tabParam = searchParams.get("tab");
  const tab: "overview" | "graph" | "workflows" | "resources" | "deployments" =
    tabParam === "graph" || tabParam === "deployments" || tabParam === "workflows" || tabParam === "resources"
      ? tabParam
      : "overview";
  const setTab = (t: "overview" | "graph" | "workflows" | "resources" | "deployments") =>
    router.replace(`/projects/${encodeURIComponent(name)}?tab=${t}`, { scroll: false });
  const [busy, setBusy] = useState("");
  // The deployment whose Redeploy modal is open (null = closed).
  const [redeployFor, setRedeployFor] = useState<Deployment | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [marketplaceOpen, setMarketplaceOpen] = useState(false);
  // A redeploy doesn't create a deployment row until its build FINISHES (the live
  // version keeps serving meanwhile). The in-flight "Building" rows come from a
  // PERSISTENT store (localStorage) so they survive navigating away + reload — the
  // global PendingBuildsProvider polls each build and removes it on completion, at
  // which point the REAL deployment row (a different id from the build id) appears.
  const rawPending = usePendingBuilds({ project: name });

  const mine = (deps ?? []).filter((d) => d.project === name);
  // MERGE, never replace: a pending row is dropped only once the real deployment
  // record for it is actually present in `mine`. Removing it the instant the build
  // reported ready left a visible gap, because the record is created on whichever
  // node ran the build while this page polls through round-robin DNS.
  const pendingRows = mergePending(rawPending, mine);

  // When a pending build completes (dropped from the store), pull the finished
  // deployment in immediately instead of waiting for the next poll tick.
  const pendingCount = pendingRows.length;
  useEffect(() => {
    refresh();
  }, [pendingCount, refresh]);
  const prod = mine.find((d) => d.production) ?? mine[0];
  // The overview card's URL. When no production exists and `prod` fell back to
  // a PREVIEW record, its own alias is the honest URL — `prod.alias` is the
  // project's production host, which that preview may or may not be claiming.
  // For a real production record deploymentSelfAlias returns d.alias verbatim.
  const prodAlias = prod ? (deploymentSelfAlias(prod) || prod.alias) : undefined;
  const rollbackTarget = mine.find((d) => !d.production); // newest non-prod build

  async function promote(id: string) {
    setBusy(id);
    try { await apiSend("POST", `/v1/deployments/${id}/promote`); await refresh(); }
    catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  // Redeploy now goes through the confirmation modal (environment + build cache).
  // Opening it for a specific deployment makes that the source/current build.
  async function removeDeployment(id: string) {
    if (!confirm(`Delete deployment ${id}?`)) return;
    setBusy(id);
    try { await apiSend("DELETE", `/v1/deployments/${id}`); await refresh(); }
    catch (e) { alert(String(e)); } finally { setBusy(""); }
  }
  /**
   * Cancel an IN-FLIGHT build (queued/building only — see the `canCancel` gate
   * on each row's menu). The id is the BUILD id: the same key `/deploy/:id`
   * renders and `/v1/builds/:id` is stored under, which is what is actually
   * still running — a finished deployment artifact is deleted, not cancelled.
   *
   * Two row kinds reach here and both must end in a truthful state:
   *   * a REAL deployment row still building — the server owns it, so a failure
   *     is surfaced and `refresh()` re-reads the authoritative state after.
   *   * an OPTIMISTIC pending row — it lives in localStorage and may not be
   *     server-registered yet, so a failed call there is EXPECTED (404) rather
   *     than an error worth shouting about; the local entry is dropped either
   *     way so a "Building" row can never outlive its own cancel.
   */
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
              <a className="text-sm text-link hover:underline" href={deploymentUrl(prodAlias, prod.region_code)} target="_blank" rel="noreferrer">
                {deploymentHost(prodAlias, prod.region_code)} <ExternalLink className="inline h-3 w-3" />
              </a>
            )}
          </div>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" onClick={() => setMarketplaceOpen(true)}>
            <ShieldCheck className="h-4 w-4" /> Marketplace
          </Button>
          <Button variant="outline" onClick={() => prod && setRedeployFor(prod)} disabled={!prod?.git}>
            {busy === "redeploy" ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />} Redeploy
          </Button>
          <Button variant="outline" onClick={() => rollbackTarget && promote(rollbackTarget.id)} disabled={!rollbackTarget || !!busy}>
            <RotateCcw className="h-4 w-4" /> Instant Rollback
          </Button>
          <Button variant="danger" onClick={deleteProject} disabled={busy === "project"}>
            {busy === "project" ? <Loader2 className="h-4 w-4 animate-spin" /> : <Trash2 className="h-4 w-4" />} Delete
          </Button>
          <a
            href={deploymentUrl(prodAlias, prod?.region_code)}
            target="_blank"
            rel="noreferrer"
            onClick={(e) => {
              // With the zkauth experiment on, mint an anonymous proof + bootstrap
              // the preview instead of a plain open. Otherwise the link proceeds.
              if (zkEnabled && prod) {
                e.preventDefault();
                openDeployment(prodAlias || prod.alias, name, prod.region_code);
              }
            }}
          >
            <Button>Visit</Button>
          </a>
        </div>
      </div>

      {/* Sub-tabs moved to the top nav (breadcrumb-tabs model, issue 3) — the
          header's row-2 now carries Overview/Service Graph/Workflows/Resources/
          Deployments/Logs/Settings for the selected project. */}
      <div className="mb-6" />

      {tab === "graph" ? (
        <ServiceGraph project={name} prod={prod} />
      ) : tab === "workflows" ? (
        /* The LITERAL upstream @workflow/web console, project-scoped (the
           iframe name carries the scope; its own Runs/Hooks/Workflows
           segmented control replaces the old ?wf= sub-tabs). */
        <WfConsoleFrame project={name} />
      ) : tab === "resources" ? (
        <>
          <MarketplaceProjectResources project={name} />
          <DeploymentResources deploymentId={prod?.id} />
        </>
      ) : tab === "overview" ? (
        <>
          <MarketplaceProjectResources project={name} />
          {/* Production Deployment */}
          <Card className="mb-6 p-0">
            <div className="flex items-center justify-between border-b border-border px-5 py-3">
              <span className="font-medium">Production Deployment</span>
              <div className="flex gap-2">
                <Button variant="outline" onClick={() => setMarketplaceOpen(true)}>
                  <ShieldCheck className="h-4 w-4" /> Marketplace
                </Button>
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
        {/* Deployments toolbar: per-deployment actions (rollback / redeploy / delete)
            now live in the "⋯" menu at the end of each row. */}
        <div className="flex flex-wrap items-center justify-between gap-2">
          <span className="text-sm text-secondary">{mine.length + pendingRows.length} deployment{mine.length + pendingRows.length === 1 ? "" : "s"}</span>
          <div className="flex gap-2">
            <Button onClick={() => setCreateOpen(true)}><Plus className="h-4 w-4" /> New Deployment</Button>
            <Button variant="outline" onClick={() => setMarketplaceOpen(true)}><ShieldCheck className="h-4 w-4" /> Marketplace</Button>
          </div>
        </div>
        <Table>
          <thead>
            <tr><Th className="px-2">Deployment</Th><Th className="px-2">Status</Th><Th className="px-2">Environment</Th><Th className="px-2 hidden md:table-cell">Source</Th><Th className="px-2 hidden sm:table-cell">Created</Th><Th className="px-2 hidden lg:table-cell">By</Th><Th className="px-2"></Th></tr>
          </thead>
          <tbody>
            {/* Optimistic "Building" rows for in-flight redeploys (immediate
                feedback). Replaced by the real row once the build finishes. */}
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
                {/* An in-flight build's only meaningful action is stopping it,
                    so this row gets the same "⋯" menu carrying Cancel alone —
                    it previously rendered an empty cell, leaving a build that
                    is actively burning CPU with no way to stop it from here. */}
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
                {/* Click the row → deployment detail (logs, status, domains). The id
                    link opens the LIVE deployment URL (stops row navigation). */}
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
                  {/* Immutable build environment + a marker for the deployment
                      currently promoted to the production domain. */}
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
                <Td className="px-2 text-secondary hidden sm:table-cell">{timeAgo(d.created_at_ms)} ago</Td>
                <Td className="px-2 text-secondary hidden lg:table-cell">{d.creator}</Td>
                <Td className="px-2">
                  <div onClick={(e) => e.stopPropagation()}>
                    <DeploymentRowMenu
                      canRollback={!d.production}
                      canRedeploy={!!d.git}
                      /* Cancel is offered ONLY while the build is genuinely in
                         flight — a ready/error deployment has nothing left to
                         stop, and offering it there would be a dead action. */
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
            {!mine.length && !pendingRows.length && <tr><Td className="text-secondary">No deployments.</Td></tr>}
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
            // The modal already persisted the in-flight build to the store (so its
            // "Building" row survives navigation); just surface the deployments tab.
            setTab("deployments");
            refresh();
          }}
        />
      )}
      {createOpen && (
        <CreateDeploymentModal
          project={name}
          repoUrl={prod?.git?.repo_url ?? ""}
          branch={prod?.git?.branch}
          onClose={() => setCreateOpen(false)}
          onDone={() => {
            setTab("deployments");
            refresh();
          }}
        />
      )}
      {marketplaceOpen && (
        <MarketplaceDeploymentModal
          project={name}
          onClose={() => setMarketplaceOpen(false)}
          onDone={() => {
            setTab("deployments");
            refresh();
          }}
        />
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

  useEffect(() => {
    let stop = false;
    setImgError(false);
    setPv(null);
    if (!prod) return;
    apiGet<Preview>(`/v1/projects/${encodeURIComponent(project)}/preview`)
      .then((d) => { if (!stop) setPv(d); })
      .catch(() => { if (!stop) setPv({ kind: "none" }); });
    return () => { stop = true; };
  }, [project, prod?.id]);

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
      try { body = JSON.stringify(JSON.parse(body), null, 2); } catch { /* keep raw */ }
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
