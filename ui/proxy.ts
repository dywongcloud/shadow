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
  // First-party GitHub App OAuth callback — GitHub redirects the browser here.
  // Must not require a Clerk session (the flow is CSRF-protected by its own
  // HMAC-signed state + nonce cookie); the token lands in an httpOnly cookie
  // bound to this browser either way.
  "/oauth/github/callback",
  "/cloud(.*)", // dashboard <-> platform API proxy (api.shadw.cloud)
  // NOTE: "/ops(.*)" (the ops-console API proxy) is deliberately NOT public —
  // see isAdminRoute below. It fronts the same privileged admin backend the
  // /admin pages use, so it gets the identical owner-allow-list gate; being
  // listed here used to skip Clerk auth entirely for anyone calling it directly
  // rather than through the /admin UI.
  "/api(.*)",
  "/status(.*)", // public, user-facing incident/status board
  // The embedded @workflow/web console mount (iframe inside /workflows).
  // Its extensionless API surfaces (/wfc/api/rpc CBOR POSTs, /wfc/*.data
  // loader fetches) MUST NOT be Clerk-redirected: a 307 → /sign-in on an
  // expired session hands the sign-in HTML to the console's cbor-x decoder,
  // which surfaced to the user as "Error loading runs — Unknown token 28".
  // Data-plane auth is enforced by the hive_jwt cookie → backend JWT check
  // (hive-world.mjs forwards it as a Bearer); with no valid session the
  // backend 401s and the console renders a readable error + auto re-mints.
  // The user-facing /workflows page (which embeds this) stays Clerk-gated.
  "/wfc(.*)",
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
  // The home route: page.tsx keeps it `force-dynamic` and its body flips
  // landing↔dashboard on CLIENT auth state. It was previously in PUBLIC_PAGES
  // (`public, s-maxage=3600, stale-while-revalidate=86400`) — which let any
  // shared cache hold a per-request-rendered document for an hour and let the
  // browser serve it stale for a DAY with no `Vary: Cookie`, so post-redeploy
  // (or mid sign-in/sign-out) mobile browsers could keep re-serving a stale
  // shell referencing dead content-hashed chunks — the same class the
  // /workflows entry below documents. The service worker's "network-first"
  // navigation fetch honors this HTTP cache too, so `public` here defeated it.
  /^\/$/,
  /^\/account(\/|$)/,
  /^\/settings(\/|$)/,
  /^\/teams(\/|$)/,
  /^\/network(\/|$)/,
  /^\/deployments(\/|$)/,
  /^\/billing(\/|$)/,
  /^\/admin(\/|$)/,
  /^\/projects\/[^/]+\/settings(\/|$)/,
  /^\/projects\/[^/]+$/, // a project's overview = its deployments listing
  // Workflow console: a stale-cached document referencing dead content-hashed
  // /assets bundles (post-redeploy) renders unstyled — never cache these.
  /^\/workflows(\/|$)/,
  /^\/wfc(\/|$)/,
];

