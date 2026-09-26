// The synchronous half of the node:fs surface.
//
// Every method here is argument handling — node's overload sets, option coercion, CWD joining,
// encoding, `bigint`, building `Stats`/`Dirent` — wrapped around exactly one blocking call to
// the host filesystem. The filesystem semantics are not here and not in this bundle: they live
// beside the providers (src/lib/vfs/), which is what lets a backend await something.
//
// ./promises.ts is the same argument handling around `hostAsync` instead of `host`. That
// duplication is node's overload sets, not filesystem behaviour — the logic those two files
// used to share through a generator protocol is now shared by *being in one place on the host*.
//
// The rule for reading this file: if something here talks about paths, options, or
// node's classes it belongs here; if it talks about requests or filesystem behavior
// it belongs below and this is a bug.

import nodeBuffer from "../buffer";
import {
	createFsError,
	fsConstants,
	normalizePath,
	toWriteBuffer,
	type AnyStats,
} from "./util";
import { encodeEntry } from "./readdir-encode";
import { ctx, host } from "./host";
import { toEpochMs } from "./util";
import { Stats, StatsFs, Dirent, Dir } from "./classes";
import { FileHandle } from "./handle";
import { fdTable } from "./fd-table";
import { stdioSync } from "./transport";
import { isRawMode } from "../../console";
import { writeStdio } from "../../stdio";
// Type-only: these are used solely in the `Omit` below. A runtime import would
// put ./sync.ts back inside the glob module-init cycle (see ./glob.ts).
import type { promisesToDepromisify } from "./promises";
import type { promisesRemaining } from "./promises-sync";

type NodeFs = typeof import("node:fs");

/**
 * `cp` with a caller-supplied filter.
 *
 * Everything else about `cp` is one host operation, but a filter is a callback living in this
 * worker, so the walk has to happen here and pay a round trip per entry. That is the same
 * reason `fs.promises.cp` has always had its own implementation — a user callback in the middle
 * of an operation, not anything about the transport.
 */
function cpWalkSync(
	fs: any,
	src: string,
	dest: string,
	o: { recursive: boolean; force: boolean; errorOnExist: boolean },
	filter: (src: string, dest: string) => boolean
) {
	if (!filter(src, dest)) return;
	let st = host.stat(ctx("stat", src), src);
	if (st.isDir) {
		if (!o.recursive) {
			throw createFsError(
				"EISDIR",
				-21,
				"recursive option not enabled, cannot copy a directory",
				"cp",
				src
			);
		}
		host.mkdir(ctx("mkdir", dest), dest, true);
		for (let entry of host.readdir(ctx("scandir", src), src).entries) {
			cpWalkSync(
				fs,
				`${src}/${entry.name}`,
				`${dest}/${entry.name}`,
				o,
				filter
			);
		}
		return;
	}
	if (host.exists(ctx("stat", dest), dest)) {
		if (o.errorOnExist) {
			throw createFsError("EEXIST", -17, "file already exists", "cp", dest);
		}
		if (!o.force) return;
	}
	host.copyFile(ctx("copyfile", src), src, dest, true);
}

let Buffer = nodeBuffer.Buffer;

// Looks up an open fd. There is one handle class and one table, so an fd from
// `open` and one from `openSync` are equally valid here — which is node's
// behavior, and a change from the two-class split this replaced.
function getHandle(fd: number, syscall: string): FileHandle {
	const handle = fdTable.get(fd);
	if (!(handle instanceof FileHandle))
		throw createFsError("EBADF", -9, "bad file descriptor", syscall);
	return handle;
}

/**
 * 0, 1 and 2 — the process's own stdio, which is not a file and has no handle.
 *
 * `fd-table.ts` has always reserved these ("leaving room for stdio") and nothing ever
 * filled them, so every `readSync(0, …)` and `writeSync(1, …)` came back EBADF. That is
 * what breaks `readline-sync`, `prompt-sync`, and `readFileSync(0)` — the ordinary way a
 * CLI reads piped input.
 */
