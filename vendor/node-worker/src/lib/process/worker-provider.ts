// A `ProcessProvider` whose children are `NodeWorker`s.
//
// `NodeWorkerOptions.process` has always described this shape without shipping it — "a second
// `NodeWorker` this page owns is the shape that keeps a command's stdout separate from the
// agent's, which the same worker cannot". This is that, and it is the reason
// `child_process.fork` was ever called structurally impossible: only a page can create a
// `NodeWorker`, so the thing that runs a child has to live here rather than in the worker
// asking for one.
//
// ## What is in here and what is not
//
// Creating the worker is the embedder's, through `start`. Which filesystem it shares, what
// answers *its* `child_process`, whether it keeps a keepalive — those are policy, they differ
// per embedder, and passing them all through would mean re-exporting most of
// `NodeWorkerOptions` for no gain. A pool would be a change to `start` alone.
//
// Everything after the worker exists is in here, because it is mechanism and every piece of it
// is a bug someone would otherwise write again:
//
//   - readers attached *before* the run starts, or the first writes are lost;
//   - `poll` that waits rather than spins, so a silent child costs one message for its life;
//   - stdin closed when the caller says so, and immediately when the caller never will,
//     because a child reading a stdin nobody closes never finishes;
//   - both exit shapes normalised (see `settle`);
//   - `terminate()` on every path, including a failed spawn and `kill`.

import { WorkerExitError, type NodeWorker } from "../index";
import type {
	ExitStatus,
	ProcCtx,
	ProcEvent,
	ProcessProvider,
	SpawnRequest,
	SpawnSyncResult,
} from "../../process/provider";

/** One child the embedder has started for us. */
export interface WorkerRun {
	/** Already created, and already given whatever provider its own `child_process` needs. */
	worker: NodeWorker;
	/** The module to run, as a path in that worker's filesystem. */
	target: string;
	/**
	 * Source to register at `target` before running, for a child that has no file — `node -e`
	 * and a program read from stdin. Removed again when the run ends.
	 */
	source?: string;
	/** How to run `target`. Defaults to `"cjs"`, which is node's default for an ambiguous `.js`. */
	module?: "cjs" | "esm";
	argv?: string[];
	env?: Record<string, string>;
	/** Called once the child is gone and its worker terminated. */
	dispose?(): void | Promise<void>;
}

export interface WorkerProcessProviderOptions {
	/**
	 * Start a child for `request`, or return `null` if this provider does not run that program —
	 * which is reported to the caller as ENOENT, the way a missing executable is.
	 */
	start(request: SpawnRequest): Promise<WorkerRun | null>;
	/**
	 * Where a child's `"inherit"` output goes.
	 *
	 * POSIX `inherit` means "the child writes to the same descriptor its parent does", and the
	 * only terminal a page owns belongs to the *calling* worker — which the process SPI gives no
	 * way to reach from here. So an embedder that wants inherited output has to say where it
	 * goes. Without this the bytes are dropped, and a warning says so once rather than leaving
	 * a command that runs, exits 0 and prints nothing.
	 */
	inherit?(fd: 1 | 2): ((bytes: Uint8Array) => void) | null;
	/** For diagnostics. */
	name?: string;
}

/**
 * A running child, from this side.
 *
 * Output accumulates until it is polled for rather than being pushed, because the transport
 * underneath is strictly request-and-reply. The caller keeps exactly one `poll` outstanding and
 * answering it is how anything gets across, so a child that prints nothing costs one message for
 * its whole life.
 */
class Child {
	readonly pid: number;
	pending: ProcEvent[] = [];
	exited = false;
	wake: (() => void) | null = null;

	constructor(pid: number) {
		this.pid = pid;
	}

	push(event: ProcEvent) {
		this.pending.push(event);
		this.wake?.();
		this.wake = null;
	}

	/** Whatever has happened since the last ask, waiting until something has. */
	async take(): Promise<ProcEvent[]> {
		while (this.pending.length === 0 && !this.exited) {
			await new Promise<void>((resolve) => {
				this.wake = resolve;
			});
		}
		return this.pending.splice(0);
	}
}

function enoent(file: string): Error {
	return Object.assign(new Error(`spawn ${file} ENOENT`), {
		code: "ENOENT",
		syscall: "spawn",
	});
}

let warnedAboutInherit = false;

