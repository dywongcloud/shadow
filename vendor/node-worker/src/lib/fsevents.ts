// Page-side socket.io client for puterfs change notifications.
//
// Puter has no filesystem-watch api, but the backend already pushes
// `item.added` / `item.updated` / `item.removed` / `item.moved` over socket.io
// to every socket joined to the authenticated user's room — the same stream
// puter.js consumes for cache invalidation. That is what backs `fs.watch` in
// the worker.
//
// This lives in the page rather than the worker for three reasons:
//   - the worker's `globalThis.WebSocket` is epoxy's WISP-tunnelled override
//     (worker/epoxy/globals.ts), so a page-side socket is a plain browser one;
//   - one connection is shared by every watcher in every worker on the token,
//     and it survives `execute` boundaries;
//   - it sits outside worker/keepalive.ts accounting, so the socket by itself
//     can never hold a run open — only watchers ref.
//
// The protocol is hand-rolled instead of pulling in socket.io-client: the
// backend is socket.io@4.8 / engine.io@6.6 with `allowEIO3` unset, we only ever
// receive server-to-client events, and websocket-only means none of the polling
// transport or upgrade machinery is reachable.
//
// ## The timestamp fallback
//
// The socket is not always reachable, and the reason is structural rather than
// flaky: the backend's handshake middleware rejects app actors and access-token
// actors outright ("only user tokens accepted"), and puter's launcher hands a
// launched app the *user's* token only in godmode — otherwise it gets an app
// token. On such a launch nothing here ever connects, and until now that meant
// `fs.watch` was silently dead and a filesystem cache had no invalidation source
// at all.
//
// So there is a second, coarse source: `GET /cache/last-change-timestamp`, a
// per-user counter the backend bumps on every `item.*` mutation. It is what
// puter-js polls, it answers for an app actor (it keys off the actor's user),
// and it says only "something changed" — no path, no kind. That is enough to
// invalidate a cache and enough to make a watcher re-check, so it is broadcast
// as `stale` and each consumer decides what revalidating means for it.
//
// Polled only while the socket is *not* delivering, since a live socket is
// strictly better. Both edges of that transition also emit `stale`: a drop and a
// reconnect each leave a window whose events nobody received.

import type { EventsCall, PuterFsEvent } from "../wire/events";

// engine.io packet types (first character of a frame).
const EIO_OPEN = "0";
const EIO_CLOSE = "1";
const EIO_PING = "2";
const EIO_PONG = "3";
const EIO_MESSAGE = "4";

// socket.io packet types (first character of an engine.io MESSAGE payload).
const SIO_CONNECT = "0";
const SIO_DISCONNECT = "1";
const SIO_EVENT = "2";
const SIO_CONNECT_ERROR = "4";

// socket.io-client's own reconnection defaults; there is no reason to be more
// aggressive than the client the backend was built against.
const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 5000;
const RECONNECT_JITTER = 0.5;

/**
 * How often the timestamp endpoint is polled while the socket is down.
 *
 * One small GET, and it replaces what `watchFile` used to do per watched file
 * (a stat each, every 5007 ms) — so this is fewer requests than the fallback it
 * supersedes, not more. It also bounds how stale a cached filesystem answer can
 * be, which is why it is not slower.
 */
const POLL_INTERVAL_MS = 2000;

interface EngineHandshake {
	sid: string;
	pingInterval: number;
	pingTimeout: number;
}

function coerceBool(value: unknown): boolean {
	// The api reports `is_dir` as a boolean on the v2 routes and as 1|0 on the
	// legacy ones, and every `item.*` payload comes from the legacy projection.
	return value === true || value === 1 || value === "1";
}

