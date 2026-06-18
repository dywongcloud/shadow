"use client";

import { useState } from "react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import {
  LayoutDashboard,
  Siren,
  Users,
  Database,
  Server,
  ScrollText,
  ArrowLeft,
  Table2,
  Menu,
  X,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { ThemeToggle } from "@/components/theme-toggle";

const nav = [
  { href: "/admin", label: "Overview", icon: LayoutDashboard },
  { href: "/admin/incidents", label: "Incidents", icon: Siren },
  { href: "/admin/teams", label: "Teams", icon: Users },
  { href: "/admin/databases", label: "Databases", icon: Database },
  { href: "/admin/data", label: "Data Browser", icon: Table2 },
  { href: "/admin/nodes", label: "Infrastructure", icon: Server },
  { href: "/admin/audit", label: "Audit log", icon: ScrollText },
];

function NavList({ pathname, onNavigate }: { pathname: string; onNavigate?: () => void }) {
  return (
    <nav className="flex flex-1 flex-col gap-0.5">
      {nav.map((n) => {
        const active = n.href === "/admin" ? pathname === "/admin" : pathname.startsWith(n.href);
        const Icon = n.icon;
        return (
          <Link
            key={n.href}
            href={n.href}
            onClick={onNavigate}
            className={cn(
              "flex items-center gap-2.5 rounded-md px-2.5 py-2 text-sm transition-colors",
              active ? "bg-fg/[0.07] font-medium text-fg" : "text-secondary hover:bg-fg/[0.04] hover:text-fg"
            )}
          >
            <Icon className="h-4 w-4 shrink-0" /> {n.label}
          </Link>
        );
      })}
    </nav>
  );
}

function SidebarHeader() {
  return (
    <div className="mb-5 flex items-center gap-2 px-2">
      <span className="flex h-7 w-7 items-center justify-center rounded-lg bg-fg text-bg">
        <Server className="h-3.5 w-3.5" />
      </span>
      <div>
        <div className="text-sm font-semibold leading-tight">Operations</div>
        <div className="text-[10px] uppercase tracking-wide text-muted">Platform owner</div>
      </div>
    </div>
  );
}

export default function AdminLayout({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  const [open, setOpen] = useState(false);

  return (
    // Full-bleed: break out of the centered max-w-[1400px] main so the sidebar
    // hugs the viewport's left edge and content fills to the right — no black
    // gutters. `mx-[calc(50%-50vw)]` spans the full viewport width regardless of
    // the parent container's width/padding.
    <div className="-my-8 mx-[calc(50%-50vw)] flex min-h-screen">
      {/* Desktop sidebar */}
      <aside className="sticky top-0 hidden h-screen w-60 shrink-0 flex-col border-r border-border bg-card px-3 py-5 md:flex">
        <SidebarHeader />
        <NavList pathname={pathname} />
        <Link href="/" className="mt-4 flex items-center gap-2 px-2.5 text-xs text-muted hover:text-fg">
          <ArrowLeft className="h-3.5 w-3.5" /> Back to dashboard
        </Link>
      </aside>

      {/* Mobile drawer */}
      {open ? (
        <div className="fixed inset-0 z-50 md:hidden">
          <div className="absolute inset-0 bg-black/50" onClick={() => setOpen(false)} />
          <aside className="absolute left-0 top-0 flex h-full w-64 max-w-[80vw] flex-col border-r border-border bg-card px-3 py-5 shadow-2xl">
            <div className="mb-5 flex items-center justify-between px-2">
              <SidebarHeader />
              <button onClick={() => setOpen(false)} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
            </div>
            <NavList pathname={pathname} onNavigate={() => setOpen(false)} />
            <Link href="/" className="mt-4 flex items-center gap-2 px-2.5 text-xs text-muted hover:text-fg">
              <ArrowLeft className="h-3.5 w-3.5" /> Back to dashboard
            </Link>
          </aside>
        </div>
      ) : null}

      {/* Content */}
      <div className="min-w-0 flex-1">
        <div className="flex items-center justify-between gap-2 border-b border-border px-4 py-3 sm:px-6">
          <div className="flex items-center gap-2 text-sm text-secondary">
            <button
              onClick={() => setOpen(true)}
              className="-ml-1 flex h-8 w-8 items-center justify-center rounded-md text-secondary hover:bg-subtle md:hidden"
              aria-label="Open menu"
            >
              <Menu className="h-4 w-4" />
            </button>
            <span className="font-medium text-fg">Operations Console</span>
            <span className="rounded-full border border-amber-500/30 bg-amber-500/10 px-2 py-0.5 text-[11px] font-medium text-amber-600 dark:text-amber-400">owner</span>
          </div>
          <ThemeToggle />
        </div>
        <div className="px-4 py-6 sm:px-6">{children}</div>
      </div>
    </div>
  );
}
