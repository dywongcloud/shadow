import { Console } from "./console";
import { DistributiveOmit, genuid } from "../util";
import { handlePeerConnect, handlePeerServe } from "./peer";
import {
	broadcastLocalFsEvent,
	handleFsEvents,
	type FsEventsFeed,
} from "./fsevents";
import { fromWireError, toWireError } from "../wire/error";
import { WIRE_PROTO } from "../wire/frame";
import type { NodeFsCapabilities } from "../wire/fs";
import { makeDispatcher } from "../wire/router";
import { PortEndpoint } from "../wire/endpoint";
import {
	KIND_CHAN,
	KIND_CONTROL,
	KIND_EVENTS,
	KIND_FS,
	KIND_PEER,
	KIND_PROCESS,
	KIND_STDIO,
} from "../wire/kinds";
import type { ControlCall, ControlResult, NodeNetInit } from "../wire/control";
import type { PeerCall } from "../wire/peer";
import type { EventsCall } from "../wire/events";
import type { StdioCall } from "../wire/stdio";
import type { ChanCall } from "../wire/chan";
import type { PortEnvelope } from "../wire/message";
import { handleProcessFrame } from "./process/dispatch";
import { createReplayCache, type ReplayCache } from "../wire/replay";
import type { ProcessProvider } from "../process/provider";
import { SYNC_TIMEOUT_MS } from "../wire/sw";
import { NodeVfs, randomSid, type MemListEntry } from "./vfs/index";
import { attachSession, SyncFsUnavailable, type Attachment } from "./sw";

export { Console, type TTYState } from "./console";
// `MemListEntry` (from ./vfs) replaces the old `NodeMemListEntry` wire type.
export type { MemListEntry as NodeMemListEntry } from "./vfs/index";

// The filesystem, and everything needed to extend it.
//
// `NodeVfs` is the host-side namespace a worker runs on; a consumer mounts providers on it,
// populates memory mounts and reads them back, all synchronously. `VfsProvider` is the
// extension point — implement it with ordinary async code (OPFS, the File System Access api,
// IndexedDB, a fetch) and mount it.
export {
	NodeVfs,
	createMemoryProvider,
	createPuterProvider,
	createDirectoryHandleProvider,
	ensureDirectoryHandleAccess,
	unionProvider,
	createCachingProvider,
	type CachingProvider,
	type VfsCacheOptions,
	type VfsCacheFreshness,
	type NodeVfsOptions,
	type MemEntry,
	type MemoryMount,
	type MemListEntry,
	type MemListOptions,
	type WriteTarget,
	type DirectoryHandleProviderOptions,
	type MountContext,
	type FsEvents,
} from "./vfs/index";
export type { VfsProvider, ProviderStream } from "../vfs/provider";
export type { FsEntry, Listing, ReaddirOpts, WireCtx } from "../vfs/entry";
export type { MountSnapshot, NodeFsCapabilities } from "../wire/fs";
export { fsError, VfsError, type WireError } from "../vfs/errno";
export { SyncFsUnavailable } from "./sw";
export type { NodeNetInit } from "../wire/control";

// Running programs, and the extension point for it.
//
// The mirror of the filesystem's: something on *this* side answers, and the worker reaches it
// over the transport it already has — including the blocking one, which is what lets
// `child_process.spawnSync` work at all from inside a worker.
export type {
	ProcessProvider,
	ProcCtx,
	SpawnRequest,
	ProcEvent,
	ExitStatus,
	SpawnSyncResult,
} from "../process/provider";

/**
 * The worker called `process.exit`, so it has been terminated.
 *
 * Every promise in flight at that moment rejects with this, including the `execute`
 * that was running — a dead worker cannot answer, and leaving those pending would
 * hang the caller forever. `code` is the program's exit status, so a caller driving
 * a CLI can report it rather than treat the rejection as a failure.
 */
/**
 * What answers one named `chan.call` from a program. See `NodeWorker.registerChannelHandler`.
 *
 * `args` is whatever the program sent and `parts` any bytes that rode alongside it. The
 * resolved value goes back as the answer; a throw goes back as an error, `code` intact.
 */
export type ChannelHandler = (
	args: unknown,
	parts: Uint8Array[],
	/** Values the caller sent by structured clone rather than as JSON. See `CallOptions.attach`. */
	attachments: readonly unknown[]
) => unknown | Promise<unknown>;

export class WorkerExitError extends Error {
	constructor(readonly code: number) {
		super(`worker exited with code ${code}`);
		this.name = "WorkerExitError";
	}
}

