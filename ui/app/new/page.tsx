"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { ArrowLeft, Github, Search, Loader2, GitBranch, FolderGit2, Lock, ExternalLink, ChevronDown, Plus, X, KeyRound, AlertTriangle, RefreshCw } from "lucide-react";
import Link from "next/link";
import { Card, Button, Input, Badge } from "@/components/ui";
import { GlobeEmptyState } from "@/components/globe";
import { apiSend, apiDeployViaServerRoute, currentTeam, switchTeam } from "@/lib/api";
import { addPendingBuild } from "@/lib/pending-builds";
import { TeamSelect } from "@/components/team-picker";
import { cachedJson } from "@/lib/cache";
import { cn } from "@/lib/utils";
import { PreparingDeployment } from "@/components/clone-animation";
import Image from "next/image";

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

/** Enriched GitHub status (subset of githubConnectionDetail) used to decide the
 *  import panel state + whether to prompt reconnect / org-approval. */
interface GhDetail {
  configured: boolean;
  connected: boolean;
  scopes?: string[];
  hasPrivateAccess?: boolean;
  hasOrgScope?: boolean;
  live?: boolean;
}

interface Template {
  name: string;
  desc: string;
  tag: string; // framework label for the monogram fallback
  color: string;
  icon?: string; // real logo path (in /public/frameworks)
  // Git-clone template: repo URL (+ optional monorepo subdir/branch). Exactly
  // one of `repo` or `image` is set per entry — never both.
  repo?: string;
  root?: string;
  branch?: string;
  // Pre-built container-image template — skips clone + build entirely (the
  // `/v1/deploy/image` path). `port`/`protocol`/`env`/`memory` seed the
  // configure screen so a game server or database needs no manual port
  // entry, while staying fully editable there (real port-mapping support,
  // not a fixed/hidden value).
  image?: string;
  port?: number;
  protocol?: string;
  env?: Record<string, string>;
  memory?: string;
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
  // Pre-built image, not a git clone — see `examples/minecraft-server/` in
  // this repo for the equivalent compose.yaml and the raw-TCP/`/tcp`-suffix
  // explanation. Real Java Edition server (itzg/minecraft-server), a
  // world-standard, actively-maintained public image — EULA=TRUE is
  // Mojang's own required acceptance flag, not a platform invention.
  { name: "Minecraft Server", desc: "Java Edition server (itzg/minecraft-server) with a persistent world, raw TCP.", image: "itzg/minecraft-server:latest", port: 25565, protocol: "tcp", env: { EULA: "TRUE" }, memory: "3g", tag: "MC", color: "#5b8c3e" },
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
        <Image src={t.icon} alt={t.name} width={30} height={30} unoptimized className="h-3/4 w-3/4 object-contain" />
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
  // Protocol override + resource overrides (container-image source only) — the
  // backend's ImageDeployReq accepts these as independent optional overrides;
  // "" means "let the backend auto-detect / use its default". A raw-protocol
  // image (Postgres, Minecraft, …) needs `protocol` set explicitly since
  // auto-detection assumes HTTP, and a UDP-only image (e.g. Minecraft Bedrock,
  // 19132/udp) has no TCP port for auto-detection to find at all.
  const [protocol, setProtocol] = useState("");
  const [memory, setMemory] = useState("");
  const [cpus, setCpus] = useState("");
  // Extra ports beyond the primary (image source only) — a service that needs
  // more than one raw port (e.g. a game server's play + query/RCON ports).
  // Sent as the full `ports` list (primary + extras) only when at least one
  // extra row is filled in; otherwise the request stays exactly as it was
  // (plain port/protocol), matching the backend's replace-only-when-non-empty
  // semantics for ImageDeployReq.ports.
  const [extraPorts, setExtraPorts] = useState<{ port: string; protocol: string; label: string }[]>([]);
  const [gh, setGh] = useState<GhDetail>({ configured: false, connected: false });
  const [repos, setRepos] = useState<GhRepo[]>([]);
  // A reconnect / org-approval prompt shown above the repo list: a dead-but-ACTIVE
  // token, a connection lacking read:org, or an org restricting the OAuth app.
  const [ghCta, setGhCta] = useState<{ text: string; approveUrl?: string } | null>(null);
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
    cachedJson<GhDetail>("/api/github/status", 30_000).then(async (s) => {
      setGh({ ...s, configured: !!s.configured, connected: !!s.connected });
      // Dead-but-ACTIVE token (ACTIVE in Composio but revoked on GitHub): scopes are
      // known yet the connection isn't live → prompt a reconnect, not a false green.
      if (s.configured && !s.connected && (s.scopes?.length ?? 0) > 0) {
        setGhCta({ text: "Your GitHub authorization is no longer valid. Reconnect to import your repositories." });
        return;
      }
      if (!s.connected) return;
      // Personal repos (cached 5 min so re-opening "New Project" is instant).
      const personal = await cachedJson<{ repos: GhRepo[] }>("/api/github/repos", 5 * 60_000)
        .then((d) => d.repos || [])
        .catch(() => [] as GhRepo[]);
      const merged: GhRepo[] = [...personal];
      const seen = new Set(personal.map((r) => r.full_name));
      // Only merge org repos when we can enumerate them (read:org). Absent scope is
      // NOT nagged here — private-repo import already works; org access is offered in
      // Integrations / the GitOps modal instead (non-disruptive per edge policy).
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
          // Surface the FIRST restricting org's approval link (only an org actually
          // blocking the app warrants a CTA — a live, working connection stays quiet).
          if (res?.restricted && res?.approve_url && !restrictedCta) {
            restrictedCta = { text: "An organization restricts DevHub. Approve the app to import its repositories.", approveUrl: res.approve_url };
          }
          for (const r of (res?.repos as GhRepo[]) || []) {
            if (r?.full_name && !seen.has(r.full_name)) { seen.add(r.full_name); merged.push(r); }
          }
        }
        if (restrictedCta) setGhCta(restrictedCta);
      }
      setRepos(merged);
    }).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
      // Via the server route so a PRIVATE github repo gets the user's GitHub token
      // attached server-side (never in the browser) for the clone.
      const res = await apiDeployViaServerRoute<{ build_id: string }>("/api/git/deploy", {
        repo_url: repoUrl,
        branch,
        project,
        root_dir: root,
        creator: "you",
        env: env && Object.keys(env).length ? env : undefined,
      });
      // Show the "Preparing Git Repository" clone animation, then transition to
      // the live build logs. The build is already running async on the node.
      const src = ownerRepo(repoUrl) + (root ? `/${root}` : "");
      const fallbackName = slug(template?.name || ownerRepo(repoUrl).split("/").pop() || "project");
      const dest = `${GIT_SCOPE}/${project || fallbackName}`;
      // Install a real GitHub webhook (push + pull_request) on the source repo so
      // future pushes auto-deploy and PRs / non-prod branches get PREVIEW deploys
      // (falls back to an Actions workflow; no-ops if GitHub isn't connected).
      // Previously fire-and-forget: a project imported without a completed GitHub
      // OAuth connection silently got neither installed, so no future push ever
      // auto-deployed, with zero visible error anywhere. Now the outcome is
      // awaited and persisted so the dashboard can surface the gap + offer a
      // retry — this call must not block navigation to the build logs, so it
      // runs but is never awaited before the redirect below.
      fetch("/api/gitops/project-ci", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ repo: repoUrl }),
      })
        .then((r) => r.json())
        .then((j: { ok?: boolean; skipped?: boolean; reason?: string; error?: string; webhookInstalled?: boolean; workflowInstalled?: boolean }) =>
          apiSend("PUT", `/v1/projects/${project || fallbackName}/git-ci`, {
            webhook_installed: !!j.webhookInstalled,
            workflow_installed: !!j.workflowInstalled,
            // Loud failures (502 {ok:false,error}) are recorded verbatim so the
            // git-settings CI card shows the REAL reason, not a blank "not
            // configured" — a silent no-op here is how pushes stop deploying.
            skipped_reason: j.skipped
              ? j.reason || "unknown"
              : j.webhookInstalled || j.workflowInstalled
              ? ""
              : j.error || j.reason || "install-failed",
            checked_ms: 0,
          }).catch(() => {}),
        )
        .catch(() =>
          apiSend("PUT", `/v1/projects/${project || fallbackName}/git-ci`, {
            webhook_installed: false,
            workflow_installed: false,
            skipped_reason: "request-failed",
            checked_ms: 0,
          }).catch(() => {}),
        );
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
  // Shared core: both the unified source bar's free-form image deploy AND an
  // image-based template's configure screen (Minecraft, etc.) POST through
  // here — one place that actually calls /v1/deploy/image, so the two entry
  // points can never drift on what fields they send.
  async function runImageDeploy(opts: {
    image: string;
    project?: string;
    port?: number;
    protocol?: string;
    memory?: string;
    cpus?: string;
    ports?: { container_port: number; protocol: string; label?: string }[];
    env?: Record<string, string>;
    template?: Template | null;
  }) {
    setDeploying(true);
    setError("");
    try {
      const res = await apiSend<{ build_id: string; project: string }>("POST", "/v1/deploy/image", {
        image: opts.image,
        creator: "you",
        project: opts.project,
        port: opts.port,
        protocol: opts.protocol,
        memory: opts.memory,
        cpus: opts.cpus,
        ports: opts.ports,
        env: opts.env && Object.keys(opts.env).length ? opts.env : undefined,
      });
      const guessed = slug(opts.image.split("/").pop()?.split(":")[0] || "app");
      const proj = res.project || opts.project || guessed;
      addPendingBuild({ id: res.build_id, project: proj, team: currentTeam(), env: "production" });
      setPreparing({ template: opts.template ?? null, team: currentTeam(), src: opts.image, dest: `${GIT_SCOPE}/${proj}` });
      setTimeout(() => router.push(`/deploy/${res.build_id}`), PREPARING_MS);
    } catch (e) {
      setError(String(e));
      setDeploying(false);
    }
  }

  async function deployFromImage(ref: string) {
    const env = urlEnv();
    const p = parseInt(port, 10);
    const filledExtras = extraPorts.filter((r) => r.port.trim());
    const ports = filledExtras.length
      ? [
          { container_port: p, protocol: protocol || "http", label: undefined },
          ...filledExtras.map((r) => ({
            container_port: parseInt(r.port, 10),
            protocol: r.protocol || "tcp",
            label: r.label.trim() || undefined,
          })),
        ].filter((s) => Number.isFinite(s.container_port) && s.container_port > 0)
      : undefined;
    await runImageDeploy({
      image: ref,
      project: name.trim() ? slug(name) : undefined,
      port: Number.isFinite(p) && p > 0 ? p : undefined,
      protocol: protocol || undefined,
      memory: memory.trim() || undefined,
      cpus: cpus.trim() || undefined,
      ports,
      env,
    });
  }

  async function connectGithub() {
    const r = await fetch("/api/github/connect", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ returnTo: "/new" }) });
    const d = await r.json();
    if (d.redirectUrl) window.location.href = d.redirectUrl;
    else setError(d.error || "Could not start GitHub connection");
  }

  // Drop the current connection then re-run OAuth so re-consent grants the current
  // scopes (fixes a dead token, or grants read:org for organization repos).
  async function reconnectGithub() {
    await fetch("/api/github/disconnect", { method: "POST" }).catch(() => {});
    await connectGithub();
  }

  const filteredRepos = repos.filter((r) => r.full_name.toLowerCase().includes(repoQ.toLowerCase()));

  // ----- Preparing Git Repository (after Create, before build logs) -----
  if (preparing) {
    return <PreparingDeployment template={preparing.template} team={preparing.team} src={preparing.src} dest={preparing.dest} />;
  }

  // ----- Configure screen (after a template is selected) -----
  if (selected) {
    return selected.image ? (
      <ConfigureImageTemplate
        template={selected}
        onBack={() => setSelected(null)}
        onCreate={(opts) => runImageDeploy({ ...opts, image: selected.image!, template: selected })}
        deploying={deploying}
        error={error}
      />
    ) : (
      <ConfigureTemplate template={selected as Template & { repo: string }} onBack={() => setSelected(null)} onCreate={(name, env) => deploy(selected.repo!, selected.branch, name, selected.root, env, selected)} deploying={deploying} error={error} />
    );
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
            </div>
            {/* Container port + protocol (image source only) — a raw-protocol
                service (Postgres, Minecraft, …) has no Host header to route on
                and gets its OWN public port instead of the shared HTTP
                gateway; `protocol` also needs an explicit override for a
                UDP-only image (e.g. Minecraft Bedrock, 19132/udp — there's no
                TCP port for auto-detection to find). */}
            <div className="flex items-center gap-2">
              <Input className="w-32 text-xs" placeholder="Port (image only)"
                value={port} onChange={(e) => setPort(e.target.value.replace(/[^0-9]/g, ""))} />
              <select
                value={protocol}
                onChange={(e) => setProtocol(e.target.value)}
                title="Protocol (image only) — required for a raw TCP/UDP/gRPC service like a database or game server"
                className="rounded-md border border-border bg-card px-2 py-1.5 text-xs"
              >
                <option value="">Auto-detect (HTTP)</option>
                <option value="http">HTTP</option>
                <option value="https">HTTPS</option>
                <option value="ws">WebSocket</option>
                <option value="wss">WebSocket (TLS)</option>
                <option value="grpc">gRPC</option>
                <option value="tcp">Raw TCP (databases, game servers)</option>
                <option value="udp">Raw UDP (e.g. Minecraft Bedrock)</option>
              </select>
              <Input className="w-28 text-xs" placeholder="Memory (e.g. 4g)"
                value={memory} onChange={(e) => setMemory(e.target.value)} />
              <Input className="w-24 text-xs" placeholder="CPUs (e.g. 2.0)"
                value={cpus} onChange={(e) => setCpus(e.target.value)} />
            </div>
            {protocol === "tcp" || protocol === "udp" ? (
              <p className="text-xs text-muted">
                A raw {protocol.toUpperCase()} service gets its own public port (shown on the deployment page once
                built) — clients connect with a plain <span className="font-mono">host:port</span> address, the
                same format a Minecraft client&apos;s &quot;Server Address&quot; field or a database connection
                string expects.
              </p>
            ) : null}
            {/* Extra ports beyond the primary — a service that needs more than
                one (e.g. a game server's play + query/RCON ports). */}
            {extraPorts.map((row, i) => (
              <div key={i} className="flex items-center gap-2">
                <Input className="w-32 text-xs" placeholder="Extra port"
                  value={row.port}
                  onChange={(e) => setExtraPorts((c) => c.map((r, j) => (j === i ? { ...r, port: e.target.value.replace(/[^0-9]/g, "") } : r)))} />
                <select
                  value={row.protocol}
                  onChange={(e) => setExtraPorts((c) => c.map((r, j) => (j === i ? { ...r, protocol: e.target.value } : r)))}
                  className="rounded-md border border-border bg-card px-2 py-1.5 text-xs"
                >
                  <option value="tcp">Raw TCP</option>
                  <option value="udp">Raw UDP</option>
                  <option value="grpc">gRPC</option>
                  <option value="http">HTTP</option>
                </select>
                <Input className="flex-1 text-xs" placeholder="Label (optional, e.g. rcon)"
                  value={row.label}
                  onChange={(e) => setExtraPorts((c) => c.map((r, j) => (j === i ? { ...r, label: e.target.value } : r)))} />
                <button type="button" className="text-muted hover:text-fg"
                  onClick={() => setExtraPorts((c) => c.filter((_, j) => j !== i))}>
                  <X className="h-4 w-4" />
                </button>
              </div>
            ))}
            <Button variant="outline" onClick={() => setExtraPorts((c) => [...c, { port: "", protocol: "tcp", label: "" }])}>
              <Plus className="h-3.5 w-3.5" /> Add another port
            </Button>
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
        Paste a Git repo URL, or a container image from Docker Hub / Quay / any registry — DevHub
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
                {/* Non-blocking org/reconnect prompt (personal + known-org repos still work). */}
                {ghCta ? (
                  <div className="mb-3 flex flex-col gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-600 dark:text-amber-400">
                    <span className="flex items-start gap-1.5"><AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" /> {ghCta.text}</span>
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
                <div className="font-medium">{ghCta ? "Reconnect GitHub" : "Connect GitHub"}</div>
                <p className="max-w-xs text-sm text-secondary">
                  {!gh.configured
                    ? "Set COMPOSIO_API_KEY to enable multi-tenant GitHub OAuth. You can still deploy any public repo by URL above."
                    : ghCta
                    ? ghCta.text
                    : "Authorize GitHub to import and deploy your repositories."}
                </p>
                {gh.configured && (
                  <Button onClick={ghCta ? reconnectGithub : connectGithub}>
                    {ghCta ? <RefreshCw className="h-4 w-4" /> : <Github className="h-4 w-4" />}
                    {ghCta ? "Reconnect GitHub" : "Connect GitHub"}
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
  // A git-clone template always has `repo` — only ConfigureImageTemplate
  // (routed by `selected.image` at the call site) handles an image template.
  template: Template & { repo: string };
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
          onClick={async () => {
            // Scope the new project to the chosen team. MUST go through
            // switchTeam (re-mints the hive_jwt cookie and awaits it) rather
            // than a raw localStorage.setItem — the backend derives the tenant
            // SOLELY from the cookie under JWT enforcement, so creating the
            // project before the cookie catches up landed it under whichever
            // tenant the browser's PREVIOUS cookie still claimed, while the
            // view believed it was already on the new team (the personal/org
            // project-list leak).
            if (typeof window !== "undefined") {
              await switchTeam(team === "personal" ? "__personal__" : team);
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

/** Configure screen for a pre-built container-image template (Minecraft,
 *  etc.) — the git-clone `ConfigureTemplate` above doesn't fit: there's no
 *  repo/branch to show, and the real per-template surface this needs is
 *  port + protocol + extra ports (RCON, a query port, …) + env, all
 *  pre-filled from the template but fully editable — real port mapping,
 *  not a hidden fixed value. */
function ConfigureImageTemplate({
  template,
  onBack,
  onCreate,
  deploying,
  error,
}: {
  template: Template;
  onBack: () => void;
  onCreate: (opts: {
    project?: string;
    port?: number;
    protocol?: string;
    memory?: string;
    ports?: { container_port: number; protocol: string; label?: string }[];
    env?: Record<string, string>;
  }) => void;
  deploying: boolean;
  error: string;
}) {
  const [projectName, setProjectName] = useState(slug(template.name));
  const [port, setPort] = useState(String(template.port ?? ""));
  const [protocol, setProtocol] = useState(template.protocol ?? "tcp");
  const [memory, setMemory] = useState(template.memory ?? "");
  const [extraPorts, setExtraPorts] = useState<{ port: string; protocol: string; label: string }[]>([]);
  const [envVars, setEnvVars] = useState<{ key: string; value: string }[]>(() => {
    const rows = Object.entries(template.env ?? {}).map(([key, value]) => ({ key, value }));
    return rows.length ? rows : [{ key: "", value: "" }];
  });
  const [team, setTeam] = useState<string>(() => currentTeam());
  useEffect(() => {
    const sync = () => setTeam(currentTeam());
    window.addEventListener("hive-team-changed", sync);
    return () => window.removeEventListener("hive-team-changed", sync);
  }, []);

  const buildEnv = () => {
    const out: Record<string, string> = {};
    for (const { key, value } of envVars) if (key.trim()) out[key.trim()] = value;
    return out;
  };

  return (
    <div className="mx-auto max-w-2xl">
      <button onClick={onBack} className="mb-6 inline-flex items-center gap-2 text-sm text-secondary hover:text-fg">
        <ArrowLeft className="h-4 w-4" /> Back
      </button>

      <Card className="p-6 sm:p-8">
        <h1 className="mb-6 text-2xl font-semibold tracking-tight">New Project</h1>

        <div className="mb-6 flex items-start gap-4 rounded-xl border border-border bg-subtle/40 p-4">
          <Monogram t={template} />
          <div className="min-w-0 flex-1">
            <div className="font-semibold">{template.name}</div>
            <div className="mt-0.5 text-sm text-secondary">{template.desc}</div>
            <div className="mt-2 font-mono text-xs text-muted">{template.image}</div>
          </div>
        </div>

        <p className="mb-6 text-sm text-secondary">
          A pre-built image — no build step. The platform pulls it directly and attaches a persistent
          volume at <span className="font-mono">/data</span> that survives redeploys.
        </p>

        <div className="mb-5 grid grid-cols-1 gap-4 sm:grid-cols-2">
          <div>
            <label className="mb-1.5 block text-sm text-secondary">Project Name</label>
            <Input value={projectName} placeholder="auto-generated if blank" onChange={(e) => setProjectName(slug(e.target.value))} />
          </div>
          <div>
            <label className="mb-1.5 block text-sm text-secondary">Team</label>
            <TeamSelect value={team} onChange={setTeam} />
          </div>
        </div>

        {/* Port mapping — the container port + protocol this template's
            primary service listens on. A raw TCP/UDP protocol gets its own
            dedicated public port allocated at deploy time (shown on the
            deployment page); HTTP-family protocols ride the shared
            gateway instead. */}
        <div className="mb-5">
          <label className="mb-1.5 block text-sm text-secondary">Port mapping</label>
          <div className="flex items-center gap-2">
            <Input className="w-32" placeholder="Container port" value={port} onChange={(e) => setPort(e.target.value.replace(/[^0-9]/g, ""))} />
            <select
              value={protocol}
              onChange={(e) => setProtocol(e.target.value)}
              className="rounded-md border border-border bg-card px-2 py-2 text-sm"
            >
              <option value="http">HTTP</option>
              <option value="https">HTTPS</option>
              <option value="ws">WebSocket</option>
              <option value="wss">WebSocket (TLS)</option>
              <option value="grpc">gRPC</option>
              <option value="tcp">Raw TCP (databases, game servers)</option>
              <option value="udp">Raw UDP (e.g. Minecraft Bedrock)</option>
            </select>
            <Input className="w-28" placeholder="Memory (e.g. 3g)" value={memory} onChange={(e) => setMemory(e.target.value)} />
          </div>
          {(protocol === "tcp" || protocol === "udp") && (
            <p className="mt-1.5 text-xs text-muted">
              A raw {protocol.toUpperCase()} service gets its own public <span className="font-mono">host:port</span> —
              shown on the deployment page once built.
            </p>
          )}
          {extraPorts.map((row, i) => (
            <div key={i} className="mt-2 flex items-center gap-2">
              <Input className="w-32 text-xs" placeholder="Extra port" value={row.port}
                onChange={(e) => setExtraPorts((c) => c.map((r, j) => (j === i ? { ...r, port: e.target.value.replace(/[^0-9]/g, "") } : r)))} />
              <select
                value={row.protocol}
                onChange={(e) => setExtraPorts((c) => c.map((r, j) => (j === i ? { ...r, protocol: e.target.value } : r)))}
                className="rounded-md border border-border bg-card px-2 py-1.5 text-xs"
              >
                <option value="tcp">Raw TCP</option>
                <option value="udp">Raw UDP</option>
                <option value="grpc">gRPC</option>
                <option value="http">HTTP</option>
              </select>
              <Input className="flex-1 text-xs" placeholder="Label (optional, e.g. rcon)" value={row.label}
                onChange={(e) => setExtraPorts((c) => c.map((r, j) => (j === i ? { ...r, label: e.target.value } : r)))} />
              <button type="button" className="text-muted hover:text-fg" onClick={() => setExtraPorts((c) => c.filter((_, j) => j !== i))}>
                <X className="h-4 w-4" />
              </button>
            </div>
          ))}
          <button
            type="button"
            onClick={() => setExtraPorts((c) => [...c, { port: "", protocol: "tcp", label: "" }])}
            className="mt-2 inline-flex items-center gap-1.5 rounded-md border border-border px-2.5 py-1.5 text-xs text-secondary hover:bg-subtle"
          >
            <Plus className="h-3.5 w-3.5" /> Add another port
          </button>
        </div>

        {/* Environment Variables — pre-filled from the template (e.g. this
            image's required EULA=TRUE) but fully editable/removable. */}
        <div className="mb-6">
          <label className="mb-1.5 flex items-center gap-2 text-sm text-secondary">
            <KeyRound className="h-4 w-4 text-muted" /> Environment Variables
          </label>
          <div className="flex flex-col gap-2">
            {envVars.map((row, i) => (
              <div key={i} className="flex items-center gap-2">
                <Input value={row.key} placeholder="KEY" className="flex-1 font-mono text-xs"
                  onChange={(e) => setEnvVars((rows) => rows.map((r, j) => (j === i ? { ...r, key: e.target.value } : r)))} />
                <Input value={row.value} placeholder="value" className="flex-1 font-mono text-xs"
                  onChange={(e) => setEnvVars((rows) => rows.map((r, j) => (j === i ? { ...r, value: e.target.value } : r)))} />
                <button type="button" className="shrink-0 rounded-md p-2 text-muted hover:bg-subtle hover:text-fg"
                  onClick={() => setEnvVars((rows) => (rows.length <= 1 ? [{ key: "", value: "" }] : rows.filter((_, j) => j !== i)))}>
                  <X className="h-4 w-4" />
                </button>
              </div>
            ))}
          </div>
          <button
            type="button"
            onClick={() => setEnvVars((rows) => [...rows, { key: "", value: "" }])}
            className="mt-3 inline-flex items-center gap-1.5 rounded-md border border-border px-2.5 py-1.5 text-xs text-secondary hover:bg-subtle"
          >
            <Plus className="h-3.5 w-3.5" /> Add Variable
          </button>
        </div>

        {error ? <p className="mb-4 text-sm text-red-600">{error}</p> : null}

        <Button
          onClick={async () => {
            if (typeof window !== "undefined") {
              await switchTeam(team === "personal" ? "__personal__" : team);
            }
            const p = parseInt(port, 10);
            const filledExtras = extraPorts.filter((r) => r.port.trim());
            const ports = filledExtras.length
              ? [
                  { container_port: p, protocol: protocol || "tcp", label: undefined },
                  ...filledExtras.map((r) => ({
                    container_port: parseInt(r.port, 10),
                    protocol: r.protocol || "tcp",
                    label: r.label.trim() || undefined,
                  })),
                ].filter((s) => Number.isFinite(s.container_port) && s.container_port > 0)
              : undefined;
            onCreate({
              project: projectName.trim() ? slug(projectName) : undefined,
              port: Number.isFinite(p) && p > 0 ? p : undefined,
              protocol: protocol || undefined,
              memory: memory.trim() || undefined,
              ports,
              env: buildEnv(),
            });
          }}
          disabled={deploying}
          className="w-full justify-center bg-fg py-2.5 text-bg"
        >
          {deploying ? <><Loader2 className="h-4 w-4 animate-spin" /> Creating…</> : "Create"}
        </Button>
      </Card>

      <div className="mt-6 text-center">
        <Link href="/new" onClick={onBack} className="text-sm text-secondary hover:text-fg">Choose a different template →</Link>
      </div>
    </div>
  );
}
