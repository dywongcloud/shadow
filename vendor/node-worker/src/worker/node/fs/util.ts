import nodeBuffer from "../buffer";
import nodePath from "../path";
import { CWD } from "../../state";
import { ERRNO, formatFsMessage } from "../../../wire/error";

// The entry shape is shared with the host and the service worker now (see
// ../../../vfs/entry.ts), but it is re-exported from here because ~15 modules import
// it as `from "./util"` and the indirection is free.
import type { FsEntry } from "../../../vfs/entry";
export type { FsEntry };

let Buffer = nodeBuffer.Buffer;

type NodeFs = typeof import("node:fs");

// A stat result that satisfies both node's number (`Stats`) and bigint
// (`BigIntStats`) shapes. Our `Stats` class produces one or the other at
// runtime depending on the `bigint` flag; casting the construction to this
// intersection lets `stat`/`statSync` be assignable to node's overloaded
// signatures (whose `bigint: true` branch returns `BigIntStats`). Callers still
// get the precise per-overload type through the public `fs` typing.
export type AnyStats = import("node:fs").Stats & import("node:fs").BigIntStats;

export let fsConstants: NodeFs["constants"] = {
	O_RDONLY: 0,
	O_WRONLY: 1,
	O_RDWR: 2,
	S_IFMT: 61440,
	S_IFREG: 32768,
	S_IFDIR: 16384,
	S_IFCHR: 8192,
	S_IFBLK: 24576,
	S_IFIFO: 4096,
	S_IFLNK: 40960,
	S_IFSOCK: 49152,
	O_CREAT: 64,
	O_EXCL: 128,
	UV_FS_O_FILEMAP: 0,
	O_NOCTTY: 256,
	O_TRUNC: 512,
	O_APPEND: 1024,
	O_DIRECTORY: 65536,
	O_NOATIME: 262144,
	O_NOFOLLOW: 131072,
	O_SYMLINK: 2097152, // macos only
	O_SYNC: 1052672,
	O_DSYNC: 4096,
	O_DIRECT: 16384,
	O_NONBLOCK: 2048,
	S_IRWXU: 448,
	S_IRUSR: 256,
	S_IWUSR: 128,
	S_IXUSR: 64,
	S_IRWXG: 56,
	S_IRGRP: 32,
	S_IWGRP: 16,
	S_IXGRP: 8,
	S_IRWXO: 7,
	S_IROTH: 4,
	S_IWOTH: 2,
	S_IXOTH: 1,
	F_OK: 0,
	R_OK: 4,
	W_OK: 2,
	X_OK: 1,
	COPYFILE_EXCL: 1,
	COPYFILE_FICLONE: 2,
	COPYFILE_FICLONE_FORCE: 4,
};

export let bigintDivideAway = (a: bigint, b: bigint) =>
	a / b + (a % b === 0n ? 0n : a > 0n === b > 0n ? 1n : -1n);

// puterfs timestamps are unix *seconds*. Missing/garbage becomes 0 (the epoch)
// rather than NaN: node's stat never yields an Invalid Date, and a NaN here
// silently poisons every `mtime` comparison downstream.
function toMs(v: unknown): number {
	let n = Number(v);
	return Number.isFinite(n) ? n * 1000 : 0;
}

// Accepts either wire shape:
//   - v2 camelCase, from `/fs/readdir` (`isDir`, `modified`, ...)
//   - v1 snake_case, from the legacy `/stat` and `/readdir` routes (`is_dir`,
//     and `is_symlink` as an int 0|1)
// Neither has ever had `created_at`/`updated_at`, despite what this runtime used
// to read — the fields are `created`/`modified`/`accessed`.
export function normalizeFsEntry(raw: any): FsEntry {
	return {
		path: raw.path,
		name: raw.name,
		uid: raw.uid ?? raw.uuid ?? raw.id,
		isDir: Boolean(raw.isDir ?? raw.is_dir),
		isSymlink: Boolean(raw.isSymlink ?? raw.is_symlink),
		size: Number(raw.size ?? 0),
		modifiedMs: toMs(raw.modified),
		createdMs: toMs(raw.created),
		accessedMs: toMs(raw.accessed),
	};
}

// The request body every `stat` call sends.
//
// `return_size` is deliberately absent. It only does anything for directories,
// where the backend answers it with `SUM(size)` over the entire subtree — an
// O(descendants) index scan — so a single `statSync` on a project root makes the
// server walk all of node_modules. Node reports a directory's `Stats.size` as a
// block count, never a subtree total, so the field was never usable anyway.
export function statRequest(path: string) {
	return {
		path,
		return_permissions: false,
		return_versions: false,
		consistency: "strong",
	};
}

// Monotonic per-request token for the `_` query parameter on cacheable GETs.
// One counter for the whole fs layer; it only has to make a URL unique, not be
// unguessable or ordered across endpoints.
let cacheBuster = 0;

export function cacheBust(): string {
	return String(cacheBuster++);
}

