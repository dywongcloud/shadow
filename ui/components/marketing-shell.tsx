"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { Twitter, Github, Linkedin, Dribbble } from "lucide-react";

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
function Logo({ className = "h-6" }: { className?: string }) {
  return <img src="/shadw-logo-dark.png" alt="shadw" className={`${className} w-auto select-none`} />;
}

export function MarketingShell({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  const active = (href: string) => pathname === href || (href !== "/" && pathname.startsWith(href + "/"));

  return (
    <div className="relative left-1/2 w-screen -translate-x-1/2 -my-8 overflow-hidden bg-black text-white">
      {/* Top nav */}
      <header className="relative z-20 border-b border-cyan-400/40 bg-black/80 backdrop-blur">
        <div className="mx-auto flex max-w-[1500px] items-center justify-between px-6 py-4 lg:px-10">
          <Link href="/"><Logo className="h-6" /></Link>
          <nav className="hidden items-center gap-8 text-[15px] text-zinc-300 lg:flex">
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
          <Link
            href="/sign-in"
            className="rounded-full border border-white/30 px-5 py-2 text-sm text-white transition-colors hover:bg-white/10"
          >
            Login
          </Link>
        </div>
      </header>

      {children}

      {/* Footer */}
      <footer className="relative overflow-hidden bg-black">
        {/* Aurora glows rising from the bottom edge — light blue, aqua, turquoise,
            indigo and pink fuchsia. */}
        <div className="pointer-events-none absolute inset-x-0 bottom-0 h-[26rem]">
          <div className="absolute -bottom-44 left-[6%] h-96 w-96 rounded-full bg-teal-400/25 blur-[120px]" />
          <div className="absolute -bottom-52 left-[33%] h-[30rem] w-[30rem] rounded-full bg-indigo-600/30 blur-[130px]" />
          <div className="absolute -bottom-44 left-[55%] h-96 w-96 rounded-full bg-fuchsia-600/20 blur-[120px]" />
          <div className="absolute -bottom-48 right-[6%] h-[26rem] w-[26rem] rounded-full bg-cyan-400/20 blur-[120px]" />
        </div>
        {/* Giant brand watermark — sits at the very bottom of the page, bleeding off
            the bottom edge, behind the footer content and lit by the glows. */}
        <div className="pointer-events-none absolute inset-x-0 bottom-0 flex justify-center overflow-hidden">
          <span
            className="select-none whitespace-nowrap bg-clip-text font-display font-bold leading-[0.78] tracking-tighter text-transparent"
            style={{
              fontSize: "clamp(7rem, 30vw, 28rem)",
              opacity: 0.25,
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
