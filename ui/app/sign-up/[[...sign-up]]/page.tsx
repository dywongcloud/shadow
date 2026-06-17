"use client";

import { SignUp } from "@clerk/nextjs";
import { useEffect, useState } from "react";
import { useTheme } from "next-themes";
import { ThemeToggle } from "@/components/theme-toggle";
import { clerkAppearance } from "@/lib/clerk-appearance";

export default function Page() {
  const { resolvedTheme } = useTheme();
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  const dark = mounted && resolvedTheme === "dark";

  return (
    <div className="fixed inset-0 z-50 flex flex-col items-center justify-center bg-bg px-4">
      <div className="absolute right-4 top-4">
        <ThemeToggle />
      </div>
      <div className="mb-8 flex items-center gap-2">
        <svg height="22" viewBox="0 0 76 65" fill="none" className="text-fg" aria-label="Hive">
          <path d="M37.59.25l36.95 64H.64l36.95-64z" fill="currentColor" />
        </svg>
        <span className="text-lg font-semibold">Hive Cloud</span>
      </div>
      {mounted && <SignUp appearance={clerkAppearance(dark)} />}
    </div>
  );
}
