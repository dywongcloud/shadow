"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { Bell, ChevronsUpDown } from "lucide-react";
import { cn } from "@/lib/utils";
import { ThemeToggle } from "@/components/theme-toggle";

const tabs = [
  { href: "/", label: "Overview" },
  { href: "/projects", label: "Projects" },
  { href: "/integrations", label: "Integrations" },
  { href: "/activity", label: "Activity" },
  { href: "/domains", label: "Domains" },
  { href: "/firewall", label: "Firewall" },
  { href: "/network", label: "Network" },
  { href: "/usage", label: "Usage" },
  { href: "/settings", label: "Settings" },
];

function HiveMark() {
  // Rounded-square brand mark (like the reference logo tile).
  return (
    <span className="flex h-7 w-7 items-center justify-center rounded-lg bg-fg text-bg">
      <svg width="14" height="14" viewBox="0 0 24 24" aria-hidden>
        <path d="M12 2 L21 7 V17 L12 22 L3 17 V7 Z" fill="none" stroke="currentColor" strokeWidth="2.2" />
      </svg>
    </span>
  );
}

export function TopNav() {
  const pathname = usePathname();
  const isActive = (href: string) =>
    href === "/" ? pathname === "/" : pathname.startsWith(href);

  return (
    <header className="sticky top-0 z-30 border-b border-border bg-bg/85 backdrop-blur">
      {/* Row 1: breadcrumb + account */}
      <div className="mx-auto flex h-[52px] max-w-[1400px] items-center justify-between px-4 sm:px-6">
        <div className="flex items-center gap-2.5 text-sm">
          <HiveMark />
          <span className="font-medium">Hive Cloud</span>
          <span className="rounded-full border border-border px-2 py-0.5 text-[11px] text-secondary">Hobby</span>
          <Selector />
          <span className="px-0.5 text-border-strong">/</span>
          <span className="hidden font-medium sm:inline">Dylan</span>
          <Selector className="hidden sm:flex" />
          <span className="hidden px-0.5 text-border-strong md:inline">/</span>
          <span className="hidden items-center gap-1.5 md:flex">
            <span className="h-2 w-2 rounded-full bg-[#f5a623]" />
            <span className="font-medium">Production</span>
          </span>
          <Selector className="hidden md:flex" />
        </div>
        <div className="flex items-center gap-2">
          <button className="relative flex h-8 w-8 items-center justify-center rounded-md text-secondary hover:bg-subtle hover:text-fg">
            <Bell className="h-4 w-4" />
            <span className="absolute -right-0.5 -top-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-[#0070f3] px-1 text-[10px] font-semibold text-white">1</span>
          </button>
          <ThemeToggle />
          <Link href="/new">
            <button className="rounded-md border border-border-strong px-3 py-1.5 text-sm font-medium hover:bg-subtle">Invite</button>
          </Link>
          <Link href="/account" className="ml-1 flex h-8 w-8 items-center justify-center rounded-full bg-[#0761d1] text-xs font-semibold text-white">D</Link>
        </div>
      </div>
      {/* Row 2: tabs — active underline sits flush on the header's bottom border */}
      <div className="mx-auto max-w-[1400px] px-2 sm:px-4">
        <nav className="-mb-px flex items-center gap-0.5 overflow-x-auto">
          {tabs.map((t) => {
            const active = isActive(t.href);
            return (
              <Link
                key={t.href}
                href={t.href}
                className={cn(
                  "whitespace-nowrap border-b-2 px-3 pb-2.5 pt-1 text-sm transition-colors",
                  active
                    ? "border-fg text-fg"
                    : "border-transparent text-secondary hover:text-fg"
                )}
              >
                {t.label}
              </Link>
            );
          })}
        </nav>
      </div>
    </header>
  );
}

function Selector({ className }: { className?: string }) {
  return (
    <span className={cn("flex cursor-pointer items-center text-muted hover:text-fg", className)}>
      <ChevronsUpDown className="h-3.5 w-3.5" />
    </span>
  );
}
