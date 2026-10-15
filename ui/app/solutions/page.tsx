import Link from "next/link";
import type { Metadata } from "next";
import { darkViewport, pageMeta } from "@/lib/page-meta";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";
import {
  ArrowRight,
  Check,
  Rocket,
  Users,
  Building2,
  Globe,
  Server,
  FileText,
  Clock,
  Radio,
  Wrench,
} from "lucide-react";

export const metadata: Metadata = pageMeta({
  title: "Solutions",
  socialTitle: "Solutions — shadw",
  description:
    "See how startups, agencies, and enterprises ship on shadw — a cloud you can self-host for full-stack apps, APIs, static sites, cron jobs, realtime, and internal tools, with Vercel-style Git deploys.",
  twitterDescription:
    "See how startups, agencies, and enterprises ship on shadw — a cloud you can self-host for full-stack apps, APIs, static sites, cron, realtime, and internal tools.",
  path: "/solutions",
});
export const viewport = darkViewport;


const audiences = [
  {
    icon: Rocket,
    title: "Startups",
    desc: "Ship fast and scale on a budget. Deploy from Git, start free, and upgrade when you outgrow the included compute.",
    bullets: ["Git deploys with framework detection", "Generous free tier", "Previews scale to zero when idle"],
  },
  {
    icon: Users,
    title: "Agencies",
    desc: "Keep each client in its own team; workloads run on each node's probed isolation backend. Spin up preview deploys for every client and every branch.",
    bullets: ["Preview deploys per client", "Team & project roles"],
  },
  {
    icon: Building2,
    title: "Enterprises",
    desc: "Self-host on your own machines with full control. Choose the regions your workloads run in and whether they fail over.",
    bullets: ["Region-targeted compute placement", "Per-project region failover", "Run on your own nodes"],
  },
];

const useCases = [
  {
    icon: Globe,
    title: "Full-stack web apps",
    desc: "Frontend, backend, and data deployed together from a single repo.",
  },
  {
    icon: Server,
    title: "APIs & microservices",
    desc: "Containers and functions with built-in routing.",
  },
  {
    icon: FileText,
    title: "Static sites & docs",
    desc: "Static output served from nodes, with long-lived caching for content-hashed assets.",
  },
  {
    icon: Clock,
    title: "Background jobs / cron",
    desc: "Cron jobs and managed queues. Each cron job runs on a single node.",
  },
  {
    icon: Radio,
    title: "Realtime",
    desc: "Managed pub/sub topics and WebSocket rooms for realtime apps.",
  },
  {
    icon: Wrench,
    title: "Internal tools",
    desc: "Private dashboards and admin apps with team-scoped access.",
  },
];

export default function SolutionsPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Solutions"
        title={
          <>
            Built for how{" "}
            <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">
              you ship
            </span>
          </>
        }
        subtitle="Serverless functions, containers, and managed databases with Vercel-style Git deploys: one platform that adapts to your team and your workloads, hosted or on machines you operate."
      >
        <Link
          href="/sign-up"
          className="group inline-flex items-center gap-2 tap-44 rounded-full border border-transparent bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
        >
          Get started
          <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
        </Link>
        <Link
          href="/contact"
          className="inline-flex items-center gap-2 tap-44 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10"
        >
          Talk to us
        </Link>
      </MarketingHero>

      <MarketingSection className="py-16">
        <div className="max-w-2xl">
          <h2 className="text-3xl font-bold tracking-tight">Solutions for every team</h2>
          <p className="mt-3 text-zinc-400">
            From your first deploy to machines you run yourself, shadw meets you where you are.
          </p>
        </div>

        <div className="mt-10 grid gap-6 sm:grid-cols-2">
          {audiences.map(({ icon: Icon, title, desc, bullets }) => (
            <GlowCard key={title}>
              <div className="flex h-10 w-10 items-center justify-center rounded-lg border border-white/10 bg-white/5">
                <Icon className="h-5 w-5 text-white" />
              </div>
              <h3 className="mt-4 text-lg font-bold text-white">{title}</h3>
              <p className="mt-2 text-zinc-400">{desc}</p>
              <ul className="mt-4 space-y-2">
                {bullets.map((b) => (
                  <li key={b} className="flex items-center gap-2 text-sm text-zinc-500">
                    <Check className="h-4 w-4 shrink-0 text-emerald-400" />
                    {b}
                  </li>
                ))}
              </ul>
            </GlowCard>
          ))}
        </div>
      </MarketingSection>

      <MarketingSection className="py-16">
        <div className="max-w-2xl">
          <h2 className="text-3xl font-bold tracking-tight">By use case</h2>
          <p className="mt-3 text-zinc-400">
            For each of these, the platform has a primitive built in.
          </p>
        </div>

        <div className="mt-10 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
          {useCases.map(({ icon: Icon, title, desc }) => (
            <GlowCard key={title}>
              <div className="flex h-10 w-10 items-center justify-center rounded-lg border border-white/10 bg-white/5">
                <Icon className="h-5 w-5 text-white" />
              </div>
              <h3 className="mt-4 font-semibold text-white">{title}</h3>
              <p className="mt-2 text-sm text-zinc-400">{desc}</p>
            </GlowCard>
          ))}
        </div>
      </MarketingSection>

      <MarketingSection className="py-16">
        <GlowCard className="text-center">
          <h2 className="text-3xl font-bold tracking-tight text-white">
            Ship your first deploy
          </h2>
          <p className="mx-auto mt-3 max-w-xl text-zinc-400">
            Connect a repo and push to deploy — serverless, containers, and managed databases, all
            on a cloud you can self-host.
          </p>
          <div className="mt-8 flex justify-center">
            <Link
              href="/sign-up"
              className="group inline-flex items-center gap-2 tap-44 rounded-full border border-transparent bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
            >
              Get started
              <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
            </Link>
          </div>
        </GlowCard>
      </MarketingSection>
    </MarketingShell>
  );
}
