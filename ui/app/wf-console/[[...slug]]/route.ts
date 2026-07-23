// app/wf-console/[[...slug]]/route.ts
//
// Catch-all Next.js route that serves the LITERAL upstream @workflow/web
// dashboard (Vercel Workflow DevKit's packages/web) at /workflows inside
// hive's dashboard. The upstream build lives in
// node_modules/@workflow/web/build/{client,server}; we mount its real Express
// app through a small Node<->Web bridge so the untouched upstream code
// answers every request — its own React Router SPA, its own Tailwind v4 CSS
// (scoped to its own /assets/*.css files, fully isolated from hive's
// Tailwind v3 dashboard), its own CBOR /api/rpc.
//
// Mount strategy (the agentos wf-app pattern, adapted):
//   - next.config.mjs rewrites /workflows, /workflows/:path* and /assets/*
//     into /wf-console/*; this handler strips the /wf-console prefix and
//     hands the remaining path to the upstream Express app.
//   - scripts/patch-wf-console.mjs re-bases the compiled React Router build
//     to basename "/workflows", so the SSR HTML, client hydration, in-app
//     navigation and .data loader fetches all agree on /workflows/* URLs.
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
  appPromise = (async () => {
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
  return appPromise;
}

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
    team: req.headers.get("x-hive-team") || undefined,
    authToken,
    project: url.searchParams.get("hiveProject") || undefined,
  };

  const resp = await als.run(wfCtx, () => expressToFetch(app, req, rewriteUrl));

  // The upstream express.static stamps assets immutable for a year keyed on
  // content-hash filenames — but scripts/patch-wf-console.mjs patches those
  // files IN PLACE (same filename, new bytes). Override to a short
  // revalidating TTL so patched bundles reach browsers.
  if (isPublicAssetPath(rewritten)) {
    const h = new Headers(resp.headers);
    h.set("Cache-Control", "public, max-age=300, must-revalidate");
    return new Response(resp.body, {
      status: resp.status,
      statusText: resp.statusText,
      headers: h,
    });
  }
  return resp;
}

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
