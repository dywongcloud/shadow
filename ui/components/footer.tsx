"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { Logo } from "@/components/logo";

export function Footer() {
  const pathname = usePathname();
  // The ops console + auth + public status pages render their own chrome.
  if (pathname.startsWith("/admin") || pathname.startsWith("/sign-in") || pathname.startsWith("/sign-up") || pathname.startsWith("/status") || pathname.startsWith("/docs")) return null;

  return (
    <footer className="mt-16 border-t border-border bg-card">
      <div className="mx-auto flex max-w-[1400px] flex-col items-center justify-between gap-3 px-4 py-6 text-xs text-muted sm:flex-row sm:px-6">
        <div className="flex items-center gap-3">
          <Link href="/" className="flex items-center">
            <Logo className="h-5 w-auto" />
          </Link>
          <span className="inline-flex items-center gap-1.5 text-secondary">
            <span className="relative flex h-2 w-2">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-green opacity-60" />
              <span className="relative inline-flex h-2 w-2 rounded-full bg-green" />
            </span>
            All systems normal
          </span>
        </div>
        <div className="flex items-center gap-4">
          <span>© {new Date().getFullYear()} Autheo DevHub</span>
          <Link href="/network" className="hover:text-fg">Status</Link>
          <Link href="/settings" className="hover:text-fg">Privacy</Link>
          <Link href="/settings" className="hover:text-fg">Terms</Link>
        </div>
      </div>
    </footer>
  );
}
