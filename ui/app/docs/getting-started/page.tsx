import Link from "next/link";
import { ArrowRight } from "lucide-react";
import { CodeBlock } from "@/components/code-block";

export const metadata = {
  title: "Getting started — shadw Docs",
  description: "Deploy your first app on shadw, the peer-to-peer cloud.",
};

function Section({ id, title, eyebrow, children }: { id: string; title: string; eyebrow?: string; children: React.ReactNode }) {
  return (
    <section id={id} className="scroll-mt-20 border-t border-border pt-12 first:border-0 first:pt-0">
      {eyebrow && <div className="mb-2 font-mono text-xs uppercase tracking-widest text-link">{eyebrow}</div>}
      <h2 className="text-2xl font-semibold tracking-tight sm:text-3xl">{title}</h2>
      <div className="mt-5 space-y-4 text-[15px] leading-relaxed text-secondary">{children}</div>
    </section>
  );
}
function H3({ children }: { children: React.ReactNode }) {
  return <h3 className="!mt-8 text-lg font-semibold text-fg">{children}</h3>;
}
function In({ children }: { children: React.ReactNode }) {
  return <code className="rounded bg-subtle px-1.5 py-0.5 font-mono text-[0.85em] text-fg">{children}</code>;
}
function Callout({ children, tone = "info" }: { children: React.ReactNode; tone?: "info" | "warn" }) {
  const cls = tone === "warn"
    ? "border-amber-500/30 bg-amber-500/10 text-amber-700 dark:text-amber-300"
    : "border-link/30 bg-link/5 text-secondary";
  return <div className={`rounded-lg border px-4 py-3 text-sm ${cls}`}>{children}</div>;
}
function One({ code, lang, filename }: { code: string; lang: "ts" | "bash" | "python" | "json"; filename?: string }) {
  return <CodeBlock tabs={[{ label: filename ?? lang, code, lang, filename }]} />;
}

