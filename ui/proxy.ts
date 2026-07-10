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
    const user = await (await clerkClient()).users.getUser(userId);
    const owner = user.emailAddresses.some((e) => ADMIN_EMAILS.has(e.emailAddress.toLowerCase()));
    ownerCache.set(userId, { at: Date.now(), owner });
    return owner;
  } catch {
    return false; // fail closed — a Clerk hiccup must not open the ops console
  }
}

const clerk = clerkMiddleware(async (auth, req) => {
  if (isAdminRoute(req)) {
    // Must be signed in AND on the owner allow-list. Deliberately does NOT
    // call bare `auth().protect()` here (see the self-connect-storm note
    // below) — the unauthenticated case is handled directly by the userId
    // check right below, which already produces the right response shape
    // per surface (JSON for /ops, redirect for /admin), so protect()'s own
    // redirect/not-found branching would only be redundant — and buggy.
    const { userId } = await auth();
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
    // Explicit `unauthenticatedUrl` (self-hosted `next start` infinite
    // self-connect storm, part 2): a bare `auth().protect()` picks between
    // `redirectToSignIn()` (a real Location-header redirect — safe) and
    // `notFound()` based on Clerk's own `isPageRequest()` heuristic (Accept:
    // text/html / Sec-Fetch-Dest: document). Any request that heuristic
    // misses — curl, most bare `fetch()` calls, health checks, some RSC/
    // prefetch requests — falls into `notFound()`, which Clerk implements as
    // an explicit `NextResponse.rewrite()` to a synthetic same-origin
    // `/clerk_<timestamp>` path. That rewrite hits the exact same Next.js
    // 14.2.35 self-hosted bug as the decorateRequest self-rewrite below (see
    // `neutralizeClerkSelfRewrite`): the server re-dispatches it over the
    // network instead of in-process, the synthetic path matches this same
    // middleware matcher, protect() denies it again with a NEW timestamp,
    // forever — confirmed live via strace as an unbounded self-connect storm
    // that never returns a byte. Passing `unauthenticatedUrl` skips
    // `isPageRequest()` entirely and always takes the safe `redirect()`
    // path, for every request shape.
    await auth.protect({ unauthenticatedUrl: new URL(`/sign-in?redirect_url=${encodeURIComponent(req.url)}`, req.url).toString() });
  }
  return withCache(req, NextResponse.next());
});

// WORKAROUND (self-hosted `next start` infinite self-connect storm): every
// clerkMiddleware() invocation that ends in NextResponse.next() gets rewritten
// by @clerk/nextjs's internal decorateRequest() into an explicit
// `NextResponse.rewrite()` targeting the SAME absolute request URL — purely a
// vehicle to smuggle auth-state (`x-clerk-auth-*`) onto the request via the
// `x-middleware-override-headers` / `x-middleware-request-*` header protocol,
// since @clerk/nextjs 5.7.6 (the final 5.x release; no later 5.x patch
// exists) predates Clerk switching to the native `NextResponse.next({
// request: { headers } })` form for this. On Vercel's edge network that
// same-URL rewrite is a no-op signal handled in-process. Under plain
// `next start` on this Next.js 14.2.35 build (also the final 14.2.x release),
// an explicit rewrite whose target equals the incoming request's own URL
// makes the server re-dispatch the request over the network instead of
// continuing in-process — and the re-dispatched request gets decorated
// identically by middleware, forever: confirmed live via strace as an
// unbounded storm of the process connect()-ing to its own listening port,
// every request carrying the same x-clerk-auth-*/cache-control headers,
// never returning a byte to the original caller.
//
// Fix: downgrade that self-rewrite back into a plain "continue" signal
// (`x-middleware-next: 1`) whenever its target is exactly the request's own
// URL, while leaving `x-middleware-override-headers` / `x-middleware-request-*`
// untouched — those are the actual header-carrier Next.js honors on a plain
// continue too (this is exactly what `NextResponse.next({ request: {
// headers } })` produces natively, which was verified NOT to loop). auth()/
// currentUser() in Server Components still see Clerk's injected headers;
// Next.js just no longer re-enters the network to deliver them.
function neutralizeClerkSelfRewrite(req: Request, res: Response): Response {
  const rewrite = res.headers.get("x-middleware-rewrite");
  if (rewrite && rewrite === req.url) {
    res.headers.delete("x-middleware-rewrite");
    res.headers.set("x-middleware-next", "1");
  }
  return res;
}

// When bypassing, skip Clerk's middleware (and its dev-browser handshake) too,
// but still apply cache headers. Next 16 renamed the `middleware` convention to
// `proxy` (Node runtime only) — this file was `middleware.ts`; the named `proxy`
// export is the new contract. The `neutralizeClerkSelfRewrite` workaround is now
// a harmless no-op under @clerk/nextjs v7 (which emits the native
// `NextResponse.next({ request: { headers } })` form, not the same-URL rewrite),
// kept as a belt-and-braces guard.
export const proxy = bypass
  ? (req: Request & { nextUrl: { pathname: string } }) => withCache(req, NextResponse.next())
  : async (req: Parameters<typeof clerk>[0], event: Parameters<typeof clerk>[1]) => {
      const res = await clerk(req, event);
      return neutralizeClerkSelfRewrite(req, res ?? NextResponse.next());
    };

export const config = {
  // Run on app routes. Skip Next internals + static FILES (extension at the end),
  // but still gate app routes that legitimately contain dots (e.g. a domain
  // detail page like /domains/example.com) — otherwise they'd bypass auth.
  matcher: [
    "/((?!_next)(?!.*\\.(?:ico|png|jpg|jpeg|gif|svg|webp|woff2?|ttf|css|js|map|txt|xml|webmanifest)$).*)",
    "/",
  ],
};
