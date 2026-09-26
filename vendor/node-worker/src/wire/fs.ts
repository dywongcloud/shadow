// The filesystem op set.
//
// The framing, the transports and the sequence numbering all come from ./frame.ts and
// ./message.ts — this is only the vocabulary. Sharing the pipe is the point: the hard
// part of calling out of a worker is doing it *synchronously*, and that machinery is
// expensive to build and free to reuse.

import type { FsEntry, Listing, ReaddirOpts, WireCtx } from "../vfs/entry";
import type { WireRequest } from "./message";

/**
 * One filesystem operation.
 *
 * Note what is *not* a primitive here. `exists`, `access`, `append`, `truncate`,
 * `mkdtemp`, `cp`, `rmrf` and `mkdirp` are compositions — the worker used to build
 * them out of several provider calls, which cost several round trips each. They are
 * single ops now because the host owns the whole filesystem and can compose them
 * locally, which is what keeps every `fs.*Sync` call to exactly one blocking request.
 *
 * `cp` deliberately carries no `filter`: it is a user callback living in the worker,
 * so a filtered copy stays a worker-driven walk.
 */
export type VfsCall =
	/** Verifies end-to-end interception at startup. See `NodeFsCapabilities`. */
	| { op: "probe" }
	| { op: "mounts" }
	| { op: "stat"; ctx: WireCtx; path: string }
	| { op: "access"; ctx: WireCtx; path: string }
	| { op: "exists"; ctx: WireCtx; path: string }
	| { op: "statfs"; ctx: WireCtx; path: string }
	| { op: "readdir"; ctx: WireCtx; path: string; opts?: ReaddirOpts }
	| { op: "readFile"; ctx: WireCtx; path: string }
	| {
			op: "readRange";
			ctx: WireCtx;
			path: string;
			offset: number;
			length: number;
	  }
	/** Bytes in `parts[0]`. */
	| { op: "writeFile"; ctx: WireCtx; path: string }
	/** Bytes in `parts[0]`. */
	| { op: "append"; ctx: WireCtx; path: string }
	| { op: "mkdir"; ctx: WireCtx; path: string; recursive: boolean }
	| { op: "rm"; ctx: WireCtx; path: string; recursive: boolean; force: boolean }
	| { op: "rmrf"; ctx: WireCtx; path: string; force: boolean }
	| { op: "rename"; ctx: WireCtx; from: string; to: string }
	| {
			op: "copyFile";
			ctx: WireCtx;
			from: string;
			to: string;
			overwrite: boolean;
	  }
	| {
			op: "utimes";
			ctx: WireCtx;
			path: string;
			atimeMs: number;
			mtimeMs: number;
	  }
	| { op: "truncate"; ctx: WireCtx; path: string; length: number }
	| { op: "mkdtemp"; ctx: WireCtx; prefix: string }
	| {
			op: "cp";
			ctx: WireCtx;
			from: string;
			to: string;
			recursive: boolean;
			force: boolean;
			errorOnExist: boolean;
	  }
	/**
	 * A stream over a path or an open fd, for `createReadStream`.
	 *
	 * The one fs op whose reply carries an attachment rather than a value, and therefore the
	 * one that can never be synchronous: a stream is an answer that has *not* completed,
	 * which is precisely what a message body cannot be. `fd` rather than `path` when the
	 * caller supplied one — a handle may hold bytes the backend has not seen, and those are
	 * the file as far as that fd is concerned.
	 */
	| {
			op: "fs.openRead";
			path?: string;
			fd?: number;
			start?: number;
			end?: number;
	  }
	| VfsFdCall;
/**
 * The fd family.
 *
 * These exist because the handle — the fd table, the whole-file buffer, the append cursor, the
 * byte-range cache — lives next to the providers rather than in the worker. That is what makes
 * every `fs.*Sync` call exactly one blocking round trip: `writeSync` used to be three (stat,
 * readFile, writeFile) and `readvSync`/`writevSync` were one per buffer.
 *
 * It also keeps the buffer on the same side as the backend, so an `appendFileSync` in a loop
 * ships only the delta instead of pulling the whole file into the worker each time.
 */
