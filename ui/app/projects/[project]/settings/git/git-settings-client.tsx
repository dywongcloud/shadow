"use client";

import { useCallback, useEffect, useState, use } from "react";
import { useRouter } from "next/navigation";
import {
  Github,
  GitBranch,
  RefreshCw,
  AlertTriangle,
  ExternalLink,
  Loader2,
  Search,
  CheckCircle2,
  XCircle,
  Webhook as WebhookIcon,
} from "lucide-react";
import { Button, Input, Badge, SettingCard } from "@/components/ui";
import { apiGet, apiSend, apiDeployViaServerRoute, currentTeam, type Deployment, type ProjectSettings } from "@/lib/api";
import { cachedJson } from "@/lib/cache";
import { addPendingBuild } from "@/lib/pending-builds";
import { toast } from "@/components/toast";
import { timeAgo } from "@/lib/utils";

interface GhRepo {
  name: string;
  full_name: string;
  clone_url: string;
  default_branch: string;
  private: boolean;
}

/** Subset of githubConnectionDetail needed here — mirrors app/new/page.tsx. */
interface GhDetail {
  configured: boolean;
  connected: boolean;
  hasOrgScope?: boolean;
}

/** Response shape from /api/gitops/project-ci (see that route for the source of
 *  truth) — the same install call the "New Project" git-import flow fires. */
interface CiInstallResult {
  ok?: boolean;
  skipped?: boolean;
  reason?: string;
  /** Present on a LOUD install failure (HTTP 502): the real GitHub-side error(s). */
  error?: string;
  webhookInstalled?: boolean;
  workflowInstalled?: boolean;
}

function reasonLabel(reason: string): string {
  switch (reason) {
    case "github-not-configured":
      return "GitHub isn't configured on this platform";
    case "github-not-connected":
      return "GitHub isn't connected for this account";
    case "bad-repo":
      return "the repository URL couldn't be parsed";
    case "request-failed":
      return "the install request failed";
    default:
      return reason;
  }
}

