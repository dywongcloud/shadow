// What this runtime reports about memory, in one place.
//
// `process.memoryUsage()`, `v8.getHeapStatistics()` and `os.totalmem()/freemem()` are three views
// of the same question, and they have to agree: code routinely computes a used-memory *fraction*
// from a pair drawn across them.
//
// Reporting zero is the one thing worse than approximating. A zero total turns every such fraction
// into NaN or Infinity, and a low-memory guard that compares against it then either always fires or
// never does — silently, with no error to trace. So these return the best number available and a
// stable plausible one otherwise.

/** Chromium exposes a coarse heap readout; nothing else does. */
type HeapReadout = { used: number; total: number; limit: number };

const FALLBACK: HeapReadout = {
	used: 256 * 1024 * 1024,
	total: 512 * 1024 * 1024,
	limit: 2048 * 1024 * 1024,
};

export function heapReadout(): HeapReadout {
	const memory = (
		performance as unknown as {
			memory?: {
				usedJSHeapSize: number;
				totalJSHeapSize: number;
				jsHeapSizeLimit: number;
			};
		}
	).memory;
	if (
		memory &&
		typeof memory.totalJSHeapSize === "number" &&
		memory.totalJSHeapSize > 0
	) {
		return {
			used: memory.usedJSHeapSize,
			total: memory.totalJSHeapSize,
			limit: memory.jsHeapSizeLimit,
		};
	}
	return FALLBACK;
}

/**
 * What `os.totalmem()` reports.
 *
 * `navigator.deviceMemory` is the only device-level signal a browser gives, in GiB and deliberately
 * coarse (it buckets, and caps at 8). Absent it, 8 GiB is a defensible constant: the point is a
 * non-zero, stable denominator, not accuracy about hardware we cannot see.
 */
export function totalMemory(): number {
	const gib = (globalThis.navigator as { deviceMemory?: number } | undefined)
		?.deviceMemory;
	const gigabytes = typeof gib === "number" && gib > 0 ? gib : 8;
	return gigabytes * 1024 ** 3;
}

/** What `os.freemem()` reports: the share of `totalMemory()` this heap has not accounted for. */
export function freeMemory(): number {
	const { used, limit } = heapReadout();
	const total = totalMemory();
	// Never zero and never above the total, so a fraction computed from the pair stays in [0, 1].
	return Math.max(1024 * 1024, total - Math.min(used, limit, total));
}
