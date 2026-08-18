"use client";

import Link from "next/link";
import { useEffect, useState } from "react";
import { ArrowRight } from "lucide-react";
import { MarqueeBanner } from "@/components/marquee-banner";
import { MarketingShell } from "@/components/marketing-shell";
import { GlobeWireframe } from "@/components/globe-wireframe";
import Image from "next/image";

/* ------------------------------------------------------------------ *
 * shadw — public landing page. Renders inside the shared MarketingShell
 * (top nav + footer) and forces a dark palette regardless of app theme.
 * ------------------------------------------------------------------ */

/** The rotating aurora glow ring from the reference video: a conic
 *  rose→violet→teal gradient (violet at top/bottom, rose left, teal right —
 *  sampled from the recording) masked into a soft ring and heavily blurred.
 *  Rotating the element spins the gradient, so the visible arc cycles color
 *  every ~2s exactly like the video. Size/position it via `className`. */
function GlowRing({
  className = "",
  duration = 2.2,
  style,
}: {
  className?: string;
  duration?: number;
  style?: React.CSSProperties;
}) {
  // Soft-feathered ring mask + heavy blur — the video's arc is diffuse, not a
  // hard band, so the mask ramps gently on both edges.
  const ring = "radial-gradient(closest-side, transparent 48%, rgba(0,0,0,0.55) 62%, black 72%, rgba(0,0,0,0.55) 84%, transparent 96%)";
  return (
    <div
      aria-hidden
      className={`pointer-events-none rounded-full ${className}`}
      style={{
        background:
          "conic-gradient(from 0deg, rgba(124,92,196,0.55), rgba(63,160,140,0.6) 25%, rgba(124,92,196,0.55) 50%, rgba(192,80,110,0.62) 75%, rgba(124,92,196,0.55))",
        WebkitMaskImage: ring,
        maskImage: ring,
        filter: "blur(min(42px,3.3vw))",
        animation: `glow-ring-spin ${duration}s linear infinite`,
        ...style,
      }}
    />
  );
}

// Whether the intro already played during THIS page load. Module scope — reset
// only by a real navigation/reload, never by a React remount. The landing↔
// dashboard flip is decided by CLIENT auth state (see home-client.tsx), so
// <Landing> can unmount and REMOUNT within a single page load — and does so
// repeatedly when Clerk's auth state cannot settle (the mobile split-brain
// cookie state). Replaying a 2.75s full-screen black overlay plus a
// documentElement scroll-lock on every remount amplified that thrash into the
// reported "flickers endlessly"; this flag makes the intro strictly
// once-per-page-load. The server never sets it (only the client effect does),
// so SSR always renders the overlay and hydration stays consistent.
let introPlayed = false;

/** Intro loading animation (reference video 1): a full-screen black overlay with
 *  the shadw ghost logo centered inside the rotating GlowRing. ~2s of spin, then
 *  the overlay fades to reveal the hero. Plays once per PAGE LOAD (a client-side
 *  remount of <Landing> — e.g. an auth-state flip — must NOT replay it); skipped
 *  entirely for prefers-reduced-motion. Rendered from first paint (SSR) so
 *  there's no pre-hydration flash of the hero. All layout is inline styles —
 *  deterministic, no class-generation dependency. */
function IntroLoader() {
  // On a remount after the intro already ran this page load, start (and stay)
  // done — no overlay frame at all. On the hydration mount `introPlayed` is
  // still false on both server and client, so SSR and the first client render
  // agree on "show".
  const [phase, setPhase] = useState<"show" | "fade" | "done">(() => (introPlayed ? "done" : "show"));
  useEffect(() => {
    if (introPlayed) {
      setPhase("done"); // no-op when the initializer already started at "done"
      return;
    }
    introPlayed = true;
    if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) {
      setPhase("done");
      return;
    }
    // Lock scrolling while the intro plays so the viewport-anchored overlay
    // can't be scrolled away mid-animation.
    document.documentElement.style.overflow = "hidden";
    const t1 = setTimeout(() => setPhase("fade"), 2000); // ring + logo hold (~2s like the video)
    const t2 = setTimeout(() => setPhase("done"), 2750); // overlay gone
    return () => {
      clearTimeout(t1);
      clearTimeout(t2);
      document.documentElement.style.overflow = "";
    };
  }, []);
  useEffect(() => {
    if (phase === "done") document.documentElement.style.overflow = "";
  }, [phase]);
  if (phase === "done") return null;
  const size = "min(30rem, 86vw)";
  return (
    // NOTE: `position:fixed` is a TRAP here — MarketingShell's page wrapper is
    // CSS-transformed (`-translate-x-1/2`), which turns any fixed descendant
    // into an absolute one against the PAGE-TALL wrapper: the overlay then spans
    // the whole document and its centered children land thousands of px below
    // the fold (the "just a black screen" bug). Anchor to the FIRST VIEWPORT
    // instead (absolute + 100vh, page always loads at scroll-top) and center
    // with flex.
    <div
      style={{
        position: "absolute",
        top: 0,
        left: 0,
        right: 0,
        height: "100vh",
        zIndex: 100,
        background: "#0c0d10",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        opacity: phase === "fade" ? 0 : 1,
        transition: "opacity 700ms ease",
        pointerEvents: phase === "fade" ? "none" : "auto",
      }}
    >
      <div
        style={{
          position: "relative",
          width: size,
          height: size,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
        }}
      >
        <GlowRing style={{ position: "absolute", inset: 0 }} />
        <Image
          src="/shadw-logo-dark.png"
          alt="shadw"
          width={274}
          height={91}
          priority
          style={{
            position: "relative",
            zIndex: 1,
            height: "2.4rem",
            width: "auto",
            animation: "intro-logo-pan 2.2s ease-in-out infinite",
          }}
        />
      </div>
    </div>
  );
}

export function Landing() {
  return (
    <MarketingShell>
      {/* Intro loading animation (once per page load): ring + logo, then fade. */}
      <IntroLoader />
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
          {/* Leftmost glow = the reference video's animated ring: a big rotating
              conic ring anchored off-screen top-left so only its lower-right arc
              sweeps the corner, its colors cycling rose↔violet↔teal as it spins. */}
          <GlowRing className="absolute left-[max(-24rem,-30vw)] top-[max(-22rem,-27.5vw)] h-[min(44rem,55vw)] w-[min(44rem,55vw)]" />
          <div className="absolute right-[max(-10rem,-12.5vw)] top-[min(10rem,12.5vw)] h-[min(34rem,42.5vw)] w-[min(34rem,42.5vw)] rounded-full bg-cyan-500/15 blur-[min(140px,10.94vw)]" />
          <div className="absolute right-[max(-4rem,-5vw)] top-[min(13rem,16.25vw)] h-[min(20rem,25vw)] w-[min(20rem,25vw)] rounded-full bg-sky-400/30 blur-[min(110px,8.59vw)]" />
        </div>

        <div className="relative z-10 mx-auto max-w-5xl px-6 pb-2 pt-[3.15rem] text-center sm:pt-[4.2rem]">
          <h1 className="text-balance text-5xl font-normal leading-[1.05] tracking-tight text-white sm:text-7xl">
            <span className="font-bold">The compute market</span>
            <br />
            for every service on Earth
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
            <Image
              src="/shadw-device.png"
              alt="shadw Command Center"
              width={1562}
              height={1078}
              sizes="(max-width: 1024px) 100vw, 1024px"
              className="h-auto w-full rounded-xl border border-white/5"
            />
          </div>
        </div>
      </section>

    </MarketingShell>
  );
}
