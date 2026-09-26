// The one error envelope, for every kind of message.
//
// `structuredClone` of an `Error` preserves `name`, `message`, `stack` and `cause`
// and **nothing else** — so `code`, `errno`, `syscall` and `path` are all silently
// dropped. That matters more here than anywhere: `catch (e) { if (e.code !== "ENOENT")
// throw e }` is the dominant idiom in this tree (../worker/node/fs/ops.ts,
// .../handle.ts, .../module/resolve.ts, .../node-core/internal-binding/modules.ts, …),
// and an error that arrives codeless turns every one of those into a rethrow.
//
// So an error is carried as an explicit envelope, and the message travels
// **verbatim**. Composing it is node's business, `formatFsMessage` is the one place
// that does it, and both sides call it — a host that re-rendered the string and a
// worker that re-rendered it again would drift the first time either changed.
//
// This lives under `src/wire/` rather than `src/vfs/` because it is not the
// filesystem's envelope any more: a `WireError` is how *any* kind reports a failure —
// process, stdio, peer, control — carried in-band on the reply so a thrown error can
// never discard the sidebands riding alongside it. The errno table comes with it
// because `toWireError` needs it to coerce an unrecognized throw; ../vfs/errno.ts
// keeps only what a *provider* throws (`VfsError`, `fsError`) and imports the rest
// from here.

/**
 * libuv's negative errno and node's bare message for each code we can produce.
 *
 * The negative numbers are what `err.errno` must be: node's own `ERR_FS_*` paths and
 * the `internal-binding/uv.ts` consumers read it, not just `code`.
 */
export const ERRNO: Record<string, { errno: number; message: string }> = {
	EPERM: { errno: -1, message: "operation not permitted" },
	ENOENT: { errno: -2, message: "no such file or directory" },
	EIO: { errno: -5, message: "i/o error" },
	EBADF: { errno: -9, message: "bad file descriptor" },
	EACCES: { errno: -13, message: "permission denied" },
	EBUSY: { errno: -16, message: "resource busy or locked" },
	EEXIST: { errno: -17, message: "file already exists" },
	EXDEV: { errno: -18, message: "cross-device link not permitted" },
	ENOTDIR: { errno: -20, message: "not a directory" },
	EISDIR: { errno: -21, message: "illegal operation on a directory" },
	EINVAL: { errno: -22, message: "invalid argument" },
	EMFILE: { errno: -24, message: "too many open files" },
	EAGAIN: { errno: -11, message: "resource temporarily unavailable" },
	EFBIG: { errno: -27, message: "file too large" },
	ENOSPC: { errno: -28, message: "no space left on device" },
	EROFS: { errno: -30, message: "read-only file system" },
	ENOSYS: { errno: -38, message: "function not implemented" },
	ENOTEMPTY: { errno: -39, message: "directory not empty" },
	ENOTSUP: { errno: -95, message: "operation not supported" },
	// Linux aliases ENOTSUP and EOPNOTSUPP to the same value, and so does libuv.
	EOPNOTSUPP: { errno: -95, message: "operation not supported" },
};

/**
 * The exact string node puts in `err.message`: `"ENOENT: no such file or directory,
 * stat '/x'"`.
 *
 * Both the worker's `createFsError` and the host's `fsError` route through here so
 * the two can never disagree, which is what lets `WireError.message` be trusted
 * verbatim instead of rebuilt on arrival.
 */
export function formatFsMessage(
	code: string,
	message: string,
	syscall?: string,
	path?: string
): string {
	return `${code}: ${message}${syscall ? `, ${syscall}` : ""}${path ? ` '${path}'` : ""}`;
}

/**
 * A failure on the wire.
 *
 * `code`/`errno` are absent for a failure that isn't an fs error at all — a
 * `TypeError` out of a provider, say — which is exactly why `name` is carried
 * separately: `err.name === "AbortError"` is checked by real callers
 * (../worker/node/fs/streams.ts, .../watch.ts synthesize that shape), so it has to
 * survive even when there is no errno to go with it.
 */
export interface WireError {
	/** `err.name`. "Error" for fs errors; "AbortError"/"TypeError" for the rest. */
	name: string;
	/** `err.message`, already composed. Never re-rendered on arrival. */
	message: string;
	code?: string;
	/** libuv negative errno. Derived from `code` when absent. */
	errno?: number;
	syscall?: string;
	path?: string;
	/** `rename`/`copyFile`'s second path. */
	dest?: string;
	/** The *host's* stack. Appended under the local one for debugging, never used as `err.stack`. */
	stack?: string;
}

