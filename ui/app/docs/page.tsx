import Link from "next/link";
import type { Metadata } from "next";
import { ArrowRight, Boxes, GitBranch, Zap, Globe2, KeyRound, Code2 } from "lucide-react";
import { CodeBlock } from "@/components/code-block";

export const metadata: Metadata = {
  title: "Documentation — shadw",
  description:
    "shadw documentation: deploy serverless functions, containers, and static sites to a peer-to-peer cloud of your own machines. Push from Git and serve from anywhere, with guides for environment & secrets, GitOps, regions, and the REST API.",
  alternates: { canonical: "/docs" },
  openGraph: {
    title: "Documentation — shadw",
    description:
      "shadw documentation: deploy serverless functions, containers, and static sites to a peer-to-peer cloud of your own machines. Push from Git and serve from anywhere, with guides for environment & secrets, GitOps, regions, and the REST API.",
    url: "/docs",
    type: "website",
    siteName: "shadw",
  },
  twitter: {
    card: "summary_large_image",
    title: "Documentation — shadw",
    description:
      "shadw documentation: deploy serverless functions, containers, and static sites to a peer-to-peer cloud of your own machines, with guides for GitOps, regions, and the REST API.",
  },
};

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
  { icon: Boxes, title: "Deploying apps", desc: "Git import, framework detection, Dockerfile containers and instant rollback.", href: "/docs/getting-started#deploying" },
  { icon: KeyRound, title: "Environment & secrets", desc: "Inject vars into build + runtime; sensitive values are encrypted at rest.", href: "/docs/getting-started#env" },
  { icon: GitBranch, title: "GitOps", desc: "Connect GitHub once — push to deploy, config-as-code, two-way sync.", href: "/docs/getting-started#gitops" },
  { icon: Globe2, title: "Regions & the mesh", desc: "Real geo regions over Iroh QUIC with anycast routing and failover.", href: "/docs/getting-started#regions" },
  { icon: Zap, title: "Functions & Fluid", desc: "Serverless instances that scale to zero and pack concurrent requests.", href: "/docs/getting-started#deploying" },
  { icon: Code2, title: "API reference", desc: "Every admin endpoint for deploys, env, projects, nodes and more.", href: "/docs/getting-started#api" },
];

export default function DocsIndex() {
  return (
    <div className="mx-auto max-w-5xl">
      {/* Hero */}
      <div className="grid gap-10 pb-8 lg:grid-cols-[1.05fr_1fr] lg:items-center">
        <div>
          <h1 className="text-5xl font-bold leading-[1.05] tracking-tight sm:text-6xl">
            Ship anything,
            <br />
            on your own mesh
          </h1>
          <p className="mt-6 max-w-md text-lg leading-relaxed text-secondary">
            Deploy serverless functions, containers and static sites to a peer-to-peer cloud of your own
            machines. Push from git, served from anywhere — in three steps.
          </p>
          <div className="mt-8 flex flex-wrap gap-3">
            <Link href="/docs/getting-started" className="inline-flex items-center gap-2 rounded-md bg-fg px-4 py-2.5 text-sm font-semibold text-bg transition-opacity hover:opacity-90">
              Get started guide <ArrowRight className="h-4 w-4" />
            </Link>
            <Link href="/settings/api-keys" className="rounded-md border border-border px-4 py-2.5 text-sm font-medium hover:bg-subtle">Get an API key</Link>
            <Link href="/docs/getting-started#api" className="rounded-md border border-border px-4 py-2.5 text-sm font-medium hover:bg-subtle">API reference</Link>
          </div>
        </div>
        <CodeBlock tabs={HERO_TABS} />
      </div>

      {/* Intro line */}
      <p className="border-t border-border pt-8 text-[15px] leading-relaxed text-secondary">
        Connect your{" "}
        <Link href="/docs/getting-started#gitops" className="text-fg underline decoration-border underline-offset-4 hover:decoration-fg">Git repository</Link>{" "}
        to deploy on every push, with{" "}
        <Link href="/docs/getting-started#deploying" className="text-fg underline decoration-border underline-offset-4 hover:decoration-fg">instant preview deployments</Link>{" "}
        for testing changes before production.
      </p>

      {/* Cards */}
      <h2 className="mb-5 mt-12 text-2xl font-semibold tracking-tight">Build with shadw</h2>
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {CARDS.map((c) => (
          <Link
            key={c.title}
            href={c.href}
            className="group rounded-xl border border-border bg-card p-5 transition-colors hover:border-border-strong hover:bg-subtle/40"
          >
            <c.icon className="h-6 w-6 text-fg" />
            <div className="mt-4 flex items-center gap-1.5 font-semibold">
              {c.title}
              <ArrowRight className="h-4 w-4 -translate-x-1 text-muted opacity-0 transition-all group-hover:translate-x-0 group-hover:opacity-100" />
            </div>
            <p className="mt-1.5 text-sm leading-relaxed text-secondary">{c.desc}</p>
          </Link>
        ))}
      </div>

      <footer className="mt-16 border-t border-border pt-6 text-sm text-muted">© 2026 shadw.cloud</footer>
    </div>
  );
}