// Turns one `item.*` payload into the normalized shape the worker consumes.
// Returns null for events that carry no usable path, and for `item.pending`,
// which announces an upload that has not landed yet — the completing write
// emits `item.added`/`item.updated` immediately after, so forwarding both would
// report a file that does not exist yet.
function normalize(name: string, data: any): PuterFsEvent | null {
	if (!data || typeof data.path !== "string" || !data.path) return null;
	let isDir = coerceBool(data.is_dir ?? data.isDir);
	// The legacy projection carries all three spellings of the same value; a
	// cache keys on it to notice that an entry it holds under some other path is
	// the one that just moved. See `PuterFsEvent.uid`.
	let raw = data.uid ?? data.uuid ?? data.id;
	let uid = typeof raw === "string" && raw ? raw : undefined;

	switch (name) {
		case "item.added":
			return { kind: "added", path: data.path, isDir, uid };
		case "item.updated":
			return { kind: "updated", path: data.path, isDir, uid };
		case "item.removed":
			return {
				kind: "removed",
				path: data.path,
				isDir,
				uid,
				descendantsOnly: coerceBool(data.descendants_only),
			};
		case "item.moved": {
			// The new FSController emits `from_path`; LegacyFSController and
			// WebDAVController emit `old_path`. Both are live.
			let oldPath = data.from_path ?? data.old_path;
			return {
				kind: "moved",
				path: data.path,
				isDir,
				uid,
				oldPath: typeof oldPath === "string" ? oldPath : undefined,
			};
		}
		// `item.renamed` is deliberately absent: puter-js and the desktop both
		// listen for it, but no backend path has ever emitted it — rename goes
		// out as `item.updated`, naming only the *new* path. That is why `uid` is
		// carried above: it is the only thing tying the two together.
		default:
			return null;
	}
}

// Splits an engine.io MESSAGE payload into its socket.io type and JSON body.
// Wire shape is `<type>[<namespace>,][<ackId>]<json>`; we speak only the
// default namespace and never send an ack-requiring event, but a well-behaved
// parser has to skip both fields rather than assume they are absent.
function parseSocketIoPacket(payload: string): { type: string; body: any } {
	let type = payload[0];
	let rest = payload.slice(1);

	if (rest.startsWith("/")) {
		let comma = rest.indexOf(",");
		rest = comma === -1 ? "" : rest.slice(comma + 1);
	}

	let digits = 0;
	while (digits < rest.length && rest[digits] >= "0" && rest[digits] <= "9") {
		digits++;
	}
	rest = rest.slice(digits);

	let body: any = undefined;
	if (rest.length > 0) {
		try {
			body = JSON.parse(rest);
		} catch {
			body = undefined;
		}
	}

	return { type, body };
}

class FsEventsHub {
	#token?: string;
	#url: string;
	#pollUrl: string;
	/**
	 * Worker-bound sinks. Each one posts a push message onto that worker's wire.
	 *
	 * These used to be `MessagePort`s, and the port was the flaw: a worker parked inside a
	 * blocking call never reads one, so a watcher went deaf for the whole of a synchronous
	 * loop. A push can ride the reply the worker is already waiting for.
	 */
	#sinks = new Set<(msg: EventsCall) => void>();
	/** In-page consumers, which need no message at all to reach. */
	#listeners = new Set<(msg: EventsCall) => void>();

	#ws: WebSocket | undefined;
	#connected = false;
	#attempt = 0;
	#reconnectTimer: ReturnType<typeof setTimeout> | undefined;
	#watchdog: ReturnType<typeof setTimeout> | undefined;
	#pingInterval = 25000;
	#pingTimeout = 20000;
	/** Set when the token itself is bad, which no amount of retrying fixes. */
	#dead = false;
	#onEmpty: () => void;

	#pollTimer: ReturnType<typeof setInterval> | undefined;
	/** The server's last-change counter as of the previous poll; undefined until the first. */
	#lastChange: number | undefined;
	/** In-flight poll, shared so a burst of `ensureFresh` callers costs one request. */
	#polling: Promise<void> | undefined;
	/**
	 * Whether the counter is actually answering.
	 *
	 * Reported to consumers so a `watchFile` can tell "coarsely covered" from
	 * "covered by nothing", which is the only state where its own stat loop is
	 * worth what it costs.
	 */
	#pollHealthy = false;
	/**
	 * Local time at which this hub last knew it was in step with the server.
	 *
	 * `Date.now()` whenever the socket is delivering, and the time of the last
	 * successful poll otherwise. A consumer holding cached state may trust it for
	 * as long as it is willing to be this far behind — the window of unreported
	 * change is exactly `Date.now() - checkedAt`.
	 */
	#checkedAt = 0;

