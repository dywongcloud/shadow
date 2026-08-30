"use client";

import Link from "next/link";
import { useEffect, useState } from "react";
import { ArrowRight, Boxes, Database, Network } from "lucide-react";
import { MarqueeBanner } from "@/components/marquee-banner";
import { MarketingShell } from "@/components/marketing-shell";
import Image from "next/image";

/* ------------------------------------------------------------------ *
 * autheo — public landing page. Renders inside the shared MarketingShell
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

/** Intro loading animation (reference video 1): a full-screen dark overlay with
 *  the autheo ghost logo centered inside the rotating GlowRing. ~2s of spin, then
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
        background: "#050b07",
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
        <span
         style={{
           position: "relative",
           zIndex: 1,
           fontSize: "2.2rem",
           lineHeight: 1,
           letterSpacing: "-0.04em",
           fontWeight: 700,
           color: "#bbf7d0",
           animation: "intro-logo-pan 2.2s ease-in-out infinite",
         }}
        >
         autheo.dev
        </span>
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
      <section className="relative min-h-[44rem] overflow-hidden bg-[#020806]">
        <AutheoHeroLandscape />
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
        <div className="pointer-events-none absolute inset-x-0 top-0 z-[1] h-[min(32rem,40vw)]">
          {/* Left source — leans toward centre. */}
          <div className="absolute top-[max(-8rem,-10vw)] left-[6%] h-[min(26rem,32.5vw)] w-[min(26rem,32.5vw)] rounded-full bg-[#12f7d3]/16 blur-[min(130px,10.16vw)]" />
          <div className="absolute top-[max(-6rem,-7.5vw)] left-[28%] h-[min(24rem,30vw)] w-[min(24rem,30vw)] rounded-full bg-[#12f7d3]/12 blur-[min(140px,10.94vw)]" />
          {/* Right source — mirror of the left, leans toward centre. */}
          <div className="absolute top-[max(-8rem,-10vw)] right-[6%] h-[min(26rem,32.5vw)] w-[min(26rem,32.5vw)] rounded-full bg-[#12f7d3]/14 blur-[min(130px,10.16vw)]" />
          <div className="absolute top-[max(-6rem,-7.5vw)] right-[28%] h-[min(24rem,30vw)] w-[min(24rem,30vw)] rounded-full bg-[#12f7d3]/12 blur-[min(140px,10.94vw)]" />
          {/* Convergence into the navbar centre (top-centre blend of the two sides). */}
          <div className="absolute top-[max(-7rem,-8.75vw)] left-1/2 h-[min(20rem,25vw)] w-[min(42rem,52.5vw)] -translate-x-1/2 rounded-full bg-[#12f7d3]/12 blur-[min(150px,11.72vw)]" />
          {/* Lower-horizon convergence — a wide, flat band the two sides melt into. */}
          <div className="absolute bottom-0 left-1/2 h-[min(8rem,10vw)] w-[72%] -translate-x-1/2 rounded-[100%] bg-[#12f7d3]/12 blur-[min(130px,10.16vw)]" />
        </div>

        {/* Solutions-page hero glows, copied 1:1 from <MarketingHero>: a left
            violet/pink cluster and a right cyan/sky cluster. Made responsive with
            the same `min(rem, vw)` / `max(-rem, -vw)` scheme (1280px reference) so
            they shrink and pull inward on small screens while staying pixel-exact
            to the Solutions hero at ≥1280px. */}
        <div className="pointer-events-none absolute inset-0 z-[2]">
          {/* Leftmost glow = the reference video's animated ring: a big rotating
              conic ring anchored off-screen top-left so only its lower-right arc
              sweeps the corner, its colors cycling rose↔violet↔teal as it spins. */}
          <GlowRing className="absolute left-[max(-24rem,-30vw)] top-[max(-22rem,-27.5vw)] h-[min(44rem,55vw)] w-[min(44rem,55vw)]" />
          <div className="absolute right-[max(-10rem,-12.5vw)] top-[min(10rem,12.5vw)] h-[min(34rem,42.5vw)] w-[min(34rem,42.5vw)] rounded-full bg-emerald-500/18 blur-[min(140px,10.94vw)]" />
          <div className="absolute right-[max(-4rem,-5vw)] top-[min(13rem,16.25vw)] h-[min(20rem,25vw)] w-[min(20rem,25vw)] rounded-full bg-green-400/24 blur-[min(110px,8.59vw)]" />
        </div>

        <div className="relative z-10 mx-auto max-w-5xl px-6 pb-2 pt-[6rem] text-center sm:pt-[8rem]">
          <h1 className="text-balance text-5xl font-normal leading-[1.05] tracking-tight text-white sm:text-7xl">
            <span className="font-bold">Autheo Development Hub</span>
          </h1>
          <p className="mx-auto mt-7 max-w-xl text-lg leading-relaxed text-zinc-400">
            Build once. Run anywhere on your cloud.
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

      </section>

      {/* ---------------- Scrolling "Own Your Cloud." banner (after the globe) ---------------- */}
      <MarqueeBanner />

      {/* ---------------- Marketplace ---------------- */}
      <section className="relative overflow-hidden px-6 py-24 sm:py-32">
        <div
          aria-hidden
          className="pointer-events-none absolute inset-0 opacity-[0.12] mix-blend-screen"
        >
          <Image
            src="/images/developer-workshop-streetscape.png"
            alt=""
            fill
            sizes="100vw"
            className="object-cover object-center"
          />
        </div>
        <div
          aria-hidden
          className="pointer-events-none absolute left-1/2 top-1/2 h-80 w-[min(58rem,100vw)] -translate-x-1/2 -translate-y-1/2 rounded-full bg-emerald-500/15 blur-[130px]"
        />
        <div className="relative mx-auto max-w-6xl overflow-hidden rounded-3xl border border-emerald-300/20 bg-[#07130d]/85 p-7 shadow-[0_30px_100px_-40px_rgba(74,222,128,0.45)] backdrop-blur-sm sm:p-12">
          <div className="absolute inset-x-12 top-0 h-px bg-gradient-to-r from-transparent via-emerald-300/70 to-transparent" />
          <div className="grid gap-12 lg:grid-cols-[1.05fr_0.95fr] lg:items-center">
            <div>
              <p className="text-sm font-semibold uppercase tracking-[0.2em] text-emerald-300">
                Autheo Marketplace
              </p>
              <h2 className="mt-4 text-balance text-4xl font-semibold tracking-tight text-white sm:text-5xl">
                Everything your app needs, in one marketplace.
              </h2>
              <p className="mt-5 max-w-xl text-lg leading-relaxed text-zinc-300">
                Provision compute, storage, databases, domains, and more directly alongside the applications you build on Autheo.
              </p>
              <Link
                href="/marketplace"
                className="group mt-8 inline-flex items-center gap-2 rounded-full bg-emerald-300 px-6 py-3 text-sm font-semibold text-emerald-950 shadow-lg shadow-emerald-400/20 transition-transform hover:scale-[1.03] hover:bg-emerald-200"
              >
                Explore Marketplace
                <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
              </Link>
            </div>
            <div className="grid gap-3">
              <MarketplaceCapability
                icon={<Boxes className="h-5 w-5" />}
                title="Compute"
                description="Functions, containers, and edge workloads ready to provision."
              />
              <MarketplaceCapability
                icon={<Database className="h-5 w-5" />}
                title="Storage and databases"
                description="Durable storage and managed data services for every app."
              />
              <MarketplaceCapability
                icon={<Network className="h-5 w-5" />}
                title="Domains and networking"
                description="Connect your app with domains, networking, and app services."
              />
            </div>
          </div>
        </div>
      </section>

      {/* ---------------- Device showcase (dead space above for contrast) ---------------- */}
      <section id="demo" className="relative scroll-mt-20 px-6 pb-28 pt-36 sm:pt-44">
        <div className="pointer-events-none absolute inset-0">
          <div className="absolute left-0 top-1/3 h-[30rem] w-[30rem] rounded-full bg-lime-700/20 blur-[150px]" />
          <div className="absolute right-0 top-1/4 h-[30rem] w-[30rem] rounded-full bg-emerald-500/15 blur-[150px]" />
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
              src="/autheo-device.png"
              alt="autheo Command Center"
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

