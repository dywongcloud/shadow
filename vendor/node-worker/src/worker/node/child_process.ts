// node:child_process, over the process SPI.
//
// Every export here used to throw. What changed is not that a shell was added — nothing in this
// bundle runs a program — but that there is now somewhere to *ask*: the host registers a
// `ProcessProvider` (../../process/provider.ts) and this relays to it over the same transport
// the filesystem uses. Without a provider the throws come back, with a message naming the fix.
//
// The split between the two transports is the interesting part:
//
//   `spawnSync` — the blocking XMLHttpRequest. The caller is parked for the whole run while the
//                 host executes the program on *its* event loop and answers when it finishes.
//                 That is the one call node's api gives no other way to satisfy, and it is why
//                 the process host is on the other side of this boundary rather than in here.
//   everything else — postMessage. A spawned child streams by leaving one `poll` outstanding,
//                 answered when there is output or an exit; the host never pushes.

/*
 * The bases come from ./fs/lazy-base, not from `events`/`stream` directly, and that is not
 * stylistic: this module is reachable from the node builtin barrel, which sits inside a
 * module-init cycle, so `class X extends EventEmitter` read at definition time gets
 * `undefined.EventEmitter` and the whole worker fails to start with no error anywhere useful.
 * It cost an afternoon once already — see the header of ./fs/lazy-base.ts.
 */
import { EmitterBase, ReadableBase, WritableBase } from "./fs/lazy-base";
import { procAsync, procSync } from "./fs/transport";
// A plain Map that imports nothing, so it is safe to reach from inside the init cycle
// this module sits in. See the note above on ./fs/lazy-base.
import { fdTable } from "./fs/fd-table";
// Read at call time, never at definition time, which is what keeps this safe in that same cycle.
import { ctx, host } from "./fs/host";
// `fork` is a sibling realm with a port, which is what `worker_threads` already asks for.
import { forgetRealm, spawnRealm, terminateRealm } from "./worker_threads";
import * as keepalive from "../keepalive";
import type { SpawnRequest } from "../../process/provider";

/**
 * What one descriptor is wired to.
 *
 * A `number` is an open file descriptor of *this* process, which node accepts and which the
 * process SPI deliberately does not carry: the host runs in another worker, where our fd numbers
 * mean nothing. It is handled entirely on this side — see `sinkOf`.
 */
type Stdio = "pipe" | "inherit" | "ignore" | number;

interface Options {
	cwd?: string;
	env?: Record<string, string>;
	stdio?: Stdio | Stdio[];
	input?: string | Uint8Array;
	encoding?: string;
	shell?: boolean | string;
}

/** node lets `stdio` be one word for all three. */
function stdioTriple(stdio: Options["stdio"]): Stdio[] {
	if (Array.isArray(stdio)) return stdio.slice(0, 3) as Stdio[];
	if (typeof stdio === "string") return [stdio, stdio, stdio];
	return ["pipe", "pipe", "pipe"];
}

/** 0, 1 and 2 are the parent's own stdio; handing those over is what node calls "inherit". */
function isStdioFd(one: Stdio): boolean {
	return typeof one === "number" && one >= 0 && one <= 2;
}

/**
 * The triple as the *host* has to see it.
 *
 * A descriptor the caller gave as a number becomes "pipe" on the wire, because that is what makes
 * the host produce the bytes at all; this side then writes them into the file instead of into a
 * stream. Sending the number would be meaningless — fds are per-process, and the host is a
 * different worker.
 */
function wireStdio(triple: Stdio[]): ("pipe" | "inherit" | "ignore")[] {
	return triple.map((one) =>
		typeof one !== "number" ? one : isStdioFd(one) ? "inherit" : "pipe"
	);
}

