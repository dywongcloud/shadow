import { clerkClient, clerkMiddleware, createRouteMatcher } from "@clerk/nextjs/server";
import { NextResponse } from "next/server";

const isPublic = createRouteMatcher([
  "/",
  "/docs(.*)",
  "/product(.*)",
  "/solutions(.*)",
  "/features(.*)",
  "/pricing(.*)",
  "/blog(.*)",
  "/case-studies(.*)",
  "/contact(.*)",
  "/privacy(.*)",
  "/opengraph-image(.*)",
  "/offline.html",
  "/sw.js",
  "/sign-in(.*)",
  "/sign-up(.*)",
  "/oauth/github/callback",
  "/cloud(.*)",
  "/api(.*)",
  "/status(.*)",
  "/wfc(.*)",
]);

const isClerkFree = createRouteMatcher([
  "/",
  "/docs(.*)",
  "/product(.*)",
  "/solutions(.*)",
  "/features(.*)",
  "/pricing(.*)",
  "/blog(.*)",
  "/case-studies(.*)",
  "/contact(.*)",
  "/privacy(.*)",
  "/status(.*)",
  "/opengraph-image(.*)",
  "/offline.html",
  "/sw.js",
]);

const bypass = process.env.HIVE_AUTH_BYPASS === "1" && process.env.NODE_ENV !== "production";

const NO_STORE = [
  /^\/account(\/|$)/,
  /^\/settings(\/|$)/,
  /^\/teams(\/|$)/,
  /^\/network(\/|$)/,
  /^\/deployments(\/|$)/,
  /^\/billing(\/|$)/,
  /^\/admin(\/|$)/,
  /^\/projects\/[^/]+\/settings(\/|$)/,
  /^\/projects\/[^/]+$/,
  /^\/workflows(\/|$)/,
  /^\/wfc(\/|$)/,
];

const PUBLIC_PAGES = [
  /^\/(product|solutions|features|pricing|blog|case-studies|contact|privacy|docs|status)(\/|$)/,
];

const LANDING_CACHE = "public, max-age=60, s-maxage=300, stale-while-revalidate=300";

/** Cache-Control for a dashboard/marketing route; `null` for the live-data proxies (`/cloud`, `/api`). */
function cacheControlFor(pathname: string): string | null {
  if (pathname.startsWith("/cloud") || pathname.startsWith("/ops") || pathname.startsWith("/api")) return null;
  if (NO_STORE.some((re) => re.test(pathname))) return "private, no-store, max-age=0, must-revalidate";
  if (pathname === "/") return LANDING_CACHE;
  if (PUBLIC_PAGES.some((re) => re.test(pathname)))
    return "public, max-age=300, s-maxage=3600, stale-while-revalidate=86400";
  return "private, max-age=60, stale-while-revalidate=300";
}

function withCache(req: { nextUrl: { pathname: string } }, res: Response): Response {
  const cc = cacheControlFor(req.nextUrl.pathname);
  if (cc) res.headers.set("Cache-Control", cc);
  return res;
}

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

/** Wraps a proxy handler so a cross-origin call between our own hosts works: answers the
 *  preflight (the upstream admin API has no OPTIONS route) and stamps the echo headers; inert
 *  for same-origin requests and outside the two proxy surfaces. */
function corsWrapped<Req extends { nextUrl: { pathname: string }; headers: Headers; method: string }, Ev>(
  inner: (req: Req, event: Ev) => Response | Promise<Response>,
): (req: Req, event: Ev) => Promise<Response> {
  return async (req: Req, event: Ev) => {
    const origin = req.headers.get("origin");
    const wants = isProxySurface(req.nextUrl.pathname) && isOwnOrigin(origin);
    if (wants && req.method === "OPTIONS") {
      const pre = new NextResponse(null, { status: 204 });
      pre.headers.set("Access-Control-Allow-Methods", "GET,HEAD,POST,PUT,PATCH,DELETE,OPTIONS");
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

const isAdminRoute = createRouteMatcher(["/admin(.*)", "/ops(.*)"]);
const ADMIN_EMAILS = new Set([
  "dylanwong007@gmail.com",
  "dylan@shadw.com",
  "noahbladenbankers@gmail.com",
  "dylan@simplyfi.cloud",
  "dylan@shadw.cloud",
  "dylan@weave.cloud",
]);

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
    return false;
  }
}

const clerk = clerkMiddleware(async (auth, req) => {
  if (isAdminRoute(req)) {
    const { userId } = await auth();
    if (!userId || !(await isPlatformOwner(userId))) {
      if (req.nextUrl.pathname.startsWith("/ops")) {
        return withCache(req, NextResponse.json({ error: "forbidden" }, { status: 403 }));
      }
      return withCache(req, NextResponse.redirect(new URL("/", req.url)));
    }
  } else if (!isPublic(req)) {
    const fwdHost = req.headers.get("x-forwarded-host");
    const fwdProto = req.headers.get("x-forwarded-proto") || "https";
    const publicBase = fwdHost ? `${fwdProto}://${fwdHost}` : req.url;
    const returnTo = new URL(req.nextUrl.pathname + req.nextUrl.search, publicBase).toString();
    await auth.protect({ unauthenticatedUrl: new URL(`/sign-in?redirect_url=${encodeURIComponent(returnTo)}`, publicBase).toString() });
  }
  return withCache(req, NextResponse.next());
});

function neutralizeClerkSelfRewrite(req: Request, res: Response): Response {
  const rewrite = res.headers.get("x-middleware-rewrite");
  if (rewrite && rewrite === req.url) {
    res.headers.delete("x-middleware-rewrite");
    res.headers.set("x-middleware-next", "1");
  }
  return res;
}

export const proxy = bypass
  ? corsWrapped((req: Request & { nextUrl: { pathname: string }; method: string }) =>
      withCache(req, NextResponse.next()),
    )
  : corsWrapped(
      async (req: Parameters<typeof clerk>[0], event: Parameters<typeof clerk>[1]) => {
        if (isClerkFree(req)) return withCache(req, NextResponse.next());
        const res = await clerk(req, event);
        return neutralizeClerkSelfRewrite(req, res ?? NextResponse.next());
      },
    );

export const config = {
  matcher: [
    "/((?!_next)(?!.*\\.(?:ico|png|jpg|jpeg|gif|svg|webp|woff2?|ttf|css|js|mjs|wasm|map|txt|xml|webmanifest)$).*)",
    "/",
  ],
};
