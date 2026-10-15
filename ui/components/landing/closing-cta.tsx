import Link from "next/link";
import { ArrowRight } from "lucide-react";
import { Section } from "./section";
import { delay } from "./lp-utils";

export function ClosingCta() {
  return (
    <Section className="pb-24 pt-28 sm:pb-32 sm:pt-36">
      <div className="lp-cta-panel lp-reveal px-6 py-20 text-center sm:px-12 sm:py-28" style={delay(0)}>
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-0 -z-10"
          style={{
            background:
              "radial-gradient(620px 340px at 12% 0%, rgba(168,85,247,.32), transparent 66%), radial-gradient(620px 340px at 88% 100%, rgba(34,211,238,.26), transparent 66%), radial-gradient(700px 260px at 50% 120%, rgba(37,99,235,.34), transparent 70%)",
          }}
        />
        <div aria-hidden="true" className="lp-grid -z-10 opacity-50" />
        <h2 className="lp-h2 mx-auto max-w-[760px]">
          Start with <span className="lp-gradient-text">one repository.</span>
        </h2>
        <p className="lp-lead mx-auto mt-5 max-w-[560px]">Sign up, connect a repository and watch the first build run. The rest is the same workflow, repeated.</p>
        <div className="mt-10 flex flex-wrap items-center justify-center gap-3.5">
          <Link href="/sign-up" className="lp-btn">
            Start building <ArrowRight className="h-[18px] w-[18px]" />
          </Link>
          <Link href="/docs/getting-started" className="lp-btn-ghost">
            Getting started
          </Link>
        </div>
      </div>
    </Section>
  );
}
