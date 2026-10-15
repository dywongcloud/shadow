import { Suspense } from "react";
import type { Metadata, Viewport } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import { Space_Grotesk, Electrolize } from "next/font/google";
import { ClerkProvider } from "@clerk/nextjs";
import { clerkFrontendOrigin } from "@/lib/clerk-origin.mjs";
import "./globals.css";
import { ChromeTop, ChromeBottom } from "@/components/app-chrome";
import { ThemeProvider } from "@/components/theme-provider";
import { PwaRegister } from "@/components/pwa-register";
import { Toaster } from "@/components/toast";
import { VitalsBeacon } from "@/components/vitals-beacon";

export const instant = false;

const STRUCTURED_DATA = {
  "@context": "https://schema.org",
  "@graph": [
    {
      "@type": "Organization",
      "@id": "https://shadw.cloud/#org",
      name: "shadw",
      url: "https://shadw.cloud",
      description:
        "shadw is a cloud for serverless functions, containers and static sites deployed from Git, with managed databases.",
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
      offers: { "@type": "AggregateOffer", lowPrice: "0", priceCurrency: "USD" },
      description:
        "Deploy serverless functions, containers, and static sites from Git, with rollback to earlier deployments, a preview for every pull request on a connected GitHub repository, GitOps, and managed databases.",
    },
  ],
};

const display = Space_Grotesk({ subsets: ["latin"], variable: "--font-display", display: "swap" });
const electrolize = Electrolize({ subsets: ["latin"], weight: "400", variable: "--font-electrolize", display: "swap" });

const SITE_URL = (process.env.NEXT_PUBLIC_SITE_URL || "https://shadw.cloud").replace(/\/$/, "");
const DESCRIPTION =
  "Push to Git, get a live URL: serverless functions, containers and static sites, plus managed databases, with a preview per GitHub pull request. Free to start.";

export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title: {
    default: "shadw — Push to Git. Get a live URL.",
    template: "%s · shadw",
  },
  description: DESCRIPTION,
  applicationName: "shadw",
  manifest: "/manifest.webmanifest",
  alternates: { canonical: "/" },
  openGraph: {
    type: "website",
    siteName: "shadw",
    url: SITE_URL,
    title: "shadw — Push to Git. Get a live URL.",
    description: DESCRIPTION,
  },
  twitter: { card: "summary_large_image", title: "shadw — Push to Git. Get a live URL.", description: DESCRIPTION },
  appleWebApp: {
    capable: true,
    title: "shadw",
    statusBarStyle: "black-translucent",
  },
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  viewportFit: "cover",
  themeColor: [
    { media: "(prefers-color-scheme: dark)", color: "#000000" },
    { media: "(prefers-color-scheme: light)", color: "#ffffff" },
  ],
};

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;
const clerkOrigin = clerkFrontendOrigin(process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY);

const allowedRedirectOrigins = (
  process.env.NEXT_PUBLIC_ALLOWED_ORIGINS ||
  "http://localhost:3000,http://localhost:3002,https://shadw.cloud,https://www.shadw.cloud"
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
      <head>
        {clerkEnabled && clerkOrigin ? (
          <link rel="preconnect" href={clerkOrigin} crossOrigin="anonymous" />
        ) : null}
      </head>
      <body className="flex min-h-screen flex-col bg-bg font-sans text-fg antialiased">
        <script type="application/ld+json" dangerouslySetInnerHTML={{ __html: JSON.stringify(STRUCTURED_DATA) }} />
        <PwaRegister />
        <VitalsBeacon />
        <Toaster />
        <ThemeProvider>
          <Suspense fallback={null}>
            <ChromeTop />
          </Suspense>
          <main className="mx-auto w-full max-w-[1400px] flex-1 px-4 py-8 sm:px-6">{children}</main>
          <Suspense fallback={null}>
            <ChromeBottom />
          </Suspense>
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
