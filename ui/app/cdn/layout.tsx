"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { cn } from "@/lib/utils";

const tabs = [
  { href: "/cdn/routing", label: "Routing Rules" },
  { href: "/cdn/redirects", label: "Redirects" },
  { href: "/cdn/caches", label: "Caches" },
];

export default function CdnLayout({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  const current = tabs.find((t) => pathname.startsWith(t.href))?.label ?? "CDN";
  return (
    <div>
      <div className="mb-6 flex items-center gap-2 text-sm">
        <span className="font-medium text-muted">CDN</span>
        <span className="text-border-strong">/</span>
        <span className="font-medium">{current}</span>
      </div>
      <div className="mb-6 flex gap-1 border-b border-border">
        {tabs.map((t) => {
          const active = pathname.startsWith(t.href);
          return (
            <Link
              key={t.href}
              href={t.href}
              className={cn(
                "-mb-px border-b-2 px-3 pb-2.5 pt-1 text-sm transition-colors",
                active ? "border-fg text-fg" : "border-transparent text-secondary hover:text-fg"
              )}
            >
              {t.label}
            </Link>
          );
        })}
      </div>
      {children}
    </div>
  );
}
