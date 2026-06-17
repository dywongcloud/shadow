"use client";

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
} from "lucide-react";
import { cn } from "@/lib/utils";
import { ThemeToggle } from "@/components/theme-toggle";

const nav = [
  { href: "/admin", label: "Overview", icon: LayoutDashboard },
  { href: "/admin/incidents", label: "Incidents", icon: Siren },
  { href: "/admin/teams", label: "Teams", icon: Users },
  { href: "/admin/databases", label: "Databases", icon: Database },
  { href: "/admin/nodes", label: "Infrastructure", icon: Server },
  { href: "/admin/audit", label: "Audit log", icon: ScrollText },
];

export default function AdminLayout({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  return (
    <div className="-mx-4 -my-8 flex min-h-screen sm:-mx-6">
      {/* Ops sidebar */}
      <aside className="sticky top-0 hidden h-screen w-56 shrink-0 flex-col border-r border-border bg-subtle/50 px-3 py-5 md:flex">
        <div className="mb-5 flex items-center gap-2 px-2">
          <span className="flex h-7 w-7 items-center justify-center rounded-lg bg-fg text-bg">
            <Server className="h-3.5 w-3.5" />
          </span>
          <div>
            <div className="text-sm font-semibold leading-tight">Operations</div>
            <div className="text-[10px] uppercase tracking-wide text-muted">Platform owner</div>
          </div>
        </div>
        <nav className="flex flex-1 flex-col gap-0.5">
          {nav.map((n) => {
            const active = n.href === "/admin" ? pathname === "/admin" : pathname.startsWith(n.href);
            const Icon = n.icon;
            return (
              <Link
                key={n.href}
                href={n.href}
                className={cn(
                  "flex items-center gap-2.5 rounded-md px-2.5 py-1.5 text-sm transition-colors",
                  active ? "bg-card font-medium text-fg shadow-card" : "text-secondary hover:bg-card hover:text-fg"
                )}
              >
                <Icon className="h-4 w-4" /> {n.label}
              </Link>
            );
          })}
        </nav>
        <Link href="/" className="mt-4 flex items-center gap-2 px-2.5 text-xs text-muted hover:text-fg">
          <ArrowLeft className="h-3.5 w-3.5" /> Back to dashboard
        </Link>
      </aside>

      {/* Content */}
      <div className="min-w-0 flex-1">
        <div className="flex items-center justify-between border-b border-border px-4 py-3 sm:px-6">
          <div className="flex items-center gap-2 text-sm text-secondary">
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
