"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { ArrowLeft, Github, Search, Loader2, GitBranch, FolderGit2, Lock, ExternalLink, ChevronDown, Plus, X, KeyRound } from "lucide-react";
import Link from "next/link";
import { Card, Button, Input, Badge } from "@/components/ui";
import { GlobeEmptyState } from "@/components/globe";
import { apiSend, currentTeam } from "@/lib/api";
import { addPendingBuild } from "@/lib/pending-builds";
import { TeamSelect } from "@/components/team-picker";
import { cachedJson } from "@/lib/cache";
import { cn } from "@/lib/utils";
import { PreparingDeployment } from "@/components/clone-animation";

// How long the "Preparing Git Repository" clone animation plays before the view
// transitions to the live build logs (the build itself runs async on the node).
const PREPARING_MS = 2800;
const GIT_SCOPE = "openedge";

interface GhRepo {
  name: string;
  full_name: string;
  clone_url: string;
  default_branch: string;
  private: boolean;
}

interface Template {
  name: string;
  desc: string;
  repo: string; // git URL
  root?: string; // subdir for monorepo examples
  branch?: string;
  tag: string; // framework label for the monogram fallback
  color: string;
  icon?: string; // real logo path (in /public/frameworks)
}

// Vercel-style starters: official `vercel/vercel` examples (built from the
// monorepo subdir) plus a few standalone static templates.
const TEMPLATES: Template[] = [
  { name: "Next.js Boilerplate", desc: "Get started with Next.js and React in seconds.", repo: "https://github.com/vercel/vercel", root: "examples/nextjs", tag: "N", color: "#000", icon: "/frameworks/nextjs.png" },
  { name: "Vite + React", desc: "A lightning-fast Vite SPA, deployed to the edge.", repo: "https://github.com/vercel/vercel", root: "examples/vite", tag: "V", color: "#646cff", icon: "/frameworks/vite.png" },
  { name: "React", desc: "The library for web and native user interfaces.", repo: "https://github.com/vercel/vercel", root: "examples/create-react-app", tag: "R", color: "#149eca", icon: "/frameworks/react.png" },
  { name: "SvelteKit", desc: "Cybernetically enhanced web apps.", repo: "https://github.com/vercel/vercel", root: "examples/sveltekit-1", tag: "S", color: "#ff3e00", icon: "/frameworks/svelte.svg" },
  { name: "Nuxt", desc: "The intuitive Vue framework.", repo: "https://github.com/vercel/vercel", root: "examples/nuxtjs", tag: "Nu", color: "#00dc82", icon: "/frameworks/nuxt.svg" },
  { name: "Vue", desc: "The progressive JavaScript framework.", repo: "https://github.com/vercel/vercel", root: "examples/vue", tag: "Vu", color: "#42b883", icon: "/frameworks/vue-js.png" },
  { name: "Angular", desc: "The web development framework for building modern apps.", repo: "https://github.com/vercel/vercel", root: "examples/angular", tag: "Ng", color: "#dd0031", icon: "/frameworks/angular.png" },
  { name: "Express", desc: "Fast, unopinionated Node.js web server.", repo: "https://github.com/vercel/vercel", root: "examples/express", tag: "Ex", color: "#000", icon: "/frameworks/express-js.png" },
  { name: "Remix", desc: "Full stack web framework, focused on web standards.", repo: "https://github.com/vercel/vercel", root: "examples/remix", tag: "Rx", color: "#000", icon: "/frameworks/remix.png" },
  { name: "Astro", desc: "Content-driven sites, fast by default.", repo: "https://github.com/vercel/vercel", root: "examples/astro", tag: "A", color: "#000", icon: "/frameworks/astro.png" },
  { name: "Node.js", desc: "A minimal Node.js server, deployed to Fluid compute.", repo: "https://github.com/vercel/vercel", root: "examples/node", tag: "No", color: "#539e43", icon: "/frameworks/node.png" },
  { name: "Bun", desc: "Incredibly fast all-in-one JavaScript runtime.", repo: "https://github.com/oven-sh/bun", root: "examples/bun-http", tag: "Bu", color: "#fbf0df", icon: "/frameworks/bun.png" },
  { name: "Deno", desc: "Secure runtime for JavaScript and TypeScript.", repo: "https://github.com/denoland/examples", root: "http-server", tag: "De", color: "#000", icon: "/frameworks/deno.png" },
  { name: "Cloudflare Workers", desc: "Deploy serverless functions to the edge.", repo: "https://github.com/cloudflare/templates", root: "worker-typescript-template", tag: "CF", color: "#f6821f", icon: "/frameworks/cloudflare-workers.png" },
  { name: "HTML Starter", desc: "A clean static site, deployed instantly.", repo: "https://github.com/mdn/beginner-html-site", tag: "H", color: "#e34f26", icon: "/frameworks/html5.png" },
  { name: "Container (Dockerfile)", desc: "Railway-style: build & run any Dockerfile.", repo: "https://github.com/crccheck/docker-hello-world", tag: "D", color: "#2496ed", icon: "/frameworks/docker.png" },
];

