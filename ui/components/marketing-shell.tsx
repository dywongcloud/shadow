"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { Twitter, Github, Linkedin, Dribbble, Menu, X } from "lucide-react";

const ELECTROLIZE_FONT = "var(--font-electrolize), ui-sans-serif, system-ui, sans-serif";

/* ------------------------------------------------------------------ *
 * Shared chrome for the public marketing pages (Product, Solutions,
 * Features, Pricing, Blog, Case Studies, …). Full-bleed dark brand
 * surface with the top nav + footer, so every marketing page matches the
 * landing page. The landing page renders its own hero inside this shell.
 * ------------------------------------------------------------------ */

export const MARKETING_NAV: { label: string; href: string }[] = [
  { label: "Product", href: "/product" },
  { label: "Solutions", href: "/solutions" },
  { label: "Features", href: "/features" },
  { label: "Docs", href: "/docs" },
  { label: "Pricing", href: "/pricing" },
  { label: "Blog", href: "/blog" },
  { label: "Case Studies", href: "/case-studies" },
];

const FOOTER_COLS: { title: string; links: { label: string; href: string }[] }[] = [
  {
    title: "Product",
    links: [
      { label: "Overview", href: "/product" },
      { label: "Features", href: "/features" },
      { label: "Pricing", href: "/pricing" },
      { label: "Changelog", href: "/blog" },
    ],
  },
  {
    title: "Resources",
    links: [
      { label: "Documentation", href: "/docs" },
      { label: "API Reference", href: "/docs/api-reference" },
      { label: "Guides", href: "/docs/getting-started" },
      { label: "Blog", href: "/blog" },
    ],
  },
  {
    title: "Company",
    links: [
      { label: "Solutions", href: "/solutions" },
      { label: "Case Studies", href: "/case-studies" },
      { label: "Contact", href: "/contact" },
      { label: "Privacy", href: "/privacy" },
    ],
  },
];

/* eslint-disable @next/next/no-img-element */
function Logo({ className = "h-7" }: { className?: string }) {
  // Wordmark is 1826×407 (≈ 4.49:1). Sized by height so it always fits the nav row
  // and scales responsively; `w-auto` preserves the aspect ratio.
  return <img src="/shadw-logo-wordmark.png" alt="shadw" className={`${className} w-auto select-none`} />;
}

