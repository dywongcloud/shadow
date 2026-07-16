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
  Globe,
  Database,
  Network,
  GitBranch,
  GitFork,
  Hammer,
  Radar,
} from "lucide-react";

// Perf: prerender this public page (see root layout note).
export const dynamic = "force-static";
export const revalidate = 3600;

export const metadata: Metadata = {
  title: "Product — shadw",
  description:
    "shadw is a self-hosted, peer-to-peer cloud: serverless functions, containers, edge functions, and durable data (GuardianDB) over an Iroh QUIC mesh, with Vercel-style Git deploys, immutable builds, and instant rollbacks.",
  alternates: { canonical: "/product" },
  openGraph: {
    title: "Product — shadw",
    description:
      "shadw is a self-hosted, peer-to-peer cloud: serverless functions, containers, edge functions, and durable data (GuardianDB) over an Iroh QUIC mesh, with Vercel-style Git deploys, immutable builds, and instant rollbacks.",
    url: "/product",
    type: "website",
    siteName: "shadw",
  },
  twitter: {
    card: "summary_large_image",
    title: "Product — shadw",
    description:
      "shadw is a self-hosted, peer-to-peer cloud: serverless functions, containers, edge functions, and durable data over an Iroh QUIC mesh, with Vercel-style Git deploys and instant rollbacks.",
  },
};


const CAPABILITIES = [
  {
    icon: Zap,
    title: "Serverless Functions",
    desc: "Ship request-driven functions that scale to zero and spin up instantly across the mesh — no servers to manage.",
  },
  {
    icon: Box,
    title: "Containers (Dockerfile)",
    desc: "Bring your own Dockerfile. Run any language or runtime as a long-lived container, scheduled onto healthy peers.",
  },
  {
    icon: Globe,
    title: "Edge Functions",
    desc: "Execute lightweight logic at the edge, milliseconds from your users, with global routing baked in.",
  },
  {
    icon: Database,
    title: "Durable Data (GuardianDB)",
    desc: "Replicated, always-on storage that survives node churn — strongly consistent state without a central database.",
  },
  {
    icon: Network,
    title: "P2P Mesh (Iroh QUIC)",
    desc: "Nodes discover and connect directly over QUIC, forming an encrypted, NAT-traversing peer-to-peer fabric.",
  },
  {
    icon: GitBranch,
    title: "GitOps deploys",
    desc: "Push to Git and we build an immutable deployment automatically — Vercel-style previews and instant rollbacks.",
  },
];

const STEPS = [
  {
    icon: GitFork,
    title: "Connect a repo",
    desc: "Link your Git repository. Every push triggers a build with zero pipeline configuration.",
  },
  {
    icon: Hammer,
    title: "Build immutable deployment",
    desc: "We compile your functions, containers, and edge logic into a versioned, reproducible artifact.",
  },
  {
    icon: Radar,
    title: "Serve from the nearest node",
    desc: "The mesh routes each request to the closest healthy peer for the lowest possible latency.",
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
        subtitle="A self-hosted, peer-to-peer cloud platform. Serverless functions, containers, edge, and durable data running over an encrypted P2P mesh — with Vercel-style deploys straight from Git."
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

      {/* Capabilities */}
      <MarketingSection className="py-16">
        <div className="max-w-2xl">
          <h2 className="text-3xl font-bold tracking-tight text-white">
            Everything you ship, in one mesh
          </h2>
          <p className="mt-3 text-zinc-400">
            Compute, storage, and networking primitives that run on your own
            nodes — no centralized cloud, no lock-in.
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

      {/* How it works */}
      <MarketingSection className="py-16">
        <div className="max-w-2xl">
          <h2 className="text-3xl font-bold tracking-tight text-white">
            How it works
          </h2>
          <p className="mt-3 text-zinc-400">
            From a Git push to a globally served deployment in three steps.
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

      {/* Closing CTA */}
      <MarketingSection className="py-16">
        <GlowCard className="text-center">
          <div className="mx-auto max-w-2xl py-8">
            <h2 className="text-3xl font-bold tracking-tight text-white">
              Deploy your first node today
            </h2>
            <p className="mt-3 text-zinc-400">
              Spin up the mesh, connect a repo, and watch your code serve from
              everywhere at once.
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
