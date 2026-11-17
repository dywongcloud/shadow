import type { Metadata } from "next";
import Link from "next/link";
import { ArrowRight } from "lucide-react";
import { darkViewport, pageMeta } from "@/lib/page-meta";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";
import { Tok } from "@/components/landing/tok";
import { COMPARISONS } from "@/lib/compare-data";
import { CompareNav } from "@/components/compare/compare-nav";
import "@/components/landing/landing.css";

export const metadata: Metadata = pageMeta({
  title: "Compare",
  socialTitle: "Compare shadw with Vercel, Railway, Fly.io and Cloudflare",
  description:
    "See how shadw compares with Vercel, Railway, Fly.io and Cloudflare: Git deploys, previews, managed data and the choice to run on your own machines, with sources linked on every page.",
  twitterDescription:
    "Git deploys, previews, managed data and the choice to self-host. Four comparisons, with sources.",
  path: "/compare",
});
export const viewport = darkViewport;

export default function CompareIndex() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Compare"
        title={
          <>
            One workflow. <span className="lp-gradient-text">Four alternatives.</span>
          </>
        }
        subtitle="Push to Git and get a live URL, a preview per pull request, managed data and the choice to run it on your own machines. Here is how that compares, in plain terms."
      >
        <Link href="/sign-up" className="lp-btn">
          Start building <ArrowRight className="h-[18px] w-[18px]" />
        </Link>
        <Link href="/docs/getting-started" className="lp-btn-ghost">
          Getting started
        </Link>
      </MarketingHero>

      <CompareNav active="" />

      <MarketingSection className="py-16 sm:py-20">
        <h2 className="sr-only">All comparisons</h2>
        <div className="grid gap-5 md:grid-cols-2">
          {COMPARISONS.map((c) => (
            <GlowCard key={c.slug}>
              <p className="lp-eyebrow lp-mono">shadw vs.</p>
              <h3 className="lp-card-title mt-3 !text-2xl font-semibold text-white"><Tok>{c.rival}</Tok></h3>
              <p className="lp-card-copy mt-3 min-h-[72px]">{c.summary}</p>
              <Link
                href={`/compare/${c.slug}`}
                className="tap-44 group mt-5 inline-flex min-h-[44px] items-center gap-2 text-sm font-medium text-white transition-colors hover:text-zinc-300"
              >
                Read the comparison <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
              </Link>
            </GlowCard>
          ))}
        </div>
      </MarketingSection>

      <MarketingSection className="pb-24">
        <GlowCard className="text-center">
          <h2 className="lp-h2 mx-auto max-w-[760px]">The workflow stays the same.</h2>
          <p className="lp-lead mx-auto mt-4 max-w-[600px]">
            Connect a repository, push, and share a real running URL. Every comparison here is written against what each provider publishes, with sources linked on the page.
          </p>
          <div className="mt-8 flex flex-wrap items-center justify-center gap-3.5">
            <Link href="/sign-up" className="lp-btn">
              Start building <ArrowRight className="h-[18px] w-[18px]" />
            </Link>
            <Link href="/pricing" className="lp-btn-ghost">See pricing</Link>
          </div>
        </GlowCard>
      </MarketingSection>
    </MarketingShell>
  );
}
