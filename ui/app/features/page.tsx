import Link from "next/link";
import type { Metadata } from "next";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";
import {
  GitBranch,
  Boxes,
  Undo2,
  Eye,
  Globe,
  KeyRound,
  Zap,
  Container,
  Network,
  Database,
  Clock,
  Activity,
  ShieldCheck,
  Share2,
  ArrowRight,
  Check,
} from "lucide-react";

export const metadata: Metadata = {
  title: "Features — autheo",
  description:
    "Explore autheo's platform: Git deploys, immutable builds, instant rollback, preview URLs, custom domains & DNS, secrets, serverless functions, containers, edge functions, durable data, cron & workflows, observability, and ZK anonymous previews over a peer-to-peer mesh.",
  alternates: { canonical: "/features" },
  openGraph: {
    title: "Features — autheo",
    description:
      "Explore autheo's platform: Git deploys, immutable builds, instant rollback, preview URLs, custom domains & DNS, secrets, serverless functions, containers, edge functions, durable data, cron & workflows, observability, and ZK anonymous previews over a peer-to-peer mesh.",
    url: "/features",
    type: "website",
    siteName: "autheo",
  },
  twitter: {
    card: "summary_large_image",
    title: "Features — autheo",
    description:
      "Git deploys, immutable builds, instant rollback, previews, custom domains, secrets, serverless functions, containers, edge, durable data, cron, and observability over a peer-to-peer mesh.",
  },
};


const FEATURES = [
  {
    icon: GitBranch,
    title: "Git Deploys",
    desc: "Push to deploy. Automatic framework detection builds Next.js, Vite, SvelteKit and more — no config required.",
  },
  {
    icon: Boxes,
    title: "Immutable Deployments",
    desc: "Every build is a permanent, content-addressed snapshot. Nothing mutates in place, so what shipped stays shipped.",
  },
  {
    icon: Undo2,
    title: "Instant Rollback",
    desc: "Promote any previous deployment back to production in a single click. Recover from a bad release in seconds.",
  },
  {
    icon: Eye,
    title: "Preview Deployments",
    desc: "Every branch and pull request gets its own live URL, so you can review real running code before you merge.",
  },
  {
    icon: Globe,
    title: "Custom Domains & DNS",
    desc: "Bring your own domain with managed DNS and automatic TLS. Wildcard subdomains for previews come standard.",
  },
  {
    icon: KeyRound,
    title: "Environment & Secrets",
    desc: "Per-environment variables and secrets, encrypted at rest and injected only into the deployments that need them.",
  },
  {
    icon: Zap,
    title: "Serverless Functions",
    desc: "Fluid compute scales to zero and back instantly. Run request handlers across the mesh with no servers to manage.",
  },
  {
    icon: Container,
    title: "Containers",
    desc: "Ship anything with a Dockerfile. Long-running services and custom runtimes deploy alongside your functions.",
  },
  {
    icon: Network,
    title: "Edge Functions",
    desc: "Run lightweight logic close to your users at peers across the network for low-latency responses everywhere.",
  },
  {
    icon: Database,
    title: "Durable Data",
    desc: "GuardianDB gives you always-on, replicated storage over the mesh — durable state without a central database.",
  },
  {
    icon: Clock,
    title: "Cron & Workflows",
    desc: "Schedule jobs with cron and orchestrate multi-step, durable workflows that resume exactly where they left off.",
  },
  {
    icon: Activity,
    title: "Observability & Logs",
    desc: "Live logs, metrics, and traces for every deployment. See requests, errors, and cold starts in real time.",
  },
  {
    icon: ShieldCheck,
    title: "ZK Anonymous Previews",
    desc: "Share preview deployments behind zero-knowledge access proofs — verify the right viewers without tracking them.",
  },
  {
    icon: Share2,
    title: "P2P Mesh Routing",
    desc: "Traffic is routed peer-to-peer across a self-hosted mesh, with no single point of failure and no cloud lock-in.",
  },
];

