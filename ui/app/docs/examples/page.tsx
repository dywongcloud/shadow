import Link from "next/link";
import type { Metadata } from "next";
import { pageMeta } from "@/lib/page-meta";
import { ArrowRight } from "lucide-react";
import { CodeBlock } from "@/components/code-block";
import {
  DocsPage,
  DocsSection,
  DocsIn,
  DocsCallout,
  DocsFigure,
} from "@/components/docs/docs-ui";

export const metadata: Metadata = pageMeta({
  title: "Examples — Docs",
  socialTitle: "Examples — shadw Docs",
  description:
    "Worked shadw examples: deploy from Git, set an encrypted environment variable, attach a custom domain, call the AI Gateway, and bind Lakebed storage. With real dashboard captures.",
  twitterDescription:
    "Worked shadw examples: deploy from Git, encrypted env vars, custom domains, AI Gateway and Lakebed.",
  path: "/docs/examples",
});

export default function ExamplesPage() {
  return (
    <DocsPage
      eyebrow="Examples"
      title="Worked examples"
      lead="Each example uses a real route and a real request shape. Credentials are placeholders; secrets resolve server-side, never in the browser."
      back={{ href: "/docs", label: "← Back to docs" }}
    >
      <DocsSection id="deploy" eyebrow="Builds" title="Deploy from Git">
        <p>
          <DocsIn>POST /v1/git/deploy</DocsIn> builds a repository into a deployment. The API key is bound
          to its team, so no team header is needed. A local node without <DocsIn>HIVE_JWT_SECRET</DocsIn>
          accepts <DocsIn>x-hive-team</DocsIn> instead — that fallback is dev-only and does not apply to
          the enforced public API.
        </p>
        <CodeBlock
          tabs={[
            {
              label: "cURL",
              lang: "bash",
              filename: "deploy.sh",
              code: `curl -X POST https://api.shadw.cloud/v1/git/deploy \\
  -H 'content-type: application/json' \\
  -H 'Authorization: Bearer hive_YOUR_API_KEY' \\
  -d '{ "repo_url": "https://github.com/acme/app",
        "project": "my-app", "production": true }'
# -> { "build_id": "dpl-…", "project": "my-app" }`,
            },
            {
              label: "TypeScript",
              lang: "ts",
              filename: "deploy.ts",
              code: `const res = await fetch("https://api.shadw.cloud/v1/git/deploy", {
  method: "POST",
  headers: {
    "content-type": "application/json",
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
              lang: "python",
              filename: "deploy.py",
              code: `import os
import requests

res = requests.post(
    "https://api.shadw.cloud/v1/git/deploy",
    headers={"Authorization": f"Bearer {os.environ['HIVE_API_KEY']}"},
    json={
        "repo_url": "https://github.com/acme/app",
        "project": "my-app",
        "production": True,
    },
)
build_id = res.json()["build_id"]`,
            },
          ]}
        />
        <DocsCallout>
          A deployment fails in one of two shapes: starts then dies, or never listens. A broken deployment
          reports <DocsIn>DEPLOYMENT_CIRCUIT_OPEN</DocsIn>, never a capacity error. A successful start does
          not clear a crash streak.
        </DocsCallout>
      </DocsSection>

      <DocsSection id="dashboard" eyebrow="Dashboard" title="What you see after a deploy">
        <p>
          The deployments view lists each build with its status, environment, source commit and creation
          time. Redeploy, Instant Rollback, Delete and Visit act on the selected deployment. This is a real
          capture of a seeded local node, not a mock.
        </p>
        <DocsFigure
          src="/product/dashboard-deployments.png"
          alt="shadw dashboard deployments tab, dark theme, with a project named storefront and three deployment rows."
          caption="Deployments view — the state a build lands in."
          note="Real 2x capture of a seeded local node, dark theme, 2026-10-14. It shows the dashboard shell, not the November 2026 platform features. Frames are unretouched."
          width={2880}
          height={1800}
        />
        <p>
          The projects view is the same shell with the project list. Both are shown at full 2x resolution
          in a scrollable frame, so the labels stay readable in a narrow docs column.
        </p>
        <DocsFigure
          src="/product/dashboard-projects.png"
          alt="shadw dashboard projects tab, dark theme, with a list of projects and their deployment status."
          caption="Projects view — where a new project starts."
          note="Real 2x capture of a seeded local node, dark theme, 2026-10-14. Same provenance and date caveat as the deployments frame."
          width={2880}
          height={1800}
        />
      </DocsSection>

      <DocsSection id="env" eyebrow="Configuration" title="Encrypted environment variables">
        <p>
          A variable is injected at runtime only unless you pass a build scope. Sensitive values are sealed
          before they touch disk with ChaCha20-Poly1305, stored as <DocsIn>enc:v1:…</DocsIn>, masked in
          API responses, and decrypted only when injected. Build-time values such as{" "}
          <DocsIn>NEXT_PUBLIC_*</DocsIn> must carry a build or all scope.
        </p>
        <CodeBlock
          tabs={[
            {
              label: "cURL",
              lang: "bash",
              filename: "env.sh",
              code: `curl -X POST https://api.shadw.cloud/v1/projects/my-app/env \\
  -H 'content-type: application/json' \\
  -H 'Authorization: Bearer hive_YOUR_API_KEY' \\
  -d '{ "key": "API_KEY", "value": "example-value-not-a-secret",
        "target": "all", "sensitive": true }'`,
            },
            {
              label: "JSON",
              lang: "json",
              filename: "request.json",
              code: `{
  "key": "API_KEY",
  "value": "example-value-not-a-secret",
  "target": "all",
  "sensitive": true
}`,
            },
          ]}
        />
        <DocsCallout tone="warn">
          Back up <DocsIn>$HIVE_DATA/secret.key</DocsIn> (or set <DocsIn>HIVE_SECRET_KEY</DocsIn>). Without
          it, sealed secrets cannot be decrypted. Rotation requires keeping the previous key in{" "}
          <DocsIn>HIVE_SECRET_KEY_OLD</DocsIn> during the roll.
        </DocsCallout>
      </DocsSection>

      <DocsSection id="domain" eyebrow="Networking" title="Attach a custom domain">
        <p>
          Routing keys are full hostnames before any first-label or wildcard lookup. Attachment needs
          observed DNS proof; only a verified record locks to its tenant. Reserved proof names such as{" "}
          <DocsIn>_acme-challenge</DocsIn> and <DocsIn>_hive-verify</DocsIn> are refused in add, import
          and zone-parse. A delegated name must never serve neither addresses nor delegation, so a cutover
          is a restore-on-failure transaction.
        </p>
        <CodeBlock
          tabs={[
            {
              label: "cURL",
              lang: "bash",
              filename: "domain.sh",
              code: `curl -X POST https://api.shadw.cloud/v1/projects/my-app/domains \\
  -H 'content-type: application/json' \\
  -H 'Authorization: Bearer hive_YOUR_API_KEY' \\
  -d '{ "domain": "www.example.com" }'`,
            },
          ]}
        />
        <p>
          <Link href="/docs/getting-started#domains" className="tap-44 inline-flex items-center gap-1.5 text-link hover:underline">
            Domains &amp; TLS <ArrowRight className="h-3.5 w-3.5" />
          </Link>
        </p>
      </DocsSection>

      <DocsSection id="ai" eyebrow="AI" title="Call the AI Gateway">
        <p>
          The gateway exposes OpenAI-shaped <DocsIn>/v1/chat/completions</DocsIn>, plus{" "}
          <DocsIn>/v1/responses</DocsIn> and <DocsIn>/v1/messages</DocsIn>. Model discovery is{" "}
          <DocsIn>GET /v1/models</DocsIn>. Prices come only from the rate card.
        </p>
        <CodeBlock
          tabs={[
            {
              label: "cURL",
              lang: "bash",
              filename: "chat.sh",
              code: `curl -X POST https://api.shadw.cloud/v1/chat/completions \\
  -H 'content-type: application/json' \\
  -H 'Authorization: Bearer hive_YOUR_API_KEY' \\
  -d '{ "model": "claude-opus-5-5",
        "messages": [{"role": "user", "content": "What changed in this deploy?"}] }'`,
            },
          ]}
        />
        <p>
          <Link href="/docs/platform#ai-gateway" className="tap-44 inline-flex items-center gap-1.5 text-link hover:underline">
            AI Gateway details <ArrowRight className="h-3.5 w-3.5" />
          </Link>
        </p>
      </DocsSection>

      <DocsSection id="lakebed" eyebrow="Storage" title="Bind Lakebed storage">
        <p>
          Lakebed is preset and detection only. The example project binds <DocsIn>ctx.db</DocsIn> and{" "}
          <DocsIn>storage</DocsIn> to Blob. There is no hosted source runtime for tenant bindings — the
          platform does not execute a shared runtime on your behalf.
        </p>
        <CodeBlock
          tabs={[
            {
              label: "TypeScript",
              lang: "ts",
              filename: "lakebed.ts",
              code: `// examples/lakebed-todo binds ctx.db / storage to Blob.
export async function handler(ctx) {
  const row = await ctx.db.get("todo:1");
  await ctx.storage.put("last-read", String(row?.id ?? "empty"));
  return new Response("ok");
}`,
            },
          ]}
        />
        <p>
          <Link href="/docs/platform#managed-data" className="tap-44 inline-flex items-center gap-1.5 text-link hover:underline">
            Managed data <ArrowRight className="h-3.5 w-3.5" />
          </Link>
        </p>
      </DocsSection>
    </DocsPage>
  );
}
