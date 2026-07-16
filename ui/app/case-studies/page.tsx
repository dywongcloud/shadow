import Link from "next/link";
import type { Metadata } from "next";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";
import { ArrowRight } from "lucide-react";

// Perf: prerender this public page (see root layout note).
export const dynamic = "force-static";
export const revalidate = 3600;

export const metadata: Metadata = {
  title: "Case Studies — shadw",
  description:
    "How real teams cut costs and ship faster on shadw's self-hosted, peer-to-peer cloud — cheaper AI inference, hundreds of per-client preview deploys, full data-residency compliance, and nine-second rollbacks under peak traffic.",
  alternates: { canonical: "/case-studies" },
  openGraph: {
    title: "Case Studies — shadw",
    description:
      "How real teams cut costs and ship faster on shadw's self-hosted, peer-to-peer cloud — cheaper AI inference, hundreds of per-client preview deploys, full data-residency compliance, and nine-second rollbacks under peak traffic.",
    url: "/case-studies",
    type: "website",
    siteName: "shadw",
  },
  twitter: {
    card: "summary_large_image",
    title: "Case Studies — shadw",
    description:
      "How real teams cut costs and ship faster on shadw's peer-to-peer cloud — cheaper AI inference, per-client preview deploys, data-residency compliance, and instant rollbacks.",
  },
};


const HEADLINE_METRICS = [
  { value: "40%", label: "lower infrastructure cost on average" },
  { value: "6x", label: "faster deploys across teams" },
  { value: "99.99%", label: "uptime on the peer-to-peer cloud" },
];

const CASES = [
  {
    company: "Northwind AI",
    industry: "AI / ML",
    summary:
      "Moved model inference off hyperscaler GPUs and onto Fluid compute, scaling capacity peer-to-peer without renegotiating reserved instances.",
    metric: "55%",
    metricLabel: "cheaper inference",
    quote:
      "We doubled throughput and still cut our bill in half. shadw let us own the hardware story without owning a datacenter.",
    person: "Priya Nadeem, VP Engineering",
  },
  {
    company: "Atlas Agency",
    industry: "Digital agency",
    summary:
      "Spins up isolated preview deploys per client on demand, so stakeholders review real environments instead of screenshots.",
    metric: "200+",
    metricLabel: "preview deploys per week",
    quote:
      "Every client gets a live, self-hosted preview in seconds. It changed how we sell and how we ship.",
    person: "Marcus Lee, Founder",
  },
  {
    company: "Helio Fintech",
    industry: "Fintech",
    summary:
      "Self-hosts the entire stack to keep customer data inside required regions, passing audits without bolting on extra vendors.",
    metric: "100%",
    metricLabel: "data residency compliance",
    quote:
      "Data residency used to block deals. With shadw the data never leaves where it should, and auditors love it.",
    person: "Sofia Ramos, Head of Compliance",
  },
  {
    company: "Orbit Commerce",
    industry: "E-commerce",
    summary:
      "Leaned on instant rollback during peak traffic, reverting a bad release mid-event before customers ever noticed.",
    metric: "9s",
    metricLabel: "to roll back on Black Friday",
    quote:
      "A bad deploy hit at our busiest minute. One click later we were back. shadw saved our Black Friday.",
    person: "Dev Okafor, Director of Platform",
  },
];

export default function CaseStudiesPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Case Studies"
        title={
          <>
            Teams that{" "}
            <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">
              own their cloud
            </span>
          </>
        }
        subtitle="How teams ship faster and cut costs on the peer-to-peer cloud."
      />

      <MarketingSection className="py-16">
        <div className="grid gap-8 sm:grid-cols-3">
          {HEADLINE_METRICS.map((metric) => (
            <div key={metric.value} className="text-center">
              <div className="bg-gradient-to-r from-emerald-300 to-emerald-500 bg-clip-text text-4xl font-bold text-transparent">
                {metric.value}
              </div>
              <div className="mt-2 text-sm text-zinc-400">{metric.label}</div>
            </div>
          ))}
        </div>
      </MarketingSection>

      <MarketingSection>
        <div className="mb-10">
          <h2 className="text-3xl font-bold tracking-tight text-white">
            Stories from the network
          </h2>
          <p className="mt-3 text-zinc-400">
            Real teams running production workloads on infrastructure they control.
          </p>
        </div>
        <div className="grid gap-6 md:grid-cols-2">
          {CASES.map((study) => (
            <Link key={study.company} href="/contact" className="group block">
              <GlowCard>
                <div className="flex items-start justify-between gap-4">
                  <h3 className="text-lg font-bold text-white transition-colors group-hover:text-emerald-300">
                    {study.company}
                  </h3>
                  <span className="shrink-0 rounded-full border border-white/10 px-3 py-1 text-xs font-medium text-zinc-300">
                    {study.industry}
                  </span>
                </div>

                <p className="mt-4 text-zinc-400">{study.summary}</p>

                <div className="mt-6 border-t border-white/10 pt-6">
                  <div className="bg-gradient-to-r from-emerald-300 to-emerald-500 bg-clip-text text-3xl font-bold text-transparent">
                    {study.metric}
                  </div>
                  <div className="mt-1 text-sm text-zinc-400">{study.metricLabel}</div>
                </div>

                <blockquote className="mt-6 italic text-zinc-300">
                  &ldquo;{study.quote}&rdquo;
                </blockquote>
                <div className="mt-3 text-sm text-zinc-500">{study.person}</div>
              </GlowCard>
            </Link>
          ))}
        </div>
      </MarketingSection>

      <MarketingSection>
        <GlowCard className="text-center">
          <h2 className="text-3xl font-bold tracking-tight text-white">
            Ready to own your cloud?
          </h2>
          <p className="mx-auto mt-4 max-w-xl text-zinc-400">
            Join the teams shipping faster and cutting costs on the self-hosted,
            peer-to-peer cloud. Get started in minutes or talk to our team about
            your workload.
          </p>
          <div className="mt-8 flex flex-wrap items-center justify-center gap-4">
            <Link
              href="/sign-up"
              className="inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black transition-transform hover:scale-[1.03]"
            >
              Start building
              <ArrowRight className="h-4 w-4" />
            </Link>
            <Link
              href="/contact"
              className="inline-flex items-center gap-2 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10"
            >
              Talk to sales
            </Link>
          </div>
        </GlowCard>
      </MarketingSection>
    </MarketingShell>
  );
}
