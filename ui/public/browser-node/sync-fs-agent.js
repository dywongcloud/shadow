// Page half of node-worker's SYNCHRONOUS `fs` bridge
// (bn-node-worker-service-worker-sync-fs).
//
// node-worker's module resolver is synchronous end to end: a `require` inside
// the guest parks the worker thread and issues a BLOCKING XMLHttpRequest, which
// `dist/sw.js` answers — a service worker that relays the request to this page
// over a MessagePort and turns the reply into the response body. No service
// worker, no synchronous `fs`, no working `require`, nothing runs. That is the
// whole reason this file exists.
//
// The registration has to happen HERE, in a page. `navigator.serviceWorker` is
// `[Exposed=Window]`: it does not exist in the run-node SharedWorker, nor in
// the Web Locks fallback's dedicated Worker, so the worker that owns the
// browser node structurally cannot register anything — it brokers one through a
// connected page, exactly like the sqlite worker and the peer-mesh agent (see
// the header of public/run-node-worker.js).
//
// ## The scope, and why it is what it is
//
// sw.js is published at `/browser-node/node-worker/sw.js`, so its DEFAULT
// registration scope — the directory the script sits in — is
// `/browser-node/node-worker/`. That is the scope used here:
//
//   1. It is exactly the directory `worker.js` is served from. Measured
//      upstream across Blink, Gecko and WebKit (vendor/node-worker/src/lib/
//      sw.ts): what decides whether a *worker's* requests are intercepted is
//      whether the worker's own SCRIPT URL falls inside the registration
//      scope; whether the page is controlled is irrelevant. worker.js is in
//      that directory, so the blocking XHRs it issues — to
//      `/browser-node/node-worker/__nwm/v<proto>/<sid>/<seq>-<op>` — are
//      inside the scope and get intercepted.
//   2. It is deliberately NOT `/`. The dashboard already registers an
//      unrelated service worker at the origin root (public/sw.js, from
//      ui/components/pwa-register.tsx) and that one owns the app's offline
//      cache. Registering a second worker AT that scope would replace it. A
//      nested scope is legal, needs no `Service-Worker-Allowed` header — which
//      matters, because these are static assets and nothing here can set one —
//      and wins for in-scope clients by longest-prefix scope match while the
//      page keeps its existing controller. `navigator.serviceWorker
//      .getRegistration()` with no argument (ui/lib/push.ts) therefore still
//      resolves to the root worker for any page URL.
//   3. It is the narrowest scope that works. A worker at this scope sees only
//      requests under `/browser-node/node-worker/`, and its own fetch handler
//      passes through everything that is not a POST under `__nwm/` — so no
//      dashboard asset, API call or navigation is routed through it.
//
// ## The one invariant worth asserting
//
// A worker script outside the registration scope is not an error anywhere:
// the blocking XHR simply goes to the network, the thread stays parked, and
// the symptom is a hang inside a guest app rather than a message. So the
// worker URL is checked against the scope BEFORE registering (registering is a
// mutation of scope-wide state and must not be attempted for a scope that
// cannot work) and again against the scope the browser actually reports.
//
// ## Never take over someone else's registration
//
// Only ONE service worker can own a scope, and `register()` at a scope another
// script already owns does not fail — it replaces it. So an existing
// registration at this scope whose script is NOT our sw.js is refused here,
// named, with the upstream remedy (import `installNodeWorkerFetch` from
// dist/sw-handler.js inside that worker instead of registering ours). A
// registration at a BROADER scope — the root one — is not a conflict: nested
// registrations are legal and ours wins for its own clients.
//
// Nothing here ever unregisters. Once registered, the worker is inert with no
// session attached (its fetch handler passes everything through), and
// unregistering would race every other tab of this origin that is using it.

/** Scope-relative path segment the service worker claims.
 *
 * TWO IMPLEMENTATIONS OF ONE CONSTANT, and the drift is silent: this mirrors
 * `SW_PATH_SEGMENT` in vendor/node-worker/src/wire/sw.ts (compiled into
 * dist/sw.js). Disagree with it and every synchronous `fs` request misses the
 * fetch handler, which reads as a filesystem fault rather than as a build
 * skew. Kept here, next to the registration that has to agree with it. */
export const SYNC_FS_PATH_SEGMENT = "__nwm";

/** Polling, never `navigator.serviceWorker.ready`: `ready` resolves only for a
 *  registration whose scope contains THIS PAGE, and by design ours does not —
 *  awaiting it here would hang forever. */
const ACTIVE_POLL_MS = 50;
const ACTIVE_DEADLINE_MS = 10_000;

/** Every failure leaves this module as an Error whose message starts with
 *  `sync_fs_`: greppable in a donor's console and classifiable page-side,
 *  instead of a bare DOMException whose text is browser-specific. */
function syncFsError(reason, detail) {
  const error = new Error(`sync_fs_${reason}: ${detail}`);
  error.name = "SyncFsUnavailable";
  error.syncFsReason = reason;
  return error;
}

/** Which script owns a registration, whichever state its worker is in. */
function scriptOf(registration) {
  const worker = registration.active || registration.waiting || registration.installing;
  return worker ? worker.scriptURL : null;
}

/** Enough of the registration to tell a remote report apart: a bare "did not
 *  acknowledge" names a symptom shared by every cause. */