export function MarketingShell({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  const active = (href: string) => pathname === href || (href !== "/" && pathname.startsWith(href + "/"));

  // The marketing/landing surface is ALWAYS the dark brand surface (#0c0d10),
  // independent of the platform light/dark theme. Force the ROOT (html/body)
  // background to match so the overscroll/rubber-band area and any gutter show the
  // same dark — not the theme background (white in light mode). `color-scheme:dark`
  // makes native scrollbars (and their corner) render dark, and `overflow-x:clip`
  // removes the horizontal scrollbar the full-bleed `w-screen` shell would induce
  // (which is what produced the tiny white scrollbar-corner square). All restored
  // on unmount so the dashboard keeps its themed background.
  useEffect(() => {
    const html = document.documentElement;
    const body = document.body;
    // Capture ONLY the color-scheme next-themes owns, to hand it back on unmount.
    // Background + overflow overrides are reverted to "" (the stylesheet/theme
    // value) DETERMINISTICALLY rather than to a captured snapshot.
    //
    // Why not snapshot-and-restore the background too: on a fast logout→login the
    // shell can re-mount while the forced dark values are still applied, so a
    // snapshot would capture `#0c0d10` and then "restore" it onto the dashboard —
    // leaving the dashboard on a dark body (looks unstyled/broken) until a manual
    // refresh. Clearing to "" reverts to `--background` every time, regardless of
    // mount ordering / StrictMode double-invoke.
    const prevScheme = html.style.colorScheme;
    html.style.backgroundColor = "#0c0d10";
    body.style.backgroundColor = "#0c0d10";
    html.style.colorScheme = "dark";
    body.style.overflowX = "clip";
    return () => {
      html.style.backgroundColor = "";
      body.style.backgroundColor = "";
      html.style.colorScheme = prevScheme; // never inherit our forced "dark"
      body.style.overflowX = "";
    };
  }, []);

  // Mobile nav menu (below lg, where the inline nav is hidden).
  const [menuOpen, setMenuOpen] = useState(false);
  // Close the menu on navigation.
  useEffect(() => setMenuOpen(false), [pathname]);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setMenuOpen(false);
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  // Lock background scroll while the full-screen menu overlay is open.
  useEffect(() => {
    if (!menuOpen) return;
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = prev;
    };
  }, [menuOpen]);

  return (
    <div
      className="relative left-1/2 w-screen -translate-x-1/2 -my-8 overflow-hidden bg-[#0c0d10] text-white"
      style={{ fontFamily: "var(--font-electrolize), ui-sans-serif, system-ui, sans-serif" }}
    >
      {/* Top nav — blue lining matches the SVG globe's blue streaks (#218CFF). */}
      <header className="relative z-30 border-b border-[#218cff]/50 bg-[#0c0d10]/80 backdrop-blur">
        <div className="mx-auto flex max-w-[1500px] items-center justify-between px-6 py-4 lg:px-10">
          <Link href="/" className="shrink-0"><Logo className="h-7 sm:h-8" /></Link>
          <nav className="hidden items-center gap-8 font-semibold uppercase text-[18px] text-zinc-300 lg:flex">
            {MARKETING_NAV.map((n) => (
              <Link
                key={n.href}
                href={n.href}
                className={`transition-colors hover:text-white ${active(n.href) ? "text-white" : ""}`}
              >
                {n.label}
              </Link>
            ))}
          </nav>
          {/* Desktop: Login. Mobile/tablet: a hamburger toggling the menu below. */}
          <div className="flex items-center gap-3">
            <Link
              href="/sign-in"
              className="hidden rounded-full border border-white/30 px-5 py-2 font-semibold uppercase text-[16.8px] text-white transition-colors hover:bg-white/10 lg:inline-flex"
            >
              Login
            </Link>
            <button
              type="button"
              aria-label={menuOpen ? "Close menu" : "Open menu"}
              aria-expanded={menuOpen}
              onClick={() => setMenuOpen((o) => !o)}
              className="inline-flex h-10 w-10 items-center justify-center rounded-md border border-white/20 text-white transition-colors hover:bg-white/10 lg:hidden"
            >
              {menuOpen ? <X className="h-5 w-5" /> : <Menu className="h-5 w-5" />}
            </button>
          </div>
        </div>

      </header>

      {/* Mobile/tablet menu — FULL-SCREEN overlay with a close (X) top-right.
          Portaled to <body> so it escapes the shell's `transform` ancestor (which
          would otherwise make `position: fixed` resolve to the shell, not the
          viewport) and covers the entire screen. */}
      {menuOpen &&
        typeof document !== "undefined" &&
        createPortal(
          <div className="fixed inset-0 z-[200] flex flex-col bg-[#0c0d10] text-white lg:hidden" style={{ fontFamily: ELECTROLIZE_FONT }}>
            {/* Top bar: logo + close button (mirrors the navbar). */}
            <div className="flex items-center justify-between border-b border-[#218cff]/50 px-6 py-4">
              <Link href="/" onClick={() => setMenuOpen(false)} className="shrink-0">
                <Logo className="h-7" />
              </Link>
              <button
                type="button"
                aria-label="Close menu"
                onClick={() => setMenuOpen(false)}
                className="inline-flex h-10 w-10 items-center justify-center rounded-md border border-white/20 text-white transition-colors hover:bg-white/10"
              >
                <X className="h-5 w-5" />
              </button>
            </div>
            {/* Links fill the rest of the screen. */}
            <nav className="flex flex-1 flex-col overflow-y-auto px-6 py-6">
              <ul className="flex flex-col">
                {MARKETING_NAV.map((n) => (
                  <li key={n.href}>
                    <Link
                      href={n.href}
                      onClick={() => setMenuOpen(false)}
                      className={`block border-b border-white/5 py-4 font-semibold uppercase text-[18px] transition-colors hover:text-white ${
                        active(n.href) ? "text-white" : "text-zinc-300"
                      }`}
                    >
                      {n.label}
                    </Link>
                  </li>
                ))}
              </ul>
              <Link
                href="/sign-in"
                onClick={() => setMenuOpen(false)}
                className="mt-8 inline-flex w-full items-center justify-center rounded-full border border-white/30 px-5 py-3 font-semibold uppercase text-[18px] text-white transition-colors hover:bg-white/10"
              >
                Login
              </Link>
            </nav>
          </div>,
          document.body
        )}

      {children}

      {/* Footer */}
      <footer className="relative overflow-hidden bg-[#0c0d10]">
        {/* Aurora glows rising from the bottom edge — light blue, aqua, turquoise,
            indigo and pink fuchsia. Dimmer + slightly larger radii for softer wash. */}
        <div className="pointer-events-none absolute inset-x-0 bottom-0 h-[26rem]">
          <div className="absolute -bottom-44 left-[6%] h-[25.2rem] w-[25.2rem] rounded-full bg-teal-400/36 blur-[126px]" />
          <div className="absolute -bottom-52 left-[33%] h-[31.5rem] w-[31.5rem] rounded-full bg-indigo-600/45 blur-[137px]" />
          <div className="absolute -bottom-44 left-[55%] h-[25.2rem] w-[25.2rem] rounded-full bg-fuchsia-600/30 blur-[126px]" />
          <div className="absolute -bottom-48 right-[6%] h-[27.3rem] w-[27.3rem] rounded-full bg-cyan-400/30 blur-[126px]" />
        </div>
        {/* Giant brand watermark — sits at the very bottom of the page, bleeding off
            the bottom edge, behind the footer content and lit by the glows. */}
        <div className="pointer-events-none absolute inset-x-0 bottom-0 flex justify-center overflow-hidden">
          <span
            className="select-none whitespace-nowrap bg-clip-text font-bold leading-[0.78] tracking-tighter text-transparent"
            style={{
              fontSize: "clamp(7rem, 30vw, 28rem)",
              opacity: 0.16,
              transform: "translateY(26%)",
              backgroundImage: "linear-gradient(100deg, #5eead4 0%, #22d3ee 30%, #818cf8 60%, #e879f9 100%)",
              maskImage: "linear-gradient(to top, black 30%, transparent 95%)",
              WebkitMaskImage: "linear-gradient(to top, black 30%, transparent 95%)",
            }}
          >
            shadw
          </span>
        </div>
        <div className="relative z-10 mx-auto max-w-[1500px] px-6 pt-14 lg:px-10">
          <div className="grid grid-cols-2 gap-10 md:grid-cols-5">
            <div className="col-span-2 md:col-span-2">
              <Logo className="h-7" />
              <p className="mt-4 max-w-xs text-sm leading-relaxed text-zinc-500">
                Unleash the Power of Peer-to-Peer: Seamlessly Connect, Collaborate, and Conquer.
              </p>
            </div>
            {FOOTER_COLS.map((col) => (
              <div key={col.title}>
                <div className="text-sm font-semibold text-white">{col.title}</div>
                <ul className="mt-4 space-y-3">
                  {col.links.map((l) => (
                    <li key={l.label}>
                      <Link href={l.href} className="text-sm text-zinc-500 transition-colors hover:text-zinc-200">{l.label}</Link>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>

          <div className="mt-12 flex flex-col items-center justify-between gap-4 pt-6 sm:flex-row">
            <span className="text-sm text-zinc-600">2026 Shadw. All rights reserved.</span>
            <div className="flex items-center gap-5 text-zinc-500">
              <a href="#" aria-label="Twitter" className="transition-colors hover:text-white"><Twitter className="h-4 w-4" /></a>
              <a href="#" aria-label="GitHub" className="transition-colors hover:text-white"><Github className="h-4 w-4" /></a>
              <a href="#" aria-label="LinkedIn" className="transition-colors hover:text-white"><Linkedin className="h-4 w-4" /></a>
              <a href="#" aria-label="Dribbble" className="transition-colors hover:text-white"><Dribbble className="h-4 w-4" /></a>
            </div>
          </div>
          {/* Reserve vertical space below the content so the watermark wordmark
              has room to show at the bottom of the page. */}
          <div className="h-40 sm:h-56" />
        </div>
      </footer>
    </div>
  );
}

/* ---- Small shared marketing primitives (dark theme) ---- */

export function MarketingHero({ eyebrow, title, subtitle, children }: { eyebrow?: string; title: React.ReactNode; subtitle?: string; children?: React.ReactNode }) {
  return (
    <section className="relative">
      <div className="pointer-events-none absolute inset-0">
        <div className="absolute -left-40 top-0 h-[40rem] w-[40rem] rounded-full bg-violet-700/25 blur-[140px]" />
        <div className="absolute -left-20 top-16 h-[22rem] w-[22rem] rounded-full bg-pink-500/30 blur-[110px]" />
        <div className="absolute -right-40 top-40 h-[34rem] w-[34rem] rounded-full bg-cyan-500/15 blur-[140px]" />
        <div className="absolute -right-16 top-52 h-[20rem] w-[20rem] rounded-full bg-sky-400/30 blur-[110px]" />
      </div>
      <div className="relative mx-auto max-w-4xl px-6 pb-12 pt-24 text-center sm:pt-28">
        {eyebrow && (
          <div className="mb-5 inline-flex items-center rounded-full border border-white/15 bg-white/5 px-3 py-1 text-xs font-medium uppercase tracking-wider text-zinc-300">
            {eyebrow}
          </div>
        )}
        <h1 className="text-balance text-4xl font-bold leading-[1.08] tracking-tight sm:text-6xl">{title}</h1>
        {subtitle && <p className="mx-auto mt-6 max-w-2xl text-lg leading-relaxed text-zinc-400">{subtitle}</p>}
        {children && <div className="mt-10 flex flex-wrap items-center justify-center gap-4">{children}</div>}
      </div>
    </section>
  );
}

export function MarketingSection({ className = "", children }: { className?: string; children: React.ReactNode }) {
  return <section className={`relative mx-auto max-w-6xl px-6 ${className}`}>{children}</section>;
}

export function GlowCard({ className = "", children }: { className?: string; children: React.ReactNode }) {
  return (
    <div className={`rounded-2xl border border-white/10 bg-white/[0.03] p-6 transition-colors hover:border-white/20 ${className}`}>
      {children}
    </div>
  );
}