// The url every file read GETs, cache-busted.
//
// The api answers `/read` with `ETag` and `Last-Modified` but *no*
// `Cache-Control`, which is precisely the case where a browser is allowed to
// invent its own freshness lifetime (RFC 9111 heuristic caching, in practice a
// fraction of the Last-Modified age) and serve the body out of the disk cache
// without revalidating. The bytes on disk then outlive the file: rewrite it and
// the next read still returns the old version, which is what breaks HMR — the
// dev server is told the file changed and reads back its previous contents.
//
// `_` is inert on the server: the legacy `/read` handler dispatches on `file`
// alone and ignores every other query parameter (see the backend's
// LegacyFSController `read`). This stays a CORS-simple GET, so it costs no
// preflight — unlike a `Cache-Control: no-cache` request header, which would.
export function readUrl(path: string): string {
	return `read?file=${encodeURIComponent(path)}&_=${cacheBust()}`;
}

// Maps Puter API error codes to Node.js fs errno codes.
// Puter error codes are defined in the backend at src/backend/src/api/APIError.js.
//
// Only the node code is recorded: the errno and the bare message come from `ERRNO` in
// ../../../vfs/errno.ts, which is shared with the host and the service worker. This
// table used to spell all three out per entry, which meant 28 hand-maintained copies of
// facts that already existed elsewhere — and the errno is the half that matters and the
// half nobody checks, since node's own `ERR_FS_*` paths read `err.errno` rather than
// `err.code`.
let puterErrorToNodeError: Record<string, string> = {
	// Not found
	subject_does_not_exist: "ENOENT",
	source_does_not_exist: "ENOENT",
	dest_does_not_exist: "ENOENT",
	shortcut_target_not_found: "ENOENT",
	offset_without_existing_file: "ENOENT",

	// Already exists
	item_with_same_name_exists: "EEXIST",

	// Permission
	forbidden: "EACCES",
	permission_denied: "EACCES",
	immutable: "EACCES",

	// Directory not empty
	not_empty: "ENOTEMPTY",

	// Not a directory / is a directory
	dest_is_not_a_directory: "ENOTDIR",
	readdir_of_non_directory: "ENOTDIR",
	cannot_read_a_directory: "EISDIR",
	cannot_overwrite_a_directory: "EISDIR",

	// Invalid argument
	invalid_file_name: "EINVAL",
	unresolved_relative_path: "EINVAL",
	invalid_operation: "EINVAL",
	// The api's catch-all for a malformed request. Reachable from normal code: a
	// recursive readdir of `/` returns it (see readdir-recursive.ts).
	bad_request: "EINVAL",
	// Self-referential operations
	cannot_move_item_into_itself: "EINVAL",
	cannot_copy_item_into_itself: "EINVAL",
	source_and_dest_are_the_same: "EINVAL",

	// Cannot write/move/copy to root
	cannot_move_to_root: "EPERM",
	cannot_copy_to_root: "EPERM",
	cannot_write_to_root: "EPERM",

	// Storage
	storage_limit_reached: "ENOSPC",
	file_too_large: "EFBIG",

	// Not supported
	not_yet_supported: "ENOTSUP",
	missing_filesystem_capability: "ENOTSUP",
};

// Translates a Puter API error code string into a Node.js-style fs error object.
// Returns undefined if the error code is not recognized, in which case the caller
// should fall back to a generic Error.
export function translatePuterError(
	puterCode: string,
	syscall?: string,
	path?: string
): (NodeJS.ErrnoException & { code: string; errno: number }) | undefined {
	let code = puterErrorToNodeError[puterCode];
	if (!code) return undefined;
	let mapping = ERRNO[code];
	if (!mapping) return undefined;

	let err = new Error(
		formatFsMessage(code, mapping.message, syscall, path)
	) as NodeJS.ErrnoException & { code: string; errno: number };
	err.code = code;
	err.errno = mapping.errno;
	if (syscall) err.syscall = syscall;
	if (path) err.path = path;
	return err;
}

// Coerces a path-like value (string, Buffer, or URL) to a string.
// Follows Node.js fs conventions:
// - string: returned as-is
// - Buffer: decoded as UTF-8
// - URL: must have 'file:' protocol; pathname is extracted and decoded
// - number (file descriptor): throws, as Puter does not support file descriptors
// Throws TypeError for invalid inputs, matching Node.js behavior.
export function toPathString(path: string | Buffer | URL | number): string {
	if (typeof path === "string") return path;
	if (typeof path === "number")
		throw new TypeError("File descriptors are not supported");
	if (path instanceof Buffer) return path.toString("utf8");
	if (path instanceof URL) {
		if (path.protocol !== "file:")
			throw new TypeError(
				`The URL must be of scheme file, received ${path.protocol}`
			);
		// Decode percent-encoded characters in the pathname
		return decodeURIComponent(path.pathname);
	}
	throw new TypeError(
		'The "path" argument must be of type string, Buffer, or URL'
	);
}

