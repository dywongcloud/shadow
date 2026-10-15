import Link from "next/link";
import type { Metadata } from "next";
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

export const metadata: Metadata = {
  title: "Product — shadw",
  description:
    "shadw is a self-hostable, peer-to-peer cloud: serverless functions, containers, and managed databases over an Iroh QUIC mesh, with Vercel-style Git deploys, immutable builds, and rollback to earlier deployments.",
  alternates: { canonical: "/product" },
  openGraph: {
    title: "Product — shadw",
    description:
      "shadw is a self-hostable, peer-to-peer cloud: serverless functions, containers, and managed databases over an Iroh QUIC mesh, with Vercel-style Git deploys, immutable builds, and rollback to earlier deployments.",
    url: "/product",
    type: "website",
    siteName: "shadw",
  },
  twitter: {
    card: "summary_large_image",
    title: "Product — shadw",
    description:
      "shadw is a self-hostable, peer-to-peer cloud: serverless functions, containers, and managed databases over an Iroh QUIC mesh, with Vercel-style Git deploys and rollback to earlier deployments.",
  },
};


const CAPABILITIES = [
  {
    icon: Zap,
    title: "Serverless Functions",
    desc: "Ship request-driven functions that scale to zero when idle and run on mesh nodes picked by the scheduler — no servers to manage.",
  },
  {
    icon: Box,
    title: "Containers (Dockerfile)",
    desc: "Bring your own Dockerfile. Run any language or runtime as a long-lived container, scheduled onto healthy peers.",
  },
  {
    icon: Database,
    title: "Managed Databases",
    desc: "Postgres, Redis and SQLite databases per project, each with its own connection details and running on a single host node.",
  },
  {
    icon: Network,
    title: "P2P Mesh (Iroh QUIC)",
    desc: "Nodes discover and connect directly over QUIC, forming an encrypted, NAT-traversing peer-to-peer fabric. One active owner coordinates the mesh with standby failover, and shared state is eventually consistent.",
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
    title: "Serve from nearby nodes",
    desc: "Platform DNS favours nodes near the requester, and the mesh forwards a request to the node that runs the deployment when it arrives elsewhere.",
  },
];

export default function ProductPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Platform"
        title={
          <>
            The cloud,{" "}
            <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">
              peer-to-peer
            </span>
          </>
        }
        subtitle="A self-hostable, peer-to-peer cloud platform. Serverless functions, containers, and managed databases running over an encrypted P2P mesh — with Vercel-style deploys straight from Git."
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
            Everything you ship, in one mesh
          </h2>
          <p className="mt-3 text-zinc-400">
            Compute, storage, and networking primitives that run on a mesh of
            nodes you can operate yourself.
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
              Deploy your first node today
            </h2>
            <p className="mt-3 text-zinc-400">
              Spin up the mesh, connect a repo, and push to deploy.
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