export function GitSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const router = useRouter();

  const [dep, setDep] = useState<Deployment | null>(null);
  const [settings, setSettings] = useState<ProjectSettings | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [loadErr, setLoadErr] = useState("");

  const [retrying, setRetrying] = useState(false);
  const [retryMsg, setRetryMsg] = useState("");

  // ---- "Connect a Git Repository" picker (only relevant when there's no git
  // source yet — a project created via zip-upload or a registry image). ----
  const [gh, setGh] = useState<GhDetail>({ configured: false, connected: false });
  const [repos, setRepos] = useState<GhRepo[]>([]);
  const [ghCta, setGhCta] = useState<{ text: string; approveUrl?: string } | null>(null);
  const [repoQ, setRepoQ] = useState("");
  const [pickedRepo, setPickedRepo] = useState<GhRepo | null>(null);
  const [branch, setBranch] = useState("");
  const [connecting, setConnecting] = useState(false);
  const [connectError, setConnectError] = useState("");

  const load = useCallback(async () => {
    try {
      const [all, s] = await Promise.all([
        apiGet<Deployment[]>("/deployments").catch(() => [] as Deployment[]),
        apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`),
      ]);
      setDep(all.find((d) => d.project === project) ?? null);
      setSettings(s);
      setLoadErr("");
    } catch (e) {
      setLoadErr(String(e));
    } finally {
      setLoaded(true);
    }
  }, [project]);

  useEffect(() => {
    // Fetch-on-mount/project-change: load() sets state only after its
    // internal awaits resolve, syncing external (server) settings into React.
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch effect; state is set only after internal awaits, not synchronously
    load();
  }, [load]);

  const hasGit = !!dep?.git;

  // Populate the repo picker only once we know the project has no git source —
  // avoids an unnecessary GitHub round trip for the (common) already-connected case.
  useEffect(() => {
    if (!loaded || hasGit) return;
    cachedJson<GhDetail>("/api/github/status", 30_000)
      .then(async (s) => {
        setGh({ ...s, configured: !!s.configured, connected: !!s.connected });
        if (!s.connected) return;
        const personal = await cachedJson<{ repos: GhRepo[] }>("/api/github/repos", 5 * 60_000)
          .then((d) => d.repos || [])
          .catch(() => [] as GhRepo[]);
        const merged: GhRepo[] = [...personal];
        const seen = new Set(personal.map((r) => r.full_name));
        // Only merge org repos when we can enumerate them (read:org) — mirrors
        // the same non-disruptive policy as the New Project import screen.
        if (s.hasOrgScope) {
          const orgs = await cachedJson<{ orgs: { login: string }[] }>("/api/github/orgs", 5 * 60_000)
            .then((d) => d.orgs || [])
            .catch(() => [] as { login: string }[]);
          const results = await Promise.all(
            orgs.slice(0, 8).map((o) =>
              fetch(`/api/github/repos?org=${encodeURIComponent(o.login)}`)
                .then((r) => r.json())
                .catch(() => ({ repos: [] as GhRepo[] })),
            ),
          );
          let restrictedCta: { text: string; approveUrl?: string } | null = null;
          for (const res of results) {
            if (res?.restricted && res?.approve_url && !restrictedCta) {
              restrictedCta = {
                text: "An organization restricts DevHub. Approve the app to import its repositories.",
                approveUrl: res.approve_url,
              };
            }
            for (const r of (res?.repos as GhRepo[]) || []) {
              if (r?.full_name && !seen.has(r.full_name)) {
                seen.add(r.full_name);
                merged.push(r);
              }
            }
          }
          if (restrictedCta) setGhCta(restrictedCta);
        }
        setRepos(merged);
      })
      .catch(() => {});
  }, [loaded, hasGit]);

  async function connectGithub() {
    const r = await fetch("/api/github/connect", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ returnTo: `/projects/${encodeURIComponent(project)}/settings/git` }),
    });
    const d = await r.json();
    if (d.redirectUrl) window.location.href = d.redirectUrl;
    else setConnectError(d.error || "Could not start GitHub connection");
  }
  async function reconnectGithub() {
    await fetch("/api/github/disconnect", { method: "POST" }).catch(() => {});
    await connectGithub();
  }

  // The SAME install logic the "New Project" git-import flow fires right after
  // create (see app/new/page.tsx's deploy()) — reused verbatim here as a
  // standalone, idempotent call: /api/gitops/project-ci already takes just
  // `{ repo }` and returns a fresh outcome every time (GitHub itself no-ops a
  // duplicate webhook rather than creating a second one), so calling it again
  // on an existing project is a safe manual retry, not a first-time-only step.
  async function installCi(repoUrl: string): Promise<CiInstallResult> {
    const j: CiInstallResult = await fetch("/api/gitops/project-ci", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ repo: repoUrl }),
    })
      .then((r) => r.json())
      .catch(() => ({ skipped: true, reason: "request-failed" }) as CiInstallResult);
    await apiSend("PUT", `/v1/projects/${encodeURIComponent(project)}/git-ci`, {
      webhook_installed: !!j.webhookInstalled,
      workflow_installed: !!j.workflowInstalled,
      skipped_reason: j.skipped ? j.reason || "unknown" : "",
      checked_ms: 0,
    }).catch(() => {});
    return j;
  }

  async function retryInstall() {
    if (!dep?.git?.repo_url) return;
    setRetrying(true);
    setRetryMsg("");
    try {
      const j = await installCi(dep.git.repo_url);
      const s = await apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`, { fresh: true });
      setSettings(s);
      setRetryMsg(
        j.webhookInstalled || j.workflowInstalled
          ? "Installed — pushes to this repository will now auto-deploy."
          : j.error
          ? `Install failed: ${j.error}`
          : j.skipped
          ? `Skipped: ${reasonLabel(j.reason || "unknown")}.`
          : "Install attempt finished — see status above.",
      );
    } catch (e) {
      setRetryMsg(String(e));
    } finally {
      setRetrying(false);
    }
  }

  async function connectRepo() {
    if (!pickedRepo) return;
    setConnecting(true);
    setConnectError("");
    try {
      const ref = branch.trim() || pickedRepo.default_branch;
      // Via the server route so a PRIVATE repo gets the user's GitHub token
      // attached server-side for the clone (never in the browser).
      const res = await apiDeployViaServerRoute<{ build_id: string }>("/api/git/deploy", {
        repo_url: pickedRepo.clone_url,
        branch: ref,
        project,
        target: "production",
        production: true,
        // This project already exists (created via zip/image) — reuse its name
        // verbatim instead of treating this as a colliding new-project create.
        redeploy: true,
      });
      addPendingBuild({ id: res.build_id, project, team: currentTeam(), env: "production" });
      // Awaited (unlike the fire-and-forget on the New Project screen) since the
      // user stays on this settings page rather than navigating to build logs —
      // the status card below should already be right by the time they see it.
      await installCi(pickedRepo.clone_url);
      toast("Repository connected — deploying…");
      router.push(`/deploy/${res.build_id}`);
    } catch (e) {
      setConnectError(String(e));
      setConnecting(false);
    }
  }

  const filteredRepos = repos.filter((r) => r.full_name.toLowerCase().includes(repoQ.toLowerCase()));

  if (!loaded) return <div className="text-sm text-secondary">Loading…</div>;
  if (loadErr) return <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-4 text-sm text-red-500">Failed to load: {loadErr}</div>;

  return (
    <div className="flex flex-col gap-6">
      {hasGit ? (
        <>
          <SettingCard title="Connected Repository" desc="The Git source this project builds and deploys from.">
            <div className="flex flex-col divide-y divide-border text-sm">
              <Row label="Repository" value={<span className="font-mono text-xs">{dep!.git!.repo_url}</span>} />
              <Row
                label="Branch"
                value={
                  <span className="inline-flex items-center gap-1.5 font-mono text-xs">
                    <GitBranch className="h-3.5 w-3.5" /> {dep!.git!.branch || "—"}
                  </span>
                }
              />
              <Row label="Production Branch" value={<span className="font-mono text-xs">{settings?.production_branch || "—"}</span>} />
              {dep!.git!.commit ? (
                <Row
                  label="Latest Commit"
                  value={
                    <span className="max-w-xs truncate font-mono text-xs" title={dep!.git!.commit_message}>
                      {dep!.git!.commit.slice(0, 7)} — {dep!.git!.commit_message}
                    </span>
                  }
                />
              ) : null}
            </div>
          </SettingCard>

          <SettingCard
            title="Continuous Integration"
            desc="Whether a push to this repository actually triggers an auto-deploy — a real GitHub webhook (covers every branch + PR), or an Actions-workflow fallback (push-to-production-branch only) when a webhook can't be installed."
            footer={settings?.git_ci?.checked_ms ? `Last checked ${timeAgo(settings.git_ci.checked_ms)} ago` : "Never checked — this project's git-import CI install may not have run yet."}
            footerAction={
              <Button onClick={retryInstall} disabled={retrying}>
                {retrying ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
                Retry webhook install
              </Button>
            }
          >
            <div className="flex flex-col gap-3">
              <div className="flex items-center justify-between text-sm">
                <span className="inline-flex items-center gap-2">
                  <WebhookIcon className="h-4 w-4 text-muted" /> GitHub webhook
                </span>
                {settings?.git_ci?.webhook_installed ? (
                  <Badge tone="green">
                    <CheckCircle2 className="h-3 w-3" /> Installed
                  </Badge>
                ) : (
                  <Badge tone="default">
                    <XCircle className="h-3 w-3" /> Not installed
                  </Badge>
                )}
              </div>
              <div className="flex items-center justify-between text-sm">
                <span className="inline-flex items-center gap-2">
                  <GitBranch className="h-4 w-4 text-muted" /> Actions workflow (fallback)
                </span>
                {settings?.git_ci?.workflow_installed ? (
                  <Badge tone="green">
                    <CheckCircle2 className="h-3 w-3" /> Installed
                  </Badge>
                ) : (
                  <Badge tone="default">
                    <XCircle className="h-3 w-3" /> Not installed
                  </Badge>
                )}
              </div>
              {!settings?.git_ci?.webhook_installed && !settings?.git_ci?.workflow_installed && (
                <div className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-600 dark:text-amber-400">
                  <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  <span>
                    Auto-deploy on push is NOT configured{settings?.git_ci?.skipped_reason ? ` — ${reasonLabel(settings.git_ci.skipped_reason)}` : ""}.
                    Future pushes to this repository will not trigger a deployment until this is retried successfully.
                  </span>
                </div>
              )}
              {retryMsg ? <p className="text-xs text-secondary">{retryMsg}</p> : null}
            </div>
          </SettingCard>
        </>
      ) : (
        <SettingCard
          title="Connect a Git Repository"
          desc="This project was created without a Git source (a .zip upload or a container image). Connect a repository to switch it to Git-based deployments — every push will build and deploy automatically once connected."
        >
          {gh.connected ? (
            <div className="flex flex-col gap-3">
              {ghCta ? (
                <div className="flex flex-col gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-600 dark:text-amber-400">
                  <span className="flex items-start gap-1.5">
                    <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" /> {ghCta.text}
                  </span>
                  <div className="flex flex-wrap items-center gap-3">
                    {ghCta.approveUrl ? (
                      <a href={ghCta.approveUrl} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 font-medium underline underline-offset-2 hover:text-fg">
                        Approve the app <ExternalLink className="h-3 w-3" />
                      </a>
                    ) : null}
                    <button onClick={reconnectGithub} className="inline-flex items-center gap-1 font-medium underline underline-offset-2 hover:text-fg">
                      <RefreshCw className="h-3 w-3" /> Reconnect / grant access
                    </button>
                  </div>
                </div>
              ) : null}

              {pickedRepo ? (
                <div className="rounded-lg border border-border p-4">
                  <div className="flex items-center gap-2 text-sm font-medium">
                    <Github className="h-4 w-4" /> {pickedRepo.full_name}
                  </div>
                  <label className="mb-1.5 mt-3 block text-xs text-secondary">Branch to deploy</label>
                  <Input value={branch} onChange={(e) => setBranch(e.target.value)} placeholder={pickedRepo.default_branch} />
                  {connectError ? <p className="mt-2 text-sm text-red-600 dark:text-red-400">{connectError}</p> : null}
                  <div className="mt-4 flex items-center gap-2">
                    <Button onClick={connectRepo} disabled={connecting}>
                      {connecting ? <Loader2 className="h-4 w-4 animate-spin" /> : "Connect & Deploy"}
                    </Button>
                    <Button
                      variant="outline"
                      onClick={() => {
                        setPickedRepo(null);
                        setConnectError("");
                      }}
                      disabled={connecting}
                    >
                      Back
                    </Button>
                  </div>
                </div>
              ) : (
                <>
                  <div className="relative">
                    <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
                    <Input placeholder="Search…" value={repoQ} onChange={(e) => setRepoQ(e.target.value)} className="pl-9" />
                  </div>
                  <div className="flex max-h-80 flex-col divide-y divide-border overflow-y-auto">
                    {filteredRepos.map((r) => (
                      <div key={r.full_name} className="flex items-center justify-between py-2.5">
                        <div className="flex items-center gap-2 text-sm">
                          <Github className="h-4 w-4 text-secondary" />
                          <span className="font-medium">{r.name}</span>
                          {r.private ? <Badge>private</Badge> : null}
                        </div>
                        <Button
                          onClick={() => {
                            setBranch("");
                            setConnectError("");
                            setPickedRepo(r);
                          }}
                        >
                          Connect
                        </Button>
                      </div>
                    ))}
                    {!filteredRepos.length && <div className="py-6 text-center text-sm text-secondary">No repositories.</div>}
                  </div>
                </>
              )}
            </div>
          ) : (
            <div className="flex flex-col items-center gap-3 py-8 text-center">
              <Github className="h-8 w-8" />
              <div className="font-medium">{ghCta ? "Reconnect GitHub" : "Connect GitHub"}</div>
              <p className="max-w-xs text-sm text-secondary">
                {!gh.configured
                  ? "GitHub isn't configured on this platform yet."
                  : ghCta
                  ? ghCta.text
                  : "Authorize GitHub to connect a repository to this project."}
              </p>
              {gh.configured && (
                <Button onClick={ghCta ? reconnectGithub : connectGithub}>
                  {ghCta ? <RefreshCw className="h-4 w-4" /> : <Github className="h-4 w-4" />}
                  {ghCta ? "Reconnect GitHub" : "Connect GitHub"}
                </Button>
              )}
              {connectError ? <p className="text-sm text-red-600 dark:text-red-400">{connectError}</p> : null}
            </div>
          )}
        </SettingCard>
      )}
    </div>
  );
}

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between py-2.5">
      <span className="text-secondary">{label}</span>
      <span className="font-medium">{value}</span>
    </div>
  );
}
