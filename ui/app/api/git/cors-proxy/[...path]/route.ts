// Same-origin git smart-HTTP proxy for client-side isomorphic-git.
//
// Browsers can't speak the git HTTP protocol to github.com directly (no CORS).
// isomorphic-git is configured with `corsProxy: "/api/git/cors-proxy"`, which
// rewrites a remote URL `https://github.com/owner/repo/...` to
// `/api/git/cors-proxy/github.com/owner/repo/...`. This route reconstructs the
// upstream URL and forwards the request server-side (no CORS on that hop),
// streaming the (binary) response back.
//
// Safety: only git hosts are allowed (no open proxy / SSRF), and only the git
// smart-HTTP endpoints. Any Authorization header the client set via isomorphic-
// git's `onAuth` (the user's PAT) is forwarded to the host but never stored.
//
// Composio connection as the git client: when the request targets github.com and
// the browser did NOT supply its own Authorization (no PAT), we inject the GitHub
// OAuth token from THIS user's ACTIVE Composio connection server-side — so the
// in-browser git uses the linked GitHub account without a pasted PAT, and the
// token never reaches the browser. Fully additive: a request that already carries
// auth (PAT) is forwarded unchanged, and if there's no Composio token we fall back
// to the original behavior (anonymous for public repos).

import { NextRequest, NextResponse } from "next/server";
import { githubAccessToken, resolveEntity } from "@/lib/composio";

export const dynamic = "force-dynamic";

// Allow-list of git hosts. Add more as needed.
const ALLOWED_HOSTS = new Set(["github.com", "gitlab.com", "bitbucket.org", "codeberg.org"]);

// Only the git smart-HTTP paths.
function isGitPath(pathname: string): boolean {
  return (
    pathname.endsWith("/info/refs") ||
    pathname.endsWith("/git-upload-pack") ||
    pathname.endsWith("/git-receive-pack")
  );
}

// Request headers we forward upstream (everything git needs, incl. auth).
const FORWARD_REQ = [
  "authorization",
  "content-type",
  "accept",
  "accept-encoding",
  "git-protocol",
  "user-agent",
];

function buildTarget(params: { path: string[] }, search: string): URL | null {
  const joined = params.path.join("/");
  // The path starts with the host (protocol stripped by isomorphic-git).
  const host = joined.split("/")[0];
  if (!ALLOWED_HOSTS.has(host)) return null;
  let url: URL;
  try {
    url = new URL(`https://${joined}${search}`);
  } catch {
    return null;
  }
  if (url.hostname !== host || !ALLOWED_HOSTS.has(url.hostname)) return null;
  if (!isGitPath(url.pathname)) return null;
  return url;
}

async function proxy(req: NextRequest, params: { path: string[] }): Promise<Response> {
  const target = buildTarget(params, req.nextUrl.search);
  if (!target) {
    return NextResponse.json({ error: "forbidden git proxy target" }, { status: 403 });
  }
  const headers = new Headers();
  for (const h of FORWARD_REQ) {
    const v = req.headers.get(h);
    if (v) headers.set(h, v);
  }
  // git servers require a User-Agent.
  if (!headers.has("user-agent")) headers.set("user-agent", "git/isomorphic-git");

  // Use the Composio GitHub connection as the client credential when the browser
  // sent no auth of its own. ONLY for github.com, ONLY when no Authorization is
  // present — so a pasted PAT (already forwarded above) always wins, and a public
  // anonymous clone with no Composio link still works. Best-effort: any failure
  // leaves the request exactly as it was.
  if (target.hostname === "github.com" && !headers.has("authorization")) {
    try {
      const entity = await resolveEntity();
      const token = await githubAccessToken(entity);
      if (token) {
        // GitHub accepts an OAuth token as the Basic-auth password (the same shape
        // isomorphic-git uses for a PAT): username `x-access-token`, password=token.
        const basic = Buffer.from(`x-access-token:${token}`).toString("base64");
        headers.set("authorization", `Basic ${basic}`);
      }
    } catch {
      /* no Composio token available — fall through unauthenticated (unchanged behavior) */
    }
  }

  const init: RequestInit = {
    method: req.method,
    headers,
    redirect: "follow",
  };
  if (req.method !== "GET" && req.method !== "HEAD") {
    init.body = await req.arrayBuffer();
  }

  let upstream: Response;
  try {
    upstream = await fetch(target.toString(), init);
  } catch {
    return NextResponse.json({ error: "git upstream fetch failed" }, { status: 502 });
  }

  // Pass through status + content-type + the binary body.
  const respHeaders = new Headers();
  const ct = upstream.headers.get("content-type");
  if (ct) respHeaders.set("content-type", ct);
  const cc = upstream.headers.get("cache-control");
  if (cc) respHeaders.set("cache-control", cc);
  const body = await upstream.arrayBuffer();
  return new Response(body, { status: upstream.status, headers: respHeaders });
}

export async function GET(req: NextRequest, ctx: { params: { path: string[] } }) {
  return proxy(req, ctx.params);
}

export async function POST(req: NextRequest, ctx: { params: { path: string[] } }) {
  return proxy(req, ctx.params);
}
