// The worker's two ways of reaching the host filesystem.
//
//   `vfsSync`  — a blocking XMLHttpRequest to a virtual URL a service worker intercepts and
//                relays to the page. This is what makes `fs.readFileSync` work.
//   `vfsAsync` — a postMessage round trip, for `fs.promises`.
//
// Both send the *same frame* to the *same dispatch table* (src/lib/vfs/dispatch.ts). The
// failure mode this boundary is most exposed to is a bug that exists on one transport and not
// the other, and there is nothing to diverge if there is only one implementation.
//
// Deliberately free of module-scope side effects and of any reference to `process`: this sits
// in the fs subgraph, which evaluates inside a module-init cycle (see ./lazy-base.ts), and
// anything imported this widely has to be safe to evaluate first.

import { fromWireError, toWireError } from "../../../wire/error";
import {
	decodeFrame,
	FrameError,
	type DecodedFrame,
} from "../../../wire/frame";
import {
	packRequest as packWithSidebands,
	primaryParts,
} from "../../../wire/pack";
import {
	KIND_CHAN,
	KIND_FS,
	KIND_PROCESS,
	KIND_STDIO,
	isAsyncOnlyOp,
	isSyncCapable,
	kindName,
} from "../../../wire/kinds";
import type {
	MountSnapshot,
	NodeFsCapabilities,
	VfsCall,
	VfsInit,
	VfsResult,
} from "../../../wire/fs";
import type { WireReply } from "../../../wire/message";
import { SW_STATUS } from "../../../wire/sw";
import type { ProcessCall, ProcessResult } from "../../../wire/process";
import type { StdioCall, StdioResult } from "../../../wire/stdio";
import type { ChanCall } from "../../../wire/chan";
// The global is deleted so programs cannot see it (epoxy/globals.ts); this transport keeps the
// captured constructor, which is the one thing that still legitimately needs a blocking XHR.
import { NATIVE_XHR } from "../../epoxy/globals";
import { under } from "../../../vfs/path";
import * as keepalive from "../../keepalive";
import { asArrayBuffer } from "../../../wire/endpoint";
import { wire } from "../../wire";

let CFG: VfsInit | undefined;
let capabilities: NodeFsCapabilities = {
	sync: false,
	reason: "probe-failed",
	detail: "the filesystem transport has not been initialized",
};
// No counter of its own. Both transports mint from the endpoint's, because the host's
// replay record is keyed on `seq` — two counters would let a retry over one transport be
// answered from the other's record.

// ------------------------------------------------------------ the mount snapshot

/**
 * Seeded with a placeholder so a call arriving before `init` fails here, with a message that
 * says so, rather than somewhere confusing further down.
 */
let mounts: MountSnapshot[] = [
	{
		root: "/",
		name: "uninitialized",
		readOnly: false,
		createdMs: 0,
		hasNativeRange: false,
		canStream: false,
		hasCopyFile: false,
		hasStatfs: false,
	},
];

export function applyMountSnapshot(snapshot: MountSnapshot[]) {
	// Longest root first, so the first match is the most specific — the same ordering the
	// host's table uses, because both answer the same question.
	mounts = [...snapshot].sort((a, b) => b.root.length - a.root.length);
}

export function mountFor(path: string): MountSnapshot {
	for (const m of mounts) if (under(m.root, path)) return m;
	return mounts[mounts.length - 1];
}

export function listMountSnapshot(): readonly MountSnapshot[] {
	return mounts;
}

// ------------------------------------------------------------- reply side-effects
//
// A reply can carry watch events and resolver-cache invalidations caused by the very call being
// answered. They ride the reply rather than being pushed separately because that is the only
// delivery that works while this thread is parked inside a blocking request — a pushed message
// would sit in the queue until the XHR returned, long after the caller had moved on.
//
// Registered rather than imported so this module keeps no edge into `../../fsevents` or
// `../../module/resolve`, both of which sit in heavier parts of the graph.

type MetaSink = (meta: {
	events?: unknown[];
	invalidate?: { paths?: string[]; subtrees?: string[] };
	apiCalls?: Record<string, number>;
}) => void;

let metaSink: MetaSink | undefined;

export function onReplyMeta(sink: MetaSink) {
	metaSink = sink;
}

function applyMeta(reply: WireReply) {
	if (!metaSink) return;
	if (!reply.events && !reply.invalidate && !reply.apiCalls) return;
	try {
		metaSink({
			events: reply.events,
			invalidate: reply.invalidate,
			apiCalls: reply.apiCalls,
		});
	} catch {
		// A watcher or cache callback throwing must not fail the filesystem call that
		// happened to carry its notification.
	}
}

