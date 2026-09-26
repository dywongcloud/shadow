// node:worker_threads, over sibling `NodeWorker`s.
//
// A thread here is a second worker the *page* creates — only a page can, since a worker global
// has no `navigator.serviceWorker` and therefore no synchronous filesystem and no module resolver
// on the other side. That is the same reason `child_process` needs a host, and this asks the same
// way: a `chan.call` the page answers, under a name node-worker registers for itself so this
// works with no embedder setup at all.
//
// ## Messages do not go over the wire
//
// The page creates one `MessageChannel` and gives an end to each worker, so `postMessage` is a
// real port between two realms: structured clone, transferables, ordering, and the page not in
// the data path. That is worth stating because the alternative was seriously considered and is
// much worse — the wire carries a JSON header and byte `parts`, so putting `postMessage` on it
// would mean serialising arbitrary cloneable values to bytes, which is exactly the binary format
// ./v8.ts refuses to fake. Here there is no serialisation code at all.
//
// `MessagePort`, `MessageChannel` and `BroadcastChannel` are the platform's own, as they already
// were in the stub this replaces. They needed no help; only `Worker` did.
//
// ## What is not here
//
// `receiveMessageOnPort` needs a *synchronous* drain of a port, which browsers do not offer
// without `SharedArrayBuffer` — and this runtime deliberately has none, which is what lets it run
// without cross-origin isolation. `moveMessagePortToContext` needs vm contexts, which ./vm.ts
// explains do not exist here. Both keep throwing rather than pretending.

import { EmitterBase, ReadableBase } from "./fs/lazy-base";
import { call as chanCall, takeChannel } from "../channels";
import * as keepalive from "../keepalive";

declare const Buffer: any;

// ------------------------------------------------------------------- ports

/**
 * A browser `MessagePort` given node's `MessagePort` manners.
 *
 * The two are the same object with different surfaces: the platform's is an `EventTarget`
 * (`onmessage`, `addEventListener`), and node's is an `EventEmitter` (`.on("message")`, and the
 * message's *data* rather than a `MessageEvent`). Every worker_threads example ever written uses
 * the second, so a port handed to a program without this fails on its first line — which is
 * exactly how it presented: "Failed to load module", because `parentPort.on` is not a function.
 *
 * Patched onto the instance rather than wrapped in a class of our own, deliberately. A wrapper
 * would not be a `MessagePort` any more, and could therefore not be transferred — which is the
 * one property that makes any of this worth having.
 */
function nodeifyPort(port: MessagePort): MessagePort {
	const p = port as any;
	if (p.__nodeified) return port;
	p.__nodeified = true;

	/*
	 * A real `EventEmitter` does the work, rather than a listener map of our own: `once`,
	 * ordering, `prependListener`, the max-listeners warning and the special meaning of an
	 * unhandled `error` are all things node programs rely on and none of them are worth
	 * reimplementing. It is delegated to rather than inherited from because the port already
	 * exists — and it has to stay the *same object*, or it could not be transferred, which is the
	 * one property that makes this worth having at all.
	 */
	const emitter: any = new EmitterBase();
	const DELEGATED = [
		"on",
		"once",
		"off",
		"addListener",
		"removeListener",
		"removeAllListeners",
		"emit",
		"listenerCount",
		"listeners",
		"rawListeners",
		"eventNames",
		"prependListener",
		"prependOnceListener",
		"setMaxListeners",
		"getMaxListeners",
	];
	const STARTS = new Set(["on", "once", "addListener", "prependListener", "prependOnceListener"]);
	for (const name of DELEGATED) {
		p[name] = (...args: any[]) => {
			// A port with a listener is one somebody is waiting on. The platform does not
			// deliver until `start()`, and only `onmessage =` implies it.
			if (STARTS.has(name)) port.start();
			const result = emitter[name](...args);
			// Keep the chainable ones chaining on the port rather than on the emitter behind it.
			return result === emitter ? p : result;
		};
	}

	// node hands a listener the message's *data*; the platform hands it the event.
	port.addEventListener("message", (event) =>
		emitter.emit("message", (event as MessageEvent).data)
	);
	port.addEventListener("messageerror", (event) => emitter.emit("messageerror", event));

	return port;
}

