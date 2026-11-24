"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useMemo, useRef, useState } from "react";
import { Search, ChevronRight, ChevronDown } from "lucide-react";
import { Logo } from "@/components/logo";
import { ThemeToggle } from "@/components/theme-toggle";
import { ApiSidebar } from "@/components/api-sidebar";

interface NavItem { label: string; href: string; badge?: string }
const NAV: { group: string; items: NavItem[] }[] = [
  {
    group: "Start",
    items: [
      { label: "Overview", href: "/docs" },
      { label: "Getting started", href: "/docs/getting-started" },
      { label: "Introduction", href: "/docs/getting-started#introduction" },
      { label: "Quickstart", href: "/docs/getting-started#quickstart" },
    ],
  },
  {
    group: "Deploy",
    items: [
      { label: "Deploying apps", href: "/docs/getting-started#deploying" },
      { label: "Environment & secrets", href: "/docs/getting-started#env" },
      { label: "GitOps", href: "/docs/getting-started#gitops" },
      { label: "Domains & TLS", href: "/docs/getting-started#domains" },
    ],
  },
  {
    group: "Platform",
    items: [
      { label: "Platform overview", href: "/docs/platform" },
      { label: "AI Gateway", href: "/docs/platform#ai-gateway" },
      { label: "Managed data", href: "/docs/platform#managed-data" },
      { label: "Security", href: "/docs/platform#security" },
      { label: "Regions", href: "/docs/getting-started#regions" },
      { label: "CLI", href: "/docs/getting-started#cli" },
      { label: "API reference", href: "/docs/api-reference" },
      { label: "Self-hosting", href: "/docs/getting-started#self-hosting", badge: "Ops" },
    ],
  },
  {
    group: "Examples",
    items: [
      { label: "Worked examples", href: "/docs/examples" },
      { label: "What's new", href: "/docs/platform#whats-new" },
    ],
  },
  {
    group: "Errors",
    items: [
      { label: "DEPLOYMENT_NOT_FOUND", href: "/docs/errors/deployment-not-found" },
    ],
  },
];

