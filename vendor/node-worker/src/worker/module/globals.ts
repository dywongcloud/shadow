// Node exposes a handful of values as globals that aren't part of the web
// platform. CJS gets them as function-scope parameters via the harness in
// ./cjs.ts, ESM has no per-module wrapper so it sees them via globalThis.
// Both paths read from the same NODE_GLOBALS object so they stay in sync.

import internalModules from "../node";
import {
	setTimeoutWrap,
	setIntervalWrap,
	setImmediateWrap,
	clearTimeoutWrap,
	clearIntervalWrap,
	clearImmediateWrap,
} from "../node/timers";
import {
	holder as asyncContextHolder,
	bindFrame,
} from "../node-core/internal-binding/async_context_frame";

// JS owns the name of the async-context holder global. The rewriter is told this
// name (see esm.ts / cjs.ts) and emits `<name>.frame` save/restore around each
// user `await`, so transformed code and the upstream `async_context_frame`
// binding read/write one shared storage slot. Keep in sync with the value passed
// into the rewriter.
export const ACF_GLOBAL = "__nw_acf";

// queueMicrotask, made async-context-aware: a microtask scheduled while ALS is in
// use runs under the frame current at schedule time. Fast-path until ALS is ever
// used (holder.active) so we add no wrapper/allocation for code that never touches
// AsyncLocalStorage; once active we always bind — even a currently-undefined frame
// must be captured so a microtask scheduled outside any context doesn't inherit a
// later-active one.
const rawQueueMicrotask = globalThis.queueMicrotask.bind(globalThis);
function queueMicrotaskWrap(callback: () => void) {
	if (!asyncContextHolder.active) return rawQueueMicrotask(callback);
	return rawQueueMicrotask(bindFrame(callback, asyncContextHolder.frame));
}

export const NODE_GLOBALS = {
	process: internalModules.process,
	Buffer: internalModules.buffer.Buffer,
	console: internalModules.console,
	global: globalThis,
	globalThis,
	setImmediate: setImmediateWrap,
	clearImmediate: clearImmediateWrap,
	queueMicrotask: queueMicrotaskWrap,
	setTimeout: setTimeoutWrap,
	clearTimeout: clearTimeoutWrap,
	setInterval: setIntervalWrap,
	clearInterval: clearIntervalWrap,
};

export type NodeGlobals = typeof NODE_GLOBALS;

// Install Node-only entries on globalThis for ESM (and any code that touches
// globalThis.X directly). The timer wrappers go here too so ESM Node code
// (and node-core sources that read globalThis.setInterval) gets the same
// ref-aware Timeout objects as CJS Node code. queueMicrotask is a web global but
// we override it with the context-aware wrapper so user microtasks propagate the
// async context.
let nodeOnly = [
	"process",
	"Buffer",
	"console",
	"global",
	"setImmediate",
	"clearImmediate",
	"queueMicrotask",
	"setTimeout",
	"clearTimeout",
	"setInterval",
	"clearInterval",
] as const;
for (let k of nodeOnly) {
	(globalThis as any)[k] = NODE_GLOBALS[k];
}

// Install the async-context holder under the JS-owned name so transformed user
// code (which runs in the SystemJS register / CJS harness scope and can only see
// globals) can reach the same frame slot the binding uses.
(globalThis as any)[ACF_GLOBAL] = asyncContextHolder;

// Propagate the async context across explicit `.then`/`.catch`/`.finally`. Native
// `await` bypasses this patched `then` (it uses V8's internal PromiseThen), so it
// is handled separately by the await-transform; the two are complementary and
// never double-restore the same continuation. `catch`/`finally` are spec'd to
// call `this.then`, so patching `then` covers them. Guarded against double-install
// (globals.ts is a singleton, but be defensive); the empty-frame fast path keeps
// the hot path free of wrapper allocations when ALS isn't active.
const kThenPatched = Symbol.for("node-worker.asyncContext.thenPatched");
if (!(Promise.prototype as any)[kThenPatched]) {
	const rawThen = Promise.prototype.then;
	// eslint-disable-next-line no-extend-native
	(Promise.prototype as any).then = function (
		this: Promise<any>,
		onFulfilled?: any,
		onRejected?: any
	) {
		if (asyncContextHolder.active) {
			const frame = asyncContextHolder.frame;
			if (typeof onFulfilled === "function")
				onFulfilled = bindFrame(onFulfilled, frame);
			if (typeof onRejected === "function")
				onRejected = bindFrame(onRejected, frame);
		}
		return rawThen.call(this, onFulfilled, onRejected);
	};
	Object.defineProperty(Promise.prototype, kThenPatched, {
		value: true,
		enumerable: false,
		writable: false,
		configurable: true,
	});
}