/**
 * `MessageChannel`, with both ends given node's manners.
 *
 * The platform's own class otherwise, so `instanceof MessagePort` and transferring still work.
 */
class NodeMessageChannel {
	readonly port1: MessagePort;
	readonly port2: MessagePort;
	constructor() {
		const channel = new globalThis.MessageChannel();
		this.port1 = nodeifyPort(channel.port1);
		this.port2 = nodeifyPort(channel.port2);
	}
}

// ---------------------------------------------------------------- this thread

interface ThreadInit {
	threadId: number;
	workerData?: unknown;
	portChannel: string;
	ipc?: boolean;
}

let isMainThread = true;
let threadId = 0;
let parentPort: MessagePort | null = null;
let workerData: unknown = undefined;

/**
 * @internal Called from `ctl.execute` before the module runs.
 *
 * Before, not after, and that is the whole reason this is not just another async lookup:
 * `require("worker_threads").parentPort` is read at module scope constantly, and a promise cannot
 * stand in for a port there. The page makes it possible by awaiting `chan.open` before it sends
 * `ctl.execute`, so the port is already delivered and `takeChannel` finds it synchronously.
 */
export function initThread(init: ThreadInit | undefined): void {
	if (!init) return;
	isMainThread = false;
	threadId = init.threadId;
	workerData = init.workerData;
	const delivered = takeChannel(init.portChannel);
	parentPort = delivered ? nodeifyPort(delivered) : null;
	if (!parentPort) return;
	parentPort.start?.();

	/*
	 * A live parent port keeps the thread alive, as it does in node.
	 *
	 * Without this a thread that only registers a listener — which is the overwhelmingly common
	 * shape — finishes its top-level code, the run settles, and the host terminates it before a
	 * single message arrives. Top-level code finishing is not the program finishing, and for a
	 * worker whose whole job is to answer messages it is not even the beginning of it.
	 *
	 * Given up by `close()` or `unref()`, which is how a thread says it is done. `MessagePort` has
	 * no `ref`/`unref` in the browser, so they are added here to mean what node means by them.
	 */
	/*
	 * A `fork` child says the same thing through `process`, because that is where node puts it.
	 * Same port either way — `fork` and `Worker` differ in manners, not in mechanism.
	 */
	if (init.ipc) {
		const proc: any = (globalThis as any).process;
		if (proc) {
			proc.send = (message: unknown, ...rest: unknown[]) => {
				const callback = rest.find((r) => typeof r === "function") as
					| ((err: Error | null) => void)
					| undefined;
				parentPort!.postMessage(message);
				callback?.(null);
				return true;
			};
			proc.connected = true;
			proc.disconnect = () => {
				proc.connected = false;
				parentPort?.close();
			};
			// `process` is already an EventEmitter, so this is the whole of `process.on("message")`.
			(parentPort as any).on("message", (data: unknown) => proc.emit("message", data));
		}
	}

	let release: (() => void) | null = keptAlive();
	const port = parentPort as MessagePort & {
		unref?: () => void;
		ref?: () => void;
	};
	const close = port.close.bind(port);
	port.close = () => {
		release?.();
		release = null;
		close();
	};
	port.unref = () => {
		release?.();
		release = null;
	};
	port.ref = () => {
		release ??= keptAlive();
	};
}

function keptAlive(): () => void {
	return keepalive.refOperation();
}

// ------------------------------------------------------------------- children

/** Control-port handlers for realms this thread started, by the id the page knows them under. */
const children = new Map<number, (msg: ControlMessage) => void>();

/** What the page sends on the control port about a realm it started for us. */
export interface ControlMessage {
	id: number;
	type: "stdout" | "stderr" | "error" | "exit";
	code?: number;
	message?: string;
	stack?: string;
	bytes?: Uint8Array;
}

/** What a realm is started with. `child_process.fork` uses the same call `Worker` does. */
export interface RealmSpec {
	target: string;
	argv?: string[];
	env?: Record<string, string>;
	stdout?: boolean;
	stderr?: boolean;
	name?: string;
	workerData?: unknown;
	/** Present the port to the child as `process.send`, for `child_process.fork`. */
	ipc?: boolean;
}

