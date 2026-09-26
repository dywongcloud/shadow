// Routing a message to its dispatcher, and building the answer.
//
// Both halves used to exist twice. Routing was a pair of near-identical ternaries in
// lib/index.ts — one on the service-worker path, one on the postMessage path — which is
// how they came to disagree about which session id to dispatch under, splitting one
// worker's replay record in two. Reply building was a pair of `reply()` functions, one
// per dispatcher, which is how the process side came to set `parts` by hand at every
// call site while the filesystem derived it.
//
// One table and one builder, so a third kind is a `register()` call rather than another
// branch in two places that have to stay in step.

import { decodeFrame, encodeFrame, frameKind, hasSideband } from "./frame";
import { primaryParts, unpackSidebands } from "./pack";
import { recall, remember, type ReplayCache } from "./replay";
import { kindName } from "./kinds";
import { toWireError } from "./error";
import type { WireReply, WireRequest } from "./message";

/**
 * An answer, and the handles that go with it.
 *
 * A dispatcher that has nothing to hand over returns the bytes alone; one whose reply
 * carries a `MessagePort` or a stream returns both. Those handles are what makes an op
 * async-only — an XHR body cannot hold one — which is why the kind they belong to is
 * declared non-sync in ./kinds.ts rather than discovered when a caller hangs.
 */
export interface DispatchResult {
	frame: Uint8Array;
	transfer?: Transferable[];
}

/**
 * What a kind's handler does: bytes in, bytes out, plus any handles either way.
 *
 * `attachments` are the handles that arrived *with* the request — the console streams on
 * `init`, the port on `chan.open`. Most dispatchers ignore them.
 *
 * Failures ride *inside* the answer rather than being thrown past it, because the reply
 * also carries the sidebands — watch events, cache invalidations — that a thrown error
 * would discard. A dispatcher that throws anyway is caught here and packed the same way,
 * so the worker parked on the other end always gets a message back.
 */
export type Dispatcher = (
	frame: ArrayBuffer | Uint8Array,
	attachments: readonly unknown[]
) => Promise<Uint8Array | DispatchResult>;

/** Sideband state drained onto whatever reply is being built. */
export interface Sidebands {
	events?: unknown[];
	invalidate?: { paths?: string[]; subtrees?: string[] };
	apiCalls?: Record<string, number>;
}

/**
 * Encode an answering message.
 *
 * `parts` are the payload; their lengths are derived here rather than at each call
 * site, which is the half the process dispatcher used to get wrong. Sidebands are
 * attached only when they carry something, so an idle reply stays small.
 */
export function encodeReply(
	kind: number,
	body: WireReply,
	parts?: readonly Uint8Array[],
	side?: Sidebands
): Uint8Array {
	if (side) {
		if (side.events?.length) body.events = side.events;
		if (side.invalidate) body.invalidate = side.invalidate;
		if (side.apiCalls && Object.keys(side.apiCalls).length) {
			body.apiCalls = side.apiCalls;
		}
	}
	if (parts && parts.length) body.parts = parts.map((p) => p.length);
	return encodeFrame(body, parts, kind);
}

/**
 * Encode a failure.
 *
 * `seq` must be the *message's* own sequence number. Answering with anything else — a
 * transport-local counter, say — produces a reply the worker rejects as a crossed
 * response, which reports the plumbing rather than the failure that actually happened.
 */
export function encodeErrorReply(
	kind: number,
	seq: number,
	err: unknown,
	syscall?: string
): Uint8Array {
	return encodeFrame(
		{ seq, result: { ok: false, error: toWireError(err, syscall) } },
		undefined,
		kind
	);
}

/** What a kind's handler returns for one op. */
export interface Answered {
	value?: unknown;
	parts?: readonly Uint8Array[];
	/** Handles to attach to the reply. Makes the op async-only. */
	transfer?: Transferable[];
}

/**
 * Build a dispatcher from a plain `(call) => answer` function.
 *
 * Decode, dispatch, encode, and turn a throw into an in-band `WireError` — every kind
 * needs those four and none of them differ between kinds. Writing them out per dispatcher
 * is how the process side ended up deriving `parts` by hand and answering an undecodable
 * message with a syscall name the filesystem had chosen.
 *
 * `syscallOf` names the operation in an error, so `err.syscall` reads as node's would.
 */
export function makeDispatcher<Call extends { op: string }>(
	kind: number,
	handle: (
		call: Call,
		parts: Uint8Array[],
		attachments: readonly unknown[]
	) => Promise<Answered | void>,
	syscallOf?: (call: Call) => string | undefined,
	/*
	 * The exactly-once record for a sync-capable kind, read per call so a caller can hand over
	 * a session id it does not know at registration time.
	 *
	 * Only kinds in `SYNC_CAPABLE` need this, and only because the blocking transport retries a
	 * failed send with the *same* `seq`: without a record the handler runs a second and third
	 * time for one call. A kind that is async-only can leave it undefined — nothing will ever
	 * repeat a seq at it.
	 */
	replay?: () => { cache: ReplayCache; sid: string } | undefined
): Dispatcher {
	return async (frame, attachments) => {
		let request: WireRequest<Call>;
		let parts: Uint8Array[];
		try {
			const decoded = decodeFrame<WireRequest<Call>>(frame);
			request = decoded.header;
			parts = primaryParts(decoded.header, decoded.parts);
		} catch (err) {
			// Nothing to echo, so answer with seq 0: it matches no outstanding request, which
			// makes the sender report its own diagnostic rather than trusting a fabricated one.
			return { frame: encodeErrorReply(kind, 0, err) };
		}
		const record = replay?.();
		const already = record && recall(record.cache, record.sid, request.seq);
		if (already) return { frame: already };

		let answer: Uint8Array;
		try {
			const answered = (await handle(request.call, parts, attachments)) ?? {};
			answer = encodeReply(
				kind,
				{
					seq: request.seq,
					result: { ok: true, value: answered.value ?? null },
				},
				answered.parts
			);
			// A reply carrying a handle is not replayable: the handle is transferred, so a
			// second answer from the record would hand over a stream already detached.
			if (answered.transfer) return { frame: answer, transfer: answered.transfer };
		} catch (err) {
			answer = encodeErrorReply(
				kind,
				request.seq,
				err,
				syscallOf?.(request.call)
			);
		}
		if (record) remember(record.cache, record.sid, request.seq, answer);
		return { frame: answer };
	};
}