export default function DocsLayout({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  const isApi = pathname.startsWith("/docs/api-reference");
  const [q, setQ] = useState("");
  const [mobileOpen, setMobileOpen] = useState(false);
  const searchRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setMobileOpen(true);
        requestAnimationFrame(() => searchRef.current?.focus());
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const groups = useMemo(() => {
    const s = q.trim().toLowerCase();
    if (!s) return NAV;
    return NAV.map((g) => ({ ...g, items: g.items.filter((i) => i.label.toLowerCase().includes(s)) })).filter((g) => g.items.length);
  }, [q]);

  const [hash, setHash] = useState("");
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setHash(window.location.hash);
    const onHash = () => setHash(window.location.hash);
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, [pathname]);

  useEffect(() => {
    const frags = NAV.flatMap((g) => g.items)
      .filter((i) => i.href.split("#")[0] === pathname && i.href.includes("#"))
      .map((i) => i.href.split("#")[1]);
    const els = frags
      .map((f) => document.getElementById(f))
      .filter((el): el is HTMLElement => !!el);
    if (!els.length) return;
    const obs = new IntersectionObserver(
      (entries) => {
        const visible = entries
          .filter((e) => e.isIntersecting)
          .sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top);
        if (visible[0]) setHash(`#${visible[0].target.id}`);
      },
      { rootMargin: "-80px 0px -70% 0px", threshold: 0 }
    );
    els.forEach((el) => obs.observe(el));
    return () => obs.disconnect();
  }, [pathname]);

  const isActive = (href: string) => {
    const [base, frag] = href.split("#");
    if (pathname !== base) return false;
    return frag ? hash === `#${frag}` : hash === "" || hash === "#";
  };

  return (
    <div className="relative left-1/2 w-screen -translate-x-1/2 -my-8 min-h-screen bg-bg text-fg">
      <a
        href="#docs-content"
        onClick={(e) => {
          e.preventDefault();
          document.getElementById("docs-content")?.focus();
        }}
        className="sr-only focus:not-sr-only focus:fixed focus:left-4 focus:top-4 focus:z-[300] focus:min-h-[44px] focus:items-center focus:rounded-md focus:bg-fg focus:px-4 focus:py-2 focus:text-sm focus:font-medium focus:text-bg"
      >
        Skip to content
      </a>
      <header className="sticky top-0 z-30 border-b border-border bg-bg/85 backdrop-blur">
        <div className="flex h-14 items-center justify-between px-4 lg:px-6">
          <div className="flex min-w-0 items-center gap-3">
            <Link href="/" className="tap-44 inline-flex shrink-0 items-center"><Logo className="h-5 w-auto max-w-none sm:h-6" /></Link>
            <span className="text-muted">/</span>
            <Link href="/docs" className="tap-44 inline-flex items-center text-[15px] font-semibold">Docs</Link>
            <nav aria-label="Docs sections" className="ml-6 hidden min-w-0 items-center gap-6 text-sm text-secondary md:flex">
              <Link href="/docs/getting-started" className="tap-44 inline-flex items-center hover:text-fg">Guides</Link>
              <Link href="/docs/api-reference" className="tap-44 inline-flex items-center hover:text-fg">API</Link>
              <Link href="/docs/platform" className="tap-44 inline-flex items-center hover:text-fg">Platform</Link>
              <Link href="/docs/examples" className="tap-44 inline-flex items-center hover:text-fg">Examples</Link>
            </nav>
          </div>
          <div className="flex min-w-0 items-center gap-2.5">
            <ThemeToggle />
            <Link href="/sign-in" className="tap-44 inline-flex items-center rounded-md border border-border px-3 py-1.5 text-sm hover:bg-subtle">Dashboard</Link>
          </div>
        </div>
      </header>

      <div className="mx-auto flex max-w-[1600px] flex-col lg:flex-row">
        <nav
          aria-label="Docs navigation"
          className="w-full border-b border-border px-4 py-3 lg:sticky lg:top-14 lg:h-[calc(100vh-3.5rem)] lg:w-72 lg:shrink-0 lg:overflow-y-auto lg:border-b-0 lg:border-r lg:px-4 lg:py-6"
        >
          <button
            type="button"
            aria-expanded={mobileOpen}
            aria-controls="docs-nav-groups"
            onClick={() => setMobileOpen((v) => !v)}
            className="tap-44 flex w-full items-center justify-between rounded-lg border border-border px-3 py-2 text-sm font-medium lg:hidden"
          >
            Browse docs
            <ChevronDown className={`h-4 w-4 transition-transform ${mobileOpen ? "rotate-180" : ""}`} />
          </button>
          <div id="docs-nav-groups" className={`${mobileOpen ? "mt-3" : "hidden"} lg:mt-0 lg:block`}>
            {isApi ? (
              <ApiSidebar />
            ) : (
              <>
                <div className="relative mb-5">
                  <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
                  <input
                    ref={searchRef}
                    value={q}
                    onChange={(e) => setQ(e.target.value)}
                    placeholder="Search Docs"
                    aria-label="Search docs"
                    className="tap-44 w-full rounded-lg border border-border bg-card py-2 pl-9 pr-12 text-sm outline-none focus-visible:ring-2 focus-visible:ring-link focus-visible:ring-offset-2 focus-visible:ring-offset-bg"
                  />
                  <kbd className="absolute right-2.5 top-1/2 -translate-y-1/2 rounded border border-border bg-subtle px-1.5 py-0.5 font-mono text-[10px] text-muted">⌘K</kbd>
                </div>
                {groups.map((g) => (
                  <div key={g.group} className="mb-5">
                    <div className="mb-1.5 px-3 text-[11px] font-medium uppercase tracking-wide text-secondary">{g.group}</div>
                    <ul>
                      {g.items.map((it) => (
                        <li key={it.label}>
                          <Link
                            href={it.href}
                            className={`tap-44 group flex items-center justify-between rounded-md px-3 py-1.5 text-sm transition-colors ${
                              isActive(it.href) ? "bg-subtle font-medium text-fg" : "text-secondary hover:bg-subtle/60 hover:text-fg"
                            }`}
                          >
                            <span className="flex items-center gap-2">
                              {it.label}
                              {it.badge && <span className="rounded-full bg-link/10 px-1.5 py-0.5 text-[10px] font-medium text-link">{it.badge}</span>}
                            </span>
                            {it.href.includes("#") && <ChevronRight className="h-3.5 w-3.5 text-muted opacity-0 group-hover:opacity-100" />}
                          </Link>
                        </li>
                      ))}
                    </ul>
                  </div>
                ))}
                {!groups.length && <div className="px-3 py-8 text-center text-sm text-secondary">No matches.</div>}
              </>
            )}
          </div>
        </nav>

        <div id="docs-content" tabIndex={-1} className="min-w-0 flex-1 px-5 py-10 outline-none sm:px-8 lg:px-14">{children}</div>
      </div>
    </div>
  );
}
