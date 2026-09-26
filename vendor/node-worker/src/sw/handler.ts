// The transport half of the bridge: a blocking XHR from the node worker, relayed to the
// page, answered as an HTTP response.
//
// The service worker never looks inside a frame. It moves opaque bytes and does nothing
// else — no mount table, no providers, no filesystem knowledge at all — which is why this
// file imports only constants and why it can be a **classic** script. (It has to be: module
// service workers are not universally shipped, and there is nothing to gain from ESM in
// 200 lines.)
//
// Exported as `installNodeWorkerFetch` rather than being the whole worker, because **only
// one service worker can own a scope**. A host application that already has one cannot
// register ours, and without this it could never use synchronous filesystem access at all;
// with it, they call this from their own worker.
//
// Two browser facts shape everything below, and both are easy to get wrong:
//
//   1. **`MessagePort.postMessage` does not start a stopped service worker;
//      `ServiceWorker.postMessage` does.** So the page always *attaches* through
//      `registration.active.postMessage`, and this side never initiates over a port it
//      merely happens to be holding. A message to a port whose SW instance has been killed
//      is silently lost.
//   2. **A service worker is evicted when idle and loses all module state.** The session
//      registry below is a cache, never the truth, and arriving at a cold worker is the
//      *normal* path — in testing Chrome dropped it between two phases of the same run.
//      `rescue` is how it gets its port back, and only a window client can answer.

import { WIRE_PROTO } from "../wire/frame";
import {
	OP_TIMEOUT_MS,
	RESCUE_TIMEOUT_MS,
	SW_BROADCAST_CHANNEL,
	SW_PATH_SEGMENT,
	SW_STATUS,
	type PageToSw,
	type SessionId,
} from "../wire/sw";

interface Session {
	port: MessagePort;
	/** The window that owns it, so a restarted worker can resolve it directly. */
	clientId: string | undefined;
}

interface Pending {
	resolve(frame: ArrayBuffer): void;
	reject(err: unknown): void;
	timer: ReturnType<typeof setTimeout>;
}

const HEADERS = {
	"Content-Type": "application/octet-stream",
	"Cache-Control": "no-store",
};