function describeRegistration(registration) {
  const state = (worker) => (worker ? worker.state : "none");
  return (
    `scope=${new URL(registration.scope).pathname}` +
    ` active=${state(registration.active)}` +
    ` waiting=${state(registration.waiting)}` +
    ` installing=${state(registration.installing)}` +
    ` script=${scriptOf(registration) ?? "none"}`
  );
}

function waitForActive(registration) {
  if (registration.active) return Promise.resolve(registration.active);
  return new Promise((resolve, reject) => {
    const deadline = setTimeout(() => {
      clearInterval(poll);
      reject(
        syncFsError(
          "inactive",
          `the service worker never became active within ${ACTIVE_DEADLINE_MS}ms (${describeRegistration(
            registration,
          )}) — a worker that installs but never activates answers no synchronous fs request`,
        ),
      );
    }, ACTIVE_DEADLINE_MS);
    const poll = setInterval(() => {
      if (!registration.active) return;
      clearInterval(poll);
      clearTimeout(deadline);
      resolve(registration.active);
    }, ACTIVE_POLL_MS);
  });
}

/**
 * Register node-worker's sw.js so the node worker gets synchronous `fs`.
 *
 * @param {object}  options
 * @param {string}  options.swUrl      URL of the deployed dist/sw.js.
 * @param {string}  options.workerUrl  URL of the deployed dist/worker.js — the
 *   client whose blocking requests must be intercepted. Asserted in scope.
 * @param {string} [options.scope]     Override; defaults to the directory
 *   `swUrl` sits in, which is the scope that covers `workerUrl`.
 * @returns {Promise<{scope: string, scriptURL: string, prefix: string, workerCovered: boolean}>}
 * @throws {Error} with a `sync_fs_<reason>:` message — never a bare DOMException.
 */
export async function registerNodeWorkerSyncFs(options = {}) {
  const { swUrl, workerUrl } = options;
  if (!swUrl) {
    throw syncFsError("no-sw-url", "registerNodeWorkerSyncFs needs `swUrl` (the deployed dist/sw.js)");
  }
  const container = typeof navigator !== "undefined" ? navigator.serviceWorker : undefined;
  if (!container) {
    throw syncFsError(
      "no-sw",
      "navigator.serviceWorker is unavailable in this context — service workers are registered from a page, and this browser removes the property outright in private windows",
    );
  }
  if (typeof isSecureContext === "boolean" && !isSecureContext) {
    throw syncFsError("insecure-context", "service workers require a secure context (https, or localhost)");
  }

  const scriptURL = new URL(swUrl, location.href).href;
  // Default scope = the directory sw.js sits in. No `Service-Worker-Allowed`
  // header is needed for a scope at or below the script's own directory, and
  // static hosting cannot set one.
  const scope = new URL(options.scope || new URL(".", scriptURL).href, location.href).href;
  const scopePath = new URL(scope).pathname;

  const worker = workerUrl ? new URL(workerUrl, location.href) : null;
  const checkWorkerInScope = () => {
    if (!worker) return;
    if (worker.origin !== new URL(scope).origin || !worker.pathname.startsWith(scopePath)) {
      throw syncFsError(
        "out-of-scope",
        `the node worker script ${worker.pathname} is outside the service worker scope ${scopePath}, so its synchronous fs requests would never be intercepted — serve sw.js from ${new URL(
          ".",
          worker,
        ).pathname} or widen the scope`,
      );
    }
  };
  checkWorkerInScope();

  // Exact scope match, never `getRegistration(scope)`: that call resolves by
  // longest-prefix, so with only the root worker registered it returns the
  // ROOT registration and would read as "our scope is taken by /sw.js".
  const existing = await container.getRegistrations().catch(() => []);
  const mine = existing.find((registration) => registration.scope === scope);
  if (mine) {
    const owner = scriptOf(mine);
    if (owner && owner !== scriptURL) {
      throw syncFsError(
        "scope-taken",
        `${scopePath} is already served by ${owner}; registering sw.js there would replace an unrelated service worker — import installNodeWorkerFetch from ${new URL(
          "./sw-handler.js",
          scriptURL,
        ).pathname} inside that worker instead`,
      );
    }
  }

  let registration = mine;
  if (!registration) {
    try {
      // `type: "classic"`, stated rather than defaulted: dist/sw.js is rolled
      // up as an IIFE because module service workers are not universally
      // shipped, and a future default flip must not silently change it.
      registration = await container.register(scriptURL, { scope, type: "classic" });
    } catch (error) {
      throw syncFsError(
        "register-failed",
        `navigator.serviceWorker.register(${new URL(scriptURL).pathname}, { scope: ${scopePath} }) failed: ${
          (error && error.message) || error
        }`,
      );
    }
  }

  const active = await waitForActive(registration);
  // Against the scope the browser actually reports, not the one requested:
  // `register` accepts a scope and can still not own it.
  checkWorkerInScope();

  return {
    scope: registration.scope,
    scriptURL: active.scriptURL || scriptURL,
    // What the node worker POSTs to. Mirrors dist/sw.js's own derivation
    // (`new URL(SW_PATH_SEGMENT + "/", registration.scope)`), and is only
    // diagnostic here — the authoritative prefix is the one the service
    // worker hands back in its `attached` reply.
    prefix: new URL(`${SYNC_FS_PATH_SEGMENT}/`, registration.scope).pathname,
    workerCovered: true,
  };
}
