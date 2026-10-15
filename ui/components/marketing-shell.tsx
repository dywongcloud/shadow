"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Menu, X } from "lucide-react";
import Image from "next/image";
import "@/components/landing/landing.css";

const ELECTROLIZE_FONT = "var(--font-electrolize), ui-sans-serif, system-ui, sans-serif";

export const MARKETING_NAV: { label: string; href: string }[] = [
  { label: "Product", href: "/product" },
  { label: "Solutions", href: "/solutions" },
  { label: "Docs", href: "/docs" },
  { label: "Pricing", href: "/pricing" },
];

const FOOTER_COLS: { title: string; links: { label: string; href: string }[] }[] = [
  {
    title: "Product",
    links: [
      { label: "Overview", href: "/product" },
      { label: "Features", href: "/features" },
      { label: "Solutions", href: "/solutions" },
      { label: "Pricing", href: "/pricing" },
    ],
  },
  {
    title: "Resources",
    links: [
      { label: "Documentation", href: "/docs" },
      { label: "API reference", href: "/docs/api-reference" },
      { label: "Getting started", href: "/docs/getting-started" },
      { label: "Status", href: "/status" },
    ],
  },
  {
    title: "Company",
    links: [
      { label: "Contact", href: "/contact" },
      { label: "Privacy", href: "/privacy" },
    ],
  },
];

function Logo({ className = "h-7" }: { className?: string }) {
  return <Image src="/shadw-logo-wordmark.png" alt="shadw" width={1826} height={407} priority className={`${className} w-auto select-none`} />;
}

const MARKETING_SURFACE_COLOR = "#0c0d10";

/** Forces the dark brand surface on the document root while mounted (overscroll, gutters and native scrollbars match). The full-bleed `w-screen` shell is
 *  100vw, wider than a window that carries a classic scrollbar: `html:has(#lp-content)` (landing.css) clips the root from the first byte, which hides the horizontal
 *  scrollbar, and the `body` clip set here then clips for itself (a `body` clip alone propagates to the viewport and leaves the document 5 px wider than the window,
 *  and wide children such as the ticker track widen it further once the root is clipped). Background reverts to "" rather than a snapshot, because a fast
 *  logout-login remount would capture the forced values; only the color-scheme next-themes owns is handed back. */
function useForcedDarkRoot() {
  useEffect(() => {
    const html = document.documentElement;
    const body = document.body;
    const prevScheme = html.style.colorScheme;
    html.style.backgroundColor = MARKETING_SURFACE_COLOR;
    body.style.backgroundColor = MARKETING_SURFACE_COLOR;
    html.style.colorScheme = "dark";
    body.style.overflowX = "clip";
    // the browser's own bar: a light-scheme visitor would otherwise get the layout's white theme-color above a black page
    const bars = Array.from(document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]'));
    const barColors = bars.map((m) => m.content);
    bars.forEach((m) => (m.content = MARKETING_SURFACE_COLOR));
    return () => {
      bars.forEach((m, i) => (m.content = barColors[i]));
      html.style.backgroundColor = "";
      body.style.backgroundColor = "";
      html.style.colorScheme = prevScheme;
      body.style.overflowX = "";
    };
  }, []);
}

