// Backend for upstream `internal/async_context_frame.js`, which destructures
// `get/setContinuationPreservedEmbedderData` from `internalBinding('async_context_frame')`
// and uses them as `AsyncContextFrame.current()`/`.set()`.
//
// In real Node these map to `v8::Isolate::Get/SetContinuationPreservedEmbedderData`,
// which V8 auto-propagates across every await/.then/native-promise continuation.
// A browser Worker can't expose that, so here the "frame" is just a module-level
// holder slot: correct for synchronous scopes, and for timers (upstream
// internal/timers.js threads the frame itself). Propagation across native
// `await`, `.then`, `queueMicrotask`, and `process.nextTick` is added on top by
// the await-transform (in the rewriter) and the patches in module/globals.ts and
// node/process.ts — all of which mutate `holder.frame` below.
//
// `holder` is the single source of truth. globals.ts installs this SAME object
// on `globalThis[<name>]` so transformed user code can reach it by the JS-chosen
// global name; the binding methods here and that global therefore share storage.

export const holder: {
	/** the current AsyncContextFrame (a SafeMap of ALS-instance -> store), or undefined */
	frame: any;
	/**
	 * Set true the first time a non-undefined frame is installed (first als.run/
	 * enterWith). Until then no async context exists, so the microtask/then patches
	 * can skip all wrapping — zero overhead for code that never touches ALS.
	 */
	active: boolean;
	/**
	 * Reinstall a saved frame and return `value`. Emitted by the await-transform as
	 * `holder.restore(holder.frame, await e)`: the saved frame is captured as the
	 * first argument before the await suspends, and this runs synchronously in the
	 * resumed continuation, so the context is correct immediately after the await.
	 */
	restore(saved: any, value: any): any;
} = {
	frame: undefined,
	active: false,
	restore(saved: any, value: any) {
		holder.frame = saved;
		return value;
	},
};

// Run `fn` with `frame` installed as the current frame, restoring the prior
// frame afterward — the JS equivalent of `AsyncContextFrame.exchange` around a
// call. Used by the microtask/nextTick patches to bind a callback to the frame
// that was current when it was scheduled.
export function bindFrame<F extends (...args: any[]) => any>(fn: F, frame: any): F {
	return function (this: any, ...args: any[]) {
		const prev = holder.frame;
		holder.frame = frame;
		try {
			return fn.apply(this, args);
		} finally {
			holder.frame = prev;
		}
	} as F;
}

export default {
	getContinuationPreservedEmbedderData: () => holder.frame,
	setContinuationPreservedEmbedderData: (value: any) => {
		holder.frame = value;
		if (value !== undefined) holder.active = true;
	},
};