export default function GettingStarted() {
  return (
    <div className="mx-auto max-w-3xl">
      <div className="mb-12">
        <div className="mb-2 font-mono text-xs uppercase tracking-widest text-link">Documentation</div>
        <h1 className="text-4xl font-bold tracking-tight sm:text-5xl">Getting started</h1>
        <p className="mt-4 text-lg text-secondary">
          Everything you need to deploy and run apps on <strong className="text-fg">shadw</strong> — the
          self-hosted, peer-to-peer cloud.
        </p>
      </div>

      <div className="space-y-2">
        <Section id="introduction" eyebrow="Overview" title="Introduction">
          <p>
            shadw turns any machine you run into a region. Nodes find each other over a real peer-to-peer mesh
            (Iroh QUIC, with NAT traversal + relay fallback) and serve your deployments by anycast — no data
            center, no public IPs. The platform is two layers over one isolation backend:
          </p>
          <ul className="list-disc space-y-1.5 pl-5">
            <li><strong className="text-fg">Builds</strong> — turn a git repo into build output (framework detection or a Dockerfile).</li>
            <li><strong className="text-fg">Fluid compute</strong> — serve functions on long-lived instances that handle many concurrent requests, autoscale, and scale to zero.</li>
          </ul>
          <p>Everything below talks to a node&apos;s <strong className="text-fg">admin API</strong> (default <In>127.0.0.1:8786</In>); the public gateway serves traffic on <In>:8787</In>.</p>
        </Section>

        <Section id="quickstart" eyebrow="Get started" title="Quickstart">
          <p>From the dashboard, click <strong className="text-fg">New Project</strong>, pick a template or paste a Git URL, and deploy. Or via the API:</p>
          <One lang="bash" filename="deploy.sh" code={`curl -X POST http://127.0.0.1:8786/v1/git/deploy \\
  -H 'content-type: application/json' -H 'x-hive-team: personal' \\
  -d '{ "repo_url": "https://github.com/acme/app", "project": "my-app", "production": true }'
# -> { "build_id": "dpl-…", "project": "my-app" }`} />
          <p>Your deployment is reachable instantly at <In>my-app.localhost:8787</In> (a self-refreshing <em>Building…</em> page shows until the build finishes).</p>
        </Section>

        <Section id="deploying" eyebrow="Builds" title="Deploying apps">
          <p>shadw detects how to build your repo and produces a deployment routed by its subdomain.</p>
          <H3>From a Git repository</H3>
          <p>Framework-Defined Infrastructure detects Next.js, Vite, React, SvelteKit, Nuxt, Vue, Astro, Remix, Express and more — runs install/build and normalizes the output into static assets and/or a serverless server.</p>
          <H3>Containers (Dockerfile)</H3>
          <p>If a <In>Dockerfile</In> is present, shadw builds the image and runs it as a container (Railway-style). Stateful container singletons use consensus-free leases + fencing, so a node failure triggers automatic failover.</p>
          <H3>Build configuration</H3>
          <p>Override detection per project under <strong className="text-fg">Settings → Build</strong>: install/build commands, output directory, and root directory (monorepos).</p>
          <Callout>The latest production deploy owns the project&apos;s domain; each deploy also gets a per-deployment URL. Roll back instantly by promoting a prior deployment.</Callout>
        </Section>

        <Section id="env" eyebrow="Configuration" title="Environment & secrets">
          <p>Set variables when creating a project or under <strong className="text-fg">Settings → Environment Variables</strong>. They&apos;re injected into <strong className="text-fg">both build and runtime</strong> — so <In>NEXT_PUBLIC_*</In>, <In>VITE_*</In> and server config all work.</p>
          <H3>Secrets are encrypted at rest</H3>
          <p>Variables marked <strong className="text-fg">Sensitive</strong> are sealed with ChaCha20-Poly1305 before they touch disk — stored as <In>enc:v1:…</In>, masked in API responses, and decrypted only when injected.</p>
          <One lang="bash" filename="env.sh" code={`curl -X POST http://127.0.0.1:8786/v1/projects/my-app/env \\
  -H 'content-type: application/json' -H 'x-hive-team: personal' \\
  -d '{ "key": "API_KEY", "value": "sk-live-…", "target": "all", "sensitive": true }'`} />
          <Callout tone="warn">Back up <In>$HIVE_DATA/secret.key</In> (or set <In>HIVE_SECRET_KEY</In>) — without it, sealed secrets can&apos;t be decrypted.</Callout>
        </Section>

        <Section id="gitops" eyebrow="Automation" title="GitOps">
          <p>Connect GitHub once and shadw manages your org <strong className="text-fg">as code</strong>: it commits project config to a repo as <In>openedge.yaml</In> and keeps it in sync.</p>
          <ul className="list-disc space-y-1.5 pl-5">
            <li><strong className="text-fg">Push to deploy</strong> — a push triggers a build &amp; deploy via an installed GitHub Action + webhook.</li>
            <li><strong className="text-fg">Config-as-code</strong> — declarative config, versioned in git.</li>
            <li><strong className="text-fg">Two-way sync</strong> — dashboard changes commit back; commits redeploy.</li>
          </ul>
          <p>The workflow calls your node&apos;s public webhook (<In>OPENEDGE_WEBHOOK_URL</In>) at <In>/v1/git/webhook</In>.</p>
        </Section>

        <Section id="regions" eyebrow="Network" title="Regions & the peer-to-peer mesh">
          <p>Your regions are <strong className="text-fg">wherever your nodes actually are</strong> — each node geolocates and is auto-placed on its continent.</p>
          <ul className="list-disc space-y-1.5 pl-5">
            <li><strong className="text-fg">Iroh QUIC</strong> — direct, encrypted connections with NAT traversal and relay fallback.</li>
            <li><strong className="text-fg">Anycast</strong> — requests route to the lowest-latency healthy node, failing over automatically.</li>
            <li><strong className="text-fg">Mesh routing</strong> — any node can serve any deployment by proxying to a peer.</li>
          </ul>
          <One lang="bash" filename="add-node.sh" code={`hive-cloud --name node-b --peer http://<node-a-ip>:8786
# region is auto-derived from the node's real location`} />
        </Section>

        <Section id="domains" eyebrow="Networking" title="Domains & TLS">
          <p>Every project gets <In>&lt;project&gt;.localhost:8787</In>. Deployments route purely by subdomain, so the same project is reachable at <In>&lt;project&gt;.&lt;your-domain&gt;</In> once the gateway is exposed there.</p>
          <p>The gateway terminates TLS (self-signed locally, or your cert via <In>HIVE_TLS_CERT</In>/<In>HIVE_TLS_KEY</In>) and runs an authoritative DNS server. Add custom domains under <strong className="text-fg">Domains</strong>.</p>
        </Section>

        <Section id="cli" eyebrow="Tooling" title="CLI">
          <p>The repo ships CLIs that talk to a node&apos;s admin API.</p>
          <One lang="bash" filename="cli.sh" code={`fluidctl deploy examples/hello     # deploy a local app
fluidctl ls                        # list deployments
hivectl submit --image node:20 -c 'npm ci' -c 'npm run build' --follow`} />
        </Section>

        <Section id="api" eyebrow="Reference" title="API reference">
          <p>All endpoints live on a node&apos;s <strong className="text-fg">admin API</strong> (default <In>:8786</In>). Scope a request with the <In>x-hive-team</In> header; when <In>HIVE_JWT_SECRET</In> is set, mutations require a <In>Bearer</In> token.</p>
          <ApiTable />
        </Section>

        <Section id="self-hosting" eyebrow="Operations" title="Self-hosting">
          <p>shadw is one node binary. Run at least two for the full mesh (anycast, failover):</p>
          <One lang="bash" filename="cluster.sh" code={`hive-cloud --name node-a --listen 127.0.0.1:8787 --admin 127.0.0.1:8786

HIVE_DATA=~/.hive-cloud-b HIVE_DNS_ADDR=127.0.0.1:5355 HIVE_TLS_ADDR=127.0.0.1:8444 \\
  hive-cloud --name node-b --listen 127.0.0.1:8789 --admin 127.0.0.1:8788 \\
  --peer http://127.0.0.1:8786`} />
          <ul className="list-disc space-y-1.5 pl-5">
            <li><In>$HIVE_DATA</In> — durable state dir (snapshot, blobs, GuardianDB, secret key)</li>
            <li><In>HIVE_TLS_CERT</In> / <In>HIVE_TLS_KEY</In> — production TLS</li>
            <li><In>HIVE_JWT_SECRET</In> — require Bearer auth on mutations</li>
            <li><In>HIVE_SECRET_KEY</In> — at-rest encryption key for secrets</li>
          </ul>
          <p className="flex items-center gap-1.5">
            <Link href="/" className="inline-flex items-center gap-1 text-link hover:underline">Open the dashboard <ArrowRight className="h-3.5 w-3.5" /></Link>
          </p>
        </Section>
      </div>

      <footer className="mt-16 flex items-center justify-between border-t border-border pt-6 text-sm text-muted">
        <span>© 2026 shadw.cloud</span>
        <Link href="/docs" className="hover:text-fg">← Back to docs</Link>
      </footer>
    </div>
  );
}

