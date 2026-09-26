// Node timers, backed by upstream node-core. The whole implementation — the
// Timeout/Immediate classes, the PriorityQueue of timer lists, ref-counting via
// `timeoutInfo`/`immediateInfo`, refresh/unref/hasRef, `Symbol.toPrimitive` and
// numeric-id lookup via `knownTimersById` — comes from `internal/timers.js` and
// `timers.js` unchanged. This module just supplies the "libuv" side:
//
//   1. Register the JS callbacks the timer/immediate handles invoke, by calling
//      getTimerCallbacks(runNextTicks) and handing the pair to the binding's
//      setupTimers (node's bootstrap does this; the worker bypasses bootstrap).
//   2. Re-export the public API under the *Wrap names the globals installer and
//      timers/promises wrappers consume.
//
// Keepalive integration lives entirely in internal-binding/timers.ts, at the
// toggleTimerRef/toggleImmediateRef taps — nothing here touches the ref count.

// @ts-ignore - CJS upstream module, no types
import internalTimers from "node-core:internal/timers";
// @ts-ignore - CJS upstream module, no types
import publicTimers from "node-core:timers";
import timersBinding from "../node-core/internal-binding/timers";
import { runNextTicks } from "./process";
import timersPromises from "./timers-promises";

// Wire the driver: getTimerCallbacks builds processImmediate/processTimers
// (closing over runNextTicks for inter-callback tick draining); setupTimers
// stores them where the binding's scheduleTimer/drainImmediates invoke them.
const { processImmediate, processTimers } = internalTimers.getTimerCallbacks(runNextTicks);
timersBinding.setupTimers(processImmediate, processTimers);

export const setTimeoutWrap = publicTimers.setTimeout;
export const setIntervalWrap = publicTimers.setInterval;
export const setImmediateWrap = publicTimers.setImmediate;
export const clearTimeoutWrap = publicTimers.clearTimeout;
export const clearIntervalWrap = publicTimers.clearInterval;
export const clearImmediateWrap = publicTimers.clearImmediate;

// The `node:timers` module. `promises` is a lazy getter (like upstream) so the
// timers <-> timers/promises import cycle stays inert until first accessed.
const timers = {
	setTimeout: setTimeoutWrap,
	clearTimeout: clearTimeoutWrap,
	setInterval: setIntervalWrap,
	clearInterval: clearIntervalWrap,
	setImmediate: setImmediateWrap,
	clearImmediate: clearImmediateWrap,
	get promises() {
		return timersPromises;
	},
};

export default timers as unknown as typeof import("node:timers");
