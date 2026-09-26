// JS implementation of node's `timers` internal binding. Upstream
// `internal/timers.js` + `lib/timers.js` do all the timer bookkeeping (the
// PriorityQueue of TimersList, the refed-timeout count in `timeoutInfo[0]`, the
// immediate queue and `immediateInfo` fields) and funnel into this binding at
// exactly the points libuv would be touched:
//
//   - scheduleTimer(msecs)  -> arm the (single) libuv timer handle
//   - getLibuvNow()         -> uv_now(loop) - timer_base
//   - toggleTimerRef(bool)  -> uv_ref/uv_unref the timer handle
//   - toggleImmediateRef()  -> uv_ref/uv_unref the immediate check handle
//   - setupTimers(cbs)      -> register the JS callbacks libuv invokes
//
// There is no libuv here, so we drive `processTimers`/`processImmediate`
// ourselves off the browser's macrotask queue, and we synthesize libuv's
// "keep the loop alive while a refed handle exists" by tapping the two handle
// ref states into the worker keepalive count. See src/worker/keepalive.ts.

import * as keepalive from "../../keepalive";

// Indexes into `immediateInfo`, matching Environment::ImmediateInfo::Fields
// (src/env.h): [kCount, kRefCount, kHasOutstanding]. Only the first two are read
// here; kHasOutstanding (index 2) is written by upstream internal/timers.js.
const kCount = 0;
const kRefCount = 1;

// Shared with internal/timers.js and lib/timers.js via `internalBinding`.
const immediateInfo = new Uint32Array(3);
const timeoutInfo = new Int32Array(1);

// Native timer primitives, captured before node's wrappers shadow the globals
// (this module loads via the first `internalBinding('timers')`, which happens
// while node/timers.ts is importing — before module/globals.ts overwrites the
// global setTimeout). Using the wrapped ones would recursively enroll our own
// scheduling into the keepalive count.
const realSetTimeout = globalThis.setTimeout.bind(globalThis);
const realClearTimeout = globalThis.clearTimeout.bind(globalThis);

// uv_now-equivalent: monotonic integer milliseconds relative to a fixed base.
const timerBase = performance.now();
function getLibuvNow(): number {
	return Math.trunc(performance.now() - timerBase);
}

let processTimersCb: ((now: number) => number) | undefined;
let processImmediateCb: (() => void) | undefined;

// --- keepalive: mirror the ref state of libuv's timer handle and immediate
// check handle. Node keeps the event loop alive while either is refed; here
// each contributes at most one to the worker keepalive count. ---

let timerHandleRefed = false;
function setTimerHandleRef(v: boolean) {
	if (v === timerHandleRefed) return;
	timerHandleRefed = v;
	if (v) keepalive.ref();
	else keepalive.unref();
}

let immediateHandleRefed = false;
function setImmediateHandleRef(v: boolean) {
	if (v === immediateHandleRefed) return;
	immediateHandleRefed = v;
	if (v) keepalive.ref();
	else keepalive.unref();
}

function reportUncaught(e: unknown) {
	// Surface like an uncaught exception in a callback without derailing the
	// driver, matching the existing worker convention.
	queueMicrotask(() => {
		throw e;
	});
}

// --- timer handle: a single re-armable browser timeout, mirroring the one
// uv_timer_t node schedules for the whole timer subsystem. ---

let scheduledTimer: ReturnType<typeof realSetTimeout> | undefined;

function scheduleTimer(msecs: number) {
	// Upstream only calls this to bring the next wakeup *sooner* (insert) or to
	// re-arm for the next expiry (from runTimers); either way replace the
	// pending wakeup, like uv_timer_start on the same handle.
	if (scheduledTimer !== undefined) realClearTimeout(scheduledTimer);
	scheduledTimer = realSetTimeout(runTimers, msecs);
}

// Mirror Environment::RunTimers: call processTimers(now), then act on its
// return value — 0 = no timers remain, >0 = next expiry with refed timers
// still pending, <0 = next expiry but only unrefed timers remain.
function runTimers() {
	scheduledTimer = undefined;
	const cb = processTimersCb;
	if (cb === undefined) return;

	const now = getLibuvNow();
	let expiry = 0;
	// A throwing timer callback aborts the pass; the JS side has already removed
	// that timer, so re-invoking processes the rest (progress is guaranteed).
	for (;;) {
		try {
			expiry = cb(now);
			break;
		} catch (e) {
			reportUncaught(e);
		}
	}

	if (expiry !== 0) {
		const duration = Math.abs(expiry) - getLibuvNow();
		scheduleTimer(duration > 0 ? duration : 1);
	}
	// Reconcile: refed timers keep the loop alive iff expiry > 0. Timers that
	// fired to completion decremented timeoutInfo[0] directly (no toggleTimerRef),
	// so this is where that drop is reflected into keepalive.
	setTimerHandleRef(expiry > 0);
}

function toggleTimerRef(ref: boolean) {
	setTimerHandleRef(ref);
}

// --- immediate check handle: node runs processImmediate once per loop turn
// while immediates are queued. There is no per-`setImmediate` binding call, but
// every Immediate is born refed, so toggleImmediateRef(true) fires whenever the
// queue goes from "no refed immediate" to "has one" — our cue to schedule a
// drain. The drain self-reschedules while the queue is non-empty. ---

let immediateDrainScheduled = false;

function scheduleImmediateDrain() {
	if (immediateDrainScheduled) return;
	immediateDrainScheduled = true;
	realSetTimeout(drainImmediates, 0);
}

function drainImmediates() {
	immediateDrainScheduled = false;
	const cb = processImmediateCb;
	if (cb === undefined) return;

	if (immediateInfo[kCount] > 0) {
		try {
			cb();
		} catch (e) {
			// processImmediate resumes remaining items via its outstandingQueue on
			// the next pass; surface the error and keep draining.
			reportUncaught(e);
		}
	}

	// Immediates scheduled while draining run on the next turn (node's next loop
	// iteration). Reconcile keepalive with whether a refed immediate remains
	// (the fired ones decremented immediateInfo[kRefCount] directly).
	setImmediateHandleRef(immediateInfo[kRefCount] > 0);
	if (immediateInfo[kCount] > 0) scheduleImmediateDrain();
}

function toggleImmediateRef(ref: boolean) {
	setImmediateHandleRef(ref);
	if (ref) scheduleImmediateDrain();
}

// Registers the JS callbacks the "libuv" side invokes. Node's bootstrap calls
// this; the worker bypasses bootstrap, so node/timers.ts calls it explicitly
// with getTimerCallbacks(runNextTicks).
function setupTimers(
	processImmediate: () => void,
	processTimers: (now: number) => number
) {
	processImmediateCb = processImmediate;
	processTimersCb = processTimers;
}

export { setupTimers };

export default {
	immediateInfo,
	timeoutInfo,
	getLibuvNow,
	scheduleTimer,
	toggleTimerRef,
	toggleImmediateRef,
	setupTimers,
};