function ApiTable() {
  const rows: { m: string; path: string; desc: string }[] = [
    { m: "POST", path: "/v1/git/deploy", desc: "Build & deploy from a git repo" },
    { m: "GET", path: "/deployments", desc: "List deployments" },
    { m: "POST", path: "/deployments", desc: "Create a deployment from a manifest" },
    { m: "DELETE", path: "/v1/deployments/:id", desc: "Delete a deployment" },
    { m: "POST", path: "/v1/deployments/:id/promote", desc: "Promote (instant rollback)" },
    { m: "POST", path: "/v1/projects/:project/redeploy", desc: "Rebuild the latest commit" },
    { m: "GET", path: "/v1/projects/:project/settings", desc: "Project settings (env masked)" },
    { m: "POST", path: "/v1/projects/:project/env", desc: "Set an env var (sensitive ones encrypted)" },
    { m: "PUT", path: "/v1/projects/:project/functions", desc: "Function settings: regions, fluid, duration" },
    { m: "GET", path: "/v1/regions/catalog", desc: "Regions from the live mesh" },
    { m: "GET", path: "/v1/nodes", desc: "Mesh nodes (region, geo, capacity, health)" },
    { m: "GET", path: "/v1/overview", desc: "Overview analytics" },
    { m: "POST", path: "/v1/sandbox", desc: "Run code in an isolated cell" },
  ];
  const color = (m: string) =>
    m === "GET" ? "text-emerald-600 dark:text-emerald-400"
    : m === "POST" ? "text-blue-600 dark:text-blue-400"
    : m === "PUT" ? "text-amber-600 dark:text-amber-400"
    : "text-red-600 dark:text-red-400";
  return (
    <div className="overflow-hidden rounded-lg border border-border">
      <table className="w-full text-left text-sm">
        <thead className="bg-subtle text-xs uppercase tracking-wide text-muted">
          <tr><th className="px-4 py-2.5 font-medium">Method</th><th className="px-4 py-2.5 font-medium">Endpoint</th><th className="hidden px-4 py-2.5 font-medium sm:table-cell">Description</th></tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.path} className="border-t border-border">
              <td className={`px-4 py-2.5 font-mono text-xs font-semibold ${color(r.m)}`}>{r.m}</td>
              <td className="px-4 py-2.5 font-mono text-xs text-fg">{r.path}</td>
              <td className="hidden px-4 py-2.5 text-secondary sm:table-cell">{r.desc}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