/**
 * Best-effort `seq` for a message that could not be handed to a dispatcher.
 *
 * Answering with the wrong `seq` is worse than answering with 0: the worker checks it
 * and reports a crossed reply, burying the real reason. 0 never matches an outstanding
 * request, so the sender falls back to its own diagnostic — the honest outcome when the
 * message was unroutable in the first place.
 */
function seqOf(frame: ArrayBuffer | Uint8Array): number {
	try {
		const { header } = decodeFrame<{ seq?: number }>(frame);
		return typeof header?.seq === "number" ? header.seq : 0;
	} catch {
		return 0;
	}
}

/**
 * Answer a message that could not be handled at all.
 *
 * Takes the *message* rather than a seq, because the seq is the thing most easily got
 * wrong here: a transport that answers with its own counter produces a reply the worker
 * rejects as a crossed response, reporting the plumbing instead of the failure. The kind
 * comes from the message too, so a broken process message is not answered as a
 * filesystem one.
 */
export function replyToBrokenFrame(
	frame: ArrayBuffer | Uint8Array,
	err: unknown,
	syscall?: string
): Uint8Array {
	return encodeErrorReply(frameKind(frame), seqOf(frame), err, syscall);
}

/**
 * The kind → dispatcher table, shared by every inbound path.
 *
 * The same instance answers messages arriving over the service-worker relay, over the
 * dedicated message port, and over the bootstrap channel — which is the point. Three
 * arrival paths that route independently are three chances to route differently.
 */
export class Router {
	#kinds = new Map<number, Dispatcher>();

	/**
	 * Route every message riding this one, ignoring failures.
	 *
	 * A push cannot be reported on — there is nobody waiting for it — so a handler that
	 * throws is logged by the handler itself or not at all. What must not happen is a
	 * sideband failure taking down the carrier, which is a real message with a real caller.
	 */
	#deliverSidebands(frame: ArrayBuffer | Uint8Array): void {
		// One bit test on the common path. Almost no message carries anything, and decoding
		// every one of them to find that out would double the JSON parsing on the hot path.
		if (!hasSideband(frame)) return;
		let sidebands;
		try {
			const decoded = decodeFrame<{
				n?: number;
				out?: import("./message").Push[];
			}>(frame);
			if (!decoded.header?.out?.length) return;
			sidebands = unpackSidebands(decoded.header, decoded.parts).sidebands;
		} catch {
			// The carrier's own dispatcher will report the decode failure properly.
			return;
		}
		for (const { push, parts } of sidebands) {
			const dispatcher = this.#kinds.get(push.kind);
			if (!dispatcher) continue;
			void Promise.resolve()
				.then(() =>
					dispatcher(
						encodeFrame(
							{ seq: 0, call: { op: push.op, ...(push.args as object) } },
							parts,
							push.kind
						),
						[]
					)
				)
				.catch(() => {
					// Nothing is waiting on a push, so there is nowhere to report this.
				});
		}
	}

	register(kind: number, dispatcher: Dispatcher): void {
		if (this.#kinds.has(kind)) {
			throw new Error(
				`wire: dispatcher for ${kindName(kind)} already registered`
			);
		}
		this.#kinds.set(kind, dispatcher);
	}

	has(kind: number): boolean {
		return this.#kinds.has(kind);
	}

	/**
	 * Route one message and answer it. Never rejects.
	 *
	 * A message for a kind nobody registered is answered with ENOSYS rather than
	 * dropped: dropping it parks the worker until a timeout it cannot distinguish from
	 * a hung page, and the sender is owed the difference.
	 */
	async handle(
		frame: ArrayBuffer | Uint8Array,
		attachments: readonly unknown[] = []
	): Promise<DispatchResult> {
		// Anything riding this message is delivered first, and separately.
		//
		// First because a sideband is by definition older than its carrier: the bytes a
		// program printed before it called `readFileSync` were queued before that call was
		// made, and the terminal has to see them in that order. Separately because a push has
		// no reply — it rode here precisely because there was no round trip to give it.
		this.#deliverSidebands(frame);

		const kind = frameKind(frame);
		const dispatcher = this.#kinds.get(kind);
		if (!dispatcher) {
			return {
				frame: encodeErrorReply(
					kind,
					seqOf(frame),
					Object.assign(
						new Error(
							`ENOSYS: no handler for ${kindName(kind)} messages, this build registered none`
						),
						{ code: "ENOSYS" }
					)
				),
			};
		}
		try {
			const out = await dispatcher(frame, attachments);
			return out instanceof Uint8Array ? { frame: out } : out;
		} catch (err) {
			// A dispatcher is supposed to pack its own failures in-band; reaching here
			// means one threw past that, and the worker is still waiting either way.
			return { frame: encodeErrorReply(kind, seqOf(frame), err) };
		}
	}
}