/** `lead` is rendered right after the skip link, so a control that must be a first Tab stop (the landing's Pause) is one. */
export function MarketingShell({ children, lead }: { children: React.ReactNode; lead?: React.ReactNode }) {
  const pathname = usePathname();
  const active = (href: string) => pathname === href || (href !== "/" && pathname.startsWith(href + "/"));

  useForcedDarkRoot();

  const [menuOpen, setMenuOpen] = useState(false);
  const toggleRef = useRef<HTMLButtonElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  // eslint-disable-next-line react-hooks/set-state-in-effect -- resets menu-open state whenever the route changes, a derived-reset-on-change sync
  useEffect(() => setMenuOpen(false), [pathname]);
  // An open menu is a dialog: focus moves into it (it is appended to <body>, 23 Tab stops away otherwise), the page behind is `inert`, Tab wraps
  // inside it, and focus returns to the toggle when it closes.
  useEffect(() => {
    if (!menuOpen) return;
    closeRef.current?.focus();
    const toggle = toggleRef.current;
    return () => toggle?.focus();
  }, [menuOpen]);
  const wrapTab = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== "Tab") return;
    const items = e.currentTarget.querySelectorAll<HTMLElement>("a[href], button");
    const first = items[0], last = items[items.length - 1];
    if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
  };
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setMenuOpen(false);
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
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
      inert={menuOpen}
      className="relative left-1/2 w-screen -translate-x-1/2 -my-8 overflow-x-clip bg-[#0c0d10] text-white"
      style={{ fontFamily: "var(--font-electrolize), ui-sans-serif, system-ui, sans-serif" }}
    >
      <a
        href="#lp-content"
        className="sr-only focus:not-sr-only focus:fixed focus:left-4 focus:top-4 focus:z-[300] focus:rounded-md focus:bg-white focus:px-4 focus:py-2 focus:text-[14px] focus:text-black"
      >
        Skip to content
      </a>
      {lead}
      <header className="sticky top-0 z-40 border-b border-white/10 bg-[#0c0d10]/70 backdrop-blur-xl">
        <div className="mx-auto grid max-w-[1500px] grid-cols-[1fr_auto_1fr] items-center gap-6 px-6 py-3 lg:px-10">
          <Link href="/" className="inline-flex min-h-[44px] shrink-0 items-center justify-self-start">
            <Logo className="h-7 sm:h-8" />
          </Link>
          <nav aria-label="Primary" className="col-start-2 hidden items-center gap-10 text-[13px] uppercase tracking-[0.18em] text-zinc-300 lg:flex">
            {MARKETING_NAV.map((n) => (
              <Link
                key={n.href}
                href={n.href}
                aria-current={active(n.href) ? "page" : undefined}
                className={`relative py-[13px] transition-colors hover:text-white after:absolute after:inset-x-0 after:bottom-[5px] after:h-px after:bg-gradient-to-r after:from-transparent after:via-indigo-300 after:to-transparent after:transition-opacity ${
                  active(n.href) ? "text-white after:opacity-100" : "after:opacity-0 hover:after:opacity-60"
                }`}
              >
                {n.label}
              </Link>
            ))}
          </nav>
          <div className="col-start-3 flex items-center gap-2 justify-self-end sm:gap-3">
            <Link
              href="/sign-in"
              className="hidden rounded-full px-4 py-[13px] text-[13px] uppercase tracking-[0.18em] text-zinc-300 transition-colors hover:text-white lg:inline-flex"
            >
              Sign in
            </Link>
            <Link
              href="/sign-up"
              className="hidden rounded-full border border-transparent bg-white px-5 py-[11px] text-[13px] font-semibold uppercase tracking-[0.14em] text-black shadow-[0_0_0_1px_rgba(255,255,255,0.6),0_8px_30px_-8px_rgba(129,140,248,0.75)] transition-transform hover:scale-[1.04] lg:inline-flex"
            >
              Get started
            </Link>
            <button
              ref={toggleRef}
              type="button"
              aria-label={menuOpen ? "Close menu" : "Open menu"}
              aria-expanded={menuOpen}
              onClick={() => setMenuOpen((o) => !o)}
              className="inline-flex h-[44px] w-[44px] items-center justify-center rounded-md border border-white/20 text-white transition-colors hover:bg-white/10 lg:hidden"
            >
              {menuOpen ? <X className="h-5 w-5" /> : <Menu className="h-5 w-5" />}
            </button>
          </div>
        </div>
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-x-0 -bottom-px h-px"
          style={{ background: "linear-gradient(90deg, transparent, rgba(129,140,248,0.6) 28%, rgba(34,211,238,0.5) 72%, transparent)" }}
        />
      </header>

      {menuOpen &&
        typeof document !== "undefined" &&
        createPortal(
          <div
            role="dialog"
            aria-modal="true"
            aria-label="Menu"
            onKeyDown={wrapTab}
            className="fixed inset-0 z-[200] flex flex-col bg-[#0c0d10] text-white lg:hidden"
            style={{ fontFamily: ELECTROLIZE_FONT }}
          >
            <div className="flex items-center justify-between border-b border-white/10 px-6 py-3.5">
              <Link href="/" onClick={() => setMenuOpen(false)} className="inline-flex min-h-[44px] shrink-0 items-center">
                <Logo className="h-7" />
              </Link>
              <button
                ref={closeRef}
                type="button"
                aria-label="Close menu"
                onClick={() => setMenuOpen(false)}
                className="inline-flex h-[44px] w-[44px] items-center justify-center rounded-md border border-white/20 text-white transition-colors hover:bg-white/10"
              >
                <X className="h-5 w-5" />
              </button>
            </div>
            <nav aria-label="Primary" className="flex flex-1 flex-col overflow-y-auto px-6 py-6">
              <ul className="flex flex-col">
                {MARKETING_NAV.map((n) => (
                  <li key={n.href}>
                    <Link
                      href={n.href}
                      onClick={() => setMenuOpen(false)}
                      aria-current={active(n.href) ? "page" : undefined}
                      className={`block border-b border-white/5 py-4 text-[18px] font-medium transition-colors hover:text-white ${
                        active(n.href) ? "text-white" : "text-zinc-300"
                      }`}
                    >
                      {n.label}
                    </Link>
                  </li>
                ))}
              </ul>
              <div className="mt-8 flex flex-col gap-3">
                <Link
                  href="/sign-up"
                  onClick={() => setMenuOpen(false)}
                  className="inline-flex w-full items-center justify-center rounded-full border border-transparent bg-white px-5 py-3.5 text-[16px] font-semibold text-black"
                >
                  Get started
                </Link>
                <Link
                  href="/sign-in"
                  onClick={() => setMenuOpen(false)}
                  className="inline-flex w-full items-center justify-center rounded-full border border-white/30 px-5 py-3.5 text-[16px] font-medium text-white transition-colors hover:bg-white/10"
                >
                  Sign in
                </Link>
              </div>
            </nav>
          </div>,
          document.body
        )}

      <div id="lp-content" tabIndex={-1} className="outline-none">
        {children}
      </div>

      <footer className="relative overflow-hidden bg-[#0c0d10]">
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-x-0 bottom-0 h-[26rem]"
          style={{
            background:
              "radial-gradient(420px 420px at 14% 100%, rgba(45,212,191,0.28), transparent 70%), radial-gradient(540px 540px at 42% 106%, rgba(79,70,229,0.4), transparent 70%), radial-gradient(420px 420px at 66% 100%, rgba(192,38,211,0.26), transparent 70%), radial-gradient(450px 450px at 92% 100%, rgba(34,211,238,0.24), transparent 70%)",
          }}
        />
        <div className="pointer-events-none absolute inset-x-0 bottom-0 flex justify-center overflow-hidden">
          <span
            aria-hidden="true"
            className="select-none whitespace-nowrap bg-clip-text font-bold leading-[0.78] tracking-tighter text-transparent"
            style={{
              fontSize: "clamp(7rem, 30vw, 28rem)",
              opacity: 0.16,
              transform: "translateY(12%)",
              backgroundImage: "linear-gradient(100deg, #5eead4 0%, #22d3ee 30%, #818cf8 60%, #e879f9 100%)",
              maskImage: "linear-gradient(to top, black 30%, transparent 95%)",
              WebkitMaskImage: "linear-gradient(to top, black 30%, transparent 95%)",
            }}
          >
            shadw
          </span>
        </div>
        <div className="relative z-10 mx-auto max-w-[1500px] px-6 pt-14 lg:px-10">
          <div className="grid grid-cols-2 gap-10 md:grid-cols-4">
            <div className="col-span-2 md:col-span-1">
              <Logo className="h-7" />
              <p className="mt-4 max-w-xs text-sm leading-relaxed text-zinc-400">
                Serverless functions and containers deployed from Git, plus managed data.
              </p>
            </div>
            {FOOTER_COLS.map((col) => (
              <div key={col.title} className={col.title === "Company" ? "col-span-2 md:col-span-1" : ""}>
                <div className="text-sm font-semibold text-white">{col.title}</div>
                <ul className="mt-3 lg:space-y-0.5">
                  {col.links.map((l) => (
                    <li key={l.label}>
                      <Link href={l.href} className="tap-44 inline-flex min-h-[44px] min-w-[44px] items-center text-sm text-zinc-400 transition-colors hover:text-white lg:min-h-0 lg:min-w-0 lg:py-1.5">
                        {l.label}
                      </Link>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>

          <div className="mt-12 flex flex-col items-center justify-between gap-4 border-t border-white/10 pt-6 sm:flex-row">
            <span className="text-sm text-zinc-400">© 2026 shadw. All rights reserved.</span>
          </div>
          <div className="h-40 sm:h-56" />
        </div>
      </footer>
    </div>
  );
}

export function MarketingHero({ eyebrow, title, subtitle, children }: { eyebrow?: string; title: React.ReactNode; subtitle?: string; children?: React.ReactNode }) {
  return (
    <section className="lp lp-hero">
      <div className="lp-bg" aria-hidden="true">
        <div className="lp-aurora-a" />
        <div className="lp-aurora-b" />
        <div className="lp-grid" />
        <div className="lp-grain" />
      </div>
      <div className="relative z-10 mx-auto max-w-[920px] px-6 pb-16 pt-16 sm:pt-24">
        {eyebrow && <span className="lp-pill !px-4 !py-1.5 uppercase !tracking-[0.18em] !text-[12px]">{eyebrow}</span>}
        <h1 className="lp-h1 lp-h1-page mt-6">{title}</h1>
        {subtitle && <p className="lp-sub mx-auto mt-6 max-w-[680px]">{subtitle}</p>}
        {children && <div className="mt-9 flex flex-wrap items-center justify-center gap-4">{children}</div>}
      </div>
    </section>
  );
}

export function MarketingSection({ className = "", children }: { className?: string; children: React.ReactNode }) {
  return <section className={`relative mx-auto max-w-6xl px-6 ${className}`}>{children}</section>;
}

/** A marketing card: gradient hairline, a spotlight that follows the pointer (`--mx`/`--my` read by `.lp-card::before`). */
export function GlowCard({ className = "", children }: { className?: string; children: React.ReactNode }) {
  const spot = (e: React.PointerEvent<HTMLDivElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    e.currentTarget.style.setProperty("--mx", `${e.clientX - r.left}px`);
    e.currentTarget.style.setProperty("--my", `${e.clientY - r.top}px`);
  };
  return (
    <div onPointerMove={spot} className={`lp-card p-6 ${className}`}>
      {children}
    </div>
  );
}
