import type { Metadata, Viewport } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import { Space_Grotesk, Electrolize } from "next/font/google";
import { ClerkProvider } from "@clerk/nextjs";
import "./globals.css";
import { ChromeTop, ChromeBottom } from "@/components/app-chrome";
import { ThemeProvider } from "@/components/theme-provider";
import { PwaRegister } from "@/components/pwa-register";
import { Toaster } from "@/components/toast";

// Sleek geometric tech typeface for the Shadow brand wordmark + landing.
const display = Space_Grotesk({ subsets: ["latin"], variable: "--font-display", display: "swap" });
// Electrolize — the marketing/landing surface's primary typeface (single 400 weight).
const electrolize = Electrolize({ subsets: ["latin"], weight: "400", variable: "--font-electrolize", display: "swap" });

export const metadata: Metadata = {
  title: "shadw — Beyond the Edge are Shadows",
  description:
    "shadw is a peer-to-peer cloud: seamlessly connect, collaborate, and conquer. Serverless functions, containers, edge & durable data over a P2P mesh (Iroh QUIC).",
  applicationName: "shadw",
  manifest: "/manifest.webmanifest",
  // Favicon + icons come from the file-based metadata in app/ (favicon.ico,
  // icon.png, apple-icon.png) so there's a single source of truth.
  appleWebApp: {
    capable: true,
    title: "shadw",
    statusBarStyle: "black-translucent",
  },
};

// Browser UI / installed-app theme color, theme-aware.
export const viewport: Viewport = {
  themeColor: [
    { media: "(prefers-color-scheme: dark)", color: "#000000" },
    { media: "(prefers-color-scheme: light)", color: "#ffffff" },
  ],
};

// Render per-request (never statically). The layout's chrome is auth-dependent
// (`<SignedIn>` TopNav/Footer) and the home route flips Landing↔Dashboard on auth,
// so static rendering would SSR a signed-OUT page for everyone — producing the
// landing→dashboard flash, a hydration mismatch, and a missing TopNav until the
// client re-rendered. Forcing dynamic makes Clerk's SSR use the real request auth,
// so the server renders the correct, already-signed-in chrome + content.
export const dynamic = "force-dynamic";

// Clerk is enabled when a publishable key is present; otherwise the app runs in
// local mode (no auth) so it still works without keys.
const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

// Origins Clerk is allowed to redirect back to after sign-in / OAuth — i.e. the
// app's callback URLs. This lets login work both locally and on the public
// platform domain. Override/extend via NEXT_PUBLIC_ALLOWED_ORIGINS
// (comma-separated); the defaults cover the common local ports + shadw.cloud.
const allowedRedirectOrigins = (
  process.env.NEXT_PUBLIC_ALLOWED_ORIGINS ||
  "http://localhost:3000,http://localhost:3002,https://shadw.cloud"
)
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);

export default function RootLayout({ children }: { children: React.ReactNode }) {
  const tree = (
    <html
      lang="en"
      suppressHydrationWarning
      className={`${GeistSans.variable} ${GeistMono.variable} ${display.variable} ${electrolize.variable}`}
    >
      <body className="flex min-h-screen flex-col bg-bg font-sans text-fg antialiased">
        <PwaRegister />
        <Toaster />
        <ThemeProvider>
          {/* Dashboard chrome (top nav + footer + overlays) is auth-gated in a
              CLIENT component so it reacts to client-side login/logout — the
              signed-out landing renders its own full-bleed nav/footer. See
              `app-chrome.tsx` for why this must not live in the server layout. */}
          <ChromeTop />
          <main className="mx-auto w-full max-w-[1400px] flex-1 px-4 py-8 sm:px-6">{children}</main>
          <ChromeBottom />
        </ThemeProvider>
      </body>
    </html>
  );
  return clerkEnabled ? (
    <ClerkProvider allowedRedirectOrigins={allowedRedirectOrigins}>{tree}</ClerkProvider>
  ) : (
    tree
  );
}
