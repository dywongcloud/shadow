// The header every message carries, whatever its kind.
//
// One request, one reply, correlated by `seq` — the same `seq` on both transports, and
// the same one the host's replay record is keyed on. Nothing here is kind-specific:
// `call` is whatever op set the kind declares (../wire/fs.ts, ./process.ts, …), and the
// dispatcher on the far side is chosen by the frame's `kind` before this is ever parsed.

import type { WireError } from "./error";

/**
 * What a message looks like on the asynchronous transport.
 *
 * Two fields, and both are needed: `f` is the encoded message, and `a` carries the
 * handles — a `MessagePort`, a `ReadableStream` — that bytes cannot express. A message
 * with anything in `a` is async-only by construction, which is why the sync transport
 * takes the bare bytes and never this.
 *
 * There is no correlation id here. The message already carries a `seq` and the reply
 * echoes it, so an envelope id would be a second answer to a question already answered —
 * which is what the previous `{id, frame, error}` shape was, along with a third error
 * format that dropped `code` and `errno` on the way across.
 */
export interface PortEnvelope {
	f: ArrayBuffer;
	/**
	 * Handles handed over, then values merely cloned. See `CallOptions.attach` — a transfer list
	 * may hold only `Transferable`s, so anything else rides here and is structured-cloned with
	 * the envelope itself.
	 */
	a?: unknown[];
}

/**
 * A message riding along on another one.
 *
 * Pushes exist because the far side cannot always be *reached*. A worker parked inside a
 * blocking XHR will never read a `postMessage`, so anything the host wants to tell it —
 * a watch event, a cache invalidation — has to arrive on the reply it is already waiting
 * for. The same holds in the other direction for a worker that has buffered output and is
 * about to park: the bytes ride the request rather than racing it.
 *
 * `np` is how many of the message's parts this push consumes. The payload is one flat
 * array: the primary's `n` parts first, then each push's `np` in order. Keeping one array
 * is what lets ./frame.ts stay ignorant of sidebands entirely — it slices by
 * `header.parts` and never has to know who the slices belong to.
 */
export interface Push {
	kind: number;
	op: string;
	args?: unknown;
	np?: number;
}

/**
 * The header of an answering message.
 *
 * `events` and `invalidate` are piggybacked rather than pushed separately, and both
 * are load-bearing.
 *
 * `events` replaces what the providers used to do directly: they called
 * `emitLocalFsEvent` in the worker the moment a mutation succeeded, which is what
 * makes write-then-observe (chokidar's `awaitWriteFinish`, a dev server's HMR trigger)
 * feel immediate. Riding the reply keeps that timing *and* works while the worker is
 * blocked inside a synchronous call, because the event arrives on the response the
 * worker is already waiting for — which is the whole reason a push cannot be a
 * `MessagePort` message: a parked worker will never read one.
 *
 * `invalidate` replaces the direct calls into the module resolver's caches. Those
 * caches are deliberately never invalidated otherwise, so a file the *host* wrote
 * would be answered as "missing" forever — a bug that only shows up on a second run.
 */
export interface WireReply {
	/** Echo of the request's sequence number. A mismatch is a routing bug, never a slow answer. */
	seq: number;
	result: { ok: true; value: unknown } | { ok: false; error: WireError };
	/** Byte length of every part in the payload, in order, sidebands included. */
	parts?: number[];
	/** How many of `parts` belong to the result itself. Absent ⇒ all of them. */
	n?: number;
	/** Messages riding this reply, consuming the parts after the result's. */
	push?: Push[];
	events?: unknown[];
	invalidate?: { paths?: string[]; subtrees?: string[] };
	/** Per-endpoint backend call counts, for `NODE_WORKER_API_STATS`. */
	apiCalls?: Record<string, number>;
}

export interface WireRequest<Call = unknown> {
	seq: number;
	call: Call;
	/** Byte length of every part in the payload, in order, sidebands included. */
	parts?: number[];
	/** How many of `parts` belong to the call itself. Absent ⇒ all of them. */
	n?: number;
	/** Messages riding this request, consuming the parts after the call's. */
	out?: Push[];
}
