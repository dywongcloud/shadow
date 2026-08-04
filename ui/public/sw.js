/* shadw service worker — makes the app installable + resilient offline.
 *
 * Deliberately conservative: API / proxy / auth requests are NEVER cached
 * (always hit the network), navigations AND app code (JS/CSS) are network-first
 * — so the latest build always loads and a stale cached bundle can never pin an
 * old UI (which previously left mobile showing an outdated dashboard). The ONE
 * exception is the versioned browser-node pkg URLs (/browser-node/pkg/*?v=N),
 * which are immutable per version by contract and therefore cache-first (see
 * below). Only media/fonts are cached stale-while-revalidate. Cache is the
 * offline fallback. Bump VERSION to evict every prior cache on activate. */

const VERSION = "shadw-v5";
const STATIC_CACHE = `shadw-static-${VERSION}`;
const PRECACHE = [
  "/offline.html",
  "/web-app-manifest-192x192.png",
  "/shadw-logo-dark.png",
];

/* No skipWaiting: app code is network-first above, so a waiting SW is harmless
 * to live tabs and instant takeover buys nothing — while takeover mid-session
 * fires controllerchange in EVERY open tab, which is the raw material of the
 * mobile reload-loop incident. The updated SW activates when the last tab
 * closes. */
self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(STATIC_CACHE).then((c) => c.addAll(PRECACHE)));
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

  // Versioned browser-node pkg URLs (/browser-node/pkg/*?v=WASM_BUNDLE_VERSION):
  // CACHE-FIRST. The ?v= query IS the cache key — bytes are immutable per
  // version by contract, so staleness is caught by the version bump itself
  // (that is what wasm_bundle_version exists for), backstopped by the worker's
  // live wasmBundleVersion() check after init. Network-first here made Safari
  // re-download the 4.4MB hive_browser_bg.wasm on EVERY page load (witnessed
  // bn-safari-wasm-refetch-cost: transferSize 4382386, 200 not 304, while the
  // JS glue cached fine) — every Safari donor paying ~4.4MB per page view.
  // Network-first only ever bought protection against same-URL content
  // changes, which the version contract forbids. A version bump is a new URL,
  // hence a cache miss, hence fresh bytes.
  const isVersionedPkg = url.pathname.startsWith("/browser-node/pkg/") && url.searchParams.has("v");
  if (isVersionedPkg) {
    event.respondWith(
      caches.open(STATIC_CACHE).then(async (cache) => {
        const cached = await cache.match(req);
        if (cached) return cached;
        try {
          const res = await fetch(req);
          // Same poison guard as the network-first branch: never cache a
          // redirect-bounced HTML document under an asset URL.
          if (res && res.status === 200 && res.type === "basic" && !res.redirected) {
            cache.put(req, res.clone());
          }
          return res;
        } catch {
          return Response.error();
        }
      })
    );
    return;
  }

  // App CODE (JS/CSS/source-maps/WASM): NETWORK-FIRST. The freshest build always
  // wins when online; the cache is only an offline fallback. This stops a stale
  // cached bundle from rendering an outdated UI when chunk URLs aren't
  // content-hashed (e.g. on the dev/preview server). .wasm is explicitly
  // included here (bn-ui-pwa-install-offline: a prior pass claimed this file
  // already covered install/offline/caching for /run-node, but the wasm bundle
  // -- the actual thing "Run a node" cannot function without -- previously
  // matched NEITHER this branch nor the media one below, so it got no caching
  // or offline-fallback treatment at all). NOTE: the versioned browser-node
  // pkg URLs never reach this branch — they are cache-first above; this is
  // network-first for everything else (unversioned chunks whose bytes CAN
  // change under the same URL).
  const isCode = /\.(?:js|css|map|wasm)$/.test(url.pathname);
  if (isCode) {
    event.respondWith(
      caches.open(STATIC_CACHE).then(async (cache) => {
        try {
          // `{cache:'reload'}` is load-bearing: a plain `fetch(req)` still
          // honors the browser's own HTTP cache, so a code chunk with a live
          // `max-age` (e.g. the /wfc console assets) is served from that HTTP
          // cache WITHOUT a network round-trip — which means one chunk can be
          // served from an OLD cached build while a sibling chunk of the SAME
          // page loads fresh, producing a mixed module graph and the fatal
          // "requested module does not provide an export named X" crash the
          // workflows console hit. `reload` forces a real revalidated network
          // fetch every time, so every chunk of a page load comes from the
          // SAME (internally-consistent) origin build. Bandwidth cost is a
          // conditional request per chunk; ETag/304 keeps it cheap.
          const res = await fetch(req, { cache: "reload" });
          // `!res.redirected` guard: a 200 that arrived via a redirect (e.g.
          // an auth bounce to a sign-in page) is NOT the asset — caching that
          // HTML body under the .css/.js URL would poison the offline
          // fallback with a MIME-mismatched document.
          if (res && res.status === 200 && res.type === "basic" && !res.redirected) {
            cache.put(req, res.clone());
          }
          return res;
        } catch {
          const cached = await cache.match(req);
          return cached || Response.error();
        }
      })
    );
    return;
  }

  // Media / fonts: stale-while-revalidate (safe to serve fast, refresh in bg).
  const isMedia =
    url.pathname.startsWith("/fonts/") ||
    /\.(?:png|svg|jpg|jpeg|webp|gif|ico|woff2?)$/.test(url.pathname);
  if (isMedia) {
    event.respondWith(
      caches.open(STATIC_CACHE).then(async (cache) => {
        const cached = await cache.match(req);
        const network = fetch(req)
          .then((res) => {
            if (res && res.status === 200 && res.type === "basic" && !res.redirected) {
              cache.put(req, res.clone());
            }
            return res;
          })
          .catch(() => cached);
        return cached || network;
      })
    );
  }
});

/* ---- Web Push ----
 * Payload (JSON from the backend push pipeline):
 *   {title, body, severity, category, project, url, id, ts_ms}
 * `tag: id` dedupes re-delivery of the same notification (a re-pushed id
 * replaces the shown notification instead of stacking a duplicate). */
self.addEventListener("push", (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch {
    // Non-JSON payload (should never happen from our pipeline) — show as-is.
    payload = { body: event.data ? event.data.text() : "" };
  }
  const title = payload.title || "shadw";
  event.waitUntil(
    self.registration.showNotification(title, {
      body: payload.body || "",
      icon: "/icon.png", // app-router icon route (ui/app/icon.png)
      tag: payload.id || undefined,
      data: { url: payload.url || "/" },
    })
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const url = (event.notification.data && event.notification.data.url) || "/";
  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((wins) => {
        // Prefer focusing an already-open dashboard tab (navigated to the
        // notification's target) over spawning a new window every click.
        for (const client of wins) {
          if ("focus" in client) {
            return client
              .focus()
              .then((focused) => ((focused || client).navigate ? (focused || client).navigate(url) : undefined))
              .catch(() => self.clients.openWindow(url));
          }
        }
        return self.clients.openWindow(url);
      })
      .catch(() => self.clients.openWindow(url))
  );
});