export interface NodeWorkerOptions {
	keepalive?: boolean;
	/**
	 * `dist/sw.js`, however your bundler spells its URL. Required for synchronous `fs`.
	 *
	 * It must sit where its default registration scope covers `workerURL` — which
	 * `import swURL from "node-worker/sw?url"` gives you for free, since bundlers emit it
	 * beside the worker. Measured across Blink, Gecko and WebKit: the worker being in scope
	 * is what decides interception, and the *page* needs no control at all, so no
	 * `Service-Worker-Allowed` header is involved.
	 */
	swURL?: string;
	/** Registration scope. Defaults to the directory `swURL` sits in. */
	swScope?: string;
	/**
	 * The filesystem this worker runs on. A fresh one per worker by default, because every
	 * worker has always had its own `/tmp`, its own overlay and its own memory mounts — pass
	 * an instance to share a namespace between workers deliberately.
	 */
	vfs?: NodeVfs;
	/** ms a blocked synchronous `fs` call may wait before giving up with EIO. 0 disables. */
	syncTimeoutMs?: number;
	/**
	 * Reject if synchronous `fs` turns out to be unavailable. **Default true.**
	 *
	 * The module resolver is synchronous end to end, so without this transport there is no
	 * `require` and nothing runs at all. Failing at startup with the reason named beats every
	 * program failing to resolve its first import with something unrecognizable.
	 *
	 * Set false only if you genuinely intend to run with `fs.promises` alone; a `*Sync` call
	 * will then throw ENOSYS naming why.
	 */
	requireSyncFs?: boolean;
	/**
	 * The network, for a worker started **without** a puter token.
	 *
	 * A token is otherwise what buys network access: the wisp relay credentials behind
	 * `fetch`/sockets are minted by `wisp/relay-token/create`, and a peer is identified to the
	 * signaller by that same token. Supply these instead and the worker never calls
	 * api.puter.com at all.
	 *
	 *   // Any wisp relay, dialed as given.
	 *   net: { wispUrl: MY_RELAY_URL, peerToken: crypto.randomUUID() }
	 *
	 *   // A relay that authenticates over the wisp password extension, which is how the
	 *   // puter relays are reached: `wisp/relay-token/create`'s `server` and `token`.
	 *   net: { wispUrl: server, relayToken: token, peerToken: crypto.randomUUID() }
	 *
	 * Ignored when a puter token is passed, which mints all of it for itself.
	 */
	net?: NodeNetInit;
	/**
	 * Where to load epoxy from, without the trailing slash. `<base>/full.js` is imported
	 * and `<base>/full.wasm` fetched. Defaults to a pinned build on puter's CDN.
	 *
	 * epoxy is the whole network stack — TCP, TLS and everything above it — and it is loaded
	 * **lazily**, from inside the worker, the first time something actually opens a socket. A
	 * worker that never touches the network never fetches it, which is what keeps a
	 * worker-per-child-process page from paying a CDN import and a wasm compile per command.
	 *
	 * The tradeoff is where an unreachable base shows up: not at startup, but as a failure on
	 * the first connection, naming this option. Point this at a copy you serve yourself to
	 * remove the dependency, or at a local build to test a change to epoxy itself.
	 *
	 * Cross-origin bases must be CORS-readable: the `import()` is a module fetch and the
	 * wasm arrives through `fetch`.
	 */
	epoxyBase?: string;
	/**
	 * What runs programs for `node:child_process`. Without one, every call throws ENOSYS.
	 *
	 * Deliberately not built in. A shell is megabytes that most workers never spawn, and where
	 * it *runs* is the embedder's decision — a second `NodeWorker` this page owns is the shape
	 * that keeps a command's stdout separate from the agent's, which the same worker cannot.
	 */
	process?: ProcessProvider;
	/**
	 * Handlers for `chan.call`, by name — the same thing `registerChannelHandler` adds, for the
	 * ones a worker might ask for before the page has had a chance to register them.
	 */
	channels?: Record<string, ChannelHandler>;
	/**
	 * Whether this worker's stdio is a terminal. Default `true`.
	 *
	 * Set it `false` when the output is being captured rather than shown to someone: node
	 * colourises on `process.stdout.getColorDepth()`, which reports truecolor for a TTY, so a
	 * captured `console.log(1 + 1)` arrives as `\x1b[33m2\x1b[39m` and compares unequal to `2`
	 * for reasons nothing in the output makes visible.
	 *
	 * `worker.console.setIsTTY()` changes it later; this is the same thing without the round
	 * trip, which matters when the worker is short-lived enough for one to show.
	 */
	isTTY?: boolean;
}

/** Options shared by `import` and `require`: what the run's process looks like. */
export interface RunOptions {
	/** Complete `process.argv`, `argv[0]` included. Defaults to `["node", path]`. */
	argv?: string[];
	/** Replaces `process.env` wholesale, `TERM` included. */
	env?: Record<string, string>;
	/**
	 * Run this as a `worker_threads` thread rather than a top-level program: sets `threadId`,
	 * `workerData` and `parentPort`, and makes `isMainThread` false. The port named by
	 * `portChannel` must already have been delivered with `openChannelWith`.
	 *
	 * `workerData` is structured-cloned rather than JSON-encoded, so a `Map` or a typed array
	 * arrives as itself.
	 */
	thread?: {
		threadId: number;
		workerData?: unknown;
		portChannel: string;
		/** Present the port as `process.send`, which is what `child_process.fork` promises. */
		ipc?: boolean;
	};
}

/**
 * How long exit listeners have to read worker-side state before it goes away.
 *
 * Their one window, and not a veto: the program has already ended, so anything still waiting
 * on the run is waiting on this.
 */
const EXIT_LISTENER_GRACE_MS = 1_000;

/**
 * How long a program's own `beforeExit`/`exit` handlers get before the exit is reported anyway.
 *
 * Generous: they legitimately write files, and a slow mount is not a wedged one. What it bounds
 * is the case where they never finish at all, which on this runtime takes the thread with them.
 */
const EXIT_TEARDOWN_GRACE_MS = 10_000;

let workers = 0;

export class NodeWorker {
	/**
	 * The host-side filesystem. Mount providers on it, populate memory mounts, read them back
	 * — all synchronously, since none of it crosses a boundary any more.
	 */
	readonly vfs: NodeVfs;
	/** Whether synchronous `fs` works, and if not, why. Resolves with `ready`. */
	readonly capabilities: Promise<NodeFsCapabilities>;

	private worker!: Worker;
	/**
	 * Set by `terminate()`. Checked on both sides of the service-worker await in `ready`,
	 * because until the worker exists `terminate()` has nothing to stop — and without this a
	 * terminate during startup would be a silent no-op followed by a worker appearing.
	 */
	#terminated = false;
	#attachment: Attachment | undefined;
	/** Armed by `ctl.exiting`, cleared by `ctl.exit`. See the handler. */
	#exitDeadline: ReturnType<typeof setTimeout> | undefined;
	/**
	 * The worker's filesystem channel. `port2` is transferred with `init`; this side keeps
	 * `port1` and answers frames on it. See {@link VfsInit.port} for why it is separate from
	 * the general channel.
	 */
	/**
	 * The worker's own channel for messages of every kind.
	 *
	 * Named for the wire rather than the filesystem because it stopped being the
	 * filesystem's: process, stdio and control messages ride the same port to the same
	 * router. Keeping them off the general worker channel is what stops a reply queueing
	 * behind the console output of the very program waiting for it.
	 */
	#wireChannel: MessageChannel | undefined;
	/**
	 * What answers `node:child_process`. Absent ⇒ every call throws ENOSYS naming the fix,
	 * which is what it did unconditionally before there was an SPI to register.
	 */
	#process: ProcessProvider | undefined;
	/**
	 * This worker's sync-fs transport session, and deliberately *this worker's* rather than the
	 * filesystem's.
	 *
	 * It namespaces the virtual URLs the blocking XHR posts to — `{syncPrefix}v{proto}/{sid}/{id}-{op}`
	 * — where `id` is a request counter each worker starts from zero. Taken from the vfs, two workers
	 * sharing one filesystem emitted byte-identical URLs and the service worker answered the second
	 * from the first: a sub-worker would ask for its own entry file, be told the right path, and run
	 * the previous worker's bytes. A `NodeVfs` is explicitly allowed to back several workers, so the
	 * id that separates their traffic cannot be a property of it.
	 */
	readonly #syncSid: string = randomSid();