export function installNodeWorkerFetch(): void {
	const sw = self as unknown as ServiceWorkerGlobalScope;

	// Scope-relative, so one build works at any registration scope and the page can compute
	// the identical URL from `registration.scope`. This is what removes the need for a
	// `Service-Worker-Allowed` header, which matters because the deployment target is static
	// hosting that cannot set one.
	const prefix = new URL(`${SW_PATH_SEGMENT}/`, sw.registration.scope).pathname;

	const sessions = new Map<SessionId, Session>();
	const waiting = new Map<SessionId, Array<(s: Session | undefined) => void>>();
	// Keyed by session *and* sequence, never sequence alone.
	//
	// `nextSeq` restarts at 1 every time this worker is evicted and respawned, which is routine and
	// can happen between any two operations. Keyed on the number by itself, a reply arriving on one
	// session's port could settle a request issued to a different session — and since the frame was
	// then decoded and returned without further checks, the caller got another session's answer as
	// if it were its own. That is how two workers sharing a filesystem read each other's files.
	const pending = new Map<string, Pending>();
	let nextSeq = 1;

	const key = (sid: SessionId, seq: number) => `${sid}:${seq}`;

	function settle(sid: SessionId, seq: number, frame: ArrayBuffer) {
		const k = key(sid, seq);
		const entry = pending.get(k);
		if (!entry) return;
		pending.delete(k);
		clearTimeout(entry.timer);
		entry.resolve(frame);
	}

	function expire(sid: SessionId, seq: number) {
		const k = key(sid, seq);
		const entry = pending.get(k);
		if (!entry) return;
		pending.delete(k);
		entry.reject(new Error("host did not answer in time"));
	}

	function onPortMessage(sid: SessionId, data: PageToSw | undefined) {
		if (!data) return;
		if (data.t === "res") {
			settle(sid, data.seq, data.frame);
			return;
		}
		if (data.t === "progress") {
			// A slow operation is not a hung page — a large write into OPFS can legitimately
			// outlast the deadline — so a heartbeat refreshes it rather than raising it for
			// everything.
			const entry = pending.get(key(sid, data.seq));
			if (entry) {
				clearTimeout(entry.timer);
				entry.timer = setTimeout(() => expire(sid, data.seq), OP_TIMEOUT_MS);
			}
		}
	}

	function adopt(
		sid: SessionId,
		port: MessagePort,
		clientId: string | undefined
	) {
		// Always replaces. From the page's side a stale port is indistinguishable from a live
		// one, so it must be free to mint a replacement at any time — and this side must
		// prefer the newest.
		port.onmessage = (e: MessageEvent) => onPortMessage(sid, e.data);
		port.start?.();
		sessions.set(sid, { port, clientId });
		const list = waiting.get(sid);
		if (list) {
			waiting.delete(sid);
			const session = sessions.get(sid);
			for (const resolve of list) resolve(session);
		}
		return clientId;
	}

	sw.addEventListener("message", (event: ExtendableMessageEvent) => {
		const data = event.data as PageToSw | undefined;
		if (!data) return;
		if (data.t === "attach") {
			const port = event.ports[0];
			if (!port) return;
			const clientId = (event.source as Client | null)?.id;
			adopt(data.sid, port, clientId);
			port.postMessage({
				t: "attached",
				prefix,
				proto: WIRE_PROTO,
				clientId,
			});
			return;
		}
		if (data.t === "detach") {
			sessions.delete(data.sid);
		}
	});

	sw.addEventListener("fetch", (event: FetchEvent) => {
		// The respondWith decision must be reachable with NO await, and this handler must
		// never throw for a URL it does not own: with a fetch listener registered, *every*
		// in-scope request arrives here — the 8 MB worker script, every asset beside it, the
		// cross-origin epoxy import, every api call. A plain `return` leaves them to the
		// network untouched.
		//
		// NOT `respondWith(fetch(request))`. That would route the whole asset graph through
		// this thread, degrade byte-range handling (a synthesized 200 for a `Range` request
		// breaks media elements) and put the epoxy wasm's `Content-Type` — which streaming
		// compilation depends on — at the mercy of our response construction. Nothing is
		// cached here either: a cached worker script one build older than the page is a
		// version-skew bug nobody enjoys.
		const url = new URL(event.request.url);
		if (url.origin !== sw.location.origin) return;
		if (!url.pathname.startsWith(prefix)) return;
		if (event.request.method !== "POST") return;
		// `serve` must never reject. A rejected `respondWith` reaches a synchronous XHR as an
		// opaque network error ("Failed to load …"), with no way for the worker to say what
		// happened — so every internal failure is turned into a response carrying a reason.
		event.respondWith(
			serve(url, event.request).catch(
				(err) =>
					new Response(`node-worker: service worker failed: ${err}`, {
						status: SW_STATUS.noSession,
						headers: HEADERS,
					})
			)
		);
	});

	async function serve(url: URL, request: Request): Promise<Response> {
		// `{prefix}v{proto}/{sid}/{seq}-{op}`. The sid has to be in the URL because the
		// respondWith decision happens before the body can be read; `{seq}-{op}` makes the
		// devtools network panel a readable syscall trace.
		const rest = url.pathname.slice(prefix.length).split("/");
		if (rest[0] !== `v${WIRE_PROTO}`) {
			// A page and a service worker can be different builds — a stale worker is a normal
			// consequence of a redeploy — so say so instead of parsing a body neither side
			// agrees on.
			return new Response(
				`node-worker: service worker speaks v${WIRE_PROTO}, page asked for ${rest[0]} — reload`,
				{ status: SW_STATUS.protoMismatch, headers: HEADERS }
			);
		}
		const sid = rest[1];
		if (!sid) {
			return new Response("node-worker: no session in url", {
				status: SW_STATUS.noSession,
				headers: HEADERS,
			});
		}

		// Both of these can fail for reasons that have nothing to do with the filesystem — a
		// body that cannot be read, a `clients` call on a client that has gone — and neither
		// used to be guarded, which is how an internal failure became an unexplained network
		// error at the other end.
		let frame: ArrayBuffer;
		try {
			frame = await request.arrayBuffer();
		} catch (err) {
			return new Response(
				`node-worker: could not read the request body: ${err}`,
				{
					status: SW_STATUS.noSession,
					headers: HEADERS,
				}
			);
		}
		let session: Session | undefined;
		try {
			session = sessions.get(sid) ?? (await rescue(sid));
		} catch (err) {
			return new Response(`node-worker: session lookup failed: ${err}`, {
				status: SW_STATUS.noSession,
				headers: HEADERS,
			});
		}
		if (!session) {
			return new Response("node-worker: no filesystem host attached", {
				status: SW_STATUS.noSession,
				headers: HEADERS,
			});
		}

		const seq = nextSeq++;
		try {
			const out = await new Promise<ArrayBuffer>((resolve, reject) => {
				const timer = setTimeout(() => expire(sid, seq), OP_TIMEOUT_MS);
				pending.set(key(sid, seq), { resolve, reject, timer });
				session.port.postMessage({ t: "op", seq, frame }, [frame]);
			});
			return new Response(out, { status: SW_STATUS.ok, headers: HEADERS });
		} catch (err) {
			return new Response(`node-worker: ${(err as Error).message}`, {
				status: SW_STATUS.timeout,
				headers: HEADERS,
			});
		}
	}

	/**
	 * The registry was empty. Normal, not exceptional — see the note at the top.
	 *
	 * Every window is asked, plus a broadcast — `BroadcastChannel` from a service worker
	 * reaches same-origin pages regardless of control or scope. *This* side initiates,
	 * because a message to the page's stale port would go nowhere.
	 *
	 * There used to be a fast path here: `clients.get(sid.split(".")[0])`, on the premise
	 * that a sid is `<pageClientId>.<nonce>`. It never once succeeded. A page cannot know
	 * its own client id before it has attached — the id comes back *in* the `attached`
	 * reply — so the sid it mints has no dot in it, `clients.get` was handed the whole sid,
	 * and every rescue fell through to the code below having first burned a lookup. The
	 * premise cannot be satisfied without a second attach round trip at startup, which
	 * costs more than the fallback it was avoiding, so the fast path is gone rather than
	 * left in place looking like it works.
	 */
	async function rescue(sid: SessionId): Promise<Session | undefined> {
		{
			let windows: readonly Client[] = [];
			try {
				windows = await sw.clients.matchAll({
					type: "window",
					// A page whose controller changed under it is still the owner of the session and
					// still the only thing that can answer.
					includeUncontrolled: true,
				});
			} catch {
				// Treated as "nobody to ask", which is the honest reading.
			}
			if (windows.length === 0) {
				// Nobody could possibly answer. Fail immediately rather than making the worker
				// wait out a deadline for a reply that is not coming.
				return undefined;
			}
			for (const client of windows) client.postMessage({ t: "rescue", sid });
			try {
				new BroadcastChannel(SW_BROADCAST_CHANNEL).postMessage({
					t: "rescue",
					sid,
				});
			} catch {
				// BroadcastChannel is not everywhere; the direct posts above are the main path.
			}
		}

		return new Promise<Session | undefined>((resolve) => {
			const list = waiting.get(sid) ?? [];
			list.push(resolve);
			waiting.set(sid, list);
			setTimeout(() => resolve(sessions.get(sid)), RESCUE_TIMEOUT_MS);
		});
	}
}
