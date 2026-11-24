import Link from "next/link";
import type { Metadata } from "next";
import { pageMeta } from "@/lib/page-meta";
import { ArrowRight, Boxes, GitBranch, Zap, Globe2, KeyRound, Code2, Sparkles, ShieldCheck, Database, Workflow } from "lucide-react";
import { CodeBlock } from "@/components/code-block";
import { DocsCard, DocsFigure } from "@/components/docs/docs-ui";

export const metadata: Metadata = pageMeta({
  title: "Documentation",
  socialTitle: "Documentation — shadw",
  description:
    "shadw documentation: deploy serverless functions, containers, and static sites from Git, hosted or on machines you operate, with guides for environment & secrets, GitOps, regions, and the REST API.",
  twitterDescription:
    "shadw documentation: deploy serverless functions, containers, and static sites from Git, with guides for GitOps, regions, and the REST API.",
  path: "/docs",
});

const HERO_TABS = [
  {
    label: "cURL",
    lang: "bash" as const,
    filename: "deploy.sh",
    code: `# Auth: an API key (hive_…) binds the request to its team.
# Local dev (no HIVE_JWT_SECRET): 'x-hive-team: personal' works too.
curl -X POST https://api.shadw.cloud/v1/git/deploy \\
  -H 'content-type: application/json' \\
  -H 'Authorization: Bearer hive_YOUR_API_KEY' \\
  -d '{
    "repo_url": "https://github.com/acme/app",
    "project": "my-app",
    "production": true
  }'`,
  },
  {
    label: "TypeScript",
    lang: "ts" as const,
    filename: "deploy.ts",
    code: `const res = await fetch("https://api.shadw.cloud/v1/git/deploy", {
  method: "POST",
  headers: {
    "content-type": "application/json",
    // API key (Settings → API Keys) — binds the request to its team.
    Authorization: \`Bearer \${process.env.HIVE_API_KEY}\`,
  },
  body: JSON.stringify({
    repo_url: "https://github.com/acme/app",
    project: "my-app",
    production: true,
  }),
});
const { build_id } = await res.json();`,
  },
  {
    label: "Python",
    lang: "python" as const,
    filename: "deploy.py",
    code: `import os
import requests

res = requests.post(
    "https://api.shadw.cloud/v1/git/deploy",
    # API key (Settings → API Keys) — binds the request to its team.
    headers={"Authorization": f"Bearer {os.environ['HIVE_API_KEY']}"},
    json={
        "repo_url": "https://github.com/acme/app",
        "project": "my-app",
        "production": True,
    },
)
build_id = res.json()["build_id"]`,
  },
];

const CARDS = [
  { icon: Boxes, title: "Deploying apps", desc: "Git import, framework detection, Dockerfile containers and rollback.", href: "/docs/getting-started#deploying" },
  { icon: KeyRound, title: "Environment & secrets", desc: "Inject vars at runtime or into the build; sensitive values are encrypted at rest.", href: "/docs/getting-started#env" },
  { icon: GitBranch, title: "GitOps", desc: "Connect GitHub once — push to deploy, config-as-code, two-way sync.", href: "/docs/getting-started#gitops" },
  { icon: Globe2, title: "Regions", desc: "Regions follow where your nodes are; requests go to a healthy node in their own region first.", href: "/docs/getting-started#regions" },
  { icon: Zap, title: "Functions & Fluid", desc: "Serverless instances that pack concurrent requests; production stays warm, previews scale to zero.", href: "/docs/getting-started#deploying" },
  { icon: Code2, title: "API reference", desc: "The documented endpoints for deploys, env, projects, nodes and more.", href: "/docs/api-reference" },
  { icon: Sparkles, title: "AI Gateway", desc: "One provider pool, profitable per-model price cards and bounded web search.", href: "/docs/platform#ai-gateway" },
  { icon: Database, title: "Managed data", desc: "Per-database SQLite, a CRDT browser database, and managed Postgres and Redis records.", href: "/docs/platform#managed-data" },
  { icon: ShieldCheck, title: "Security", desc: "Attack Challenge Mode, sensitive-path denies, optional 2FA and a signed contract oplog.", href: "/docs/platform#security" },
  { icon: Workflow, title: "Queues, cron & workflows", desc: "Tenant-held queue tokens, node-local cron and durable workflow runs.", href: "/docs/platform#workloads" },
  { icon: Boxes, title: "Worked examples", desc: "Deploy, encrypted env, custom domains, gateway calls and Lakebed bindings.", href: "/docs/examples" },
  { icon: Globe2, title: "Platform overview", desc: "What the platform ships today, with real dashboard captures and a dated change list.", href: "/docs/platform" },
];

export default function DocsIndex() {
  return (
    <div className="mx-auto max-w-5xl">
      <div className="grid gap-10 pb-8 lg:grid-cols-[1.05fr_1fr] lg:items-center">
        <div>
          <h1 className="text-4xl font-semibold leading-[1.05] tracking-tight sm:text-5xl">
            Push to Git,
            <br />
            get a live URL
          </h1>
          <p className="mt-6 max-w-md text-lg leading-relaxed text-secondary">
            Deploy serverless functions, containers and static sites from Git, hosted or on machines you
            operate.
          </p>
          <div className="mt-8 flex flex-wrap gap-3">
            <Link href="/docs/getting-started" className="tap-44 inline-flex items-center gap-2 rounded-md border border-transparent bg-fg px-4 py-2.5 text-sm font-semibold text-bg transition-opacity hover:opacity-90">
              Get started guide <ArrowRight className="h-4 w-4" />
            </Link>
            <Link href="/settings/api-keys" className="tap-44 inline-flex items-center rounded-md border border-border px-4 py-2.5 text-sm font-medium hover:bg-subtle">Get an API key</Link>
            <Link href="/docs/api-reference" className="tap-44 inline-flex items-center rounded-md border border-border px-4 py-2.5 text-sm font-medium hover:bg-subtle">API reference</Link>
          </div>
        </div>
        <CodeBlock tabs={HERO_TABS} />
      </div>

      {/* M4: asymmetric evidence band breaks the uniform card grid. */}

      <DocsFigure
        src="/product/dashboard-deployments.png"
        alt="shadw dashboard, deployments tab: a project named storefront with three deployment rows showing status ready, environment Production or Preview, source commit and creation time."
        caption="Deployments view — status, environment, source and time per build."
        note="Real 2x capture of a seeded local node, dark theme, 2026-10-14. Shown as evidence of the dashboard shell; it predates the November 2026 features (AI Gateway, usage metering, Attack Challenge Mode, 2FA, managed data). Frames are unretouched. Recapture on a seeded node is an operator task."
        width={2880}
        height={1800}
      />

      <p className="border-t border-border pt-8 text-[15px] leading-relaxed text-secondary">
        Connect your{" "}
        <Link href="/docs/getting-started#gitops" className="text-fg underline decoration-border underline-offset-4 hover:decoration-fg">Git repository</Link>{" "}
        to deploy on every push, with{" "}
        <Link href="/docs/getting-started#deploying" className="text-fg underline decoration-border underline-offset-4 hover:decoration-fg">preview deployments</Link>{" "}
        for testing changes before production.
      </p>

      <h2 className="mb-5 mt-12 text-2xl font-semibold tracking-tight">Build with shadw</h2>
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {CARDS.map((c) => (
          <DocsCard key={c.title} icon={<c.icon className="h-6 w-6" />} title={c.title} desc={c.desc} href={c.href} />
        ))}
      </div>

      <footer className="mt-16 border-t border-border pt-6 text-sm text-muted">© 2026 shadw.cloud</footer>
    </div>
  );
}