export type VfsFdCall =
	/** `flags` is normalized but not parsed: a canonical string, or an O_* bitmask. */
	| { op: "open"; ctx: WireCtx; path: string; flags: string | number }
	| { op: "close"; ctx: WireCtx; fd: number }
	| {
			op: "fdRead";
			ctx: WireCtx;
			fd: number;
			length: number;
			position: number | null;
	  }
	| {
			op: "fdReadv";
			ctx: WireCtx;
			fd: number;
			lengths: number[];
			position: number | null;
	  }
	/** Bytes in `parts[0]`. */
	| { op: "fdWrite"; ctx: WireCtx; fd: number; position: number | null }
	/** One part per buffer. */
	| { op: "fdWritev"; ctx: WireCtx; fd: number; position: number | null }
	| { op: "fdReadFile"; ctx: WireCtx; fd: number }
	/** Bytes in `parts[0]`. */
	| { op: "fdWriteFile"; ctx: WireCtx; fd: number }
	/** Bytes in `parts[0]`. */
	| { op: "fdAppend"; ctx: WireCtx; fd: number }
	| { op: "fdStat"; ctx: WireCtx; fd: number }
	| { op: "fdTruncate"; ctx: WireCtx; fd: number; length: number }
	| { op: "fdSync"; ctx: WireCtx; fd: number }
	| {
			op: "fdUtimes";
			ctx: WireCtx;
			fd: number;
			atimeMs: number;
			mtimeMs: number;
	  }
	/**
	 * write + sync + close, as one op.
	 *
	 * `Utf8Stream.flushSync` did exactly that sequence and paid five blocking round trips for
	 * it; a logger flushing per line felt every one.
	 */
	| { op: "fdFlushWrite"; ctx: WireCtx; fd: number };

export type VfsOpName = VfsCall["op"];

/** What each op answers. Ops that return bytes carry them in `parts`, not here. */
export interface VfsResults {
	probe: { proto: number; sid: string };
	mounts: MountSnapshot[];
	stat: FsEntry;
	access: null;
	exists: boolean;
	statfs: { used: number; capacity: number };
	readdir: Listing;
	readFile: null;
	readRange: null;
	writeFile: null;
	append: null;
	mkdir: string | undefined;
	rm: null;
	rmrf: null;
	rename: null;
	copyFile: null;
	utimes: boolean;
	truncate: null;
	mkdtemp: string;
	cp: null;
	/** The stream itself is the attachment; this is only what the host knew about its size. */
	"fs.openRead": { size?: number } | null;

	// the fd family
	open: { fd: number };
	close: null;
	/** Bytes in `parts[0]`; the count is that part's length. */
	fdRead: null;
	/** One part per requested length, truncated at the first short read. */
	fdReadv: null;
	fdWrite: number;
	fdWritev: number;
	fdReadFile: null;
	fdWriteFile: null;
	fdAppend: null;
	fdStat: FsEntry;
	fdTruncate: null;
	fdSync: null;
	fdUtimes: boolean;
	fdFlushWrite: null;
}

export type VfsResult<K extends VfsOpName> = VfsResults[K];

/**
 * What the worker is told about a mount.
 *
 * Only what it can act on locally. `hasNativeRange` is the one that has to be honest
 * in both directions: a derived ranged read slices a whole-file read, so claiming a
 * native range that isn't one makes a positioned-read loop re-read the entire file per
 * chunk, while under-claiming merely buffers the file once.
 */
export interface MountSnapshot {
	root: string;
	/** Provider name; diagnostics only. */
	name: string;
	readOnly: boolean;
	createdMs: number;
	hasNativeRange: boolean;
	canStream: boolean;
	hasCopyFile: boolean;
	hasStatfs: boolean;
}

/**
 * Whether the synchronous transport actually works, and if not, why.
 *
 * A `reason` rather than a bare boolean because the failures are unrelated and the fix
 * differs for each — and because the alternative is reporting them all as `EIO`, which
 * is indistinguishable from a genuine I/O failure and sends people hunting the wrong
 * bug. Note that without sync fs there is no module resolution at all (the resolver is
 * synchronous end to end), so this is normally a startup error rather than a degraded
 * mode.
 */
export interface NodeFsCapabilities {
	sync: boolean;
	reason?:
		| "no-sw"
		| "insecure-context"
		| "sync-xhr-blocked"
		| "out-of-scope"
		| "blob-worker"
		| "proto-mismatch"
		| "probe-failed";
	/** Human-readable detail for the reason, when there is any. */
	detail?: string;
}

/** Everything the worker needs to reach the host filesystem, delivered with `init`. */
export interface VfsInit {
	/** Identifies this worker's session to the service worker and the host. */
	sid: string;
	proto: number;
	/**
	 * Absolute URL prefix the worker POSTs a frame to, derived by the page from the
	 * service worker's registration scope — never guessed by the worker. Absent when
	 * there is no service worker, which means no synchronous filesystem.
	 */
	syncPrefix?: string;
	/** ms a blocked synchronous op may wait before giving up with EIO. 0 disables. */
	timeoutMs: number;
	mounts: MountSnapshot[];
}

/** The filesystem's request header — {@link WireRequest} carrying a {@link VfsCall}. */
export type VfsRequest = WireRequest<VfsCall>;
