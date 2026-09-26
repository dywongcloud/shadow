// What a *provider* throws.
//
// The envelope these errors cross the boundary in — `WireError`, `toWireError`,
// `fromWireError`, the errno table and `formatFsMessage` — moved to ../wire/error.ts,
// because it stopped being the filesystem's: every kind of message reports failure
// that way now. What is left here is the vocabulary a provider writes against, which
// is genuinely filesystem-specific and which ../lib/vfs/* imports by the dozen.

import { ERRNO, formatFsMessage, type FsErrnoException } from "../wire/error";

// Re-exported so a provider needs only this module: `fsError` and the errno table it
// is named after belong together at the call site, wherever the table itself lives.
export {
	ERRNO,
	formatFsMessage,
	isVfsError,
	toWireError,
	fromWireError,
	type WireError,
	type FsErrnoException,
} from "../wire/error";

/**
 * What a provider throws.
 *
 * Deliberately **branded** rather than identified by `instanceof`: a third-party
 * provider bundled separately gets its own copy of this class, and an `instanceof`
 * check would fail on it — silently downgrading its ENOENTs to EIO, which is the
 * one mistranslation that breaks every `if (e.code !== "ENOENT")` above it.
 */
export class VfsError extends Error implements FsErrnoException {
	/** The brand. Structural, so a separately-bundled copy is still recognized. */
	readonly __nodeWorkerFsError = 1 as const;
	readonly code: string;
	readonly errno: number;
	readonly syscall?: string;
	readonly path?: string;
	readonly dest?: string;

	constructor(
		code: string,
		opts: {
			syscall?: string;
			path?: string;
			dest?: string;
			/** Overrides the table's bare message; still formatted by `formatFsMessage`. */
			message?: string;
		} = {}
	) {
		const known = ERRNO[code];
		const bare = opts.message ?? known?.message ?? "i/o error";
		super(formatFsMessage(code, bare, opts.syscall, opts.path));
		this.name = "Error";
		this.code = code;
		this.errno = known?.errno ?? ERRNO.EIO.errno;
		if (opts.syscall) this.syscall = opts.syscall;
		if (opts.path) this.path = opts.path;
		if (opts.dest) this.dest = opts.dest;
	}
}

/**
 * The idiom a provider uses: `throw fsError("ENOENT", ctx)`.
 *
 * Takes a {@link WireCtx}-shaped object so the common case is one argument — the
 * context the operation was already handed.
 */
export function fsError(
	code: string,
	ctx?: {
		syscall?: string;
		reportPath?: string;
		path?: string;
		dest?: string;
		message?: string;
	}
): VfsError {
	return new VfsError(code, {
		syscall: ctx?.syscall,
		path: ctx?.path ?? ctx?.reportPath,
		dest: ctx?.dest,
		message: ctx?.message,
	});
}