function slug(s: string) {
  return s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
}
function ownerRepo(url: string) {
  return url.replace(/^https?:\/\/(www\.)?github\.com\//, "").replace(/\.git$/, "");
}

/** Wrap an imported repo as a Template so it flows through the same configure
 *  screen — letting the user assign a project name before deploying. */
function repoToTemplate(r: GhRepo): Template {
  return {
    name: r.name,
    desc: r.full_name,
    repo: r.clone_url,
    branch: r.default_branch,
    tag: (r.name[0] || "R").toUpperCase(),
    color: "#000",
  };
}

function Monogram({ t, className = "h-10 w-10" }: { t: Template; className?: string }) {
  if (t.icon) {
    return (
      <span className={`flex ${className} shrink-0 items-center justify-center overflow-hidden rounded-lg border border-border bg-white`}>
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img src={t.icon} alt={t.name} className="h-3/4 w-3/4 object-contain" />
      </span>
    );
  }
  return (
    <span className={`flex ${className} shrink-0 items-center justify-center rounded-lg text-sm font-bold text-white`} style={{ background: t.color }}>
      {t.tag}
    </span>
  );
}

export default function NewProjectPage() {
  const router = useRouter();
  // Single unified source input: accepts a Git repository URL OR a container image
  // / registry reference (auto-detected on submit).
  const [url, setUrl] = useState("");
  const [deploying, setDeploying] = useState(false);
  const [error, setError] = useState("");
  // Collapsible options for the unified source card: env vars + optional project
  // name + optional container port (port applies only to a container-image source).
  const [urlEnvOpen, setUrlEnvOpen] = useState(false);
  const [urlEnvRows, setUrlEnvRows] = useState<{ k: string; v: string }[]>([{ k: "", v: "" }]);
  const [name, setName] = useState("");
  const [port, setPort] = useState("");
  const [gh, setGh] = useState<{ configured: boolean; connected: boolean }>({ configured: false, connected: false });
  const [repos, setRepos] = useState<GhRepo[]>([]);
  const [repoQ, setRepoQ] = useState("");
  const [selected, setSelected] = useState<Template | null>(null);
  const [tplPage, setTplPage] = useState(0);
  // Set once Create succeeds: drives the "Preparing Git Repository" animation
  // shown between Create and the build-logs view.
  const [preparing, setPreparing] = useState<{ template: Template | null; team: string; src: string; dest: string } | null>(null);

  const TPL_PER = 5;
  const tplPages = Math.max(1, Math.ceil(TEMPLATES.length / TPL_PER));
  const shownTemplates = TEMPLATES.slice(tplPage * TPL_PER, tplPage * TPL_PER + TPL_PER);

  useEffect(() => {
    cachedJson<{ configured: boolean; connected: boolean }>("/api/github/status", 30_000).then((s) => {
      setGh({ configured: !!s.configured, connected: !!s.connected });
      if (s.connected) {
        // Repos cached for 5 min so re-opening "New Project" is instant.
        cachedJson<{ repos: GhRepo[] }>("/api/github/repos", 5 * 60_000).then((d) => setRepos(d.repos || [])).catch(() => {});
      }
    }).catch(() => {});
  }, []);

  // Build the env map for the quick git-URL deploy from its editor rows.
  function urlEnv(): Record<string, string> {
    const out: Record<string, string> = {};
    for (const r of urlEnvRows) if (r.k.trim()) out[r.k.trim()] = r.v;
    return out;
  }
  // Paste a `.env` blob into a KEY field → expand into rows.
  function onUrlEnvPaste(e: React.ClipboardEvent, idx: number) {
    const text = e.clipboardData.getData("text");
    if (!text.includes("\n") && !text.includes("=")) return;
    e.preventDefault();
    const parsed = text
      .split("\n").map((l) => l.trim())
      .filter((l) => l && !l.startsWith("#") && l.includes("="))
      .map((l) => { const i = l.indexOf("="); return { k: l.slice(0, i).trim(), v: l.slice(i + 1).trim().replace(/^["']|["']$/g, "") }; });
    if (!parsed.length) return;
    setUrlEnvRows((cur) => { const next = [...cur]; next.splice(idx, 1, ...parsed); return next.filter((r) => r.k || r.v).concat({ k: "", v: "" }); });
  }

  async function deploy(repoUrl: string, branch?: string, project?: string, root?: string, env?: Record<string, string>, template?: Template) {
    if (!repoUrl) return;
    setDeploying(true);
    setError("");
    try {
      const res = await apiSend<{ build_id: string }>("POST", "/v1/git/deploy", {
        repo_url: repoUrl,
        branch,
        project,
        root_dir: root,
        creator: "you",
        env: env && Object.keys(env).length ? env : undefined,
      });
      // Install a real GitHub webhook (push + pull_request) on the source repo so
      // future pushes auto-deploy and PRs / non-prod branches get PREVIEW deploys
      // (falls back to an Actions workflow; no-ops if GitHub isn't connected).
      fetch("/api/gitops/project-ci", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ repo: repoUrl }),
      }).catch(() => {});
      // Show the "Preparing Git Repository" clone animation, then transition to
      // the live build logs. The build is already running async on the node.
      const src = ownerRepo(repoUrl) + (root ? `/${root}` : "");
      const fallbackName = slug(template?.name || ownerRepo(repoUrl).split("/").pop() || "project");
      const dest = `${GIT_SCOPE}/${project || fallbackName}`;
      // Persist the in-flight build so it shows as "Building" in the deployments
      // lists if the user navigates there before the build finishes.
      addPendingBuild({ id: res.build_id, project: project || fallbackName, team: currentTeam(), env: "production" });
      setPreparing({ template: template ?? null, team: currentTeam(), src, dest });
      setTimeout(() => router.push(`/deploy/${res.build_id}`), PREPARING_MS);
    } catch (e) {
      setError(String(e));
      setDeploying(false);
    }
  }

  // Classify the unified source input. It's a CONTAINER IMAGE / registry reference
  // UNLESS it clearly looks like a git URL (a scheme, a known git host, or `.git`).
  // So `fruitbox12/simplifi:latest`, `quay.io/org/img:tag`, `nginx`, `docker.io/...`
  // are images; `https://github.com/owner/repo(.git)`, `git@…`, `ssh://…` are git.
  function isImageRef(s: string): boolean {
    const v = s.trim();
    if (!v) return false;
    if (/^image:\/\//i.test(v)) return true;
    if (/^(https?:\/\/|git@|ssh:\/\/|git:\/\/)/i.test(v)) return false;
    if (/\.git($|[?#])/i.test(v)) return false;
    if (/(github\.com|gitlab\.com|bitbucket\.org|dev\.azure\.com|sourcehut\.org)/i.test(v)) return false;
    return true;
  }

  // Unified submit for the source card. Routes a git URL through the git build
  // pipeline (clone → build → deploy), or a container image ref through the
  // registry-image pipeline (CREATE PROJECT → build (pull the image) → DEPLOY) —
  // a first-class deployment, NOT a bare container run. Both land on the same
  // build-logs flow.
  async function submit() {
    const v = url.trim();
    if (!v || deploying) return;
    if (isImageRef(v)) {
      await deployFromImage(v);
    } else {
      await deploy(v, undefined, name.trim() ? slug(name) : undefined, undefined, urlEnv());
    }
  }

  // Create a project + build + deployment FROM a registry image. The backend
  // (/v1/deploy/image → start_named_deploy) creates the project, runs a build that
  // pulls the image + auto-detects the port (or uses the override) + attaches a
  // persistent ≥1 GB volume + injects env, and registers a real deployment.
  async function deployFromImage(ref: string) {
    setDeploying(true);
    setError("");
    const env = urlEnv();
    const p = parseInt(port, 10);
    try {
      const res = await apiSend<{ build_id: string; project: string }>("POST", "/v1/deploy/image", {
        image: ref,
        creator: "you",
        project: name.trim() ? slug(name) : undefined,
        port: Number.isFinite(p) && p > 0 ? p : undefined,
        env: Object.keys(env).length ? env : undefined,
      });
      const guessed = slug(ref.split("/").pop()?.split(":")[0] || "app");
      const proj = res.project || (name.trim() ? slug(name) : guessed);
      addPendingBuild({ id: res.build_id, project: proj, team: currentTeam(), env: "production" });
      setPreparing({ template: null, team: currentTeam(), src: ref, dest: `${GIT_SCOPE}/${proj}` });
      setTimeout(() => router.push(`/deploy/${res.build_id}`), PREPARING_MS);
    } catch (e) {
      setError(String(e));
      setDeploying(false);
    }
  }

  async function connectGithub() {
    const r = await fetch("/api/github/connect", { method: "POST" });
    const d = await r.json();
    if (d.redirectUrl) window.location.href = d.redirectUrl;
    else setError(d.error || "Could not start GitHub connection");
  }

  const filteredRepos = repos.filter((r) => r.full_name.toLowerCase().includes(repoQ.toLowerCase()));

  // ----- Preparing Git Repository (after Create, before build logs) -----
  if (preparing) {
    return <PreparingDeployment template={preparing.template} team={preparing.team} src={preparing.src} dest={preparing.dest} />;
  }

  // ----- Configure screen (after a template is selected) -----
  if (selected) {
    return <ConfigureTemplate template={selected} onBack={() => setSelected(null)} onCreate={(name, env) => deploy(selected.repo, selected.branch, name, selected.root, env, selected)} deploying={deploying} error={error} />;
  }

  return (
    <div className="mx-auto max-w-4xl">
      <Link href="/" className="mb-6 inline-flex items-center gap-2 text-sm text-secondary hover:text-fg">
        <ArrowLeft className="h-4 w-4" /> Back
      </Link>
      <h1 className="mb-6 text-2xl sm:text-3xl font-semibold tracking-tight">Let&apos;s build something new</h1>

      {/* Unified source bar — a Git repository URL OR a container image / registry
          reference. The source type is auto-detected on submit and routed to the
          matching build → deploy pipeline. */}
      <Card className="mb-2 p-2">
        <div className="flex items-center gap-2">
          <Input
            placeholder="Git repository URL or container image… e.g. github.com/owner/repo, fruitbox12/simplifi:latest, quay.io/org/img:tag"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
            className="border-0 focus:ring-0"
          />
          <Button onClick={() => submit()} disabled={deploying || !url}>
            {deploying ? <Loader2 className="h-4 w-4 animate-spin" /> : "Deploy"}
          </Button>
        </div>
        {url.trim() ? (
          <p className="px-2 pt-1 text-xs text-muted">
            Detected source: <span className="font-medium text-secondary">{isImageRef(url) ? "container image (build → deploy from registry)" : "Git repository (clone → build → deploy)"}</span>
          </p>
        ) : null}
        {/* Options: env vars + optional project name + optional container port. */}
        <button
          type="button"
          onClick={() => setUrlEnvOpen((o) => !o)}
          className="mt-1 flex w-full items-center gap-1.5 px-2 py-1.5 text-left text-xs text-secondary hover:text-fg"
        >
          <ChevronDown className={cn("h-3.5 w-3.5 transition-transform", urlEnvOpen ? "" : "-rotate-90")} />
          Environment Variables &amp; Options{(() => { const n = Object.keys(urlEnv()).length; return n ? ` (${n})` : ""; })()}
        </button>
        {urlEnvOpen && (
          <div className="space-y-2 px-2 pb-2">
            <div className="flex items-center gap-2">
              <Input className="flex-1 text-xs" placeholder="Project name (optional — derived from the source)"
                value={name} onChange={(e) => setName(e.target.value)} />
              <Input className="w-44 text-xs" placeholder="Container port (image only)"
                value={port} onChange={(e) => setPort(e.target.value.replace(/[^0-9]/g, ""))} />
            </div>
            {urlEnvRows.map((row, i) => (
              <div key={i} className="flex items-center gap-2">
                <Input className="flex-1 font-mono text-xs" placeholder="KEY" value={row.k}
                  onPaste={(e) => onUrlEnvPaste(e, i)}
                  onChange={(e) => setUrlEnvRows((c) => c.map((r, j) => (j === i ? { ...r, k: e.target.value } : r)))} />
                <Input className="flex-1 font-mono text-xs" placeholder="value" value={row.v}
                  onChange={(e) => setUrlEnvRows((c) => c.map((r, j) => (j === i ? { ...r, v: e.target.value } : r)))} />
                <button type="button" className="text-muted hover:text-fg" onClick={() => setUrlEnvRows((c) => { const n = c.filter((_, j) => j !== i); return n.length ? n : [{ k: "", v: "" }]; })}>
                  <X className="h-4 w-4" />
                </button>
              </div>
            ))}
            <Button variant="outline" onClick={() => setUrlEnvRows((c) => [...c, { k: "", v: "" }])}>
              <Plus className="h-3.5 w-3.5" /> Add Variable
            </Button>
            <p className="text-xs text-muted">
              Env is applied to the build/runtime. A container-image project gets a persistent ≥1&nbsp;GB volume at{" "}
              <span className="font-mono">/data</span>. Tip: paste a .env file into a KEY field to import many at once.
            </p>
          </div>
        )}
      </Card>
      <p className="mb-8 text-center text-sm text-muted">
        Paste a Git repo URL, or a container image from Docker Hub / Quay / any registry — OpenEdge
        creates the project and builds → deploys it (clone &amp; build, or pull the image).{" "}
        <Link href="/new/upload" className="text-secondary underline decoration-dotted underline-offset-2 hover:text-fg">
          No repository? Upload a .zip instead
        </Link>
        .
      </p>
      {error ? <p className="mb-6 text-center text-sm text-red-600">{error}</p> : null}

      <div className="grid grid-cols-1 gap-8 md:grid-cols-2">
        {/* Import Git Repository */}
        <div>
          <h2 className="mb-4 text-lg font-semibold">Import Git Repository</h2>
          <Card className="p-4">
            {gh.connected ? (
              <>
                <div className="relative mb-3">
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
                      <Button onClick={() => { setError(""); setSelected(repoToTemplate(r)); }} disabled={deploying}>
                        Import
                      </Button>
                    </div>
                  ))}
                  {!filteredRepos.length && <div className="py-6 text-center text-sm text-secondary">No repositories.</div>}
                </div>
              </>
            ) : (
              <div className="flex flex-col items-center gap-3 py-8 text-center">
                <Github className="h-8 w-8" />
                <div className="font-medium">Connect GitHub</div>
                <p className="max-w-xs text-sm text-secondary">
                  {gh.configured
                    ? "Authorize GitHub to import and deploy your repositories."
                    : "Set COMPOSIO_API_KEY to enable multi-tenant GitHub OAuth. You can still deploy any public repo by URL above."}
                </p>
                {gh.configured && (
                  <Button onClick={connectGithub}>
                    <Github className="h-4 w-4" /> Connect GitHub
                  </Button>
                )}
              </div>
            )}
          </Card>
        </div>

        {/* Clone Template */}
        <div>
          <div className="mb-4 flex items-center justify-between">
            <h2 className="text-lg font-semibold">Clone Template</h2>
            {tplPages > 1 && (
              <div className="flex items-center gap-1.5 text-xs text-muted">
                <button
                  onClick={() => setTplPage((p) => Math.max(0, p - 1))}
                  disabled={tplPage === 0}
                  className="rounded border border-border px-1.5 py-0.5 hover:bg-subtle disabled:opacity-40"
                >‹</button>
                <span className="tabular-nums">{tplPage + 1}/{tplPages}</span>
                <button
                  onClick={() => setTplPage((p) => Math.min(tplPages - 1, p + 1))}
                  disabled={tplPage >= tplPages - 1}
                  className="rounded border border-border px-1.5 py-0.5 hover:bg-subtle disabled:opacity-40"
                >›</button>
              </div>
            )}
          </div>
          <Card className="divide-y divide-border p-0">
            {shownTemplates.map((t) => (
              <div key={t.name} className="flex items-center justify-between gap-3 px-3 py-2.5">
                <div className="flex min-w-0 items-center gap-2.5">
                  <Monogram t={t} className="h-8 w-8" />
                  <div className="min-w-0">
                    <div className="truncate text-sm font-medium">{t.name}</div>
                    <div className="truncate text-xs text-secondary">{t.desc}</div>
                  </div>
                </div>
                <Button variant="outline" className="px-2.5 py-1 text-xs" onClick={() => { setError(""); setSelected(t); }} disabled={deploying}>
                  Use
                </Button>
              </div>
            ))}
          </Card>
        </div>
      </div>

      {/* Deployment progress placeholder */}
      <Card className="mt-8 overflow-hidden p-6 sm:p-8">
        <h2 className="text-2xl font-semibold tracking-tight">Deployment</h2>
        <p className="mt-1.5 text-sm text-secondary">
          {deploying ? "Starting your deployment…" : "Once you're ready, start deploying to see the progress here…"}
        </p>
        <div className="pointer-events-none mt-6 flex justify-center">
          <GlobeEmptyState title="" desc={undefined} />
        </div>
      </Card>
    </div>
  );
}

/** The Vercel-style "New Project" configure screen shown after picking a template. */
function ConfigureTemplate({
  template,
  onBack,
  onCreate,
  deploying,
  error,
}: {
  template: Template;
  onBack: () => void;
  onCreate: (projectName: string, env: Record<string, string>) => void;
  deploying: boolean;
  error: string;
}) {
  const [repoName, setRepoName] = useState(slug(template.name));
  // Environment variables to inject into the build + runtime (collapsible).
  const [envOpen, setEnvOpen] = useState(false);
  const [envVars, setEnvVars] = useState<{ key: string; value: string }[]>([{ key: "", value: "" }]);
  const setEnvAt = (i: number, patch: Partial<{ key: string; value: string }>) =>
    setEnvVars((rows) => rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const addEnvRow = () => setEnvVars((rows) => [...rows, { key: "", value: "" }]);
  const removeEnvRow = (i: number) => setEnvVars((rows) => (rows.length <= 1 ? [{ key: "", value: "" }] : rows.filter((_, j) => j !== i)));
  // Paste a `.env` blob: split KEY=VALUE lines into rows.
  const pasteDotenv = (text: string) => {
    const parsed = text
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith("#") && l.includes("="))
      .map((l) => {
        const idx = l.indexOf("=");
        let value = l.slice(idx + 1).trim();
        if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) value = value.slice(1, -1);
        return { key: l.slice(0, idx).trim().replace(/^export\s+/, ""), value };
      })
      .filter((r) => r.key);
    if (parsed.length) setEnvVars((rows) => [...rows.filter((r) => r.key.trim()), ...parsed]);
  };
  const envCount = envVars.filter((r) => r.key.trim()).length;
  const buildEnv = () => {
    const out: Record<string, string> = {};
    for (const { key, value } of envVars) {
      const k = key.trim();
      if (k) out[k] = value;
    }
    return out;
  };
  // Default the Team field to the team/org the user is CURRENTLY viewing (the
  // navbar breadcrumb selection) instead of always "personal", and keep it in
  // sync if they switch the active team while this screen is open.
  const [team, setTeam] = useState<string>(() => currentTeam());
  useEffect(() => {
    const sync = () => setTeam(currentTeam());
    window.addEventListener("hive-team-changed", sync);
    return () => window.removeEventListener("hive-team-changed", sync);
  }, []);
  const [scope] = useState("openedge");

  return (
    <div className="mx-auto max-w-2xl">
      <button onClick={onBack} className="mb-6 inline-flex items-center gap-2 text-sm text-secondary hover:text-fg">
        <ArrowLeft className="h-4 w-4" /> Back
      </button>

      <Card className="p-6 sm:p-8">
        <h1 className="mb-6 text-2xl font-semibold tracking-tight">New Project</h1>

        {/* Template summary */}
        <div className="mb-6 flex items-start gap-4 rounded-xl border border-border bg-subtle/40 p-4">
          <Monogram t={template} />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-1.5 font-semibold">
              {template.name}
              <a href={`${template.repo}${template.root ? `/tree/${template.branch || "main"}/${template.root}` : ""}`} target="_blank" rel="noreferrer">
                <ExternalLink className="h-3.5 w-3.5 text-muted hover:text-fg" />
              </a>
            </div>
            <div className="mt-0.5 text-sm text-secondary">{template.desc}</div>
            <div className="mt-3 text-xs text-muted">Cloning from GitHub</div>
            <div className="mt-1 flex flex-wrap items-center gap-x-4 gap-y-1 text-sm">
              <span className="flex items-center gap-1.5"><Github className="h-3.5 w-3.5" /> {ownerRepo(template.repo)}</span>
              <span className="flex items-center gap-1.5 text-secondary"><GitBranch className="h-3.5 w-3.5" /> {template.branch || "main"}</span>
              {template.root && <span className="flex items-center gap-1.5 text-secondary"><FolderGit2 className="h-3.5 w-3.5" /> {template.root}</span>}
            </div>
          </div>
        </div>

        <p className="mb-6 text-sm text-secondary">
          Create a Git repository to easily update your project after deploying it. Every push to that
          Git repository will be deployed automatically.
        </p>

        <div className="mb-5 grid grid-cols-1 gap-4 sm:grid-cols-2">
          <div>
            <label className="mb-1.5 block text-sm text-secondary">Git Scope</label>
            <div className="flex items-center justify-between rounded-md border border-border bg-card px-3 py-2 text-sm">
              <span className="flex items-center gap-2"><Github className="h-4 w-4" /> {scope}</span>
              <ChevronDown className="h-4 w-4 text-muted" />
            </div>
          </div>
          <div>
            <label className="mb-1.5 block text-sm text-secondary">Project Name</label>
            <div className="relative">
              <Input value={repoName} placeholder="auto-generated if blank" onChange={(e) => setRepoName(slug(e.target.value))} />
              <Lock className="absolute right-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
            </div>
            <p className="mt-1 text-xs text-muted">Must be unique. Leave blank and we&apos;ll generate one.</p>
          </div>
        </div>

        <div className="mb-6">
          <label className="mb-1.5 block text-sm text-secondary">Team</label>
          <TeamSelect value={team} onChange={setTeam} />
          <p className="mt-1 text-xs text-muted">The project is created under this team — matches your current view.</p>
        </div>

        {/* Environment Variables (collapsible) — injected into the build & runtime. */}
        <div className="mb-6 rounded-md border border-border">
          <button
            type="button"
            onClick={() => setEnvOpen((o) => !o)}
            className="flex w-full items-center justify-between px-3 py-2.5 text-sm"
          >
            <span className="flex items-center gap-2 font-medium">
              <KeyRound className="h-4 w-4 text-muted" /> Environment Variables
              {envCount > 0 && (
                <span className="rounded-full bg-subtle px-2 py-0.5 text-xs text-secondary">{envCount}</span>
              )}
            </span>
            <ChevronDown className={cn("h-4 w-4 text-muted transition-transform", envOpen ? "" : "-rotate-90")} />
          </button>

          {envOpen && (
            <div className="border-t border-border p-3">
              <p className="mb-3 text-xs text-muted">
                Set keys available during the build (e.g. <span className="font-mono">NEXT_PUBLIC_*</span>,{" "}
                <span className="font-mono">VITE_*</span>) and at runtime. Pasting a <span className="font-mono">.env</span> into any
                key field expands it into rows.
              </p>
              <div className="flex flex-col gap-2">
                {envVars.map((row, i) => (
                  <div key={i} className="flex items-center gap-2">
                    <Input
                      value={row.key}
                      placeholder="KEY"
                      onChange={(e) => setEnvAt(i, { key: e.target.value })}
                      onPaste={(e) => {
                        const t = e.clipboardData.getData("text");
                        if (t.includes("=") && (t.includes("\n") || /^\s*export\s/.test(t))) {
                          e.preventDefault();
                          pasteDotenv(t);
                        }
                      }}
                      className="flex-1 font-mono text-xs"
                    />
                    <Input
                      value={row.value}
                      placeholder="value"
                      onChange={(e) => setEnvAt(i, { value: e.target.value })}
                      className="flex-1 font-mono text-xs"
                    />
                    <button
                      type="button"
                      onClick={() => removeEnvRow(i)}
                      className="shrink-0 rounded-md p-2 text-muted hover:bg-subtle hover:text-fg"
                      aria-label="Remove variable"
                    >
                      <X className="h-4 w-4" />
                    </button>
                  </div>
                ))}
              </div>
              <button
                type="button"
                onClick={addEnvRow}
                className="mt-3 inline-flex items-center gap-1.5 rounded-md border border-border px-2.5 py-1.5 text-xs text-secondary hover:bg-subtle"
              >
                <Plus className="h-3.5 w-3.5" /> Add Variable
              </button>
            </div>
          )}
        </div>

        {error ? <p className="mb-4 text-sm text-red-600">{error}</p> : null}

        <Button
          onClick={() => {
            // Scope the new project to the chosen team (drives x-hive-team).
            if (typeof window !== "undefined") {
              localStorage.setItem("hive_team", team === "personal" ? "__personal__" : team);
              window.dispatchEvent(new Event("hive-team-changed"));
            }
            onCreate(repoName || slug(template.name), buildEnv());
          }}
          disabled={deploying}
          className="w-full justify-center bg-fg py-2.5 text-bg"
        >
          {deploying ? <><Loader2 className="h-4 w-4 animate-spin" /> Creating…</> : "Create"}
        </Button>
      </Card>

      <div className="mt-6 text-center">
        <Link href="/new" onClick={onBack} className="text-sm text-secondary hover:text-fg">Import a different Git Repository →</Link>
      </div>
    </div>
  );
}
