import Link from "next/link";
import { ArrowRight, Check } from "lucide-react";
import { MarketingShell, MarketingSection, GlowCard } from "@/components/marketing-shell";
import { Tok } from "@/components/landing/tok";
import { CompareNav } from "@/components/compare/compare-nav";
import type { Comparison } from "@/lib/compare-data";

function Hero({ c }: { c: Comparison }) {
  return (
    <section className="lp lp-hero">
      <div className="lp-bg" aria-hidden="true">
        <div className="lp-aurora-a" />
        <div className="lp-aurora-b" />
        <div className="lp-grid" />
        <div className="lp-grain" />
      </div>
      <div className="relative z-10 mx-auto max-w-[1200px] px-6 pb-14 pt-14 sm:pt-20">
        <Link
          href="/compare"
          className="lp-pill tap-44 inline-flex min-h-[44px] !px-4 !py-1.5 uppercase !tracking-[0.18em] !text-[12px]"
        >
          Compare
        </Link>

        <div className="lp-cta-panel mt-8 px-6 py-12 text-center sm:px-12 sm:py-16">
          <div
            aria-hidden="true"
            className="pointer-events-none absolute inset-0 -z-10"
            style={{
              background:
                "radial-gradient(372px 204px at 12% 0%, rgba(168,83,186,.30), transparent 66%), radial-gradient(372px 204px at 88% 100%, rgba(42,138,246,.26), transparent 66%), radial-gradient(420px 156px at 50% 120%, rgba(37,99,235,.30), transparent 70%)",
            }}
          />
          <div aria-hidden="true" className="lp-grid -z-10 opacity-50" />
          <p aria-hidden="true" className="flex flex-wrap items-center justify-center gap-x-5 gap-y-2 text-[clamp(28px,5vw,56px)] font-medium leading-[1.1] tracking-tight">
            <span className="lp-gradient-text">shadw</span>
            <span className="lp-mono text-[0.42em] uppercase tracking-[0.2em] text-zinc-500">vs.</span>
            <span className="text-white"><Tok>{c.rival}</Tok></span>
          </p>
        </div>

        <h1
          className="lp-h1 lp-h1-page mx-auto mt-12 max-w-[900px]"
          style={{ fontSize: "clamp(28px, 3.4vw, 48px)", lineHeight: 1.12 }}
        >
          <span className="text-zinc-500">shadw vs. <Tok>{c.rival}</Tok>.</span>{" "}{c.heroLead}
        </h1>
        <p className="lp-sub mx-auto mt-6 max-w-[720px]">{c.heroSub}</p>

        <div className="mt-9 flex flex-wrap items-center justify-center gap-3.5">
          <Link href="/sign-up" className="lp-btn">
            Start building <ArrowRight className="h-[18px] w-[18px]" />
          </Link>
          <Link href="/docs/getting-started" className="lp-btn-ghost">
            Getting started
          </Link>
        </div>
      </div>
    </section>
  );
}

function SectionHead({ eyebrow, title, lead }: { eyebrow: string; title: React.ReactNode; lead?: string }) {
  return (
    <div className="max-w-[820px]">
      <p className="lp-eyebrow lp-mono">{eyebrow}</p>
      <h2 className="lp-h2 mt-4">{title}</h2>
      {lead && <p className="lp-lead mt-5 max-w-[700px]">{lead}</p>}
    </div>
  );
}

function Approaches({ c }: { c: Comparison }) {
  const cols = [
    { n: "01", title: `How ${c.rival} works`, body: c.rivalApproach },
    { n: "02", title: "How shadw works", body: c.shadwApproach },
  ];
  return (
    <MarketingSection className="py-16 sm:py-20">
      <SectionHead
        eyebrow="Two approaches"
        title={
          <>
            Two different approaches. <span className="text-zinc-500">{c.approachesLead}</span>
          </>
        }
      />
      <div className="mt-12 grid gap-5 md:grid-cols-2">
        {cols.map((col) => (
          <GlowCard key={col.n}>
            <div className="flex items-center gap-3 border-b border-white/10 pb-4">
              <span className="lp-mono text-[13px] tabular-nums text-zinc-500">{col.n}</span>
              <h3 className="lp-card-title font-semibold text-white">{col.title}</h3>
            </div>
            <p className="lp-card-copy mt-5">{col.body}</p>
          </GlowCard>
        ))}
      </div>
    </MarketingSection>
  );
}

function Differences({ c }: { c: Comparison }) {
  return (
    <MarketingSection className="py-16 sm:py-20">
      <SectionHead
        eyebrow="What changes"
        title={
          <>
            What shadw does differently. <span className="text-zinc-500">{c.differencesLead}</span>
          </>
        }
      />
      <div className="mt-12 grid gap-5 md:grid-cols-2">
        {c.differences.map((d, i) => (
          <GlowCard key={d.title}>
            <div className="flex items-start gap-4">
              <span className="lp-mono shrink-0 text-[15px] tabular-nums text-zinc-500">
                {String(i + 1).padStart(2, "0")}
              </span>
              <div className="min-w-0">
                <h3 className="lp-card-title font-semibold text-white">{d.title}</h3>
                <p className="lp-card-copy mt-3">{d.body}</p>
              </div>
            </div>
          </GlowCard>
        ))}
      </div>
    </MarketingSection>
  );
}

