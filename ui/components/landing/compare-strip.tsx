import Link from "next/link";
import { ArrowRight } from "lucide-react";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";
import { Tok } from "./tok";
import { COMPARE_LINKS } from "@/lib/compare-links";

/** A compact link strip on the landing page: the four comparisons are reachable from the page
 *  content, not only from the shared nav and footer. Copy stays on outcomes, not architecture. */
export function CompareStrip() {
  return (
    <Section className="pt-24 sm:pt-32" wash="duo">
      <SectionHead
        eyebrow="Compare"
        title="A live URL from a Git push."
        lead="Compare shadw with four other ways to ship: a preview per pull request, managed data in the same project, and the choice to run on your own machines."
      />
      <ul className="lp-limb-top relative mt-12 grid gap-px overflow-hidden rounded-3xl border border-white/10 bg-white/10 sm:grid-cols-2 lg:grid-cols-4">
        {COMPARE_LINKS.map((c, i) => (
          <li key={c.slug} className="lp-reveal lp-fact bg-[#050505] p-6" style={delay(i * 80)}>
            <p className="lp-caption text-[12px] uppercase tracking-[0.18em] text-zinc-400">shadw vs.</p>
            <h3 className="lp-card-title mt-2 !text-[20px]"><Tok>{c.rival}</Tok></h3>
            <Link
              href={`/compare/${c.slug}`}
              className="tap-44 group mt-4 inline-flex min-h-[44px] items-center gap-2 text-sm font-medium text-white transition-colors hover:text-zinc-300"
            >
              Read the comparison <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
            </Link>
          </li>
        ))}
      </ul>
    </Section>
  );
}
