import Link from "next/link";
import { notFound } from "next/navigation";
import { Link2 } from "lucide-react";
import { CodeBlock } from "@/components/code-block";
import {
  API_CATEGORIES,
  METHOD_TONE,
  type ApiParam,
  codeExamples,
  endpointUrl,
  findEndpoint,
} from "@/lib/api-catalog";

// Statically generate a page for every endpoint in the catalog.
export function generateStaticParams() {
  return API_CATEGORIES.flatMap((c) => c.endpoints.map((e) => ({ category: c.slug, method: e.slug })));
}

function Section({ id, title, children }: { id?: string; title: string; children: React.ReactNode }) {
  return (
    <section className="mt-12">
      <h2 id={id} className="group mb-4 flex scroll-mt-20 items-center gap-2 text-2xl font-bold tracking-tight">
        {title}
        {id && <Link2 className="h-4 w-4 text-muted opacity-0 transition-opacity group-hover:opacity-100" />}
      </h2>
      {children}
    </section>
  );
}

function ParamRow({ p }: { p: ApiParam }) {
  return (
    <div className="border-b border-border py-3 last:border-0">
      <div className="flex flex-wrap items-center gap-2">
        <code className="font-mono text-sm font-medium">{p.name}</code>
        <span className="rounded bg-subtle px-1.5 py-0.5 font-mono text-xs text-secondary">{p.type}</span>
        {p.required ? (
          <span className="text-xs font-medium text-red-500">Required</span>
        ) : (
          <span className="text-xs text-muted">Optional</span>
        )}
      </div>
      <p className="mt-1.5 text-sm text-secondary">{p.desc}</p>
    </div>
  );
}

export default function MethodPage({ params }: { params: { category: string; method: string } }) {
  const found = findEndpoint(params.category, params.method);
  if (!found) notFound();
  const { endpoint: ep } = found;
  const ex = codeExamples(ep);

  return (
    <div className="mx-auto flex max-w-6xl flex-col gap-8 lg:flex-row lg:gap-10">
      {/* Main content */}
      <article className="min-w-0 flex-1 pb-24">
        <div className="mb-6 flex flex-wrap items-center gap-2 text-sm text-secondary">
          <span>APIs &amp; SDKs</span>
          <span className="text-muted">/</span>
          <Link href="/docs/api-reference" className="hover:text-fg">shadw REST API</Link>
          <span className="text-muted">/</span>
          <span className="font-medium text-fg">{ep.name}</span>
        </div>

        <h1 className="text-3xl font-bold tracking-tight sm:text-4xl">{ep.name}</h1>

        <div className="mt-5 flex flex-wrap items-center gap-3">
          <span className={`rounded-md px-2 py-0.5 font-mono text-xs font-semibold ${METHOD_TONE[ep.method]}`}>
            {ep.method}
          </span>
          <code className="break-all font-mono text-sm">{endpointUrl(ep)}</code>
        </div>

        <p className="mt-5 text-[15px] leading-relaxed text-secondary">{ep.summary}</p>

        <Section id="authentication" title="Authentication">
          <div className="flex items-center gap-2">
            <code className="font-mono text-sm font-medium">Authorization</code>
            <span className="rounded bg-subtle px-1.5 py-0.5 font-mono text-xs text-secondary">bearerToken</span>
          </div>
          <p className="mt-1.5 text-sm text-secondary">
            A platform API key (<code className="font-mono text-xs">hive_…</code>, created under
            Settings → API Keys) or a short-lived JWT minted with{" "}
            <code className="font-mono text-xs">POST /v1/token</code>. An API key scopes the request
            to the team it was created under.
          </p>
        </Section>

        {ep.pathParams?.length ? (
          <Section id="path-parameters" title="Path parameters">
            {ep.pathParams.map((p) => <ParamRow key={p.name} p={p} />)}
          </Section>
        ) : null}

        {ep.queryParams?.length ? (
          <Section id="query-parameters" title="Query parameters">
            {ep.queryParams.map((p) => <ParamRow key={p.name} p={p} />)}
          </Section>
        ) : null}

        {ep.bodyParams?.length ? (
          <Section id="body-parameters" title="Body parameters">
            {ep.bodyParams.map((p) => <ParamRow key={p.name} p={p} />)}
          </Section>
        ) : null}
      </article>

      {/* Right column: request examples + response */}
      <aside className="w-full shrink-0 space-y-4 lg:sticky lg:top-20 lg:h-fit lg:w-[420px]">
        <CodeBlock
          tabs={[
            { label: "TypeScript", lang: "ts", code: ex.typescript },
            { label: "Next.js", lang: "ts", code: ex.nextjs },
            { label: "cURL", lang: "bash", code: ex.curl },
          ]}
        />
        {ep.response && (
          <div>
            <div className="mb-2 text-xs font-medium text-muted">Response</div>
            <CodeBlock tabs={[{ label: "Response", lang: "json", code: ep.response }]} />
          </div>
        )}
      </aside>
    </div>
  );
}