export function createWorkerProcessProvider(
	options: WorkerProcessProviderOptions
): ProcessProvider {
	const children = new Map<number, Live>();
	let nextPid = 1;

	interface Live {
		child: Child;
		run: WorkerRun;
		stdin: WritableStreamDefaultWriter<Uint8Array> | null;
		/** Resolves once the run has settled and both readers have ended. */
		done: Promise<ExitStatus>;
		killed: string | null;
	}

	/** Both exit shapes, normalised by `NodeWorker.settleRun`. */
	async function settle(run: WorkerRun): Promise<ExitStatus> {
		const status = await run.worker.settleRun(run.target, {
			module: run.module,
			argv: run.argv,
			env: run.env,
		});
		return { status, signal: null };
	}

	/** Forward one of the child's output streams into its event queue until it ends. */
	async function pump(
		stream: ReadableStream<Uint8Array>,
		onBytes: (bytes: Uint8Array) => void
	): Promise<void> {
		const reader = stream.getReader();
		try {
			for (;;) {
				const { value, done } = await reader.read();
				if (done) break;
				if (value?.length) onBytes(value);
			}
		} catch {
			// The worker went away mid-read. The exit event is what the caller is waiting for.
		} finally {
			reader.releaseLock();
		}
	}

	function sinkFor(
		fd: 1 | 2,
		disposition: string | undefined,
		child: Child,
		kind: "stdout" | "stderr"
	): ((bytes: Uint8Array) => void) | null {
		if (disposition === "ignore") return null;
		if (disposition === "inherit") {
			const sink = options.inherit?.(fd);
			if (sink) return sink;
			if (!warnedAboutInherit) {
				warnedAboutInherit = true;
				globalThis.console.warn(
					"[node-worker] a child asked for stdio \"inherit\" and this provider has " +
						"nowhere to put it — pass `inherit` to createWorkerProcessProvider(). " +
						"The output is being dropped."
				);
			}
			return null;
		}
		return (bytes) => child.push({ kind, bytes });
	}

	async function begin(
		request: SpawnRequest
	): Promise<{ pid: number; live: Live }> {
		const run = await options.start(request);
		if (!run) throw enoent(request.file);

		const pid = nextPid++;
		const child = new Child(pid);
		const stdio = request.stdio ?? ["pipe", "pipe", "pipe"];

		if (run.source !== undefined) {
			await run.worker.registerVirtualModule(run.target, run.source);
		}

		/*
		 * Readers first, and the run started only after. A `TransformStream` buffers, so this is
		 * belt and braces rather than a race in practice — but the ordering is free and the
		 * failure it prevents (a program's first line missing, sometimes) is the kind that gets
		 * blamed on everything else first.
		 */
		const out = sinkFor(1, stdio[1], child, "stdout");
		const err = sinkFor(2, stdio[2], child, "stderr");
		const readers = [
			pump(run.worker.console.stdout, (b) => out?.(b)),
			pump(run.worker.console.stderr, (b) => err?.(b)),
		];

		/*
		 * A child whose stdin the caller will never write has to see EOF now. Left open, a
		 * program that reads to the end of its input waits for a writer that does not exist,
		 * and the run never settles — which presents as a hang rather than as a failure.
		 */
		let stdin: WritableStreamDefaultWriter<Uint8Array> | null = null;
		if (stdio[0] === "pipe") {
			stdin = run.worker.console.stdin.getWriter();
		} else {
			await run.worker.console.stdin.close().catch(() => {});
		}

		const live: Live = { child, run, stdin, killed: null, done: null! };

		live.done = (async () => {
			let status: ExitStatus;
			try {
				status = await settle(run);
			} catch (err) {
				// The run failed for a reason that is not an exit status at all — the worker
				// died, or the module would not load. Report it as output and a failure, which
				// is what a program crashing looks like from the outside.
				child.push({
					kind: "stderr",
					bytes: new TextEncoder().encode(
						`${(err as Error)?.stack ?? String(err)}\n`
					),
				});
				status = { status: 1, signal: null };
			}

			/*
			 * Both readers to the end *before* the exit event. The caller stops polling the
			 * moment it sees `exit`, so anything pushed after it is discarded — and a reader
			 * still inside `read()` when the run settles has not finished. Queueing the exit
			 * behind a microtask does not wait for it either: the last chunk loses the race and
			 * the command reads as having produced nothing at all.
			 */
			try {
				// With a deadline. The run has already settled — the child is gone — so this is
				// waiting for its last bytes to be accepted, and a consumer that has stopped
				// reading must not be able to stop the exit event from ever being pushed.
				await run.worker.console.flushStdio?.(500);
			} catch {
				// Best effort; the terminate below is what actually ends the readers.
			}
			try {
				run.worker.terminate();
			} catch {
				// Already gone — `process.exit` terminates it for us.
			}
			await Promise.all(readers);

			if (live.killed) status = { status: null, signal: live.killed };
			child.exited = true;
			child.push({ kind: "exit", ...status });

			try {
				if (run.source !== undefined) {
					await run.worker.removeVirtualModule(run.target);
				}
			} catch {
				// The vfs may be gone with the worker; nothing to clean up then.
			}
			await run.dispose?.();
			return status;
		})();

		return { pid, live };
	}

	return {
		name: options.name ?? "node-worker",

		async spawn(_ctx: ProcCtx, request: SpawnRequest) {
			const { pid, live } = await begin(request);
			children.set(pid, live);
			void live.done.finally(() => {
				// Kept until the exit event has been collected, which `poll` does.
			});
			return { pid };
		},

		async poll(_ctx: ProcCtx, pid: number) {
			const live = children.get(pid);
			if (!live) {
				throw Object.assign(new Error(`no such process ${pid}`), {
					code: "ESRCH",
					syscall: "read",
				});
			}
			const events = await live.child.take();
			if (events.some((e) => e.kind === "exit")) children.delete(pid);
			return events;
		},

		async write(_ctx: ProcCtx, pid: number, bytes: Uint8Array) {
			const live = children.get(pid);
			if (!live?.stdin) return;
			try {
				await live.stdin.write(bytes);
			} catch {
				// Writing to a child that has gone is the child's problem, and by the time it
				// can fail here there is nobody left to report it to.
			}
		},

		async endStdin(_ctx: ProcCtx, pid: number) {
			const live = children.get(pid);
			if (!live?.stdin) return;
			try {
				await live.stdin.close();
			} catch {
				// Already closed, or the worker is gone.
			}
			live.stdin = null;
		},

		async kill(_ctx: ProcCtx, pid: number, signal: string) {
			const live = children.get(pid);
			if (!live) return;
			live.killed = signal;
			// The run's own teardown does the rest: terminate rejects the pending call, `settle`
			// turns that into a status, and `killed` overrides it with the signal.
			try {
				live.run.worker.terminate(new WorkerExitError(-1));
			} catch {
				// Already terminated.
			}
		},

		async spawnSync(
			ctx: ProcCtx,
			request: SpawnRequest & { input?: Uint8Array }
		): Promise<SpawnSyncResult> {
			// The same path, collected. This is the call the architecture is for: the *caller's*
			// worker is parked in a blocking XHR while this side runs the program on its own
			// event loop, so a synchronous spawn can be a whole pipeline rather than only a
			// program that never waits.
			let live: Live;
			let pid: number;
			try {
				const begun = await begin(request);
				pid = begun.pid;
				live = begun.live;
			} catch (err) {
				return {
					status: null,
					signal: null,
					stdout: new Uint8Array(0),
					stderr: new Uint8Array(0),
					error: {
						message: (err as Error)?.message ?? String(err),
						code: (err as { code?: string })?.code,
					},
				};
			}
			children.set(pid, live);

			if (request.input?.length && live.stdin) {
				await live.stdin.write(request.input);
			}
			if (live.stdin) {
				await live.stdin.close().catch(() => {});
				live.stdin = null;
			}

			const status = await live.done;
			children.delete(pid);

			const out: Uint8Array[] = [];
			const err: Uint8Array[] = [];
			for (const event of live.child.pending) {
				if (event.kind === "stdout") out.push(event.bytes);
				else if (event.kind === "stderr") err.push(event.bytes);
			}
			void ctx;
			return {
				status: status.status,
				signal: status.signal,
				stdout: concat(out),
				stderr: concat(err),
			};
		},
	};
}

