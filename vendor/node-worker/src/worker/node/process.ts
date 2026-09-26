import { CWD, setPuterCWD } from "../state";
import nodeEvents from "./events";
import { announceExit, requestExit } from "../exit";
import { heapReadout } from "./memory";
import { holder as asyncContextHolder } from "../node-core/internal-binding/async_context_frame";

const queue: {
	callback: (...args: any[]) => void;
	args: any[];
	frame: any;
}[] = [];
let scheduled = false;

function flushNextTickQueue() {
	scheduled = false;
	while (queue.length > 0) {
		const { callback, args, frame } = queue.shift()!;
		// Run each tick under the async context frame that was current when it was
		// scheduled, matching how V8 would preserve continuation data for nextTick.
		const prev = asyncContextHolder.frame;
		asyncContextHolder.frame = frame;
		try {
			callback(...args);
		} catch (e) {
			queueMicrotask(() => {
				throw e;
			});
		} finally {
			asyncContextHolder.frame = prev;
		}
	}
}

// Synchronously drain the nextTick queue. Upstream internal/timers.js calls
// this (as `runNextTicks`) between timer/immediate callbacks so ticks queued by
// one callback run before the next one, matching node's ordering.
export function runNextTicks() {
	flushNextTickQueue();
}

function nextTick(callback: (...args: any[]) => void, ...args: any[]) {
	if (typeof callback !== "function") {
		throw new TypeError("callback must be a function");
	}

	queue.push({ callback, args, frame: asyncContextHolder.frame });
	if (!scheduled) {
		scheduled = true;
		queueMicrotask(flushNextTickQueue);
	}
}

