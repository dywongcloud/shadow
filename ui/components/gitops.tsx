"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { usePathname } from "next/navigation";
import { Github, GitBranch, Check, Loader2, X, ArrowRight, ExternalLink } from "lucide-react";
import { currentTeam } from "@/lib/api";

interface GhStatus { configured: boolean; connected: boolean; entity?: string | null }
interface Repo { name: string; full_name: string; default_branch: string; private: boolean; owner?: string }

/** Last GitOps sync outcome, persisted for the UI (mirror card, error banner). */
export interface GitopsSyncStatus {
  at: number; // epoch ms
  ok: boolean;
  mode: "server"; // server-side sync is the only path (no in-browser git)
  detail: string; // "pushed <sha>" | "unchanged" | error message
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
  } catch {
    /* storage full/blocked — status is best-effort */
  }
  const color = s.ok ? "color:#3fb950" : "color:#f85149";
  console.log(`%cgitops%c ${s.mode} sync ${s.ok ? "ok" : "FAILED"} — ${s.detail}`, "background:#8957e5;color:#fff;padding:1px 5px;border-radius:3px;font-weight:600", color);
}

let syncInFlight = false;

/**
 * Fire a GitOps config sync — fully autonomous and in the background.
 *
 * SERVER-SIDE ONLY: posts to the sync route (`/api/gitops/sync`), which reads the
 * tenant's linked config repo, regenerates the artifact tree from live platform
 * state, and commits it via the GitHub API using the connected (Composio) token —
 * reliable, no browser git, no CORS proxy, works for every mutation made while
 * signed in. There is NO in-browser git fallback: when GitOps isn't set up (Composio
 * not configured / GitHub not connected / no repo linked) the sync is a benign no-op
 * and the "Set up GitOps" onboarding flow is how the user connects. Every outcome
 * (pushed / unchanged / not-configured / error) is recorded to `hive_gitops_status`
 * so state is VISIBLE in the UI instead of dying in the console.
 */
export function triggerGitopsSync() {
  if (typeof window === "undefined" || syncInFlight) return;
  syncInFlight = true;
  (async () => {
    try {
      const team = currentTeam();
      const r = await fetch("/api/gitops/sync", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ team }),
      });
      const d = await r.json().catch(() => ({}) as any);
      if (d?.ok) {
        recordStatus({ at: Date.now(), ok: true, mode: "server", detail: `pushed ${String(d.commit || "").slice(0, 8)} (${d.files} artifacts)`, repo: d.repo, commit: d.commit });
        return;
      }
      if (d?.unchanged) {
        recordStatus({ at: Date.now(), ok: true, mode: "server", detail: "unchanged", repo: d.repo });
        return;
      }
      if (d?.skipped) {
        // GitOps isn't set up yet (Composio not configured, GitHub not connected, or
        // no config repo linked). This is a benign NOT-CONFIGURED state, not a sync
        // failure — the "Set up GitOps" onboarding connects GitHub + links the repo.
        // Record it (never a red error toast) and do NO in-browser git.
        recordStatus({ at: Date.now(), ok: true, mode: "server", detail: `not configured (${String(d.reason || "not set up")})` });
        return;
      }
      // Server route reachable but the commit failed — surface it.
      recordStatus({ at: Date.now(), ok: false, mode: "server", detail: String(d?.error || `sync failed (HTTP ${r.status})`) });
    } catch (e) {
      recordStatus({ at: Date.now(), ok: false, mode: "server", detail: e instanceof Error ? e.message : String(e) });
    } finally {
      syncInFlight = false;
    }
  })();
}

/**
 * Post-signup onboarding modal + the background config-sync loop.
 *
 * Onboarding: prompts the user to optionally connect GitHub (Composio OAuth).
 * After connecting they pick a scope (personal account or an org repo) and a
 * config repo; we then link it and push the first `openedge.yaml`. They can also
 * just skip and use their personal scope.
 *
 * Auto-sync: once linked, any settings/runtime/region/tier change reflects in the
 * committed config — a light interval re-syncs (deduped by content hash server
 * side) and a `gitops-sync` window event triggers an immediate push.
 */