// Public marketing / docs / status pages — shared (CDN) + browser cacheable.
// Deliberately NOT here: "/" (force-dynamic auth flip — see NO_STORE) and
// /sign-in|/sign-up (auth surfaces; Clerk's middleware/handshake acts
// per-request around them, and a shared cache must never hold their
// responses — they fall through to the `private` default below).
const PUBLIC_PAGES = [
  /^\/(product|solutions|features|pricing|blog|case-studies|contact|privacy|docs|status)(\/|$)/,
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

/* ---- CORS for our OWN sibling origin, on the proxy surfaces only ----------
 *
 * `/cloud/*` and `/api/*` are deliberately SAME-ORIGIN proxies (next.config's
 * rewrite says so outright: "Proxy dashboard API calls to a hive-cloud node's
 * admin API (avoids CORS)"), so normally no CORS headers are needed and none
 * are emitted.
 *
 * The failure this exists for: the site answers on BOTH the apex and `www`. A
 * browser that has cached a permanent apex→www redirect applies it BEFORE the
 * network is consulted, so a page loaded on the apex issues `/api/...` and
 * `/cloud/...` requests the browser itself rewrites to the www host — silently
 * turning a same-origin call cross-origin. The preflight then fails with "No
 * 'Access-Control-Allow-Origin' header is present" and run-node loses presence
 * publishing and gitops sync entirely. Witnessed live; this is NOT a stray
 * server rule to delete — all 14 fleet nodes answer that preflight 204 with no
 * Location, so nothing server-side is redirecting. Making the two origins
 * interchangeable is the only durable fix.
 *
 * Deliberately narrow: only these two path prefixes, the Origin is ECHOED only
 * when it is one of OUR hosts (never reflected blindly, never `*` — these are
 * cookie-authenticated and `*` is illegal with credentials), and `Vary: Origin`
 * keeps a cache from serving one origin's response to the other. */
function isOwnOrigin(origin: string | null): origin is string {
  if (!origin) return false;
  let host: string;
  try {
    const u = new URL(origin);
    if (u.protocol !== "https:" && u.protocol !== "http:") return false;
    host = u.hostname.toLowerCase();
  } catch {
    return false;
  }
  const apex = (process.env.NEXT_PUBLIC_HIVE_PLATFORM_DOMAIN || "shadw.cloud").toLowerCase();
  return host === apex || host === `www.${apex}` || host === "localhost" || host === "127.0.0.1";
}

function isProxySurface(pathname: string): boolean {
  return pathname.startsWith("/api/") || pathname.startsWith("/cloud/");
}

function withCors(origin: string, res: Response): Response {
  res.headers.set("Access-Control-Allow-Origin", origin);
  res.headers.set("Access-Control-Allow-Credentials", "true");
  res.headers.append("Vary", "Origin");
  return res;
}

/** Wraps a proxy handler so a cross-origin call between our own hosts works:
 *  answers the preflight directly (the upstream admin API has no OPTIONS route)
 *  and stamps the echo headers on the real response. Inert for every request
 *  that is same-origin or outside the two proxy surfaces. */
function corsWrapped<Req extends { nextUrl: { pathname: string }; headers: Headers; method: string }, Ev>(
  inner: (req: Req, event: Ev) => Response | Promise<Response>,
): (req: Req, event: Ev) => Promise<Response> {
  return async (req: Req, event: Ev) => {
    const origin = req.headers.get("origin");
    const wants = isProxySurface(req.nextUrl.pathname) && isOwnOrigin(origin);
    if (wants && req.method === "OPTIONS") {
      const pre = new NextResponse(null, { status: 204 });
      pre.headers.set("Access-Control-Allow-Methods", "GET,HEAD,POST,PUT,PATCH,DELETE,OPTIONS");
      // Echo the requested headers: the upstream still does its own
      // authorization, so allowing a header name grants nothing by itself.
      pre.headers.set(
        "Access-Control-Allow-Headers",
        req.headers.get("access-control-request-headers") || "content-type,authorization",
      );
      pre.headers.set("Access-Control-Max-Age", "600");
      return withCors(origin, pre);
    }
    const res = await inner(req, event);
    return wants ? withCors(origin, res) : res;
  };
}

// Operations Console (/admin) AND its API proxy (/ops, -> admin.shadw.cloud) —
// platform-owner only. Restricted to this fixed allow-list of emails; any
// other signed-in user is bounced to the dashboard. Both must share this gate:
// /admin is only the UI, /ops is the actual privileged backend it talks to —
// gating one without the other left the real admin API reachable by anyone
// who called /ops/* directly instead of clicking through the /admin page.
const isAdminRoute = createRouteMatcher(["/admin(.*)", "/ops(.*)"]);
const ADMIN_EMAILS = new Set([
  "dylanwong007@gmail.com",
  "dylan@shadw.com",
  "noahbladenbankers@gmail.com",
  "dylan@simplyfi.cloud",
  "dylan@shadw.cloud",
  "dylan@weave.cloud",
]);

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
    // Build the sign-in return URL from the PUBLIC origin, never req.url.
    // Under self-hosted `next start -p 3002` behind hive-node's reverse proxy,
    // req.url's host is the loopback upstream (localhost:3002/127.0.0.1:3002)
    // — live-captured on a real phone-shaped request as
    // `redirect_url=https%3A%2F%2Flocalhost%3A3002%2Fprojects`, an unreachable
    // return address that bounces the sign-in round-trip back to itself. The
    // hive-node proxy always stamps x-forwarded-host/-proto with the real
    // public origin (main.rs dashboard_proxy); trust those, fall back to
    // req.url only for direct (un-proxied, dev) access.
    const fwdHost = req.headers.get("x-forwarded-host");
    const fwdProto = req.headers.get("x-forwarded-proto") || "https";
    const publicBase = fwdHost ? `${fwdProto}://${fwdHost}` : req.url;
    const returnTo = new URL(req.nextUrl.pathname + req.nextUrl.search, publicBase).toString();
    await auth.protect({ unauthenticatedUrl: new URL(`/sign-in?redirect_url=${encodeURIComponent(returnTo)}`, publicBase).toString() });
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
// `next start` on the Next.js 14.2.35 build this was confirmed against (the final
// 14.2.x release; the dashboard now runs Next 16, guard retained defensively),
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
// export is the new contract. `neutralizeClerkSelfRewrite` is retained as a
// defensive guard: this build pins @clerk/nextjs ^6.39.x (v7 was rejected because
// it removed the `SignedIn`/`SignedOut` components this dashboard relies on). The
// guard only rewrites when the response's `x-middleware-rewrite` target equals the
// request URL, so it stays inert unless that exact same-URL self-rewrite reappears.
export const proxy = bypass
  ? corsWrapped((req: Request & { nextUrl: { pathname: string }; method: string }) =>
      withCache(req, NextResponse.next()),
    )
  : corsWrapped(
      async (req: Parameters<typeof clerk>[0], event: Parameters<typeof clerk>[1]) => {
        const res = await clerk(req, event);
        return neutralizeClerkSelfRewrite(req, res ?? NextResponse.next());
      },
    );

export const config = {
  // Run on app routes. Skip Next internals + static FILES (extension at the end),
  // but still gate app routes that legitimately contain dots (e.g. a domain
  // detail page like /domains/example.com) — otherwise they'd bypass auth.
  // wasm+mjs are in the exclusion list: the browser-node package
  // (/browser-node/pkg/*.wasm) and the sqlite module set (crsqlite-sync.mjs,
  // crsqlite-sync.wasm) are PUBLIC static assets a not-yet-signed-in browser
  // must be able to fetch — gating them 307s a wasm fetch into a sign-in HTML
  // redirect and the whole browser-node boot fails (witnessed 2026-08-04:
  // hive_browser_bg.wasm -> 307 while hive_browser.js -> 200).
  matcher: [
    "/((?!_next)(?!.*\\.(?:ico|png|jpg|jpeg|gif|svg|webp|woff2?|ttf|css|js|mjs|wasm|map|txt|xml|webmanifest)$).*)",
    "/",
  ],
};