const STDIN_FD = 0;
function isStdioFd(fd: unknown): fd is 0 | 1 | 2 {
	return fd === 0 || fd === 1 || fd === 2;
}

/**
 * `/dev/stdin`, `/dev/stdout`, `/dev/stderr`, `/dev/tty` — the process's own stdio, by path.
 *
 * These live here rather than in the host's `/dev` provider (src/lib/vfs/dev.ts, which has
 * `null`, `zero` and `full`) for a reason that is not convenience: a `NodeVfs` can be shared
 * by several workers, and stdio is per-process. A shared mount could only ever offer *one*
 * `/dev/stdout`, which would be the wrong worker's. Real `/dev/stdout` is per-process too.
 *
 * `/dev/tty` maps to stdin for reading and stdout for writing, which is what a program
 * opening it wants — the controlling terminal, not a third stream.
 */
function stdioPath(path: unknown): 0 | 1 | 2 | undefined {
	if (typeof path !== "string") return undefined;
	switch (path) {
		case "/dev/stdin":
			return 0;
		case "/dev/stdout":
			return 1;
		case "/dev/stderr":
			return 2;
		// Reading it is stdin; a caller that writes gets stdout via `writeSync`'s own mapping.
		case "/dev/tty":
			return 0;
		default:
			return undefined;
	}
}

/**
 * One read of stdin, into `buffer`. Returns bytes read; 0 means end of input.
 *
 * Blocking, *except* in raw mode, where it raises `EAGAIN` when there is nothing to read —
 * which is what node does and what a program in raw mode is written against. A raw-mode program
 * drives its own input loop and drains with `readSync` until it is told there is no more; give
 * it a blocking read instead and the drain never ends.
 *
 * That is not a hypothetical: a terminal program shutting down drains stdin, nobody is typing,
 * and the read waits. Here waiting means a *synchronous transport request* held open — so it
 * does not merely stall, it eventually fails the whole request with a transport timeout, out of
 * a teardown path that was never written to handle one. The program is then neither running nor
 * exited, which is a worse state than either.
 */
function readStdinSync(
	buffer: ArrayBufferView,
	offset: number,
	length: number
): number {
	if (length <= 0) return 0;
	const raw = isRawMode();
	const answer = stdioSync({
		op: "io.read",
		fd: 0,
		length,
		blocking: !raw,
	});
	const bytes = answer.parts[0];
	if (!bytes?.length) {
		// Nothing now is not the same as nothing ever: only the end of input is 0 bytes.
		if (raw && !answer.value?.eof) {
			throw createFsError(
				"EAGAIN",
				-11,
				"resource temporarily unavailable",
				"read"
			);
		}
		return 0;
	}
	const view = new Uint8Array(
		buffer.buffer,
		buffer.byteOffset,
		buffer.byteLength
	);
	view.set(bytes.subarray(0, length), offset);
	return Math.min(bytes.length, length);
}

/** Everything on stdin, to end of input. Backs `readFileSync(0)` and `/dev/stdin`. */
function readAllStdinSync(): Buffer {
	const chunks: Uint8Array[] = [];
	let total = 0;
	for (;;) {
		const answer = stdioSync({
			op: "io.read",
			fd: 0,
			length: 64 * 1024,
			blocking: true,
		});
		const bytes = answer.parts[0];
		if (bytes?.length) {
			chunks.push(new Uint8Array(bytes));
			total += bytes.length;
		}
		if (answer.value.eof) break;
	}
	const out = new Uint8Array(total);
	let at = 0;
	for (const chunk of chunks) {
		out.set(chunk, at);
		at += chunk.length;
	}
	return Buffer.from(out);
}

