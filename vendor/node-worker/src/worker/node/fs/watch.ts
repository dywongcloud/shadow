// fs.watch / fs.watchFile, backed by puterfs change notifications.
//
// puter has no watch api. What it does have is a socket.io feed of `item.*`
// mutations scoped to the authenticated user, which the page relays to us as
// `PuterFsEvent`s (see ../../fsevents.ts). Those events carry absolute paths for
// the user's whole tree, so a watcher is pure local filtering — every watcher
// shares one connection and `recursive: true` costs nothing extra.
//
// Divergences from node, each commented at its site:
//   - `fs.watch` does not throw ENOENT synchronously.
//   - only the authenticated user's own changes are visible (the api scopes the
//     feed to the user's room), so another user's writes into a shared
//     directory produce nothing.
//   - a recursive delete reports the top entry, not each descendant.
//
// ## When there is no socket
//
// There often isn't. puter's socket refuses anything but a *user* token, and an
// app is handed one only in godmode — so on an ordinary launch this feed never
// connects and, until the fallback below, `fs.watch` reported nothing at all for
// the whole session.
//
// The page polls puterfs's change counter in that case and relays a pathless
// `stale`: something moved, and nobody can say what. A watcher answers it by
// listing what it watches and diffing against the last listing, which is how
// node's own `watchFile` has always worked — one listing per watcher per change,
// rather than the one stat per file per interval `StatWatcher` used to run.
//
// The first `stale` a watcher sees only establishes that baseline, so it reports
// nothing. That is why the page announces a gap the moment the socket is
// *refused* rather than waiting for the first change: the baseline is taken while
// nothing has happened yet, and the first real change is a diff.

import nodeBuffer from "../buffer";
import nodePath from "../path";
import * as keepalive from "../../keepalive";
import {
	fsEventsCovered,
	onFsEventsStale,
	onFsEventsState,
	subscribeFsEvents,
	type PuterFsEvent,
} from "../../fsevents";
import { ctx, hostAsync } from "./host";
import { normalizePath, type AnyStats } from "./util";
import { Stats } from "./classes";
import { promisesToDepromisify } from "./promises";
// Not `events.EventEmitter` directly: see ./lazy-base.ts for why the fs subgraph
// can't read a `node/*` barrel at module scope.
import { EmitterBase } from "./lazy-base";

type NodeFs = typeof import("node:fs");

let Buffer = nodeBuffer.Buffer;

// node's default `watchFile` poll interval. We only poll as a fallback (see
// StatWatcher), but the option still has to mean something.
const DEFAULT_INTERVAL = 5007;

function abortError(): Error {
	let err = new Error("The operation was aborted") as Error & { code: string };
	err.name = "AbortError";
	err.code = "ABORT_ERR";
	return err;
}

// node applies the `encoding` option to the raw filename bytes: "buffer" hands
// back a Buffer, anything else re-encodes.
function encodeName(name: string, encoding: string | undefined | null) {
	if (encoding === "buffer") return Buffer.from(name, "utf8");
	if (!encoding || encoding === "utf8" || encoding === "utf-8") return name;
	return Buffer.from(name, "utf8").toString(encoding as BufferEncoding);
}

// Strips the trailing slash so `dirname`/`startsWith` comparisons line up.
// "/" stays "/" — it has no parent and every path is under it.
function watchRoot(path: string): string {
	if (path.length > 1 && path.endsWith("/")) return path.slice(0, -1);
	return path;
}

interface NormalizedWatchOptions {
	persistent: boolean;
	recursive: boolean;
	encoding: string;
	signal?: AbortSignal;
}

function watchOptions(options: any): NormalizedWatchOptions {
	if (typeof options === "string") options = { encoding: options };
	else if (!options) options = {};
	return {
		persistent: options.persistent !== false,
		recursive: !!options.recursive,
		encoding: options.encoding ?? "utf8",
		signal: options.signal,
	};
}

