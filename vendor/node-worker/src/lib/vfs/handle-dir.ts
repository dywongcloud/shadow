// A filesystem over a `FileSystemDirectoryHandle`.
//
// One provider for both places those come from, because they are the same object:
//
//   navigator.storage.getDirectory()   → OPFS: private, persistent, no permission prompt
//   showDirectoryPicker()              → a real directory the user chose
//
// This is the backend the whole move existed to make possible. Every method here awaits
// something, and under the old worker-side contract that was simply not expressible: the same
// code had to satisfy `fs.readFileSync` over a blocking transport, so a provider could not
// await at all. Nothing here is unusual now — it is ordinary async web code.
//
// ## What this backend cannot do
//
//   - **Timestamps.** A `File` has `lastModified`, and that is the only time available: there is
//     no created or accessed time, and no way to *set* any of them. So `utimes` reports `false`
//     and directories report 0 rather than inventing a value that would jitter every stat and
//     defeat anything caching on mtime.
//   - **Rename.** `FileSystemHandle.move()` exists in Chromium and is the only atomic option;
//     elsewhere this degrades to copy-then-delete, which for a directory means a recursive walk.
//   - **Locking is real here.** An open `FileSystemWritableFileStream` excludes other writers to
//     the same file, including another tab. That surfaces as EBUSY rather than a hang: writes are
//     serialized per path within this provider so it never fights itself, and a lock held
//     elsewhere is reported instead of waited on.

import { fsError } from "../../vfs/errno";
import { basename, dirname } from "../../vfs/path";
import type { FsEntry, Listing, ReaddirOpts, WireCtx } from "../../vfs/entry";
import type { ProviderStream, VfsProvider } from "../../vfs/provider";
import { NO_EVENTS, type FsEvents } from "./events";

export interface DirectoryHandleProviderOptions {
	name?: string;
	/** Reject every mutation with EROFS, without asking the backend. */
	readOnly?: boolean;
	events?: FsEvents;
	/**
	 * The absolute path this provider is mounted at, for watch events. A constant it is handed,
	 * never a lookup — providers do not consult the mount table.
	 */
	prefix?: string;
}

function segments(path: string): string[] {
	return path.split("/").filter((s) => s.length > 0 && s !== ".");
}

/**
 * Translate a DOMException into the errno a filesystem would report.
 *
 * The mapping is the whole reason a caller can treat this like any other backend: without it
 * every `catch (e) { if (e.code !== "ENOENT") throw e }` upstream — which is the dominant idiom
 * in this tree — would rethrow on a perfectly ordinary missing file.
 */
function translate(err: unknown, ctx: WireCtx): never {
	const name = (err as DOMException)?.name;
	// Always `reportPath`, never the mount-local path: a provider works in its own terms but an
	// error has to name the path the caller asked about. A mount at `/opfs` failing on its local
	// `/conf/x` must report `/opfs/conf/x`.
	const at = { ...ctx, path: ctx.reportPath };
	switch (name) {
		case "NotFoundError":
			throw fsError("ENOENT", at);
		case "TypeMismatchError":
			// Asked for a file and found a directory, or the reverse. Which errno depends on what
			// was asked for, and the caller knows: `syscall` carries it.
			throw fsError(at.syscall === "scandir" ? "ENOTDIR" : "EISDIR", at);
		case "NotAllowedError":
			// Permission was not granted, or was revoked.
			throw fsError("EACCES", at);
		case "SecurityError":
			throw fsError("EACCES", at);
		case "InvalidModificationError":
			// `removeEntry` on a non-empty directory without `recursive`.
			throw fsError("ENOTEMPTY", at);
		case "NoModificationAllowedError":
			// Someone else holds a writable on this file — another tab, or another handle.
			throw fsError("EBUSY", at);
		case "QuotaExceededError":
			throw fsError("ENOSPC", at);
		case "InvalidStateError":
			throw fsError("EIO", at);
		default:
			throw err;
	}
}

