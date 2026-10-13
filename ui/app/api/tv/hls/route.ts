import { NextRequest, NextResponse } from "next/server";

// Same-origin HLS proxy: hls.js fetches the playlist and every segment over fetch/XHR, which the CSP `connect-src` gates, and stream hosts are arbitrary third
// parties that can never be allowlisted. The playlist's segment/variant URIs are REWRITTEN to point back at this route (resolved against the playlist's own
// URL) and segments stream straight through; only https upstreams are proxied and the platform node is the client, so no browser CSP is weakened (at the cost of
// viewing bytes on the node's egress).

const SELF_PATH = "/api/tv/hls";

function proxied(absUrl: string): string {
  return `${SELF_PATH}?url=${encodeURIComponent(absUrl)}`;
}

/** Rewrite every URI line + URI="" attribute in an m3u8 to route through us. */
function rewritePlaylist(text: string, baseUrl: string): string {
  const base = new URL(baseUrl);
  const resolve = (u: string) => new URL(u, base).toString();
  return text
    .split("\n")
    .map((line) => {
      const t = line.trim();
      if (!t) return line;
      if (t.startsWith("#")) {
        return line.replace(/URI="([^"]+)"/g, (_m, u) => `URI="${proxied(resolve(u))}"`);
      }
      return proxied(resolve(t));
    })
    .join("\n");
}

export async function GET(req: NextRequest) {
  const url = req.nextUrl.searchParams.get("url");
  if (!url) return new NextResponse("missing url", { status: 400 });
  let target: URL;
  try {
    target = new URL(url);
  } catch {
    return new NextResponse("bad url", { status: 400 });
  }
  if (target.protocol !== "https:") {
    return new NextResponse("only https upstreams are proxied", { status: 400 });
  }
  const range = req.headers.get("range");
  const fetchUpstream = () =>
    fetch(target.toString(), {
      headers: {
        "user-agent": "Mozilla/5.0 (compatible; shadw-tv/1.0)",
        ...(range ? { range } : {}),
      },
      cache: "no-store",
      signal: AbortSignal.timeout(25_000),
    });
  let upstream: Response;
  try {
    upstream = await fetchUpstream();
  } catch {
    try {
      upstream = await fetchUpstream();
    } catch (e) {
      return new NextResponse(`upstream error: ${String(e)}`, { status: 502 });
    }
  }
  if (!upstream.ok) {
    return new NextResponse(`upstream ${upstream.status}`, { status: 502 });
  }

  const ctype = (upstream.headers.get("content-type") || "").toLowerCase();
  const isPlaylist =
    target.pathname.endsWith(".m3u8") ||
    ctype.includes("mpegurl") ||
    ctype.includes("vnd.apple.mpegurl");

  if (isPlaylist) {
    const text = await upstream.text();
    const urlAfterRedirects = upstream.url || target.toString();
    const rewritten = rewritePlaylist(text, urlAfterRedirects);
    return new NextResponse(rewritten, {
      status: 200,
      headers: {
        "content-type": "application/vnd.apple.mpegurl",
        "cache-control": "no-store",
      },
    });
  }

  const segHeaders: Record<string, string> = {
    "content-type": ctype || "application/octet-stream",
    "cache-control": "no-store",
  };
  for (const h of ["content-range", "accept-ranges", "content-length"]) {
    const v = upstream.headers.get(h);
    if (v) segHeaders[h] = v;
  }
  return new NextResponse(upstream.body, {
    status: upstream.status === 206 ? 206 : 200,
    headers: segHeaders,
  });
}
