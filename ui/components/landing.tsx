"use client";

import Link from "next/link";
import { ArrowRight } from "lucide-react";
import { MarqueeBanner } from "@/components/marquee-banner";
import { MarketingShell } from "@/components/marketing-shell";
import { GlobeWireframe } from "@/components/globe-wireframe";
import { NetworkCode } from "@/components/network-code";

/* ------------------------------------------------------------------ *
 * shadw — public landing page. Renders inside the shared MarketingShell
 * (top nav + footer) and forces a dark palette regardless of app theme.
 * ------------------------------------------------------------------ */

/* eslint-disable @next/next/no-img-element */
export function Landing() {
  return (
    <MarketingShell>
      {/* ---------------- Hero ---------------- */}
      <section className="relative overflow-hidden">
        {/* Aurora glows: two equal sources on the left and right that bleed inward
            and converge at the navbar centre (top) and a lower horizon band — all
            layered BELOW the hero text (the navbar stays on top, z-20).

            RESPONSIVE: every size / blur / rem-offset is `min(rem, N vw)` (or
            `max(-rem, -N vw)` for negative offsets) against a 1280px reference
            (1rem ↔ 1.25vw). At ≥1280px the `rem` wins → pixel-identical to the
            desktop design; below 1280px the `vw` term wins, so the whole glow
            field scales down proportionally — keeping the globe ringed by black on
            phones/tablets instead of being washed out. The %-based positions are
            already viewport-relative and stay as-is. */}
        <div className="pointer-events-none absolute inset-x-0 top-0 z-0 h-[min(32rem,40vw)]">
          {/* Left source — leans toward centre. */}
          <div className="absolute top-[max(-8rem,-10vw)] left-[6%] h-[min(26rem,32.5vw)] w-[min(26rem,32.5vw)] rounded-full bg-teal-400/14 blur-[min(130px,10.16vw)]" />
          <div className="absolute top-[max(-6rem,-7.5vw)] left-[28%] h-[min(24rem,30vw)] w-[min(24rem,30vw)] rounded-full bg-indigo-600/14 blur-[min(140px,10.94vw)]" />
          {/* Right source — mirror of the left, leans toward centre. */}
          <div className="absolute top-[max(-8rem,-10vw)] right-[6%] h-[min(26rem,32.5vw)] w-[min(26rem,32.5vw)] rounded-full bg-cyan-400/14 blur-[min(130px,10.16vw)]" />
          <div className="absolute top-[max(-6rem,-7.5vw)] right-[28%] h-[min(24rem,30vw)] w-[min(24rem,30vw)] rounded-full bg-fuchsia-600/14 blur-[min(140px,10.94vw)]" />
          {/* Convergence into the navbar centre (top-centre blend of the two sides). */}
          <div className="absolute top-[max(-7rem,-8.75vw)] left-1/2 h-[min(20rem,25vw)] w-[min(42rem,52.5vw)] -translate-x-1/2 rounded-full bg-indigo-500/12 blur-[min(150px,11.72vw)]" />
          {/* Lower-horizon convergence — a wide, flat band the two sides melt into. */}
          <div className="absolute bottom-0 left-1/2 h-[min(8rem,10vw)] w-[72%] -translate-x-1/2 rounded-[100%] bg-sky-500/12 blur-[min(130px,10.16vw)]" />
        </div>

        {/* Solutions-page hero glows, copied 1:1 from <MarketingHero>: a left
            violet/pink cluster and a right cyan/sky cluster. Made responsive with
            the same `min(rem, vw)` / `max(-rem, -vw)` scheme (1280px reference) so
            they shrink and pull inward on small screens while staying pixel-exact
            to the Solutions hero at ≥1280px. */}
        <div className="pointer-events-none absolute inset-0">
          <div className="absolute left-[max(-10rem,-12.5vw)] top-0 h-[min(40rem,50vw)] w-[min(40rem,50vw)] rounded-full bg-violet-700/25 blur-[min(140px,10.94vw)]" />
          <div className="absolute left-[max(-5rem,-6.25vw)] top-[min(4rem,5vw)] h-[min(22rem,27.5vw)] w-[min(22rem,27.5vw)] rounded-full bg-pink-500/30 blur-[min(110px,8.59vw)]" />
          <div className="absolute right-[max(-10rem,-12.5vw)] top-[min(10rem,12.5vw)] h-[min(34rem,42.5vw)] w-[min(34rem,42.5vw)] rounded-full bg-cyan-500/15 blur-[min(140px,10.94vw)]" />
          <div className="absolute right-[max(-4rem,-5vw)] top-[min(13rem,16.25vw)] h-[min(20rem,25vw)] w-[min(20rem,25vw)] rounded-full bg-sky-400/30 blur-[min(110px,8.59vw)]" />
        </div>

        <div className="relative z-10 mx-auto max-w-5xl px-6 pb-2 pt-[3.15rem] text-center sm:pt-[4.2rem]">
          <h1 className="text-balance text-5xl font-bold uppercase leading-[1.05] tracking-tight text-white sm:text-7xl">
            Own the Edge.
            <br />
            <span className="italic">Run in the Shadows</span>
          </h1>
          <p className="mx-auto mt-7 max-w-xl text-lg leading-relaxed text-zinc-400">
            Unleash the Power of Peer-to-Peer: Seamlessly Connect, Collaborate, and Conquer with Our
            Cutting-Edge Cloud
          </p>
          <div className="mt-10 flex items-center justify-center gap-4">
            <Link
              href="/sign-up"
              className="group inline-flex items-center gap-2 rounded-full bg-white px-6 py-3 text-sm font-semibold text-black transition-transform hover:scale-[1.03]"
            >
              Start Now <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
            </Link>
            <a
              href="#demo"
              className="inline-flex items-center gap-2 rounded-full bg-white px-6 py-3 text-sm font-medium text-black transition-colors hover:bg-white/90"
            >
              Watch Demo
            </a>
          </div>
        </div>

        {/* Animated globe wireframe directly below the hero text + buttons. The blue
            circuit pulses stay frozen until the visitor first interacts. */}
        <GlobeWireframe className="mx-auto mt-4 w-full max-w-6xl px-4" />
      </section>

      {/* ---------------- Scrolling "Own Your Cloud." banner (after the globe) ---------------- */}
      <MarqueeBanner />

      {/* ---------------- P2P network graphic + code-art overlay (below the banner) ---------------- */}
      <NetworkCode />

      {/* ---------------- Device showcase (dead space above for contrast) ---------------- */}
      <section id="demo" className="relative scroll-mt-20 px-6 pb-28 pt-36 sm:pt-44">
        <div className="pointer-events-none absolute inset-0">
          <div className="absolute left-0 top-1/3 h-[30rem] w-[30rem] rounded-full bg-violet-700/20 blur-[150px]" />
          <div className="absolute right-0 top-1/4 h-[30rem] w-[30rem] rounded-full bg-cyan-500/15 blur-[150px]" />
        </div>
        <div className="relative mx-auto max-w-5xl" style={{ perspective: "2200px" }}>
          <div
            className="overflow-hidden rounded-2xl border border-white/10 bg-[#0b0b10] p-2 shadow-[0_40px_120px_-20px_rgba(0,0,0,0.9)]"
            style={{ transform: "rotateX(6deg) rotateY(-12deg)", transformStyle: "preserve-3d" }}
          >
            <div className="flex justify-center pb-2 pt-1">
              <span className="h-1.5 w-1.5 rounded-full bg-white/25" />
            </div>
            <img
              src="/shadw-device.png"
              alt="shadw Command Center"
              className="w-full rounded-xl border border-white/5"
            />
          </div>
        </div>
      </section>

    </MarketingShell>
  );
}
