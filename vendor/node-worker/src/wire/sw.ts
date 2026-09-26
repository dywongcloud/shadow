// The service worker's side channel to the page.
//
// The service worker cannot answer a request itself — it has no mount table, no
// providers and no stdio — so it relays: a blocking XHR from the node worker arrives as
// a `fetch` event, the SW hands the message to the page over a `MessagePort`, the page
// runs the (async) operation, and the answering message becomes the XHR's response
// body. The node worker's thread is parked inside `send()` for the whole trip, which is
// the point.
//
// The relay never decodes what it carries and never reads the kind. It is a byte pipe
// with a session registry, and routing a new kind through it costs nothing here.
//
// Two browser facts shape every message below, and both are easy to get wrong:
//
//   1. **`MessagePort.postMessage` does not start a stopped service worker;
//      `ServiceWorker.postMessage` does.** So the page always *attaches* through
//      `registration.active.postMessage`, never over a port it happens to be holding.
//      A message sent to a port whose SW instance has been killed is simply lost.
//   2. **A service worker is evicted when idle (~30s) and loses all module state.** Its
//      session registry is therefore a cache, never the truth, and arriving at a cold
//      worker is the *normal* path rather than an edge case. `Rescue` is how it gets
//      its port back, and only a window client can answer it.

/**
 * Identifies one worker's session to the service worker and to the host.
 *
 * Opaque, and deliberately so. It was once documented as `<pageClientId>.<nonce>` on the
 * premise that a restarted SW could `clients.get()` the owner directly — but a page cannot
 * know its own client id until it has already attached, so no sid ever had that shape and
 * the fast path built on it never fired. Rescue asks every window instead; see
 * `sw/handler.ts`.
 */
export type SessionId = string;

/** The scope-relative path segment the service worker claims. */
export const SW_PATH_SEGMENT = "__nwm";

/**
 * Page → service worker, over `registration.active.postMessage`, with the page's end of
 * a fresh `MessageChannel` in the transfer list.
 *
 * Sent at startup and again on every `Rescue` and `controllerchange`. The SW always
 * prefers the newest port: from the page's side a stale port is indistinguishable from a
 * live one, so it must be free to mint a replacement at any time.
 */
export interface SwAttach {
	t: "attach";
	sid: SessionId;
	proto: number;
}

/** Service worker → page, over the attach port, once the session is registered. */
export interface SwAttached {
	t: "attached";
	/** The URL prefix the node worker must POST to, derived from `registration.scope`. */
	prefix: string;
	proto: number;
	/** The client id the SW resolved this session to. Diagnostics only. */
	clientId?: string;
}

/** Page → service worker: this session is finished (`pagehide`, `terminate()`). */
export interface SwDetach {
	t: "detach";
	sid: SessionId;
}

/** Service worker → page: run this request. */
export interface SwOp {
	t: "op";
	seq: number;
	frame: ArrayBuffer;
}

/** Page → service worker: here is the answer. */
export interface SwRes {
	t: "res";
	seq: number;
	frame: ArrayBuffer;
}

/**
 * Page → service worker: this op is still running, don't time it out.
 *
 * Needed because the SW's deadline has to be short enough to be useful and a genuine
 * operation can be slow — a large write into OPFS is not a hung page, a `spawnSync` may
 * legitimately take minutes, and a blocking `io.read` waits exactly as long as the
 * person at the keyboard does. Without this the 15s ceiling below is absolute, which is
 * what it was for as long as nothing sent one of these.
 */
export interface SwProgress {
	t: "progress";
	seq: number;
}

/**
 * Service worker → page: the node worker gave up on this op.
 *
 * The page aborts its controller rather than completing a mutation nobody is waiting
 * for. Reached when the worker's own `xhr.timeout` fires and it posts an abandon
 * request.
 */
export interface SwAbandon {
	t: "abandon";
	seq: number;
}

/**
 * Service worker → **window client** (via `client.postMessage`, not a port): the SW
 * restarted and has no port for this session.
 *
 * The page answers with a fresh {@link SwAttach}. Delivered to every window client and by
 * broadcast — see the notes at the top of this file for why the SW has to be the one to
 * initiate, and `sw/handler.ts` for why there is no direct lookup.
 */
export interface SwRescue {
	t: "rescue";
	sid: SessionId;
}

export type SwToPage = SwAttached | SwOp | SwAbandon;
export type PageToSw = SwAttach | SwDetach | SwRes | SwProgress;

/** Carries a rescue to same-origin pages regardless of control or scope. */
export const SW_BROADCAST_CHANNEL = "node-worker-wire";

/** ms the service worker waits for a page to re-attach after a restart. */
export const RESCUE_TIMEOUT_MS = 1500;

/**
 * ms the service worker waits for an answer before failing the request.
 *
 * Refreshed by every {@link SwProgress}, so this is a liveness deadline rather than a
 * limit on how long an operation may take. Kept well inside the browsers' own
 * fetch-event ceilings (Chrome ~5 min, Firefox ~300s) so the failure is ours and legible
 * rather than theirs and opaque.
 */
export const OP_TIMEOUT_MS = 15_000;

/**
 * ms between heartbeats while an op is outstanding.
 *
 * A third of the deadline: two may be lost — to a page busy on the main thread, or to a
 * `postMessage` arriving late — without the SW concluding the page is gone.
 */
export const PROGRESS_INTERVAL_MS = OP_TIMEOUT_MS / 3;

/**
 * ms the *worker* waits, as the backstop.
 *
 * Longer than `OP_TIMEOUT_MS` on purpose: the SW's own timeout produces a `504` with a
 * readable message, so it should normally win the race. This one exists for the case
 * where there is no service worker left to produce anything — and it is the only limit
 * that holds then, because nothing else in the worker can run while the thread is
 * parked. (`xhr.timeout` is settable on a synchronous request in a worker; the spec only
 * forbids it when the global is a `Window`.)
 *
 * An op that may legitimately park for as long as a person is willing to wait — a
 * blocking `io.read` at a prompt — opts out with 0, and is then bounded only by the
 * heartbeat above failing.
 */
export const SYNC_TIMEOUT_MS = 20_000;

/** HTTP statuses the service worker answers with. 200 means "an op ran", success or not. */
export const SW_STATUS = {
	/** The message carries the result, including an in-band ENOENT. */
	ok: 200,
	/** Protocol skew between the page and the service worker. */
	protoMismatch: 400,
	/** No session attached, rescue failed, or the page detached. */
	noSession: 503,
	/** The host did not answer in time. */
	timeout: 504,
} as const;