export default function FeaturesPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Features"
        title={
          <>
            Everything you need to{" "}
            <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">
              ship
            </span>
          </>
        }
        subtitle="A self-hosted, peer-to-peer cloud with the developer experience you already love — Git deploys, instant rollback, previews, durable data, and observability over a decentralized mesh."
      >
        <Link
          href="/sign-up"
          className="group inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
        >
          Start free
          <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
        </Link>
        <Link
          href="/pricing"
          className="inline-flex items-center gap-2 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10"
        >
          See pricing
        </Link>
      </MarketingHero>

      {/* 1. Feature grid */}
      <MarketingSection className="py-16">
        <div className="grid gap-5 sm:grid-cols-2 lg:grid-cols-3">
          {FEATURES.map(({ icon: Icon, title, desc }) => (
            <GlowCard key={title}>
              <div className="flex h-10 w-10 items-center justify-center rounded-lg border border-white/10 bg-white/5">
                <Icon className="h-5 w-5 text-white" />
              </div>
              <h3 className="mt-4 font-bold text-white">{title}</h3>
              <p className="mt-2 text-sm text-zinc-400">{desc}</p>
            </GlowCard>
          ))}
        </div>
      </MarketingSection>

      {/* 2. Deploy in seconds */}
      <MarketingSection className="py-16">
        <div className="grid items-center gap-10 lg:grid-cols-2">
          <div>
            <h2 className="text-3xl font-bold tracking-tight text-white">Deploy in seconds</h2>
            <p className="mt-3 text-zinc-400">
              Connect a repository and push. autheo detects your framework, builds an immutable deployment,
              and serves it across the mesh — no pipelines to wire up, no servers to provision.
            </p>
            <ul className="mt-6 space-y-3">
              {[
                "Zero-config builds with automatic framework detection",
                "A unique preview URL for every push and pull request",
                "Promote or roll back to any deployment instantly",
              ].map((item) => (
                <li key={item} className="flex items-start gap-3 text-sm text-zinc-300">
                  <span className="mt-0.5 flex h-5 w-5 flex-none items-center justify-center rounded-full border border-emerald-400/30 bg-emerald-400/10">
                    <Check className="h-3 w-3 text-emerald-400" />
                  </span>
                  {item}
                </li>
              ))}
            </ul>
          </div>

          <GlowCard>
            <div className="flex items-center gap-2 border-b border-white/10 pb-4">
              <span className="h-3 w-3 rounded-full bg-red-400/80" />
              <span className="h-3 w-3 rounded-full bg-yellow-400/80" />
              <span className="h-3 w-3 rounded-full bg-emerald-400/80" />
            </div>
            <div className="mt-4 space-y-1.5 font-mono text-sm">
              <div className="text-zinc-200">$ git push</div>
              <div className="text-zinc-400">→ Cloning…</div>
              <div className="text-zinc-400">→ Building (Next.js detected)</div>
              <div className="text-zinc-400">→ Distributing to mesh peers…</div>
              <div className="text-emerald-400">✓ Deployed to app.localhost</div>
            </div>
          </GlowCard>
        </div>
      </MarketingSection>

      {/* 3. Closing CTA */}
      <MarketingSection className="py-16">
        <GlowCard className="text-center">
          <h2 className="text-3xl font-bold tracking-tight text-white">Ship on your own cloud today</h2>
          <p className="mx-auto mt-3 max-w-xl text-zinc-400">
            Get the deploy experience you love, fully self-hosted over a peer-to-peer mesh. No lock-in, no central
            point of failure.
          </p>
          <div className="mt-8 flex justify-center">
            <Link
              href="/sign-up"
              className="group inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
            >
              Start free
              <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
            </Link>
          </div>
        </GlowCard>
      </MarketingSection>
    </MarketingShell>
  );
}
