// app/wf-console/[[...slug]]/route.ts
//
// Catch-all Next.js route that serves the LITERAL upstream @workflow/web
// dashboard (Vercel Workflow DevKit's packages/web) at /wfc — embedded by the
// dashboard's own /workflows page (platform navbar + seamless iframe, see
// components/wf-console-frame.tsx). The upstream build lives in
// node_modules/@workflow/web/build/{client,server}; we mount its real Express
// app through a small Node<->Web bridge so the untouched upstream code
// answers every request — its own React Router SPA, its own Tailwind v4 CSS
// (scoped to its own /assets/*.css files, fully isolated from hive's
// Tailwind v3 dashboard), its own CBOR /wfc/api/rpc.
//
// Mount strategy (the agentos wf-app pattern, adapted):
//   - next.config.mjs rewrites /wfc, /wfc/:path* and /assets/* into
//     /wf-console/*; this handler strips the /wf-console prefix and
//     hands the remaining path to the upstream Express app.
//   - scripts/patch-wf-console.mjs re-bases the compiled React Router build
//     to basename "/wfc", so the SSR HTML, client hydration, in-app
//     navigation and .data loader fetches all agree on /wfc/* URLs.
//   - Data comes from hive: the bundle resolves its World via
//     WORKFLOW_TARGET_WORLD -> lib/wf-console/hive-world.mjs, which proxies
//     every read + run-op to hive's /v1/workflows/* API.
//   - Per-request auth (hive_jwt cookie -> Authorization, x-hive-team) is
//     threaded to that World through an AsyncLocalStorage shared via a
//     global symbol — the bundle's World methods have no request argument.

import path from "node:path";
import { AsyncLocalStorage } from "node:async_hooks";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-ignore — express ships without bundled type defs; treat as opaque.
import express from "express";

// Static import so Next/Vercel TRACE the package into the server bundle even
// though the build files are loaded dynamically below.
import "@workflow/web/server";

import { expressToFetch } from "@/lib/wf-console/express-bridge";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type ExpressApp = any;

export const dynamic = "force-dynamic";
export const runtime = "nodejs";

type WfRequestContext = { team?: string; authToken?: string; project?: string };

const ALS_KEY = Symbol.for("hive.wfConsole.requestContext");
const g = globalThis as unknown as Record<symbol, unknown>;
if (!g[ALS_KEY]) g[ALS_KEY] = new AsyncLocalStorage<WfRequestContext>();
const als = g[ALS_KEY] as AsyncLocalStorage<WfRequestContext>;

let appPromise: Promise<ExpressApp> | null = null;

async function getApp(): Promise<ExpressApp> {
  if (appPromise) return appPromise;
  const p = (async () => {
    // The compiled bundle resolves its World with
    // require(process.env.WORKFLOW_TARGET_WORLD) — point it at the hive
    // World proxy BEFORE the bundle is imported. Overridable from the
    // environment for debugging (e.g. back to a local world).
    if (!process.env.WORKFLOW_TARGET_WORLD) {
      process.env.WORKFLOW_TARGET_WORLD = path.join(
        process.cwd(),
        "lib",
        "wf-console",
        "hive-world.mjs"
      );
    }

    // Resolve the @workflow/web build dir from node_modules.
    let buildDir = path.join(process.cwd(), "node_modules", "@workflow", "web", "build");
    try {
      const req = createRequire(import.meta.url);
      const pkgJson = req.resolve("@workflow/web/package.json");
      if (pkgJson) buildDir = path.join(path.dirname(pkgJson), "build");
    } catch {
      // best-effort; cwd-based fallback above
    }

    const serverEntry = path.join(buildDir, "server/index.js");
    const { app: rrApp } = (await import(
      /* webpackIgnore: true */ pathToFileURL(serverEntry).href
    )) as { app: ExpressApp };

    // Same composition order as the upstream server.js: static client assets
    // first, then the React Router server app.
    const server = express();
    server.use(
      "/assets",
      express.static(path.join(buildDir, "client/assets"), {
        immutable: true,
        maxAge: "1y",
      })
    );
    server.use(express.static(path.join(buildDir, "client"), { maxAge: "1h" }));
    server.use(rrApp);
    return server;
  })();
  // A transiently-failed init (e.g. fs hiccup mid-deploy) must not stick as a
  // forever-rejected cached promise — clear it so the next request retries.
  appPromise = p;
  p.catch(() => {
    if (appPromise === p) appPromise = null;
  });
  return p;
}

// Warm the ~6MB upstream server bundle at module load so the FIRST console
// request doesn't pay the import latency (a cold first paint racing its own
// CSS was one ingredient of the unstyled-first-load defect).
void getApp().catch(() => {});

function isPublicAssetPath(p: string): boolean {
  return (
    p.startsWith("/assets/") ||
    p === "/favicon.ico" ||
    p.endsWith(".css") ||
    p.endsWith(".js") ||
    p.endsWith(".woff") ||
    p.endsWith(".woff2")
  );
}

// Deterministic Content-Type by extension for the asset branch. The express
// bridge was observed INTERMITTENTLY dropping Content-Type on large chunks
// (same asset served with the header on one request and without it on the
// next) — and with the platform-wide `X-Content-Type-Options: nosniff`, a
// typeless module script is refused by the browser ("Failed to fetch
// dynamically imported module: /assets/entry.client-….js"), which silently
// killed the console's ENTIRE hydration: static SSR HTML, dead buttons, an
// empty runs table that never fetches. Never depend on the pass-through for
// something this load-bearing.
const ASSET_TYPES: Record<string, string> = {
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".map": "application/json; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
};

