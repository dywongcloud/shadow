import Link from "next/link";
import type { Metadata } from "next";
import { pageMeta } from "@/lib/page-meta";
import { ArrowRight, Database, ShieldCheck, Sparkles, Boxes, Workflow, Radio, Cpu, KeyRound } from "lucide-react";
import { CodeBlock } from "@/components/code-block";
import {
  DocsPage,
  DocsSection,
  DocsH3,
  DocsIn,
  DocsCallout,
  DocsFigure,
  DocsCard,
} from "@/components/docs/docs-ui";

export const metadata: Metadata = pageMeta({
  title: "Platform — Docs",
  socialTitle: "Platform — shadw Docs",
  description:
    "What the shadw platform ships today: builds and deployments, AI Gateway, usage metering, managed data, sandboxes, security, mesh networking and self-hosting. With real dashboard captures and worked examples.",
  twitterDescription:
    "What shadw ships today: builds, AI Gateway, usage metering, managed data, security and mesh networking.",
  path: "/docs/platform",
});

const FEATURES = [
  { icon: Boxes, title: "Builds & deployments", desc: "Git or Dockerfile builds, framework detection, preview URLs, instant rollback and no-cache redeploys.", href: "/docs/getting-started#deploying" },
  { icon: Sparkles, title: "AI Gateway", desc: "One provider pool, profitable per-model price cards and a bounded server-side web search tool.", href: "/docs/platform#ai-gateway" },
  { icon: Radio, title: "Usage metering", desc: "19 real cumulative counters at stored-record boundaries; charging stays opt-in.", href: "/docs/platform#metering" },
  { icon: Database, title: "Managed data", desc: "Per-database SQLite files, a CRDT browser database, and managed Postgres and Redis records.", href: "/docs/platform#managed-data" },
  { icon: Cpu, title: "Sandboxes & isolation", desc: "A capability-probed Litebox backend on macOS plus Linux cell backends; placement never assumes a runtime.", href: "/docs/platform#sandboxes" },
  { icon: ShieldCheck, title: "Security", desc: "Attack Challenge Mode, sensitive-path denies, optional account 2FA and a signed contract oplog.", href: "/docs/platform#security" },
  { icon: Workflow, title: "Queues, cron & workflows", desc: "Tenant-held queue tokens, node-local cron and durable workflow runs.", href: "/docs/platform#workloads" },
  { icon: KeyRound, title: "Credentials & keys", desc: "Server-side credential resolution, encrypted host-only cookies and a fail-closed at-rest key.", href: "/docs/getting-started#env" },
];

