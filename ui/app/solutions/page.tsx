import Link from "next/link";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";
import {
  ArrowRight,
  Check,
  Rocket,
  Users,
  Building2,
  Brain,
  Globe,
  Server,
  FileText,
  Clock,
  Radio,
  Wrench,
} from "lucide-react";

const audiences = [
  {
    icon: Rocket,
    title: "Startups",
    desc: "Ship fast and scale on a budget. Deploy from Git, pay for what you use, and let the mesh handle the rest.",
    bullets: ["Zero-config Git deploys", "Generous free tier", "Scale to zero when idle"],
  },
  {
    icon: Users,
    title: "Agencies",
    desc: "Run multi-tenant teams with clean isolation. Spin up preview deploys for every client and every branch.",
    bullets: ["Preview deploys per client", "Team & project roles", "White-label friendly"],
  },
  {
    icon: Building2,
    title: "Enterprises",
    desc: "Self-host on your own mesh with full control. Keep data where it belongs and govern compute with leases.",
    bullets: ["Data residency by region", "Lease-based capacity", "Run on your own nodes"],
  },
  {
    icon: Brain,
    title: "AI apps",
    desc: "Fluid compute for inference workloads, durable data for state, and edge delivery for low-latency UX.",
    bullets: ["Burstable inference compute", "Durable, replicated data", "Edge-served responses"],
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
    desc: "Containers and functions wired into the mesh with built-in routing.",
  },
  {
    icon: FileText,
    title: "Static sites & docs",
    desc: "Globally cached pages served from the nearest peer in the network.",
  },
  {
    icon: Clock,
    title: "Background jobs / cron",
    desc: "Scheduled and queued work that runs reliably across the mesh.",
  },
  {
    icon: Radio,
    title: "Realtime & edge",
    desc: "Low-latency sockets and edge functions close to your users.",
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
        subtitle="shadw is a self-hosted, peer-to-peer cloud — serverless functions, containers, edge, and durable data over a P2P mesh, with Vercel-style Git deploys. One platform that adapts to your team and your workloads."
      >
        <Link
          href="/sign-up"
          className="group inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
        >
          Get started
          <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
        </Link>
        <Link
          href="/contact"
          className="inline-flex items-center gap-2 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10"
        >
          Talk to us
        </Link>
      </MarketingHero>

      {/* By audience */}
      <MarketingSection className="py-16">
        <div className="max-w-2xl">
          <h2 className="text-3xl font-bold tracking-tight">Solutions for every team</h2>
          <p className="mt-3 text-zinc-400">
            From your first deploy to a self-hosted global mesh, shadw meets you where you are.
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

      {/* By use case */}
      <MarketingSection className="py-16">
        <div className="max-w-2xl">
          <h2 className="text-3xl font-bold tracking-tight">By use case</h2>
          <p className="mt-3 text-zinc-400">
            Whatever you&apos;re building, the mesh has a primitive for it.
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

      {/* Closing CTA */}
      <MarketingSection className="py-16">
        <GlowCard className="text-center">
          <h2 className="text-3xl font-bold tracking-tight text-white">
            Ship your first deploy on the mesh
          </h2>
          <p className="mx-auto mt-3 max-w-xl text-zinc-400">
            Connect a repo and push to deploy — serverless, containers, edge, and durable data, all
            on a peer-to-peer cloud you control.
          </p>
          <div className="mt-8 flex justify-center">
            <Link
              href="/sign-up"
              className="group inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
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