/**
 * @internal Start a sibling realm and take its ports.
 *
 * Shared with `child_process.fork`, which is the same primitive with different manners on top:
 * node's `fork` is not a POSIX fork at all — it starts a new process at the top of a named module
 * with an IPC channel, copying nothing — so it needs exactly what a thread needs and no more.
 */
export async function spawnRealm(
	spec: RealmSpec,
	onControl: (msg: ControlMessage) => void
): Promise<{ id: number; threadId: number; port: MessagePort | null }> {
	const answer = (await chanCall(
		"nw:thread.spawn",
		{
			target: spec.target,
			argv: spec.argv,
			env: spec.env,
			stdout: !!spec.stdout,
			stderr: !!spec.stderr,
			name: spec.name,
			ipc: !!spec.ipc,
		},
		undefined,
		// Cloned rather than JSON-encoded, so a Map or a typed array survives the trip.
		[spec.workerData]
	)) as { id: number; threadId: number };

	children.set(answer.id, onControl);
	// Both ports were delivered before the page answered, so they are here now.
	attachControl();
	const delivered = takeChannel(`nw:thread:${answer.id}`);
	return {
		id: answer.id,
		threadId: answer.threadId,
		port: delivered ? nodeifyPort(delivered) : null,
	};
}

/** @internal Stop caring about a realm's control traffic. */
export function forgetRealm(id: number): void {
	children.delete(id);
}

/** @internal Ask the page to tear a realm down. */
export async function terminateRealm(id: number): Promise<void> {
	try {
		await chanCall("nw:thread.terminate", { id });
	} catch {
		// Already gone.
	}
}

/**
 * The page's channel for everything that is not a user message: `online`, `error`, `exit`, and a
 * child's output when the parent asked to see it.
 *
 * One per worker rather than one per thread — the ids tag the traffic, and a port per child would
 * be two more handles per spawn for no gain. Delivered by the page during the first spawn, before
 * it answers, so it is here by the time that answer arrives.
 */
let control: MessagePort | null = null;

const kHandle = Symbol("handleControl");

function attachControl(): void {
	if (control) return;
	control = takeChannel("nw:threads") ?? null;
	if (!control) return;
	control.onmessage = (event: MessageEvent) => {
		const msg = event.data as ControlMessage;
		children.get(msg.id)?.(msg);
	};
	control.start?.();
}

export interface WorkerOptions {
	workerData?: unknown;
	env?: Record<string, string>;
	argv?: unknown[];
	eval?: boolean;
	name?: string;
	transferList?: Transferable[];
	stdout?: boolean;
	stderr?: boolean;
	/** Accepted and ignored: there is no V8 heap to bound here. */
	resourceLimits?: unknown;
}

export class Worker extends EmitterBase {
	threadId = -1;
	stdout: any = null;
	stderr: any = null;
	stdin: any = null;

	#id: number | null = null;
	#port: MessagePort | null = null;
	#queued: Array<[unknown, Transferable[] | undefined]> = [];
	#release: (() => void) | null = null;
	#exited = false;
	#terminating: Promise<number> | null = null;

	constructor(filename: string | URL, options: WorkerOptions = {}) {
		super();

		/*
		 * A live thread is a reason for this program to stay alive, exactly as it is in node,
		 * where an unfinished worker keeps the parent's loop open. Without the hold, a parent
		 * whose only outstanding work is a thread looks finished and the host tears it down
		 * mid-run. `unref()` gives it up, as node's does.
		 */
		this.#release = keepalive.refOperation();

		if (options.stdout) this.stdout = new ReadableBase({ read() {} });
		if (options.stderr) this.stderr = new ReadableBase({ read() {} });

		void this.#start(String(filename), options);
	}

