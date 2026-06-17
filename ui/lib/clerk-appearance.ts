// Theme-aware appearance for Clerk widgets so the sign-in/up screens match the
// dashboard's light/dark mode (instead of always rendering white).

export function clerkAppearance(dark: boolean) {
  return {
    variables: {
      colorPrimary: dark ? "#ffffff" : "#000000",
      colorText: dark ? "#ededed" : "#0a0a0a",
      colorTextSecondary: dark ? "#a1a1a1" : "#666666",
      colorBackground: dark ? "#0a0a0a" : "#ffffff",
      colorInputBackground: dark ? "#111111" : "#ffffff",
      colorInputText: dark ? "#ededed" : "#0a0a0a",
      colorNeutral: dark ? "#ffffff" : "#000000",
      borderRadius: "0.5rem",
    },
    elements: {
      rootBox: "w-full",
      card: "shadow-none border border-[hsl(var(--border))] bg-[hsl(var(--card))]",
      headerTitle: "text-[hsl(var(--foreground))]",
      headerSubtitle: "text-[hsl(var(--secondary))]",
      socialButtonsBlockButton: "border-[hsl(var(--border))]",
      formButtonPrimary:
        dark ? "bg-white text-black hover:bg-white/90" : "bg-black text-white hover:bg-black/90",
      footerActionLink: "text-[hsl(var(--link))]",
    },
  } as const;
}
