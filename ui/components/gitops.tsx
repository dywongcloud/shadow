"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { usePathname } from "next/navigation";
import { Github, GitBranch, Check, Loader2, X, ArrowRight, ExternalLink } from "lucide-react";
import { currentTeam, useCan } from "@/lib/api";
import { roleHint, roleNote } from "@/lib/team-role";

interface GhStatus {
  configured: boolean;
  connected: boolean;
  entity?: string | null;
  login?: string | null;
  scopes?: string[];
  hasPrivateAccess?: boolean;
  hasOrgScope?: boolean;
  live?: boolean;
}
interface Repo { name: string; full_name: string; default_branch: string; private: boolean; owner?: string }
interface GhOrg { login: string; name?: string }

/** Last GitOps sync outcome, persisted for the UI (mirror card, error banner). */
export interface GitopsSyncStatus {
  at: number;
  ok: boolean;
  mode: "server";
  detail: string;
  repo?: string;
  commit?: string;
}
const STATUS_KEY = "hive_gitops_status";
export function readGitopsStatus(): GitopsSyncStatus | null {
  if (typeof window === "undefined") return null;
  try {
    const raw = localStorage.getItem(STATUS_KEY);
    return raw ? (JSON.parse(raw) as GitopsSyncStatus) : null;
  } catch {
    return null;
  }
}
function recordStatus(s: GitopsSyncStatus) {
  try {
    localStorage.setItem(STATUS_KEY, JSON.stringify(s));
    window.dispatchEvent(new Event("gitops-status"));
  } catch {}
  const color = s.ok ? "color:#3fb950" : "color:#f85149";
  console.log(`%cgitops%c ${s.mode} sync ${s.ok ? "ok" : "FAILED"} — ${s.detail}`, "background:#8957e5;color:#fff;padding:1px 5px;border-radius:3px;font-weight:600", color);
}

const syncInFlight = new Map<string, boolean>();
const syncPending = new Map<string, boolean>();

const RETRY_DELAYS_MS = [2000, 4000, 8000];

function wait(ms: number) {
  return new Promise<void>((resolve) => setTimeout(resolve, ms));
}

/** One team's sync attempt, with bounded retry on transient (network / 5xx) failure. */
async function runGitopsSync(team: string) {
  for (let attempt = 0; ; attempt++) {
    let r: Response;
    let d: any;
    try {
      r = await fetch("/api/gitops/sync", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ team }),
      });
      d = await r.json().catch(() => ({}) as any);
    } catch (e) {
      if (attempt < RETRY_DELAYS_MS.length) {
        await wait(RETRY_DELAYS_MS[attempt]);
        continue;
      }
      recordStatus({ at: Date.now(), ok: false, mode: "server", detail: e instanceof Error ? e.message : String(e) });
      return;
    }

    if (d?.ok) {
      recordStatus({
        at: Date.now(),
        ok: true,
        mode: "server",
        detail: `pushed ${String(d.commit || "").slice(0, 8)} (${d.files} artifacts${d.deletionsSkipped ? ", cleanup retrying" : ""})`,
        repo: d.repo,
        commit: d.commit,
      });
      return;
    }
    if (d?.unchanged) {
      recordStatus({ at: Date.now(), ok: true, mode: "server", detail: "unchanged", repo: d.repo });
      return;
    }
    if (d?.skipped) {
      recordStatus({ at: Date.now(), ok: true, mode: "server", detail: `not configured (${String(d.reason || "not set up")})` });
      return;
    }

    const retryable = r.status >= 500 && r.status < 600;
    if (retryable && attempt < RETRY_DELAYS_MS.length) {
      await wait(RETRY_DELAYS_MS[attempt]);
      continue;
    }
    recordStatus({ at: Date.now(), ok: false, mode: "server", detail: String(d?.error || `sync failed (HTTP ${r.status})`) });
    return;
  }
}

/** Fires a background GitOps config sync, server-side only: `POST /api/gitops/sync` regenerates the artifact tree from live state
 *  and commits it with the connected GitHub credential (no browser git). Per team, a trigger during an in-flight sync is coalesced
 *  into exactly ONE trailing re-run for that same team; every outcome is recorded to `hive_gitops_status`. */
export function triggerGitopsSync(team: string = currentTeam()) {
  if (typeof window === "undefined") return;
  if (syncInFlight.get(team)) {
    syncPending.set(team, true);
    return;
  }
  syncInFlight.set(team, true);
  (async () => {
    try {
      await runGitopsSync(team);
    } finally {
      syncInFlight.delete(team);
      if (syncPending.get(team)) {
        syncPending.delete(team);
        triggerGitopsSync(team);
      }
    }
  })();
}