function MarketplaceCapability({
  icon,
  title,
  description,
}: {
  icon: React.ReactNode;
  title: string;
  description: string;
}) {
  return (
    <div className="flex gap-4 rounded-2xl border border-white/10 bg-white/[0.045] p-5 transition-colors hover:border-emerald-300/30 hover:bg-emerald-300/[0.07]">
      <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl border border-emerald-300/25 bg-emerald-300/10 text-emerald-200">
        {icon}
      </div>
      <div>
        <h3 className="font-semibold text-white">{title}</h3>
        <p className="mt-1 text-sm leading-relaxed text-zinc-400">{description}</p>
      </div>
    </div>
  );
}

function AutheoHeroLandscape() {
  return (
    <div aria-hidden className="pointer-events-none absolute inset-x-0 bottom-0 z-0 h-[34rem] overflow-hidden">
      <div className="absolute inset-0 bg-[radial-gradient(ellipse_at_50%_80%,rgba(18,247,211,0.2),transparent_48%),linear-gradient(to_bottom,transparent_18%,#020806_94%)]" />
      <svg viewBox="0 0 1440 560" preserveAspectRatio="xMidYMax slice" className="absolute inset-x-0 bottom-0 h-full w-full text-[#12f7d3]">
        <defs>
          <pattern id="hero-dot-grid" width="18" height="18" patternUnits="userSpaceOnUse">
            <circle cx="1" cy="1" r="0.7" fill="currentColor" opacity="0.3" />
          </pattern>
          <linearGradient id="hero-globe-fill" x1="0" x2="0" y1="0" y2="1">
            <stop stopColor="#12f7d3" stopOpacity="0.15" />
            <stop offset="1" stopColor="#12f7d3" stopOpacity="0.02" />
          </linearGradient>
        </defs>
        <path d="M165 560 A555 555 0 0 1 1275 560 Z" fill="url(#hero-globe-fill)" />
        <path d="M165 560 A555 555 0 0 1 1275 560" fill="none" stroke="currentColor" strokeOpacity="0.58" strokeWidth="1.2" />
        <path d="M273 560 A447 447 0 0 1 1167 560 M391 560 A329 329 0 0 1 1049 560 M509 560 A211 211 0 0 1 931 560" fill="none" stroke="currentColor" strokeOpacity="0.28" strokeWidth="0.8" />
        <path d="M720 5 C602 100 602 420 720 560 M720 5 C838 100 838 420 720 560 M432 88 C586 220 586 428 432 560 M1008 88 C854 220 854 428 1008 560" fill="none" stroke="currentColor" strokeOpacity="0.24" strokeWidth="0.8" />
        <path d="M180 560 Q720 480 1260 560 M213 560 Q720 398 1227 560 M275 560 Q720 320 1165 560" fill="none" stroke="currentColor" strokeOpacity="0.25" strokeWidth="0.8" />
        <path d="M0 560 H1440 V410 Q720 340 0 410 Z" fill="url(#hero-dot-grid)" opacity="0.42" />
        <HeroTree x={88} y={390} scale={1.15} />
        <HeroTree x={205} y={410} scale={0.8} />
        <HeroTree x={335} y={375} scale={1.08} />
        <HeroTree x={470} y={415} scale={0.68} />
        <HeroTree x={970} y={410} scale={0.75} />
        <HeroTree x={1090} y={372} scale={1.13} />
        <HeroTree x={1235} y={410} scale={0.82} />
        <HeroTree x={1360} y={382} scale={1.08} />
        <path d="M0 552 C280 514 430 548 720 532 C1010 516 1210 550 1440 520 V560 H0Z" fill="#020806" fillOpacity="0.72" />
      </svg>
    </div>
  );
}

function HeroTree({ x, y, scale }: { x: number; y: number; scale: number }) {
  return (
    <g transform={`translate(${x} ${y}) scale(${scale})`} fill="none" stroke="currentColor" strokeLinecap="round">
      <path d="M0 162 V54 M0 83 L-34 48 M0 104 L39 62 M0 126 L-45 92 M0 143 L46 106" strokeOpacity="0.6" strokeWidth="1.3" />
      <path d="M-60 75 C-70 37 -40 5 -7 25 C8 -5 50 12 43 43 C78 45 75 88 42 93 C25 119 -22 111 -29 93 C-58 102 -79 91 -60 75Z" fill="currentColor" fillOpacity="0.13" strokeOpacity="0.6" />
      <path d="M-48 66 L-16 31 L0 76 L24 26 L52 74 M-34 100 L0 72 L35 101" strokeOpacity="0.38" strokeWidth="0.8" />
      <circle cx="-16" cy="31" r="2" fill="currentColor" fillOpacity="0.65" stroke="none" />
      <circle cx="24" cy="26" r="2" fill="currentColor" fillOpacity="0.65" stroke="none" />
      <circle cx="0" cy="76" r="2" fill="currentColor" fillOpacity="0.65" stroke="none" />
    </g>
  );
}