// -------------------------------------------------------------------- hop counters

let hops = new Map<string, number>();

function countHop(op: string, kind: "sync" | "async") {
	const key = `${op} ${kind}`;
	hops.set(key, (hops.get(key) ?? 0) + 1);
}

export function getHopStats(): Record<string, number> {
	return Object.fromEntries([...hops].sort((a, b) => b[1] - a[1]));
}

export function resetHopStats() {
	hops.clear();
}

// ------------------------------------------------------------------------- results

export interface Answer<V> {
	value: V;
	/** Views into the reply frame. Copy before keeping. */
	parts: Uint8Array[];
}

export type VfsAnswer<K extends VfsCall["op"]> = Answer<VfsResult<K>>;
export type ProcAnswer<K extends ProcessCall["op"]> = Answer<ProcessResult<K>>;
export type ChanAnswer = Answer<unknown>;

function unpack<V>(
	bytes: ArrayBuffer | Uint8Array,
	call: { op: string },
	expected: number
): Answer<V> {
	return unpackDecoded<V>(decodeFrame<WireReply>(bytes), call, expected);
}

/**
 * The same, on a message that has already been decoded.
 *
 * Split out for the port transport, which has to read the reply's `seq` to know whose
 * reply it is. Decoding there and again here would be two `JSON.parse`es of the same
 * header — and, worse, two chances to disagree about what it says.
 */
function unpackDecoded<V>(
	decoded: DecodedFrame<WireReply>,
	call: { op: string },
	expected: number,
	signal?: AbortSignal
): Answer<V> {
	const { header } = decoded;
	// Same rule as the host side: anything riding this reply belongs to whoever it was
	// pushed to, not to this answer's payload.
	const parts = primaryParts(header, decoded.parts);
	// The reply must be the answer to *this* request. Nothing upstream guaranteed that: the
	// service worker correlates a reply to a fetch through a counter it restarts from 1 whenever
	// it is evicted, so a reply could be handed to the wrong caller and — before this — decoded
	// and returned as if it were the right one. Silent wrong bytes are the worst possible failure
	// for a filesystem, so this is checked rather than assumed.
	if (header.seq !== expected) {
		throw transportError(
			`the host answered request ${header.seq}, not ${expected} ` +
				"(the transport crossed two replies)"
		);
	}
	applyMeta(header);
	// The abort is checked *after* the sidebands are applied, and the order is the whole
	// point. A caller that aborts once the host has already done the work still owes the
	// rest of the runtime that work's consequences: the watch events a mutation produced and
	// the resolver-cache invalidations that go with them. Checking first — which this did —
	// dropped both silently, so an aborted write landed on disk and nothing watching it ever
	// heard.
	signal?.throwIfAborted();
	if (!header.result.ok) throw fromWireError(header.result.error);
	return { value: header.result.value as V, parts };
}

/**
 * A failure to *deliver*, as opposed to a filesystem answer of "no".
 *
 * Named so the retry above can tell them apart: an ENOENT must never be retried, and a service
 * worker that vanished mid-request must never be reported as an ENOENT.
 */
export class TransportError extends Error {
	readonly __nodeWorkerFsError = 1 as const;
	code = "EIO";
	errno = -5;
	constructor(message: string) {
		super(message);
		this.name = "Error";
	}
}

function transportError(message: string, cause?: unknown): TransportError {
	// EIO, so the `if (e.code !== "ENOENT") throw e` idiom above behaves, but with the real
	// cause in the message — a bare "i/o error" tells whoever is debugging nothing.
	//
	// The result is a real `TransportError`. It used to be whatever `fromWireError` built,
	// which is a plain `Error` — so `sendSync`'s `err instanceof TransportError` was false for
	// every error this produced, the retry loop rethrew on its first attempt, and the host's
	// replay record had no client at all. The message is composed exactly as before.
	const wire = toWireError(
		Object.assign(new Error(message), { cause }),
		undefined
	);
	const err = new TransportError(wire.message);
	if (cause !== undefined) (err as Error & { cause?: unknown }).cause = cause;
	return err;
}

/**
 * A condition retrying cannot fix: no service worker, or a kind that can never be
 * synchronous. Deliberately *not* a {@link TransportError} — the retry loop exists for a
 * delivery that failed, and re-asking a question with no answer only delays the report.
 */
function permanentError(message: string): Error {
	return fromWireError(
		toWireError(
			Object.assign(new Error(message), { code: "ENOSYS" }),
			undefined
		)
	);
}

// ---------------------------------------------------------------- initialization

