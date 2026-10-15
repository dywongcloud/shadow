import Link from "next/link";
import type { Metadata } from "next";
import { ExternalLink } from "lucide-react";
import { CodeBlock } from "@/components/code-block";

const DESC =
  "Why a request returns 404 DEPLOYMENT_NOT_FOUND on shadw, how to read the error ID, and how to fix each cause — wrong URL, deleted or superseded deployment, or an unattached custom domain — and how it differs from 503 DEPLOYMENT_NOT_READY.";

export const metadata: Metadata = {
  title: "DEPLOYMENT_NOT_FOUND — shadw Docs",
  description: DESC,
  alternates: { canonical: "/docs/errors/deployment-not-found" },
  openGraph: {
    title: "DEPLOYMENT_NOT_FOUND — shadw Docs",
    description: DESC,
    url: "/docs/errors/deployment-not-found",
    type: "website",
    siteName: "shadw",
  },
  twitter: {
    card: "summary_large_image",
    title: "DEPLOYMENT_NOT_FOUND — shadw Docs",
    description: DESC,
  },
};

function Section({ id, title, children }: { id: string; title: string; children: React.ReactNode }) {
  return (
    <section id={id} className="scroll-mt-20 border-t border-border pt-12 first:border-0 first:pt-0">
      <h2 className="text-2xl font-semibold tracking-tight sm:text-3xl">{title}</h2>
      <div className="mt-5 space-y-4 text-[15px] leading-relaxed text-secondary">{children}</div>
    </section>
  );
}

function Cause({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-border p-5">
      <h3 className="text-[15px] font-semibold text-fg">{title}</h3>
      <div className="mt-2 space-y-2 text-[15px] leading-relaxed text-secondary">{children}</div>
    </div>
  );
}

export default function DeploymentNotFoundPage() {
  return (
    <div className="mx-auto max-w-3xl space-y-12 pb-24">
      <header>
        <div className="mb-2 font-mono text-xs uppercase tracking-widest text-link">Errors</div>
        <h1 className="text-3xl font-semibold tracking-tight sm:text-4xl">DEPLOYMENT_NOT_FOUND</h1>
        <p className="mt-4 text-[15px] leading-relaxed text-secondary">
          A request was made for a deployment that does not exist. The hostname resolved and reached
          a shadw edge node, but neither that node nor the peers it knows of have a deployment or
          project for that host. The response is{" "}
          <code className="font-mono text-sm">404</code> with the header{" "}
          <code className="font-mono text-sm">x-hive-error: DEPLOYMENT_NOT_FOUND</code>.
        </p>
        <p className="mt-3 text-[15px] leading-relaxed text-secondary">
          This is a routing answer, not a crash. Your app was never invoked, so there are no runtime
          logs for the request — which is exactly how you tell it apart from an application error.
        </p>
      </header>

      <Section id="error-id" title="Read the error ID first">
        <p>
          Every 404 screen carries a unique ID. It is the fastest way to identify which node answered:
        </p>
        <CodeBlock
          tabs={[
            {
              label: "404 response",
              lang: "bash",
              code: "Code: `DEPLOYMENT_NOT_FOUND`\nID: `sfo1::a1b2c-1785527146044-3f9c2db1e77a`",
            },
          ]}
        />
        <p>
          The part before <code className="font-mono text-sm">::</code> is the region that served the
          response. That matters because shadw resolves the public hostnames round-robin across the
          fleet: if only some requests 404, compare the region prefix on a failing response against a
          succeeding one — a difference points at one node, not at your deployment.
        </p>
      </Section>

      <Section id="causes" title="Common causes">
        <div className="grid gap-4">
          <Cause title="The URL is wrong or the deployment was deleted">
            <p>
              The most common case. A per-deployment URL points at one immutable build; deleting the
              deployment, or deleting the project, retires that URL permanently. Re-check the address
              against the deployment list in your dashboard.
            </p>
          </Cause>
          <Cause title="You are using a preview URL for a superseded build">
            <p>
              Preview and commit URLs are pinned to a specific commit. Promoting or rolling back
              production does not move them. For an address that always follows the current
              production build, use the project&apos;s production domain rather than a commit URL.
            </p>
          </Cause>
          <Cause title="A custom domain is not attached to a project">
            <p>
              DNS can point at shadw while no project claims the hostname — the request arrives and
              matches nothing. Attach the domain to the project under its domain settings, then
              re-request. Until it is attached, every request for that host returns this error.
            </p>
          </Cause>
          <Cause title="The fleet knows the host but nothing is ready to serve it">
            <p>
              This case returns <code className="font-mono text-sm">503</code>, not this 404. When a
              deployment record or a project claims the host but this node has nothing ready to
              serve for it (a build in progress, a failed or cancelled build, no completed build
              yet, or no healthy route to the node that hosts it), the response carries the header{" "}
              <code className="font-mono text-sm">x-hive-error: DEPLOYMENT_NOT_READY</code>, a{" "}
              <code className="font-mono text-sm">retry-after</code> of 30 seconds and a page that
              names the project and the reason. A 404 means neither this node nor its peers know
              the host.
            </p>
          </Cause>
        </div>
      </Section>

      <Section id="checks" title="Quick checks">
        <p>Confirm what the edge actually returned, including which node and why:</p>
        <CodeBlock
          tabs={[
            {
              label: "curl",
              lang: "bash",
              code: "curl -sI https://your-app.shadw.app/ | grep -i 'x-hive-'",
            },
          ]}
        />
        <p>
          <code className="font-mono text-sm">x-hive-error</code> confirms the classification and{" "}
          <code className="font-mono text-sm">x-hive-region</code> names the serving region. If the
          header is absent entirely, the response did not come from a shadw edge node at all — check
          that the hostname&apos;s DNS still points here before debugging anything else.
        </p>
      </Section>

      <Section id="related" title="Related">
        <p>
          shadw&apos;s error code and 404 screen intentionally mirror Vercel&apos;s, so a URL or
          workflow moved between the two platforms surfaces the same diagnosis. Vercel documents the
          equivalent error here:
        </p>
        <p>
          <a
            href="https://vercel.com/docs/errors/deployment_not_found"
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-1.5 text-link underline decoration-border underline-offset-4 hover:decoration-current"
          >
            Vercel — DEPLOYMENT_NOT_FOUND <ExternalLink className="h-3.5 w-3.5" />
          </a>
        </p>
        <p className="pt-2">
          <Link
            href="/docs/getting-started#domains"
            className="text-fg underline decoration-border underline-offset-4 hover:decoration-fg"
          >
            Domains &amp; TLS
          </Link>{" "}
          covers attaching a custom domain, and{" "}
          <Link
            href="/docs/getting-started#regions"
            className="text-fg underline decoration-border underline-offset-4 hover:decoration-fg"
          >
            Regions
          </Link>{" "}
          explains how a request finds the node hosting your deployment.
        </p>
      </Section>
    </div>
  );
}