// puterfs has no symlinks and no path-based link api (only `/mkshortcut`, which
// targets a uid and can't dangle), so rather than emulate them badly these
// report the errno a filesystem without the feature would: tar, fs-extra and
// friends already have a fallback path for it. `readlink` distinguishes "exists
// but isn't a link" (EINVAL, node's own errno) from "isn't there" (ENOENT).
function noLinks(syscall: string, path: string): never {
	throw createFsError("EPERM", -1, "operation not permitted", syscall, path);
}

// Not a member of `fsSync` — see `realpathSync` below, which needs a `.native`
// pointing back at itself.
//
// puterfs resolves nothing — no symlinks, no shortcuts on the read path — so the
// real path is just the canonical path, and this stays free of I/O. It used to
// round-trip the *raw* argument through a Buffer without normalizing, so
// `realpath("/a/./b")` answered "/a/./b" — not a canonical path by any definition,
// and now actively harmful: callers key caches on what realpath returns, and the
// mount layer routes on canonical form, so handing back two spellings of one file
// invites them to disagree.
function realpathSyncImpl(path: any, options?: any) {
	if (typeof options == "string") options = { encoding: options };
	else if (!options) options = {};

	let resolved = normalizePath(path);
	if (options.encoding == "buffer") return Buffer.from(resolved, "utf8");
	return Buffer.from(resolved, "utf8").toString(options.encoding || "utf8");
}

// Type-level mask: declare exactly the sync surface we implement.
// Excluded keys (the async methods, classes, constants, and the watcher family,
// which lives in ./watch.ts) surface as missing-method warnings at the
// `satisfies typeof import("node:fs")` site in `./index.ts`, which is the right
// place to track them.
export let fsSync: Omit<
	NodeFs,
	// Containers and constants, assembled in ./index.ts
	| "promises"
	| "constants"
	| "Dir"
	| "Dirent"
	| "Stats"
	| "StatsFs"
	| "exists"
	// ./watch.ts
	| "watchFile"
	| "unwatchFile"
	// ./streams.ts
	| "createReadStream"
	| "createWriteStream"
	| "ReadStream"
	| "WriteStream"
	| "Utf8Stream"
	// ./glob.ts
	| "glob"
	| "globSync"
	// ./fd.ts — the callback-style numeric-fd family
	| "close"
	| "read"
	| "write"
	| "fstat"
	| "fsync"
	| "fdatasync"
	| "ftruncate"
	| "readv"
	| "writev"
	| "fchmod"
	| "fchown"
	| "futimes"
	// The promise impls, depromisified in ./index.ts
	| keyof typeof promisesToDepromisify
	| keyof typeof promisesRemaining
