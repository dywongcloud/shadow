import { HomeClient } from "./home-client";

// The home route flips landing↔dashboard on auth. NOTE (verified against
// @clerk/nextjs v6): the ClerkProvider in the root layout does NOT pass the
// `dynamic` prop, so SSR gets `initialState: null` and `<SignedIn>/<SignedOut>`
// both render NOTHING server-side regardless of the request's cookies — the
// server shell's auth region is EMPTY for everyone, and the client's first
// (hydration) render is identically empty until clerk-js resolves. There is
// deliberately no SSR/CSR auth mismatch here; the one visible flip happens
// client-side when Clerk settles. `force-dynamic` is kept (as a server shell —
// route config from a "use client" page is ignored) so the apex document is
// always the current build, never a prerendered/ISR-cached shell; the proxy
// serves it `no-store` to match. Every OTHER page is static/ISR or a static
// shell (see the root layout note).
export const dynamic = "force-dynamic";

export default function Page() {
  return <HomeClient />;
}
