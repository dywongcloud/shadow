// Worker-side end of the puterfs change-notification feed.
//
// The socket itself lives in the page (src/lib/fsevents.ts); this fans what it
// reports out to whoever is watching. The feed is opened lazily on the first
// subscribe and closed when the last subscriber leaves, so a program that never
// calls `fs.watch` never opens a socket.
//
// It used to own a `MessagePort`, and the port was the bug: a worker parked inside
// a blocking XHR never reads one, so a watcher saw nothing for the whole duration
// of a synchronous loop. Events are messages now — they ride the reply the worker
// is already waiting for — which is why `emitLocalFsEvent` and the remote feed
// finally arrive by the same route.
//
// Deliberately does NOT touch keepalive: the channel is plumbing, not an active
// handle. Watchers ref (see node/fs/watch.ts) — if this reffed too, every run
// that so much as stat'd a file would hang waiting for a socket nobody is
// listening to.

import { KIND_EVENTS } from "../wire/kinds";
import { makeDispatcher } from "../wire/router";
import type { EventsCall, EventsResult, PuterFsEvent } from "../wire/events";
import { console_warn } from "./console";
import { API_ORIGIN } from "./puter";
import { PUTER_TOKEN } from "./state";
import { call, wire } from "./wire";

export type { PuterFsEvent };

type Handler = (event: PuterFsEvent) => void;

let handlers = new Set<Handler>();
let subscribed = false;
let opening: Promise<void> | undefined;
let connected = false;
let covered = false;
let stateHandlers = new Set<(covered: boolean) => void>();
let staleHandlers = new Set<() => void>();

/** Whether the page currently has a live socket. */
export function fsEventsConnected(): boolean {
	return connected;
}

/**
 * Whether *anything* is telling us about changes — a live socket, or the page
 * polling puterfs's change counter on our behalf.
 *
 * The distinction `watchFile` needs is not "is there a socket" but "is there any
 * signal at all", since the coarse one still tells it when to re-stat. Only when
 * both are gone does node's own interval have to earn its keep.
 */
export function fsEventsCovered(): boolean {
	return covered;
}

/**
 * Watch whether anything is reporting changes. See `fsEventsCovered`.
 */
export function onFsEventsState(fn: (covered: boolean) => void): () => void {
	stateHandlers.add(fn);
	return () => stateHandlers.delete(fn);
}

/**
 * "Something changed and nobody can tell you what."
 *
 * The page falls back to polling puterfs's change counter whenever the socket is
 * not delivering — which, for an app launched with an app token rather than the
 * user's, is always: the socket's handshake middleware refuses anything but a
 * user token. The counter carries no path, so this carries none either.
 *
 * A watcher answers it by re-checking what it watches. That is the whole reason
 * `fs.watch` works on such a launch at all; before this it saw nothing.
 */
export function onFsEventsStale(fn: () => void): () => void {
	staleHandlers.add(fn);
	return () => staleHandlers.delete(fn);
}

function setState(isConnected: boolean, isPolling: boolean) {
	connected = isConnected;
	let next = isConnected || isPolling;
	if (covered === next) return;
	covered = next;
	for (let fn of [...stateHandlers]) {
		try {
			fn(next);
		} catch (err) {
			console_warn("[node-worker] [fs-events] state handler threw", err);
		}
	}
}

// Registered once, at module scope, because an event may arrive on the reply to a
// message this worker is already parked on — there is no later moment at which to
// start listening.
wire.router.register(
	KIND_EVENTS,
	makeDispatcher<EventsCall>(KIND_EVENTS, async (msg) => {
		if (msg.op === "ev.fs") {
			dispatch(msg.event);
			return;
		}
		if (msg.op === "ev.state") {
			setState(msg.connected, msg.polling);
			return;
		}
		if (msg.op === "ev.stale") {
			dispatchStale();
			return;
		}
		if (msg.op === "ev.error") {
			// A fatal error means the token was rejected at the handshake and no
			// retry will help. Watchers stay alive and keep reporting local
			// mutations; they just won't see changes made elsewhere.
			console_warn(`[node-worker] [fs-events] ${msg.message}`);
			// Not `setState(false, false)`: a refused socket is exactly when the page
			// starts polling instead, and it says so in the `state` that follows.
			if (msg.fatal) connected = false;
		}
	})
);

function dispatch(event: PuterFsEvent) {
	// Snapshot: a handler closing its watcher mid-dispatch mutates the set.
	for (let fn of [...handlers]) {
		try {
			fn(event);
		} catch (err) {
			console_warn("[node-worker] [fs-events] handler threw", err);
		}
	}
}

function dispatchStale() {
	for (let fn of [...staleHandlers]) {
		try {
			fn();
		} catch (err) {
			console_warn("[node-worker] [fs-events] stale handler threw", err);
		}
	}
}

function ensureFeed(): Promise<void> {
	if (subscribed) return Promise.resolve();
	if (opening) return opening;

	opening = (async () => {
		let { value } = await call<EventsResult<"ev.subscribe">>(KIND_EVENTS, {
			op: "ev.subscribe",
			token: PUTER_TOKEN,
			apiOrigin: API_ORIGIN,
		});
		// Everyone may have unsubscribed while the round trip was in flight.
		if (handlers.size === 0) {
			wire.post(KIND_EVENTS, { op: "ev.close" });
			return;
		}
		subscribed = true;
		setState(value.connected, value.polling);
	})().finally(() => {
		opening = undefined;
	});

	return opening;
}

function closeFeed() {
	if (!subscribed) return;
	subscribed = false;
	wire.post(KIND_EVENTS, { op: "ev.close" });
	setState(false, false);
}

/**
 * Subscribe to puterfs mutations. Returns an unsubscribe function; when the
 * last subscriber unsubscribes the channel is torn down.
 */
export function subscribeFsEvents(fn: Handler): () => void {
	handlers.add(fn);
	ensureFeed().catch((err) => {
		console_warn("[node-worker] [fs-events] failed to open the feed", err);
	});

	let done = false;
	return () => {
		if (done) return;
		done = true;
		handlers.delete(fn);
		if (handlers.size === 0) closeFeed();
	};
}

/**
 * Report a mutation this worker just performed, without waiting for the api to
 * echo it back over the socket.
 *
 * Echoes are deliberately NOT deduplicated. The local event is the fast,
 * approximate signal (we don't stat before writing, so a file creation is
 * reported as `updated`); the socket echo that follows carries the api's own
 * classification. node's `fs.watch` is documented as coalescing and
 * occasionally double-reporting, and every real consumer (chokidar and friends)
 * re-stats and dedupes anyway — so an extra event is cheap and a missing one is
 * not.
 */
export function emitLocalFsEvent(event: PuterFsEvent): void {
	if (handlers.size === 0) return;
	dispatch(event);
}
