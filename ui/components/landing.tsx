"use client";

import { useRef } from "react";
import { MarqueeBanner } from "@/components/marquee-banner";
import { MarketingShell } from "@/components/marketing-shell";
import { IntroLoader } from "@/components/landing/intro-loader";
import { Hero } from "@/components/landing/hero";
import { StackStrip } from "@/components/landing/stack-strip";
import { HowItWorks } from "@/components/landing/how-it-works";
import { Bento } from "@/components/landing/bento";
import { ProductShowcase } from "@/components/landing/product-showcase";
import { Developer } from "@/components/landing/developer";
import { UnderTheHood } from "@/components/landing/under-the-hood";
import { ClosingCta } from "@/components/landing/closing-cta";
import { useReveal } from "@/components/landing/lp-utils";
import "@/components/landing/landing.css";

/** The signed-out home page: hero, brand ticker, then the sections in attention-interest-desire-action order. */
export function Landing() {
  const root = useRef<HTMLDivElement>(null);
  useReveal(root);
  return (
    <MarketingShell>
      <IntroLoader />
      <div ref={root} className="lp">
        <Hero />
        <MarqueeBanner />
        <StackStrip />
        <HowItWorks />
        <Bento />
        <ProductShowcase />
        <Developer />
        <UnderTheHood />
        <ClosingCta />
      </div>
    </MarketingShell>
  );
}
