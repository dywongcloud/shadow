import Link from "next/link";
import { ArrowRight, Play } from "lucide-react";
import { GlobeWireframe } from "@/components/globe-wireframe";
import { DeployTerminal } from "./deploy-terminal";
import { DomeLight } from "./dome-light";
import { PreviewCard } from "./preview-card";
import { delay } from "./lp-utils";

/** What the cards need below the globe from the `lg` breakpoint up: the finished terminal's height (about 375 px in a 974 px dome, 309 px in a 1160 px one)
 *  less the 54 % of the globe's height it overlaps, fitted as a line in the dome's width because a padding percentage is a share of the width. */
const CARDS_OVERHANG = "max(0px, calc(725px - 55.6%))";

/** The hero: one dominant luminous object (the globe dome under its tracers) with the product shown doing its job in two glass panels. Every
 *  claim is one the code backs; the dome prints no count and names no region, and says so. */
export function Hero() {
  return (
    <section className="lp-hero pb-14 lg:pb-24">
      <div className="lp-bg" aria-hidden="true">
        <div className="lp-aurora-a" />
        <div className="lp-aurora-b" />
        <div className="lp-grid" />
        <div className="lp-grain" />
      </div>

      <div className="relative z-10 mx-auto max-w-[1100px] px-6 pt-12 sm:pt-[60px]">
        <p className="lp-rise" style={delay(0)}>
          <span className="lp-pill">
            <b>Free</b>
            Start on the Hobby plan
          </span>
        </p>
        <h1 className="lp-h1 lp-rise mt-7" style={delay(90)}>
          Push to Git.
          <br />
          <span className="lp-gradient-text">Get a live URL.</span>
        </h1>
        <p className="lp-sub lp-rise mx-auto mt-6 max-w-[640px]" style={delay(180)}>
          Deploy functions, containers and static sites from Git, a zip upload or a container image, with managed databases beside them. Every
          pull request on a connected GitHub repository gets a live preview.
        </p>
        <div className="lp-rise mt-9 flex flex-wrap items-center justify-center gap-x-3.5 gap-y-4" style={delay(270)}>
          <Link href="/sign-up" className="lp-btn">
            Start building <ArrowRight className="h-[18px] w-[18px]" />
          </Link>
          <Link href="/docs" className="lp-btn-ghost">
            Read the docs
          </Link>
          <a
            href="https://www.youtube.com/watch?v=qp_lhPioZF4"
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex h-[44px] items-center gap-2 rounded-full px-4 text-[15px] text-zinc-300 transition-colors hover:text-white"
          >
            <Play className="h-4 w-4 fill-current" /> Watch the demo
          </a>
        </div>
      </div>

      <div className="lp-dome lp-rise mt-[76px] px-6" style={delay(360)}>
        <div className="relative">
          <GlobeWireframe className="relative -ml-[34vw] w-[168vw] sm:ml-0 sm:w-full" boxClassName="lp-dome-mask">
            <DomeLight />
          </GlobeWireframe>
          <DeployTerminal className="relative z-10 mx-auto -mt-12 w-[min(560px,calc(100%-32px))] lg:absolute lg:left-0 lg:top-[46%] lg:mx-0 lg:mt-0 lg:w-[min(540px,45%)]" />
          <PreviewCard className="absolute right-0 top-[30%] z-10 hidden w-[320px] lg:block" />
          <span className="lp-chip lp-mono absolute -top-[46px] right-0 z-10 !text-[11px] uppercase tracking-[0.14em] text-[var(--lp-dim)] sm:top-0">Illustrative</span>
        </div>
        <div aria-hidden="true" className="hidden lg:block" style={{ paddingBottom: CARDS_OVERHANG }} />
        <p className="lp-mono mx-auto mt-7 max-w-[420px] px-6 text-center text-[12px] uppercase leading-[1.7] tracking-[0.16em] text-[var(--lp-dim)] lg:mt-8">
          Illustrative. Not a map of our nodes.
        </p>
      </div>
    </section>
  );
}
