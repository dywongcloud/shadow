"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";

const COLUMNS: { title: string; links: { label: string; href: string }[] }[] = [
  {
    title: "Platform",
    links: [
      { label: "Overview", href: "/" },
      { label: "Projects", href: "/projects" },
      { label: "Storage", href: "/storage" },
      { label: "Observability", href: "/observability" },
      { label: "CDN", href: "/cdn" },
    ],
  },
  {
    title: "Resources",
    links: [
      { label: "Network", href: "/network" },
      { label: "Firewall", href: "/firewall" },
      { label: "Integrations", href: "/integrations" },
      { label: "Usage", href: "/usage" },
    ],
  },
  {
    title: "Team",
    links: [
      { label: "Teams", href: "/teams" },
      { label: "Settings", href: "/settings" },
      { label: "Operations", href: "/admin" },
    ],
  },
];

function VercelMark() {
  return (
    <svg height="18" viewBox="0 0 76 65" fill="none" className="text-fg" aria-label="Vercel">
      <path d="M37.59.25l36.95 64H.64l36.95-64z" fill="currentColor" />
    </svg>
  );
}

export function Footer() {
  const pathname = usePathname();
  // The ops console + auth + public status pages render their own chrome.
  if (pathname.startsWith("/admin") || pathname.startsWith("/sign-in") || pathname.startsWith("/sign-up") || pathname.startsWith("/status")) return null;

  return (
    <footer className="mt-16 border-t border-border bg-subtle/40">
      <div className="mx-auto max-w-[1400px] px-4 py-12 sm:px-6">
        <div className="grid grid-cols-2 gap-8 sm:grid-cols-4">
          <div className="col-span-2 flex flex-col gap-3 sm:col-span-1">
            <Link href="/" className="flex items-center gap-2">
              <VercelMark />
              <span className="text-sm font-semibold">OpenEdge</span>
            </Link>
            <p className="text-xs text-secondary">
              A unified, self-hosted cloud — builds, Fluid compute, edge & data, meshed over iroh P2P.
            </p>
            <div className="mt-1 inline-flex items-center gap-1.5 text-xs text-secondary">
              <span className="relative flex h-2 w-2">
                <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-green opacity-60" />
                <span className="relative inline-flex h-2 w-2 rounded-full bg-green" />
              </span>
              All systems normal
            </div>
          </div>
          {COLUMNS.map((col) => (
            <div key={col.title}>
              <div className="mb-3 text-xs font-semibold uppercase tracking-wide text-muted">{col.title}</div>
              <ul className="flex flex-col gap-2">
                {col.links.map((l) => (
                  <li key={l.label}>
                    <Link href={l.href} className="text-sm text-secondary transition-colors hover:text-fg">
                      {l.label}
                    </Link>
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>
        <div className="mt-10 flex flex-col items-center justify-between gap-3 border-t border-border pt-6 text-xs text-muted sm:flex-row">
          <span>© {new Date().getFullYear()} OpenEdge. Built on a Vercel-style platform.</span>
          <div className="flex items-center gap-4">
            <Link href="/network" className="hover:text-fg">Status</Link>
            <Link href="/settings" className="hover:text-fg">Privacy</Link>
            <Link href="/settings" className="hover:text-fg">Terms</Link>
          </div>
        </div>
      </div>
    </footer>
  );
}
