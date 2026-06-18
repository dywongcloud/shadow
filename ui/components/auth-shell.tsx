"use client";

import { useEffect, useState } from "react";
import { useTheme } from "next-themes";
import { ThemeToggle } from "@/components/theme-toggle";
import { clerkAppearance } from "@/lib/clerk-appearance";

const FEATURES = [
  "Global edge network across every region",
  "Fluid compute that scales to zero",
  "Built-in WAF, CDN & DDoS protection",
  "Multi-tenant teams with strict isolation",
];

/** Polished split-screen auth layout shared by sign-in / sign-up. Left: brand
 *  + value props on a subtle gradient. Right: the Clerk widget. Looks good in
 *  both light and dark. */
export function AuthShell({
  render,
  footer,
}: {
  render: (appearance: ReturnType<typeof clerkAppearance>) => React.ReactNode;
  footer: React.ReactNode;
}) {
  const { resolvedTheme } = useTheme();
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  const dark = mounted && resolvedTheme === "dark";

  return (
    <div className="fixed inset-0 z-50 grid grid-cols-1 bg-bg lg:grid-cols-2">
      {/* Brand / marketing panel */}
      <div className="relative hidden flex-col justify-between overflow-hidden border-r border-border p-12 lg:flex">
        <div
          className="pointer-events-none absolute inset-0 opacity-70"
          style={{
            background:
              "radial-gradient(60% 50% at 30% 20%, hsl(var(--foreground) / 0.06), transparent 70%), radial-gradient(50% 40% at 80% 90%, hsl(var(--foreground) / 0.05), transparent 70%)",
          }}
        />
        <div className="relative flex items-center gap-2">
          <svg height="24" viewBox="0 0 76 65" fill="none" className="text-fg" aria-label="OpenEdge">
            <path d="M37.59.25l36.95 64H.64l36.95-64z" fill="currentColor" />
          </svg>
          <span className="text-lg font-semibold tracking-tight">OpenEdge</span>
        </div>
        <div className="relative">
          <h1 className="max-w-md text-3xl font-semibold leading-tight tracking-tight">
            Your unified, self-hosted cloud.
          </h1>
          <p className="mt-3 max-w-sm text-sm text-secondary">
            Builds, Fluid compute, edge and regions — meshed across your own machines.
          </p>
          <ul className="mt-8 flex flex-col gap-3">
            {FEATURES.map((f) => (
              <li key={f} className="flex items-center gap-2.5 text-sm text-secondary">
                <span className="flex h-5 w-5 items-center justify-center rounded-full bg-fg text-bg">
                  <svg width="11" height="11" viewBox="0 0 24 24" fill="none"><path d="M5 13l4 4L19 7" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" /></svg>
                </span>
                {f}
              </li>
            ))}
          </ul>
        </div>
        <div className="relative text-xs text-muted">© {2026} OpenEdge · Built on a Vercel-style platform.</div>
      </div>

      {/* Auth panel */}
      <div className="relative flex flex-col items-center justify-center px-4 py-10">
        <div className="absolute right-4 top-4"><ThemeToggle /></div>
        <div className="mb-6 flex items-center gap-2 lg:hidden">
          <svg height="22" viewBox="0 0 76 65" fill="none" className="text-fg" aria-label="OpenEdge">
            <path d="M37.59.25l36.95 64H.64l36.95-64z" fill="currentColor" />
          </svg>
          <span className="text-lg font-semibold">OpenEdge</span>
        </div>
        <div className="w-full max-w-[400px]">
          {mounted && render(clerkAppearance(dark))}
        </div>
        <div className="mt-6 text-center text-xs text-muted">{footer}</div>
      </div>
    </div>
  );
}