function Cell({ text }: { text: string }) {
  return (
    <span className="flex min-w-0 gap-2.5">
      <Check className="mt-[3px] h-4 w-4 shrink-0 text-white" aria-hidden="true" />
      <span className="min-w-0 break-words text-zinc-200">{text}</span>
    </span>
  );
}

function ComparisonTable({ c }: { c: Comparison }) {
  return (
    <MarketingSection className="py-16 sm:py-20">
      <SectionHead eyebrow="At a glance" title="The key differences at a glance" />
      <div className="mt-10 overflow-x-auto">
        <table className="w-full min-w-[560px] border-collapse text-left text-[15px] max-md:block max-md:min-w-0">
          <caption className="sr-only">shadw and {c.rival} compared</caption>
          <thead className="max-md:block">
            <tr className="border-b border-white/20 max-md:grid max-md:grid-cols-2">
              <td className="max-md:hidden" />
              <th scope="col" className="w-[40%] py-4 pr-4 text-[18px] font-medium text-white max-md:w-auto">
                shadw
              </th>
              <th scope="col" className="w-[40%] py-4 text-[18px] font-medium text-zinc-300 max-md:w-auto">
                <Tok>{c.rival}</Tok>
              </th>
            </tr>
          </thead>
          <tbody className="max-md:block">
            {c.rows.map((row) => (
              <tr key={row.label} className="border-b border-white/10 align-top max-md:grid max-md:grid-cols-2 max-md:py-4">
                <th scope="row" className="py-5 pr-4 font-normal text-zinc-400 max-md:col-span-2 max-md:py-0 max-md:pb-2">
                  {row.label}
                </th>
                <td className="py-5 pr-4 max-md:py-0"><Cell text={row.shadw} /></td>
                <td className="py-5 max-md:py-0"><Cell text={row.rival} /></td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </MarketingSection>
  );
}

function Fit({ c }: { c: Comparison }) {
  return (
    <MarketingSection className="py-16 sm:py-20">
      <SectionHead eyebrow="When to choose" title="shadw is a strong fit if…" />
      <ul className="mt-10 grid gap-4 md:grid-cols-2">
        {c.fit.map((item) => (
          <li key={item} className="lp-well flex min-h-[76px] items-start gap-3 p-5">
            <Check className="mt-1 h-4 w-4 shrink-0 text-white" aria-hidden="true" />
            <span className="min-w-0 text-[15px] leading-relaxed text-zinc-300">{item}</span>
          </li>
        ))}
      </ul>
    </MarketingSection>
  );
}

function Sources({ c }: { c: Comparison }) {
  return (
    <MarketingSection className="pb-10">
      <div className="border-t border-white/10 pt-8">
        <p className="lp-caption text-[14px] text-zinc-400">As of {c.asOf}, from what {c.rival} publishes.</p>
        <div className="mt-3 flex flex-wrap items-center gap-x-3 gap-y-1">
          <span className="text-[14px] text-zinc-400">Sources:</span>
          {c.sources.map((s) => (
            <a
              key={s.href}
              href={s.href}
              target="_blank"
              rel="noopener noreferrer"
              className="tap-44 inline-flex min-h-[44px] items-center text-[14px] text-zinc-300 underline underline-offset-4 transition-colors hover:text-white"
            >
              <Tok>{s.label}</Tok>
            </a>
          ))}
        </div>
      </div>
    </MarketingSection>
  );
}

function ClosingCta() {
  return (
    <MarketingSection className="pb-24 pt-10 sm:pb-28">
      <div className="lp-cta-panel px-6 py-16 text-center sm:px-12 sm:py-20">
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-0 -z-10"
          style={{
            background:
              "radial-gradient(372px 204px at 12% 0%, rgba(168,83,186,.32), transparent 66%), radial-gradient(372px 204px at 88% 100%, rgba(42,138,246,.26), transparent 66%), radial-gradient(420px 156px at 50% 120%, rgba(37,99,235,.34), transparent 70%)",
          }}
        />
        <div aria-hidden="true" className="lp-grid -z-10 opacity-50" />
        <h2 className="lp-h2 mx-auto max-w-[760px]">
          Start with <span className="lp-gradient-text">one repository.</span>
        </h2>
        <p className="lp-lead mx-auto mt-5 max-w-[560px]">
          Sign up, connect a repository and watch the first build run. Free to start, with a live URL and a preview per pull request.
        </p>
        <div className="mt-9 flex flex-wrap items-center justify-center gap-3.5">
          <Link href="/sign-up" className="lp-btn">
            Start building <ArrowRight className="h-[18px] w-[18px]" />
          </Link>
          <Link href="/pricing" className="lp-btn-ghost">
            See pricing
          </Link>
        </div>
      </div>
    </MarketingSection>
  );
}

export function ComparePage({ c }: { c: Comparison }) {
  return (
    <MarketingShell>
      <Hero c={c} />
      <CompareNav active={c.slug} />
      <Approaches c={c} />
      <Differences c={c} />
      <ComparisonTable c={c} />
      <Fit c={c} />
      <Sources c={c} />
      <ClosingCta />
    </MarketingShell>
  );
}
