// `node:v8`, as far as a browser can answer it.
//
// The heap numbers come from ./memory.ts, which is also what `process.memoryUsage()` reports, so the
// two agree — code that compares a `getHeapStatistics()` field against a `memoryUsage()` one is
// common enough that disagreeing would be its own bug.
//
// The serialization half of `node:v8` is deliberately absent rather than faked: `serialize` and
// `deserialize` have an exact binary format, and a plausible-looking substitute would corrupt data
// that round-trips through it. Absence is a clean failure.

import { heapReadout } from "./memory";

function unsupported(name: string) {
	return () => {
		throw new Error(`node:v8.${name} is not supported in this runtime`);
	};
}

const v8 = {
	getHeapStatistics() {
		const { used, total, limit } = heapReadout();
		return {
			total_heap_size: total,
			total_heap_size_executable: 0,
			total_physical_size: total,
			total_available_size: Math.max(0, limit - used),
			used_heap_size: used,
			heap_size_limit: limit,
			malloced_memory: 0,
			peak_malloced_memory: 0,
			does_zap_garbage: 0,
			number_of_native_contexts: 1,
			number_of_detached_contexts: 0,
			total_global_handles_size: 0,
			used_global_handles_size: 0,
			external_memory: 0,
		};
	},
	getHeapSpaceStatistics(): unknown[] {
		return [];
	},
	getHeapCodeStatistics() {
		return {
			code_and_metadata_size: 0,
			bytecode_and_metadata_size: 0,
			external_script_source_size: 0,
			cpu_profiler_metadata_size: 0,
		};
	},
	// Accepting and ignoring flags is right: they tune a VM we are not running, and throwing would
	// break callers that set one opportunistically.
	setFlagsFromString() {},
	cachedDataVersionTag() {
		return 0;
	},
	writeHeapSnapshot: unsupported("writeHeapSnapshot"),
	getHeapSnapshot: unsupported("getHeapSnapshot"),
	serialize: unsupported("serialize"),
	deserialize: unsupported("deserialize"),
	Serializer: undefined,
	Deserializer: undefined,
	takeCoverage() {},
	stopCoverage() {},
};

export default v8 as unknown as typeof import("node:v8");
