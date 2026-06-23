import { clerkMiddleware, createRouteMatcher } from "@clerk/nextjs/server";
import { NextResponse } from "next/server";

// Public routes (no auth required): sign-in/up + the node API proxy + static.
const isPublic = createRouteMatcher([
  "/", // public Shadow landing (the dashboard renders here only when signed in)
  "/docs(.*)", // public developer documentation
  // Public marketing pages (landing nav).
  "/product(.*)",
  "/solutions(.*)",
  "/features(.*)",
  "/pricing(.*)",
  "/blog(.*)",
  "/case-studies(.*)",
  "/contact(.*)",
  "/privacy(.*)",
  "/offline.html", // PWA offline fallback (precached by the service worker)
  "/sw.js", // service worker
  "/sign-in(.*)",
  "/sign-up(.*)",
  "/cloud(.*)", // dashboard <-> node admin API proxy
  "/api(.*)",
  "/status(.*)", // public, user-facing incident/status board
]);

// Dev-only escape hatch: set HIVE_AUTH_BYPASS=1 to disable login gating entirely
// (used for headless screenshots / local previews). Never set in production.
const bypass = process.env.HIVE_AUTH_BYPASS === "1";

// Sensitive / highly-dynamic surfaces that must NEVER be client- or CDN-cached:
// personal settings, team settings + org management, project settings, project
// deployments, the network tab, and billing/admin. Stale views here are unsafe.
const NO_STORE = [
  /^\/account(\/|$)/,
  /^\/settings(\/|$)/,
  /^\/teams(\/|$)/,
  /^\/network(\/|$)/,
  /^\/deployments(\/|$)/,
  /^\/billing(\/|$)/,
  /^\/admin(\/|$)/,
  /^\/projects\/[^/]+\/settings(\/|$)/,
  /^\/projects\/[^/]+$/, // a project's overview = its deployments listing
];

// Public marketing / docs / status pages — shared (CDN) + browser cacheable.
const PUBLIC_PAGES = [
  /^\/$/,
  /^\/(product|solutions|features|pricing|blog|case-studies|contact|privacy|docs|status)(\/|$)/,
  /^\/sign-(in|up)(\/|$)/,
];

/**
 * Reasonable Cache-Control for a dashboard/marketing route to cut traffic and
 * data transfer. Returns `null` for paths we must not touch (the node API proxy
 * and app APIs, which serve live per-request data).
 */
function cacheControlFor(pathname: string): string | null {
  if (pathname.startsWith("/cloud") || pathname.startsWith("/api")) return null;
  if (NO_STORE.some((re) => re.test(pathname))) return "private, no-store, max-age=0, must-revalidate";
  if (PUBLIC_PAGES.some((re) => re.test(pathname)))
    return "public, max-age=300, s-maxage=3600, stale-while-revalidate=86400";
  // Other authenticated dashboard tabs: cache the page shell briefly in the
  // user's browser only (live data still streams from /cloud). `private` keeps
  // it off shared caches.
  return "private, max-age=60, stale-while-revalidate=300";
}

function withCache(req: { nextUrl: { pathname: string } }, res: Response): Response {
  const cc = cacheControlFor(req.nextUrl.pathname);
  if (cc) res.headers.set("Cache-Control", cc);
  return res;
}

const clerk = clerkMiddleware((auth, req) => {
  if (!isPublic(req)) {
    auth().protect();
  }
  return withCache(req, NextResponse.next());
});

// When bypassing, skip Clerk's middleware (and its dev-browser handshake) too,
// but still apply cache headers.
export default bypass ? (req: Request & { nextUrl: { pathname: string } }) => withCache(req, NextResponse.next()) : clerk;

export const config = {
  // Run on app routes. Skip Next internals + static FILES (extension at the end),
  // but still gate app routes that legitimately contain dots (e.g. a domain
  // detail page like /domains/example.com) — otherwise they'd bypass auth.
  matcher: [
    "/((?!_next)(?!.*\\.(?:ico|png|jpg|jpeg|gif|svg|webp|woff2?|ttf|css|js|map|txt|xml|webmanifest)$).*)",
    "/",
  ],
};
