import { KIND_CONTROL } from "../wire/kinds";
import { call } from "./wire";
import { writeStdio } from "./stdio";
import { stdioAsync } from "./node/fs/transport";
// The three web streams that used to be transferred here are gone. stdio is messages now,
// which is the only way `readSync(0)` and `writeSync(1)` can work at all — a stream can
// only be read asynchronously, so stdio built on one is stdio a node program cannot use
// from inside a synchronous call. The page keeps the embedder-facing ends and drives them
// from the `io.*` dispatcher.
import nodeBuffer from "./node/buffer";
import nodeStream from "./node/stream";
import nodeProcess from "./node/process";
import * as keepalive from "./keepalive";
import { setExitFlusher } from "./exit";

let isTTY = true;
let isRaw = false;

// Terminal dimensions, as `tty.WriteStream.columns`/`rows`.
//
// 80x24 until the host says otherwise, because a CLI reaching for `columns` gets a
// number either way: vite's build progress does `output.length < process.stdout.columns`
// and then `substring(0, columns - 1)`, so `undefined` there does not throw — it
// silently writes the empty string and the build appears to produce no output at all.
let columns = 80;
let rows = 24;

/**
 * Is stdin in raw mode?
 *
 * Read by the synchronous `fs` path, which has to answer `readSync(0)` the way node does — and
 * node's answer depends on this. See `readStdinSync`.
 */
export function isRawMode(): boolean {
	return isRaw;
}

export function setIsTTY(IsTTY: boolean) {
	isTTY = IsTTY;
}

/**
 * Report new terminal dimensions, and tell the program about them.
 *
 * Updating `columns`/`rows` is not enough on its own: a full-screen TUI lays out once and then
 * repaints on an event, so a resize that only changes the numbers is invisible until something
 * else happens to trigger a render. Node signals this two ways and real programs use both — Ink
 * listens for `"resize"` on the stream, while readline-based CLIs handle `SIGWINCH` — so both are
 * emitted here.
 *
 * Only when a dimension actually changed: the host calls this on every `fit()`, and a ResizeObserver
 * fires plenty of times that resolve to the same cell grid. Re-laying-out on each of those would
 * make dragging a window edge quadratic.
 */
export function setTTYSize(size: { columns?: number; rows?: number }) {
	let previousColumns = columns;
	let previousRows = rows;

	if (size.columns && size.columns > 0) columns = Math.floor(size.columns);
	if (size.rows && size.rows > 0) rows = Math.floor(size.rows);

	if (columns === previousColumns && rows === previousRows) return;

	// Guarded because these run before `initConsole` on the very first size message.
	stdoutStream?.emit("resize");
	stderrStream?.emit("resize");
	nodeProcess.emit("SIGWINCH");
}

export interface TTYStateChange {
	isRaw?: boolean;
	echo?: boolean;
}

export let stdinStream: InstanceType<typeof nodeStream.Readable>;
export let stdoutStream: InstanceType<typeof nodeStream.Writable>;
export let stderrStream: InstanceType<typeof nodeStream.Writable>;

/** Captured before anything can wrap it, so a flush cannot be kept alive by its own wait. */
const realSetTimeout = globalThis.setTimeout;

function nextMacrotask(): Promise<void> {
	return new Promise<void>((r) => realSetTimeout(r, 0));
}

/** How much stdin is asked for in one read. */
const STDIN_CHUNK = 64 * 1024;

function attachTTYGetter(stream: object) {
	Object.defineProperty(stream, "isTTY", {
		configurable: true,
		enumerable: true,
		get() {
			return isTTY;
		},
	});
}

async function emitTTYStateChange(change: TTYStateChange) {
	await call(KIND_CONTROL, {
		op: "ctl.tty",
		isRaw: change.isRaw,
		echo: change.echo,
	});
}

// Node's tty.WriteStream exposes getColorDepth()/hasColors(); console's
// `shouldColorize` consults getColorDepth() to enable ANSI output. We report
// 24-bit truecolor while a TTY (xterm renders ANSI) and monochrome otherwise,
// so colorization tracks the TTY state set via setIsTTY().
function attachColorCapabilities(stream: object) {
	Object.defineProperty(stream, "getColorDepth", {
		configurable: true,
		enumerable: true,
		value() {
			return isTTY ? 24 : 1;
		},
	});

	Object.defineProperty(stream, "hasColors", {
		configurable: true,
		enumerable: true,
		value(count?: number) {
			if (!isTTY) return false;
			return count === undefined ? true : count <= 2 ** 24;
		},
	});
}

