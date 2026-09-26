// The asynchronous half of the node:fs surface.
//
// The mirror of ./sync.ts: identical argument handling, `hostAsync` instead of `host`. Anything
// that differs between the two files is either a genuine asynchronous capability the sync api
// cannot express (an `AbortSignal`, a `Readable` payload, an async `cp` filter) or a bug.

import nodeBuffer from "../buffer";
import nodeStream from "../stream";
import nodePath from "../path";
import {
	createFsError,
	fsConstants,
	normalizePath,
	toWriteBuffer,
	type AnyStats,
} from "./util";
import { encodeEntry } from "./readdir-encode";
import { ctx, hostAsync } from "./host";
import { toEpochMs } from "./util";
import { Stats, StatsFs, Dirent, Dir } from "./classes";
import { FileHandle } from "./handle";
import { streamToBuffer } from "../utils";

type NodeFs = typeof import("node:fs");
type NodeFsPromises = NodeFs["promises"];

let Buffer = nodeBuffer.Buffer;

export let promisesToDepromisify: Omit<
	NodeFsPromises,
	"watch" | "glob" | "constants"
> = {
	async appendFile(path, data, options) {
		if (typeof options === "string") options = { encoding: options };
		else if (!options) options = {};

		let p = normalizePath(path as any);
		// No signal: node's appendFile options don't carry one.
		await hostAsync.append(
			ctx("open", p),
			p,
			toWriteBuffer(data, options.encoding)
		);
	},
	async copyFile(src, dest, mode) {
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

		await hostAsync.copyFile(ctx("copyfile", from), from, to, overwrite);
	},
	async mkdir(path, options) {
		let p = normalizePath(path);

		if (typeof options === "number" || typeof options === "string")
			options = { mode: options };
		else if (!options) options = {};

		// mode is ignored: puterfs has no POSIX permission bits.
		let recursive = options.recursive || false;
		let first = await hostAsync.mkdir(ctx("mkdir", p), p, recursive);
		// node returns the first directory created, or undefined. The api doesn't
		// reliably report it, so this is undefined more often than on a real fs.
		//
		// Cast because node splits this across overloads — `Promise<string|undefined>`
		// for `{recursive: true}` and `Promise<void>` otherwise — and one
		// implementation signature can't be assignable to both.
		return (recursive ? first : undefined) as any;
	},
	async opendir(path, options?) {
		path = normalizePath(path);

		let entries = (await this.readdir(path, {
			withFileTypes: true,
			recursive: options?.recursive,
			encoding: options?.encoding,
		})) as InstanceType<typeof Dirent>[];
		return new Dir(path, entries);
	},
	async open(path, flags?, mode?) {
		void mode;
		// Our FileHandle implements node's whole surface but with single, widened
		// signatures rather than node's overload sets (`read(buffer, offset, ...)`
		// vs `read(buffer, options)`), which TS won't accept as assignable even
		// though every call shape works. The cast is the boundary; callers still
		// get node's precise types from the public `fs.promises` typing.
		return (await FileHandle.open(path, flags)) as unknown as Awaited<
			ReturnType<NodeFsPromises["open"]>
		>;
	},
	async readdir(path, options?) {
		let p = normalizePath(path);

		if (typeof options === "string") options = { encoding: options } as {};
		else if (!options) options = {};

		// One request per subtree instead of one per directory: see
		// ./readdir-recursive.ts for the paging and depth-horizon handling.
		let listing = await hostAsync.readdir(ctx("scandir", p), p, {
			recursive: options.recursive,
		});
		return listing.entries.map((entry) => encodeEntry(entry, p, options));
	},
	async readFile(path, options) {
		let p = normalizePath(path as any);

		if (typeof options === "string") options = { encoding: options };
		else if (!options) options = {};

		// options.flag is accepted and ignored: puterfs has no open modes to honor.
		let buf = await hostAsync.readFile(ctx("open", p), p, options.signal);
		if (options.encoding)
			// not sure why ts doesn't like this
			return buf.toString(options.encoding) as any;
		else return buf;
	},
	async rename(oldPath, newPath) {
		let from = normalizePath(oldPath);
		let to = normalizePath(newPath);
		await hostAsync.rename(ctx("rename", from), from, to);
	},
	async rmdir(path) {
		return await this.unlink(path);
	},
	async rm(path, options) {
		// TODO retries?
		let p = normalizePath(path);
		if (!options) options = {};

		await hostAsync.rm(
			ctx("rm", p),
			p,
			options.recursive || false,
			options.force || false
		);
	},
	async stat(path, options?) {
		let p = normalizePath(path);
		if (!options) options = {};

		let entry = await hostAsync.stat(ctx("stat", p), p);
		return new Stats(entry, options.bigint || false) as AnyStats;
	},
	// puter fs has no symlinks; lstat is just stat.
	async lstat(path, options?) {
		return (await this.stat(path, options as any)) as AnyStats;
	},
	async statfs(path, options?) {
		if (!options) options = {};

		let p = normalizePath(path);
		let df = await hostAsync.statfs(ctx("statfs", p), p);
		return new StatsFs(df, options.bigint || false);
	},
	async writeFile(file, data, options) {
		let p = normalizePath(file as any);

		if (typeof options === "string") options = { encoding: options };
		else if (!options) options = {};

		// options.flag is accepted and ignored: puterfs has no open modes to honor.
		//
		// A `Readable` payload is the one coercion the sync surface cannot share: it
		// has to be drained before the write can be described, and draining is
		// asynchronous. Doing it here, in argument handling, is exactly right — by the
		// time the plan is built there is only a Buffer.
		let buf =
			data instanceof nodeStream.Readable
				? await streamToBuffer(data)
				: toWriteBuffer(data, options.encoding);

		await hostAsync.writeFile(ctx("write", p), p, buf, options.signal);
	},
	async unlink(path) {
		let p = normalizePath(path);
		await hostAsync.rm(ctx("unlink", p), p, false, false);
	},
	// puterfs resolves nothing, so the real path is the canonical path and this does
	// no I/O. See the note on `realpathSyncImpl` in ./sync.ts for why it normalizes
	// rather than echoing the argument back.
	async realpath(path: any, options: any) {
		if (typeof options == "string") options = { encoding: options };
		else if (!options) options = {};

		let resolved = normalizePath(path);
		if (options.encoding == "buffer")
			return Buffer.from(resolved, "utf8") as any;
		return Buffer.from(resolved, "utf8").toString(
			options.encoding || "utf8"
		) as any;
	},
	// Existence + permission probe. puterfs has no real permission bits (mode is
	// a constant 0o777), so R/W/X_OK always pass — only F_OK can fail, which the
	// stat below surfaces as ENOENT.
	async access(path, _mode?) {
		await hostAsync.access(
			ctx("access", normalizePath(path)),
			normalizePath(path)
		);
	},
	async truncate(path, len) {
		let p = normalizePath(path as any);
		await hostAsync.truncate(ctx("open", p), p, len ?? 0);
	},
	// Not `cpPlan` from ./ops.ts, unlike every other derived operation here.
	//
	// `fs.promises.cp` accepts a filter that returns a promise, and a plan cannot
	// await one — so this drives its own walk over the same facade calls. It is the
	// only place the two surfaces genuinely can't share an implementation, and the
	// reason is a user callback in the middle of the operation rather than anything
	// about the transport. Keep the two in step by hand.
	async cp(source, destination, opts) {
		let options = (opts || {}) as any;
		let force = options.force !== false;
		let errorOnExist = options.errorOnExist || false;
		let recursive = options.recursive || false;
		let filter = options.filter as
			| ((s: string, d: string) => boolean | Promise<boolean>)
			| undefined;

		let self = this;
		async function copyEntry(src: string, dest: string): Promise<void> {
			if (filter && !(await filter(src, dest))) return;

			let srcStat = await self.stat(src);
			if (srcStat.isDirectory()) {
				if (!recursive)
					throw createFsError(
						"EISDIR",
						-21,
						"recursive option not enabled, cannot copy a directory",
						"cp",
						src
					);
				await self.mkdir(dest, { recursive: true });
				let entries = (await self.readdir(src)) as string[];
				for (let entry of entries)
					await copyEntry(
						nodePath.join(src, entry),
						nodePath.join(dest, entry)
					);
				return;
			}

			let destExists = false;
			try {
				await self.stat(dest);
				destExists = true;
			} catch {}
			if (destExists) {
				if (errorOnExist)
					throw createFsError("EEXIST", -17, "file already exists", "cp", dest);
				if (!force) return;
			}
			await self.copyFile(src, dest);
		}

		await copyEntry(
			normalizePath(source as any),
			normalizePath(destination as any)
		);
	},
	async mkdtemp(prefix, options?) {
		if (typeof options === "string") options = { encoding: options };
		else if (!options) options = {};

		let path = await hostAsync.mkdtemp(
			ctx("mkdtemp", normalizePath(prefix as any)),
			normalizePath(prefix as any)
		);

		let nameBuf = Buffer.from(path, "utf8");
		if ((options as any).encoding === "buffer") return nameBuf as any;
		return nameBuf.toString((options as any).encoding || undefined) as any;
	},
	async mkdtempDisposable(prefix, options?) {
		let path = (await (this.mkdtemp as any)(prefix, options)) as string;
		let self = this;
		let removed = false;
		let remove = async () => {
			if (removed) return;
			removed = true;
			await self.rm(path, { recursive: true, force: true });
		};
		return {
			path,
			remove,
			[Symbol.asyncDispose]: remove,
		} as any;
	},
	// puterfs has no symlinks and no path-based link api — only `/mkshortcut`,
	// which targets a *uid*, so it can't dangle and `readlink` would have to
	// resolve a uid back to a path. Rather than emulate that badly, report the
	// errno a filesystem genuinely lacking the feature reports; tar, fs-extra and
	// npm all have a fallback path for it.
	async link(existingPath, newPath) {
		void existingPath;
		throw createFsError(
			"EPERM",
			-1,
			"operation not permitted",
			"link",
			normalizePath(newPath as any)
		);
	},
	async symlink(target, path, _type?) {
		void target;
		throw createFsError(
			"EPERM",
			-1,
			"operation not permitted",
			"symlink",
			normalizePath(path as any)
		);
	},
	async readlink(path, _options?) {
		// EINVAL is node's errno for readlink on something that isn't a link, so
		// the stat is load-bearing: it's what distinguishes that from ENOENT.
		let resolved = normalizePath(path as any);
		await this.stat(resolved);
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
	async utimes(path, atime, mtime) {
		let p = normalizePath(path as any);
		// A no-op still has to report ENOENT for a path that isn't there, hence
		// the stat when nothing was sent.
		// The host validates the path itself, so a missing file still reports ENOENT even when
		// the backend cannot represent the requested times.
		await hostAsync.utimes(
			ctx("utime", p),
			p,
			toEpochMs(atime, "utime"),
			toEpochMs(mtime, "utime")
		);
	},
	// Nothing can be a symlink, so there is no link to *not* follow.
	async lutimes(path, atime, mtime) {
		await this.utimes(path, atime, mtime);
	},
	// puterfs has no mode/owner bits; validate existence then no-op.
	async chmod(path, _mode) {
		await this.stat(path);
	},
	async lchmod(path, _mode) {
		await this.stat(path);
	},
	async chown(path, _uid, _gid) {
		await this.stat(path);
	},
	async lchown(path, _uid, _gid) {
		await this.stat(path);
	},
};

// Bind every method to the object, so a *detached* reference still works.
//
// Several methods above reach a sibling through `this` — `lstat` -> `stat`, `chmod`/`chown` ->
// `stat`, `rm` -> `readdir`, `rmdir` -> `unlink` — which is fine for `fs.promises.lstat(p)` and
// broken the moment the function is taken off the object. Both ways of doing that are idiomatic:
//
//     import { lstat } from "node:fs/promises";
//     const { readFile } = require("fs/promises");
//
// and node's own fs.promises functions are standalone, so nothing is supposed to care about the
// receiver. Unbound, they threw "this.stat is not a function" — which is exactly how chokidar's
// `lstat` import failed, taking live reload of settings, themes and keybindings with it and
// surfacing only as a warning.
//
// `depromisify` in ../utils.ts already does this for the callback API, and says why; the promise
// API was the half that went out unbound.
for (const [name, value] of Object.entries(promisesToDepromisify)) {
	if (typeof value !== "function") continue;
	const bound = (value as (...args: any[]) => any).bind(promisesToDepromisify);
	// `bind` renames to "bound stat"; keep the original so anything reading `.name` still sees it.
	Object.defineProperty(bound, "name", { value: name, configurable: true });
	(promisesToDepromisify as unknown as Record<string, unknown>)[name] = bound;
}