/** Post-signup onboarding modal (optional GitHub connect, scope + config-repo pick, first push) and the background sync loop: a
 *  light interval re-sync (deduped by content hash server-side) plus an immediate push on the `gitops-sync` window event. */
export function GitOps() {
  const [step, setStep] = useState<"hidden" | "intro" | "repo">("hidden");
  const [status, setStatus] = useState<GhStatus>({ configured: false, connected: false });
  const [repos, setRepos] = useState<Repo[]>([]);
  const [loadingRepos, setLoadingRepos] = useState(false);
  const [scope, setScope] = useState<"personal" | "org">("personal");
  const [mode, setMode] = useState<"create" | "existing">("create");
  const [repo, setRepo] = useState("");
  const [orgLogin, setOrgLogin] = useState("");
  const [orgs, setOrgs] = useState<GhOrg[]>([]);
  const [isPrivate, setIsPrivate] = useState(true);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(false);
  const [result, setResult] = useState<{ repo: string; created: boolean; files: number } | null>(null);
  const [error, setError] = useState("");
  const [approveUrl, setApproveUrl] = useState("");
  const inited = useRef(false);
  const pathname = usePathname();
  const mayLink = useCan()("repo.link");

  useEffect(() => {
    if (step !== "repo" || mode !== "existing" || !status.connected) return;
    const org = scope === "org" ? orgLogin.trim() : "";
    if (scope === "org" && !org) return;
    const t = setTimeout(() => loadRepos(org), 350);
    return () => clearTimeout(t);
  }, [step, mode, scope, orgLogin, status.connected]);

  const finishOnboarding = useCallback(() => {
    localStorage.setItem("hive_onboarded", "1");
    setStep("hidden");
  }, []);

  useEffect(() => {
    if (step !== "repo" || !status.connected) return;
    let cancelled = false;
    fetch("/api/github/orgs")
      .then((r) => r.json())
      .then((d) => { if (!cancelled) setOrgs(Array.isArray(d.orgs) ? d.orgs : []); })
      .catch(() => { if (!cancelled) setOrgs([]); });
    return () => { cancelled = true; };
  }, [step, status.connected]);

  useEffect(() => {
    const open = () => {
      setDone(false);
      setError("");
      setApproveUrl("");
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), 4000);
      fetch("/api/github/status", { signal: ctrl.signal })
        .then((r) => r.json())
        .then((s: GhStatus) => {
          setStatus(s);
          if (s.connected) { setStep("repo"); loadRepos(); }
          else setStep("intro");
        })
        .catch(() => setStep("intro"))
        .finally(() => clearTimeout(timer));
    };
    window.addEventListener("hive-open-gitops", open);
    return () => window.removeEventListener("hive-open-gitops", open);
  }, []);

  useEffect(() => {
    if (inited.current) return;
    inited.current = true;
    const params = new URLSearchParams(window.location.search);
    const justConnected = params.get("connected") === "github";
    if (justConnected) {
      params.delete("connected");
      const qs = params.toString();
      window.history.replaceState({}, "", window.location.pathname + (qs ? `?${qs}` : ""));
    }
    const onboarded = localStorage.getItem("hive_onboarded") === "1";
    if (!justConnected && localStorage.getItem("hive_gitops_linked") === "1") {
      localStorage.setItem("hive_onboarded", "1");
      return;
    }

    const statusCtrl = new AbortController();
    const statusTimer = setTimeout(() => statusCtrl.abort(), 4000);
    fetch("/api/github/status", { signal: statusCtrl.signal })
      .then((r) => r.json())
      .then((s: GhStatus) => {
        setStatus(s);
        if (justConnected && s.connected) {
          setStep("repo");
          loadRepos();
        } else if (s.connected) {
          localStorage.setItem("hive_onboarded", "1");
        } else if (!onboarded) {
          setStep("intro");
        }
      })
      .catch(() => {
        if (localStorage.getItem("hive_onboarded") !== "1") setStep("intro");
      })
      .finally(() => clearTimeout(statusTimer));
  }, []);

  async function loadRepos(org?: string) {
    setLoadingRepos(true);
    setApproveUrl("");
    try {
      const q = org && org.trim() ? `?org=${encodeURIComponent(org.trim())}` : "";
      const r = await fetch(`/api/github/repos${q}`);
      const d = await r.json();
      if (d.restricted) {
        setRepos([]);
        setRepo("");
        if (d.approve_url) setApproveUrl(d.approve_url);
        return;
      }
      const list: Repo[] = Array.isArray(d.repos) ? d.repos : [];
      setRepos(list);
      setRepo(list[0] ? list[0].full_name : "");
    } catch {} finally {
      setLoadingRepos(false);
    }
  }

  async function connectGithub() {
    setBusy(true);
    try {
      const r = await fetch("/api/github/connect", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ returnTo: window.location.pathname }),
      });
      const d = await r.json();
      if (d.redirectUrl) {
        window.location.href = d.redirectUrl;
        return;
      }
    } finally {
      setBusy(false);
    }
  }

  async function reconnectGithub() {
    setBusy(true);
    try {
      await fetch("/api/github/disconnect", { method: "POST" }).catch(() => {});
      const r = await fetch("/api/github/connect", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ returnTo: window.location.pathname }),
      });
      const d = await r.json();
      if (d.redirectUrl) { window.location.href = d.redirectUrl; return; }
    } finally {
      setBusy(false);
    }
  }

  async function link() {
    setError("");
    setApproveUrl("");
    if (mode === "existing" && !repo.includes("/")) return;
    if (scope === "org" && !orgLogin.trim()) { setError("Enter the GitHub organization login."); return; }
    setBusy(true);
    try {
      const r = await fetch("/api/gitops/init", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          team: currentTeam(),
          scope,
          create: mode === "create",
          org: scope === "org" ? orgLogin.trim() : undefined,
          isPrivate,
          repo: mode === "existing" ? repo : undefined,
          branch: mode === "existing" ? repos.find((x) => x.full_name === repo)?.default_branch : undefined,
        }),
      });
      const d = await r.json();
      if (!d.ok) {
        setError(d.error || "Failed to set up GitOps.");
        if (d.approve_url) setApproveUrl(d.approve_url);
        setBusy(false);
        return;
      }
      localStorage.setItem("hive_gitops_linked", "1");
      triggerGitopsSync();
      setResult({ repo: d.repo, created: !!d.created, files: Array.isArray(d.files) ? d.files.length : 0 });
      setDone(true);
      setTimeout(finishOnboarding, 2200);
    } catch {
      setError("Failed to set up GitOps.");
      setBusy(false);
    }
  }

  useEffect(() => {
    const tick = () => triggerGitopsSync();
    const onEvent = () => triggerGitopsSync();
    const id = setInterval(tick, 45_000);
    window.addEventListener("gitops-sync", onEvent);
    return () => {
      clearInterval(id);
      window.removeEventListener("gitops-sync", onEvent);
    };
  }, []);

  const [syncErr, setSyncErr] = useState<GitopsSyncStatus | null>(null);
  useEffect(() => {
    const onStatus = () => {
      const s = readGitopsStatus();
      setSyncErr(s && !s.ok && localStorage.getItem("hive_gitops_linked") === "1" ? s : null);
    };
    window.addEventListener("gitops-status", onStatus);
    onStatus();
    return () => window.removeEventListener("gitops-status", onStatus);
  }, []);

  const errToast =
    syncErr && !pathname.startsWith("/status") ? (
      <div className="fixed bottom-4 right-4 z-[90] w-80 rounded-lg border border-red-500/40 bg-card p-3 shadow-2xl">
        <div className="flex items-start justify-between gap-2">
          <div className="flex items-center gap-2 text-sm font-medium text-red-500">
            <GitBranch className="h-4 w-4" /> GitOps sync failed
          </div>
          <button onClick={() => setSyncErr(null)} className="text-muted hover:text-fg" aria-label="Dismiss">
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
        <p className="mt-1 break-words text-xs text-secondary">{syncErr.detail}</p>
        <button
          onClick={() => { setSyncErr(null); triggerGitopsSync(); }}
          className="mt-2 rounded-md border border-border px-2 py-1 text-xs text-secondary hover:bg-subtle hover:text-fg"
        >
          Retry now
        </button>
      </div>
    ) : null;

  if (step === "hidden" || pathname.startsWith("/status")) return errToast;

  return (
    <>
    {errToast}
    <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/50 p-4 backdrop-blur-sm">
      <div className="w-full max-w-md overflow-hidden rounded-xl border border-border bg-card shadow-2xl">
        <div className="flex items-center justify-between border-b border-border px-5 py-3.5">
          <div className="flex items-center gap-2 text-sm font-semibold">
            <GitBranch className="h-4 w-4" /> Set up GitOps
          </div>
          <button onClick={finishOnboarding} className="text-muted hover:text-fg" aria-label="Close">
            <X className="h-4 w-4" />
          </button>
        </div>

        {step === "intro" ? (
          <div className="px-5 py-5">
            <p className="text-sm text-secondary">
              Connect GitHub to manage your org as code. We&apos;ll commit your projects&apos;
              config to a repo as <span className="font-mono text-fg">openedge.yaml</span>, and a push
              to that repo will automatically rebuild and deploy your projects.
            </p>
            <ul className="mt-4 space-y-2 text-sm text-secondary">
              <FeatureRow text="Declarative config, versioned in git" />
              <FeatureRow text="Pushes trigger builds & deployments" />
              <FeatureRow text="Settings, runtime & region changes sync back" />
            </ul>
            <div className="mt-5 flex flex-col gap-2">
              <button
                onClick={connectGithub}
                disabled={busy || !status.configured}
                className="flex items-center justify-center gap-2 rounded-md bg-fg px-3 py-2.5 text-sm font-medium text-bg hover:opacity-90 disabled:opacity-50"
              >
                {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Github className="h-4 w-4" />}
                Connect GitHub
              </button>
              {!status.configured ? (
                <p className="text-center text-xs text-muted">
                  GitHub OAuth requires <span className="font-mono">COMPOSIO_API_KEY</span>. You can
                  still use your personal scope.
                </p>
              ) : null}
              <button
                onClick={finishOnboarding}
                className="rounded-md px-3 py-2 text-sm text-secondary hover:bg-bg"
              >
                Use personal scope for now
              </button>
            </div>
          </div>
        ) : null}

        {step === "repo" ? (
          <div className="px-5 py-5">
            {done && result ? (
              <div className="flex flex-col items-center gap-2 py-5 text-center">
                <span className="flex h-10 w-10 items-center justify-center rounded-full bg-green/15 text-green">
                  <Check className="h-5 w-5" />
                </span>
                <p className="text-sm font-medium">{result.created ? "Repository created & pushed" : "Config pushed"}</p>
                <p className="text-xs text-muted">
                  {result.files} artifact{result.files === 1 ? "" : "s"} committed to{" "}
                  <span className="font-mono text-fg">{result.repo}</span>
                </p>
                <a
                  href={`https://github.com/${result.repo}`}
                  target="_blank"
                  rel="noreferrer"
                  className="mt-1 inline-flex items-center gap-1 text-xs text-link hover:underline"
                >
                  View on GitHub <ExternalLink className="h-3 w-3" />
                </a>
              </div>
            ) : (
              <>
                <p className="text-sm text-secondary">
                  GitHub connected. We&apos;ll generate the OpenEdge spec, configs &amp; meta artifacts and
                  commit them to a repo.
                </p>

                <div className="mt-4 grid grid-cols-2 gap-2">
                  <ScopeCard active={scope === "personal"} onClick={() => setScope("personal")} title="Personal" desc="Your personal account" />
                  <ScopeCard active={scope === "org"} onClick={() => setScope("org")} title="Organization" desc="A GitHub org you own" />
                </div>

                {scope === "org" ? (
                  orgs.length > 0 ? (
                    <select
                      value={orgLogin}
                      onChange={(e) => setOrgLogin(e.target.value)}
                      className="mt-2 w-full rounded-md border border-border bg-bg px-3 py-2 text-sm outline-none focus:border-fg"
                    >
                      <option value="">Select an organization…</option>
                      {orgs.map((o) => (
                        <option key={o.login} value={o.login}>{o.login}</option>
                      ))}
                    </select>
                  ) : (
                    <div className="mt-2 space-y-2">
                      <div className="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-600 dark:text-amber-400">
                        We can&apos;t list your organizations — install the GitHub App on the
                        organization (Integrations → GitHub), or{" "}
                        <button
                          type="button"
                          onClick={reconnectGithub}
                          disabled={busy}
                          className="font-medium underline underline-offset-2 hover:text-fg disabled:opacity-50"
                        >
                          reconnect to grant organization access
                        </button>
                        , or type a known org login below.
                      </div>
                      <input
                        value={orgLogin}
                        onChange={(e) => setOrgLogin(e.target.value)}
                        placeholder="github-org-login"
                        className="w-full rounded-md border border-border bg-bg px-3 py-2 text-sm outline-none focus:border-fg"
                      />
                    </div>
                  )
                ) : null}

                <div className="mt-4 flex rounded-md border border-border p-0.5 text-xs">
                  {(["create", "existing"] as const).map((m) => (
                    <button
                      key={m}
                      onClick={() => setMode(m)}
                      className={`flex-1 rounded px-2 py-1.5 ${mode === m ? "bg-subtle font-medium text-fg" : "text-secondary"}`}
                    >
                      {m === "create" ? "Create new repo" : "Use existing"}
                    </button>
                  ))}
                </div>

                {mode === "create" ? (
                  <>
                    <div className="mt-3 rounded-md border border-border bg-bg px-3 py-2.5 text-xs text-secondary">
                      A dedicated repo <span className="font-mono text-fg">openedge-gitops-••••••</span> will be
                      auto-created for your config. Your source repos are never touched.
                    </div>
                    <label className="mt-2 flex cursor-pointer items-center gap-2 text-xs text-secondary">
                      <input type="checkbox" checked={isPrivate} onChange={(e) => setIsPrivate(e.target.checked)} className="h-3.5 w-3.5 accent-fg" />
                      Private repository
                    </label>
                  </>
                ) : (
                  <>
                    <label className="mt-3 block text-xs font-medium text-secondary">Config repository</label>
                    <select
                      value={repo}
                      onChange={(e) => setRepo(e.target.value)}
                      disabled={loadingRepos}
                      className="mt-1 w-full rounded-md border border-border bg-bg px-3 py-2 text-sm outline-none focus:border-fg"
                    >
                      {loadingRepos ? <option>Loading repositories…</option> : null}
                      {repos.map((r) => (
                        <option key={r.full_name} value={r.full_name}>
                          {r.full_name} {r.private ? "(private)" : ""}
                        </option>
                      ))}
                      {!loadingRepos && repos.length === 0 ? <option value="">No repositories found</option> : null}
                    </select>
                  </>
                )}

                {error ? <p className="mt-3 text-xs text-red-500">{error}</p> : null}
                {mayLink ? null : <p className="mt-3 text-xs text-muted">{roleNote("repo.link")}</p>}
                {approveUrl ? (
                  <div className="mt-2 flex flex-col gap-1.5">
                    <p className="text-xs text-secondary">
                      This organization restricts third-party OAuth apps. An org owner must approve
                      OpenEdge (members can request it), then re-check access.
                    </p>
                    <div className="flex flex-wrap items-center gap-3">
                      <a
                        href={approveUrl}
                        target="_blank"
                        rel="noreferrer"
                        className="inline-flex items-center gap-1 text-xs font-medium text-link hover:underline"
                      >
                        Approve this app for the organization <ExternalLink className="h-3 w-3" />
                      </a>
                      <button
                        type="button"
                        onClick={() => loadRepos(scope === "org" ? orgLogin.trim() : "")}
                        disabled={loadingRepos}
                        className="inline-flex items-center gap-1 text-xs text-secondary hover:text-fg disabled:opacity-50"
                      >
                        {loadingRepos ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
                        Re-check access
                      </button>
                    </div>
                  </div>
                ) : null}

                <div className="mt-5 flex items-center justify-between gap-2">
                  <button onClick={finishOnboarding} className="rounded-md px-3 py-2 text-sm text-secondary hover:bg-bg">
                    Skip
                  </button>
                  <button
                    onClick={link}
                    disabled={busy || !mayLink || (mode === "existing" && !repo.includes("/")) || (scope === "org" && !orgLogin.trim())}
                    title={mayLink ? undefined : roleHint("repo.link")}
                    className="flex items-center gap-2 rounded-md bg-fg px-3 py-2 text-sm font-medium text-bg hover:opacity-90 disabled:opacity-50"
                  >
                    {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <ArrowRight className="h-4 w-4" />}
                    {mode === "create" ? "Create & push" : "Link & push"}
                  </button>
                </div>
              </>
            )}
          </div>
        ) : null}
      </div>
    </div>
    </>
  );
}

function FeatureRow({ text }: { text: string }) {
  return (
    <li className="flex items-center gap-2">
      <Check className="h-4 w-4 shrink-0 text-green" /> {text}
    </li>
  );
}

function ScopeCard({ active, onClick, title, desc }: { active: boolean; onClick: () => void; title: string; desc: string }) {
  return (
    <button
      onClick={onClick}
      className={`rounded-lg border p-3 text-left transition-colors ${
        active ? "border-fg bg-bg" : "border-border hover:border-border-strong"
      }`}
    >
      <div className="text-sm font-medium">{title}</div>
      <div className="mt-0.5 text-[11px] text-muted">{desc}</div>
    </button>
  );
}