// The cursor half of node's tty.WriteStream: `columns`, `rows`, and the four movement
// helpers, each emitting the ANSI sequence node's readline would.
//
// Node only puts these on a stream that *is* a TTY, and a CLI is supposed to check
// `isTTY` first. Plenty do not — vite's build has both a guarded `clearLine()` and an
// unguarded one — so a missing method surfaces as `process.stdout.clearLine is not a
// function` in the middle of an otherwise working build. Providing them
// unconditionally costs nothing: when the output is not a terminal the sequences are
// inert bytes, which is the same thing a redirected TTY write would be.
function attachCursorControl(stream: object) {
	let define = (name: string, value: unknown) =>
		Object.defineProperty(stream, name, {
			configurable: true,
			enumerable: true,
			value,
		});

	for (let [name, get] of [
		["columns", () => columns],
		["rows", () => rows],
	] as const) {
		Object.defineProperty(stream, name, {
			configurable: true,
			enumerable: true,
			get,
		});
	}

	// Every one of these takes an optional callback and returns true, as node's do:
	// there is no backpressure to report because the write is already queued.
	let emit = (sequence: string, callback?: () => void) => {
		(stream as any).write(sequence);
		callback?.();
		return true;
	};

	define("getWindowSize", () => [columns, rows]);

	define("clearLine", (dir: number, callback?: () => void) =>
		// -1 to the cursor, 1 from the cursor, 0 the whole line.
		emit(dir < 0 ? "\x1b[1K" : dir > 0 ? "\x1b[0K" : "\x1b[2K", callback)
	);

	define("clearScreenDown", (callback?: () => void) =>
		emit("\x1b[0J", callback)
	);

	define(
		"cursorTo",
		(x: number, y?: number | (() => void), callback?: () => void) => {
			// node allows cursorTo(x, cb) as well as cursorTo(x, y, cb).
			if (typeof y === "function") {
				callback = y;
				y = undefined;
			}
			let column = Math.max(0, Math.floor(x)) + 1;
			if (y === undefined) return emit(`\x1b[${column}G`, callback);
			return emit(
				`\x1b[${Math.max(0, Math.floor(y)) + 1};${column}H`,
				callback
			);
		}
	);

	define("moveCursor", (dx: number, dy: number, callback?: () => void) => {
		let sequence = "";
		if (dy < 0) sequence += `\x1b[${-dy}A`;
		else if (dy > 0) sequence += `\x1b[${dy}B`;
		if (dx > 0) sequence += `\x1b[${dx}C`;
		else if (dx < 0) sequence += `\x1b[${-dx}D`;
		return emit(sequence, callback);
	});
}

function attachTTYControl(stream: object) {
	Object.defineProperty(stream, "isRaw", {
		configurable: true,
		enumerable: true,
		get() {
			return isRaw;
		},
	});

	Object.defineProperty(stream, "setRawMode", {
		configurable: true,
		enumerable: true,
		value(mode: boolean) {
			let next = !!mode;
			let prev = isRaw;
			isRaw = next;
			emitTTYStateChange({ isRaw, echo: !isRaw });
			return prev;
		},
	});
}

function makeReadableStream(): InstanceType<typeof nodeStream.Readable> {
	let reading = false;
	let ended = false;
	let paused = false;
	let userUnrefed = false;
	let refed = false;

	// Mirror libuv's readStart/readStop + ref semantics for stdin. In Node the
	// process stays alive while `process.stdin` is an active, refed handle: the
	// stream is actively reading (a consumer wants data), it is not paused, and
	// it has not been `.unref()`ed. This is what keeps a TUI (e.g. an agent CLI)
	// alive while it sits at an idle prompt blocked on a keypress with no pending
	// timers. Without it the ref count hits zero and `drain()` settles the run
	// prematurely. `reading` tracks readStart/readStop, the pause/resume/end/
	// close listeners below track flowing state, and ref()/unref() the manual
	// override.
	function syncKeepalive() {
		let want = reading && !paused && !ended && !userUnrefed;
		if (want === refed) return;
		refed = want;
		if (refed) keepalive.ref();
		else keepalive.unref();
	}

	let stream = new nodeStream.Readable({
		read() {
			if (reading || ended) {
				return;
			}

			reading = true;
			syncKeepalive();

			void (async () => {
				try {
					while (!stream.destroyed) {
						// A blocking read: the host answers when there is input or when stdin
						// ends, however long that takes. The service worker's deadline is
						// refreshed by a heartbeat rather than capping this, which is what lets
						// a prompt sit waiting for a person.
						let answer = await stdioAsync({
							op: "io.read",
							fd: 0,
							length: STDIN_CHUNK,
							blocking: true,
						});
						if (answer.value.eof) {
							ended = true;
							stream.push(null);
							return;
						}
						let value = answer.parts[0];
						if (!value?.length) {
							continue;
						}

						if (!stream.push(nodeBuffer.Buffer.from(value))) {
							return;
						}
					}
				} catch (error) {
					stream.destroy(error as Error);
				} finally {
					reading = false;
					syncKeepalive();
				}
			})();
		},
	});

	// A paused stream is not a live read handle; ending/closing it retires the
	// handle for good. Track both so the ref count follows the stream's state.
	stream.on("pause", () => {
		paused = true;
		syncKeepalive();
	});
	stream.on("resume", () => {
		paused = false;
		syncKeepalive();
	});
	stream.on("end", () => {
		ended = true;
		syncKeepalive();
	});
	stream.on("close", () => {
		ended = true;
		syncKeepalive();
	});

	// Node's process.stdin is a Socket/ReadStream, so it exposes ref/unref;
	// programs that want the run to be able to exit while still occasionally
	// reading stdin rely on `process.stdin.unref()`.
	(stream as any).ref = function () {
		userUnrefed = false;
		syncKeepalive();
		return stream;
	};
	(stream as any).unref = function () {
		userUnrefed = true;
		syncKeepalive();
		return stream;
	};

	(stream as typeof stream & { fd?: number }).fd = 0;
	attachTTYGetter(stream);
	attachTTYControl(stream);
	return stream;
}

