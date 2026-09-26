// What a filesystem backend implements. The public extension point.
//
// A provider is the layer that actually knows how to reach some storage: puterfs over
// HTTP, an in-memory tree, a `FileSystemDirectoryHandle`, a zip's central directory.
// Everything above it — argument coercion, node's classes and overload sets — belongs to
// the `fs` surface inside the worker, and everything about *which* provider serves a path
// belongs to the mount table. A provider sees an absolute, already-normalized,
// mount-local path and a `Uint8Array`, and nothing else.
//
// This interface is **async throughout**, and that is the entire point of moving the
// filesystem out of the worker. The old worker-side contract could not await anything at
// all, because the same code had to satisfy `fs.readFileSync` over a *blocking*
// transport — so a backend was either an HTTP api reachable by synchronous XHR or
// something answered out of memory, and OPFS, the File System Access API and IndexedDB
// were all structurally excluded. Here an implementation may await whatever it likes;
// the worker's synchronous call is parked on a service-worker round trip while it does.
//
// Three invariants hold for every implementation, none of them expressible in the type
// system, and all three worth grepping for in review:
//
//   1. **Providers are pure with respect to the mount table.** A provider never asks
//      which mount serves a path it was handed. Layering is composed from outside
//      instead — `unionProvider(upper, lower)` receives its layers as arguments. Without
//      this rule the facade and the providers import each other, and whichever side
//      happens to evaluate first shifts with any unrelated change.
//   2. **Paths arrive canonical.** Absolute, normalized, mount-local, free of `.` and
//      `..`. The `fs` surface resolves against the cwd and the facade re-roots, so a
//      provider that re-normalizes is doing it twice, and one that expects a relative
//      path is wrong.
//   3. **Never await anything that needs the node worker to make progress.** A
//      synchronous `fs` call is blocked on this method returning, so reaching back into
//      the worker — `nodeWorker.import`, `nodeWorker.send`, anything that waits on the
//      runtime — is a hard deadlock rather than a slow path. A re-entrancy guard turns it
//      into a thrown error instead of a hang, but the rule is the real protection.
//
// Types only: this module is imported by the page, the worker and the service worker
// alike, so it must carry no runtime.

import type { FsEntry, Listing, ReaddirOpts, WireCtx } from "./entry";

/**
 * A streamed read.
 *
 * A plain web `ReadableStream` rather than a pull interface of our own, because that is
 * what every backend already hands you: `(await handle.getFile()).stream()` on OPFS or a
 * picked directory, `res.body` from a fetch, `new Blob([bytes]).stream()` from memory.
 * It is also transferable, so it crosses to the worker without being copied.
 *
 * Streaming sits outside the request/reply protocol on purpose — a frame's result is an
 * answer that has already completed, which is the one thing a stream is not — so this is
 * reachable only from the async transport. There is no synchronous streaming, and never
 * was.
 */
export interface ProviderStream {
	stream: ReadableStream<Uint8Array>;
	/** Total length, when the source knows it without reading. */
	size?: number;
}

export interface VfsProvider {
	/** For diagnostics and cache keying. */
	readonly name: string;

	// --- primitives: every provider implements these ---

	/** Throws a node-shaped ENOENT (or ENOTDIR) when the path isn't there — see `fsError`. */
	stat(ctx: WireCtx, path: string): Promise<FsEntry>;
	readdir(ctx: WireCtx, path: string, opts?: ReaddirOpts): Promise<Listing>;
	readFile(ctx: WireCtx, path: string): Promise<Uint8Array>;
	writeFile(ctx: WireCtx, path: string, data: Uint8Array): Promise<void>;
	mkdir(
		ctx: WireCtx,
		path: string,
		opts: { recursive: boolean }
	): Promise<string | undefined>;
	rm(
		ctx: WireCtx,
		path: string,
		opts: { recursive: boolean; force: boolean }
	): Promise<void>;
	rename(ctx: WireCtx, from: string, to: string): Promise<void>;
	/**
	 * Set access and modification times, in epoch milliseconds.
	 *
	 * Returns whether anything was actually applied. `false` is a normal answer, not an
	 * error, because a backend may not be able to represent the request at all: puterfs
	 * can only set a timestamp to *now*, and a `FileSystemFileHandle` cannot set one at
	 * all. A caller that gets `false` still owes the user an ENOENT for a missing path,
	 * so it validates some other way.
	 *
	 * Deciding what is representable belongs to the provider, since it is a fact about
	 * the backend. An in-memory tree simply sets both exactly.
	 */
	utimes(
		ctx: WireCtx,
		path: string,
		atimeMs: number,
		mtimeMs: number
	): Promise<boolean>;

	// --- optional fast paths: the facade derives these when absent ---

	/**
	 * Server-side copy. puterfs has `/copy`, which is one round trip and never moves the
	 * bytes through the browser; deriving it from read+write would move the whole file
	 * twice. Absent on backends where the derived form costs nothing.
	 */
	copyFile?(
		ctx: WireCtx,
		from: string,
		to: string,
		opts: { overwrite: boolean }
	): Promise<void>;
	/**
	 * Positioned read.
	 *
	 * Absent ⇒ the facade serves it by slicing a whole-file read, and the mount reports
	 * `hasNativeRange: false` so a positioned-read loop buffers the file once instead of
	 * re-reading it per chunk. Implementing this when it is *not* genuinely positioned
	 * is the one mistake here that costs quadratic transfer, so leave it off unless the
	 * backend really can seek — `Blob.slice()` can, a `Range` header can, a re-read
	 * cannot.
	 */
	readRange?(
		ctx: WireCtx,
		path: string,
		offset: number,
		length: number
	): Promise<Uint8Array>;
	statfs?(ctx: WireCtx): Promise<{ used: number; capacity: number }>;
	/** Absent ⇒ the facade synthesizes one from `readFile`, so callers never branch on this. */
	openRead?(
		ctx: WireCtx,
		path: string,
		range?: { start: number; end?: number }
	): Promise<ProviderStream>;
}
