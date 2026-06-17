import type { Metadata } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import { ClerkProvider } from "@clerk/nextjs";
import "./globals.css";
import { TopNav } from "@/components/topnav";
import { ThemeProvider } from "@/components/theme-provider";

export const metadata: Metadata = {
  title: "Hive Cloud",
  description: "A unified, self-hosted cloud — builds, Fluid compute, edge, regions.",
};

// Clerk is enabled when a publishable key is present; otherwise the app runs in
// local mode (no auth) so it still works without keys.
const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

export default function RootLayout({ children }: { children: React.ReactNode }) {
  const tree = (
    <html
      lang="en"
      suppressHydrationWarning
      className={`${GeistSans.variable} ${GeistMono.variable}`}
    >
      <body className="min-h-screen bg-bg font-sans text-fg antialiased">
        <ThemeProvider>
          <TopNav />
          <main className="mx-auto max-w-[1400px] px-4 py-8 sm:px-6">{children}</main>
        </ThemeProvider>
      </body>
    </html>
  );
  return clerkEnabled ? <ClerkProvider>{tree}</ClerkProvider> : tree;
}
