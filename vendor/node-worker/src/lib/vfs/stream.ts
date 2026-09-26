// Turning bytes already in hand into a `ProviderStream`.
//
// Used in two places, and it matters that it is the same code in both: the facade
// synthesizes a stream for any provider without `openRead`, and `unionProvider` needs it
// for the case where the layer it picked cannot stream but the other one can. Two copies
// would be two chances to get the `start`/`end` window subtly different, and `end` being
// *inclusive* (node's `createReadStream` contract) is exactly the kind of off-by-one that
// survives review.

import type { ProviderStream } from "../../vfs/provider";

/**
 * `range.end` is **inclusive**, as node's `createReadStream` has it, and both ends are
 * clamped so a window past EOF yields an empty stream rather than an error — which is what
 * node does for a `start` at or beyond the end of the file.
 */
export function streamOfBytes(
	bytes: Uint8Array,
	range?: { start: number; end?: number }
): ProviderStream {
	const start = Math.min(Math.max(range?.start ?? 0, 0), bytes.length);
	const end =
		range?.end === undefined
			? bytes.length
			: Math.min(Math.max(range.end + 1, start), bytes.length);
	// A copy, not a view: the consumer owns what comes out of a stream, and this one is
	// handed across a worker boundary where the backing buffer may be transferred.
	const slice = new Uint8Array(bytes.subarray(start, end));
	return {
		size: slice.length,
		stream: new ReadableStream<Uint8Array>({
			start(controller) {
				if (slice.length > 0) controller.enqueue(slice);
				controller.close();
			},
		}),
	};
}