> = {
	appendFileSync(path, data, options) {
		if (typeof options === "string") options = { encoding: options };
		else if (!options) options = {};

		// Appending to a terminal is writing to it; there is no position to append at.
		// Written out rather than delegating through `this`, because these methods are
		// routinely destructured off the module and `this` would not survive it.
		const appendDev = stdioPath(path);
		const appendFd = isStdioFd(path) ? path : appendDev === 0 ? 1 : appendDev;
		if (appendFd !== undefined) {
			if (appendFd === STDIN_FD) {
				throw createFsError("EBADF", -9, "bad file descriptor", "write");
			}
			const buf = toWriteBuffer(data, options.encoding);
			writeStdio(
				appendFd,
				new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength)
			);
			return;
		}
		let p = normalizePath(path);
		host.append(ctx("open", p), p, toWriteBuffer(data, options.encoding));
	},
	copyFileSync(src, dest, mode) {
		let from = normalizePath(src);
		let to = normalizePath(dest);

		mode ??= 0;
		let overwrite = (mode & fsConstants.COPYFILE_EXCL) === 0;

		if (mode & fsConstants.COPYFILE_FICLONE_FORCE) {
			throw createFsError(
				"EOPNOTSUPP",
				-95,
				"operation not supported",
				"copyfile"
			);
		}

		host.copyFile(ctx("copyfile", from), from, to, overwrite);
	},
	existsSync(path) {
		// A stdio device is always there, and asking the filesystem about it would be asking
		// the wrong thing — it is this process's, not the mount table's.
		if (stdioPath(path) !== undefined) return true;
		// node's `existsSync` never throws — it answers false for *any* failure, not
		// just ENOENT. `existsPlan` is deliberately stricter than that (its other
		// callers want to hear about a 500 rather than silently treat it as "absent"),
		// so the swallowing belongs here, at the node boundary.
		try {
			return host.exists(ctx("stat", normalizePath(path)), normalizePath(path));
		} catch {
			return false;
		}
	},
	mkdirSync(path, options) {
		let p = normalizePath(path);

		if (typeof options === "number" || typeof options === "string")
			options = { mode: options };
		else if (!options) options = {};

		// mode is ignored: puterfs has no POSIX permission bits.
		let recursive = options.recursive || false;
		let first = host.mkdir(ctx("mkdir", p), p, recursive);
		// node's recursive mkdir returns the first directory it created, or undefined.
		// The api doesn't reliably report it, so this is undefined more often than on
		// a real filesystem.
		return recursive ? first : undefined;
	},
	opendirSync(path, options?) {
		path = normalizePath(path);

		let entries = this.readdirSync(path, {
			withFileTypes: true,
			recursive: options?.recursive,
			encoding: options?.encoding,
		}) as InstanceType<typeof Dirent>[];
		return new Dir(path, entries);
	},
	readdirSync(path, options?) {
		let p = normalizePath(path);

		if (typeof options === "string") options = { encoding: options } as {};
		else if (!options) options = {};

		// Same plan as the async twin; the only difference is which driver runs it.
		let listing = host.readdir(ctx("scandir", p), p, {
			recursive: options.recursive,
		});
		return listing.entries.map((entry) => encodeEntry(entry, p, options));
	},
	readFileSync(path, options) {
		// `readFileSync(0)` and `readFileSync("/dev/stdin")` are the ordinary way a CLI reads
		// piped input, and both mean "everything on stdin". Handled before `normalizePath`,
		// which has no idea what to do with a file descriptor.
		if (path === STDIN_FD || stdioPath(path) === STDIN_FD) {
			let all = readAllStdinSync();
			if (typeof options === "string") options = { encoding: options };
			return (options?.encoding ? all.toString(options.encoding) : all) as any;
		}
		let p = normalizePath(path);

		if (typeof options === "string") options = { encoding: options };
		else if (!options) options = {};

		// options.flag is accepted and ignored: puterfs has no open modes to honor.
		let buf = host.readFile(ctx("open", p), p);
		if (options.encoding)
			// not sure why ts doesn't like this
			return buf.toString(options.encoding) as any;
		else return buf;
	},
	renameSync(oldPath, newPath) {
		let from = normalizePath(oldPath);
		let to = normalizePath(newPath);
		host.rename(ctx("rename", from), from, to);
	},
	rmdirSync(path) {
		return this.unlinkSync(path);
	},
	rmSync(path, options) {
		// TODO retries?
		let p = normalizePath(path);
		if (!options) options = {};

		host.rm(
			ctx("rm", p),
			p,
			options.recursive || false,
			options.force || false
		);
	},
	statSync(path, options?) {
		let p = normalizePath(path);
		if (!options) options = {};

		let entry = host.stat(ctx("stat", p), p);
		return new Stats(entry, options.bigint || false) as AnyStats;
	},
	// puter fs has no symlinks, so lstat is just stat.
	lstatSync(path, options?) {
		return this.statSync(path, options as any) as AnyStats;
	},
	statfsSync(path, options?) {
		if (!options) options = {};

		// The path selects which backend answers, but its capacity report covers the
		// whole of that backend rather than the subtree — as `statfs(2)` does.
		let p = normalizePath(path);
		let df = host.statfs(ctx("statfs", p), p);
		return new StatsFs(df, options.bigint || false);
	},
	writeFileSync(file, data, options) {
		if (typeof options === "string") options = { encoding: options };
		else if (!options) options = {};

		// `writeFileSync(1, …)` and `writeFileSync("/dev/stdout", …)` are both "print this",
		// and `/dev/tty` means the terminal — which for a write is stdout, not stdin.
		const dev = stdioPath(file);
		const fd = isStdioFd(file) ? file : dev === 0 ? 1 : dev;
		if (fd !== undefined) {
			if (fd === STDIN_FD) {
				throw createFsError("EBADF", -9, "bad file descriptor", "write");
			}
			const buf = toWriteBuffer(data, options.encoding);
			writeStdio(
				fd,
				new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength)
			);
			return;
		}
		let p = normalizePath(file);

		// options.flag is accepted and ignored: puterfs has no open modes to honor.
		let buf = toWriteBuffer(data, options.encoding);
		host.writeFile(ctx("write", p), p, buf);
	},
	unlinkSync(path) {
		let p = normalizePath(path);
		host.rm(ctx("unlink", p), p, false, false);
	},
	// puterfs resolves nothing — no symlinks, no shortcuts on the read path — so
	// the real path is the path. `.native` is the same impl, as in node on a
	// filesystem with nothing to resolve.
	realpathSync: Object.assign(realpathSyncImpl, {
		native: realpathSyncImpl,
	}) as NodeFs["realpathSync"],
	// Existence + permission probe. puterfs has no real permission bits, so only
	// F_OK can fail (surfaced as ENOENT by the stat).
	accessSync(path, _mode?) {
		host.access(ctx("access", normalizePath(path)), normalizePath(path));
	},
	truncateSync(path, len?) {
		let p = normalizePath(path);
		host.truncate(ctx("open", p), p, len ?? 0);
	},
	cpSync(source, destination, opts?) {
		let options = (opts || {}) as any;
		let from = normalizePath(source as any);
		let to = normalizePath(destination as any);
		let o = {
			recursive: !!options.recursive,
			force: options.force !== false,
			errorOnExist: !!options.errorOnExist,
		};
		// Unfiltered: one host op for the whole tree, where this used to be a round trip per
		// entry. With a filter it cannot be, because the filter is a callback living in this
		// worker — so that case keeps walking, exactly as `promises.cp` always has.
		if (!options.filter) {
			host.cp(ctx("cp", from), from, to, o);
			return;
		}
		cpWalkSync(this, from, to, o, options.filter);
	},
	mkdtempSync(prefix, options?) {
		if (typeof options === "string") options = { encoding: options };
		else if (!options) options = {};

		let path = host.mkdtemp(
			ctx("mkdtemp", normalizePath(prefix as any)),
			normalizePath(prefix as any)
		);

		let nameBuf = Buffer.from(path, "utf8");
		if ((options as any).encoding === "buffer") return nameBuf as any;
		return nameBuf.toString((options as any).encoding || undefined) as any;
	},
	mkdtempDisposableSync(prefix, options?) {
		let path = this.mkdtempSync(prefix, options as any) as string;
		let self = this;
		let removed = false;
		let remove = () => {
			if (removed) return;
			removed = true;
			self.rmSync(path, { recursive: true, force: true });
		};
		return {
			path,
			remove,
			[Symbol.dispose]: remove,
		} as any;
	},
	// See `noLinks` above for why these report EPERM rather than emulating.
	linkSync(_existingPath, newPath) {
		noLinks("link", normalizePath(newPath as any));
	},
	symlinkSync(_target, path, _type?) {
		noLinks("symlink", normalizePath(path as any));
	},
	readlinkSync(path, _options?) {
		// EINVAL is node's errno for readlink on something that isn't a link, so
		// the stat is load-bearing: it's what distinguishes that from ENOENT.
		let resolved = normalizePath(path as any);
		this.statSync(resolved);
		throw createFsError(
			"EINVAL",
			-22,
			"invalid argument",
			"readlink",
			resolved
		);
	},
	// The only timestamp api is `POST /touch`, whose fields are
	// `set_{modified,accessed,created}_to_now` — there is no way to set an
	// arbitrary value. So a request for ~now is honored for real, and anything
	// else validates the path and no-ops rather than throwing, matching how
	// chmod/chown already behave here.
	utimesSync(path, atime, mtime) {
		let p = normalizePath(path as any);
		// `false` means the backend couldn't represent the requested times (puterfs can
		// only set them to *now*), so nothing was sent — but a missing path still owes
		// the caller an ENOENT, which the stat provides.
		// The host validates the path itself, so a missing file still reports ENOENT even
		// when the backend cannot represent the requested times.
		host.utimes(
			ctx("utime", p),
			p,
			toEpochMs(atime, "utime"),
			toEpochMs(mtime, "utime")
		);
	},
	// Nothing can be a symlink, so there is no link to *not* follow.
	lutimesSync(path, atime, mtime) {
		this.utimesSync(path, atime, mtime);
	},
	futimesSync(fd, atime, mtime) {
		this.utimesSync(getHandle(fd, "futime").filePath, atime, mtime);
	},
	// puterfs has no mode/owner bits; validate existence then no-op.
	chmodSync(path, _mode) {
		this.statSync(path);
	},
	lchmodSync(path, _mode) {
		this.statSync(path);
	},
	chownSync(path, _uid, _gid) {
		this.statSync(path);
	},
	lchownSync(path, _uid, _gid) {
		this.statSync(path);
	},
	async openAsBlob(path, options?) {
		let buf = this.readFileSync(path) as Buffer;
		return new Blob([buf as unknown as BlobPart], {
			type: (options as any)?.type ?? "",
		});
	},
	// --- numeric fd family (sync) ---
	//
	// The same `FileHandle` the async family uses, driven with the blocking driver.
	// An fd opened here is therefore usable with `fs.read`, `fs.promises` and
	// `createReadStream({fd})`, which is node's behavior and which the previous
	// two-class split rejected with EBADF.
	openSync(path, flags?, _mode?) {
		// Opening one of the stdio device paths hands back the descriptor it names, so
		// `readSync`/`writeSync` on the result go straight to the process's own stdio. That
		// is what makes a shell's `> /dev/stdout` work through the ordinary open/write path
		// rather than needing a special case of its own.
		const stdio = stdioPath(path);
		if (stdio !== undefined) return stdio;
		return FileHandle.openSync(path as any, flags).fd;
	},
	closeSync(fd) {
		getHandle(fd, "close").closeSync();
	},
	readSync(fd, buffer, offsetOrOptions?: any, length?: any, position?: any) {
		if (isStdioFd(fd)) {
			if (fd !== STDIN_FD) {
				throw createFsError("EBADF", -9, "bad file descriptor", "read");
			}
			let off: number;
			let len: number;
			if (typeof offsetOrOptions === "object" && offsetOrOptions !== null) {
				off = offsetOrOptions.offset ?? 0;
				len = offsetOrOptions.length ?? buffer.byteLength - off;
			} else {
				off = offsetOrOptions ?? 0;
				len = length ?? buffer.byteLength - off;
			}
			return readStdinSync(buffer, off, len);
		}
		let handle = getHandle(fd, "read");
		let offset: number;
		let len: number;
		let pos: number | null;
		if (typeof offsetOrOptions === "object" && offsetOrOptions !== null) {
			offset = offsetOrOptions.offset ?? 0;
			len = offsetOrOptions.length ?? buffer.byteLength - offset;
			pos = offsetOrOptions.position ?? null;
		} else {
			offset = offsetOrOptions ?? 0;
			len = length ?? buffer.byteLength - offset;
			pos = position ?? null;
		}
		if (typeof pos === "bigint") pos = Number(pos);
		return handle.readSync(buffer, offset, len, pos);
	},
	writeSync(
		fd,
		data,
		offsetOrPositionOrOptions?: any,
		lengthOrEncoding?: any,
		position?: any
	) {
		if (isStdioFd(fd)) {
			if (fd === STDIN_FD) {
				throw createFsError("EBADF", -9, "bad file descriptor", "write");
			}
			let bytes =
				typeof data === "string"
					? toWriteBuffer(
							data,
							(typeof lengthOrEncoding === "string"
								? lengthOrEncoding
								: "utf8") as BufferEncoding
						)
					: toWriteBuffer(data);
			// Buffered, not sent. The bytes leave as a sideband on whatever message goes
			// next, which is what keeps them ahead of the very call that carries them —
			// there is no way for a synchronous function to await a flush, so ordering has
			// to come from the packing rather than from a barrier.
			return writeStdio(
				fd,
				new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength)
			);
		}
		let handle = getHandle(fd, "write");

		if (typeof data === "string") {
			// writeSync(fd, string, position?, encoding?)
			let pos =
				typeof offsetOrPositionOrOptions === "number"
					? offsetOrPositionOrOptions
					: null;
			let encoding =
				typeof lengthOrEncoding === "string" ? lengthOrEncoding : "utf8";
			return handle.writeSync(
				toWriteBuffer(data, encoding as BufferEncoding),
				pos
			);
		}

		let src = toWriteBuffer(data);

		let offset: number;
		let len: number;
		let pos: number | null;
		if (
			typeof offsetOrPositionOrOptions === "object" &&
			offsetOrPositionOrOptions !== null
		) {
			offset = offsetOrPositionOrOptions.offset ?? 0;
			len = offsetOrPositionOrOptions.length ?? src.byteLength - offset;
			pos = offsetOrPositionOrOptions.position ?? null;
		} else {
			offset = offsetOrPositionOrOptions ?? 0;
			len = lengthOrEncoding;
			if (typeof len !== "number") len = src.byteLength - offset;
			pos = position ?? null;
		}
		if (typeof pos === "bigint") pos = Number(pos);
		return handle.writeSync(src.subarray(offset, offset + len), pos);
	},
	fstatSync(fd, options?) {
		return getHandle(fd, "fstat").statSync(
			(options as any)?.bigint || false
		) as AnyStats;
	},
	fsyncSync(fd) {
		getHandle(fd, "fsync").syncSync();
	},
	fdatasyncSync(fd) {
		getHandle(fd, "fdatasync").syncSync();
	},
	ftruncateSync(fd, len?) {
		getHandle(fd, "ftruncate").truncateSync(len ?? 0);
	},
	// One round trip for the whole vector. These used to loop, paying a blocking request per
	// buffer — which for a scatter read of eight 64 KiB buffers was eight.
	readvSync(fd, buffers, position?) {
		let pos = position ?? null;
		if (typeof pos === "bigint") pos = Number(pos);
		return getHandle(fd, "readv").readvSync(buffers, pos);
	},
	writevSync(fd, buffers, position?) {
		let pos = position ?? null;
		if (typeof pos === "bigint") pos = Number(pos);
		return getHandle(fd, "writev").writevSync(buffers, pos);
	},
	// puterfs has no mode/owner bits; validate the fd and no-op.
	fchmodSync(fd, _mode) {
		getHandle(fd, "fchmod");
	},
	fchownSync(fd, _uid, _gid) {
		getHandle(fd, "fchown");
	},
};

// Bound for the same reason as the promise API — see the note at the end of ./promises.ts.
// `const { statSync } = require("fs")` is if anything more common than the promise form, and
// `lstatSync` -> `statSync`, `chmodSync` -> `statSync` and `rmSync` -> `readdirSync` each reach a
// sibling through `this`.
for (const [name, value] of Object.entries(fsSync)) {
	if (typeof value !== "function") continue;
	const bound = (value as (...args: any[]) => any).bind(fsSync);
	Object.defineProperty(bound, "name", { value: name, configurable: true });
	(fsSync as unknown as Record<string, unknown>)[name] = bound;
}
