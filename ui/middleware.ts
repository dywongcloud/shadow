import { clerkClient, clerkMiddleware, createRouteMatcher } from "@clerk/nextjs/server";
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
  "/cloud(.*)", // dashboard <-> platform API proxy (api.shadw.cloud)
  // NOTE: "/ops(.*)" (the ops-console API proxy) is deliberately NOT public —
  // see isAdminRoute below. It fronts the same privileged admin backend the
  // /admin pages use, so it gets the identical owner-allow-list gate; being
  // listed here used to skip Clerk auth entirely for anyone calling it directly
  // rather than through the /admin UI.
  "/api(.*)",
  "/status(.*)", // public, user-facing incident/status board
]);

// Dev-only escape hatch: set HIVE_AUTH_BYPASS=1 to disable login gating entirely
// (used for headless screenshots / local previews). Never set in production —
// and now enforced, not just a comment: a stray HIVE_AUTH_BYPASS=1 accidentally
// carried into a production environment (e.g. copy-pasted from a staging
// .env) can no longer disable auth for every account, since NODE_ENV is set by
// the Next.js production build/start itself, not by an easily-copied env file.
const bypass = process.env.HIVE_AUTH_BYPASS === "1" && process.env.NODE_ENV !== "production";

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
  if (pathname.startsWith("/cloud") || pathname.startsWith("/ops") || pathname.startsWith("/api")) return null;
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

// Operations Console (/admin) AND its API proxy (/ops, -> admin.shadw.cloud) —
// platform-owner only. Restricted to this fixed allow-list of emails; any
// other signed-in user is bounced to the dashboard. Both must share this gate:
// /admin is only the UI, /ops is the actual privileged backend it talks to —
// gating one without the other left the real admin API reachable by anyone
// who called /ops/* directly instead of clicking through the /admin page.
const isAdminRoute = createRouteMatcher(["/admin(.*)", "/ops(.*)"]);
const ADMIN_EMAILS = new Set(["dylanwong007@gmail.com", "dylan@shadw.com"]);

// Every /admin navigation otherwise costs a Clerk API round trip — this result
// changes only if the allow-list or the user's email changes, neither of which
// happens mid-session, so a short in-memory TTL avoids refetching it on every
// click into the ops console. Module-scope state (best-effort: resets on a cold
// middleware instance, which just falls back to the real check — never fails
// open on auth, only on speed).
const ownerCache = new Map<string, { at: number; owner: boolean }>();
const OWNER_TTL_MS = 5 * 60_000;

/** Whether the signed-in user owns one of the allow-listed admin emails. */
async function isPlatformOwner(userId: string): Promise<boolean> {
  const hit = ownerCache.get(userId);
  if (hit && Date.now() - hit.at < OWNER_TTL_MS) return hit.owner;
  try {
    const user = await clerkClient().users.getUser(userId);
    const owner = user.emailAddresses.some((e) => ADMIN_EMAILS.has(e.emailAddress.toLowerCase()));
    ownerCache.set(userId, { at: Date.now(), owner });
    return owner;
  } catch {
    return false; // fail closed — a Clerk hiccup must not open the ops console
  }
}

const clerk = clerkMiddleware(async (auth, req) => {
  if (isAdminRoute(req)) {
    // Must be signed in AND on the owner allow-list.
    auth().protect();
    const { userId } = auth();
    if (!userId || !(await isPlatformOwner(userId))) {
      // /ops/* is an API proxy (called by fetch, not page navigation) — an
      // HTML redirect there would surface as a broken/unparsable response to
      // its caller. /admin/* is a page, so redirecting to the dashboard home
      // is the right UX. Same auth gate, response shape fits the surface.
      if (req.nextUrl.pathname.startsWith("/ops")) {
        return withCache(req, NextResponse.json({ error: "forbidden" }, { status: 403 }));
      }
      return withCache(req, NextResponse.redirect(new URL("/", req.url)));
    }
  } else if (!isPublic(req)) {
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