const nodeProcess: any = {
	env: { TERM: "xterm-256color" },
	platform: "browser",
	arch: "wasm",
	pid: 1,
	ppid: 0,
	argv: ["node"],
	argv0: "node",
	execPath: "node",
	execArgv: [],
	// Set by a program to pick an exit code without exiting. The `execute` reply
	// carries whatever it holds when the run finishes; see `takeExitCode`.
	exitCode: undefined as number | undefined,
	// Must track node_core: see NODE_{MAJOR,MINOR,PATCH}_VERSION in
	// node_core/src/node_version.h. The leading "v" is part of node's own
	// `process.version` and semver parsers reject the string without it, so a
	// program that gates a feature on the runtime version silently took the
	// wrong branch while these two disagreed.
	version: "v25.9.0",
	versions: {
		node: "25.9.0",
		v8: "13.6.0",
		uv: "1.51.0",
		modules: "137",
	},
	features: {
		require_module: false,
		cached_builtins: true,
		debug: false,
		inspector: false,
		ipv6: true, // false
		tls: false, // TODO
		tls_alpn: false, // TODO
		tls_ocsp: false, // TODO
		tls_sni: false, // TODO
		typescript: false,
		uv: true, // false
		// We back crypto with OpenSSL (not BoringSSL); internal/crypto/util.js
		// branches on this when deciding which WebCrypto algorithms are gated.
		openssl_is_boringssl: false
	},
	// process is `inject`-ed into upstream node-core, so importing ../console
	// here would form a cycle through node/stream's wrapper. console.ts assigns
	// stdin/stdout/stderr in `initConsole`.
	stdin: undefined as any,
	stdout: undefined as any,
	stderr: undefined as any,
	cwd() {
		return CWD;
	},
	chdir(dir: string) {
		// TODO ?
		setPuterCWD(dir);
	},
	nextTick,
	emitWarning(message: any, type: string = "Warning") {
		if (typeof console !== "undefined" && typeof console.warn === "function") {
			console.warn(`${type}: ${message}`);
		}
	},
	kill() {
		return false;
	},
	exit(code?: number) {
		const status = code ?? nodeProcess.exitCode ?? 0;
		// node emits both before the process goes away, and a surprising amount of
		// code does its only cleanup here: restoring the terminal, flushing state to
		// disk, releasing a lock. `requestExit` throws the ProcessExit sentinel and
		// the host then tears the worker down, so this is the last point at which a
		// listener can run at all.
		//
		// A listener that throws must not keep the process alive — that would turn a
		// clean exit into a hang — so each is isolated.
		//
		// A listener that *blocks* is the case the try/catch cannot touch, and nothing on this
		// thread can: synchronous `fs` is a blocking request, so one bad cleanup parks the
		// worker before `requestExit` below is ever reached and the exit is never reported at
		// all. So the page is told first, while this thread still turns.
		announceExit(status);
		for (const event of ["beforeExit", "exit"]) {
			try {
				nodeProcess.emit(event, status);
			} catch {
				/* a failing listener does not get to block the exit */
			}
		}
		requestExit(status);
	},
	/** Some teardown paths reach past a wrapped `exit` to the raw one. */
	reallyExit(code?: number) {
		requestExit(code ?? nodeProcess.exitCode ?? 0);
	},
	hrtime: Object.assign(
		(time?: [number, number]): [number, number] => {
			const now = performance.now() * 1e6;
			const seconds = Math.floor(now / 1e9);
			const nanos = Math.floor(now % 1e9);
			if (time) {
				return [seconds - time[0], nanos - time[1]];
			}
			return [seconds, nanos];
		},
		{
			bigint(): bigint {
				return BigInt(Math.floor(performance.now() * 1e6));
			},
		}
	),
	uptime() {
		return performance.now() / 1000;
	},
	binding() {
		throw new Error("process.binding is not supported");
	},
	setSourceMapsEnabled() {},

	// ------------------------------------------------------------------ memory
	//
	// Call sites for these are routinely *unguarded* — `memoryUsage()` in particular reads like
	// something that cannot fail — so their absence surfaces as a TypeError in the middle of
	// ordinary work rather than as a missing-feature branch. See ../memory.ts on the numbers.
	memoryUsage: Object.assign(
		() => {
			const { used, total } = heapReadout();
			return {
				rss: total,
				heapTotal: total,
				heapUsed: used,
				external: 0,
				arrayBuffers: 0,
			};
		},
		{ rss: () => heapReadout().total }
	),
	constrainedMemory() {
		return 0;
	},
	availableMemory() {
		const { used, limit } = heapReadout();
		return Math.max(0, limit - used);
	},
	cpuUsage() {
		return { user: 0, system: 0 };
	},
	resourceUsage() {
		return {
			userCPUTime: 0,
			systemCPUTime: 0,
			maxRSS: Math.round(heapReadout().total / 1024),
			sharedMemorySize: 0,
			unsharedDataSize: 0,
			unsharedStackSize: 0,
			minorPageFault: 0,
			majorPageFault: 0,
			swappedOut: 0,
			fsRead: 0,
			fsWrite: 0,
			ipcSent: 0,
			ipcReceived: 0,
			signalsCount: 0,
			voluntaryContextSwitches: 0,
			involuntaryContextSwitches: 0,
		};
	},
	umask() {
		return 0o022;
	},

	// ------------------------------------------------------------ misc surface
	/*
	 * The uncaught-exception capture hooks, all three inert.
	 *
	 * Upstream these divert an exception that reached the top of the stack to a callback
	 * instead of emitting `uncaughtException`. There is no such top here — a throw leaves
	 * through the Worker's own error path — so there is nothing to divert, and reporting
	 * "no callback installed" keeps callers on the branch that still works.
	 *
	 * `add...` is node 25's, and `lib/repl.js:197` calls it while constructing a REPLServer:
	 * it is what replaced the old domain-based error routing. Leaving it out is not an option
	 * that degrades — the constructor throws and there is no prompt at all. Inert costs the
	 * REPL only its handling of an exception thrown *asynchronously*, after the eval that
	 * started it returned; a `throw` typed at the prompt is caught by `defaultEval` itself.
	 */
	addUncaughtExceptionCaptureCallback() {},
	setUncaughtExceptionCaptureCallback() {},
	hasUncaughtExceptionCaptureCallback() {
		return false;
	},
	// `getBuiltinModule` is attached by ./index.ts, next to `module.builtinModules`: it needs the
	// module registry, and importing that here would cycle straight back through this file.

	// ---------------------------------------------------- deliberately absent
	//
	// getuid / geteuid / getgid / getegid — this runtime has no uids, and the VFS reports 0 from
	//   `stat()`. Every reasonable caller guards on `typeof process.getuid === "function"`, so
	//   absence makes ownership checks *skip*, which is right. Defining them would make those
	//   checks compare a fabricated uid against the VFS's 0 and fail on files the caller does own.
	//
	// send — its presence is how a program detects that it was forked over an IPC channel. It was
	//   not, and there is no channel to answer on.
};

Object.setPrototypeOf(nodeProcess, nodeEvents.EventEmitter.prototype);
(nodeEvents.EventEmitter as any).call(nodeProcess);

(globalThis as any).process = nodeProcess;

// ------------------------------------------------------------ per-run state
//
// argv, env and the exit code belong to a run, not to the worker, and the host sets
// them on the `execute` message. These are exported rather than left to the message
// handler because worker/index.ts must not reference `process` even once — rollup's
// inject plugin would hoist an import for it above the bootstrap. See the header
// comment there.

/** Install the run's `process.argv`. `argv[0]` also becomes `argv0`. */
export function setArgv(argv: string[]): void {
	nodeProcess.argv = [...argv];
	nodeProcess.argv0 = argv[0] ?? "node";
}

/**
 * Replace `process.env`'s contents.
 *
 * Mutates in place rather than assigning a new object: `process` is injected into
 * upstream node-core, and modules there capture `process.env` itself, so swapping
 * the reference would leave them reading the old one forever.
 */
export function setEnv(env: Record<string, string>): void {
	let target = nodeProcess.env as Record<string, string>;
	for (let key of Object.keys(target)) delete target[key];
	Object.assign(target, env);
}

/** The run's exit code, clearing it so it cannot carry into the next one. */
export function takeExitCode(): number {
	let code = nodeProcess.exitCode ?? 0;
	nodeProcess.exitCode = undefined;
	return code;
}

export default nodeProcess as typeof import("node:process");
