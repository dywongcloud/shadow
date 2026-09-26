// The filesystem facade: what everything above this layer calls.
//
// Resolves a path to its mount, delegates, and owns the three things that are about the
// *namespace* rather than any one backend:
//
//   - **Mount grafting.** A listing has to show mounts rooted beneath it, and a recursive
//     listing must not report the real contents of a directory that a mount shadows.
//   - **Cross-mount operations.** No single backend can rename between two of them.
//   - **The optional half of the provider interface.** `readRange`, `copyFile` and
//     `openRead` are fast paths where a backend has one and are derived here where it
//     doesn't, so no caller ever branches on provider capability.
//
// Deliberately NOT here: argument coercion and node's classes, which stay in the worker
// with the `fs` surface, and anything that reads the cwd. Every path reaching this file is
// already absolute and normalized — the worker resolves against its own `process.cwd()`
// before the call crosses, which is also the containment guarantee the mount layer relies
// on.

import { fsError } from "../../vfs/errno";
import { basename, under } from "../../vfs/path";

/** What `statfs` reports for a backend that has no notion of capacity. See `statfs` below. */
const UNKNOWN_CAPACITY = 1024 ** 3;
import type { FsEntry, Listing, ReaddirOpts, WireCtx } from "../../vfs/entry";
import type { ProviderStream, VfsProvider } from "../../vfs/provider";
import type { Mount, MountTable } from "./mounts";
import { streamOfBytes } from "./stream";

/** Lift a provider's mount-local entry onto the absolute namespace. */
function reroot(root: string, entry: FsEntry): FsEntry {
	if (root === "/") return entry;
	return { ...entry, path: entry.path === "/" ? root : root + entry.path };
}

/** The directory entry a mount point presents to a listing of its parent. */
function mountPointEntry(m: Mount): FsEntry {
	return {
		path: m.root,
		name: basename(m.root),
		uid: "",
		isDir: true,
		isSymlink: false,
		size: 0,
		modifiedMs: m.createdMs,
		createdMs: m.createdMs,
		accessedMs: m.createdMs,
	};
}

export class Facade {
	#table: MountTable;
	/**
	 * Operations per mount root, for the same reason the api call counts exist: this is the
	 * only way to see how much filesystem traffic a run actually makes, and after the move
	 * every one of these is a round trip the worker paid for.
	 */
	#opCounts = new Map<string, number>();

	constructor(table: MountTable) {
		this.#table = table;
	}

	opStats(): Record<string, number> {
		return Object.fromEntries([...this.#opCounts].sort((a, b) => b[1] - a[1]));
	}

	resetOpStats() {
		this.#opCounts.clear();
	}

	#resolve(path: string, ctx: WireCtx) {
		const r = this.#table.resolve(path);
		const key = `${r.mount.root} ${ctx.syscall}`;
		this.#opCounts.set(key, (this.#opCounts.get(key) ?? 0) + 1);
		return r;
	}

	#assertWritable(m: Mount, ctx: WireCtx) {
		if (m.readOnly) throw fsError("EROFS", ctx);
	}

	// ------------------------------------------------------------- primitives

	async stat(ctx: WireCtx, path: string): Promise<FsEntry> {
		const r = this.#resolve(path, ctx);
		return reroot(r.mount.root, await r.mount.provider.stat(ctx, r.local));
	}