// Absolute, canonical, and free of `.` / `..` / `//`.
//
// This used to return an already-absolute path *verbatim*, normalizing only
// relative ones. That is fine while one backend serves everything and merely
// forwards whatever it is given, but it breaks the moment a path has to be matched
// against a mount, in two ways that both fail silently:
//
//   - `/p/node_modules/../node_modules/lodash` does not prefix-match a
//     `/p/node_modules` mount, so it is routed to the wrong backend — wrong bytes
//     or a spurious ENOENT.
//   - `/tmp/../u/secret` *does* match the `/tmp` mount, handing its provider a
//     local path of `/../u/secret`. puterfs rejects `..` and so fails safe, but an
//     in-memory tree would happily create a node literally named "..". Providers
//     must never see one.
//
// `path.resolve` collapses all three, and `resolve("/", "../x")` is `/x`, so no
// path can escape the root — which is the containment guarantee the mount layer
// relies on.
//
// The leading "/" is load-bearing rather than decorative. `path.resolve` falls back
// to `process.cwd()` when its accumulated result isn't absolute, and `process.cwd()`
// returns `CWD` — so a relative `CWD` (reachable through `process.chdir`, which
// forwards unvalidated) would otherwise produce a relative answer. Anchoring here
// means `CWD` can be anything and the result is still absolute, which is why
// `state.ts` gets to stay a leaf module with no imports of its own.
export function normalizePath(path: string | Buffer | URL | number): string {
	return nodePath.resolve("/", CWD, toPathString(path));
}

// Builds a Node-style fs error (code/errno/syscall/path) for cases where there
// is no Puter API error to translate (bad flags, bad fd, validation, ...).
//
// The message is composed by `formatFsMessage` rather than inline, because the host
// side of the filesystem composes the same string for errors it sends over the wire
// (see ../../../vfs/errno.ts). Two copies of that template would drift the first time
// either changed, and the wire carries the message *verbatim* — so a drift would show
// up as errors that read differently depending on which side produced them.
export function createFsError(
	code: string,
	errno: number,
	message: string,
	syscall: string,
	path?: string
): NodeJS.ErrnoException & { code: string; errno: number } {
	const err = new Error(
		formatFsMessage(code, message, syscall, path)
	) as NodeJS.ErrnoException & { code: string; errno: number };
	err.code = code;
	err.errno = errno;
	err.syscall = syscall;
	if (path) err.path = path;
	return err;
}

// Moved to ../../../vfs/flags.ts, because the *host* is what opens files now and has to agree
// with this exactly. Re-exported so the ~4 modules that import it from here do not change.
export { parseOpenFlags, type OpenFlags } from "../../../vfs/flags";

// Coerces one of node's time arguments (`utimes`, `futimes`, ...) to epoch
// milliseconds, following node's own `toUnixTimestamp` rules: a number or
// numeric string is *seconds*, a Date is used directly, and NaN/Infinity mean
// "now" (which is how `touch(1)`-style callers spell it).
export function toEpochMs(time: unknown, syscall: string): number {
	if (typeof time === "string" && +time == (time as any)) time = +time;
	if (typeof time === "number") {
		if (!Number.isFinite(time)) return Date.now();
		if (time < 0) return Date.now();
		return time * 1000;
	}
	if (time instanceof Date) return time.getTime();
	throw createFsError(
		"EINVAL",
		-22,
		"invalid time value",
		syscall
	) as unknown as never;
}

// The api can only set a timestamp to *now* (`POST /touch` takes
// `set_modified_to_now` and friends — there is no field for an arbitrary value),
// so this decides whether a requested time is close enough to now to be worth a
// round trip. Two seconds covers the gap between a caller reading the clock and
// us issuing the request.
export const TOUCH_NOW_TOLERANCE_MS = 2000;

export function isEffectivelyNow(epochMs: number): boolean {
	return Math.abs(Date.now() - epochMs) <= TOUCH_NOW_TOLERANCE_MS;
}

// Coerces one of node's write payloads to the bytes to send.
//
// The typed-array branch honors `byteOffset`/`byteLength`. The inline versions this
// replaces used `Buffer.from(data.buffer)`, which discards both — so writing a view
// into a larger ArrayBuffer (`new Uint8Array(big, 100, 10)`, which is what every
// pooled or sliced buffer looks like) wrote the *entire* backing buffer instead of
// the ten bytes asked for.
export function toWriteBuffer(
	data: unknown,
	encoding?: BufferEncoding | null
): Buffer {
	if (typeof data === "string") {
		return Buffer.from(data, encoding || undefined);
	}
	if (Buffer.isBuffer(data)) return data;
	if (ArrayBuffer.isView(data)) {
		return Buffer.from(data.buffer, data.byteOffset, data.byteLength);
	}
	if (data instanceof ArrayBuffer) return Buffer.from(data);
	throw createFsError("EINVAL", -22, "invalid argument", "write");
}

// Generates a 6-character random suffix for mkdtemp(), matching Node's length.
export function randomTempSuffix(): string {
	const alphabet =
		"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
	let out = "";
	for (let i = 0; i < 6; i++)
		out += alphabet[Math.floor(Math.random() * alphabet.length)];
	return out;
}
