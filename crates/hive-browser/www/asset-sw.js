const CACHE = "hive-browser-assets-v1";
const PREFIX = "/__hive_asset/";
const DIGEST = /^[0-9a-f]{64}$/;

function unsatisfied(size) {
  return new Response(null, {
    status: 416,
    headers: { "content-range": `bytes */${size}`, "accept-ranges": "bytes" },
  });
}

function parseRange(value, size) {
  const match = /^bytes=(\d*)-(\d*)$/.exec(value || "");
  if (!match || (match[1] === "" && match[2] === "")) return null;
  let start;
  let end;
  if (match[1] === "") {
    const suffix = Number(match[2]);
    if (!Number.isSafeInteger(suffix) || suffix <= 0) return null;
    start = Math.max(0, size - suffix);
    end = size - 1;
  } else {
    start = Number(match[1]);
    end = match[2] === "" ? size - 1 : Number(match[2]);
  }
  if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 0 || start >= size || end < start) return null;
  return { start, end: Math.min(end, size - 1) };
}

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", event => event.waitUntil(self.clients.claim()));
self.addEventListener("fetch", event => {
  const url = new URL(event.request.url);
  const digest = url.pathname.startsWith(PREFIX) ? url.pathname.slice(PREFIX.length) : "";
  if (!DIGEST.test(digest) || (event.request.method !== "GET" && event.request.method !== "HEAD")) return;
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    const stored = await cache.match(new Request(new URL(`${PREFIX}${digest}`, url.origin)));
    if (!stored) return new Response("asset not pinned locally", { status: 404 });
    const headers = new Headers(stored.headers);
    headers.set("accept-ranges", "bytes");
    const rangeValue = event.request.headers.get("range");
    if (!rangeValue) {
      return new Response(event.request.method === "HEAD" ? null : stored.body, {
        status: stored.status,
        statusText: stored.statusText,
        headers,
      });
    }
    const bytes = new Uint8Array(await stored.arrayBuffer());
    const range = parseRange(rangeValue, bytes.byteLength);
    if (!range) return unsatisfied(bytes.byteLength);
    const body = bytes.slice(range.start, range.end + 1);
    headers.set("content-range", `bytes ${range.start}-${range.end}/${bytes.byteLength}`);
    headers.set("content-length", String(body.byteLength));
    return new Response(event.request.method === "HEAD" ? null : body, { status: 206, headers });
  })());
});
