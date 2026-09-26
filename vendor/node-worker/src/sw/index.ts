// The shipped service worker: `dist/sw.js`.
//
// A consumer who already owns their scope should import `installNodeWorkerFetch` from
// `node-worker/sw-handler` into their own worker instead — only one service worker can own a
// scope, and this one claims the whole thing.

import { installNodeWorkerFetch } from "./handler";

const sw = self as unknown as ServiceWorkerGlobalScope;

installNodeWorkerFetch();

// `skipWaiting` on *install* only, which is the case where there is nothing to displace: a
// first registration has no existing worker, so taking over immediately is free and saves the
// page a reload before synchronous filesystem access works.
//
// Deliberately NOT on update. A waiting worker takes over once its clients are gone, and
// forcing it in mid-run would swap the transport under a worker that is parked in a blocking
// request. Version skew is detected instead: the protocol version rides in both the handshake
// and the request URL, and a mismatch is answered with a legible error rather than a body
// neither side agrees on.
sw.addEventListener("install", (event: ExtendableEvent) => {
	if (!sw.registration.active) event.waitUntil(sw.skipWaiting());
});

// Claiming is not required for interception — measured across Blink, Gecko and WebKit, a
// dedicated worker is controlled because its own script URL is in scope, and the page needs no
// control at all. It is done anyway because it costs nothing and makes a page-side `fetch`
// probe possible; nothing in the design depends on it.
sw.addEventListener("activate", (event: ExtendableEvent) => {
	event.waitUntil(sw.clients.claim());
});