	constructor(
		token: string | undefined,
		apiOrigin: string,
		onEmpty: () => void
	) {
		this.#token = token;
		this.#onEmpty = onEmpty;

		let url = new URL("/socket.io/", apiOrigin);
		url.protocol = url.protocol === "http:" ? "ws:" : "wss:";
		url.searchParams.set("EIO", "4");
		url.searchParams.set("transport", "websocket");
		this.#url = url.toString();

		// A GET carrying the token as a query parameter, so it stays a CORS-simple
		// request and costs no preflight — the same reasoning as `PuterApi.#url`.
		let poll = new URL("/cache/last-change-timestamp", apiOrigin);
		if (token) poll.searchParams.set("auth_token", token);
		this.#pollUrl = poll.toString();
	}

	/**
	 * Hands the worker one end of a channel fed by this hub, and this side the means to
	 * take it back.
	 *
	 * Detaching used to be the worker's move alone — it sends `{type:"close"}` when its
	 * last watcher goes away. A *terminated* worker sends nothing, so its port stayed in
	 * `#ports` forever, and since the socket lives exactly as long as that set is
	 * non-empty, one terminated worker with a watcher kept a socket.io connection open for
	 * the life of the page. `NodeWorker.terminate` calls the returned `close`.
	 */
	attach(sink: (msg: EventsCall) => void): FsEventsFeed {
		this.#sinks.add(sink);
		if (this.#dead) {
			this.#post(sink, {
				op: "ev.error",
				message: "fs events unavailable: socket auth rejected",
				fatal: true,
			});
		}

		this.#onAttached();
		// Idempotent: detaching a sink that has already gone is a no-op, so this is safe to
		// call after the worker closed the feed itself.
		return {
			connected: this.#connected,
			polling: !this.#connected && this.#pollHealthy,
			close: () => this.#detach(sink),
		};
	}

	/**
	 * The same feed, for a consumer living in this page.
	 *
	 * The filesystem cache is one, and routing it through a `MessageChannel` would
	 * mean serializing every event to reach an object three modules away. It refs
	 * the hub exactly as `attach` does: the socket lives as long as *anything*
	 * wants events, and a cache wants them for the whole session rather than only
	 * while something calls `fs.watch`.
	 */
	subscribe(fn: (msg: EventsCall) => void): () => void {
		this.#listeners.add(fn);
		fn(this.#state());
		this.#onAttached();
		return () => {
			if (!this.#listeners.delete(fn)) return;
			if (this.#empty) this.close();
		};
	}

	get #empty(): boolean {
		return this.#sinks.size === 0 && this.#listeners.size === 0;
	}

	#onAttached() {
		if (!this.#dead) this.#connect();
		// A `#dead` hub never connects, so polling is the only signal it will ever
		// have — start it here rather than only on a disconnect.
		this.#syncPolling();
	}

	#detach(sink: (msg: EventsCall) => void) {
		if (!this.#sinks.delete(sink)) return;
		if (this.#empty) this.close();
	}

	close() {
		this.#clearTimers();
		this.#sinks.clear();
		this.#listeners.clear();
		this.#teardownSocket();
		this.#onEmpty();
	}

	/** Push an event this page produced, rather than one the socket delivered. */
	inject(event: PuterFsEvent) {
		this.#broadcast({ op: "ev.fs", event });
	}

	get connected(): boolean {
		return this.#connected;
	}

	/** See `#checkedAt`. */
	freshAsOf(): number {
		return this.#connected ? Date.now() : this.#checkedAt;
	}

	/**
	 * Bring `freshAsOf()` within `maxAgeMs` of now, if it isn't already.
	 *
	 * A cache calls this before trusting itself. Concurrent callers share the one
	 * request, so a burst of cached reads costs a single small GET — which is the
	 * whole point of a coarse signal.
	 */
	async ensureFresh(maxAgeMs: number): Promise<void> {
		if (Date.now() - this.freshAsOf() <= maxAgeMs) return;
		await this.#poll();
	}

	#state(): EventsCall {
		return {
			op: "ev.state",
			connected: this.#connected,
			polling: !this.#connected && this.#pollHealthy,
		};
	}

	#setPollHealthy(healthy: boolean) {
		if (this.#pollHealthy === healthy) return;
		this.#pollHealthy = healthy;
		this.#broadcast(this.#state());
	}

	#post(sink: (msg: EventsCall) => void, msg: EventsCall) {
		try {
			sink(msg);
		} catch {
			// A worker that has gone away is not worth reporting.
		}
	}

	#broadcast(msg: EventsCall) {
		for (let sink of [...this.#sinks]) this.#post(sink, msg);
		for (let fn of [...this.#listeners]) {
			try {
				fn(msg);
			} catch (err) {
				console.warn("[node-worker] [fs-events] listener threw", err);
			}
		}
	}

	// ------------------------------------------------------- the timestamp fallback

	#syncPolling() {
		// A live socket reports every change with a path attached, which is
		// strictly more than this can say. Poll only when it isn't.
		let wanted = !!this.#token && !this.#empty && !this.#connected;
		if (wanted === (this.#pollTimer !== undefined)) return;
		if (!wanted) {
			clearInterval(this.#pollTimer);
			this.#pollTimer = undefined;
			this.#setPollHealthy(false);
			return;
		}
		this.#pollTimer = setInterval(() => void this.#poll(), POLL_INTERVAL_MS);
		void this.#poll();
	}

	#poll(): Promise<void> {
		return (this.#polling ??= this.#pollOnce().finally(() => {
			this.#polling = undefined;
		}));
	}

	async #pollOnce(): Promise<void> {
		if (!this.#token) return;
		let timestamp: number;
		try {
			let res = await fetch(this.#pollUrl, { method: "GET" });
			if (!res.ok) throw new Error(`HTTP ${res.status}`);
			let body = await res.json();
			timestamp = Number(body?.timestamp);
			if (!Number.isFinite(timestamp)) throw new Error("no timestamp in body");
		} catch (err) {
			// `#checkedAt` deliberately does not advance: a consumer bounding its
			// staleness by it must stop trusting itself while this is failing,
			// which is the correct answer to "I cannot tell whether anything moved".
			console.warn("[node-worker] [fs-events] last-change poll failed", err);
			this.#setPollHealthy(false);
			return;
		}

		let first = this.#lastChange === undefined;
		let moved = !first && timestamp !== this.#lastChange;
		this.#lastChange = timestamp;
		this.#checkedAt = Date.now();
		this.#setPollHealthy(true);
		// The first poll establishes the baseline. Reporting it as a change would
		// throw away a cache that is not yet known to be wrong.
		if (moved) this.#broadcast({ op: "ev.stale", timestamp });
	}

	/**
	 * Announce a window nobody was listening through.
	 *
	 * Both edges of the socket's connectivity qualify. A drop is obvious. A
	 * *reconnect* is the less obvious one: the gap between the last poll and the
	 * socket coming live is unobserved by either source, and the socket sends no
	 * backlog.
	 */
	#announceGap() {
		this.#broadcast({ op: "ev.stale", timestamp: Date.now() });
	}

	#clearTimers() {
		if (this.#reconnectTimer !== undefined) clearTimeout(this.#reconnectTimer);
		if (this.#watchdog !== undefined) clearTimeout(this.#watchdog);
		if (this.#pollTimer !== undefined) clearInterval(this.#pollTimer);
		this.#reconnectTimer = undefined;
		this.#watchdog = undefined;
		this.#pollTimer = undefined;
	}

	#teardownSocket() {
		let ws = this.#ws;
		this.#ws = undefined;
		if (!ws) return;
		ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
		try {
			ws.close();
		} catch {
			// Already closing.
		}
	}

	#setConnected(connected: boolean) {
		if (this.#connected === connected) return;
		let hadBaseline = this.#lastChange !== undefined;
		this.#connected = connected;
		this.#broadcast(this.#state());

		if (!connected) {
			// The counter was not being watched while the socket was, so there is no
			// value to compare the first post-drop poll against — it returns whatever
			// the world is at now, gap included. Assume the worst. Announced *before*
			// polling resumes, so that first poll reads as a baseline rather than as a
			// second, duplicate change.
			this.#announceGap();
			this.#lastChange = undefined;
			this.#syncPolling();
			return;
		}

		this.#syncPolling();
		if (!hadBaseline) {
			// Nothing covered the stretch before the socket came up, so anything
			// learned during it is suspect.
			this.#announceGap();
			return;
		}
		// There *is* a baseline: one more poll answers precisely whether the window
		// between the last one and the socket coming live contained a change. Worth
		// a request — this fires exactly when a session has finished resolving its
		// modules, and announcing a gap unconditionally would throw that away every
		// single startup.
		void this.#poll();
	}

	#connect() {
		if (!this.#token) return;
		if (this.#ws || this.#dead || this.#empty) return;

		let ws: WebSocket;
		try {
			ws = new WebSocket(this.#url);
		} catch (err) {
			console.warn("[node-worker] [fs-events] socket construction failed", err);
			this.#scheduleReconnect();
			return;
		}
		this.#ws = ws;

		ws.onmessage = (e) => {
			if (typeof e.data !== "string") return;
			this.#onFrame(ws, e.data);
		};
		ws.onerror = () => {
			// `error` is always followed by `close`, which does the reconnecting.
			// Engine.io gives no detail here beyond "the socket failed".
			console.warn("[node-worker] [fs-events] socket error");
		};
		ws.onclose = () => {
			if (this.#ws !== ws) return;
			this.#teardownSocket();
			this.#setConnected(false);
			this.#scheduleReconnect();
		};
	}

	#onFrame(ws: WebSocket, frame: string) {
		if (frame.length === 0) return;
		let type = frame[0];
		let payload = frame.slice(1);

		if (type === EIO_PING) {
			// engine.io v4 has the *server* ping and the client pong.
			ws.send(EIO_PONG);
			this.#armWatchdog();
			return;
		}
		if (type === EIO_OPEN) {
			this.#onHandshake(ws, payload);
			return;
		}
		if (type === EIO_CLOSE) {
			// The server is shutting the transport down; `onclose` follows.
			return;
		}
		if (type === EIO_MESSAGE) {
			this.#onMessage(payload);
			return;
		}
		// PONG/UPGRADE/NOOP: we never ping and never upgrade, so nothing to do.
	}

	#onHandshake(ws: WebSocket, payload: string) {
		let handshake: EngineHandshake;
		try {
			handshake = JSON.parse(payload);
		} catch {
			console.warn("[node-worker] [fs-events] malformed handshake");
			this.#teardownSocket();
			this.#scheduleReconnect();
			return;
		}

		this.#pingInterval = handshake.pingInterval ?? 25000;
		this.#pingTimeout = handshake.pingTimeout ?? 20000;
		this.#armWatchdog();

		// The backend reads the token from `socket.handshake.auth.auth_token`
		// and joins the socket to the user's room; there is no cookie or query
		// fallback. This is the socket.io CONNECT packet for the default
		// namespace with `auth` as its payload.
		ws.send(
			EIO_MESSAGE + SIO_CONNECT + JSON.stringify({ auth_token: this.#token })
		);
	}

	#armWatchdog() {
		if (this.#watchdog !== undefined) clearTimeout(this.#watchdog);
		// A silent socket is worse than a closed one: no `close` event fires
		// when the connection is black-holed, so watchers would sit believing
		// they are live. Give the server one full interval plus its own timeout
		// before declaring the transport dead.
		this.#watchdog = setTimeout(() => {
			console.warn("[node-worker] [fs-events] ping timeout, reconnecting");
			this.#teardownSocket();
			this.#setConnected(false);
			this.#scheduleReconnect();
		}, this.#pingInterval + this.#pingTimeout);
	}

	#onMessage(payload: string) {
		let { type, body } = parseSocketIoPacket(payload);

		if (type === SIO_CONNECT) {
			this.#attempt = 0;
			this.#setConnected(true);
			return;
		}

		if (type === SIO_CONNECT_ERROR) {
			// Auth is checked once, in the handshake middleware, so a rejection
			// here will reject identically forever. Retrying a bad token is what
			// puter.js gets wrong (it registers no `connect_error` handler at
			// all) — stop, and tell the worker why.
			let message =
				(body && (body.message || body.data?.reason)) ?? "socket auth failed";
			console.warn("[node-worker] [fs-events] connect error", body);
			this.#dead = true;
			this.#clearTimers();
			this.#teardownSocket();
			this.#setConnected(false);
			this.#broadcast({
				op: "ev.error",
				message: `fs events unavailable: ${message}`,
				fatal: true,
			});
			// `#clearTimers` took the poller down with the reconnect timer, and this
			// is the one state where the poller is the *only* source there will ever
			// be — a non-godmode app launch gets an app token, which the handshake
			// middleware refuses. Bring it back up.
			this.#syncPolling();
			// `#setConnected(false)` was a no-op here: this socket never *became*
			// connected, so nothing transitioned. The gap is real all the same, and
			// announcing it is what tells a consumer to establish a baseline now
			// rather than after the first change it would otherwise miss.
			this.#announceGap();
			return;
		}

		if (type === SIO_DISCONNECT) {
			this.#setConnected(false);
			this.#teardownSocket();
			this.#scheduleReconnect();
			return;
		}

		if (type === SIO_EVENT) {
			if (!Array.isArray(body) || typeof body[0] !== "string") return;
			let event = normalize(body[0], body[1]);
			if (event) this.#broadcast({ op: "ev.fs", event });
		}
	}

	#scheduleReconnect() {
		if (this.#dead || this.#empty) return;
		if (this.#reconnectTimer !== undefined) return;

		let backoff = Math.min(
			RECONNECT_BASE_MS * 2 ** this.#attempt,
			RECONNECT_MAX_MS
		);
		this.#attempt++;
		let delay = backoff * (1 + RECONNECT_JITTER * (Math.random() * 2 - 1));

		this.#reconnectTimer = setTimeout(() => {
			this.#reconnectTimer = undefined;
			this.#connect();
		}, delay);
	}
}

