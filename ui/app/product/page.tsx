import Link from "next/link";
import type { Metadata } from "next";
import { pageMeta } from "@/lib/page-meta";
import {
  MarketingShell,
  MarketingHero,
  MarketingSection,
  GlowCard,
} from "@/components/marketing-shell";
import {
  ArrowRight,
  Zap,
  Box,
  Database,
  Network,
  GitBranch,
  GitFork,
  Hammer,
  Radar,
} from "lucide-react";

export const metadata: Metadata = pageMeta({
  title: "Product",
  socialTitle: "Product — shadw",
  description:
    "shadw is a cloud you can self-host: serverless functions, containers, and managed databases, with Vercel-style Git deploys, immutable builds, and rollback to earlier deployments.",
  twitterDescription:
    "shadw is a cloud you can self-host: serverless functions, containers, and managed databases, with Vercel-style Git deploys and rollback to earlier deployments.",
  path: "/product",
});


const CAPABILITIES = [
  {
    icon: Zap,
    title: "Serverless Functions",
    desc: "Ship request-driven functions that stay warm in production, scale previews to zero when idle, and run on nodes picked by the scheduler — no servers to manage.",
  },
  {
    icon: Box,
    title: "Containers (Dockerfile)",
    desc: "Bring your own Dockerfile. Run any language or runtime as a long-lived container, scheduled onto healthy nodes.",
  },
  {
    icon: Database,
    title: "Managed Databases",
    desc: "Postgres, Redis and SQLite databases per project, each with its own connection details and running on a single host node.",
  },
  {
    icon: Network,
    title: "Request Routing",
    desc: "Platform DNS favours the node that already serves your app, and a request that arrives at another node is forwarded to the one that runs your deployment. One active owner coordinates the nodes with standby failover, and shared state is eventually consistent.",
  },
  {
    icon: GitBranch,
    title: "GitOps deploys",
    desc: "Push to Git and we build an immutable deployment automatically — Vercel-style previews, and rollback to any earlier deployment.",
  },
];

const STEPS = [
  {
    icon: GitFork,
    title: "Connect a repo",
    desc: "Link your Git repository. Every push triggers a build; major frameworks are detected, and a monorepo with several apps needs a root directory.",
  },
  {
    icon: Hammer,
    title: "Build immutable deployment",
    desc: "We build your functions and containers into a versioned, content-addressed artifact.",
  },
  {
    icon: Radar,
    title: "Route to the right node",
    desc: "Platform DNS favours the node that already serves your app, and a request that arrives at another node is forwarded to the one that runs your deployment.",
  },
];

export default function ProductPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Platform"
        title={
          <>
            One cloud for{" "}
            <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">
              what you ship
            </span>
          </>
        }
        subtitle="Serverless functions, containers, and managed databases on one platform — with Vercel-style deploys straight from Git. Hosted, or self-hosted on machines you operate."
      >
        <Link
          href="/sign-up"
          className="group inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
        >
          Start building
          <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
        </Link>
        <Link
          href="/docs"
          className="inline-flex items-center gap-2 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10"
        >
          Read the docs
        </Link>
      </MarketingHero>

      <MarketingSection className="py-16">
        <div className="max-w-2xl">
          <h2 className="text-3xl font-bold tracking-tight text-white">
            Everything you ship, in one place
          </h2>
          <p className="mt-3 text-zinc-400">
            Compute, storage, and networking primitives on one platform, hosted or
            on nodes you can operate yourself.
          </p>
        </div>

        <div className="mt-10 grid gap-5 sm:grid-cols-2 lg:grid-cols-3">
          {CAPABILITIES.map(({ icon: Icon, title, desc }) => (
            <GlowCard key={title}>
              <div className="flex h-10 w-10 items-center justify-center rounded-lg border border-white/10 bg-white/5">
                <Icon className="h-5 w-5 text-white" />
              </div>
              <h3 className="mt-4 text-base font-bold text-white">{title}</h3>
              <p className="mt-2 text-sm text-zinc-400">{desc}</p>
            </GlowCard>
          ))}
        </div>
      </MarketingSection>

      <MarketingSection className="py-16">
        <div className="max-w-2xl">
          <h2 className="text-3xl font-bold tracking-tight text-white">
            How it works
          </h2>
          <p className="mt-3 text-zinc-400">
            From a Git push to a served deployment in three steps.
          </p>
        </div>

        <div className="mt-10 grid gap-5 md:grid-cols-3">
          {STEPS.map(({ icon: Icon, title, desc }, i) => (
            <GlowCard key={title}>
              <div className="flex items-center gap-4">
                <span className="bg-gradient-to-r from-emerald-400 via-cyan-300 to-violet-400 bg-clip-text text-4xl font-bold tabular-nums tracking-tight text-transparent">
                  {String(i + 1).padStart(2, "0")}
                </span>
                <div className="flex h-10 w-10 items-center justify-center rounded-lg border border-white/10 bg-white/5">
                  <Icon className="h-5 w-5 text-white" />
                </div>
              </div>
              <h3 className="mt-5 text-base font-bold text-white">{title}</h3>
              <p className="mt-2 text-sm text-zinc-400">{desc}</p>
            </GlowCard>
          ))}
        </div>
      </MarketingSection>

      <MarketingSection className="py-16">
        <GlowCard className="text-center">
          <div className="mx-auto max-w-2xl py-8">
            <h2 className="text-3xl font-bold tracking-tight text-white">
              Deploy your first project today
            </h2>
            <p className="mt-3 text-zinc-400">
              Connect a repo and push to deploy.
            </p>
            <div className="mt-8 flex justify-center">
              <Link
                href="/sign-up"
                className="group inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
              >
                Start building
                <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
              </Link>
            </div>
          </div>
        </GlowCard>
      </MarketingSection>
    </MarketingShell>
  );
}