/**
 * Take the host's configuration and verify, once, that the synchronous transport genuinely
 * works.
 *
 * The probe is this design's safety valve. Every way interception can silently fail — a scope
 * that does not cover this worker, a policy that blocks synchronous XHR, a service worker that
 * never activated — produces a *hang* at the first `readFileSync` if it is not caught here.
 * One round trip at startup turns all of them into a legible startup state instead.
 */
export function initTransport(cfg: VfsInit): NodeFsCapabilities {
	CFG = cfg;
	applyMountSnapshot(cfg.mounts);

	if (!cfg.syncPrefix) {
		capabilities = {
			sync: false,
			reason: "no-sw",
			detail: "the host did not register a service worker",
		};
		return capabilities;
	}

	try {
		const answer = rawSync(wire.nextSeq(), { op: "probe" }, undefined, KIND_FS);
		const value = answer.value as { proto: number; sid: string };
		if (value?.sid !== cfg.sid) {
			capabilities = {
				sync: false,
				reason: "probe-failed",
				detail: `the filesystem answered for session ${value?.sid}, not ${cfg.sid}`,
			};
		} else if (value.proto !== cfg.proto) {
			capabilities = {
				sync: false,
				reason: "proto-mismatch",
				detail: `host speaks v${value.proto}, worker speaks v${cfg.proto}`,
			};
		} else {
			capabilities = { sync: true };
		}
	} catch (err) {
		const message = (err as Error)?.message ?? String(err);
		capabilities = {
			sync: false,
			// A DOMException from `send()` on a synchronous request is what a
			// `Permissions-Policy: sync-xhr=()` looks like — worth naming, because puter serves
			// apps in iframes and the whole filesystem depends on it.
			reason: /InvalidAccessError|not allowed|sync-xhr/i.test(message)
				? "sync-xhr-blocked"
				: "probe-failed",
			detail: message,
		};
	}
	return capabilities;
}

export function syncCapabilities(): NodeFsCapabilities {
	return capabilities;
}

// --------------------------------------------------------------- the transports

/** `packRequest` with this endpoint's pending sidebands folded in. */
function packRequest(
	id: number,
	call: { op: string },
	parts: Uint8Array[] | undefined,
	kind: number
): Uint8Array {
	return packWithSidebands(kind, id, call, parts, wire.outbound.drain());
}

function rawSync<V>(
	id: number,
	call: { op: string },
	parts: Uint8Array[] | undefined,
	kind: number
): Answer<V> {
	if (!CFG?.syncPrefix) {
		throw permanentError(
			`ENOSYS: synchronous transport unavailable (${capabilities.reason}: ${capabilities.detail ?? "?"})`
		);
	}
	// The same drain the asynchronous path does, so buffered output leaves with whichever
	// message goes first and the two transports cannot disagree about ordering.
	const frame = packRequest(id, call, parts, kind);
	countHop(call.op, "sync");

	const xhr = new NATIVE_XHR();
	xhr.open(
		"POST",
		`${CFG.syncPrefix}v${CFG.proto}/${CFG.sid}/${id}-${kindName(kind)}.${call.op}`,
		false
	);
	xhr.responseType = "arraybuffer";
	// Legal here, illegal in a Window: the spec's `InvalidAccessError` for setting `timeout`,
	// `responseType` or `withCredentials` on a synchronous request applies only when the global
	// is a `Window`.
	//
	// Measured rather than assumed, though: Blink and Gecko honour it (a `TimeoutError` at the
	// deadline), and **WebKit accepts the setter and ignores it** — a stalled request there
	// runs until the service worker's own deadline answers 504. So this is defence-in-depth and
	// the SW-side deadline is the primary bound; do not weaken that one on the strength of this.
	if (CFG.timeoutMs > 0) {
		try {
			xhr.timeout = CFG.timeoutMs;
		} catch {
			// Some engine disagrees about where this is legal; the SW deadline still applies.
		}
	}

	try {
		xhr.send(asArrayBuffer(frame));
	} catch (err) {
		throw transportError(
			`synchronous filesystem request failed: ${(err as Error)?.message ?? err}`,
			err
		);
	}

	if (xhr.status !== SW_STATUS.ok) {
		// The body carries the reason for anything the service worker answered itself, and it is
		// plain text rather than a frame — decode it, because "answered 503" alone tells nobody
		// anything.
		let detail = "";
		try {
			const bytes = new Uint8Array(xhr.response ?? 0);
			if (bytes.length && bytes.length < 4096) {
				detail = ": " + new TextDecoder().decode(bytes);
			}
		} catch {
			// Not text; the status is all there is.
		}
		throw transportError(
			`synchronous filesystem request answered ${xhr.status}${detail}` +
				(xhr.status === SW_STATUS.noSession
					? " (no filesystem host attached — is the page still open?)"
					: xhr.status === SW_STATUS.timeout
						? " (the host did not answer in time)"
						: xhr.status === SW_STATUS.protoMismatch
							? " (service worker is a different build — reload the page)"
							: "")
		);
	}

	try {
		return unpack<V>(xhr.response, call, id);
	} catch (err) {
		// A frame that will not decode means the plumbing broke, not the filesystem — most
		// often a service worker that has been unregistered, in which case the XHR was answered
		// by the real server and this is somebody's 404 page.
		if (err instanceof FrameError) throw transportError(err.message, err);
		throw err;
	}
}

