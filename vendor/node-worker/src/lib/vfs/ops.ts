// Operations that are compositions of other operations rather than backend primitives.
//
// These sit above the mount layer on purpose: `cp` between two different mounts has to
// become read-then-write, and `truncate` on any backend is read-modify-write because no
// backend here can write part of a file. Pushing them down into providers would mean every
// provider reimplementing them.
//
// They live *here*, on the host, rather than in the worker, and that is the whole reason
// every `fs.*Sync` call is now exactly one blocking round trip. As worker-side compositions
// each of these cost one round trip per step: `appendFileSync` was two, `truncateSync` two,
// `cpSync` of a tree O(entries), `mkdtempSync` one plus its mkdir. Composed next to the
// providers they are one each.
//
// The exception is `cp` with a filter, which cannot come here at all: the filter is a user
// callback living in the worker, so a filtered copy stays a worker-driven walk over the
// primitives. That is the same reason `fs.promises.cp` has always had its own
// implementation — a user callback in the middle of an operation, not anything about the
// transport.

import { fsError } from "../../vfs/errno";
import { join } from "../../vfs/path";
import type { WireCtx } from "../../vfs/entry";
import type { Facade } from "./facade";

/** Generates a 6-character random suffix for mkdtemp(), matching node's length. */
function randomTempSuffix(): string {
	const alphabet =
		"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
	let out = "";
	for (let i = 0; i < 6; i++) {
		out += alphabet[Math.floor(Math.random() * alphabet.length)];
	}
	return out;
}

function isMissing(err: unknown): boolean {
	const code = (err as NodeJS.ErrnoException).code;
	return code === "ENOENT" || code === "ENOTDIR";
}

/**
 * Existence probe. puterfs has no real permission bits — `Stats.mode` is a constant
 * `type | 0o777` — so R/W/X_OK can never fail and only F_OK is a real question, which is
 * what the stat answers.
 */
export async function exists(
	fs: Facade,
	ctx: WireCtx,
	path: string
): Promise<boolean> {
	try {
		await fs.stat(ctx, path);
		return true;
	} catch (err) {
		if (isMissing(err)) return false;
		throw err;
	}
}

export async function append(
	fs: Facade,
	ctx: WireCtx,
	path: string,
	data: Uint8Array
): Promise<void> {
	// Reading the existing bytes rather than text: appending is a byte operation, and
	// decoding then re-encoding would corrupt any file that isn't valid text in the
	// requested encoding.
	let old: Uint8Array;
	try {
		old = await fs.readFile(ctx, path);
	} catch (err) {
		// ONLY a missing file means "start from empty". A bare `catch` here would turn
		// every other failure — a 500, EACCES, EISDIR, an aborted read — into a silent
		// truncation: the append would proceed with an empty base and write back only the
		// new data, destroying the file it was supposed to extend.
		if (!isMissing(err)) throw err;
		old = new Uint8Array(0);
	}
	const out = new Uint8Array(old.length + data.length);
	out.set(old, 0);
	out.set(data, old.length);
	await fs.writeFile(ctx, path, out);
}

export async function truncate(
	fs: Facade,
	ctx: WireCtx,
	path: string,
	len: number
): Promise<void> {
	if (!Number.isInteger(len)) len = Math.trunc(len);
	if (len < 0) len = 0;

	// Truncating to zero needs no read at all — the old contents are being discarded. The
	// general path below would download the whole file first just to drop it.
	if (len === 0) {
		await fs.writeFile(ctx, path, new Uint8Array(0));
		return;
	}

	const buf = await fs.readFile(ctx, path);
	let out: Uint8Array;
	if (len <= buf.length) {
		out = buf.subarray(0, len);
	} else {
		// Growing a file zero-fills, as ftruncate(2) does.
		out = new Uint8Array(len);
		out.set(buf, 0);
	}
	await fs.writeFile(ctx, path, out);
}

export async function mkdtemp(
	fs: Facade,
	ctx: WireCtx,
	prefix: string
): Promise<string> {
	const path = prefix + randomTempSuffix();
	await fs.mkdir({ ...ctx, syscall: "mkdir" }, path, { recursive: false });
	return path;
}

export async function mkdirp(
	fs: Facade,
	ctx: WireCtx,
	path: string
): Promise<void> {
	await fs.mkdir(ctx, path, { recursive: true });
}

export async function rmrf(
	fs: Facade,
	ctx: WireCtx,
	path: string,
	force: boolean
): Promise<void> {
	await fs.rm(ctx, path, { recursive: true, force });
}

export interface CpOptions {
	force?: boolean;
	errorOnExist?: boolean;
	recursive?: boolean;
}

/**
 * Recursive copy, without a filter — see the note at the top of this file for why a
 * filtered copy cannot be composed here.
 */
export async function cp(
	fs: Facade,
	ctx: WireCtx,
	source: string,
	destination: string,
	opts: CpOptions
): Promise<void> {
	const force = opts.force !== false;
	const errorOnExist = opts.errorOnExist || false;
	const recursive = opts.recursive || false;

	async function copyEntry(src: string, dest: string): Promise<void> {
		const srcStat = await fs.stat({ syscall: "stat", reportPath: src }, src);
		if (srcStat.isDir) {
			if (!recursive) {
				throw fsError("EISDIR", {
					syscall: "cp",
					path: src,
					message: "recursive option not enabled, cannot copy a directory",
				});
			}
			await fs.mkdir({ syscall: "mkdir", reportPath: dest }, dest, {
				recursive: true,
			});
			const listing = await fs.readdir(
				{ syscall: "scandir", reportPath: src },
				src
			);
			for (const entry of listing.entries) {
				await copyEntry(join(src, entry.name), join(dest, entry.name));
			}
			return;
		}

		if (await exists(fs, { syscall: "stat", reportPath: dest }, dest)) {
			if (errorOnExist) throw fsError("EEXIST", { syscall: "cp", path: dest });
			if (!force) return;
		}
		await fs.copyFile({ syscall: "copyfile", reportPath: src }, src, dest, {
			overwrite: true,
		});
	}

	await copyEntry(source, destination);
}

/**
 * `utimes`, with the validation a backend that cannot represent the request still owes.
 *
 * A provider returning `false` means "I could not apply this", which is a normal answer —
 * puterfs can only set a timestamp to *now*, and a `FileSystemFileHandle` cannot set one at
 * all. But node still reports ENOENT for a missing path, so when nothing was applied the
 * path has to be validated some other way, and a stat is that way.
 */
export async function utimes(
	fs: Facade,
	ctx: WireCtx,
	path: string,
	atimeMs: number,
	mtimeMs: number
): Promise<boolean> {
	const applied = await fs.utimes(ctx, path, atimeMs, mtimeMs);
	if (!applied) await fs.stat({ syscall: "stat", reportPath: path }, path);
	return applied;
}
