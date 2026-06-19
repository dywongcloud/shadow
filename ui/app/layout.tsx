import type { Metadata } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import { Space_Grotesk } from "next/font/google";
import { ClerkProvider, SignedIn } from "@clerk/nextjs";
import "./globals.css";
import { TopNav } from "@/components/topnav";
import { Footer } from "@/components/footer";
import { ThemeProvider } from "@/components/theme-provider";
import { GitOps } from "@/components/gitops";

// Sleek geometric tech typeface for the Shadow brand wordmark + landing.
const display = Space_Grotesk({ subsets: ["latin"], variable: "--font-display", display: "swap" });

export const metadata: Metadata = {
  title: "shadw — Beyond the Edge are Shadows",
  description:
    "shadw is a peer-to-peer cloud: seamlessly connect, collaborate, and conquer. Serverless functions, containers, edge & durable data over a P2P mesh (Iroh QUIC).",
};

// Clerk is enabled when a publishable key is present; otherwise the app runs in
// local mode (no auth) so it still works without keys.
const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

// Origins Clerk is allowed to redirect back to after sign-in / OAuth — i.e. the
// app's callback URLs. This lets login work both locally and through an ngrok
// tunnel at the same time. Override/extend via NEXT_PUBLIC_ALLOWED_ORIGINS
// (comma-separated); the defaults cover the common local ports + the ngrok host.
const allowedRedirectOrigins = (
  process.env.NEXT_PUBLIC_ALLOWED_ORIGINS ||
  "http://localhost:3000,http://localhost:3002,https://shadow.ngrok.pizza"
)
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);

export default function RootLayout({ children }: { children: React.ReactNode }) {
  const tree = (
    <html
      lang="en"
      suppressHydrationWarning
      className={`${GeistSans.variable} ${GeistMono.variable} ${display.variable}`}
    >
      <body className="flex min-h-screen flex-col bg-bg font-sans text-fg antialiased">
        <ThemeProvider>
          {/* Dashboard chrome (top nav + footer) is for signed-in users. The
              signed-out landing page renders its own nav/footer full-bleed. In
              local (no-Clerk) mode the chrome always shows. */}
          {clerkEnabled ? (
            <SignedIn>
              <TopNav />
            </SignedIn>
          ) : (
            <TopNav />
          )}
          <main className="mx-auto w-full max-w-[1400px] flex-1 px-4 py-8 sm:px-6">{children}</main>
          {clerkEnabled ? (
            <SignedIn>
              <Footer />
            </SignedIn>
          ) : (
            <Footer />
          )}
          {/* GitOps onboarding is for signed-in users only — never show it on the
              sign-in / sign-up pages. In local (no-Clerk) mode it always renders. */}
          {clerkEnabled ? (
            <SignedIn>
              <GitOps />
            </SignedIn>
          ) : (
            <GitOps />
          )}
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
