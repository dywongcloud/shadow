// process.exit.
//
// There is no way to stop a worker from inside it, so exiting is a two-part move:
// tell the page (which terminates us — the worker *is* the process, so its death is
// the process's) and throw, so that the statements after the `exit()` call do not
// run in the window before termination arrives. Neither half is sufficient alone.
// The throw is not the mechanism, it is only what makes the wait observable-free.
//
// Kept in its own module, importing nothing but ./conn, because node/process.ts
// imports it: process is `inject`-ed into upstream node-core, so anything it reaches
// for has to be free of the node subgraph or the cycle its own header warns about
// closes.

import { KIND_CONTROL } from "../wire/kinds";
import { wire } from "./wire";

/**
 * Unwinds the current run after `process.exit`. Caught by the `execute` handler,
 * which reports the code instead of the error.
 */
export class ProcessExit extends Error {
	constructor(readonly code: number) {
		super(`process.exit(${code})`);
		this.name = "ProcessExit";
	}
}

/**
 * Gets the program's buffered output across to the page before it is told we are exiting.
 *
 * Injected rather than imported: this module is reached from node/process.ts, whose header
 * explains why it cannot pull in ../console — the cycle runs back through node/stream.
 */
let flushOutput: (() => Promise<void>) | undefined;

export function setExitFlusher(flush: () => Promise<void>): void {
	flushOutput = flush;
}

/**
 * How long the flush may take before the exit is reported anyway.
 *
 * Generous, because the flush is usually instant and the thing it protects — the last line a
 * program printed — is worth waiting for.
 */
const FLUSH_GRACE_MS = 500;

/**
 * Tell the page an exit is under way, before anything that could stop it being reported.
 *
 * Called from `process.exit` ahead of the program's own `beforeExit`/`exit` handlers. Those run
 * synchronously and can block this thread for good — synchronous `fs` is a blocking request —
 * and nothing on this side can bound that. This is posted while the thread still turns, so the
 * page has the one fact it needs: an exit was intended. Silence after it means the cleanup
 * wedged, not that the program is still working.
 */
export function announceExit(code: number): void {
	try {
		wire.post(KIND_CONTROL, { op: "ctl.exiting", code });
	} catch {
		// If the wire will not take it, the exit itself will not get out either; the page's
		// deadline is what is left.
	}
}

export function requestExit(code: number): never {
	// The message goes *after* the flush, deliberately. The page terminates this worker
	// the moment it hears about the exit, so announcing it first would throw away
	// whatever the program had just printed — for a CLI that prints a summary and exits,
	// that is the entire summary.
	void (async () => {
		try {
			/*
			 * Bounded, and that bound is the difference between losing a line and losing the
			 * exit. The flush waits on the page's write chain, and a chunk only settles when
			 * whoever is reading `stdout` accepts it — so a reader that stops, or merely
			 * stalls, holds this open. The exit was reported from a callback, which means the
			 * `ProcessExit` throw went into a handler nobody is watching and this message is
			 * the *only* remaining evidence the process ended. A program that dies with
			 * nothing noticing leaves its run unsettled and everything waiting on it waiting
			 * for good.
			 *
			 * So the flush is a courtesy with a deadline. Reporting the exit is not optional.
			 */
			await Promise.race([
				flushOutput?.() ?? Promise.resolve(),
				new Promise<void>((resolve) => setTimeout(resolve, FLUSH_GRACE_MS)),
			]);
		} catch {
			// A stuck flush must not stop the exit from being reported at all.
		}
		// Fire-and-forget, and now literally so: the page terminates us on receipt, so a
		// reply may never be delivered and there is nothing to wait for. `post` mints a seq
		// and parks nobody on it.
		wire.post(KIND_CONTROL, { op: "ctl.exit", code });
	})();
	throw new ProcessExit(code);
}
