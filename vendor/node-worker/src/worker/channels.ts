// Named `MessagePort`s the page hands to a program running in this worker.
//
// Everything else the page can say to a program goes through its stdio, and a control protocol
// multiplexed onto stdout is fine right up until the program prints something unexpected —
// which for a shell is not a hypothetical. A port avoids the question: structured clone,
// transferables, its own ordering, and no framing to get wrong.
//
// Ports may arrive before or after the program asks for one, and both orders are ordinary: the
// page usually opens the channel while the program is still being required. So a request that
// arrives first waits, and a port that arrives first is kept.

import { KIND_CHAN } from "../wire/kinds";
import { makeDispatcher } from "../wire/router";
import type { ChanCall } from "../wire/chan";
import { wire } from "./wire";
// Read at call time from the transport, which is a leaf module with no `process` and no
// primordials — the worker entry already imports it directly for that reason.
import { chanAsync, chanSync } from "./node/fs/transport";

const ready = new Map<string, MessagePort>();
const waiting = new Map<string, ((port: MessagePort) => void)[]>();

// Its own kind, registered here rather than folded into the control handler, so the
// module that owns the map is the module that fills it.
wire.router.register(
	KIND_CHAN,
	makeDispatcher<ChanCall>(KIND_CHAN, async (call, _parts, attachments) => {
		if (call.op !== "chan.open") {
			// `chan.call` runs in the other direction: it is how a *program* asks its host a
			// question, including from inside a synchronous call. Nothing answers it here.
			throw Object.assign(
				new Error(`ENOSYS: ${call.op} is not answered by the worker`),
				{ code: "ENOSYS" }
			);
		}
		const port = attachments[0] as MessagePort | undefined;
		if (!port) {
			throw Object.assign(
				new Error(`chan.open("${call.name}") carried no port`),
				{ code: "EINVAL" }
			);
		}
		deliverChannel(call.name, port);
	})
);

/** @internal Called when the page opens one. */
function deliverChannel(name: string, port: MessagePort): void {
	const pending = waiting.get(name);
	if (pending?.length) {
		waiting.delete(name);
		for (const resolve of pending) resolve(port);
		return;
	}
	ready.set(name, port);
}

/**
 * The port opened under `name` **if it is already here**, and undefined otherwise.
 *
 * The synchronous half of `channel`, and it exists for one case: `worker_threads.parentPort` has
 * to be a real port on the first line the module runs, and a promise cannot be. That is sound
 * because of an ordering the page controls — `chan.open` is awaited, so a channel opened before
 * `ctl.execute` is already in `ready` by the time the module is required. Anything that cannot
 * rely on that ordering should await `channel` instead.
 */
/**
 * The port the page opened under `name`, waiting for it if it has not arrived.
 *
 * Never rejects and never times out. A program that asks for a channel nobody opens is a
 * program waiting for its host, which is the same thing a server waiting for a connection is —
 * whoever wants a deadline can race one.
 */
/**
 * The port opened under `name` **if it is already here**, and undefined otherwise.
 *
 * The synchronous half of `channel`, and it exists for one case: `worker_threads.parentPort` has
 * to be a real port on the first line the module runs, and a promise cannot be. That is sound
 * because of an ordering the page controls — `chan.open` is awaited, so a channel opened before
 * `ctl.execute` is already in `ready` by the time the module is required. Anything that cannot
 * rely on that ordering should await `channel` instead.
 */
export function takeChannel(name: string): MessagePort | undefined {
	const already = ready.get(name);
	if (already) ready.delete(name);
	return already;
}

export function channel(name: string): Promise<MessagePort> {
	const already = ready.get(name);
	if (already) {
		ready.delete(name);
		return Promise.resolve(already);
	}
	return new Promise((resolve) => {
		const queue = waiting.get(name) ?? [];
		queue.push(resolve);
		waiting.set(name, queue);
	});
}

/**
 * Ask the host a question by name, and wait for its answer.
 *
 * The other half of `channel`. A port is the right shape when the program and the page have a
 * protocol of their own to run and want structured clone and transferables; this is the right
 * shape for one question with one answer, and it is the only shape available at all to a program
 * that is *parked inside a synchronous call* — see `callSync`.
 *
 * `args` is whatever the two sides agreed on, and bytes ride in `parts` rather than inside it.
 * A name the page has not registered comes back as ENOSYS naming the fix, rather than hanging.
 */
export async function call(
	name: string,
	args?: unknown,
	parts?: Uint8Array[],
	/**
	 * Values structured-cloned beside the call, for what neither JSON nor bytes can carry — a
	 * `Map`, a `Date`, a nested graph. They arrive as the handler's `attachments`.
	 */
	attach?: unknown[]
): Promise<unknown> {
	const answer = await chanAsync({ op: "chan.call", name, args }, parts, undefined, attach);
	return answer.value;
}

/**
 * The same, without returning to the event loop.
 *
 * This is the one thing `chan.open` structurally cannot do. A worker blocked in a synchronous
 * call runs nothing — no microtasks, no timers, no `postMessage` delivery — so a program in that
 * state can never read a port. It can still send, and this is how.
 *
 * Note the host may see a repeat: a blocking send retries the same `seq` on a transport failure,
 * and the page answers a repeat from its reply record rather than running the handler twice.
 */
export function callSync(
	name: string,
	args?: unknown,
	parts?: Uint8Array[]
): unknown {
	return chanSync({ op: "chan.call", name, args }, parts).value;
}