function concat(chunks: Uint8Array[]): Uint8Array {
	if (chunks.length === 0) return new Uint8Array(0);
	if (chunks.length === 1) return chunks[0];
	let total = 0;
	for (const c of chunks) total += c.length;
	const out = new Uint8Array(total);
	let at = 0;
	for (const c of chunks) {
		out.set(c, at);
		at += c.length;
	}
	return out;
}

/**
 * node's command line, as much of it as a child process needs.
 *
 * Something at the page has to read this: with a shell wired up, a program calling
 * `spawn("node", ["-e", src])` arrives here as argv and nothing else. Returns `null` for a
 * command line this cannot run, which the caller reports as ENOENT.
 */
export function nodeCommandLine(args: string[]):
	| { kind: "eval"; source: string; print: boolean; rest: string[] }
	| { kind: "script"; path: string; module: "cjs" | "esm"; rest: string[] }
	| null {
	for (let i = 0; i < args.length; i++) {
		const a = args[i];
		if (a === "-e" || a === "--eval" || a === "-p" || a === "--print") {
			const source = args[i + 1];
			if (source === undefined) return null;
			return {
				kind: "eval",
				source,
				print: a === "-p" || a === "--print",
				rest: args.slice(i + 2),
			};
		}
		if (a === "--") {
			const path = args[i + 1];
			if (path === undefined) return null;
			return { kind: "script", path, module: moduleOf(path), rest: args.slice(i + 2) };
		}
		// Anything else beginning with "-" is a flag this does not implement. Skipping it
		// rather than failing keeps `node --enable-source-maps script.js` working.
		if (a.startsWith("-") && a !== "-") continue;
		return { kind: "script", path: a, module: moduleOf(a), rest: args.slice(i + 1) };
	}
	return null;
}

/** node's own rule: extension first. `.js` is CommonJS absent a package `type`. */
function moduleOf(path: string): "cjs" | "esm" {
	if (path.endsWith(".mjs")) return "esm";
	return "cjs";
}