	async readdir(
		ctx: WireCtx,
		path: string,
		opts?: ReaddirOpts
	): Promise<Listing> {
		const r = this.#resolve(path, ctx);
		const listing = await r.mount.provider.readdir(ctx, r.local, opts);
		let entries = listing.entries.map((e) => reroot(r.mount.root, e));
		let complete = listing.complete;

		if (opts?.recursive) {
			// The provider happily listed the real contents of a directory that a mount
			// shadows. Drop them; the recursion below contributes the mount's own view.
			entries = entries.filter((e) => !this.#shadowed(path, e.path));
		}

		// Graft in mounts rooted directly beneath this directory. Never stats them: a stat
		// per mount per listing is a hidden round trip on a hot path, and a caller that
		// wants real numbers stats the path, which resolves *into* the mount and gets the
		// truth.
		for (const m of this.#table.childMounts(path)) {
			const name = basename(m.root);
			const i = entries.findIndex((e) => e.name === name);
			// A mount over a real directory keeps that directory's timestamps, the way a
			// mount point reports the underlying dentry on Linux.
			if (i >= 0) entries[i] = { ...entries[i], isDir: true, isSymlink: false };
			else entries.push(mountPointEntry(m));
		}

		if (opts?.recursive) {
			for (const m of this.#table.mountsUnder(path)) {
				const sub = await this.readdir(ctx, m.root, opts);
				entries.push(...sub.entries);
				complete = complete && sub.complete;
			}
		}

		return { entries, complete };
	}

	async readFile(ctx: WireCtx, path: string): Promise<Uint8Array> {
		const r = this.#resolve(path, ctx);
		return r.mount.provider.readFile(ctx, r.local);
	}

	async writeFile(ctx: WireCtx, path: string, data: Uint8Array): Promise<void> {
		const r = this.#resolve(path, ctx);
		this.#assertWritable(r.mount, ctx);
		await r.mount.provider.writeFile(ctx, r.local, data);
	}

	async mkdir(
		ctx: WireCtx,
		path: string,
		opts: { recursive: boolean }
	): Promise<string | undefined> {
		const r = this.#resolve(path, ctx);
		this.#assertWritable(r.mount, ctx);
		// node reports the first directory a recursive mkdir created; the provider names it
		// in its own local terms, so lift it back onto the mount.
		const first = await r.mount.provider.mkdir(ctx, r.local, opts);
		if (first === undefined) return undefined;
		return r.mount.root === "/" ? first : r.mount.root + first;
	}

	async rm(
		ctx: WireCtx,
		path: string,
		opts: { recursive: boolean; force: boolean }
	): Promise<void> {
		const r = this.#resolve(path, ctx);
		this.#assertWritable(r.mount, ctx);
		await r.mount.provider.rm(ctx, r.local, opts);
	}

	async rename(ctx: WireCtx, from: string, to: string): Promise<void> {
		const src = this.#resolve(from, ctx);
		const dst = this.#resolve(to, ctx);
		this.#assertWritable(src.mount, ctx);
		this.#assertWritable(dst.mount, ctx);

		if (src.mount === dst.mount) {
			await src.mount.provider.rename(ctx, src.local, dst.local);
			return;
		}

		// No backend can move bytes into another one, so this degrades to copy-then-delete.
		// Deliberately transparent rather than EXDEV: vite and npm both rename a temp file
		// into place, and those two paths will routinely straddle a mount boundary once
		// node_modules is served from an archive. The cost is that it is not atomic, which
		// is worth stating out loud.
		const entry = await src.mount.provider.stat(ctx, src.local);
		if (entry.isDir) throw fsError("EXDEV", { ...ctx, path: from });
		const data = await src.mount.provider.readFile(ctx, src.local);
		await dst.mount.provider.writeFile(ctx, dst.local, data);
		await src.mount.provider.rm(ctx, src.local, {
			recursive: false,
			force: false,
		});
	}

	async utimes(
		ctx: WireCtx,
		path: string,
		atimeMs: number,
		mtimeMs: number
	): Promise<boolean> {
		const r = this.#resolve(path, ctx);
		this.#assertWritable(r.mount, ctx);
		return r.mount.provider.utimes(ctx, r.local, atimeMs, mtimeMs);
	}

	// --------------------------------------------- always available, sometimes derived

	/** Sliced out of a whole-file read when the backend has no positioned read. */
	async readRange(
		ctx: WireCtx,
		path: string,
		offset: number,
		length: number
	): Promise<Uint8Array> {
		const r = this.#resolve(path, ctx);
		const p = r.mount.provider;
		if (p.readRange) return p.readRange(ctx, r.local, offset, length);
		const whole = await p.readFile(ctx, r.local);
		return whole.subarray(offset, offset + length);
	}

	/** Read-then-write when the backend has no server-side copy. */
	async copyFile(
		ctx: WireCtx,
		from: string,
		to: string,
		opts: { overwrite: boolean }
	): Promise<void> {
		const src = this.#resolve(from, ctx);
		const dst = this.#resolve(to, ctx);
		this.#assertWritable(dst.mount, ctx);

		// The server-side copy is only usable when both ends are the same backend.
		if (src.mount === dst.mount && src.mount.provider.copyFile) {
			await src.mount.provider.copyFile(ctx, src.local, dst.local, opts);
			return;
		}
		if (!opts.overwrite) {
			let exists = true;
			try {
				await dst.mount.provider.stat(
					{ syscall: "stat", reportPath: to },
					dst.local
				);
			} catch (err) {
				const code = (err as NodeJS.ErrnoException).code;
				if (code !== "ENOENT" && code !== "ENOTDIR") throw err;
				exists = false;
			}
			if (exists) throw fsError("EEXIST", { ...ctx, path: to });
		}
		const data = await src.mount.provider.readFile(ctx, src.local);
		await dst.mount.provider.writeFile(ctx, dst.local, data);
	}

	/**
	 * Always available: synthesized from a whole-file read when the backend cannot stream.
	 *
	 * This is why the worker no longer has to ask whether a path is streamable — the
	 * question that `createReadStream` used to get wrong by assuming every path was served
	 * by puterfs. `MountSnapshot.canStream` survives only as a hint about whether the stream
	 * is *native*, i.e. whether it avoids buffering the file first.
	 */
	async openRead(
		ctx: WireCtx,
		path: string,
		range?: { start: number; end?: number }
	): Promise<ProviderStream> {
		const r = this.#resolve(path, ctx);
		const p = r.mount.provider;
		if (p.openRead) return p.openRead(ctx, r.local, range);
		return streamOfBytes(await p.readFile(ctx, r.local), range);
	}

	async statfs(
		ctx: WireCtx,
		path: string
	): Promise<{ used: number; capacity: number }> {
		const r = this.#resolve(path, ctx);
		const p = r.mount.provider;
		// A backend with no notion of capacity still must not look *full*. Zeros here were read as
		// "no bytes free" by anything that checks for room before writing — which is how a memory
		// `/tmp` with plenty of space failed every Claude Code Bash command. Report a plausible
		// capacity that is entirely free instead: unknown is closer to "room available" than to
		// "none", and the honest alternative — refusing to answer — is not open to us, because
		// node's `statfs` has no way to say "I don't know".
		if (!p.statfs) return { used: 0, capacity: UNKNOWN_CAPACITY };
		return p.statfs(ctx);
	}

	/** Whether `p` lies inside a mount grafted somewhere beneath `dir`. */
	#shadowed(dir: string, p: string): boolean {
		return this.#table.mountsUnder(dir).some((m) => under(m.root, p));
	}
}
