"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { usePathname } from "next/navigation";
import { Github, GitBranch, Check, Loader2, X, ArrowRight, ExternalLink } from "lucide-react";
import { currentTeam } from "@/lib/api";

interface GhStatus { configured: boolean; connected: boolean; entity?: string | null }
interface Repo { name: string; full_name: string; default_branch: string; private: boolean; owner?: string }

/**
 * Fire a GitOps config sync. If a remote config repo is linked, push the artifact
 * tree to it (deduped server-side by content hash). Otherwise mirror the same tree
 * into the LOCAL in-browser provider (issue #4) so CRUD ops still produce versioned
 * GitOps artifacts with zero external setup. The local module is dynamically
 * imported so isomorphic-git never bloats the main bundle.
 */
export function triggerGitopsSync() {
  if (typeof window === "undefined") return;
  if (localStorage.getItem("hive_gitops_linked") !== "1") {
    import("@/lib/gitops-local")
      .then((m) => m.syncLocalGitops())
      .catch(() => {});
    return;
  }
  fetch("/api/gitops/sync", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ team: currentTeam() }),
  }).catch(() => {});
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
  const inited = useRef(false);
  const pathname = usePathname();

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

    fetch("/api/github/status")
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
        // Status fetch failed — only prompt if we've never onboarded.
        if (localStorage.getItem("hive_onboarded") !== "1") setStep("intro");
      });
  }, []);

  async function loadRepos() {
    setLoadingRepos(true);
    try {
      const r = await fetch("/api/github/repos");
      const d = await r.json();
      const list: Repo[] = Array.isArray(d.repos) ? d.repos : [];
      setRepos(list);
      if (list[0]) setRepo(list[0].full_name);
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
      if (!d.ok) { setError(d.error || "Failed to set up GitOps."); setBusy(false); return; }
      localStorage.setItem("hive_gitops_linked", "1");
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

  if (step === "hidden" || pathname.startsWith("/status")) return null;

  return (
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