	async #start(filename: string, options: WorkerOptions) {
		try {
			const realm = await spawnRealm(
				{
					target: filename,
					argv: options.argv?.map(String),
					env: options.env,
					stdout: !!options.stdout,
					stderr: !!options.stderr,
					name: options.name,
					workerData: options.workerData,
				},
				(msg) => this[kHandle](msg)
			);

			this.#id = realm.id;
			this.threadId = realm.threadId;
			this.#port = realm.port;
			if (this.#port) {
				this.#port.on("message", (data: unknown) => this.emit("message", data));
			}
			for (const [value, transfer] of this.#queued.splice(0)) {
				this.#port?.postMessage(value, transfer ?? []);
			}
			this.emit("online");
		} catch (err) {
			this.emit("error", err as Error);
			this.#finish(1);
		}
	}

	/** @internal Control-port traffic for this child. */
	[kHandle](msg: ControlMessage) {
		if (msg.type === "stdout" || msg.type === "stderr") {
			const stream = msg.type === "stdout" ? this.stdout : this.stderr;
			stream?.push(msg.bytes ? Buffer.from(msg.bytes) : null);
			return;
		}
		if (msg.type === "error") {
			const err = new Error(msg.message ?? "worker error");
			if (msg.stack) err.stack = msg.stack;
			this.emit("error", err);
			return;
		}
		if (msg.type === "exit") this.#finish(msg.code ?? 0);
	}

	#finish(code: number) {
		if (this.#exited) return;
		this.#exited = true;
		if (this.#id !== null) forgetRealm(this.#id);
		this.stdout?.push(null);
		this.stderr?.push(null);
		// Closed here rather than left to collection: a port pair that outlives its thread is a
		// leak with no reader, and nothing else will ever close this one.
		try {
			this.#port?.close();
		} catch {
			// Already gone.
		}
		this.#port = null;
		this.unref();
		this.emit("exit", code);
	}

	postMessage(value: unknown, transferList?: Transferable[]) {
		// Before `online` the port does not exist yet. Queued rather than dropped, because
		// posting immediately after `new Worker(...)` is ordinary and node accepts it.
		if (!this.#port) {
			this.#queued.push([value, transferList]);
			return;
		}
		this.#port.postMessage(value, transferList ?? []);
	}

	terminate(): Promise<number> {
		if (this.#terminating) return this.#terminating;
		this.#terminating = (async () => {
			if (this.#id !== null) await terminateRealm(this.#id);
			this.#finish(1);
			return 1;
		})();
		return this.#terminating;
	}

	ref() {
		this.#release ??= keepalive.refOperation();
	}

	unref() {
		this.#release?.();
		this.#release = null;
	}
}

function unsupported(name: string) {
	return () => {
		throw new Error(
			`node:worker_threads.${name} is not supported in this runtime`
		);
	};
}

export const MessageChannel =
	NodeMessageChannel as unknown as typeof globalThis.MessageChannel;
export const MessagePort = globalThis.MessagePort;
export const BroadcastChannel = globalThis.BroadcastChannel;
export const SHARE_ENV = Symbol("SHARE_ENV");
export const resourceLimits = {};
export const moveMessagePortToContext = unsupported("moveMessagePortToContext");
export const receiveMessageOnPort = unsupported("receiveMessageOnPort");
export const setEnvironmentData = () => {};
export const getEnvironmentData = () => undefined;
export const markAsUntransferable = () => {};
export const isMarkedAsUntransferable = () => false;
export const markAsUncloneable = () => {};

/*
 * Getters, not values: `initThread` runs after this module is evaluated but before the program's
 * first line, so a snapshot taken here would be the pre-init one forever. That is how
 * `isMainThread` came to be hardcoded `false` — a value where a view was needed.
 */
export default {
	Worker,
	MessageChannel,
	MessagePort,
	BroadcastChannel,
	get isMainThread() {
		return isMainThread;
	},
	get parentPort() {
		return parentPort;
	},
	get threadId() {
		return threadId;
	},
	get workerData() {
		return workerData;
	},
	resourceLimits,
	SHARE_ENV,
	moveMessagePortToContext,
	receiveMessageOnPort,
	setEnvironmentData,
	getEnvironmentData,
	markAsUntransferable,
	isMarkedAsUntransferable,
	markAsUncloneable,
};
