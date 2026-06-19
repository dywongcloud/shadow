"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useMemo, useRef, useState } from "react";
import { Search, ChevronRight, Sparkles } from "lucide-react";
import { Logo } from "@/components/logo";
import { ThemeToggle } from "@/components/theme-toggle";

interface NavItem { label: string; href: string; badge?: string }
const NAV: { group: string; items: NavItem[] }[] = [
  {
    group: "Start",
    items: [
      { label: "Overview", href: "/docs" },
      { label: "Getting started", href: "/docs/getting-started" },
      { label: "Fundamentals", href: "/docs/getting-started#introduction" },
      { label: "Production checklist", href: "/docs/getting-started#self-hosting" },
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
      { label: "Regions & the mesh", href: "/docs/getting-started#regions" },
      { label: "CLI", href: "/docs/getting-started#cli" },
      { label: "API reference", href: "/docs/getting-started#api" },
      { label: "Self-hosting", href: "/docs/getting-started#self-hosting", badge: "Ops" },
    ],
  },
];

export default function DocsLayout({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  const [q, setQ] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);

  // ⌘K / "/" focuses the docs search.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        searchRef.current?.focus();
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

  const isActive = (href: string) => {
    const base = href.split("#")[0];
    return pathname === base && (href === "/docs" ? pathname === "/docs" : true);
  };

  return (
    <div className="relative left-1/2 w-screen -translate-x-1/2 -my-8 min-h-screen bg-bg text-fg">
      {/* Top nav */}
      <header className="sticky top-0 z-30 border-b border-border bg-bg/85 backdrop-blur">
        <div className="flex h-14 items-center justify-between px-4 lg:px-6">
          <div className="flex items-center gap-3">
            <Link href="/"><Logo className="h-6 w-auto" /></Link>
            <span className="text-muted">/</span>
            <Link href="/docs" className="text-[15px] font-semibold">Docs</Link>
            <nav className="ml-6 hidden items-center gap-6 text-sm text-secondary md:flex">
              <Link href="/docs/getting-started" className="hover:text-fg">Guides</Link>
              <Link href="/docs/getting-started#api" className="hover:text-fg">API</Link>
              <Link href="/docs/getting-started" className="hover:text-fg">Getting Started</Link>
            </nav>
          </div>
          <div className="flex items-center gap-2.5">
            <button
              onClick={() => searchRef.current?.focus()}
              className="hidden items-center gap-1.5 rounded-full border border-border px-3 py-1.5 text-sm font-medium hover:bg-subtle sm:flex"
            >
              <Sparkles className="h-3.5 w-3.5" /> Ask AI
            </button>
            <ThemeToggle />
            <Link href="/sign-in" className="rounded-md border border-border px-3 py-1.5 text-sm hover:bg-subtle">Dashboard</Link>
          </div>
        </div>
      </header>

      <div className="mx-auto flex max-w-[1600px]">
        {/* Sidebar */}
        <aside className="sticky top-14 hidden h-[calc(100vh-3.5rem)] w-72 shrink-0 overflow-y-auto border-r border-border px-4 py-6 lg:block">
          <div className="relative mb-5">
            <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
            <input
              ref={searchRef}
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="Search Docs"
              className="w-full rounded-lg border border-border bg-card py-2 pl-9 pr-12 text-sm outline-none focus:border-border-strong"
            />
            <kbd className="absolute right-2.5 top-1/2 -translate-y-1/2 rounded border border-border bg-subtle px-1.5 py-0.5 font-mono text-[10px] text-muted">⌘K</kbd>
          </div>
          {groups.map((g) => (
            <div key={g.group} className="mb-5">
              <div className="mb-1.5 px-3 text-[11px] font-medium uppercase tracking-wide text-muted">{g.group}</div>
              <ul>
                {g.items.map((it) => (
                  <li key={it.label}>
                    <Link
                      href={it.href}
                      className={`group flex items-center justify-between rounded-md px-3 py-1.5 text-sm transition-colors ${
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
          {!groups.length && <div className="px-3 py-8 text-center text-sm text-muted">No matches.</div>}
        </aside>

        {/* Content */}
        <main className="min-w-0 flex-1 px-5 py-10 sm:px-8 lg:px-14">{children}</main>
      </div>
    </div>
  );
}
