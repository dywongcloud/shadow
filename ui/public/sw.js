/* shadw service worker — makes the app installable + resilient offline.
 *
 * Deliberately conservative: API / proxy / auth requests are NEVER cached
 * (always hit the network), navigations are network-first with an offline
 * fallback, and only static assets are cached (stale-while-revalidate). */

const VERSION = "shadw-v1";
const STATIC_CACHE = `shadw-static-${VERSION}`;
const PRECACHE = [
  "/offline.html",
  "/web-app-manifest-192x192.png",
  "/shadw-logo-dark.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(STATIC_CACHE).then((c) => c.addAll(PRECACHE)).then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== STATIC_CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;

  let url;
  try {
    url = new URL(req.url);
  } catch {
    return;
  }
  if (url.origin !== self.location.origin) return;

  // Never cache dynamic / authenticated traffic — pass straight through.
  if (url.pathname.startsWith("/cloud") || url.pathname.startsWith("/api")) return;

  // Page navigations: network-first, fall back to cache, then the offline page.
  if (req.mode === "navigate") {
    event.respondWith(
      fetch(req).catch(() => caches.match(req).then((c) => c || caches.match("/offline.html")))
    );
    return;
  }

  // Static assets: serve from cache fast, refresh in the background.
  const isStatic =
    url.pathname.startsWith("/_next/static/") ||
    url.pathname.startsWith("/fonts/") ||
    /\.(?:png|svg|jpg|jpeg|webp|gif|ico|woff2?|css|js|map)$/.test(url.pathname);
  if (isStatic) {
    event.respondWith(
      caches.open(STATIC_CACHE).then(async (cache) => {
        const cached = await cache.match(req);
        const network = fetch(req)
          .then((res) => {
            if (res && res.status === 200 && res.type === "basic") cache.put(req, res.clone());
            return res;
          })
          .catch(() => cached);
        return cached || network;
      })
    );
  }
});