function toUint8Array(
	chunk: string | ArrayBufferView | ArrayBuffer,
	encoding: BufferEncoding
): Uint8Array<ArrayBuffer> {
	if (typeof chunk === "string") {
		return Uint8Array.from(nodeBuffer.Buffer.from(chunk, encoding));
	}

	if (chunk instanceof ArrayBuffer) {
		return new Uint8Array(chunk.slice(0));
	}

	if (ArrayBuffer.isView(chunk)) {
		return Uint8Array.from(
			new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength)
		);
	}

	throw new TypeError("unsupported stdio chunk type");
}

function makeWritableStream(
	fd: 1 | 2
): InstanceType<typeof nodeStream.Writable> {
	let stream = new nodeStream.Writable({
		write(chunk, encoding, callback) {
			// Synchronous, and completed immediately. `writeStdio` buffers, and the bytes
			// leave as a sideband on whatever message goes next — so there is nothing to
			// await and nothing that can reorder against a later synchronous call, which is
			// exactly what node's own write to a TTY guarantees.
			writeStdio(
				fd,
				toUint8Array(chunk as string | ArrayBufferView | ArrayBuffer, encoding)
			);
			callback();
		},
		final(callback) {
			// Nothing to close: the buffer is not a stream, and a program that ends its own
			// stdout has not ended the terminal's.
			callback();
		},
		destroy(error, callback) {
			callback(error);
		},
	});

	(stream as typeof stream & { fd?: number }).fd = fd;
	attachTTYGetter(stream);
	attachColorCapabilities(stream);
	attachCursorControl(stream);
	return stream;
}

// Snapshot the worker's native (devtools) console methods before anything
// installs the Node `console` global (module/globals.ts does that, but it is
// imported after this module — see module/cjs.ts). Internal worker code logs
// through these so its output reaches devtools instead of the user program's
// stdout/stderr. User-facing `console.*` is the real Node console, which writes
// only to process.stdout/stderr (module/node/console.ts).
export let console_debug = console.debug.bind(console);
export let console_log = console.log.bind(console);
export let console_info = console.info.bind(console);
export let console_warn = console.warn.bind(console);
export let console_error = console.error.bind(console);

export function initConsole(settings: { isTTY: boolean }) {
	isTTY = settings.isTTY;
	isRaw = false;

	// Deferred from module-init: nodeStream's CJS wrapper hasn't run yet at
	// our top-level, so `new nodeStream.Readable()` would throw.
	if (!stdinStream) {
		stdinStream = makeReadableStream();
		stdoutStream = makeWritableStream(1);
		stderrStream = makeWritableStream(2);
		nodeProcess.stdin = stdinStream as any;
		nodeProcess.stdout = stdoutStream as any;
		nodeProcess.stderr = stderrStream as any;
	}

	// So `process.exit` can get the program's output out before the page, which
	// terminates this worker on hearing about the exit, has a chance to.
	setExitFlusher(flushConsole);
}

/**
 * Settle once everything written to stdout and stderr has reached the page.
 *
 * One round trip. This used to be a loop that compared two chunk counters and gave up
 * after a hundred turns without progress, because output crossed the boundary through a
 * `TransformStream` bridge and a serialized writer with its own backpressure, and there
 * was no way to *ask* whether it had all arrived — only to watch and guess. A fixed pass
 * count silently became a limit on how much a program could print, which is how a 2000-line
 * run once came out truncated at line 1006.
 *
 * There is nothing to guess at now: `io.flush` is a message, and its reply means the host
 * has the bytes.
 */
export async function flushConsole(): Promise<void> {
	// Give anything that queued output in this turn a chance to reach the buffer first.
	await nextMacrotask();
	await stdioAsync({ op: "io.flush" });
}
