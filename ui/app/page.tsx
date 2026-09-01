import { HomeClient } from "./home-client";

// The home route flips landing↔dashboard on auth. NOTE (verified against
// @clerk/nextjs v6): the ClerkProvider in the root layout does NOT pass the
// `dynamic` prop, so SSR gets `initialState: null` and `<SignedIn>/<SignedOut>`
// both render NOTHING server-side regardless of the request's cookies — the
// server shell's auth region is EMPTY for everyone, and the client's first
// (hydration) render is identically empty until clerk-js resolves. There is
// deliberately no SSR/CSR auth mismatch here; the one visible flip happens
// client-side when Clerk settles. Every OTHER page is static/ISR or a static
// shell (see the root layout note).
//
// Genuinely static shell under Cache Components: HomeClient is entirely
// "use client" with ZERO server-side data access (no cookies()/headers(), no
// per-request fetch) — the prerendered HTML is identical for every visitor
// and every deploy, so there's no "serving a stale build as current" risk to
// guard against. The whole landing↔dashboard flip already happens client-side
// post-hydration regardless of whether this shell is cached.

export default function Page() {
  return <HomeClient />;
}