export class FSWatcher extends EmitterBase {
	#root: string;
	/** `#root` with a trailing slash, for recursive prefix matching. */
	#prefix: string;
	#recursive: boolean;
	#encoding: string;
	#closed = false;
	#unsubscribe: (() => void) | undefined;
	#detachStale: (() => void) | undefined;
	#detachSignal: (() => void) | undefined;
	/**
	 * Last known contents, relative name → identity, for diffing a coarse
	 * `stale` against. Undefined until the first one arrives, so a watcher on a
	 * healthy socket never lists anything.
	 */
	#snapshot: Map<string, string> | undefined;
	#rescanning = false;
	/** A `stale` that landed while a rescan was already in flight. */
	#rescanQueued = false;
	// A watcher is an active handle in libuv's sense: while it's alive and
	// ref'ed, `drain()` must not settle the run. Tracked as a bool so repeated
	// ref()/unref() calls are no-ops, matching node.
	#refed = false;

	// Queue + waiter for the async-iterator form (`fsPromises.watch`). Events are
	// buffered only once someone has asked for an iterator, so the common
	// listener-only `fs.watch` never accumulates anything.
	#queue: Array<{ eventType: string; filename: any }> = [];
	#waiter: (() => void) | undefined;
	#iterating = false;
	#iterError: Error | undefined;

	constructor(path: string, options: NormalizedWatchOptions) {
		super();
		this.#root = watchRoot(path);
		this.#prefix = this.#root === "/" ? "/" : this.#root + "/";
		this.#recursive = options.recursive;
		this.#encoding = options.encoding;

		if (options.persistent) this.ref();

		this.#unsubscribe = subscribeFsEvents((event) => this.#onEvent(event));
		this.#detachStale = onFsEventsStale(() => void this.#rescan());

		if (options.signal) {
			let signal = options.signal;
			if (signal.aborted) {
				// Close on the next tick rather than here, so the caller still
				// gets a usable watcher object back first.
				queueMicrotask(() => this.close());
			} else {
				let onAbort = () => this.close();
				signal.addEventListener("abort", onAbort, { once: true });
				this.#detachSignal = () => signal.removeEventListener("abort", onAbort);
			}
		}

		// node's fs.watch throws ENOENT *synchronously*. Doing that here would
		// cost a blocking XMLHttpRequest per watch() call, and chokidar calls
		// watch() once per directory — a large project would stall the worker on
		// N serial round trips during setup. Probe asynchronously instead and
		// surface the failure as an 'error' event, a shape consumers already
		// handle (node emits it for late failures too).
		promisesToDepromisify.stat(this.#root).catch((err) => {
			if (this.#closed) return;
			this.emit("error", err);
			this.close();
		});
	}

	#onEvent(event: PuterFsEvent) {
		if (this.#closed) return;

		this.#consider(event.path, event.kind, event.descendantsOnly);
		// A move is two filesystem events: the entry left one place and arrived
		// at another. node reports 'rename' at both ends.
		if (event.kind === "moved" && event.oldPath) {
			this.#consider(event.oldPath, "removed", false);
		}
	}

