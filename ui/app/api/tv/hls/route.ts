import { NextRequest, NextResponse } from "next/server";
import { UpstreamError, boundedBody, readBounded, safeGet, type SafeResponse } from "@/lib/safe-fetch";

const SELF_PATH = "/api/tv/hls";
const UPSTREAM_TIMEOUT_MS = 25_000;
const PLAYLIST_MAX_BYTES = 2 * 1024 * 1024;
const BODY_MAX_BYTES = 64 * 1024 * 1024;
const SINGLE_RANGE = /^bytes=\d*-\d*$/;
const PLAYLIST_PATH = /\.m3u8?$/i;
const TS_SEGMENT_PATH = /\.ts$/i;
const MISLABELLED_TS_TYPES = new Set(["text/vnd.trolltech.linguist", "text/plain"]);
const MEDIA_TYPE = /^(?:video|audio)\/[\w.+-]+$|^(?:application|binary)\/octet-stream$|^application\/mp4$|^text\/vtt$/;
const BASE_HEADERS = { "cache-control": "no-store", "x-content-type-options": "nosniff" };

function reply(body: BodyInit | null, status: number, headers: Record<string, string> = {}): NextResponse {
  return new NextResponse(body, { status, headers: { ...BASE_HEADERS, ...headers } });
}

function proxied(absUrl: string): string {
  return `${SELF_PATH}?url=${encodeURIComponent(absUrl)}`;
}

/** Rewrite every URI line + URI="" attribute in an m3u8 to route through us, resolving each against the playlist's FINAL url. */
function rewritePlaylist(text: string, base: URL): string {
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

async function fetchUpstream(target: URL, headers: Record<string, string>, signal: AbortSignal): Promise<SafeResponse> {
  try {
    return await safeGet(target, { headers, timeoutMs: UPSTREAM_TIMEOUT_MS, signal });
  } catch (e) {
    if (e instanceof UpstreamError || signal.aborted) throw e;
    return await safeGet(target, { headers, timeoutMs: UPSTREAM_TIMEOUT_MS, signal });
  }
}

async function respond(upstream: SafeResponse): Promise<NextResponse> {
  if (upstream.status < 200 || upstream.status > 299) {
    upstream.body.destroy();
    return reply(`upstream ${upstream.status}`, 502);
  }
  const ctype = String(upstream.headers["content-type"] ?? "").toLowerCase();
  if (PLAYLIST_PATH.test(upstream.url.pathname) || ctype.includes("mpegurl")) {
    const text = (await readBounded(upstream.body, PLAYLIST_MAX_BYTES)).toString("utf8");
    if (!text.trimStart().startsWith("#EXTM3U")) return reply("upstream is not an HLS playlist", 415);
    try {
      return reply(rewritePlaylist(text, upstream.url), 200, { "content-type": "application/vnd.apple.mpegurl" });
    } catch {
      return reply("malformed upstream playlist", 502);
    }
  }
  const declaredType = ctype.split(";")[0].trim() || "application/octet-stream";
  const mediaType = MISLABELLED_TS_TYPES.has(declaredType) && TS_SEGMENT_PATH.test(upstream.url.pathname) ? "video/mp2t" : declaredType;
  if (!MEDIA_TYPE.test(mediaType)) {
    upstream.body.destroy();
    return reply("unsupported upstream content type", 415);
  }
  const declaredLength = Number(upstream.headers["content-length"] ?? 0);
  if (declaredLength > BODY_MAX_BYTES) {
    upstream.body.destroy();
    return reply("upstream body exceeds the size cap", 502);
  }
  const segHeaders: Record<string, string> = { "content-type": mediaType };
  for (const h of ["content-range", "accept-ranges", "content-length"]) {
    const v = upstream.headers[h];
    if (typeof v === "string") segHeaders[h] = v;
  }
  return reply(boundedBody(upstream.body, BODY_MAX_BYTES), upstream.status === 206 ? 206 : 200, segHeaders);
}

/** Same-origin HLS proxy so hls.js obeys the CSP `connect-src 'self'`: playlists are rewritten to point back here and segments stream through, but only
 *  public https hosts are reachable (`safeGet`), only `range` is forwarded, and bodies are size- and content-type-bounded. */
export async function GET(req: NextRequest) {
  const raw = req.nextUrl.searchParams.get("url");
  if (!raw) return reply("missing url", 400);
  let target: URL;
  try {
    target = new URL(raw);
  } catch {
    return reply("bad url", 400);
  }
  const range = req.headers.get("range");
  const headers = {
    "user-agent": "Mozilla/5.0 (compatible; shadw-tv/1.0)",
    accept: "*/*",
    "accept-encoding": "identity",
    ...(range && SINGLE_RANGE.test(range) ? { range } : {}),
  };
  try {
    return await respond(await fetchUpstream(target, headers, req.signal));
  } catch (e) {
    if (e instanceof UpstreamError) return reply(e.message, e.status);
    return reply("upstream error", 502);
  }
}
