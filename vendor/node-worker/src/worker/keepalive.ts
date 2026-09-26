// Optional event-loop keepalive. Tracks "active handles" (refed timers, listening
// servers, etc.) the way Node's libuv does: while the count is non-zero, the
// worker's `execute` reply is held back from settling. The host page sees the
// reply only after the run has truly drained, mimicking Node's "exit when no
// more handles" behavior.
//
// Off by default. The handler opts in via the `keepalive` flag on init.

let enabled = false;
let refs = 0;
let waiters: Array<() => void> = [];

export function setKeepaliveEnabled(value: boolean) {
	enabled = value;
}

export function isKeepaliveEnabled(): boolean {
	return enabled;
}

export function ref() {
	if (!enabled) return;
	refs++;
}

export function unref() {
	if (!enabled) return;
	if (refs === 0) return;
	refs--;
	if (refs === 0) {
		let pending = waiters;
		waiters = [];
		for (let w of pending) w();
	}
}

export function refCount(): number {
	return refs;
}

// Native setTimeout, captured before any Node-side wrapping can shadow it.
// A macrotask scheduled with this does NOT enroll in the ref count, so the
// settle pass below can't keep itself alive.
const realSetTimeout = globalThis.setTimeout;

// --------------------------------------------------- promise-backed operations

let deferredUnrefs = 0;
let deferredScheduled = false;

function flushDeferredUnrefs() {
	deferredScheduled = false;
	let n = deferredUnrefs;
	deferredUnrefs = 0;
	for (let i = 0; i < n; i++) unref();
}

const NOOP = () => {};

/**
 * Ref the loop for an operation that completes through a **promise** rather than
 * through a handle, and hold the ref until the end of the turn its completion lands
 * in. Returns the release function; calling it more than once is harmless.
 *
 * ## Why a turn, and not just until the promise settles
 *
 * `ref`/`unref` were built for things libuv would model as handles — a timer, a
 * listening server, a socket. An in-flight *request* is different: libuv keeps the
 * loop alive from submission until its completion callback has run, and that callback
 * runs inside a loop turn during which the loop is unambiguously alive. `drain` below
 * treats one macrotask as one turn, so releasing a turn late is what reproduces that,
 * rather than being a fudge factor.
 *
 * ## Why this exists at all
 *
 * Every async filesystem operation in Node is a libuv request, so a program working
 * through files keeps the loop alive without doing anything else. Here a provider can
 * answer without yielding — a memory mount does — and then the whole operation is a
 * resolved promise that no handle ever represented. A build that reads a few hundred
 * files would look, to a handle-counting drain, exactly like a program that had
 * finished: `execute` settles, the host tears the worker down, and the work is lost
 * halfway through. Reffing the operation is what closes that gap.
 *
 * Releases are batched into one native timer rather than one per call, because the
 * fs surface goes through here on every single operation.
 */
export function refOperation(): () => void {
	if (!enabled) return NOOP;
	ref();
	let released = false;
	return () => {
		if (released) return;
		released = true;
		deferredUnrefs++;
		if (!deferredScheduled) {
			deferredScheduled = true;
			realSetTimeout(flushDeferredUnrefs, 0);
		}
	};
}

/**
 * Ref the loop across the platform promises that no handle represents.
 *
 * WASM compilation is the one that matters. It is genuine off-thread work — node
 * schedules it as a platform task and stays alive for it — but here it arrives as a
 * bare `WebAssembly.compile` promise, invisible to a handle count. That gap is
 * directly on the path this runtime cares most about: the two packages it redirects
 * bundlers to, `@rollup/wasm-node` and `esbuild-wasm`, both compile their own wasm on
 * first use, so *every* build has a stretch where the only pending work is a
 * `WebAssembly.compile`. Measured at ~14ms for rollup's parser, which is more than the
 * single macrotask `drain` allows — long enough for a build to be torn down between
 * loading its bundler and using it.
 *
 * Wrapping the global rather than our own loaders is deliberate: the compile that
 * caused this is issued by a package in `node_modules`, not by anything here.
 *
 * Installed unconditionally; the refs themselves no-op unless keepalive is enabled.
 */
export function installPlatformRefs(): void {
	for (let name of ["compile", "instantiate", "compileStreaming", "instantiateStreaming"] as const) {
		let original = WebAssembly[name] as ((...args: any[]) => Promise<any>) | undefined;
		if (typeof original !== "function") continue;
		(WebAssembly as any)[name] = function (this: unknown, ...args: any[]) {
			let release = refOperation();
			// `instantiate` accepts a Module and can then return synchronously-ish; going
			// through `Promise.resolve` keeps one code path for both overloads.
			return Promise.resolve(original.apply(this, args)).finally(release);
		};
	}
}

// Yield to a real macrotask. When it resolves, the microtask *and* process.
// nextTick queues have fully drained (both run before the next macrotask), so
// any synchronous- or microtask-scheduled re-ref has already happened.
function nextMacrotask(): Promise<void> {
	return new Promise<void>((r) => realSetTimeout(r, 0));
}

// Resolve once the run has quiesced the way libuv's event loop would stop:
// no active refed handles remain. `refs` is the analogue of libuv's refed
// active-handle count. Contributors while live: in-flight filesystem operations
// (via refOperation above), refed timers/immediates (via
// the timers binding), listening servers, connected/connecting sockets (which
// also back http, https, tls and the http2 client), in-flight fetches, and a
// reading stdin.
//
// Node re-checks loop liveness only after draining the microtask/nextTick
// queues following each callback, so we mirror that: wait for refs to reach 0,
// let one macrotask (i.e. a full microtask/nextTick drain) elapse, and confirm
// nothing re-refed. If something did (a queued tick scheduled a new timer, a
// timer callback re-armed, an immediate chained), loop and wait again.
//
// Faithfulness is bounded by ref coverage. The known remaining gap is response
// body streaming after a fetch() resolves (the fetch is reffed only through its
// headers phase), which can briefly read refs as 0 mid-stream if nothing else
// is live — narrow in practice.
export async function drain(): Promise<void> {
	if (!enabled) return;
	while (true) {
		while (refs > 0) {
			await new Promise<void>((r) => waiters.push(r));
		}
		await nextMacrotask();
		if (refs === 0) return;
	}
}
