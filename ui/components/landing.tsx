"use client";

import Link from "next/link";
import { useEffect, useState } from "react";
import { ArrowRight } from "lucide-react";
import { MarqueeBanner } from "@/components/marquee-banner";
import { MarketingShell } from "@/components/marketing-shell";
import { GlobeWireframe } from "@/components/globe-wireframe";
import Image from "next/image";

/** The rotating aurora glow ring: a conic rose-violet-teal gradient masked into a soft ring and heavily blurred; rotating the element
 *  cycles the visible arc's color (~2 s). */
function GlowRing({
  className = "",
  duration = 2.2,
  style,
}: {
  className?: string;
  duration?: number;
  style?: React.CSSProperties;
}) {
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

/** Module scope on purpose: `<Landing>` can remount within one page load (an auth flip), and the intro must play once per page load; only
 *  the client effect sets it, so SSR always renders the overlay and hydration agrees. */
let introPlayed = false;

const INTRO_FADE_AT_MS = 2000;
const INTRO_DONE_AT_MS = 2750;
const SLOW_EFFECTIVE_TYPES = ["slow-2g", "2g", "3g"];

function connectionKnownSlow(): boolean {
  const conn = (
    navigator as Navigator & {
      connection?: { effectiveType?: string; saveData?: boolean };
    }
  ).connection;
  return (
    conn?.saveData === true ||
    (typeof conn?.effectiveType === "string" && SLOW_EFFECTIVE_TYPES.includes(conn.effectiveType))
  );
}

/** Intro loading animation: a full-screen black overlay with the ghost logo inside the rotating GlowRing, ~2 s of spin then a fade; once per
 *  PAGE LOAD, skipped for prefers-reduced-motion and on known-slow connections, and rendered from first paint (SSR, inline styles only). */
function IntroLoader() {
  const [phase, setPhase] = useState<"show" | "fade" | "done">(() => (introPlayed ? "done" : "show"));
  useEffect(() => {
    if (introPlayed) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- re-syncs against the module-level `introPlayed` flag, which can flip between this render and effect commit on a fast remount; the lazy initializer alone can't observe that
      setPhase("done");
      return;
    }
    introPlayed = true;
    if (connectionKnownSlow() || window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) {
      setPhase("done");
      return;
    }
    document.documentElement.style.overflow = "hidden";
    const t1 = setTimeout(() => setPhase("fade"), INTRO_FADE_AT_MS);
    const t2 = setTimeout(() => setPhase("done"), INTRO_DONE_AT_MS);
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
      <IntroLoader />
      <section className="relative overflow-hidden">
        <div className="pointer-events-none absolute inset-x-0 top-0 z-0 h-[min(32rem,40vw)]">
          <div className="absolute top-[max(-8rem,-10vw)] left-[6%] h-[min(26rem,32.5vw)] w-[min(26rem,32.5vw)] rounded-full bg-teal-400/14 blur-[min(130px,10.16vw)]" />
          <div className="absolute top-[max(-6rem,-7.5vw)] left-[28%] h-[min(24rem,30vw)] w-[min(24rem,30vw)] rounded-full bg-indigo-600/14 blur-[min(140px,10.94vw)]" />
          <div className="absolute top-[max(-8rem,-10vw)] right-[6%] h-[min(26rem,32.5vw)] w-[min(26rem,32.5vw)] rounded-full bg-cyan-400/14 blur-[min(130px,10.16vw)]" />
          <div className="absolute top-[max(-6rem,-7.5vw)] right-[28%] h-[min(24rem,30vw)] w-[min(24rem,30vw)] rounded-full bg-fuchsia-600/14 blur-[min(140px,10.94vw)]" />
          <div className="absolute top-[max(-7rem,-8.75vw)] left-1/2 h-[min(20rem,25vw)] w-[min(42rem,52.5vw)] -translate-x-1/2 rounded-full bg-indigo-500/12 blur-[min(150px,11.72vw)]" />
          <div className="absolute bottom-0 left-1/2 h-[min(8rem,10vw)] w-[72%] -translate-x-1/2 rounded-[100%] bg-sky-500/12 blur-[min(130px,10.16vw)]" />
        </div>

        <div className="pointer-events-none absolute inset-0">
          <GlowRing className="absolute left-[max(-24rem,-30vw)] top-[max(-22rem,-27.5vw)] h-[min(44rem,55vw)] w-[min(44rem,55vw)]" />
          <div className="absolute right-[max(-10rem,-12.5vw)] top-[min(10rem,12.5vw)] h-[min(34rem,42.5vw)] w-[min(34rem,42.5vw)] rounded-full bg-cyan-500/15 blur-[min(140px,10.94vw)]" />
          <div className="absolute right-[max(-4rem,-5vw)] top-[min(13rem,16.25vw)] h-[min(20rem,25vw)] w-[min(20rem,25vw)] rounded-full bg-sky-400/30 blur-[min(110px,8.59vw)]" />
        </div>

        <div className="relative z-10 mx-auto max-w-5xl px-6 pb-2 pt-[3.15rem] text-center sm:pt-[4.2rem]">
          <h1 className="text-balance text-5xl font-normal leading-[1.05] tracking-tight text-white sm:text-7xl">
            <span className="font-bold">The serverless cloud</span>
            <br />
            for people and their agents
          </h1>
          <p className="mx-auto mt-7 max-w-2xl text-lg leading-relaxed text-zinc-400">
            Deploy apps, databases, and AI workloads across a distributed edge network. Ship simply,
            scale as you grow, and keep control of where your infrastructure runs, with all the
            familiar features from Vercel.
          </p>
          <div className="mt-10 flex items-center justify-center gap-4">
            <Link
              href="/sign-up"
              className="group inline-flex items-center gap-2 rounded-full bg-white px-6 py-3 text-sm font-semibold text-black transition-transform hover:scale-[1.03]"
            >
              Start Now <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
            </Link>
            <a
              href="https://www.youtube.com/watch?v=qp_lhPioZF4"
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-2 rounded-full bg-white px-6 py-3 text-sm font-medium text-black transition-colors hover:bg-white/90"
            >
              Watch Demo
            </a>
          </div>
        </div>

        <GlobeWireframe className="mx-auto mt-4 w-full max-w-6xl px-4" />
      </section>

      <MarqueeBanner />

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
