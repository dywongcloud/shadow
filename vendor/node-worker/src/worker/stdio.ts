// The worker's stdout and stderr buffer.
//
// Writes do not block and do not pay for a round trip. Bytes land here, get coalesced per
// fd, and leave as a *sideband* on whatever message goes next — a `readFileSync`, a
// `spawnSync`, an asynchronous `readFile`, anything. Only a program that prints without
// doing anything else ever sends a message of its own, and then only once per macrotask.
//
// The ordering property this buys is the whole point, and it is not achievable any other
// way. A synchronous fs call cannot await acknowledgement of previously-queued writes,
// because it is a synchronous function; there is no barrier to insert. But the pending
// bytes are *inside* the message that call sends, ahead of the call itself, so the host
// cannot observe the call before the output that preceded it. `spawnSync` with
// `stdio: "inherit"` gets the same guarantee for free.
//
// A leaf module: it imports `./wire` and the kind table and nothing else, so the fs
// subgraph can reach it without widening the module-init cycle ./node/fs/lazy-base.ts
// warns about.

import { KIND_STDIO } from "../wire/kinds";
import { wire } from "./wire";

type OutFd = 1 | 2;

/** One run of consecutive writes to the same fd, kept whole so it costs one push. */
interface Run {
	fd: OutFd;
	chunks: Uint8Array[];
	bytes: number;
}

let runs: Run[] = [];
let buffered = 0;
let scheduled = false;

/**
 * Bytes held before a flush is forced.
 *
 * Not a latency bound — the sideband drains on the next message whatever this is — but a
 * memory bound, for a program that prints steadily and never touches the filesystem.
 */
const MAX_BUFFERED = 64 * 1024;

// A macrotask, not a microtask. A microtask would run before the synchronous call that is
// about to carry these bytes anyway, turning the free ride into an extra message.
const realSetTimeout = globalThis.setTimeout;

/** Queue bytes for fd 1 or 2. Returns what was accepted, which is always everything. */
export function writeStdio(fd: OutFd, bytes: Uint8Array): number {
	if (bytes.length === 0) return 0;
	const last = runs[runs.length - 1];
	if (last && last.fd === fd) {
		last.chunks.push(bytes);
		last.bytes += bytes.length;
	} else {
		runs.push({ fd, chunks: [bytes], bytes: bytes.length });
	}
	buffered += bytes.length;

	if (buffered >= MAX_BUFFERED) {
		flushStdio();
		return bytes.length;
	}
	schedule();
	return bytes.length;
}

/** Whether anything is waiting to go out. */
export function stdioPending(): boolean {
	return buffered > 0;
}

/**
 * Hand the buffer to the wire's outbound queue.
 *
 * Registered as the queue's drain hook, so this runs at the moment any message is being
 * built — which is what makes the bytes ride it rather than race it.
 */
function contribute(): void {
	if (!runs.length) return;
	const pending = runs;
	runs = [];
	buffered = 0;
	for (const run of pending) {
		wire.outbound.enqueue(KIND_STDIO, "io.write", { fd: run.fd }, [
			concat(run.chunks, run.bytes),
		]);
	}
}

wire.outbound.onDrain = contribute;

function concat(chunks: Uint8Array[], total: number): Uint8Array {
	if (chunks.length === 1) return chunks[0];
	const out = new Uint8Array(total);
	let at = 0;
	for (const chunk of chunks) {
		out.set(chunk, at);
		at += chunk.length;
	}
	return out;
}

function schedule(): void {
	if (scheduled) return;
	scheduled = true;
	realSetTimeout(() => {
		scheduled = false;
		// Still here means nothing else went out this turn, so the bytes need a message of
		// their own. `post` drains the outbound queue, which is where `contribute` puts them.
		if (buffered > 0) flushStdio();
	}, 0);
}

/**
 * Send whatever is buffered now, without waiting for an answer.
 *
 * Not a guarantee that the terminal has it — that is `io.flush`, which the exit path uses
 * because it must not race the host tearing this worker down.
 */
export function flushStdio(): void {
	if (!runs.length) return;
	wire.post(KIND_STDIO, { op: "io.flush" });
}
