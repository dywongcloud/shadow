"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { Logo } from "@/components/logo";

export function Footer() {
  const pathname = usePathname();
  if (pathname.startsWith("/admin") || pathname.startsWith("/sign-in") || pathname.startsWith("/sign-up") || pathname.startsWith("/status") || pathname.startsWith("/docs")) return null;

  return (
    <footer className="mt-16 border-t border-border bg-card">
      <div className="mx-auto flex max-w-[1400px] flex-col items-center justify-between gap-3 px-4 py-6 text-xs text-muted sm:flex-row sm:px-6">
        <Link href="/" className="flex shrink-0 items-center">
          <Logo className="h-4 w-auto max-w-none sm:h-5" />
        </Link>
        <div className="flex min-w-0 items-center gap-4">
          <span>© {new Date().getFullYear()} shadw</span>
          <Link href="/status" className="hover:text-fg">Status</Link>
          <Link href="/privacy" className="hover:text-fg">Privacy</Link>
        </div>
      </div>
    </footer>
  );
}
