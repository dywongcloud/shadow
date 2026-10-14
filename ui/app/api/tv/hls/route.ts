import { NextRequest, NextResponse } from "next/server";
import { UpstreamError, boundedBody, contentCoding, readBounded, safeGet, type SafeResponse } from "@/lib/safe-fetch";

const SELF_PATH = "/api/tv/hls";
const KEY_ROLE = "key";
const KEY_TAG = /^#EXT-X-(?:SESSION-)?KEY:/;
const UPSTREAM_TIMEOUT_MS = 25_000;
const PLAYLIST_MAX_BYTES = 2 * 1024 * 1024;
const KEY_MAX_BYTES = 4096;
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

function keyReply(bytes: Buffer): NextResponse {
  return reply(bytes, 200, { "content-type": "application/octet-stream" });
}

function proxied(absUrl: string, role?: string): string {
  return `${SELF_PATH}?url=${encodeURIComponent(absUrl)}${role ? `&role=${role}` : ""}`;
}

/** Rewrite every URI line + URI="" attribute in an m3u8 to route through us, resolving each against the playlist's FINAL url; the URI of an `#EXT-X-KEY` carries `role=key`. */
function rewritePlaylist(text: string, base: URL): string {
  const resolve = (u: string) => new URL(u, base).toString();
  return text
    .split("\n")
    .map((line) => {
      const t = line.trim();
      if (!t) return line;
      if (t.startsWith("#")) {
        const role = KEY_TAG.test(t) ? KEY_ROLE : undefined;
        return line.replace(/URI="([^"]+)"/g, (_m, u) => `URI="${proxied(resolve(u), role)}"`);
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

async function respond(upstream: SafeResponse, isKey: boolean): Promise<NextResponse> {
  if (upstream.status < 200 || upstream.status > 299) {
    upstream.body.destroy();
    return reply(`upstream ${upstream.status}`, 502);
  }
  if (isKey) {
    const bytes = await readBounded(upstream, KEY_MAX_BYTES);
    return bytes.length > 0 ? keyReply(bytes) : reply("upstream key is empty", 502);
  }
  const ctype = String(upstream.headers["content-type"] ?? "").toLowerCase();
  const namedPlaylist = PLAYLIST_PATH.test(upstream.url.pathname);
  const labelledPlaylist = ctype.includes("mpegurl");
  if (namedPlaylist || labelledPlaylist) {
    const bytes = await readBounded(upstream, PLAYLIST_MAX_BYTES);
    const text = bytes.toString("utf8");
    if (text.trimStart().startsWith("#EXTM3U")) {
      try {
        return reply(rewritePlaylist(text, upstream.url), 200, { "content-type": "application/vnd.apple.mpegurl" });
      } catch {
        return reply("malformed upstream playlist", 502);
      }
    }
    if (labelledPlaylist && !namedPlaylist && bytes.length > 0 && bytes.length <= KEY_MAX_BYTES) return keyReply(bytes);
    return reply("upstream is not an HLS playlist", 415);
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
  if (contentCoding(upstream.headers) === "") {
    for (const h of ["content-range", "accept-ranges", "content-length"]) {
      const v = upstream.headers[h];
      if (typeof v === "string") segHeaders[h] = v;
    }
  }
  return reply(boundedBody(upstream, BODY_MAX_BYTES), upstream.status === 206 ? 206 : 200, segHeaders);
}

/** Same-origin HLS proxy so hls.js obeys the CSP `connect-src 'self'`: playlists are rewritten to point back here and segments stream through, but only public https hosts are reachable
 *  (`safeGet`), only `range` is forwarded, and bodies are size- and content-type-bounded (a gzip/deflate body an origin sends despite `accept-encoding: identity` is decoded under the same
 *  caps). A body is a playlist only if it starts with `#EXTM3U`: an encryption key, whether its rewritten URI says `role=key` or a server labels it as a playlist, is served as at most
 *  `KEY_MAX_BYTES` of `application/octet-stream`. */
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
    return await respond(await fetchUpstream(target, headers, req.signal), req.nextUrl.searchParams.get("role") === KEY_ROLE);
  } catch (e) {
    if (e instanceof UpstreamError) return reply(e.message, e.status);
    return reply("upstream error", 502);
  }
}
