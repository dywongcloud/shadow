// Which dispatcher a message belongs to, and whether it can be asked synchronously.
//
// A kind is a *namespace*, not a channel — see the note at the top of ./frame.ts. It
// picks the dispatcher on the far side and nothing else: there is no per-kind ordering,
// buffering or flow control, and two kinds share a transport the way two URLs share a
// socket.
//
// Sync capability is declared here, once, rather than discovered at the call site. The
// hard part of calling out of a worker is doing it *synchronously* — a blocking XHR
// answered by the service worker, with the worker's thread parked for the whole trip —
// and that machinery is expensive to build but free to reuse. What it cannot do is
// carry a `MessagePort` or a `ReadableStream`, because an XHR body is bytes. So a kind
// whose replies hand back a handle is async-only, and the transport rejects a sync send
// of one with ENOSYS instead of hanging on a reply that can never come.

export const KIND_FS = 0;
export const KIND_PROCESS = 1;
export const KIND_STDIO = 2;
export const KIND_CHAN = 3;
export const KIND_CONTROL = 4;
export const KIND_PEER = 5;
export const KIND_EVENTS = 6;

export type Kind =
	| typeof KIND_FS
	| typeof KIND_PROCESS
	| typeof KIND_STDIO
	| typeof KIND_CHAN
	| typeof KIND_CONTROL
	| typeof KIND_PEER
	| typeof KIND_EVENTS;

/** Diagnostics only — the network panel, and the text of a routing failure. */
export const KIND_NAMES: Record<number, string> = {
	[KIND_FS]: "fs",
	[KIND_PROCESS]: "proc",
	[KIND_STDIO]: "io",
	[KIND_CHAN]: "chan",
	[KIND_CONTROL]: "ctl",
	[KIND_PEER]: "peer",
	[KIND_EVENTS]: "ev",
};

/**
 * Which kinds may be asked over the blocking transport.
 *
 * `CONTROL` is absent because nothing in it is worth blocking for: `execute` runs a
 * whole program, and the rest are notifications. `PEER` and `EVENTS` are absent
 * because their replies carry handles, which bytes cannot express.
 */
const SYNC_CAPABLE: ReadonlySet<number> = new Set([
	KIND_FS,
	KIND_PROCESS,
	KIND_STDIO,
	KIND_CHAN,
]);

export function isSyncCapable(kind: number): boolean {
	return SYNC_CAPABLE.has(kind);
}

/**
 * Ops that are async-only even though their kind is not.
 *
 * A kind is sync-capable when *most* of it is, and these are the exceptions: each answers
 * with a handle rather than a value, and a handle cannot travel in an XHR body. Declared
 * here beside the kind table so the two facts about "can this block" live together.
 */
const ASYNC_ONLY_OPS: ReadonlySet<string> = new Set([
	"fs.openRead",
	"chan.open",
]);

export function isAsyncOnlyOp(op: string): boolean {
	return ASYNC_ONLY_OPS.has(op);
}

export function kindName(kind: number): string {
	return KIND_NAMES[kind] ?? `kind${kind}`;
}