export type FsErrnoException = Error & {
	code: string;
	errno: number;
	syscall?: string;
	path?: string;
	dest?: string;
};

/**
 * Whether something carries the fs-error brand, from any bundle.
 *
 * Structural rather than `instanceof` on purpose — see the note on `VfsError` in
 * ../vfs/errno.ts, which is what sets the brand.
 */
export function isVfsError(err: unknown): err is FsErrnoException {
	return (
		typeof err === "object" &&
		err !== null &&
		(err as { __nodeWorkerFsError?: unknown }).__nodeWorkerFsError === 1
	);
}

/**
 * Pack any thrown value for the wire.
 *
 * An unrecognized throw becomes **EIO** rather than travelling as an uncoded error.
 * That is deliberate: node's `fs` never throws without a `code`, so a caller doing
 * `if (e.code !== "ENOENT") throw e` would rethrow past a handler that should have
 * caught it, and a caller doing `if (e.code === "EEXIST")` would silently take the
 * wrong branch. `name` and `message` still come through, so the original failure is
 * legible even though its code is synthesized.
 */
export function toWireError(err: unknown, fallbackSyscall?: string): WireError {
	if (isVfsError(err)) {
		return {
			name: err.name || "Error",
			message: err.message,
			code: err.code,
			errno: err.errno,
			syscall: err.syscall ?? fallbackSyscall,
			path: err.path,
			dest: err.dest,
			stack: err.stack,
		};
	}

	// Anything with a code we recognize is treated as an fs error even without the
	// brand, so a provider that hand-rolls `Object.assign(new Error(...), {code})`
	// still round-trips.
	const e = err as Partial<FsErrnoException> & {
		name?: string;
		message?: string;
	};
	if (e && typeof e.code === "string" && ERRNO[e.code]) {
		return {
			name: e.name || "Error",
			message: e.message ?? formatFsMessage(e.code, ERRNO[e.code].message),
			code: e.code,
			errno: typeof e.errno === "number" ? e.errno : ERRNO[e.code].errno,
			syscall: e.syscall ?? fallbackSyscall,
			path: e.path,
			dest: e.dest,
			stack: e.stack,
		};
	}

	const name = (e && e.name) || "Error";
	const message = (e && e.message) || String(err);
	// An AbortError keeps its own message and stays uncoded — callers check `err.name`
	// for it, and inventing an errno would make it look like a disk failure.
	if (name === "AbortError") {
		return { name, message, stack: e && e.stack };
	}
	return {
		name,
		// EIO, *and* the original text.
		//
		// The code has to be EIO so the `if (e.code !== "ENOENT") throw e` idiom behaves,
		// but the message must keep saying what actually went wrong. Reporting a bare
		// "EIO: i/o error" — which this did at first — turns every unexpected failure into
		// the same unreadable line, with the real cause reachable only through a `stack`
		// that is not attached by default. That is a bad trade: nobody debugging a
		// `TypeError` out of a provider is helped by being told it was I/O.
		message: `${formatFsMessage("EIO", ERRNO.EIO.message, fallbackSyscall)} (${name}: ${message})`,
		code: "EIO",
		errno: ERRNO.EIO.errno,
		syscall: fallbackSyscall,
		stack: e && e.stack,
	};
}

/**
 * Rebuild a thrown error from its envelope, in the exact shape
 * `../worker/node/fs/util.ts`'s `createFsError` produces — message verbatim, and
 * `code`/`errno`/`syscall`/`path`/`dest` as own properties.
 *
 * `includeHostStack` is off by default: appending the host's stack is a debugging
 * aid, and a program that prints `err.stack` should not routinely see two.
 */
export function fromWireError(e: WireError, includeHostStack = false): Error {
	const err = new Error(e.message) as Error & Partial<FsErrnoException>;
	err.name = e.name || "Error";
	if (e.code) {
		err.code = e.code;
		err.errno = typeof e.errno === "number" ? e.errno : ERRNO[e.code]?.errno;
	}
	if (e.syscall) err.syscall = e.syscall;
	if (e.path) err.path = e.path;
	if (e.dest) err.dest = e.dest;
	if (includeHostStack && e.stack) {
		err.stack = `${err.stack}\n    --- host ---\n${e.stack}`;
	}
	return err;
}
