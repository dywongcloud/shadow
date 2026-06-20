"use client";

import Link from "next/link";
import { ArrowRight } from "lucide-react";
import { MarqueeBanner } from "@/components/marquee-banner";
import { MarketingShell } from "@/components/marketing-shell";

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
        <div className="relative mx-auto max-w-5xl px-6 pb-2 pt-24 text-center sm:pt-32">
          <h1 className="text-balance text-5xl font-bold leading-[1.05] tracking-tight text-white sm:text-7xl">
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

        {/* Animated globe wireframe directly below the hero text + buttons. */}
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img
          src="/globe-wireframe.svg"
          alt=""
          aria-hidden="true"
          className="pointer-events-none mx-auto mt-4 w-full max-w-6xl select-none px-4"
        />
      </section>

      {/* ---------------- Scrolling "Own Your Cloud." banner (after the globe) ---------------- */}
      <MarqueeBanner />

      {/* ---------------- Device showcase ---------------- */}
      <section id="demo" className="relative scroll-mt-20 px-6 pb-28 pt-10">
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
