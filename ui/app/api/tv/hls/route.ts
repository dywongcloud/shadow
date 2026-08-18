import { NextRequest, NextResponse } from "next/server";

// Same-origin HLS proxy. hls.js fetches the .m3u8 playlist and every segment
// over fetch/XHR, which the app's CSP `connect-src` gates — and the stream
// hosts are arbitrary third parties that can never be allowlisted. Proxying
// through this route makes every request same-origin ('self'), so the tight
// CSP is untouched. The playlist's segment/variant URIs are REWRITTEN to point
// back at this route (absolute-resolved against the playlist's own URL), so
// hls.js follows them here too; segment responses stream straight through.
//
// Safety: only https upstreams are proxied, and the platform node — not the
// arbitrary host — is the client, so no browser CSP is weakened. This does put
// viewing bytes on the node's egress; acceptable for a nice-to-have TV feature
// and the only CSP-clean way to play arbitrary IPTV.

export const dynamic = "force-dynamic";

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
      // Attribute form: URI="..." (EXT-X-KEY, EXT-X-MEDIA, EXT-X-MAP, …).
      if (t.startsWith("#")) {
        return line.replace(/URI="([^"]+)"/g, (_m, u) => `URI="${proxied(resolve(u))}"`);
      }
      // A bare URI line (segment or variant playlist).
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
  // Forward the client's Range so the video element's own byte-range requests
  // (and #EXT-X-BYTERANGE segments) are satisfied by the upstream as 206.
  const range = req.headers.get("range");
  const fetchUpstream = () =>
    fetch(target.toString(), {
      // Some CDNs 403 an empty UA / wrong referer.
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
    // One transient retry before surfacing. Mid-stream, a single failed
    // segment fetch becomes a FATAL player error, which reads to the user as a
    // freeze — so a blip must not propagate on the first try.
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
    // Resolve relative URIs against the FINAL url after redirects, not the
    // requested one. iptv-org fronts most streams with a redirector (jmp2.uk),
    // so the playlist's relative variant/segment URIs belong to the redirected
    // host — resolving them against the original would 404 every one.
    const base = upstream.url || target.toString();
    const rewritten = rewritePlaylist(text, base);
    return new NextResponse(rewritten, {
      status: 200,
      headers: {
        "content-type": "application/vnd.apple.mpegurl",
        "cache-control": "no-store",
      },
    });
  }

  // Segment (or key): stream the bytes straight through, preserving type and
  // any range/length metadata so partial (206) responses reach the player
  // intact instead of confusing its buffer.
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