/**
 * Where a descriptor's output goes when it is an fd rather than a pipe.
 *
 * The path is resolved **here**, at spawn time, and every write then goes to that path rather
 * than back through the descriptor. That is the point rather than a shortcut: on POSIX the child
 * inherits its own copy of the open file description, so the parent is free to close its fd the
 * moment `spawn` returns — and Claude Code's Bash tool does exactly that, one line after
 * spawning, because against a real kernel it can. Looking the fd up per write instead finds an
 * entry its owner has already deleted from the table and writes nowhere, which presents as a
 * command that runs, exits 0, and reports no output at all.
 *
 * `append` rather than a positioned write, because that is the O_APPEND the caller opened with
 * and it is the only shape that stays correct once we hold no descriptor of our own.
 */
function sinkOf(one: Stdio): ((bytes: Uint8Array) => void) | null {
	if (typeof one !== "number" || isStdioFd(one)) return null;
	const handle = fdTable.get(one) as { filePath?: string } | undefined;
	const path = handle?.filePath;
	if (path === undefined) {
		throw Object.assign(new Error(`spawn: bad file descriptor ${one} in stdio`), {
			code: "EBADF",
			syscall: "spawn",
		});
	}
	return (bytes) => {
		try {
			host.append(ctx("write", path), path, bytes);
		} catch {
			// A child's output descriptor going bad is the child's problem, not the parent's: by
			// the time a write can fail here there is no one left to report it to.
		}
	};
}

function request(file: string, args: string[], opts: Options): SpawnRequest {
	return {
		file,
		args,
		cwd: opts.cwd,
		env: opts.env,
		stdio: wireStdio(stdioTriple(opts.stdio)),
	};
}

/**
 * `shell: true`, and the string form of `exec`.
 *
 * Not interpreted here: quoting is the shell's business, and a half-implementation of it in
 * JavaScript is how a command containing a quote quietly starts meaning something else. The
 * whole string goes to `sh -c`, which is what node does.
 */
function throughShell(command: string, opts: Options): [string, string[]] {
	const shell = typeof opts.shell === "string" ? opts.shell : "/bin/sh";
	return [shell, ["-c", command]];
}

declare const Buffer: any;

function toBytes(v: string | Uint8Array | undefined): Uint8Array | undefined {
	if (v === undefined) return undefined;
	return typeof v === "string" ? Buffer.from(v, "utf8") : v;
}

/** node decodes when an `encoding` was asked for, and hands back a Buffer otherwise. */
function decode(bytes: Uint8Array, encoding?: string): string | Uint8Array {
	const buf = Buffer.from(bytes);
	return encoding && encoding !== "buffer"
		? buf.toString(encoding as BufferEncoding)
		: buf;
}

// ------------------------------------------------------------------ the child

export class ChildProcess extends EmitterBase {
	pid: number | undefined;
	exitCode: number | null = null;
	signalCode: string | null = null;
	killed = false;
	stdin: any = null;
	stdout: any = null;
	stderr: any = null;
	readonly stdio: any[] = [null, null, null];
	/** @internal Resolves once `close` has been emitted. */
	readonly done: Promise<void>;

	#settle!: () => void;
	#file: string;
	/** Per-descriptor fd writers, for the indices given as numbers rather than "pipe". */
	#sinks: (((bytes: Uint8Array) => void) | null)[] = [null, null, null];

	/** `fork` runs a sibling realm with an IPC port; `spawn` runs a program over the SPI. */
	#forked = false;
	#realmId: number | null = null;
	#ipc: any = null;
	#connected = false;
	/**
	 * A forked child keeps its parent alive, as node's does — its IPC channel is a ref'd handle.
	 *
	 * A spawned child needs no equivalent: its poll loop always has a request outstanding, and
	 * every in-flight request is already a keepalive. A fork has no outstanding request at all —
	 * only a port — so without this the parent's top-level code finishes, the run settles, and
	 * the child is torn down before one message crosses.
	 */
	#release: (() => void) | null = null;

