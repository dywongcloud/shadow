import UsageView from "./usage-view";

// Incremental Static Regeneration (ISR) config for the Usage route.
//
// The page is split server-shell / client-view: every figure is fetched
// CLIENT-side after hydration (usePoll → /v1/functions, /v1/billing, /v1/metrics),
// so the server output is a purely user-AGNOSTIC shell — no tenant/user data is
// ever baked into cached HTML (a shared ISR entry is safe; access stays gated by
// the Clerk middleware `auth.protect()`). `revalidate` regenerates that shell
// hourly; `force-static` opts the segment out of dynamic rendering. Live data
// refresh is independent (client polling).
//
// NOTE (witnessed at build time): the ROOT layout sets `export const dynamic =
// "force-dynamic"` (deliberate — the auth-dependent chrome + the home
// landing↔dashboard flip must render per-request to avoid a signed-out flash),
// and that root directive currently SUPERSEDES this segment's config app-wide, so
// `/usage` still renders dynamically. This ISR config is kept correct + forward-
// compatible so the shell is prerendered the moment the app's root rendering model
// allows it; it is intentionally NOT achieved by removing the root force-dynamic,
// which would reintroduce the auth-chrome flash across every route for no data-
// caching benefit here (this page carries no server data to cache).
export const dynamic = "force-static";
export const revalidate = 3600;

export default function UsagePage() {
  return <UsageView />;
}