export default function PlatformPage() {
  return (
    <DocsPage
      eyebrow="Platform"
      title="What shadw ships today"
      lead="One node binary, a Git-to-live-URL build lane, and the services around it. This page describes the platform as it stands in November 2026 — not a roadmap."
      back={{ href: "/docs", label: "← Back to docs" }}
      wide
    >
      <DocsSection id="shipped" eyebrow="Overview" title="The shipped surface">
        <p>
          The dashboard below is a real capture of a seeded local node, not an illustration. It shows the
          deployments view: status, environment, source and creation time per build. The two captures on
          this page are from 2026-10-14 and show the dashboard shell, not the November features described
          in the sections that follow.
        </p>
        <DocsFigure
          src="/product/dashboard-deployments.png"
          alt="shadw dashboard, deployments tab: a project named storefront with three deployment rows showing status ready, environment Production or Preview, source commit and creation time."
          caption="Deployments view — status, environment, source and time per build."
          note="Real 2x capture of a seeded local node, dark theme, 2026-10-14. These frames predate the November 2026 features (AI Gateway, usage metering, Attack Challenge Mode, 2FA and managed data); they are shown to document the dashboard shell, not as evidence of those features. Recapture on a seeded node is an operator task."
          width={2880}
          height={1800}
        />
        <p>
          The projects view shows the same shell with the project list. Both frames are unretouched.
        </p>
        <DocsFigure
          src="/product/dashboard-projects.png"
          alt="shadw dashboard, projects tab: a grid of project cards with names, deployment status and recent activity."
          caption="Projects view — the project list and its per-project status."
          note="Real 2x capture of a seeded local node, dark theme, 2026-10-14. Same provenance and date caveat as the deployments frame above."
          width={2880}
          height={1800}
        />
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          {FEATURES.map((f) => (
            <DocsCard key={f.title} icon={<f.icon className="h-6 w-6" />} title={f.title} desc={f.desc} href={f.href} />
          ))}
        </div>
      </DocsSection>

      <DocsSection id="ai-gateway" eyebrow="AI" title="AI Gateway">
        <p>
          The AI Gateway exposes one REST surface over a provider pool with bounded failover. Per-model
          price cards are required to clear the platform profitability floor before charging, and charging
          stays behind an explicit opt-in. A bounded server-side web search tool and current model aliases
          are available to gateway requests. Every request is server-side; a tenant never names a URL
          by interpolation — all outbound addresses are validated at connect time and on every redirect.
        </p>
        <CodeBlock
          tabs={[
            {
              label: "curl",
              lang: "bash",
              filename: "gateway.sh",
              code: `curl -X POST https://api.shadw.cloud/v1/chat/completions \\
  -H 'content-type: application/json' \\
  -H 'Authorization: Bearer hive_YOUR_API_KEY' \\
  -d '{ "model": "claude-opus-5-5",
        "messages": [{"role": "user", "content": "Summarize the last deploy log."}] }'`,
            },
            {
              label: "TypeScript",
              lang: "ts",
              filename: "gateway.ts",
              code: `const res = await fetch("https://api.shadw.cloud/v1/chat/completions", {
  method: "POST",
  headers: {
    "content-type": "application/json",
    Authorization: \`Bearer \${process.env.HIVE_API_KEY}\`,
  },
  body: JSON.stringify({
    model: "claude-opus-5-5",
    messages: [{ role: "user", content: "Summarize the last deploy log." }],
  }),
});`,
            },
          ]}
        />
        <DocsCallout tone="warn">
          A model price is bought or granted, never self-assigned; a requested plan resolves through the
          known-plan catalog. Treat the gateway example above as a shape, not a billing authorization.
        </DocsCallout>
      </DocsSection>

      <DocsSection id="metering" eyebrow="Billing" title="Usage metering">
        <p>
          Real cumulative counters are wired at single-writer boundaries, plus a canonical catalog of
          billable usage constructs. The dashboard renders measured-or-absent values: a counter that has
          not been observed does not print zero. Charging is off until explicitly enabled, so a roll of
          the metering code does not itself start charging.
        </p>
        <DocsH3>What the dashboard shows</DocsH3>
        <p>
          Prices come only from the rate card, never inferred from a request. A deployment that never
          started is reported as a failure class of its own (a warm-fail streak), and a broken deployment
          reports <DocsIn>DEPLOYMENT_CIRCUIT_OPEN</DocsIn>, not a capacity error. Those are the two shapes
          a developer sees in the UI; the distinction is load-bearing, so the docs name it.
        </p>
      </DocsSection>

      <DocsSection id="managed-data" eyebrow="Data" title="Managed data">
        <p>
          Managed SQLite (<DocsIn>DbKind::Sqlite</DocsIn>) and the browser database are separate lanes:
          one file per database under <DocsIn>$HIVE_DATA/sqlite-dbs/</DocsIn>, versus one CRDT database
          per project under <DocsIn>$HIVE_DATA/browser-dbs/</DocsIn>. Never point the Hrana/musql lane at
          a browser-database file: a plain writer there bypasses the replication clock. Browser grants
          ride the admission, are server-derived and tenant-pinned, and are re-checked on every request.
          Caps bind with a typed refusal and a whole-batch rollback rather than truncating a value.
        </p>
        <DocsH3>Managed Postgres and Redis records</DocsH3>
        <p>
          A managed database is reachable only through its own hostname record, from its host node, with
          the gateway splicing on that same node. A host that leaves the node registry strands the
          database: it is reported, never silently re-hosted, because re-provisioning yields an empty
          database. Getting the host name wrong is not a recoverable typo.
        </p>
        <DocsCallout>
          Lakebed binds <DocsIn>ctx.db</DocsIn> and <DocsIn>storage</DocsIn> to Blob storage by preset and
          detection only — it is not a hosted source runtime. See the worked examples for a binding.
        </DocsCallout>
      </DocsSection>

      <DocsSection id="sandboxes" eyebrow="Isolation" title="Sandboxes & isolation">
        <p>
          Capability is probed and advertised, never assumed; <DocsIn>None</DocsIn> means NOT CAPABLE.
          A node is Litebox only after a live probe passes on that host with the exact staged runner. A
          native macOS Litebox traps Linux-ABI ELF syscalls and applies a Seatbelt profile before guest
          code runs, but it is not Firecracker-class isolation: guest and enforcement share one address
          space, with seccomp-bpf only as a backstop. Placement gates through one shared predicate, so a
          missing runtime is a node fault, not a scoring penalty.
        </p>
        <p>
          A route that executes tenant commands checks the active backend. The sandbox route refuses on
          the Mock backend, which runs on the host shell.
        </p>
      </DocsSection>

      <DocsSection id="security" eyebrow="Security" title="Security">
        <p>
          Attack Challenge Mode terminates a failed solve: a rejected nonce or a failed fetch shows a
          visible error and a retry, with a bounded attempt counter. Solves set a sealed cookie and a
          no-store response. Sensitive-path denies and a firewall event ring are available to operators.
          Optional per-account TOTP lives inside the token mint, with a session-scoped step-up that carries
          forward only from a verified prior token.
        </p>
        <DocsH3>Tamper evidence, not consensus</DocsH3>
        <p>
          The contract oplog is data, not code: canonical bytes are length-prefixed and a refused write
          is recorded as a measurement, not a prevention. There is no consensus and no bond model —
          honesty comes from replay. A signed node-local evidence log is technical evidence, never a
          compliance attestation. Read the platform docs before claiming a contract prevents anything.
        </p>
        <DocsCallout tone="warn">
          Secrets, MACs and cookies that decide access are read with the honest decrypt path; a bad or
          unpersistable key refuses to boot rather than regenerating. Never rotate the fleet at-rest key
          without keeping the previous one available. Tenant processes get their environment from nothing.
        </DocsCallout>
        <p>
          The QUIC transport offers hybrid X25519MLKEM768 first; transport identity stays classical
          ed25519. That distinction is deliberate.
        </p>
      </DocsSection>

      <DocsSection id="workloads" eyebrow="Runtime" title="Queues, cron & workflows">
        <p>
          The queue token is the one tenant-held bearer: it is confined to enqueue and stats, age-bounded,
          and rotated in each local deployment&apos;s function environment after boot and on a schedule.
          Cron is node-local, so one scheduler replaces its whole set rather than adding to it. Workflow
          runs are durable and observable in the dashboard console.
        </p>
        <p>
          Model downloads share one node-wide slot count, one disk claim and one tenant slot, so a slow
          drip cannot trip a stall timeout. Anything a request path reserves is released by a Drop guard,
          including when a client disconnects mid-request.
        </p>
      </DocsSection>

      <DocsSection id="whats-new" eyebrow="Operations" title="What's new">
        <p>Recent shipped changes, newest first. History and full rationale live in the repository changelog.</p>
        <ul className="list-disc space-y-2 pl-5">
          <li><strong className="text-fg">2026-11-24</strong> — macOS nodes build and install without Xcode or Command Line Tools, using a shared Homebrew/zig toolchain lane. Challenge solves terminate on failure.</li>
          <li><strong className="text-fg">2026-11-20</strong> — fail2ban gains a long-window jail; SSH stays on 22; scanner-only secret paths are denied while developer routes remain reachable. Contract checkpoint writes are race-safe; an explicit region pin is honored, and a no-cache redeploy no longer returns a bare 500. Guarded, expiring IP deny lists are generated locally with TTL bounds rather than scraped.</li>
          <li><strong className="text-fg">2026-11-19</strong> — AI Gateway gains a bounded web search tool and current model aliases, plus a provider pool, a profitable rate card and a dashboard tab. A single canonical catalog of billable usage constructs lands. Firewall attack challenge mode, sensitive-path denial and bot management ship.</li>
          <li><strong className="text-fg">2026-11-18</strong> — macOS node boot no longer runs a Cargo <DocsIn>check</DocsIn> on the host; the whole platform adopts Xcode-free build wiring for macOS nodes.</li>
        </ul>
        <p>
          <Link href="/docs/getting-started" className="tap-44 inline-flex items-center gap-1.5 text-link hover:underline">
            Continue with the getting started guide <ArrowRight className="h-3.5 w-3.5" />
          </Link>
        </p>
      </DocsSection>
    </DocsPage>
  );
}
