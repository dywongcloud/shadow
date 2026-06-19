"use client";

import Link from "next/link";
import { ArrowRight, Twitter, Github, Linkedin, Dribbble } from "lucide-react";

/* ------------------------------------------------------------------ *
 * shadw — public landing page.
 * Full-bleed dark brand page (escapes the dashboard container). Forces a
 * dark palette regardless of the app theme.
 * ------------------------------------------------------------------ */

const NAV = ["Product", "Solutions", "Features", "Docs", "Pricing", "Blog", "Case Studies"];

const FOOTER_COLS: { title: string; links: string[] }[] = [
  { title: "Product", links: ["Overview", "Features", "Pricing", "Changelog"] },
  { title: "Resources", links: ["Documentation", "API Reference", "Guides", "Blog"] },
  { title: "Company", links: ["About", "Careers", "Contact", "Privacy"] },
];

/* eslint-disable @next/next/no-img-element */
function Logo({ className = "h-6" }: { className?: string }) {
  // Landing is always dark → always the white wordmark.
  return <img src="/shadw-logo-dark.png" alt="shadw" className={`${className} w-auto select-none`} />;
}

export function Landing() {
  return (
    // Full-bleed: break out of the dashboard <main> (container + padding).
    <div className="relative left-1/2 w-screen -translate-x-1/2 -my-8 overflow-hidden bg-black text-white">
      {/* ---------------- Top nav ---------------- */}
      <header className="relative z-20 border-b border-cyan-400/40 bg-black/80 backdrop-blur">
        <div className="mx-auto flex max-w-[1500px] items-center justify-between px-6 py-4 lg:px-10">
          <Link href="/"><Logo className="h-6" /></Link>
          <nav className="hidden items-center gap-8 text-[15px] text-zinc-300 lg:flex">
            {NAV.map((n) => (
              <a key={n} href="#features" className="transition-colors hover:text-white">{n}</a>
            ))}
          </nav>
          <Link
            href="/sign-in"
            className="rounded-full border border-white/30 px-5 py-2 text-sm text-white transition-colors hover:bg-white/10"
          >
            Login
          </Link>
        </div>
      </header>

      {/* ---------------- Hero ---------------- */}
      <section className="relative">
        {/* ambient glow + grid */}
        <div className="pointer-events-none absolute inset-0">
          {/* Left: purple base glow + a smaller, brighter pink glow on top. */}
          <div className="absolute -left-40 top-0 h-[40rem] w-[40rem] rounded-full bg-violet-700/25 blur-[140px]" />
          <div className="absolute -left-20 top-16 h-[22rem] w-[22rem] rounded-full bg-pink-500/40 blur-[110px]" />
          {/* Right: cyan base glow + a smaller, brighter light-blue glow on top. */}
          <div className="absolute -right-40 top-40 h-[34rem] w-[34rem] rounded-full bg-cyan-500/15 blur-[140px]" />
          <div className="absolute -right-16 top-52 h-[20rem] w-[20rem] rounded-full bg-sky-400/35 blur-[110px]" />
          <div
            className="absolute inset-0 opacity-[0.15]"
            style={{
              backgroundImage:
                "linear-gradient(to right, rgba(255,255,255,0.07) 1px, transparent 1px), linear-gradient(to bottom, rgba(255,255,255,0.07) 1px, transparent 1px)",
              backgroundSize: "60px 60px",
              maskImage: "radial-gradient(ellipse 70% 60% at 50% 35%, black, transparent 80%)",
              WebkitMaskImage: "radial-gradient(ellipse 70% 60% at 50% 35%, black, transparent 80%)",
            }}
          />
        </div>

        <div className="relative mx-auto max-w-5xl px-6 pb-10 pt-24 text-center sm:pt-32">
          <h1 className="text-balance text-5xl font-bold leading-[1.05] tracking-tight sm:text-7xl">
            Beyond the Edge
            <br />
            <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text italic text-transparent">
              are Shadows
            </span>
          </h1>
          <p className="mx-auto mt-7 max-w-xl text-lg leading-relaxed text-zinc-400">
            Unleash the Power of Peer-to-Peer: Seamlessly Connect, Collaborate, and Conquer with Our
            Cutting-Edge Cloud
          </p>
          <div className="mt-10 flex items-center justify-center gap-4">
            <Link
              href="/sign-up"
              className="group inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black shadow-[0_0_35px_rgba(52,211,153,0.55)] transition-transform hover:scale-[1.03]"
            >
              Start Now <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
            </Link>
            <a
              href="#demo"
              className="inline-flex items-center gap-2 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10"
            >
              Watch Demo
            </a>
          </div>
        </div>
      </section>

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

      {/* ---------------- Giant brand wordmark ---------------- */}
      <section className="relative">
        <div
          className="pointer-events-none absolute inset-0 opacity-[0.12]"
          style={{
            backgroundImage:
              "linear-gradient(to right, rgba(255,255,255,0.08) 1px, transparent 1px), linear-gradient(to bottom, rgba(255,255,255,0.08) 1px, transparent 1px)",
            backgroundSize: "48px 48px",
          }}
        />
        <div className="relative flex justify-center overflow-hidden">
          <span
            className="select-none whitespace-nowrap bg-gradient-to-r from-violet-600 via-indigo-400 to-teal-400 bg-clip-text font-display font-bold leading-[0.8] tracking-tighter text-transparent opacity-90"
            style={{ fontSize: "clamp(7rem, 27vw, 24rem)" }}
          >
            shadw
          </span>
        </div>
      </section>

      {/* ---------------- Footer ---------------- */}
      <footer className="relative border-t border-white/5 bg-black">
        <div className="mx-auto max-w-[1500px] px-6 py-14 lg:px-10">
          <div className="grid grid-cols-2 gap-10 md:grid-cols-5">
            <div className="col-span-2 md:col-span-2">
              <Logo className="h-6" />
              <p className="mt-4 max-w-xs text-sm leading-relaxed text-zinc-500">
                Unleash the Power of Peer-to-Peer: Seamlessly Connect, Collaborate, and Conquer.
              </p>
            </div>
            {FOOTER_COLS.map((col) => (
              <div key={col.title}>
                <div className="text-sm font-semibold text-white">{col.title}</div>
                <ul className="mt-4 space-y-3">
                  {col.links.map((l) => (
                    <li key={l}>
                      <a href="#" className="text-sm text-zinc-500 transition-colors hover:text-zinc-200">{l}</a>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>

          <div className="mt-12 flex flex-col items-center justify-between gap-4 border-t border-white/5 pt-6 sm:flex-row">
            <span className="text-sm text-zinc-600">2026 Shadw. All rights reserved.</span>
            <div className="flex items-center gap-5 text-zinc-500">
              <a href="#" aria-label="Twitter" className="transition-colors hover:text-white"><Twitter className="h-4 w-4" /></a>
              <a href="#" aria-label="GitHub" className="transition-colors hover:text-white"><Github className="h-4 w-4" /></a>
              <a href="#" aria-label="LinkedIn" className="transition-colors hover:text-white"><Linkedin className="h-4 w-4" /></a>
              <a href="#" aria-label="Dribbble" className="transition-colors hover:text-white"><Dribbble className="h-4 w-4" /></a>
            </div>
          </div>
        </div>
      </footer>
    </div>
  );
}
