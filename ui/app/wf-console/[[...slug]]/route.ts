// Catch-all route serving the upstream @workflow/web console (its real Express app, mounted through a Node<->Web bridge) at /wfc,
// embedded by the /workflows page. next.config.mjs rewrites /wfc, /wfc/:path* and /assets/* to /wf-console/*; this handler strips that
// prefix, and per-request auth (hive_jwt cookie, x-hive-team) reaches lib/wf-console/hive-world.mjs through a shared AsyncLocalStorage.

import path from "node:path";
import { AsyncLocalStorage } from "node:async_hooks";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
// @ts-ignore — express ships without bundled type defs; treat as opaque.
import express from "express";

import "@workflow/web/server";

import { expressToFetch } from "@/lib/wf-console/express-bridge";

type ExpressApp = any;

type WfRequestContext = { team?: string; authToken?: string; project?: string };

const ALS_KEY = Symbol.for("hive.wfConsole.requestContext");
const g = globalThis as unknown as Record<symbol, unknown>;
if (!g[ALS_KEY]) g[ALS_KEY] = new AsyncLocalStorage<WfRequestContext>();
const als = g[ALS_KEY] as AsyncLocalStorage<WfRequestContext>;

let appPromise: Promise<ExpressApp> | null = null;

async function getApp(): Promise<ExpressApp> {
  if (appPromise) return appPromise;
  const p = (async () => {
    if (!process.env.WORKFLOW_TARGET_WORLD) {
      process.env.WORKFLOW_TARGET_WORLD = path.join(
        process.cwd(),
        "lib",
        "wf-console",
        "hive-world.mjs"
      );
    }

    let buildDir = path.join(process.cwd(), "node_modules", "@workflow", "web", "build");
    try {
      const req = createRequire(import.meta.url);
      const pkgJson = req.resolve("@workflow/web/package.json");
      if (pkgJson) buildDir = path.join(path.dirname(pkgJson), "build");
    } catch {}

    const serverEntry = path.join(buildDir, "server/index.js");
    const { app: rrApp } = (await import(
      /* webpackIgnore: true */ pathToFileURL(serverEntry).href
    )) as { app: ExpressApp };

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
  appPromise = p;
  p.catch(() => {
    if (appPromise === p) appPromise = null;
  });
  return p;
}

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

/** Deterministic Content-Type by extension for /assets: the bridge intermittently dropped it, and `nosniff` then makes the browser
 *  refuse the module script, which silently kills the console's whole hydration. */
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

  let rewritten = url.pathname.replace(/^\/wf-console/, "") || "/";
  if (rewritten === "") rewritten = "/";

  const app = await getApp();
  const rewriteUrl = rewritten + url.search;

  const cookieHeader = req.headers.get("cookie");
  let authToken = readCookie(cookieHeader, "hive_jwt");
  if (!authToken) {
    const bearer = req.headers.get("authorization");
    if (bearer && /^Bearer\s+/i.test(bearer)) {
      authToken = bearer.replace(/^Bearer\s+/i, "").trim() || undefined;
    }
  }
  const wfCtx: WfRequestContext = {
    team: req.headers.get("x-hive-team") || url.searchParams.get("hiveTeam") || undefined,
    authToken,
    project: url.searchParams.get("hiveProject") || undefined,
  };

  const resp = await als.run(wfCtx, () => expressToFetch(app, req, rewriteUrl));

  if (isPublicAssetPath(rewritten)) {
    const h = new Headers(resp.headers);
    h.set("Cache-Control", "no-cache");
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
  const h = new Headers(resp.headers);
  h.set("Cache-Control", "private, no-store, max-age=0, must-revalidate");

  if ((resp.headers.get("content-type") || "").includes("text/html")) {
    const html = await resp.text();
    const injected = html.includes("</head>")
      ? html.replace("</head>", `${TRANSPARENT_EMBED_CSS}</head>`)
      : TRANSPARENT_EMBED_CSS + html;
    h.delete("content-length");
    return new Response(injected, { status: resp.status, statusText: resp.statusText, headers: h });
  }
  return new Response(resp.body, {
    status: resp.status,
    statusText: resp.statusText,
    headers: h,
  });
}

/** Transparent background for the embedded console document only (html/body and the app root), not a blanket `bg-background`
 *  override: cards and the sticky header must stay filled. */
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