export function GitOps() {
  const [step, setStep] = useState<"hidden" | "intro" | "repo">("hidden");
  const [status, setStatus] = useState<GhStatus>({ configured: false, connected: false });
  const [repos, setRepos] = useState<Repo[]>([]);
  const [loadingRepos, setLoadingRepos] = useState(false);
  const [scope, setScope] = useState<"personal" | "org">("personal");
  const [mode, setMode] = useState<"create" | "existing">("create");
  const [repo, setRepo] = useState("");
  const [orgLogin, setOrgLogin] = useState("");
  const [isPrivate, setIsPrivate] = useState(true);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(false);
  const [result, setResult] = useState<{ repo: string; created: boolean; files: number } | null>(null);
  const [error, setError] = useState("");
  // When an org restricts third-party OAuth apps, GitHub 403s repo creation; this
  // holds the org-owner approval URL so we show an actionable link, not raw JSON.
  const [approveUrl, setApproveUrl] = useState("");
  const inited = useRef(false);
  const pathname = usePathname();

  // Reload the "use existing" repo list for the current scope: the org's repos when
  // an org login is entered, else the personal account's. Debounced so typing an
  // org login doesn't spam the API.
  useEffect(() => {
    if (step !== "repo" || mode !== "existing" || !status.connected) return;
    const org = scope === "org" ? orgLogin.trim() : "";
    if (scope === "org" && !org) return; // wait for a login before listing org repos
    const t = setTimeout(() => loadRepos(org), 350);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [step, mode, scope, orgLogin, status.connected]);

  const finishOnboarding = useCallback(() => {
    localStorage.setItem("hive_onboarded", "1");
    setStep("hidden");
  }, []);

  // Decide whether to show the modal, and at which step.
  useEffect(() => {
    if (inited.current) return;
    inited.current = true;
    const params = new URLSearchParams(window.location.search);
    const justConnected = params.get("connected") === "github";
    if (justConnected) {
      // Clean the URL so a refresh doesn't reopen the flow.
      params.delete("connected");
      const qs = params.toString();
      window.history.replaceState({}, "", window.location.pathname + (qs ? `?${qs}` : ""));
    }
    const onboarded = localStorage.getItem("hive_onboarded") === "1";
    // Already linked a config repo in a prior session → setup is done, never nag.
    if (!justConnected && localStorage.getItem("hive_gitops_linked") === "1") {
      localStorage.setItem("hive_onboarded", "1");
      return;
    }

    // Bound the status probe so a slow Composio/backend round-trip (e.g. from a
    // far region) can't leave onboarding state hanging; abort after 4s.
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
          // GitHub is already linked (here or via Integrations/New Project) — the
          // user has effectively set up GitOps, so never nag again. Mark done.
          localStorage.setItem("hive_onboarded", "1");
        } else if (!onboarded) {
          // First run with no GitHub link: prompt once (offers GitHub OAuth or the
          // personal-scope path). finishOnboarding() persists hive_onboarded.
          setStep("intro");
        }
      })
      .catch(() => {
        // Status fetch failed/timed out — only prompt if we've never onboarded.
        if (localStorage.getItem("hive_onboarded") !== "1") setStep("intro");
      })
      .finally(() => clearTimeout(statusTimer));
  }, []);

  async function loadRepos(org?: string) {
    setLoadingRepos(true);
    try {
      // Org scope → list the ORG's repos, not the personal account's.
      const q = org && org.trim() ? `?org=${encodeURIComponent(org.trim())}` : "";
      const r = await fetch(`/api/github/repos${q}`);
      const d = await r.json();
      const list: Repo[] = Array.isArray(d.repos) ? d.repos : [];
      setRepos(list);
      setRepo(list[0] ? list[0].full_name : "");
    } catch {
      /* ignore */
    } finally {
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

  async function link() {
    setError("");
    setApproveUrl("");
    if (mode === "existing" && !repo.includes("/")) return;
    if (scope === "org" && !orgLogin.trim()) { setError("Enter the GitHub organization login."); return; }
    setBusy(true);
    try {
      // One call: create the repo (if needed), link it, scaffold + push the full
      // GitOps artifact tree as a single commit.
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
        // Org restricts third-party OAuth apps → show the org-owner approval link.
        if (d.approve_url) setApproveUrl(d.approve_url);
        setBusy(false);
        return;
      }
      localStorage.setItem("hive_gitops_linked", "1");
      // The linked repo is persisted server-side (backend GitOpsLink); the server
      // sync route reads it. Kick an immediate background sync so the first
      // post-setup change lands.
      triggerGitopsSync();
      setResult({ repo: d.repo, created: !!d.created, files: Array.isArray(d.files) ? d.files.length : 0 });
      setDone(true);
      setTimeout(finishOnboarding, 2200);
    } catch {
      setError("Failed to set up GitOps.");
      setBusy(false);
    }
  }

  // ---- background sync loop (always mounted) ----
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

  // ---- sync failure toast: make broken GitHub mirroring VISIBLE ----
  const [syncErr, setSyncErr] = useState<GitopsSyncStatus | null>(null);
  useEffect(() => {
    const onStatus = () => {
      const s = readGitopsStatus();
      // Only surface real push failures for a LINKED repo (a benign not-configured
      // sync records ok:true, so it never toasts). Auto-clears on the next success.
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
                  <input
                    value={orgLogin}
                    onChange={(e) => setOrgLogin(e.target.value)}
                    placeholder="github-org-login"
                    className="mt-2 w-full rounded-md border border-border bg-bg px-3 py-2 text-sm outline-none focus:border-fg"
                  />
                ) : null}

                {/* Create vs use-existing */}
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
                {approveUrl ? (
                  <a
                    href={approveUrl}
                    target="_blank"
                    rel="noreferrer"
                    className="mt-2 inline-flex items-center gap-1 text-xs font-medium text-link hover:underline"
                  >
                    Approve this app for the organization <ExternalLink className="h-3 w-3" />
                  </a>
                ) : null}

                <div className="mt-5 flex items-center justify-between gap-2">
                  <button onClick={finishOnboarding} className="rounded-md px-3 py-2 text-sm text-secondary hover:bg-bg">
                    Skip
                  </button>
                  <button
                    onClick={link}
                    disabled={busy || (mode === "existing" && !repo.includes("/")) || (scope === "org" && !orgLogin.trim())}
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