// One hub per (token, origin). Several NodeWorkers on the same account share a
// single socket; each gets its own port.
let hubs = new Map<string, FsEventsHub>();

/**
 * Fan a locally-produced mutation out to every attached watcher.
 *
 * The filesystem lives on this side now, so its own mutations have no socket echo to wait for.
 * The session that *caused* one already learns about it on the reply frame of the call it made —
 * that is what keeps write-then-observe immediate even while a worker is parked in a blocking
 * call — so this exists for the other direction: telling everyone *else* sharing those providers.
 *
 * A no-op when nothing is watching, since a hub only exists once a watcher subscribes.
 */
export function broadcastLocalFsEvent(event: PuterFsEvent): void {
	for (const hub of hubs.values()) hub.inject(event);
}

function hubFor(token: string | undefined, apiOrigin: string): FsEventsHub {
	// NUL as the separator, because it is the one byte that cannot appear in either half,
	// so no origin/token pair can collide with another by splitting differently. Written as
	// an escape rather than the literal byte it used to be: an embedded NUL makes grep and
	// ripgrep classify this file — and every bundle built from it — as binary, and skip it.
	let key = `${apiOrigin}\0${token}`;
	let hub = hubs.get(key);
	if (!hub) {
		hub = new FsEventsHub(token, apiOrigin, () => {
			if (hubs.get(key) === hub) hubs.delete(key);
		});
		hubs.set(key, hub);
	}
	return hub;
}