/**
 * How many times a *transport* failure is retried before giving up.
 *
 * A blocking request can fail with a network error — the service worker not answering, having
 * been evicted or restarted mid-flight — and the worker cannot tell whether the operation ran.
 * Retrying is only safe because the frame carries a `seq` and the host answers a repeat from its
 * reply record instead of re-executing, which makes the transport exactly-once. Without that,
 * a retried `append` would append twice.
 *
 * Only transport failures are retried. An operation that failed *in band* (ENOENT, EROFS) is an
 * answer, not a failure to deliver, and is returned as-is.
 */
const SYNC_RETRIES = 2;

function sendSync<V>(
	call: { op: string },
	parts: Uint8Array[] | undefined,
	kind: number
): Answer<V> {
	// Declared once, in ../../../wire/kinds.ts, rather than discovered by hanging. A kind whose
	// replies carry a `MessagePort` or a stream can never be answered by an XHR body, and the
	// caller is owed that as an error rather than as a park until the deadline.
	if (!isSyncCapable(kind) || isAsyncOnlyOp(call.op)) {
		throw permanentError(
			`ENOSYS: ${kindName(kind)}.${call.op} cannot be sent synchronously`
		);
	}
	// The same seq across attempts — that is what makes the retry safe.
	const id = wire.nextSeq();
	let last: unknown;
	for (let attempt = 0; attempt <= SYNC_RETRIES; attempt++) {
		try {
			return rawSync<V>(id, call, parts, kind);
		} catch (err) {
			if (!(err instanceof TransportError)) throw err;
			last = err;
		}
	}
	throw last;
}

/** One blocking round trip. The worker thread is parked for its whole duration. */
export function vfsSync<K extends VfsCall["op"]>(
	call: Extract<VfsCall, { op: K }>,
	parts?: Uint8Array[]
): VfsAnswer<K> {
	return sendSync<VfsResult<K>>(call, parts, KIND_FS);
}

/**
 * The same, for a process op.
 *
 * This is what `child_process.spawnSync` is built on, and it is the whole reason the process
 * host lives outside the worker: the caller parks here while the host runs the program on its
 * own event loop and answers when it is done. Nothing about that is new — it is exactly what
 * `readFileSync` has always done.
 */
export function procSync<K extends ProcessCall["op"]>(
	call: Extract<ProcessCall, { op: K }>,
	parts?: Uint8Array[]
): ProcAnswer<K> {
	return sendSync<ProcessResult<K>>(call, parts, KIND_PROCESS);
}

/**
 * The same, for a question a program asks its host by name.
 *
 * The point of the synchronous form is that it works from inside a synchronous call, which a
 * `MessagePort` never can: a worker parked in a blocking XHR will not read a port, so `chan.open`
 * — which hands over a port and gets out of the way — cannot answer a program that is already
 * parked. This can.
 */
export function chanSync(
	call: Extract<ChanCall, { op: "chan.call" }>,
	parts?: Uint8Array[]
): ChanAnswer {
	return sendSync<unknown>(call, parts, KIND_CHAN);
}

async function sendAsync<V>(
	call: { op: string },
	parts: Uint8Array[] | undefined,
	signal: AbortSignal | undefined,
	kind: number,
	attach?: unknown[]
): Promise<Answer<V>> {
	signal?.throwIfAborted();
	const id = wire.nextSeq();
	countHop(call.op, "async");

	// Every asynchronous filesystem operation is a live request as far as the event loop is
	// concerned, exactly as it is in libuv. Without this a program whose only pending work is
	// reading files is indistinguishable from one that has finished, and `drain` would let the
	// host tear it down mid-run.
	const release = keepalive.refOperation();
	try {
		const { decoded } = await wire.callWithSeq(id, kind, call, { parts, attach });
		return unpackDecoded<V>(
			decoded as DecodedFrame<WireReply>,
			call,
			id,
			signal
		);
	} finally {
		release();
	}
}

