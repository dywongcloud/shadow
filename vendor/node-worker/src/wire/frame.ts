// The message codec: one framing, every kind, both transports.
//
// A call out of the worker becomes exactly one message, answered by exactly one
// message. The *same* bytes go over both transports — a blocking `XMLHttpRequest`
// through the service worker for `fs.readFileSync`, and a `postMessage` for
// `fs.promises.readFile` — and that is deliberate rather than incidental. The async
// path does not need framing at all (`structuredClone` handles a `Uint8Array` perfectly
// well), but using it means a framing or error-envelope bug cannot hide on one
// transport and not the other, which is the failure mode this whole boundary is most
// exposed to. It also makes the crossing zero-copy: one `ArrayBuffer`, transferred.
//
// **This is not multiplexing.** Every message is one self-delimiting frame — one
// `postMessage`, or one XHR body — correlated by its own `seq`. Nothing is interleaved,
// there are no windows, no per-kind flow control and no stream ids. The `kind` field is
// a namespace tag that picks a dispatcher, nothing more. The only real channels in the
// system are the `MessagePort`s handed to program code, and those arrive as an
// *attachment* on a reply rather than as anything this layer knows about.
//
// Bundled into all three outputs — the worker, the page and the service worker — so
// nothing here may import a package, touch `Buffer`, reach for node's `path`, or point
// into `src/worker/`. See ../vfs/entry.ts, which states the same rule for the same
// reason.

/**
 * Bumped on any incompatible change to the framing or any op set.
 *
 * Carried in the message *and* in the request URL, because a page and a service worker
 * can be different builds: a stale SW is a normal consequence of a redeploy, and the
 * skew has to be detectable before either side parses a body it may not understand.
 */
export const WIRE_PROTO = 2;

/**
 * `"NWM1"`, as a little-endian u32 — so the first four bytes read as ASCII in a hex
 * dump.
 *
 * This field is the difference between a legible failure and a baffling one. When the
 * service worker has been unregistered, the blocking XHR is answered by the *real*
 * server: a 404 page, or the SPA's `index.html`. Without a magic number that HTML
 * reaches `JSON.parse` and surfaces as a `SyntaxError` from nowhere. With it, the
 * worker reports "not a node-worker message (service worker gone?)".
 */
const FRAME_MAGIC = 0x314d574e;
const HEADER_OFFSET = 16;

/** Round up to the payload's alignment. */
function align8(n: number): number {
	return (n + 7) & ~7;
}

/** Thrown by {@link decodeFrame}. Distinguishable, because it means the plumbing broke. */
export class FrameError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "FrameError";
	}
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/**
 * ```
 * off  size  field
 *  0    4    magic       "NWM1", little-endian
 *  4    2    proto       WIRE_PROTO
 *  6    2    kind        which dispatcher; see ./kinds.ts, plus SIDEBAND_BIT
 *  8    4    headerLen   bytes of UTF-8 JSON
 * 12    4    payloadLen  sum of every part's length
 * 16    H    header      JSON
 *      pad   to an 8-byte boundary
 *           payload      the parts, concatenated
 * ```
 *
 * `payloadLen` is redundant with the header's `parts` array, and that is the point: a
 * body truncated in transit would otherwise produce a **silently short file**, which
 * is the worst failure mode a filesystem has. Checked against the actual byte length,
 * truncation is an error instead.
 *
 * The 8-byte payload alignment lets a part be viewed as any typed array without
 * copying it first.
 */
/**
 * High bit of the kind field: this message carries others.
 *
 * A flag rather than something read out of the header, because the alternative is parsing
 * every message's JSON twice — once to find out whether there is a sideband and once to
 * dispatch it. The filesystem alone sends tens of thousands of these and almost none of
 * them carry anything, so the common case has to be a bit test.
 */
export const SIDEBAND_BIT = 0x8000;