export function createDirectoryHandleProvider(
	root: FileSystemDirectoryHandle,
	opts: DirectoryHandleProviderOptions = {}
): VfsProvider {
	const name = opts.name ?? "fs-handle";
	const prefix = opts.prefix ?? "";
	const events = opts.events ?? NO_EVENTS;

	/** The absolute path a local one corresponds to, for watch events. */
	function full(local: string): string {
		if (!prefix) return local;
		return local === "/" ? prefix : prefix + local;
	}

	function assertWritable(ctx: WireCtx) {
		if (opts.readOnly) throw fsError("EROFS", ctx);
	}

	/**
	 * Writes to one path are serialized, because a `FileSystemWritableFileStream` takes an
	 * exclusive lock: two overlapping writes to the same file through this provider would make it
	 * fail against itself with `NoModificationAllowedError`. A lock held *outside* this provider
	 * is still reported as EBUSY — it is not ours to wait for.
	 */
	const writeChains = new Map<string, Promise<unknown>>();

	function serialize<T>(path: string, fn: () => Promise<T>): Promise<T> {
		const prev = writeChains.get(path) ?? Promise.resolve();
		// `then(fn, fn)` rather than `then(fn)`: a failed write must not wedge the chain behind it.
		const next = prev.then(fn, fn);
		const settled = next.then(
			() => undefined,
			() => undefined
		);
		writeChains.set(path, settled);
		// Dropped once this is still the tail, so a long session does not retain one promise per
		// file it ever touched. If another write queued behind us the entry is theirs now.
		void settled.then(() => {
			if (writeChains.get(path) === settled) writeChains.delete(path);
		});
		return next;
	}

	async function dirAt(
		path: string,
		ctx: WireCtx,
		create = false
	): Promise<FileSystemDirectoryHandle> {
		let dir = root;
		for (const seg of segments(path)) {
			try {
				dir = await dir.getDirectoryHandle(seg, { create });
			} catch (err) {
				translate(err, ctx);
			}
		}
		return dir;
	}

	async function fileAt(
		path: string,
		ctx: WireCtx,
		create = false
	): Promise<FileSystemFileHandle> {
		const parent = await dirAt(dirname(path), ctx);
		try {
			return await parent.getFileHandle(basename(path), { create });
		} catch (err) {
			translate(err, ctx);
		}
	}

	/** A file's `File`, which is where size and mtime come from. */
	async function fileOf(path: string, ctx: WireCtx): Promise<File> {
		const handle = await fileAt(path, ctx);
		try {
			return await handle.getFile();
		} catch (err) {
			translate(err, ctx);
		}
	}

	function entryOfFile(path: string, file: File): FsEntry {
		return {
			path,
			name: basename(path) || "/",
			uid: "",
			isDir: false,
			isSymlink: false,
			size: file.size,
			// The only timestamp this backend has. Reported for all three rather than left at 0,
			// because `mtime` is the one anything actually reads.
			modifiedMs: file.lastModified,
			createdMs: file.lastModified,
			accessedMs: file.lastModified,
		};
	}

	function entryOfDir(path: string): FsEntry {
		return {
			path,
			name: basename(path) || "/",
			uid: "",
			isDir: true,
			isSymlink: false,
			size: 0,
			// Deliberately 0, not `Date.now()`: a directory has no timestamp here, and inventing
			// one would change on every stat and defeat anything comparing mtimes.
			modifiedMs: 0,
			createdMs: 0,
			accessedMs: 0,
		};
	}

	/** Whether a path is a directory, a file, or absent — one probe, both answers. */
	async function kindOf(
		path: string,
		ctx: WireCtx
	): Promise<{ kind: "dir" | "file"; file?: File }> {
		if (segments(path).length === 0) return { kind: "dir" };
		const parent = await dirAt(dirname(path), ctx);
		const base = basename(path);
		try {
			const handle = await parent.getFileHandle(base);
			return { kind: "file", file: await handle.getFile() };
		} catch (err) {
			const name = (err as DOMException)?.name;
			if (name !== "TypeMismatchError" && name !== "NotFoundError") {
				translate(err, ctx);
			}
			// Not a file. Either a directory or genuinely absent, and `getDirectoryHandle` says
			// which — its NotFoundError becomes the ENOENT.
			try {
				await parent.getDirectoryHandle(base);
				return { kind: "dir" };
			} catch (dirErr) {
				translate(dirErr, ctx);
			}
		}
	}

	/** The byte-writing half of `writeFile`, without the event — a copy is not a write. */
	async function writeBytes(
		path: string,
		data: Uint8Array,
		ctx: WireCtx
	): Promise<void> {
		await serialize(path, async () => {
			// `create: true` so a write to a new path works, as every other backend here does.
			const handle = await fileAt(path, ctx, true);
			let writable: FileSystemWritableFileStream;
			try {
				// Truncating: this is a whole-file write, and leaving existing data would turn a
				// shorter write into a partial overwrite.
				writable = await handle.createWritable({ keepExistingData: false });
			} catch (err) {
				translate(err, ctx);
			}
			try {
				// The cast is the SharedArrayBuffer case in the DOM types: a `Uint8Array` may in
				// principle be backed by one, which `write` does not accept. Nothing here ever
				// produces a shared buffer — frames are decoded into ordinary ones.
				await writable.write(data as unknown as ArrayBufferView<ArrayBuffer>);
				await writable.close();
			} catch (err) {
				// Best-effort: a failed write must not leave the lock held.
				await writable.abort().catch(() => undefined);
				translate(err, ctx);
			}
		});
	}

	async function removeTree(path: string, ctx: WireCtx): Promise<void> {
		const parent = await dirAt(dirname(path), ctx);
		try {
			await parent.removeEntry(basename(path), { recursive: true });
		} catch (err) {
			translate(err, ctx);
		}
	}

	/**
	 * Move a directory by copying it and deleting the original.
	 *
	 * Destination handling follows `rename(2)` rather than what is convenient: an existing empty
	 * directory is replaced, a non-empty one is ENOTEMPTY, and a file is ENOTDIR. Merging into an
	 * existing tree would be the easy thing to write here and is not a rename.
	 */
	async function moveDirectory(
		from: string,
		to: string,
		ctx: WireCtx
	): Promise<void> {
		// One `move` event for the whole tree, emitted by the caller — not a write per copied file,
		// which would describe how the rename was implemented rather than what happened.
		let existing: { kind: "dir" | "file" } | undefined;
		try {
			existing = await kindOf(to, ctx);
		} catch {
			// Absent, which is the ordinary case.
		}
		if (existing?.kind === "file") throw fsError("ENOTDIR", ctx);
		if (existing?.kind === "dir") {
			const listing = await dirAt(to, ctx);
			for await (const _ of (
				listing as unknown as { keys(): AsyncIterableIterator<string> }
			).keys()) {
				throw fsError("ENOTEMPTY", ctx);
			}
			const parent = await dirAt(dirname(to), ctx);
			try {
				await parent.removeEntry(basename(to));
			} catch (err) {
				translate(err, ctx);
			}
		}

		const copy = async (src: string, dst: string): Promise<void> => {
			await dirAt(dst, ctx, true);
			const dir = await dirAt(src, ctx);
			// Materialized before copying: adding entries to a directory while iterating it is not
			// something the api defines, and the destination can be inside the source's parent.
			const children: Array<[string, FileSystemHandle]> = [];
			for await (const pair of (
				dir as unknown as {
					entries(): AsyncIterableIterator<[string, FileSystemHandle]>;
				}
			).entries()) {
				children.push(pair);
			}
			for (const [name, handle] of children) {
				const childFrom = src === "/" ? `/${name}` : `${src}/${name}`;
				const childTo = dst === "/" ? `/${name}` : `${dst}/${name}`;
				if (handle.kind === "directory") {
					await copy(childFrom, childTo);
					continue;
				}
				const file = await (handle as FileSystemFileHandle).getFile();
				await writeBytes(
					childTo,
					new Uint8Array(await file.arrayBuffer()),
					ctx
				);
			}
		};
		await copy(from, to);
		await removeTree(from, ctx);
	}

	async function collect(
		dir: FileSystemDirectoryHandle,
		base: string,
		out: FsEntry[],
		recursive: boolean,
		depth: number,
		maxEntries: number
	): Promise<boolean> {
		for await (const [childName, handle] of (
			dir as unknown as {
				entries(): AsyncIterableIterator<[string, FileSystemHandle]>;
			}
		).entries()) {
			if (out.length >= maxEntries) return false;
			const childPath = base === "/" ? `/${childName}` : `${base}/${childName}`;
			if (handle.kind === "directory") {
				out.push(entryOfDir(childPath));
				if (recursive && depth > 1) {
					const complete = await collect(
						handle as FileSystemDirectoryHandle,
						childPath,
						out,
						recursive,
						depth - 1,
						maxEntries
					);
					if (!complete) return false;
				}
			} else {
				out.push(
					entryOfFile(
						childPath,
						await (handle as FileSystemFileHandle).getFile()
					)
				);
			}
		}
		return true;
	}

	return {
		name,

		async stat(ctx, path): Promise<FsEntry> {
			const found = await kindOf(path, ctx);
			return found.kind === "file"
				? entryOfFile(path, found.file!)
				: entryOfDir(path);
		},

		async readdir(ctx, path, o?: ReaddirOpts): Promise<Listing> {
			const found = await kindOf(path, ctx);
			if (found.kind !== "dir") throw fsError("ENOTDIR", ctx);
			const dir = await dirAt(path, ctx);
			const out: FsEntry[] = [];
			const maxEntries = o?.maxEntries ?? Infinity;
			const complete = await collect(
				dir,
				segments(path).length === 0 ? "/" : path,
				out,
				!!o?.recursive,
				o?.depth ?? Infinity,
				maxEntries
			);
			return { entries: out, complete };
		},

		async readFile(ctx, path): Promise<Uint8Array> {
			const file = await fileOf(path, ctx);
			return new Uint8Array(await file.arrayBuffer());
		},

		/**
		 * A genuine positioned read: `Blob.slice` does not move the earlier bytes.
		 *
		 * Worth being sure about, because the mount snapshot advertises this and a caller keeps a
		 * byte-range cache on the strength of it. Claiming a native range that actually re-reads
		 * the file would make a positioned-read loop quadratic.
		 */
		async readRange(ctx, path, offset, length): Promise<Uint8Array> {
			const file = await fileOf(path, ctx);
			const slice = file.slice(offset, offset + length);
			return new Uint8Array(await slice.arrayBuffer());
		},

		async openRead(ctx, path, range): Promise<ProviderStream> {
			const file = await fileOf(path, ctx);
			const start = range?.start ?? 0;
			// node's `end` is inclusive.
			const end = range?.end === undefined ? file.size : range.end + 1;
			const blob =
				start === 0 && end >= file.size ? file : file.slice(start, end);
			return { size: blob.size, stream: blob.stream() };
		},

		async writeFile(ctx, path, data): Promise<void> {
			assertWritable(ctx);
			await writeBytes(path, data, ctx);
			events.write(full(path));
		},

		async mkdir(ctx, path, o): Promise<string | undefined> {
			assertWritable(ctx);
			const parts = segments(path);
			if (parts.length === 0) throw fsError("EEXIST", ctx);

			if (!o.recursive) {
				// The parent must exist and the target must not — neither of which
				// `getDirectoryHandle({create:true})` checks, since it is idempotent and creates
				// only the last segment.
				const parent = await dirAt(dirname(path), ctx);
				const base = basename(path);
				let exists = true;
				try {
					await parent.getDirectoryHandle(base);
				} catch (err) {
					const errName = (err as DOMException)?.name;
					if (errName === "TypeMismatchError") throw fsError("EEXIST", ctx);
					if (errName !== "NotFoundError") translate(err, ctx);
					exists = false;
				}
				if (exists) throw fsError("EEXIST", ctx);
				try {
					await parent.getDirectoryHandle(base, { create: true });
				} catch (err) {
					translate(err, ctx);
				}
				events.add(full(path), true);
				return undefined;
			}

			await dirAt(path, ctx, true);
			events.add(full(path), true);
			// node reports the first directory a recursive mkdir created. Learning that would mean
			// probing every ancestor first, so it is left undefined — the same answer puterfs gives.
			return undefined;
		},

		async rm(ctx, path, o): Promise<void> {
			assertWritable(ctx);
			const parts = segments(path);
			if (parts.length === 0) throw fsError("EPERM", ctx);
			let isDir = false;
			try {
				isDir = (await kindOf(path, ctx)).kind === "dir";
			} catch (err) {
				if (o.force && (err as NodeJS.ErrnoException).code === "ENOENT") return;
				throw err;
			}
			const parent = await dirAt(dirname(path), ctx);
			try {
				await parent.removeEntry(basename(path), { recursive: o.recursive });
			} catch (err) {
				if (o.force && (err as DOMException)?.name === "NotFoundError") return;
				translate(err, ctx);
			}
			events.remove(full(path), isDir);
		},

		async rename(ctx, from, to): Promise<void> {
			assertWritable(ctx);
			const found = await kindOf(from, ctx);

			// `move` is atomic and is the only correct answer, but it is Chromium-only (and for a
			// long time OPFS-only). Probed rather than assumed, so this works either way.
			const handle =
				found.kind === "file"
					? await fileAt(from, ctx)
					: await dirAt(from, ctx);
			const movable = handle as unknown as {
				move?: (
					parent: FileSystemDirectoryHandle,
					name?: string
				) => Promise<void>;
			};
			if (typeof movable.move === "function") {
				const destParent = await dirAt(dirname(to), ctx);
				try {
					await movable.move(destParent, basename(to));
					events.move(full(from), full(to), found.kind === "dir");
					return;
				} catch (err) {
					const errName = (err as DOMException)?.name;
					// Not supported for this handle after all; fall through to the copy.
					if (errName !== "NotSupportedError" && errName !== "TypeError") {
						translate(err, ctx);
					}
				}
			}

			if (found.kind === "dir") {
				// A directory move without `move()` is a recursive copy plus a delete.
				//
				// This used to report EXDEV instead, on the reasoning that a caller renaming a
				// directory into place is relying on atomicity and half-doing it is worse than
				// declining. That was wrong about what declining costs: `vite dev` renames
				// `node_modules/.vite/deps_temp_*` over `deps` on every startup and does not catch
				// the failure, so EXDEV here means the dev server does not run at all. A
				// non-atomic move is a real limitation; refusing to move is not a smaller one.
				//
				// Where `move()` exists — Chromium's OPFS — none of this runs and the rename is
				// atomic. The gap is documented on `createDirectoryHandleProvider`.
				await moveDirectory(from, to, ctx);
				events.move(full(from), full(to), true);
				return;
			}
			const bytes = new Uint8Array(await found.file!.arrayBuffer());
			await this.writeFile!({ ...ctx, reportPath: to }, to, bytes);
			await this.rm!(ctx, from, { recursive: false, force: false });
			events.move(full(from), full(to), false);
		},

		/**
		 * Not representable: there is no api to set a timestamp on a `FileSystemFileHandle`.
		 *
		 * `false` is a normal answer rather than an error, and the composed `utimes` above still
		 * validates the path — so `utimes` on a missing file reports ENOENT as node requires.
		 */
		async utimes(): Promise<boolean> {
			return false;
		},

		async statfs(): Promise<{ used: number; capacity: number }> {
			// Origin-wide, not per-directory — which is what `statfs(2)` reports too. Only
			// meaningful for OPFS; a picked directory has no quota to report and answers zeroes.
			const estimate = await navigator.storage?.estimate?.();
			return {
				used: estimate?.usage ?? 0,
				capacity: estimate?.quota ?? 0,
			};
		},
	};
}

/**
 * Whether this handle can be used, asking for permission if it has not been granted.
 *
 * OPFS needs none of this — it is same-origin private storage. A handle from
 * `showDirectoryPicker()` does, its grant does not survive a reload, and re-requesting requires a
 * user gesture. So this must be called from a click handler, not from mount time, and mounting a
 * handle without a grant would otherwise fail per-operation with EACCES instead of once, up front.
 */
export async function ensureDirectoryHandleAccess(
	handle: FileSystemDirectoryHandle,
	mode: "read" | "readwrite" = "readwrite"
): Promise<boolean> {
	const withPermissions = handle as unknown as {
		queryPermission?: (d: { mode: string }) => Promise<PermissionState>;
		requestPermission?: (d: { mode: string }) => Promise<PermissionState>;
	};
	if (!withPermissions.queryPermission) return true; // OPFS, or an engine without the api
	if ((await withPermissions.queryPermission({ mode })) === "granted")
		return true;
	return (await withPermissions.requestPermission?.({ mode })) === "granted";
}