/** One asynchronous round trip, over the message port. */
export function vfsAsync<K extends VfsCall["op"]>(
	call: Extract<VfsCall, { op: K }>,
	parts?: Uint8Array[],
	signal?: AbortSignal
): Promise<VfsAnswer<K>> {
	return sendAsync<VfsResult<K>>(call, parts, signal, KIND_FS);
}

/** The same, for a process op. Everything but `spawnSync` goes this way. */
export function procAsync<K extends ProcessCall["op"]>(
	call: Extract<ProcessCall, { op: K }>,
	parts?: Uint8Array[],
	signal?: AbortSignal
): Promise<ProcAnswer<K>> {
	return sendAsync<ProcessResult<K>>(call, parts, signal, KIND_PROCESS);
}

/** The same, for a host question that is not being asked from inside a synchronous call. */
export function chanAsync(
	call: Extract<ChanCall, { op: "chan.call" }>,
	parts?: Uint8Array[],
	signal?: AbortSignal,
	/** Structured-cloned beside the call. See `CallOptions.attach`. */
	attach?: unknown[]
): Promise<ChanAnswer> {
	return sendAsync<unknown>(call, parts, signal, KIND_CHAN, attach);
}

/**
 * One blocking stdio round trip.
 *
 * `io.read` is the op that may park for as long as a person takes to answer a prompt,
 * which is what the `SwProgress` heartbeat exists for — the service worker's deadline is a
 * liveness check, not a limit on how long an op may take.
 */
export function stdioSync<K extends StdioCall["op"]>(
	call: Extract<StdioCall, { op: K }>,
	parts?: Uint8Array[]
): Answer<StdioResult<K>> {
	return sendSync<StdioResult<K>>(call, parts, KIND_STDIO);
}

/** The same, asynchronously. */
export function stdioAsync<K extends StdioCall["op"]>(
	call: Extract<StdioCall, { op: K }>,
	parts?: Uint8Array[]
): Promise<Answer<StdioResult<K>>> {
	return sendAsync<StdioResult<K>>(call, parts, undefined, KIND_STDIO);
}

/**
 * A stream over a path, from the host.
 *
 * An ordinary op whose reply carries an *attachment* — a real `ReadableStream`, transferred.
 * That is what makes it async-only: a stream is the one thing a completed answer is not, and
 * an XHR body cannot hold one. It used to be its own message type for exactly this reason;
 * attachments mean it no longer needs to be.
 *
 * `release` is the keepalive counterpart: an in-flight stream is a live handle as far as the
 * event loop is concerned, so a run whose only pending work is a stream must not be drained
 * out from under it. Idempotent, and the consumer MUST call it if it abandons the body.
 */
export async function openReadStream(
	path: string,
	range?: { start?: number; end?: number }
): Promise<{
	stream: ReadableStream<Uint8Array>;
	size?: number;
	release: () => void;
}> {
	return openStream({ path, start: range?.start, end: range?.end });
}

/** As above, over an open fd, so a handle's unflushed bytes are what gets streamed. */
export async function openReadStreamFd(
	fd: number,
	range?: { start?: number; end?: number }
): Promise<{
	stream: ReadableStream<Uint8Array>;
	size?: number;
	release: () => void;
}> {
	return openStream({ fd, start: range?.start, end: range?.end });
}

async function openStream(msg: {
	path?: string;
	fd?: number;
	start?: number;
	end?: number;
}): Promise<{
	stream: ReadableStream<Uint8Array>;
	size?: number;
	release: () => void;
}> {
	keepalive.ref();
	let released = false;
	const release = () => {
		if (released) return;
		released = true;
		keepalive.unref();
	};
	try {
		const { decoded, attachments } = await wire.call(KIND_FS, {
			op: "fs.openRead",
			...msg,
		});
		const header = decoded.header as WireReply;
		if (!header.result.ok) throw fromWireError(header.result.error);
		const stream = attachments[0] as ReadableStream<Uint8Array> | undefined;
		if (!stream) {
			throw transportError(
				"the host answered fs.openRead without a stream attached"
			);
		}
		const size = (header.result.value as { size?: number } | null)?.size;
		return { stream, size, release };
	} catch (err) {
		release();
		throw err;
	}
}

/** Bytes out of an answer, copied so nothing holds a view into the reply frame. */
export function answerBytes(answer: VfsAnswer<any>): Uint8Array {
	const part = answer.parts[0];
	return part ? new Uint8Array(part) : new Uint8Array(0);
}