	/**
	 * The exactly-once record for process ops. See ../wire/replay.ts.
	 *
	 * Held here rather than on the vfs because it belongs to this worker and nothing else: the
	 * filesystem's record lives on a `NodeVfs` that may outlive several workers and so has to be
	 * forgotten per session, while this one dies with the object that owns it. `proc.spawnSync`
	 * is the reason it exists — it is the only process op sent over the retrying transport.
	 */
	readonly #procReplies: ReplayCache = createReplayCache();

	/** What this worker was built from. See the constructor and `spawnSibling`. */
	readonly #spawnConfig: {
		workerURL: string;
		puterToken: string | undefined;
		cwd: string;
		options: NodeWorkerOptions | undefined;
	};

	/**
	 * What answers a program's `chan.call`, by name. See `registerChannelHandler`.
	 *
	 * Read at call time rather than captured, so a handler registered after the worker started
	 * works — the same reason `#process` is a field rather than a closure.
	 */
	readonly #channelHandlers = new Map<string, ChannelHandler>();

	/**
	 * What node-worker answers for itself — thread spawning, today.
	 *
	 * Separate from `#channelHandlers` and consulted first, so these are available with no
	 * embedder setup and an embedder cannot take one of the names by accident.
	 */
	readonly #builtinChannels = new Map<string, ChannelHandler>();

	/** Threads this worker's programs started, by the id they know them under. */
	readonly #threads = new Map<number, NodeWorker>();
	#nextThreadId = 1;
	/** This worker's end of the lifecycle channel. Opened on the first spawn. */
	#threadControl: MessagePort | undefined;
	/**
	 * The kind → dispatcher table, built once and shared by every inbound path.
	 *
	 * Routing used to be a ternary written out twice — once for the service-worker relay,
	 * once for the postMessage path — and the two disagreed: the relay dispatched under
	 * `#syncSid` and the other under the vfs's own default, so one worker's synchronous and
	 * asynchronous calls landed in two different replay records while sharing a single
	 * sequence counter, and `terminate()` only ever closed one of them.
	 */
	readonly #wire = new PortEndpoint();
	/**
	 * Things the worker asked for that live on **this** side of the boundary.
	 *
	 * Peer servers and connections, and the fs-events channel: each owns a socket the
	 * worker cannot see, and each was designed to be closed by the worker asking — over its
	 * port, or by its last watcher leaving. A terminated worker asks for nothing, so every
	 * one of them outlived the worker that created it. For a peer server that is worse than
	 * a leak: while its signaller socket is open the signaller still has that
	 * `(credential, port)` registered, so a `listen(5173)` in the *next* worker competes
	 * with a dead one, and a viewer resolving that port can be handed the corpse.
	 *
	 * Entries drop themselves when they close on their own, so this tracks what is actually
	 * live rather than everything ever created.
	 */
	#hostResources = new Set<{ close(): void }>();

	/**
	 * Unsubscribes for the vfs listeners this worker registered in its constructor.
	 *
	 * A shared `NodeVfs` outlives the workers on it, so a listener that is never removed is a
	 * leak with a live edge back into a terminated worker — and it still runs on every mutation.
	 * One dead pair per worker is invisible when a page makes one; a page that spawns a worker
	 * per child process makes them by the hundred.
	 */
	#unsubscribes: (() => void)[] = [];
	/** The change feed, while anything in the worker is watching. */
	#events: FsEventsFeed | undefined;
	/** Whether `terminate` may dispose of `vfs`, or only end this session on it. */
	#ownsVfs = false;
	private exitListeners = new Set<(code: number) => void | Promise<void>>();
	ready: Promise<void>;
	console: Console;

	/**
	 * Register a host-side resource this worker owns, so `terminate` can close it.
	 *
	 * A resource that reports its own closing (`closed`) deregisters itself, which is what
	 * keeps this from growing without bound over a worker that opens many peer
	 * connections. Anything that arrives *after* `terminate` — a handshake that was still
	 * in flight when the worker died — is closed immediately rather than added, because
	 * nothing will ever come back for it.
	 */
	#track<T extends { close(): void; closed?: Promise<void> }>(resource: T): T {
		if (this.#terminated) {
			try {
				resource.close();
			} catch (err) {
				globalThis.console.warn(
					"[node-worker] failed to close a late host resource",
					err
				);
			}
			return resource;
		}

		this.#hostResources.add(resource);
		resource.closed?.then(
			() => this.#hostResources.delete(resource),
			() => this.#hostResources.delete(resource)
		);
		return resource;
	}

	/**
	 * Post to the worker if it is still there.
	 *
	 * A W2P handler is allowed to terminate the worker — the `exit` one does exactly
	 * that — so by the time its reply is ready there may be nothing to reply to. The
	 * worker sent that message fire-and-forget for precisely this reason, so dropping
	 * the reply is correct; throwing on a detached `worker` would only turn it into an
	 * unhandled rejection.
	 */
	/**
	 * Ask the worker a control question.
	 *
	 * @internal
	 */
	async control<R>(call: ControlCall): Promise<R> {
		return this.#call<R>(call);
	}

	async #call<R>(
		call: unknown,
		opts?: { transfer?: Transferable[]; attach?: unknown[] }
	): Promise<R> {
		const { decoded } = await this.#wire.call(KIND_CONTROL, call, opts);
		const header = decoded.header;
		if (!header.result.ok) throw fromWireError(header.result.error);
		return header.result.value as R;
	}

	/**
	 * Start a worker, awaiting everything that has to be in place first.
	 *
	 * The recommended entry point, because a constructor cannot reject and a service worker
	 * that fails to register is a startup error worth surfacing rather than a filesystem that
	 * mysteriously hangs later.
	 */
	static async create(
		workerURL: string,
		puterToken: string | undefined,
		cwd: string,
		options?: NodeWorkerOptions
	): Promise<NodeWorker> {
		const worker = new NodeWorker(workerURL, puterToken, cwd, options);
		await worker.ready;
		return worker;
	}

	/**
	 * `puterToken` may be empty, which starts an **anonymous** worker: nothing here calls
	 * api.puter.com, the default filesystem is a memory root with no puterfs under it, and the
	 * network comes from `options.net` instead. See `NodeNetInit`.
	 */
	constructor(
		workerURL: string,
		puterToken: string | undefined,
		cwd: string,
		options?: NodeWorkerOptions
	) {
		/*
		 * Everything needed to make another worker like this one.
		 *
		 * Kept rather than destructured and dropped, because "a second worker configured the way
		 * the first one was" is the one thing every consumer of a sibling needs and the one thing
		 * only the page can do — a worker global has no `navigator.serviceWorker`, so no
		 * synchronous filesystem and no module resolver on the other side. See `spawnSibling`.
		 */
		this.#spawnConfig = { workerURL, puterToken, cwd, options };

		let keepalive = !!options?.keepalive;
		// No token, no puterfs — `NodeVfs` mounts its memory overlay at "/" on its own when it
		// is given no puter credentials, which is the whole of what an anonymous root is.
		const vfs =
			options?.vfs ??
			new NodeVfs(puterToken ? { puter: { token: puterToken } } : {});
		this.vfs = vfs;
		// A filesystem this worker made is this worker's to dispose of; one handed in
		// belongs to whoever handed it in and may well outlive several workers. The
		// distinction matters now that a `NodeVfs` holds a change-feed subscription,
		// and with it a socket — disposing a shared one would take that away from
		// every other worker on it.
		this.#ownsVfs = !options?.vfs;
		this.#process = options?.process;
		for (const [name, handler] of Object.entries(options?.channels ?? {})) {
			this.#channelHandlers.set(name, handler);
		}
		this.#installThreadHost();
		// One table, registered once. Both dispatchers run under `#syncSid` so a worker's
		// synchronous and asynchronous calls share one replay record — they share one sequence
		// counter, so anything else splits it.
		this.#wire.router.register(KIND_FS, (frame: ArrayBuffer | Uint8Array) =>
			vfs.handleFrame(frame as ArrayBuffer, this.#syncSid)
		);
		this.#wire.router.register(
			KIND_PROCESS,
			(frame: ArrayBuffer | Uint8Array) =>
				handleProcessFrame(this.#process, frame, {
					cache: this.#procReplies,
					sid: this.#syncSid,
				})
		);

		// NOT created here. The service worker has to be registered and active *before* the
		// worker script is fetched, because that fetch is when the browser decides whether this
		// worker is controlled — and if it is not, every synchronous `fs` call goes to the
		// network instead of to the filesystem. So creation moves into `ready` below, which
		// every public method already awaits.
		let capabilities!: (c: NodeFsCapabilities) => void;
		this.capabilities = new Promise((r) => (capabilities = r));

		let console = new Console(this, options?.isTTY ?? true);
		this.console = console;

		// Control, in the worker-to-page direction. The other direction — `ctl.init`,
		// `ctl.execute` and friends — is answered by the worker's own dispatcher.
		//
		// There is no `hi` any more. The page used to wait for one before sending `init`,
		// which is a handshake the platform already provides: a `postMessage` to a worker
		// whose script has not finished evaluating is queued, not dropped.
		this.#wire.router.register(
			KIND_CONTROL,
			makeDispatcher<ControlCall>(KIND_CONTROL, async (msg) => {
				if (msg.op === "ctl.tty") {
					console.handleTTYState({ isRaw: msg.isRaw, echo: msg.echo });
					return;
				}
				if (msg.op === "ctl.exiting") {
					/*
					 * The program said it is on its way out, and its own cleanup runs next.
					 *
					 * That cleanup is synchronous and can block this worker's thread for good,
					 * which would mean no `ctl.exit` ever arrives and a run that never settles.
					 * Nothing inside the worker can bound that. This can: the intent is known,
					 * so silence past the deadline is a wedged teardown rather than a program
					 * still doing its job.
					 *
					 * Armed only by the program declaring an exit, which is what keeps it away
					 * from a worker that is merely parked in a long blocking call — that worker
					 * never said any of this.
					 */
					clearTimeout(this.#exitDeadline);
					this.#exitDeadline = setTimeout(() => {
						globalThis.console.warn(
							"[node-worker] exit handlers did not finish; terminating"
						);
						this.terminate(new WorkerExitError(msg.code));
					}, EXIT_TEARDOWN_GRACE_MS);
					return;
				}
				if (msg.op === "ctl.exit") {
					clearTimeout(this.#exitDeadline);
					// The worker is the process, so `process.exit` is the process dying and the
					// worker goes with it. Listeners are awaited *before* the terminate: a
					// consumer whose state lives inside the worker — a memory mount it treats
					// as a replica, say — gets its one chance to read it out here, and there is
					// no second one.
					//
					// Bounded, because that chance must not become a veto. A listener that
					// never settles would leave the worker running and the pending run
					// unsettled — the program is already gone, so what waits is whoever asked
					// for it, for good. A listener that is too slow loses its read; a listener
					// that hangs must not cost the exit.
					await Promise.race([
						(async () => {
							for (let listener of [...this.exitListeners]) {
								try {
									await listener(msg.code);
								} catch (err) {
									// `globalThis`-qualified: the constructor shadows `console`
									// with the worker's stdio Console, which has no `error`.
									globalThis.console.error(
										"[node-worker] exit listener failed",
										err
									);
								}
							}
						})(),
						new Promise<void>((resolve) =>
							setTimeout(resolve, EXIT_LISTENER_GRACE_MS)
						),
					]);
					this.terminate(new WorkerExitError(msg.code));
					return;
				}
				throw Object.assign(
					new Error(`control op ${msg.op} is not for the page`),
					{ code: "ENOSYS" }
				);
			})
		);

		/*
		 * Named questions from a program to its host.
		 *
		 * The mirror of `openChannel`, and the half that was declared and never built. A port is
		 * the right shape when the two sides have a protocol to run; this is the right shape for
		 * one question with one answer, and it is the only shape that works at all while the
		 * worker is parked inside a synchronous call, because a parked worker never reads a port.
		 */
		this.#wire.router.register(
			KIND_CHAN,
			makeDispatcher<ChanCall>(
				KIND_CHAN,
				async (call, parts, attachments) => {
					if (call.op !== "chan.open") {
						// Built-ins first, so an embedder cannot shadow thread spawning by
						// registering a handler under one of node-worker's own names.
						const handler =
							this.#builtinChannels.get(call.name) ??
							this.#channelHandlers.get(call.name);
						if (!handler) {
							throw Object.assign(
								new Error(
									`no handler for channel "${call.name}" — call ` +
										`worker.registerChannelHandler(${JSON.stringify(call.name)}, fn), ` +
										"or pass `channels` to NodeWorker.create"
								),
								{ code: "ENOSYS" }
							);
						}
						return { value: await handler(call.args, parts, attachments) };
					}
					// `chan.open` travels page → worker; the worker answers it. Arriving here
					// means a frame went the wrong way, which is worth saying rather than
					// silently treating as a question with no handler.
					throw Object.assign(
						new Error("chan.open is not for the page"),
						{ code: "ENOSYS" }
					);
				},
				() => "chan",
				// Sync-capable, so a retried send must not run the handler again.
				() => ({ cache: this.#procReplies, sid: this.#syncSid })
			)
		);

		// Stdio. The kind the merge was for: `readSync(0)` and `writeSync(1)` throw EBADF
		// without it, because stdio lived on an envelope that could never be synchronous.
		//
		// Writes normally arrive as *sidebands* on some other message rather than as calls of
		// their own — the router delivers those before the message they rode on, which is what
		// keeps a program's output ahead of the call that carried it.
		this.#wire.router.register(
			KIND_STDIO,
			makeDispatcher<StdioCall>(KIND_STDIO, async (msg, parts) => {
				if (msg.op === "io.write") {
					console.writeStdio(msg.fd, parts[0] as Uint8Array<ArrayBuffer>);
					return;
				}
				if (msg.op === "io.flush") {
					await console.flushStdio();
					return;
				}
				const { bytes, eof } = await console.readStdio(
					msg.length,
					msg.blocking
				);
				return { value: { eof }, parts: bytes.length ? [bytes] : undefined };
			})
		);

		// Peers. Both ops answer with handles rather than values — a stream pair for a
		// connection, a port for a listener — which is what attachments are for and what
		// makes the kind async-only.
		this.#wire.router.register(
			KIND_PEER,
			makeDispatcher<PeerCall>(KIND_PEER, async (msg) => {
				if (msg.op === "peer.connect") {
					let peer = this.#track(
						await handlePeerConnect(
							msg.token,
							{ code: msg.code, port: msg.port },
							msg.signaller,
							msg.ice,
							msg.anon
						)
					);
					return {
						transfer: [
							peer.readable as unknown as Transferable,
							peer.writable as unknown as Transferable,
						],
					};
				}
				let server = this.#track(
					await handlePeerServe(
						msg.token,
						msg.port,
						msg.signaller,
						msg.ice,
						msg.anon
					)
				);
				return { value: { code: server.code }, transfer: [server.port] };
			})
		);

		// Backs node:fs's watchers. The socket lives here rather than in the worker so it's
		// a plain browser WebSocket (the worker's global is epoxy's WISP-tunnelled override)
		// and so one connection serves every watcher across every worker on the token.
		//
		// What the worker gets back is no longer a port. Events are pushed as messages, so
		// they can ride the reply a *parked* worker is already waiting for — which a port
		// could never do, and which is why the runtime used to need a second delivery path
		// for exactly that case.
		this.#wire.router.register(
			KIND_EVENTS,
			makeDispatcher<EventsCall>(KIND_EVENTS, async (msg) => {
				if (msg.op === "ev.subscribe") {
					this.#events?.close();
					let feed = this.#track(
						handleFsEvents(msg.token, msg.apiOrigin, (push) =>
							this.#wire.post(KIND_EVENTS, push)
						)
					);
					this.#events = feed;
					return {
						value: { connected: feed.connected, polling: feed.polling },
					};
				}
				if (msg.op === "ev.close") {
					this.#events?.close();
					this.#events = undefined;
					return;
				}
				throw Object.assign(
					new Error(`event op ${msg.op} is not for the page`),
					{ code: "ENOSYS" }
				);
			})
		);

		this.#wireChannel = new MessageChannel();
		// A tight loop over messages of every kind, and deliberately nothing else. The router
		// answers every failure in band — a message it cannot even parse comes back as a
		// node-shaped error, and one for a kind nobody registered comes back as ENOSYS — so it
		// never rejects, and there is no second error shape for this channel to invent.
		//
		// There used to be one: `{id, error: {message}}`, which is how a dispatcher failure
		// reached the worker with its `code` and `errno` stripped off. The reply is a `WireError`
		// like every other now.
		this.#wireChannel.port1.onmessage = async (e: MessageEvent) => {
			const { f } = e.data as PortEnvelope;
			const out = await this.#answerFrame(f);
			try {
				const envelope: PortEnvelope = { f: out };
				this.#wireChannel!.port1.postMessage(envelope, [out]);
			} catch {
				// The port closed between the request and the answer — the worker is going away,
				// and its own deadline covers anything still parked on this.
			}
		};

		// Every local mutation, forwarded to whatever is watching — deliberately including ones
		// this worker caused itself, which it has already seen on their reply frame.
		//
		// Filtering those out reads as the obvious optimization and is a trap: the same
		// `causedBy` covers a host write (an editor save, with no reply to ride) and a sibling
		// worker sharing these providers, so filtering drops exactly the events nothing else
		// delivers. A duplicate costs a redundant rebuild; a drop costs a dev server that has
		// silently stopped noticing edits.
		this.#unsubscribes.push(
			vfs.onFsEvent((event) => broadcastLocalFsEvent(event))
		);

		// A mount appearing or disappearing changes answers the worker gives without asking —
		// whether a path's backend has a real positioned read, for one — so re-push it.
		this.#unsubscribes.push(
			vfs.onMountsChanged((mounts) => {
				if (this.#terminated || !this.worker) return;
				this.#call({ op: "ctl.mounts", mounts }).catch(() => {
					// The worker is going away; nothing to tell.
				});
			})
		);

		this.ready = (async () => {
			if (this.#terminated) throw new Error("terminated before start");

			let syncPrefix: string | undefined;
			if (options?.swURL) {
				try {
					this.#attachment = await attachSession(
						this.#syncSid,
						(frame) => this.#wire.router.handle(frame),
						{ swURL: options.swURL, swScope: options.swScope, workerURL }
					);
					syncPrefix = this.#attachment.prefix;
				} catch (err) {
					if (options.requireSyncFs !== false) throw err;
					globalThis.console.warn(
						"[node-worker] synchronous filesystem unavailable",
						err
					);
				}
			} else if (options?.requireSyncFs !== false) {
				throw new SyncFsUnavailable({
					sync: false,
					reason: "no-sw",
					detail:
						"pass `swURL` (the url of dist/sw.js) to enable synchronous fs",
				});
			}

			// Checked again: registering a service worker is a round trip, and `terminate()`
			// may well have been called during it.
			if (this.#terminated) throw new Error("terminated before start");

			this.worker = new Worker(workerURL, {
				name: "node-worker-" + workers++,
				type: "module",
			});
			// The bootstrap, and the only message that does not go over the port — it is what
			// delivers the port, and the port is now its only attachment. Posted without
			// waiting for the worker to announce itself, because a message to a worker whose
			// script is still evaluating is queued rather than dropped.
			this.#wire.attach(this.#wireChannel!.port1);
			let settled = await this.#wire.bootstrap(
				this.worker,
				KIND_CONTROL,
				{
					op: "ctl.init",
					puter: puterToken ?? "",
					net: options?.net,
					epoxyBase: options?.epoxyBase,
					cwd,
					keepalive,
					isTTY: console.isTTY,
					vfs: {
						sid: this.#syncSid,
						proto: WIRE_PROTO,
						syncPrefix,
						timeoutMs: options?.syncTimeoutMs ?? SYNC_TIMEOUT_MS,
						mounts: vfs.snapshot(),
					},
				},
				[this.#wireChannel!.port2]
			);
			let init = settled.decoded.header;
			if (!init.result.ok) throw fromWireError(init.result.error);
			let reply = init.result.value as ControlResult<"ctl.init">;
			capabilities(reply.capabilities);

			if (!reply.capabilities.sync && options?.requireSyncFs !== false) {
				throw new SyncFsUnavailable(reply.capabilities);
			}
		})();
		// Nothing necessarily awaits `capabilities` if `ready` rejected first.
		this.ready.catch(() =>
			capabilities({ sync: false, reason: "probe-failed" })
		);
	}

	/**
	 * One message, answered.
	 *
	 * A fresh `ArrayBuffer` rather than a view, because it is transferred back and a view into a
	 * larger buffer would send the whole thing.
	 */
	async #answerFrame(frame: ArrayBuffer): Promise<ArrayBuffer> {
		const out = (await this.#wire.router.handle(frame)).frame;
		return out.buffer.slice(
			out.byteOffset,
			out.byteOffset + out.byteLength
		) as ArrayBuffer;
	}

	/**
	 * Register what runs programs, after construction.
	 *
	 * The counterpart of `NodeWorkerOptions.process`, and useful for the same reason
	 * `mount()` is: a shell often needs the worker's own filesystem to exist first, and a
	 * provider that relays to a second worker cannot be built before this one is started.
	 */
	registerProcessProvider(provider: ProcessProvider): void {
		this.#process = provider;
	}

	/**
	 * Answer a program's `chan.call` for one name.
	 *
	 * The worker side is:
	 *
	 *   const chan = require("node-worker/channel");
	 *   const answer = await chan.call("phx.suite", results);
	 *   const answer = chan.callSync("phx.suite", results);   // works while parked
	 *
	 * `args` is whatever the two sides agreed on and `parts` carries any bytes. Returning a value
	 * answers; throwing answers with the error, and `err.code` survives the crossing.
	 *
	 * A handler may be called more than once for one logical request: a synchronous send retries
	 * the same `seq` after a transport failure. The reply record answers a repeat without running
	 * the handler again, so that is handled — but a handler that starts work of its own and
	 * returns before it finishes has stepped outside that guarantee.
	 *
	 * Returns a function that removes it.
	 */
	registerChannelHandler(name: string, handler: ChannelHandler): () => void {
		this.#channelHandlers.set(name, handler);
		return () => {
			if (this.#channelHandlers.get(name) === handler) {
				this.#channelHandlers.delete(name);
			}
		};
	}

	/**
	 * Hand a program running in this worker a `MessagePort`, under a name it can ask for.
	 *
	 * The page and a program otherwise have only the console streams between them, which is a
	 * byte pipe carrying whatever the program prints — fine for output, and a poor place to put
	 * a control protocol. The worker side is:
	 *
	 *   const port = await require("node-worker/channel").channel("shell");
	 *
	 * Opening a channel the program never asks for is harmless; asking for one the page never
	 * opens waits, on the reasoning that a program waiting for its host is not an error.
	 */
	async openChannel(name: string): Promise<MessagePort> {
		const channel = new MessageChannel();
		await this.openChannelWith(name, channel.port2);
		return channel.port1;
	}

	/**
	 * Hand this worker a port the *caller* made, rather than one minted here.
	 *
	 * `openChannel` keeps the other end, which is right when the page is one of the two parties.
	 * It is wrong when the two parties are two workers: opening a channel on each would leave the
	 * page relaying every message between them, which costs a hop each way and re-transfers every
	 * transferable through a third realm. With this, the page makes one `MessageChannel` and gives
	 * an end to each worker — after which they talk directly and the page is not in the data path
	 * at all. That is what `worker_threads` needs to be worth having.
	 */
	async openChannelWith(name: string, port: MessagePort): Promise<void> {
		await this.ready;
		await this.#wire.call(
			KIND_CHAN,
			{ op: "chan.open", name },
			{ transfer: [port] }
		);
	}

	/**
	 * Another worker configured the way this one was.
	 *
	 * Only a page can create a `NodeWorker`, so anything that wants a child process, a worker
	 * thread or a second realm has to come back here for it — and every one of them wants the
	 * same thing: the same worker script, the same service worker, the same filesystem, the same
	 * network. Rebuilding that by hand at each call site is how one of them ends up with a
	 * private `NodeVfs` and a child that cannot see the files its parent just wrote.
	 *
	 * The filesystem is inherited **by reference**, deliberately: siblings share a namespace, so
	 * a guest path means the same thing in both. `vfs` in `overrides` opts out.
	 *
	 * The caller owns what comes back and must `terminate()` it. A normal return leaves a worker
	 * running — see `settleRun`.
	 */
	async spawnSibling(
		overrides?: Partial<NodeWorkerOptions> & { cwd?: string }
	): Promise<NodeWorker> {
		const { workerURL, puterToken, cwd, options } = this.#spawnConfig;
		const { cwd: cwdOverride, ...optionOverrides } = overrides ?? {};
		return NodeWorker.create(workerURL, puterToken, cwdOverride ?? cwd, {
			...options,
			// The filesystem this worker actually ended up on, which is not the same as
			// `options.vfs`: a worker given none built its own, and a sibling that built a
			// second one would share nothing with it.
			vfs: this.vfs,
			...optionOverrides,
		});
	}

	/**
	 * Run a module in this worker and answer with its exit code, whichever way it ends.
	 *
	 * There are two shapes and only one of them looks like an ending. A program that returns
	 * normally resolves `require`/`import` with its code and **leaves this worker running** —
	 * nothing tears it down, so a caller that forgets to `terminate()` leaks one per run. A
	 * program that calls `process.exit` gets there the other way: the worker posts `ctl.exit`,
	 * the page terminates it, and the pending call *rejects* with `WorkerExitError` carrying the
	 * status. Treating that rejection as a failure is how `node -e 'process.exit(3)'` turns into
	 * a crash report instead of an exit status, which is a mistake worth making once.
	 */
	async settleRun(
		target: string,
		options?: RunOptions & { module?: "cjs" | "esm" }
	): Promise<number> {
		try {
			return options?.module === "esm"
				? await this.import(target, options)
				: await this.require(target, options);
		} catch (err) {
			if (err instanceof WorkerExitError) return err.code;
			throw err;
		}
	}

	/**
	 * What answers `worker_threads.Worker` for programs in this worker.
	 *
	 * Registered unconditionally, which is the point: only a page can create a `NodeWorker`, so
	 * without this every `new Worker(...)` in every worker throws no matter what the embedder
	 * does. The pieces it needs — `spawnSibling`, `openChannelWith`, `settleRun` — are the same
	 * ones a `ProcessProvider` uses; what differs is that the data path is a real `MessagePort`
	 * between the two workers rather than anything on the wire.
	 */
	#installThreadHost(): void {
		this.#builtinChannels.set("nw:thread.spawn", async (args, _parts, attachments) => {
			const a = args as {
				target: string;
				argv?: string[];
				env?: Record<string, string>;
				stdout?: boolean;
				stderr?: boolean;
				ipc?: boolean;
			};
			const id = this.#nextThreadId++;
			const child = await this.spawnSibling();

			/*
			 * One channel, an end to each worker. `openChannel` would have kept an end here and
			 * left this page relaying every message between them — a hop each way, and every
			 * transferable re-transferred through a third realm. With both ends placed, the two
			 * workers talk directly and nothing here sees their traffic.
			 */
			const pair = new MessageChannel();
			await child.openChannelWith("nw:parentPort", pair.port2);
			await this.openChannelWith(`nw:thread:${id}`, pair.port1);

			// Lifecycle and, if asked for, output. Opened lazily and once — a worker that never
			// starts a thread never gets one.
			if (!this.#threadControl) {
				const ctl = new MessageChannel();
				await this.openChannelWith("nw:threads", ctl.port2);
				this.#threadControl = ctl.port1;
				this.#threadControl.start();
			}
			const control = this.#threadControl;

			const readers: Promise<void>[] = [];
			const forward = (stream: ReadableStream<Uint8Array>, type: string) =>
				readers.push(
					(async () => {
						const reader = stream.getReader();
						try {
							for (;;) {
								const { value, done } = await reader.read();
								if (done) break;
								if (value?.length) control.postMessage({ id, type, bytes: value });
							}
						} catch {
							// The child went away; `exit` is what the parent is waiting for.
						} finally {
							reader.releaseLock();
						}
					})()
				);
			// Asked for, or nowhere to put it. A thread whose output nobody requested writes to
			// this page's console the way node pipes a worker's stdout to its parent's.
			if (a.stdout) forward(child.console.stdout, "stdout");
			if (a.stderr) forward(child.console.stderr, "stderr");

			this.#threads.set(id, child);

			void (async () => {
				let code = 0;
				try {
					code = await child.settleRun(a.target, {
						module: a.target.endsWith(".mjs") ? "esm" : "cjs",
						argv: a.argv ?? ["node", a.target],
						env: a.env,
						thread: {
							threadId: id,
							workerData: attachments[0],
							portChannel: "nw:parentPort",
							ipc: !!a.ipc,
						},
					});
				} catch (err) {
					control.postMessage({
						id,
						type: "error",
						message: (err as Error)?.message ?? String(err),
						stack: (err as Error)?.stack,
					});
					code = 1;
				}
				// Exit last, after the readers have ended: the parent stops listening for this
				// id once it sees it, so anything posted afterwards is discarded — including the
				// last chunk a reader was still inside `read()` for.
				try {
					child.terminate();
				} catch {
					// `process.exit` already terminated it.
				}
				await Promise.all(readers);
				this.#threads.delete(id);
				control.postMessage({ id, type: "exit", code });
			})();

			return { id, threadId: id };
		});

		this.#builtinChannels.set("nw:thread.terminate", async (args) => {
			const { id } = args as { id: number };
			this.#threads.get(id)?.terminate(new WorkerExitError(1));
			return null;
		});
	}

	async setCwd(cwd: string) {
		await this.ready;
		await this.#call({ op: "ctl.cwd", cwd });
	}

	// -------------------------------------------- the filesystem, from the host
	//
	// These all used to be page↔worker messages. They are ordinary calls into `this.vfs` now,
	// which means they are **synchronous underneath** — the `async` signatures are kept only so
	// existing callers do not have to change. Reach for `worker.vfs` directly for the synchronous
	// forms and for anything the old message set could not express (mounting your own provider,
	// listing mounts, watching for changes).
	//
	//   const proj = worker.vfs.mountMemory("/proj");
	//   proj.write([
	//     { path: "package.json", data: pkgJson },
	//     { path: "src/main.js",  data: src },
	//   ]);
	//   await worker.setCwd("/proj");
	//   await worker.import("/proj/src/main.js");

	async registerVirtualModule(path: string, code: string) {
		this.vfs.addVirtualFile(path, code);
	}
	async removeVirtualModule(path: string) {
		this.vfs.removeVirtualFile(path);
	}

	/**
	 * Create a memory-backed directory at `root`.
	 *
	 * `replace` swaps out an existing mount at the same root instead of throwing, which is what
	 * re-populating a project between runs wants.
	 */
	async mountMemory(
		root: string,
		options?: { readOnly?: boolean; replace?: boolean }
	) {
		this.vfs.mountMemory(root, options);
	}

	async unmountMemory(root: string) {
		this.vfs.unmountMemory(root);
	}

	/**
	 * Write entries into the memory mount at `root`, or into the overlay over the root mount when
	 * `root` is "/".
	 *
	 * Entry paths are relative to the mount root, and files create their own parent directories —
	 * an entry with no `data` is only needed for a deliberately empty one. Strings are encoded as
	 * UTF-8.
	 *
	 * `options.transfer` is accepted and **ignored**. It used to hand the underlying buffers to
	 * the worker instead of copying them, and it detached every `Uint8Array` and `ArrayBuffer`
	 * you passed. There is no boundary to cross any more, so the copy it was avoiding is a single
	 * local one and the hazard is simply gone.
	 */
	async writeMemory(
		root: string,
		files: Array<{
			path: string;
			data?: string | Uint8Array | ArrayBuffer;
			mtimeMs?: number;
		}>,
		options?: { transfer?: boolean }
	): Promise<{ written: number; bytes: number }> {
		void options;
		return this.vfs.memory(root).write(files);
	}

	/** Remove paths (relative to `root`) from a memory mount. */
	async removeMemory(root: string, paths: string[]) {
		this.vfs.memory(root).remove(paths);
	}

	/**
	 * Read one file out of a memory mount, or `undefined` if the path is absent or a directory.
	 * `path` is relative to `root`.
	 */
	async readMemory(
		root: string,
		path: string
	): Promise<Uint8Array | undefined> {
		return this.vfs.memory(root).read(path);
	}

	/**
	 * List a directory in a memory mount, or `undefined` if the path is absent or a file. `path`
	 * is relative to `root`; `""` and `"/"` both mean the root itself.
	 *
	 * `since` reports only what was modified after that time, which is what makes "what did this
	 * run touch?" cheap even over a tree with a `node_modules` in it.
	 */
	async listMemory(
		root: string,
		path: string,
		options?: { recursive?: boolean; since?: number }
	): Promise<MemListEntry[] | undefined> {
		return this.vfs.memory(root).list(path, options);
	}

	/**
	 * Run `path` as CommonJS and resolve with its exit code.
	 *
	 * Rejects with `WorkerExitError` if the program called `process.exit`, which also
	 * terminates the worker — read `err.code` for the status.
	 */
	async require(path: string, options?: RunOptions): Promise<number> {
		return this.execute("cjs", path, options);
	}

	/** As `require`, but run `path` as an ES module. */
	async import(path: string, options?: RunOptions): Promise<number> {
		return this.execute("esm", path, options);
	}

	private async execute(
		module: "cjs" | "esm",
		target: string,
		options?: RunOptions
	): Promise<number> {
		await this.ready;
		let reply = await this.#call<ControlResult<"ctl.execute">>(
			{
				op: "ctl.execute",
				module,
				target,
				argv: options?.argv,
				env: options?.env,
				thread: options?.thread && {
					threadId: options.thread.threadId,
					portChannel: options.thread.portChannel,
					ipc: options.thread.ipc,
				},
			},
			// Cloned beside the call rather than encoded into it: the header is JSON, and
			// `workerData` is whatever structured clone can carry.
			options?.thread ? { attach: [options.thread.workerData] } : undefined
		);
		return reply.exitCode;
	}

	/**
	 * Called when the worker exits of its own accord, before it is terminated.
	 *
	 * A listener returning a promise is awaited, which is the only window in which
	 * worker-side state can still be read. Returns an unsubscribe function.
	 */
	onExit(listener: (code: number) => void | Promise<void>): () => void {
		this.exitListeners.add(listener);
		return () => this.exitListeners.delete(listener);
	}

	/**
	 * Stop the worker. Idempotent.
	 *
	 * Every request still in flight is rejected with `reason`, because a terminated
	 * worker will never answer one. That matters most for the `execute` of a program
	 * that just called `process.exit`: without this it would stay pending forever, and
	 * the caller would be left waiting on a run that has already finished.
	 */
	terminate(reason?: Error) {
		// Set first, and independently of whether the worker exists yet: creation is deferred
		// behind service-worker registration, so `terminate()` during startup has nothing to
		// stop — and without this flag it would be a silent no-op followed by a worker
		// appearing anyway.
		if (this.#terminated) return;
		this.#terminated = true;

		// Tell the service worker to stop relaying for this session, so a request in flight
		// fails immediately rather than sitting out its deadline.
		this.#attachment?.detach();
		this.#attachment = undefined;

		// Peer servers and connections, and the fs-events channel. All of these are closed
		// by the worker *asking*, and the worker is about to stop being able to ask — see
		// `#hostResources`. A peer server in particular has to go now rather than whenever
		// the page unloads, because the signaller keeps its port registered for exactly as
		// long as its socket is open.
		let resources = [...this.#hostResources];
		this.#hostResources.clear();
		for (let resource of resources) {
			try {
				resource.close();
			} catch (err) {
				globalThis.console.warn(
					"[node-worker] failed to close a host resource",
					err
				);
			}
		}

		// The vfs listeners this worker added. Same reasoning as the resources above: a shared
		// filesystem outlives the worker, so what the constructor took here has to be given back.
		let unsubscribes = this.#unsubscribes;
		this.#unsubscribes = [];
		for (let unsubscribe of unsubscribes) {
			try {
				unsubscribe();
			} catch (err) {
				globalThis.console.warn(
					"[node-worker] failed to remove a vfs listener",
					err
				);
			}
		}

		// The host owns this session's open files, and they outlive the worker unless dropped —
		// which for a memory mount means leaking the contents of unlinked files, kept alive on
		// purpose for exactly as long as a handle refers to them. Dirty buffers are deliberately
		// not flushed: a worker that died did not ask for its pending writes to be published.
		//
		// A filesystem this worker created goes further and is disposed of outright,
		// since nothing else can be holding it — that also releases its change-feed
		// subscription, which would otherwise keep a socket open for the life of the
		// page. See `#ownsVfs`.
		if (this.#ownsVfs) this.vfs.dispose();
		else this.vfs.closeSession(this.#syncSid);

		// The filesystem channel outlives the worker otherwise: a port with a live `onmessage`
		// keeps this side reachable, and the handler closes over the vfs that was just disposed
		// of above.
		this.#wireChannel?.port1.close();
		this.#wireChannel = undefined;

		this.worker?.terminate();
		this.worker = undefined!;

		// End stdout/stderr, so a page reading them stops waiting. Nothing will write again, and
		// a `TransformStream` readable only ends when this side closes its writable — so without
		// this, "read the output until it ends" never returns. Not awaited: `terminate` is
		// synchronous, and the close only has queued writes ahead of it.
		void this.console.closeStdio().catch(() => {});

		let error = reason ?? new Error("Worker terminated");
		// One call, where there used to be a hand-rolled drain of an inflight map that the
		// worker's own half never had at all — so a terminated worker left its side parked
		// forever on promises nothing would settle.
		this.#wire.close(error);

		this.ready = Promise.reject(error);
		// Nothing necessarily awaits the replacement `ready`, and an unobserved
		// rejected promise is a console warning in every browser.
		this.ready.catch(() => {});
	}
}

// Last in the file, deliberately: `./process/worker-provider` imports `NodeWorker` and
// `WorkerExitError` from this module, and the cycle only resolves in that direction.
export {
	createWorkerProcessProvider,
	nodeCommandLine,
	type WorkerRun,
	type WorkerProcessProviderOptions,
} from "./process/worker-provider";
