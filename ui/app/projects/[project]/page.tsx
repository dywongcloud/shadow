"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
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
} from "lucide-react";
import dynamic from "next/dynamic";
import { Card, Button, Badge, Triangle, Table, Th, Td } from "@/components/ui";
import { ProjectWorkflows } from "@/components/workflows";
import { DeploymentResources } from "@/components/deployment-resources";
import { DeploymentRowMenu } from "@/components/deployment-menu";
import { RedeployModal } from "@/components/redeploy-modal";

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
import { apiGet, apiSend, usePoll, type Deployment, type Overview } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { deploymentUrl, deploymentHost, openDeployment, zkEnabled } from "@/lib/deploy-url";

export default function ProjectDetail({ params }: { params: { project: string } }) {
  const name = decodeURIComponent(params.project);
  const router = useRouter();
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  const { data: ov } = usePoll<Overview>("/v1/overview", 4000);
  const [tab, setTab] = useState<"overview" | "graph" | "workflows" | "resources" | "deployments">("overview");
  useEffect(() => {
    if (typeof window === "undefined") return;
    const t = new URLSearchParams(window.location.search).get("tab");
    if (t === "graph" || t === "deployments" || t === "overview" || t === "workflows" || t === "resources") setTab(t);
  }, []);
  const [busy, setBusy] = useState("");
  // The deployment whose Redeploy modal is open (null = closed).
  const [redeployFor, setRedeployFor] = useState<Deployment | null>(null);

  const mine = (deps ?? []).filter((d) => d.project === name);
  const prod = mine.find((d) => d.production) ?? mine[0];
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
              <a className="text-sm text-link hover:underline" href={deploymentUrl(prod.alias)} target="_blank" rel="noreferrer">
                {deploymentHost(prod.alias)} <ExternalLink className="inline h-3 w-3" />
              </a>
            )}
          </div>
        </div>
        <div className="flex gap-2">
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
            href={deploymentUrl(prod?.alias)}
            target="_blank"
            rel="noreferrer"
            onClick={(e) => {
              // With the zkauth experiment on, mint an anonymous proof + bootstrap
              // the preview instead of a plain open. Otherwise the link proceeds.
              if (zkEnabled && prod) {
                e.preventDefault();
                openDeployment(prod.alias, name);
              }
            }}
          >
            <Button>Visit</Button>
          </a>
        </div>
      </div>

      {/* sub tabs */}
      <div className="mb-6 flex items-center gap-1 border-b border-border">
        {(["overview", "graph", "workflows", "resources", "deployments"] as const).map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`relative px-3 py-2 text-sm capitalize ${tab === t ? "text-fg" : "text-secondary hover:text-fg"}`}
          >
            {t === "graph" ? "Service Graph" : t}
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

      {tab === "graph" ? (
        <ServiceGraph project={name} prod={prod} />
      ) : tab === "workflows" ? (
        <ProjectWorkflows project={name} />
      ) : tab === "resources" ? (
        <DeploymentResources deploymentId={prod?.id} />
      ) : tab === "overview" ? (
        <>
          {/* Production Deployment */}
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
                  <a className="text-link hover:underline" href={deploymentUrl(prod?.alias)} target="_blank" rel="noreferrer">
                    {prod ? deploymentHost(prod.alias) : "—"} <ExternalLink className="inline h-3 w-3" />
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
        <div className="space-y-3">
        {/* Deployments toolbar: per-deployment actions (rollback / redeploy / delete)
            now live in the "⋯" menu at the end of each row. */}
        <div className="flex flex-wrap items-center justify-between gap-2">
          <span className="text-sm text-secondary">{mine.length} deployment{mine.length === 1 ? "" : "s"}</span>
          <div className="flex gap-2">
            <Link href={`/new?repo=${encodeURIComponent(prod?.git?.repo_url ?? "")}`}>
              <Button><Plus className="h-4 w-4" /> New Deployment</Button>
            </Link>
          </div>
        </div>
        <Table>
          <thead>
            <tr><Th className="px-2">Deployment</Th><Th className="px-2">Status</Th><Th className="px-2">Environment</Th><Th className="px-2">Source</Th><Th className="px-2">Created</Th><Th className="px-2">By</Th><Th className="px-2"></Th></tr>
          </thead>
          <tbody>
            {mine.map((d) => (
              <tr key={d.id}>
                {/* Link to this deployment's OWN immutable URL (commit URL / id
                    URL), so a preview row opens that preview — not production. */}
                <Td className="px-2 font-mono text-xs">
                  <a className="text-link hover:underline" href={deploymentUrl(d.commit_alias || d.id_alias || d.alias)} target="_blank" rel="noreferrer">{d.id}</a>
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
                <Td className="px-2 font-mono text-xs text-secondary">
                  {d.git ? (
                    <span className="inline-flex items-center gap-1"><Code2 className="h-3.5 w-3.5" /> {d.git.branch} {d.git.commit}</span>
                  ) : (
                    <span className="inline-flex items-center gap-1"><Terminal className="h-3.5 w-3.5" /> hive deploy</span>
                  )}
                </Td>
                <Td className="px-2 text-secondary">{timeAgo(d.created_at_ms)} ago</Td>
                <Td className="px-2 text-secondary">{d.creator}</Td>
                <Td className="px-2">
                  <DeploymentRowMenu
                    canRollback={!d.production}
                    canRedeploy={!!d.git}
                    busy={busy === d.id}
                    onRollback={() => promote(d.id)}
                    onRedeploy={() => setRedeployFor(d)}
                    onDelete={() => removeDeployment(d.id)}
                  />
                </Td>
              </tr>
            ))}
            {!mine.length && <tr><Td className="text-secondary">No deployments.</Td></tr>}
          </tbody>
        </Table>
        </div>
      )}

      {redeployFor && (
        <RedeployModal
          deployment={redeployFor}
          prodAlias={prod?.alias || `${name}.localhost`}
          onClose={() => setRedeployFor(null)}
          onDone={() => refresh()}
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
      <a href={deploymentUrl(prod?.alias)} target="_blank" rel="noreferrer" className={`${box} block bg-bg group`}>
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img
          src={`/cloud${pv.url}`}
          alt={`${project} preview`}
          className="h-full w-full object-cover object-top transition-transform group-hover:scale-[1.02]"
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
