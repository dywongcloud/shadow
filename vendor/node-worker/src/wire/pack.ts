// Sidebands: messages that ride another message.
//
// The far side cannot always be *reached*. A worker parked inside a blocking XHR will
// never read a `postMessage`, so anything the host wants to tell it — a watch event, a
// cache invalidation — has to arrive on the reply it is already waiting for. The same
// holds outbound: a worker with buffered output that is about to park should send those
// bytes *with* the request rather than racing it, because after the park nothing of its
// own runs until the answer comes back.
//
// That is also what makes stdio affordable. `process.stdout.write` does not pay for a
// round trip of its own: the bytes ride whatever message was going anyway, and only a
// program that prints without doing anything else ever sends one on its own.
//
// The payload stays one flat array — the primary's `n` parts first, then each push's `np`
// in order — so ./frame.ts never has to know sidebands exist. It slices by
// `header.parts` and this module decides who the slices belong to.

import { encodeFrame, SIDEBAND_BIT } from "./frame";
import type { Push, WireReply, WireRequest } from "./message";

/** Messages waiting for something to ride on, with their bytes. */
export interface Outbound {
	pushes: Push[];
	parts: Uint8Array[];
}

/** A drained sideband, ready to be routed one message at a time. */
export interface UnpackedSideband {
	push: Push;
	parts: Uint8Array[];
}

/** Everything a sideband adds to a header, given the primary's own parts. */
function withSideband<H extends { parts?: number[]; n?: number }>(
	header: H,
	primary: readonly Uint8Array[] | undefined,
	outbound: Outbound | undefined
): { header: H; parts: Uint8Array[] } {
	const parts = [...(primary ?? [])];
	if (primary?.length) header.n = primary.length;
	if (outbound?.pushes.length) {
		header.n = primary?.length ?? 0;
		parts.push(...outbound.parts);
	}
	if (parts.length) header.parts = parts.map((p) => p.length);
	return { header, parts };
}

export function packRequest(
	kind: number,
	seq: number,
	call: unknown,
	parts?: readonly Uint8Array[],
	outbound?: Outbound
): Uint8Array {
	const header: WireRequest = { seq, call };
	if (outbound?.pushes.length) header.out = outbound.pushes;
	const packed = withSideband(header, parts, outbound);
	return encodeFrame(
		packed.header,
		packed.parts,
		outbound?.pushes.length ? kind | SIDEBAND_BIT : kind
	);
}

export function packReply(
	kind: number,
	body: WireReply,
	parts?: readonly Uint8Array[],
	outbound?: Outbound
): Uint8Array {
	if (outbound?.pushes.length) body.push = outbound.pushes;
	const packed = withSideband(body, parts, outbound);
	return encodeFrame(
		packed.header,
		packed.parts,
		outbound?.pushes.length ? kind | SIDEBAND_BIT : kind
	);
}

/**
 * Split a decoded message's parts between the message itself and its sidebands.
 *
 * `n` is absent on a message with no sidebands, in which case every part is the primary's
 * — which is what makes this backward-compatible with a header that never heard of them.
 */
export function unpackSidebands(
	header: { n?: number; out?: Push[]; push?: Push[] },
	parts: Uint8Array[]
): { primary: Uint8Array[]; sidebands: UnpackedSideband[] } {
	const pushes = header.out ?? header.push;
	if (!pushes?.length) return { primary: parts, sidebands: [] };

	const n = header.n ?? parts.length;
	const primary = parts.slice(0, n);
	const sidebands: UnpackedSideband[] = [];
	let at = n;
	for (const push of pushes) {
		const count = push.np ?? 0;
		sidebands.push({ push, parts: parts.slice(at, at + count) });
		at += count;
	}
	return { primary, sidebands };
}

/**
 * Just the message's own parts, with any sideband's stripped off.
 *
 * Every decode site needs this and none of them may skip it. A `writeFile` that happens to
 * carry buffered stdout would otherwise see two parts where it expects one — and
 * `fdWritev`, which writes *all* of them, would write the terminal's bytes into the file.
 */
export function primaryParts(
	header: { n?: number; out?: Push[]; push?: Push[] },
	parts: Uint8Array[]
): Uint8Array[] {
	// The overwhelmingly common case is no sideband at all, and it must not cost a copy.
	if (!header.out?.length && !header.push?.length) return parts;
	return parts.slice(0, header.n ?? parts.length);
}

/**
 * A queue of messages waiting for a ride.
 *
 * Draining is all-or-nothing per message and the queue is emptied by the drain, so a push
 * is delivered exactly once — there is no acknowledgement and no retry, because the
 * carrier message already has both.
 */
export class OutboundQueue {
	#pushes: Push[] = [];
	#parts: Uint8Array[] = [];
	/**
	 * Called at the start of every drain, for a producer that coalesces.
	 *
	 * Stdout is the reason. A program printing ten thousand lines would otherwise enqueue
	 * ten thousand pushes, each with its own header entry — several hundred kilobytes of
	 * JSON to carry the same bytes. Contributing at drain time instead lets the producer
	 * hand over one run per fd, however many writes went into it.
	 */
	onDrain: (() => void) | undefined;

	/** Whether a drain right now would produce nothing. Does not run `onDrain`. */
	get empty(): boolean {
		return this.#pushes.length === 0;
	}

	enqueue(
		kind: number,
		op: string,
		args?: unknown,
		parts?: Uint8Array[]
	): void {
		this.#pushes.push({ kind, op, args, np: parts?.length ?? 0 });
		if (parts?.length) this.#parts.push(...parts);
	}

	drain(): Outbound | undefined {
		this.onDrain?.();
		if (!this.#pushes.length) return undefined;
		const out = { pushes: this.#pushes, parts: this.#parts };
		this.#pushes = [];
		this.#parts = [];
		return out;
	}
}