function assetTypeFor(p: string): string | undefined {
  const dot = p.lastIndexOf(".");
  if (dot === -1) return undefined;
  return ASSET_TYPES[p.slice(dot).toLowerCase()];
}

function readCookie(header: string | null, name: string): string | undefined {
  if (!header) return undefined;
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq === -1) continue;
    if (part.slice(0, eq).trim() === name) {
      return decodeURIComponent(part.slice(eq + 1).trim());
    }
  }
  return undefined;
}

async function handle(req: Request): Promise<Response> {
  const url = new URL(req.url);

  // Strip our /wf-console prefix. The remaining path is what the upstream
  // Express app expects: /workflows[...] (the re-based React Router app,
  // including /workflows/api/rpc + /workflows/api/stream/*) or /assets/*.
  let rewritten = url.pathname.replace(/^\/wf-console/, "") || "/";
  if (rewritten === "") rewritten = "/";

  const app = await getApp();
  const rewriteUrl = rewritten + url.search;

  // Per-request hive auth context for the World proxy (see hive-world.mjs):
  // the caller's platform JWT (httpOnly hive_jwt cookie, or an incoming
  // bearer) + the informational x-hive-team header — the same forwarding
  // shape as ui/lib/gitops-server.ts backend().
  const cookieHeader = req.headers.get("cookie");
  let authToken = readCookie(cookieHeader, "hive_jwt");
  if (!authToken) {
    const bearer = req.headers.get("authorization");
    if (bearer && /^Bearer\s+/i.test(bearer)) {
      authToken = bearer.replace(/^Bearer\s+/i, "").trim() || undefined;
    }
  }
  const wfCtx: WfRequestContext = {
    // The console's own fetches can't set headers on document/data requests,
    // so the patched bundle rides the scope as query params (hiveTeam/
    // hiveProject, sourced from window.name which the embedding iframe sets).
    team: req.headers.get("x-hive-team") || url.searchParams.get("hiveTeam") || undefined,
    authToken,
    project: url.searchParams.get("hiveProject") || undefined,
  };

  const resp = await als.run(wfCtx, () => expressToFetch(app, req, rewriteUrl));

  // The upstream express.static stamps assets `immutable, max-age=1y` keyed on
  // content-hash filenames — but scripts/patch-wf-console.mjs patches those
  // files IN PLACE (same filename, new bytes) and a fleet roll can change the
  // patched output under an unchanged name. A stale `max-age` window let a
  // browser serve ONE chunk from an old cached build while a sibling chunk
  // loaded fresh — a mixed module graph and the fatal "does not provide an
  // export named X" crash. `no-cache` (revalidate ALWAYS, ETag-gated — NOT
  // `no-store`) closes that window entirely: every chunk of a page load is
  // revalidated against the origin, so they always come from the SAME
  // internally-consistent build. Cost is a conditional request per asset;
  // express.static's ETag makes the common case a cheap 304.
  if (isPublicAssetPath(rewritten)) {
    const h = new Headers(resp.headers);
    h.set("Cache-Control", "no-cache");
    // Only stamp on SUCCESS: a 404/500 body is not the asset and must not
    // masquerade as JS/CSS.
    if (resp.status === 200) {
      const t = assetTypeFor(rewritten);
      if (t) h.set("Content-Type", t);
    }
    return new Response(resp.body, {
      status: resp.status,
      statusText: resp.statusText,
      headers: h,
    });
  }
  // HTML/data/rpc responses must NEVER be cached: a stale cached console HTML
  // referencing content-hashed /assets files that no longer exist after a
  // redeploy is exactly the unstyled-first-load (FOUC) failure the user hit —
  // next.config's catch-all PRIVATE_CACHE previously applied max-age=60 +
  // stale-while-revalidate here.
  const h = new Headers(resp.headers);
  h.set("Cache-Control", "private, no-store, max-age=0, must-revalidate");

  // The console is embedded in an iframe on /workflows, where the PLATFORM
  // shell already paints the page background. Its own document background would
  // sit on top of that and read as an opaque panel, so make the embedded
  // document transparent and let the platform background show through.
  //
  // Done here rather than in scripts/patch-wf-console.mjs so it needs no
  // rebuild of the vendored bundle, and gated strictly on `text/html`: the RPC
  // responses are CBOR and `/api/stream/*` is SSE — buffering either would
  // corrupt the payload or stall the stream.
  if ((resp.headers.get("content-type") || "").includes("text/html")) {
    const html = await resp.text();
    const injected = html.includes("</head>")
      ? html.replace("</head>", `${TRANSPARENT_EMBED_CSS}</head>`)
      : TRANSPARENT_EMBED_CSS + html;
    h.delete("content-length"); // body length changed
    return new Response(injected, { status: resp.status, statusText: resp.statusText, headers: h });
  }
  return new Response(resp.body, {
    status: resp.status,
    statusText: resp.statusText,
    headers: h,
  });
}

/// Transparent-background override for the embedded console document. Scoped to
/// the document surfaces only (html/body and the app's own root element) —
/// deliberately NOT a blanket override of the `bg-background` utility, which the
/// console also uses for real surfaces like cards and the sticky header that
/// SHOULD stay filled.
const TRANSPARENT_EMBED_CSS =
  '<style id="hive-transparent-embed">html,body{background:transparent !important}' +
  "body>#root,body>div[data-reactroot],body>div:first-of-type{background:transparent !important}</style>";

export async function GET(req: Request) {
  return handle(req);
}
export async function POST(req: Request) {
  return handle(req);
}
export async function PUT(req: Request) {
  return handle(req);
}
export async function DELETE(req: Request) {
  return handle(req);
}
export async function PATCH(req: Request) {
  return handle(req);
}