	constructor(file: string, args: string[], opts: Options, forked = false) {
		super();
		this.#file = file;
		this.#forked = forked;
		this.done = new Promise<void>((resolve) => {
			this.#settle = resolve;
		});

		const triple = stdioTriple(opts.stdio);
		if (triple[0] === "pipe") {
			this.stdin = new WritableBase({
				write: (chunk: Uint8Array, _enc: unknown, cb: (e?: Error) => void) => {
					this.#write(toBytes(chunk)!).then(
						() => cb(),
						(e) => cb(e as Error)
					);
				},
				final: (cb: (e?: Error) => void) => {
					this.#endStdin().then(
						() => cb(),
						() => cb()
					);
				},
			});
		}
		// An fd on *stdin* would mean feeding the child from a file, which is a read this side
		// would have to perform and stream over. Nothing needs it yet, and saying so is better
		// than the silent nothing that a dropped descriptor used to produce.
		if (typeof triple[0] === "number") {
			throw Object.assign(
				new Error("child_process: stdin as a file descriptor is not supported here"),
				{ code: "ERR_INVALID_ARG_VALUE" }
			);
		}

		// Readables with no `_read` of their own: what arrives is decided by the child rather
		// than by anyone reading, so the poll loop pushes into them.
		if (triple[1] === "pipe") this.stdout = new ReadableBase({ read() {} });
		if (triple[2] === "pipe") this.stderr = new ReadableBase({ read() {} });
		// `child.stdout` stays null for an fd, as it does in node — the output is the file's, not
		// the caller's to read. Without these the bytes arrived and went nowhere: `stream?.push`
		// on a null stream is a silent drop, which is how a command could run, exit 0, and report
		// no output at all.
		this.#sinks = [null, sinkOf(triple[1]), sinkOf(triple[2])];
		this.stdio[0] = this.stdin;
		this.stdio[1] = this.stdout;
		this.stdio[2] = this.stderr;

		if (forked) {
			// A forked child always has stdout and stderr to read, whatever the stdio triple
			// said: it is a node program and its output is the caller's.
			this.stdout ??= new ReadableBase({ read() {} });
			this.stderr ??= new ReadableBase({ read() {} });
			this.stdio[1] = this.stdout;
			this.stdio[2] = this.stderr;
			void this.#startForked(file, args, opts);
		} else {
			void this.#start(file, args, opts);
		}
	}

	async #startForked(file: string, args: string[], opts: Options) {
		this.#release = keepalive.refOperation();
		try {
			const realm = await spawnRealm(
				{
					target: file,
					argv: ["node", file, ...args],
					env: opts.env,
					stdout: true,
					stderr: true,
					ipc: true,
				},
				(msg) => {
					if (msg.type === "stdout" || msg.type === "stderr") {
						const stream = msg.type === "stdout" ? this.stdout : this.stderr;
						stream?.push(msg.bytes ? Buffer.from(msg.bytes) : null);
						return;
					}
					if (msg.type === "error") {
						this.emit("error", new Error(msg.message ?? "fork failed"));
						return;
					}
					if (msg.type === "exit") this.#finish(msg.code ?? 0, null);
				}
			);
			this.#realmId = realm.id;
			this.pid = realm.threadId;
			this.#ipc = realm.port;
			this.#connected = !!realm.port;
			realm.port?.on("message", (data: unknown) => this.emit("message", data));
			this.emit("spawn");
		} catch (err) {
			queueMicrotask(() => {
				this.emit("error", err);
				this.#finish(null, null);
			});
		}
	}

	get spawnfile(): string {
		return this.#file;
	}

	async #start(file: string, args: string[], opts: Options) {
		try {
			const { value } = await procAsync({
				op: "proc.spawn",
				ctx: { syscall: "spawn" },
				request: request(file, args, opts),
			});
			this.pid = value.pid;
			this.emit("spawn");
			void this.#pump();
		} catch (err) {
			// node reports a spawn failure as an `error` event rather than a throw: by the time
			// it is known, the constructor has long returned.
			queueMicrotask(() => {
				this.emit("error", err);
				this.#finish(null, null);
			});
		}
	}

	/**
	 * One outstanding `poll` at a time, for as long as the child lives.
	 *
	 * The host answers it when there is something to say, so this waits rather than spins — and
	 * because only ever one is in flight, a child that prints nothing costs a single message for
	 * its whole lifetime.
	 */
	async #pump() {
		for (;;) {
			let events: {
				kind: string;
				status?: number | null;
				signal?: string | null;
			}[];
			let parts: Uint8Array[];
			try {
				const answer = await procAsync({
					op: "proc.poll",
					ctx: { syscall: "read" },
					pid: this.pid!,
				});
				events = answer.value.events;
				parts = answer.parts;
			} catch (err) {
				this.emit("error", err);
				this.#finish(null, null);
				return;
			}
			let at = 0;
			for (const event of events) {
				if (event.kind === "exit") {
					this.#finish(event.status ?? null, event.signal ?? null);
					return;
				}
				// Copied, not kept: `parts` are views into a frame about to be reused.
				const bytes = Buffer.from(parts[at++]);
				const index = event.kind === "stdout" ? 1 : 2;
				const stream = index === 1 ? this.stdout : this.stderr;
				if (stream) stream.push(bytes);
				else this.#sinks[index]?.(bytes);
			}
		}
	}

	#finish(status: number | null, signal: string | null) {
		if (this.#realmId !== null) {
			forgetRealm(this.#realmId);
			this.#realmId = null;
		}
		if (this.#connected) this.disconnect();
		this.#release?.();
		this.#release = null;
		this.exitCode = status;
		this.signalCode = signal;
		this.stdout?.push(null);
		this.stderr?.push(null);
		this.emit("exit", status, signal);
		// `close` follows `exit` once the streams are done, which here is immediately: the pump
		// has already pushed everything the child wrote.
		queueMicrotask(() => {
			this.emit("close", status, signal);
			this.#settle();
		});
	}

	async #write(bytes: Uint8Array) {
		if (this.pid === undefined) await this.#spawned();
		await procAsync(
			{ op: "proc.write", ctx: { syscall: "write" }, pid: this.pid! },
			[bytes]
		);
	}

	async #endStdin() {
		if (this.pid === undefined) await this.#spawned();
		await procAsync({
			op: "proc.endStdin",
			ctx: { syscall: "write" },
			pid: this.pid!,
		});
	}

	/** A write can arrive before the spawn round trip has answered. */
	#spawned(): Promise<void> {
		return new Promise((resolve, reject) => {
			this.once("spawn", () => resolve());
			this.once("error", reject);
		});
	}

	kill(signal: string | number = "SIGTERM"): boolean {
		if (this.#forked) {
			this.killed = true;
			if (this.#realmId !== null) void terminateRealm(this.#realmId);
			return true;
		}
		if (this.pid === undefined) return false;
		this.killed = true;
		void procAsync({
			op: "proc.kill",
			ctx: { syscall: "kill" },
			pid: this.pid,
			signal: typeof signal === "number" ? String(signal) : signal,
		}).catch(() => {
			// Killing something that has already exited is not worth raising.
		});
		return true;
	}

	ref() {}
	unref() {}

	/**
	 * `fork`'s IPC channel. A no-op for a spawned program, which has none — as in node, where
	 * `send` on a child started without an `ipc` stdio slot returns false.
	 */
	send(message: unknown, ...rest: unknown[]): boolean {
		const callback = rest.find((r) => typeof r === "function") as
			| ((err: Error | null) => void)
			| undefined;
		if (!this.#ipc || !this.#connected) {
			callback?.(new Error("channel closed"));
			return false;
		}
		this.#ipc.postMessage(message);
		callback?.(null);
		return true;
	}

	disconnect() {
		if (!this.#connected) return;
		this.#connected = false;
		try {
			this.#ipc?.close();
		} catch {
			// Already gone.
		}
		this.#ipc = null;
		this.emit("disconnect");
	}

	get channel() {
		return this.#ipc;
	}

	get connected() {
		return this.#connected;
	}
}

// ------------------------------------------------------------------- the api

export function spawn(
	file: string,
	args?: string[] | Options,
	opts?: Options
): ChildProcess {
	const [argv, options] = Array.isArray(args)
		? [args, opts ?? {}]
		: [[] as string[], (args as Options) ?? {}];
	const [cmd, cmdArgs] = options.shell
		? throughShell([file, ...argv].join(" "), options)
		: [file, argv];
	return new ChildProcess(cmd, cmdArgs, options);
}

export interface SpawnSyncReturns {
	pid: number;
	output: (string | Uint8Array | null)[];
	stdout: string | Uint8Array;
	stderr: string | Uint8Array;
	status: number | null;
	signal: string | null;
	error?: Error;
}

export function spawnSync(
	file: string,
	args?: string[] | Options,
	opts?: Options
): SpawnSyncReturns {
	const [argv, options] = Array.isArray(args)
		? [args, opts ?? {}]
		: [[] as string[], (args as Options) ?? {}];
	const [cmd, cmdArgs] = options.shell
		? throughShell([file, ...argv].join(" "), options)
		: [file, argv];

	const input = toBytes(options.input);
	let answer;
	try {
		answer = procSync(
			{
				op: "proc.spawnSync",
				ctx: { syscall: "spawn" },
				request: request(cmd, cmdArgs, options),
			},
			input ? [input] : undefined
		);
	} catch (err) {
		// A transport or provider failure is `error` on the result rather than a throw: that is
		// how node reports "could not run it at all", and callers branch on it.
		const empty = decode(new Uint8Array(0), options.encoding);
		return {
			pid: 0,
			output: [null, empty, empty],
			stdout: empty,
			stderr: empty,
			status: null,
			signal: null,
			error: err as Error,
		};
	}

	/*
	 * The same fd handling as a spawned child, collected rather than streamed.
	 *
	 * What comes back is reported as empty rather than as node's `null`: the bytes went to the
	 * file, so there is nothing for the caller to read either way, and an empty Buffer keeps
	 * `result.stdout.toString()` working where a null would throw.
	 */
	const triple = stdioTriple(options.stdio);
	const outBytes = answer.parts[0] ?? new Uint8Array(0);
	const errBytes = answer.parts[1] ?? new Uint8Array(0);
	const outSink = sinkOf(triple[1]);
	const errSink = sinkOf(triple[2]);
	if (outSink) outSink(outBytes);
	if (errSink) errSink(errBytes);

	const stdout = decode(outSink ? new Uint8Array(0) : outBytes, options.encoding);
	const stderr = decode(errSink ? new Uint8Array(0) : errBytes, options.encoding);
	const failed = answer.value.error;
	return {
		pid: 0,
		output: [null, stdout, stderr],
		stdout,
		stderr,
		status: answer.value.status,
		signal: answer.value.signal,
		...(failed
			? {
					error: Object.assign(new Error(failed.message), {
						code: failed.code,
					}),
				}
			: {}),
	};
}

type ExecCallback = (
	error: Error | null,
	stdout: string | Uint8Array,
	stderr: string | Uint8Array
) => void;

function collect(
	child: ChildProcess,
	opts: Options,
	cb?: ExecCallback
): ChildProcess {
	if (!cb) return child;
	/*
	 * `exec` and `execFile` hand the callback **strings**: their documented default is
	 * `encoding: "utf8"`, and only an explicit "buffer" (or null) asks for bytes. `spawnSync`
	 * defaults the other way round, which is why this belongs here and not in `decode`.
	 *
	 * Getting it wrong is not a type nicety. A caller that does the ordinary
	 * `execFile(f, a, (e, stdout) => stdout.trim())` gets "stdout.trim is not a function", and it
	 * throws from inside the `close` handler — a stack that names this module and not the line
	 * that called it.
	 */
	const encoding = opts.encoding === undefined ? "utf8" : opts.encoding;
	const out: Uint8Array[] = [];
	const err: Uint8Array[] = [];
	child.stdout?.on("data", (c: Buffer) => void out.push(c));
	child.stderr?.on("data", (c: Buffer) => void err.push(c));
	child.on("close", (status: number | null, signal: string | null) => {
		const stdout = decode(Buffer.concat(out), encoding);
		const stderr = decode(Buffer.concat(err), encoding);
		const failed =
			status === 0
				? null
				: Object.assign(
						new Error(
							`Command failed${signal ? ` with ${signal}` : ` with exit code ${status}`}`
						),
						{ code: status ?? undefined, signal }
					);
		cb(failed, stdout, stderr);
	});
	child.on("error", (e: Error) =>
		cb(e, decode(new Uint8Array(0), encoding), decode(new Uint8Array(0), encoding))
	);
	return child;
}

export function exec(
	command: string,
	optsOrCb?: Options | ExecCallback,
	maybeCb?: ExecCallback
): ChildProcess {
	const opts = typeof optsOrCb === "function" ? {} : (optsOrCb ?? {});
	const cb = typeof optsOrCb === "function" ? optsOrCb : maybeCb;
	const [shell, args] = throughShell(command, opts);
	return collect(new ChildProcess(shell, args, opts), opts, cb);
}

export function execFile(
	file: string,
	argsOrOpts?: string[] | Options | ExecCallback,
	optsOrCb?: Options | ExecCallback,
	maybeCb?: ExecCallback
): ChildProcess {
	const args = Array.isArray(argsOrOpts) ? argsOrOpts : [];
	const rest = Array.isArray(argsOrOpts) ? optsOrCb : argsOrOpts;
	const opts = typeof rest === "function" ? {} : ((rest as Options) ?? {});
	const cb =
		typeof rest === "function"
			? rest
			: typeof optsOrCb === "function"
				? optsOrCb
				: maybeCb;
	return collect(new ChildProcess(file, args, opts), opts, cb);
}

/** Both of these are `spawnSync` with node's throw-on-failure convention on top. */
function syncOrThrow(result: SpawnSyncReturns): string | Uint8Array {
	if (result.error) throw result.error;
	if (result.status !== 0) {
		throw Object.assign(
			new Error(`Command failed with exit code ${result.status}`),
			{
				status: result.status,
				stdout: result.stdout,
				stderr: result.stderr,
			}
		);
	}
	return result.stdout;
}

export function execSync(
	command: string,
	opts: Options = {}
): string | Uint8Array {
	const [shell, args] = throughShell(command, opts);
	return syncOrThrow(spawnSync(shell, args, opts));
}

export function execFileSync(
	file: string,
	args: string[] = [],
	opts: Options = {}
): string | Uint8Array {
	return syncOrThrow(spawnSync(file, args, opts));
}

/**
 * `fork`, which is not a POSIX fork.
 *
 * Node's is a special case of `spawn`: a new process starting at the top of a *named module*,
 * copying nothing, with an IPC channel wired up. So it needs exactly what `worker_threads.Worker`
 * needs — a sibling realm and a `MessagePort` — and it is built on the same call, which is why
 * this needs no process provider and works wherever the runtime does.
 *
 * The IPC channel is a real port, so `send` carries anything structured clone carries. Node
 * serialises to JSON by default here (`serialization: "advanced"` opts into its own format);
 * a port is strictly more capable than either, so nothing is lost by not choosing.
 */
export function fork(
	modulePath: string,
	args?: string[] | Options,
	opts?: Options
): ChildProcess {
	const [argv, options] = Array.isArray(args)
		? [args, opts ?? {}]
		: [[] as string[], (args as Options) ?? {}];
	return new ChildProcess(modulePath, argv, options, true);
}

const childProcess = {
	ChildProcess,
	exec,
	execFile,
	execFileSync,
	execSync,
	fork,
	spawn,
	spawnSync,
};

export default childProcess as unknown as typeof import("node:child_process");
