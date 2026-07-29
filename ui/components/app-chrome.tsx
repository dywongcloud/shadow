"use client";

import dynamic from "next/dynamic";
import { useSettledAuth } from "@/lib/auth-settle";
import { TopNav } from "@/components/topnav";
import { Footer } from "@/components/footer";
import { ZkPreviewAuth } from "@/components/zk-preview-auth";
import { PendingBuildsProvider } from "@/components/pending-builds-provider";
import { SessionToken } from "@/components/session-token";

// Both mount on EVERY authenticated page but render nothing until the user
// actively opens them (⌘K for the command bar; the onboarding/repo flow for
// GitOps) — so their JS has no business being in every page's initial bundle.
// ssr:false is safe: they're pure client overlays with zero SEO surface, and
// `loading: () => null` is correct since their own first render is invisible
// too (no layout shift from the swap).
const GitOps = dynamic(() => import("@/components/gitops").then((m) => m.GitOps), {
  ssr: false,
  loading: () => null,
});
const CommandBar = dynamic(() => import("@/components/command-bar").then((m) => m.CommandBar), {
  ssr: false,
  loading: () => null,
});

// Auth-gated dashboard chrome (top nav + footer + overlays). This MUST be a client
// component: when it lived in the server root layout, Clerk's `<SignedIn>` was
// evaluated once at SSR and frozen — so on client-side login/logout (soft nav) the
// TopNav didn't appear/disappear until a hard refresh. Bugs:
//   • after login → dashboard, the navbar was missing until reload
//   • after logout → landing, the navbar lingered above the landing's own navbar
// `app/page.tsx` (a client component) already flips Landing↔Dashboard reactively
// from the same signal; rendering the chrome from a client tree makes it react
// identically. In local (no-Clerk) mode the chrome always shows.
//
// GUARDED, not raw `<SignedIn>`: the chrome renders from `useSettledAuth()` —
// the SAME shared committed/debounced/flip-bounded auth view the home route's
// landing↔dashboard flip uses (see lib/auth-settle.ts) — so the whole page
// (chrome + body) always flips as ONE unit driven by ONE guarded signal, and a
// flapping Clerk auth state (the mobile/ITP handshake failure) can neither
// thrash the nav/footer nor storm-mount the pollers living down here
// (PendingBuildsProvider, SessionToken, GitOps). Committed transitions still
// happen (login/logout reactivity this file's history depends on — navbar
// appears after login, disappears after logout, without a hard refresh); they
// are just debounced and bounded instead of tracking every oscillation.
const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

export function ChromeTop() {
  if (!clerkEnabled) return <TopNav />;
  // Split subtree so `useSettledAuth` (needs ClerkProvider) never runs in
  // local/no-Clerk mode — the same split the old `<SignedIn>` gave for free.
  return <GuardedChromeTop />;
}

function GuardedChromeTop() {
  const view = useSettledAuth();
  if (view !== "in") return null;
  return <TopNav />;
}

export function ChromeBottom() {
  if (!clerkEnabled) {
    return (
      <>
        <Footer />
        <GitOps />
        <CommandBar />
        <PendingBuildsProvider />
      </>
    );
  }
  return <GuardedChromeBottom />;
}

function GuardedChromeBottom() {
  const view = useSettledAuth();
  // Everything here (incl. the SessionToken cookie-mint keeper and the
  // PendingBuildsProvider poller) mounts only once auth has SETTLED signed-in —
  // an oscillating signal can no longer mount/unmount these on every flap.
  if (view !== "in") return null;
  return (
    <>
      <Footer />
      <GitOps />
      <CommandBar />
      <ZkPreviewAuth />
      <PendingBuildsProvider />
      <SessionToken />
    </>
  );
}