export function encodeFrame(
	header: object,
	parts?: readonly Uint8Array[],
	kind = 0
): Uint8Array {
	const headerBytes = encoder.encode(JSON.stringify(header));
	const payloadStart = align8(HEADER_OFFSET + headerBytes.length);
	let payloadLen = 0;
	if (parts) for (const part of parts) payloadLen += part.length;

	const out = new Uint8Array(payloadStart + payloadLen);
	const view = new DataView(out.buffer);
	view.setUint32(0, FRAME_MAGIC, true);
	view.setUint16(4, WIRE_PROTO, true);
	view.setUint16(6, kind, true);
	view.setUint32(8, headerBytes.length, true);
	view.setUint32(12, payloadLen, true);
	out.set(headerBytes, HEADER_OFFSET);

	let at = payloadStart;
	if (parts) {
		for (const part of parts) {
			out.set(part, at);
			at += part.length;
		}
	}
	return out;
}

/**
 * Which dispatcher a message belongs to, read without decoding it.
 *
 * Cheap on purpose: the filesystem alone sends tens of thousands of these, and routing
 * one must not cost a `JSON.parse` of its header.
 *
 * A buffer too short to hold a header has no kind to report. It answers 0 and lets
 * {@link decodeFrame} produce the real diagnostic — every caller decodes immediately
 * after routing, so the malformed message is one line away from a `FrameError` that
 * says what is actually wrong with it.
 */
export function frameKind(bytes: ArrayBuffer | Uint8Array): number {
	return rawKind(bytes) & ~SIDEBAND_BIT;
}

/** Whether anything is riding this message, without decoding it. */
export function hasSideband(bytes: ArrayBuffer | Uint8Array): boolean {
	return (rawKind(bytes) & SIDEBAND_BIT) !== 0;
}

function rawKind(bytes: ArrayBuffer | Uint8Array): number {
	const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
	if (u8.length < HEADER_OFFSET) return 0;
	return new DataView(u8.buffer, u8.byteOffset, u8.byteLength).getUint16(
		6,
		true
	);
}

export interface DecodedFrame<H> {
	header: H;
	parts: Uint8Array[];
	kind: number;
}

/**
 * The inverse, with every check that distinguishes "the handler said no" from "the
 * bridge broke". The `parts` are **views** into `bytes`, not copies.
 */
export function decodeFrame<H>(
	bytes: ArrayBuffer | Uint8Array | null
): DecodedFrame<H> {
	if (!bytes) throw new FrameError("empty response");
	const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
	if (u8.length < HEADER_OFFSET) {
		throw new FrameError(`response too short (${u8.length} bytes)`);
	}

	const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
	if (view.getUint32(0, true) !== FRAME_MAGIC) {
		throw new FrameError(
			"not a node-worker message (service worker gone or unregistered?)"
		);
	}
	const proto = view.getUint16(4, true);
	if (proto !== WIRE_PROTO) {
		throw new FrameError(
			`protocol mismatch: message is v${proto}, this build speaks v${WIRE_PROTO} — reload the page`
		);
	}
	const kind = view.getUint16(6, true) & ~SIDEBAND_BIT;

	const headerLen = view.getUint32(8, true);
	const payloadLen = view.getUint32(12, true);
	const payloadStart = align8(HEADER_OFFSET + headerLen);
	if (payloadStart + payloadLen !== u8.length) {
		throw new FrameError(
			`truncated message: expected ${payloadStart + payloadLen} bytes, got ${u8.length}`
		);
	}

	let header: H;
	try {
		header = JSON.parse(
			decoder.decode(u8.subarray(HEADER_OFFSET, HEADER_OFFSET + headerLen))
		);
	} catch (err) {
		throw new FrameError(`malformed message header: ${(err as Error).message}`);
	}

	const lengths =
		(header as { parts?: number[] })?.parts ?? (payloadLen ? [payloadLen] : []);
	const parts: Uint8Array[] = [];
	let at = payloadStart;
	for (const length of lengths) {
		parts.push(u8.subarray(at, at + length));
		at += length;
	}
	if (at !== u8.length) {
		throw new FrameError(
			`message parts do not cover the payload: ${at - payloadStart} of ${payloadLen} bytes`
		);
	}

	return { header, parts, kind };
}