	#consider(
		path: string,
		kind: PuterFsEvent["kind"],
		descendantsOnly: boolean | undefined
	) {
		let filename: string;

		if (path === this.#root) {
			// The watched entry itself. `descendants_only` means the entry
			// survived and only its children were dropped (how the api reports
			// emptying Trash), so that's a change to this directory rather than a
			// rename of it.
			if (kind === "removed" && descendantsOnly) {
				this.#dispatch("change", nodePath.basename(this.#root));
				return;
			}
			this.#dispatch(
				kind === "updated" ? "change" : "rename",
				nodePath.basename(this.#root)
			);
			return;
		}

		if (nodePath.dirname(path) === this.#root) {
			filename = nodePath.basename(path);
		} else if (this.#recursive && path.startsWith(this.#prefix)) {
			filename = nodePath.relative(this.#root, path);
		} else {
			return;
		}

		// A directory removed recursively yields one api event for the directory
		// itself, not one per descendant, so a recursive watcher sees the
		// directory disappear but not its contents. Consumers that care re-read
		// the tree on any event, which is what chokidar does.
		this.#dispatch(kind === "updated" ? "change" : "rename", filename);
	}

	/**
	 * Re-list what this watcher watches and report whatever moved.
	 *
	 * The answer to a `stale`, which carries no path. Serialized rather than
	 * queued deeply: one more listing settles everything a burst of counter bumps
	 * could have meant, so overlapping rescans would only re-read the same tree.
	 */
	async #rescan(): Promise<void> {
		if (this.#closed) return;
		if (this.#rescanning) {
			this.#rescanQueued = true;
			return;
		}
		this.#rescanning = true;
		try {
			do {
				this.#rescanQueued = false;
				await this.#rescanOnce();
			} while (this.#rescanQueued && !this.#closed);
		} finally {
			this.#rescanning = false;
		}
	}

	async #rescanOnce(): Promise<void> {
		let next = new Map<string, string>();
		try {
			let listing = await hostAsync.readdir(
				ctx("scandir", this.#root),
				this.#root,
				this.#recursive ? { recursive: true } : undefined
			);
			for (let entry of listing.entries) {
				// Relative, because that is the `filename` node reports and it is what
				// the non-recursive and recursive cases have in common.
				next.set(
					nodePath.relative(this.#root, entry.path),
					entry.isDir ? "d" : `f:${entry.uid}:${entry.size}:${entry.modifiedMs}`
				);
			}
		} catch (err) {
			if (this.#closed) return;
			// The watched directory itself is gone. node reports that as a rename of
			// the entry, which is what `#consider` does for a removal at the root.
			let code = (err as NodeJS.ErrnoException)?.code;
			if (code === "ENOENT" || code === "ENOTDIR") {
				if (this.#snapshot) {
					this.#snapshot = undefined;
					this.#dispatch("rename", nodePath.basename(this.#root));
				}
				return;
			}
			return;
		}
		if (this.#closed) return;

		let previous = this.#snapshot;
		this.#snapshot = next;
		// Nothing to compare against yet — this listing *is* the baseline. See the
		// header on why it is taken before the first change rather than after.
		if (!previous) return;

		for (let [name, identity] of next) {
			let before = previous.get(name);
			if (before === undefined) this.#dispatch("rename", name);
			else if (before !== identity) this.#dispatch("change", name);
		}
		for (let name of previous.keys()) {
			if (!next.has(name)) this.#dispatch("rename", name);
		}
	}

	#dispatch(eventType: string, name: string) {
		let filename = encodeName(name, this.#encoding);
		this.emit("change", eventType, filename);
		if (this.#iterating) {
			this.#queue.push({ eventType, filename });
			this.#wake();
		}
	}

	#wake() {
		let waiter = this.#waiter;
		this.#waiter = undefined;
		if (waiter) waiter();
	}

	close(): void {
		if (this.#closed) return;
		this.#closed = true;

		this.#unsubscribe?.();
		this.#unsubscribe = undefined;
		this.#detachStale?.();
		this.#detachStale = undefined;
		this.#detachSignal?.();
		this.#detachSignal = undefined;
		this.#snapshot = undefined;
		this.unref();

		this.emit("close");
		this.#wake();
	}

	ref(): this {
		if (!this.#refed && !this.#closed) {
			this.#refed = true;
			keepalive.ref();
		}
		return this;
	}

	unref(): this {
		if (this.#refed) {
			this.#refed = false;
			keepalive.unref();
		}
		return this;
	}

	// node's FSWatcher is async-iterable, which is how `fsPromises.watch` is
	// built. Buffering starts here, not when the generator body first runs, so
	// events between `watch()` and the first `next()` aren't lost.
	[Symbol.asyncIterator](): AsyncGenerator<
		{ eventType: string; filename: any },
		void
	> {
		this.#iterating = true;
		return this.#iterate();
	}

	async *#iterate(): AsyncGenerator<
		{ eventType: string; filename: any },
		void
	> {
		try {
			while (true) {
				while (this.#queue.length > 0) yield this.#queue.shift()!;
				if (this.#iterError) throw this.#iterError;
				if (this.#closed) return;
				await new Promise<void>((resolve) => {
					this.#waiter = resolve;
				});
			}
		} finally {
			this.#iterating = false;
			this.#queue.length = 0;
			this.close();
		}
	}

	// @internal — lets `fsPromises.watch` turn an abort into a rejection rather
	// than a silent end-of-iteration.
	failIteration(err: Error) {
		this.#iterError = err;
		this.#wake();
	}
}

// A `Stats` for a path that doesn't exist. node hands `watchFile` listeners an
// all-zero Stats in that case, which is how consumers tell "gone" from "never
// existed" (both `mtimeMs` read 0).
function missingStats(path: string, bigint: boolean): AnyStats {
	return new Stats(
		{
			path,
			name: nodePath.basename(path),
			uid: "",
			isDir: false,
			isSymlink: false,
			size: 0,
			modifiedMs: 0,
			createdMs: 0,
			accessedMs: 0,
		},
		bigint,
		false
	) as AnyStats;
}

function statsDiffer(a: any, b: any): boolean {
	// Everything puterfs varies is covered by these three; dev/ino/nlink/mode are
	// synthesized constants here, so comparing them could never fire.
	return (
		String(a.mtimeMs) !== String(b.mtimeMs) ||
		String(a.size) !== String(b.size) ||
		String(a.ctimeMs) !== String(b.ctimeMs)
	);
}

export class StatWatcher extends EmitterBase {
	#path: string;
	#bigint: boolean;
	#interval: number;
	#prev: AnyStats | undefined;
	#unsubscribe: (() => void) | undefined;
	#detachStale: (() => void) | undefined;
	#detachState: (() => void) | undefined;
	#timer: ReturnType<typeof setInterval> | undefined;
	#stopped = false;
	#checking = false;
	#refed = false;

	constructor(
		path: string,
		options: { bigint?: boolean; interval?: number; persistent?: boolean }
	) {
		super();
		this.#path = path;
		this.#bigint = !!options.bigint;
		this.#interval = options.interval ?? DEFAULT_INTERVAL;

		if (options.persistent !== false) this.ref();

		// Seed `prev` without emitting: node's first 'change' reflects a real
		// change, not the initial observation.
		void this.#readStats().then((stats) => {
			if (!this.#stopped && !this.#prev) this.#prev = stats;
		});

		this.#unsubscribe = subscribeFsEvents((event) => {
			if (event.path === this.#path || event.oldPath === this.#path) {
				void this.#check();
			}
		});

		// The page's change counter is the cheap fallback: one small request for the
		// whole tab, whoever is watching, instead of one stat per watched file per
		// interval. A bump means *something* changed, so re-stat and let
		// `statsDiffer` decide — which is what this watcher would have done on its
		// own schedule anyway, only now on the schedule of actual change.
		this.#detachStale = onFsEventsStale(() => void this.#check());

		// node really does poll, and the interval survives as the last resort: it
		// covers the stretch where neither the socket nor the counter is answering,
		// which is the one case nothing else can report. A literal 5007ms-per-file
		// poll is one api call per file per interval, so it stops the moment
		// anything cheaper is reporting.
		this.#detachState = onFsEventsState((isCovered) => {
			if (isCovered) this.#stopPolling();
			else this.#startPolling();
		});
		if (!fsEventsCovered()) this.#startPolling();
	}

	async #readStats(): Promise<AnyStats> {
		try {
			return (await promisesToDepromisify.stat(this.#path, {
				bigint: this.#bigint,
			} as any)) as AnyStats;
		} catch {
			return missingStats(this.#path, this.#bigint);
		}
	}

	async #check() {
		if (this.#stopped || this.#checking) return;
		this.#checking = true;
		try {
			let curr = await this.#readStats();
			if (this.#stopped) return;
			let prev = this.#prev ?? missingStats(this.#path, this.#bigint);
			this.#prev = curr;
			if (statsDiffer(curr, prev)) this.emit("change", curr, prev);
		} finally {
			this.#checking = false;
		}
	}

	#startPolling() {
		if (this.#timer !== undefined || this.#stopped) return;
		this.#timer = setInterval(() => void this.#check(), this.#interval);
		// The interval must not hold the run open on its own: the watcher's ref
		// already does that, and an unref'ed watcher shouldn't be resurrected by
		// its fallback poller.
		(this.#timer as any)?.unref?.();
	}

	#stopPolling() {
		if (this.#timer === undefined) return;
		clearInterval(this.#timer);
		this.#timer = undefined;
	}

	stop(): void {
		if (this.#stopped) return;
		this.#stopped = true;
		this.#stopPolling();
		this.#unsubscribe?.();
		this.#unsubscribe = undefined;
		this.#detachStale?.();
		this.#detachStale = undefined;
		this.#detachState?.();
		this.#detachState = undefined;
		this.unref();
		this.emit("stop");
	}

	ref(): this {
		if (!this.#refed && !this.#stopped) {
			this.#refed = true;
			keepalive.ref();
		}
		return this;
	}

	unref(): this {
		if (this.#refed) {
			this.#refed = false;
			keepalive.unref();
		}
		return this;
	}
}

// One StatWatcher per path, as in node: a second watchFile() on the same path
// adds a listener to the existing watcher rather than opening a second one, and
// that's what lets `unwatchFile(path)` stop all of them at once.
let statWatchers = new Map<string, StatWatcher>();

function watchImpl(
	filename: any,
	optionsOrListener?: any,
	maybeListener?: any
): FSWatcher {
	let listener =
		typeof optionsOrListener === "function" ? optionsOrListener : maybeListener;
	let options = watchOptions(
		typeof optionsOrListener === "function" ? undefined : optionsOrListener
	);

	let watcher = new FSWatcher(normalizePath(filename), options);
	if (listener) watcher.on("change", listener);
	return watcher;
}

function watchFileImpl(
	filename: any,
	optionsOrListener?: any,
	maybeListener?: any
): StatWatcher {
	let listener =
		typeof optionsOrListener === "function" ? optionsOrListener : maybeListener;
	let options =
		typeof optionsOrListener === "function" ? {} : (optionsOrListener ?? {});

	let path = normalizePath(filename);
	let watcher = statWatchers.get(path);
	if (!watcher) {
		watcher = new StatWatcher(path, options);
		statWatchers.set(path, watcher);
	}
	if (listener) watcher.on("change", listener);
	return watcher;
}

function unwatchFileImpl(filename: any, listener?: any): void {
	let path = normalizePath(filename);
	let watcher = statWatchers.get(path);
	if (!watcher) return;

	if (listener) watcher.removeListener("change", listener);
	else watcher.removeAllListeners("change");

	if (watcher.listenerCount("change") === 0) {
		statWatchers.delete(path);
		watcher.stop();
	}
}

// `fsPromises.watch` — the async-iterator form. node returns the iterator
// itself, not the watcher, so the only ways to stop it are breaking out of the
// loop (the generator's `finally` closes the watcher) or aborting the signal.
function promisesWatchImpl(filename: any, options?: any) {
	let normalized = watchOptions(options);
	// The caller never sees the watcher, so it can't unref() one; node treats
	// fsPromises.watch as non-persistent for the same reason.
	let watcher = new FSWatcher(normalizePath(filename), {
		...normalized,
		persistent: false,
	});

	if (normalized.signal) {
		let signal = normalized.signal;
		let fail = () => watcher.failIteration(abortError());
		if (signal.aborted) fail();
		else signal.addEventListener("abort", fail, { once: true });
	}

	return watcher[Symbol.asyncIterator]();
}

// node's callback fs is heavily overloaded and a single implementation signature
// can't satisfy the union, so each is cast at the boundary — the same approach
// `depromisify` takes in ../utils.ts.
export let watch = watchImpl as unknown as NodeFs["watch"];
export let watchFile = watchFileImpl as unknown as NodeFs["watchFile"];
export let unwatchFile = unwatchFileImpl as unknown as NodeFs["unwatchFile"];
export let promisesWatch =
	promisesWatchImpl as unknown as (typeof import("node:fs/promises"))["watch"];
