// Theme-aware appearance for Clerk widgets so the sign-in/up screens match the
// dashboard's light/dark mode. Built on Clerk's official `dark` baseTheme so
// every control (text, inputs, social buttons, OTP) is legible in both modes.
import { dark as darkTheme } from "@clerk/themes";

export function clerkAppearance(dark: boolean) {
  return {
    baseTheme: dark ? darkTheme : undefined,
    variables: {
      colorPrimary: dark ? "#ffffff" : "#000000",
      colorBackground: dark ? "#0a0a0a" : "#ffffff",
      borderRadius: "0.625rem",
      fontFamily: "var(--font-geist-sans), ui-sans-serif, system-ui, sans-serif",
    },
    elements: {
      rootBox: "w-full",
      cardBox: "shadow-none border border-[hsl(var(--border))] rounded-2xl",
      card: "shadow-none bg-[hsl(var(--card))]",
      headerTitle: "text-[hsl(var(--foreground))]",
      headerSubtitle: "text-[hsl(var(--secondary))]",
      formButtonPrimary:
        dark ? "bg-white text-black hover:bg-white/90 normal-case" : "bg-black text-white hover:bg-black/90 normal-case",
      footerActionLink: "text-[hsl(var(--link))]",
    },
  } as const;
}
