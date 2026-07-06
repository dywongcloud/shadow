import Link from "next/link";
import { CodeBlock } from "@/components/code-block";
import { API_BASE, API_CATEGORIES, endpointHref, totalEndpoints } from "@/lib/api-catalog";

/* The API Reference overview ("Using the REST API") — intro + API basics +
 * Authentication + team scoping + rate limits + errors + the Endpoints grid, with
 * an "On this page" table of contents on the right (Vercel-style). */

const TOC = [
  { id: "api-basics", label: "API basics" },
  { id: "authentication", label: "Authentication" },
  { id: "accessing-team-resources", label: "Accessing team resources" },
  { id: "rate-limits", label: "Rate limits" },
  { id: "errors", label: "Errors" },
  { id: "endpoints", label: "Endpoints" },
];

function H2({ id, children }: { id: string; children: React.ReactNode }) {
  return (
    <h2 id={id} className="mb-4 mt-14 scroll-mt-20 text-2xl font-bold tracking-tight">
      {children}
    </h2>
  );
}

function Code({ children }: { children: React.ReactNode }) {
  return <code className="rounded bg-subtle px-1.5 py-0.5 font-mono text-[0.85em]">{children}</code>;
}

export default function ApiReferencePage() {
  return (
    <div className="mx-auto flex max-w-6xl gap-12">
      <article className="min-w-0 flex-1 pb-24">
        <div className="mb-6 flex items-center gap-2 text-sm text-secondary">
          <span>APIs &amp; SDKs</span>
          <span className="text-muted">/</span>
          <span className="font-medium text-fg">shadw REST API</span>
        </div>

        <h1 className="text-4xl font-bold tracking-tight">Using the REST API</h1>
        <p className="mt-6 text-[15px] leading-relaxed text-secondary">
          Interact programmatically with your shadw cloud using direct HTTP requests. You can create
          deployments, manage custom domains and DNS, configure projects, and wire up webhooks — all
          over the mesh.
        </p>
        <p className="mt-4 text-[15px] leading-relaxed text-secondary">
          The API supports any programming language or framework that can send HTTP requests.
        </p>

        <H2 id="api-basics">API basics</H2>
        <p className="text-[15px] leading-relaxed text-secondary">
          The API is exposed as an HTTP/1 and HTTP/2 service over SSL. All endpoints live under the
          URL <Code>{API_BASE}</Code> and follow the REST architecture.
        </p>

        <H2 id="authentication">Authentication</H2>
        <p className="text-[15px] leading-relaxed text-secondary">
          Authenticate with a platform <strong className="text-fg">API key</strong> — a long-lived,
          team-bound token prefixed <Code>hive_</Code>. Create one under{" "}
          <strong className="text-fg">Settings → API Keys</strong> (or with{" "}
          <Code>POST /v1/apikeys</Code>). The full token is shown exactly once at creation; only its
          SHA-256 hash is stored. Include it in the <Code>Authorization</Code> header:
        </p>
        <div className="mt-4">
          <CodeBlock tabs={[{ label: "Authorization", lang: "bash", code: "Authorization: Bearer hive_…" }]} />
        </div>
        <p className="mt-4 text-[15px] leading-relaxed text-secondary">
          Alternatively, mint a short-lived (8&nbsp;hour) JWT with <Code>POST /v1/token</Code> and
          present it in the same header. Minting requires <Code>HIVE_JWT_SECRET</Code> to be
          configured on the node; when JWT auth is enforced, mutating requests (POST/PUT/DELETE)
          must present a JWT.
        </p>

        <H2 id="accessing-team-resources">Accessing team resources</H2>
        <p className="text-[15px] leading-relaxed text-secondary">
          Every request resolves to a team (tenant), in this priority order: a JWT&apos;s{" "}
          <Code>tenant</Code> claim wins; otherwise an API key scopes the request to the team it was
          created under — no extra header needed; otherwise, on nodes without JWT enforcement (no{" "}
          <Code>HIVE_JWT_SECRET</Code>, e.g. local dev), the <Code>x-hive-team</Code> header is
          honored. With none of these, requests fall back to the <Code>personal</Code> team.
        </p>
        <div className="mt-4">
          <CodeBlock
            tabs={[{ label: "cURL", lang: "bash", code: `# An API key is already bound to its team — no team header needed.\ncurl ${API_BASE}/v1/overview \\\n  -H 'Authorization: Bearer hive_…'` }]}
          />
        </div>

        <H2 id="rate-limits">Rate limits</H2>
        <p className="text-[15px] leading-relaxed text-secondary">
          API requests are rate-limited per team. When you exceed the limit the API responds with{" "}
          <Code>429 Too Many Requests</Code>; back off and retry after the window resets. Burst limits
          are higher on Pro and Enterprise plans.
        </p>

        <H2 id="errors">Errors</H2>
        <p className="text-[15px] leading-relaxed text-secondary">Standard HTTP status codes are used throughout.</p>
        <div className="mt-4 overflow-hidden rounded-lg border border-border">
          <table className="w-full text-left text-sm">
            <tbody className="divide-y divide-border">
              {[
                ["200", "Success."],
                ["400", "Bad request — malformed JSON or missing required fields."],
                ["401", "Unauthorized — missing/invalid token, or a protected preview."],
                ["404", "Not found — unknown project, deployment or domain."],
                ["409", "Conflict — e.g. a name already in use."],
                ["429", "Too many requests — rate limited."],
                ["500", "Internal error — check the node logs."],
              ].map(([code, desc]) => (
                <tr key={code}>
                  <td className="w-16 px-3 py-2 font-mono text-xs">{code}</td>
                  <td className="px-3 py-2 text-secondary">{desc}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        <H2 id="endpoints">Endpoints</H2>
        <p className="text-[15px] leading-relaxed text-secondary">
          Browse all {totalEndpoints()} available REST API endpoints grouped by category.
        </p>
        <div className="mt-5 grid gap-3 sm:grid-cols-2">
          {API_CATEGORIES.map((cat) => (
            <Link
              key={cat.slug}
              href={endpointHref(cat.slug, cat.endpoints[0].slug)}
              className="flex items-center justify-between rounded-lg border border-border px-4 py-3.5 transition-colors hover:border-border-strong hover:bg-subtle/40"
            >
              <span className="text-sm font-medium">{cat.name}</span>
              <span className="text-xs text-muted">
                {cat.endpoints.length} endpoint{cat.endpoints.length === 1 ? "" : "s"}
              </span>
            </Link>
          ))}
        </div>
      </article>

      {/* On this page */}
      <aside className="sticky top-20 hidden h-fit w-52 shrink-0 xl:block">
        <div className="mb-3 text-xs font-medium text-muted">On this page</div>
        <ul className="space-y-2 border-l border-border">
          {TOC.map((t) => (
            <li key={t.id}>
              <a href={`#${t.id}`} className="-ml-px block border-l border-transparent pl-3 text-sm text-secondary hover:border-fg hover:text-fg">
                {t.label}
              </a>
            </li>
          ))}
        </ul>
      </aside>
    </div>
  );
}
