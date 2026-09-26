// The vocabulary both sides of the filesystem boundary speak.
//
// This is the third shared module, alongside ../protocol.ts and ../util.ts, and it
// carries the tightest constraint of the three: it is bundled into *all* outputs —
// the worker, the page's `dist/index.js`, and the service worker — and only
// `dist/worker.js` has package resolution or node's own `path`/`Buffer`. So
// everything here and everywhere else under `src/vfs/` must be:
//
//   - free of package imports (`dist/index.js` is built with typescript alone; see
//     the third rollup entry, which has no nodeResolve),
//   - free of `Buffer` (a worker-only polyfill) — bytes are `Uint8Array`,
//   - free of `node/path` (`src/worker/node/path.ts` is a re-export out of the
//     node_core tree, reachable only through `nodeCorePlugin`) — see ./path.ts.
//
// Types only, no runtime, for one more reason: the worker's fs subgraph evaluates
// inside a module-init cycle (see ../worker/node/fs/lazy-base.ts) and anything
// imported this widely has to be safe to evaluate first.

/**
 * One directory entry, normalized.
 *
 * The shape every provider answers in and `Stats`/`Dirent` are built from, so
 * field-name and unit handling lives in exactly one place per backend rather than
 * leaking into the fs surface. Times are epoch **milliseconds** — puterfs speaks
 * seconds on the wire and converts on the way in.
 *
 * `path` is **mount-local** as a provider returns it; the facade lifts it onto the
 * absolute namespace before anyone above sees it.
 */
export interface FsEntry {
	path: string;
	name: string;
	uid: string;
	isDir: boolean;
	isSymlink: boolean;
	size: number;
	modifiedMs: number;
	createdMs: number;
	accessedMs: number;
}

export interface ReaddirOpts {
	recursive?: boolean;
	depth?: number;
	/** Stop once this many entries have accumulated. */
	maxEntries?: number;
}

export interface Listing {
	entries: FsEntry[];
	/**
	 * Whether the listing is *exhaustive*. A walk cut short by `maxEntries` is a
	 * valid prefix but not a complete picture, and the difference is load-bearing:
	 * "this directory contains nothing else" is what lets a later miss under it be
	 * answered as ENOENT without a round trip. Anything deriving a negative from a
	 * listing must check this first.
	 */
	complete: boolean;
}

/**
 * Per-operation context, threaded across the boundary with every call.
 *
 * Explicit rather than reconstructed by a `catch` upstream, because a backend's
 * error translation bakes the syscall and path into the *message string* — so
 * decorating an error after the fact would mean re-rendering it, and the worker
 * would have to re-derive a message the host already composed.
 */
export interface WireCtx {
	/** node's syscall name, for `err.syscall` and the message: "stat", "scandir", "open", "unlink", … */
	readonly syscall: string;
	/**
	 * The path as the caller spelled it, which is not the path the provider is
	 * working on. A zip mounted at `/p/node_modules` that fails on its local
	 * `/lodash/index.js` has to report `'/p/node_modules/lodash/index.js'`, or the
	 * error names a path that does not exist from the caller's point of view.
	 */
	readonly reportPath: string;
}
