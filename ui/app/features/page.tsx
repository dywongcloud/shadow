import Link from "next/link";
import type { Metadata } from "next";
import { darkViewport, pageMeta } from "@/lib/page-meta";
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
  Database,
  Clock,
  Activity,
  ShieldCheck,
  Share2,
  ArrowRight,
  Check,
} from "lucide-react";

export const metadata: Metadata = pageMeta({
  title: "Features",
  socialTitle: "Features — shadw",
  description:
    "Explore shadw's platform: Git deploys, immutable builds, rollback to earlier deployments, preview URLs, custom domains & DNS, secrets, serverless functions, containers, managed databases, cron & workflows, logs and metrics, and experimental ZK preview access.",
  twitterDescription:
    "Git deploys, immutable builds, rollback, previews, custom domains, secrets, serverless functions, containers, managed databases, cron, and logs and metrics.",
  path: "/features",
});
export const viewport = darkViewport;


const FEATURES = [
  {
    icon: GitBranch,
    title: "Git Deploys",
    desc: "Push to deploy. Framework detection covers Next.js, Vite, SvelteKit and more; a monorepo with several apps needs a root directory.",
  },
  {
    icon: Boxes,
    title: "Immutable Deployments",
    desc: "Every build is an immutable, content-addressed deployment. Nothing mutates in place, and you can delete a deployment you no longer need.",
  },
  {
    icon: Undo2,
    title: "Rollback",
    desc: "Promote any earlier deployment back to production to recover from a bad release.",
  },
  {
    icon: Eye,
    title: "Preview Deployments",
    desc: "Every pull request on a connected GitHub repository gets its own live URL, so you can review real running code before you merge.",
  },
  {
    icon: Globe,
    title: "Custom Domains & DNS",
    desc: "Bring your own domain with managed DNS and automatic TLS. Wildcard subdomains for previews come standard.",
  },
  {
    icon: KeyRound,
    title: "Environment & Secrets",
    desc: "Per-environment variables, scoped to build, runtime or both. Sensitive values are encrypted at rest.",
  },
  {
    icon: Zap,
    title: "Serverless Functions",
    desc: "Production stays warm and previews scale to zero when idle, with no servers to manage.",
  },
  {
    icon: Container,
    title: "Containers",
    desc: "Ship anything with a Dockerfile. Long-running services and custom runtimes deploy alongside your functions.",
  },
  {
    icon: Database,
    title: "Managed Databases",
    desc: "Managed Postgres, Redis and SQLite databases for your projects. Each database runs on a single host node.",
  },
  {
    icon: Clock,
    title: "Cron & Workflows",
    desc: "Schedule jobs with cron and run multi-step workflows, with a console to inspect, cancel and replay runs. Each cron job runs on a single node.",
  },
  {
    icon: Activity,
    title: "Logs & Metrics",
    desc: "Request logs and metrics for your deployments, polled from the dashboard. See requests, errors and cold-start timings.",
  },
  {
    icon: ShieldCheck,
    title: "ZK Preview Access",
    desc: "Experimental. Team members unlock protected previews with a linkable ring-signature proof: the serving node learns that some member signed, not which one.",
  },
  {
    icon: Share2,
    title: "Request Routing",
    desc: "Platform DNS favours the node that already serves your app, and a request that arrives at another node is forwarded to the one that runs your deployment. One active owner coordinates the nodes, with standby failover.",
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
        subtitle="The developer experience you already love — Git deploys, rollback, previews, managed databases, and logs and metrics — on a cloud you can self-host."
      >
        <Link
          href="/sign-up"
          className="group inline-flex items-center gap-2 tap-44 rounded-full border border-transparent bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
        >
          Start free
          <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
        </Link>
        <Link
          href="/pricing"
          className="inline-flex items-center gap-2 tap-44 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10"
        >
          See pricing
        </Link>
      </MarketingHero>

      <MarketingSection className="py-16">
        <h2 className="sr-only">Platform features</h2>
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

      <MarketingSection className="py-16">
        <div className="grid items-center gap-10 lg:grid-cols-2">
          <div>
            <h2 className="text-3xl font-bold tracking-tight text-white">Deploy with a git push</h2>
            <p className="mt-3 text-zinc-400">
              Connect a repository and push. shadw detects supported frameworks, builds an immutable deployment,
              and serves it — no servers to provision.
            </p>
            <ul className="mt-6 space-y-3">
              {[
                "Automatic framework detection; a monorepo with several apps needs a root directory",
                "A unique deployment URL for every push, and a preview for every pull request on a connected GitHub repository",
                "Promote or roll back to any earlier deployment",
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
              <div className="text-zinc-400">→ Deploying…</div>
              <div className="text-emerald-400">✓ Deployed to app.localhost</div>
            </div>
          </GlowCard>
        </div>
      </MarketingSection>

      <MarketingSection className="py-16">
        <GlowCard className="text-center">
          <h2 className="text-3xl font-bold tracking-tight text-white">Ship on shadw today</h2>
          <p className="mx-auto mt-3 max-w-xl text-zinc-400">
            Get the deploy experience you love on a cloud you can self-host. The hosted service uses Clerk,
            Stripe and Vercel DNS.
          </p>
          <div className="mt-8 flex justify-center">
            <Link
              href="/sign-up"
              className="group inline-flex items-center gap-2 tap-44 rounded-full border border-transparent bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
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
