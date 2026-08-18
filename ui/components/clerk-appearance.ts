"use client";

import { dark } from "@clerk/themes";
import { useTheme } from "next-themes";

/** Shared Clerk appearance that follows the app's light/dark green theme.
 *  Using the official `dark` baseTheme fixes black-on-black text and hidden controls
 *  (e.g. "Create organization") in dark mode. Green lightning theme for autheo.dev DevHub. */
export function useClerkAppearance() {
  const { resolvedTheme } = useTheme();
  const isDark = resolvedTheme === "dark";
  return {
    baseTheme: isDark ? dark : undefined,
    variables: {
      colorPrimary: isDark ? "#6DF55E" : "#22C55E",       // Vibrant green - lightning
      colorBackground: isDark ? "#0a0e0a" : "#f7fef4",   // Green-tinted backgrounds
      borderRadius: "0.625rem",
      fontFamily: "var(--font-geist-sans), ui-sans-serif, system-ui, sans-serif",
    },
    elements: {
      rootBox: "w-full",
      card: "shadow-none bg-transparent",
      // Keep Clerk's switcher trigger consistent with our nav controls.
      organizationSwitcherTrigger: "rounded-md",
    },
  } as const;
}
