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
import { VitalsBeacon } from "@/components/vitals-beacon";

// schema.org JSON-LD so AI search / LLMs (and rich results) can parse what shadw
// is — structured, machine-readable content the AI-search era favors. Kept
// accurate to the product; rendered once in the root so every page carries it.
const STRUCTURED_DATA = {
  "@context": "https://schema.org",
  "@graph": [
    {
      "@type": "Organization",
      "@id": "https://shadw.cloud/#org",
      name: "shadw",
      url: "https://shadw.cloud",
      description:
        "shadw is a peer-to-peer cloud for serverless functions, containers, edge routing and durable data over an Iroh QUIC mesh.",
    },
    {
      "@type": "WebSite",
      "@id": "https://shadw.cloud/#site",
      url: "https://shadw.cloud",
      name: "shadw",
      publisher: { "@id": "https://shadw.cloud/#org" },
    },
    {
      "@type": "SoftwareApplication",
      name: "shadw",
      applicationCategory: "DeveloperApplication",
      operatingSystem: "Web",
      offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
      description:
        "Deploy serverless functions, containers, and static sites to a global peer-to-peer edge with instant rollbacks, preview deploys, GitOps, and durable data.",
    },
  ],
};

// Sleek geometric tech typeface for the Shadow brand wordmark + landing.
const display = Space_Grotesk({ subsets: ["latin"], variable: "--font-display", display: "swap" });
// Electrolize — the marketing/landing surface's primary typeface (single 400 weight).
const electrolize = Electrolize({ subsets: ["latin"], weight: "400", variable: "--font-electrolize", display: "swap" });

const SITE_URL = (process.env.NEXT_PUBLIC_SITE_URL || "https://shadw.cloud").replace(/\/$/, "");
const DESCRIPTION =
  "shadw is a peer-to-peer cloud: seamlessly connect, collaborate, and conquer. Serverless functions, containers, edge & durable data over a P2P mesh (Iroh QUIC).";

export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title: {
    default: "shadw — Beyond the Edge are Shadows",
    // Public pages set their own titles; this templates them for consistent SEO.
    template: "%s · shadw",
  },
  description: DESCRIPTION,
  applicationName: "shadw",
  manifest: "/manifest.webmanifest",
  alternates: { canonical: "/" },
  // Social + AI-search preview cards (title/description inherit unless a page
  // overrides). OG image is provided by app/opengraph-image.
  openGraph: {
    type: "website",
    siteName: "shadw",
    url: SITE_URL,
    title: "shadw — Beyond the Edge are Shadows",
    description: DESCRIPTION,
  },
  twitter: { card: "summary_large_image", title: "shadw", description: DESCRIPTION },
  // Favicon + icons come from the file-based metadata in app/ (favicon.ico,
  // icon.png, apple-icon.png) so there's a single source of truth.
  appleWebApp: {
    capable: true,
    title: "shadw",
    statusBarStyle: "black-translucent",
  },
};

// Responsive viewport for ALL devices/screen sizes: scale to device width, allow
// user zoom (accessibility — never lock maximumScale), and extend under notches
// (viewport-fit=cover) so mobile safe-areas render correctly. themeColor stays
// theme-aware for the browser/installed-app UI.
export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  viewportFit: "cover",
  themeColor: [
    { media: "(prefers-color-scheme: dark)", color: "#000000" },
    { media: "(prefers-color-scheme: light)", color: "#ffffff" },
  ],
};

// NOTE: the root layout is intentionally NOT `force-dynamic`. All auth-dependent
// chrome (TopNav/Footer via <SignedIn>) and the home landing↔dashboard flip are
// CLIENT components that hydrate to the correct state, and NO page reads server
// auth (`auth()`/`cookies()`/`headers()`), so public pages can be prerendered as
// static/ISR (huge mobile win) and dashboard pages ship a fast static shell that
// fetches per-user data after hydration. Only the home route ('/') keeps
// `force-dynamic` (its whole-page content flip would otherwise flash).

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
        {/* eslint-disable-next-line react/no-danger */}
        <script type="application/ld+json" dangerouslySetInnerHTML={{ __html: JSON.stringify(STRUCTURED_DATA) }} />
        <PwaRegister />
        <VitalsBeacon />
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