/** What a worker's subscription to the feed looks like from the page's side. */
export interface FsEventsFeed {
	readonly connected: boolean;
	readonly polling: boolean;
	close(): void;
}

export function handleFsEvents(
	token: string | undefined,
	apiOrigin: string,
	sink: (msg: EventsCall) => void
): FsEventsFeed {
	return hubFor(token, apiOrigin).attach(sink);
}

/**
 * What an in-page consumer of the feed gets back.
 *
 * `freshAsOf` and `ensureFresh` are the half a `MessagePort` cannot carry: a
 * cache has to *ask* how far behind it might be before answering from itself,
 * and an answer that has to cross a channel and come back is no longer an answer
 * about now.
 */
export interface FsEventsSubscription {
	close(): void;
	readonly connected: boolean;
	/**
	 * Local time at which the feed last knew it was in step with the server:
	 * `Date.now()` while the socket is delivering, the last successful poll of the
	 * change counter otherwise, and 0 before either has happened.
	 */
	freshAsOf(): number;
	/** Bring `freshAsOf()` within `maxAgeMs` of now, if it isn't already. */
	ensureFresh(maxAgeMs: number): Promise<void>;
}

/**
 * Subscribe to the feed from inside the page.
 *
 * Same hub, same socket, same refcount as `handleFsEvents` — this exists because
 * the host filesystem's cache lives three modules away rather than across a
 * worker boundary, and serializing every event through a `MessageChannel` to
 * reach it would be pure ceremony.
 */
export function subscribeFsEvents(
	token: string | undefined,
	apiOrigin: string,
	handler: (msg: EventsCall) => void
): FsEventsSubscription {
	let hub = hubFor(token, apiOrigin);
	let detach = hub.subscribe(handler);
	return {
		close: detach,
		get connected() {
			return hub.connected;
		},
		freshAsOf: () => hub.freshAsOf(),
		ensureFresh: (maxAgeMs) => hub.ensureFresh(maxAgeMs),
	};
}
