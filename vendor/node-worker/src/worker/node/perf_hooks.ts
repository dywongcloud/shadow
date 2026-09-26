// `node:perf_hooks`.
//
// `performance` is the browser's, which already implements most of what node's does — `now`, the
// user-timing marks and measures, and the entry buffers. What node adds on top is the observer and
// the event-loop histogram, and code reaches for those without guarding, because in node they are
// always there.

const NativeObserver = (globalThis as { PerformanceObserver?: unknown })
	.PerformanceObserver;

/**
 * A histogram that reports nothing, for `monitorEventLoopDelay` and `createHistogram`.
 *
 * There is no event loop *delay* to measure that would mean what a caller expects: the worker's
 * turn queue is not node's libuv loop. Reporting zeroes is honest about that, and keeps the shape
 * callers destructure — `mean`, `percentile()`, `percentiles` — from being undefined.
 */
function emptyHistogram() {
	return {
		enable() {
			return false;
		},
		disable() {
			return false;
		},
		reset() {},
		count: 0,
		min: 0,
		max: 0,
		mean: 0,
		stddev: 0,
		exceeds: 0,
		percentile() {
			return 0;
		},
		percentiles: new Map<number, number>(),
	};
}

const perf_hooks = {
	performance: globalThis.performance,
	PerformanceObserver:
		NativeObserver ??
		class PerformanceObserver {
			observe() {}
			disconnect() {}
			takeRecords(): unknown[] {
				return [];
			}
			static supportedEntryTypes: string[] = [];
		},
	PerformanceEntry: (globalThis as any).PerformanceEntry,
	PerformanceMark: (globalThis as any).PerformanceMark,
	PerformanceMeasure: (globalThis as any).PerformanceMeasure,
	monitorEventLoopDelay: emptyHistogram,
	createHistogram: emptyHistogram,
	constants: {
		NODE_PERFORMANCE_GC_MAJOR: 4,
		NODE_PERFORMANCE_GC_MINOR: 1,
		NODE_PERFORMANCE_GC_INCREMENTAL: 8,
		NODE_PERFORMANCE_GC_WEAKCB: 16,
	},
};

export default perf_hooks as unknown as typeof import("node:perf_hooks");
