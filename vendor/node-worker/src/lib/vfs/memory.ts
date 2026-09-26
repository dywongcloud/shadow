// An in-memory filesystem.
//
// A real tree — directories, mtimes, listings — not a flat path→bytes map, because the
// things built on it need to be indistinguishable from files: `statSync`, `readdirSync`
// of the parent, and the module resolver's own probing all have to work without knowing a
// path is synthetic.
//
// ## The lazy-content seam
//
// `MemFile.content` may be bytes *or* a thunk. That is the hook an archive-backed mount
// hangs on: it builds the tree from the zip's central directory alone — name, size,
// offset — and each file's thunk inflates its own entry on demand, with `size` coming
// from the header so **`stat` never materializes anything** (and the module resolver stats
// far more paths than it reads).
//
// Note what changed in moving here: the thunk used to have to be *synchronous*, because
// the same code had to satisfy `readFileSync` over a blocking transport and could not
// await. Host-side it can return a promise, so `DecompressionStream` is usable and the
// zlib wasm the worker bundle carries is not needed at all. The half of the rationale
// that survives is the valuable half — `stat` staying free.

import { fsError } from "../../vfs/errno";
import { basename } from "../../vfs/path";
import type { FsEntry, Listing, ReaddirOpts, WireCtx } from "../../vfs/entry";
import type { ProviderStream, VfsProvider } from "../../vfs/provider";
import { NO_EVENTS, type FsEvents } from "./events";
import { streamOfBytes } from "./stream";

export type LazyContent = () => Uint8Array | Promise<Uint8Array>;

export interface MemFile {
	kind: "file";
	/** Known without materializing `content`. */
	size: number;
	mtimeMs: number;
	ctimeMs: number;
	atimeMs: number;
	content: Uint8Array | LazyContent;
	/**
	 * Detached from its parent but still referenced by an open handle. Reads keep working,
	 * as they do on a real filesystem; a flush must not resurrect it.
	 */
	unlinked?: boolean;
}

export interface MemDir {
	kind: "dir";
	children: Map<string, MemNode>;
	mtimeMs: number;
	ctimeMs: number;
	atimeMs: number;
}

export type MemNode = MemFile | MemDir;

/**
 * One node as reported to an out-of-band walk.
 *
 * Deliberately not `FsEntry`: this serves the host, which wants a path it can key a map
 * by and a size/mtime it can diff, not the uid/symlink/atime fields the fs surface has to
 * carry.
 */
export interface MemListEntry {
	/** Path relative to the provider root, with a leading "/". */
	path: string;
	kind: "file" | "dir";
	/** 0 for directories. */
	size: number;
	mtimeMs: number;
}

export interface MemListOptions {
	/** Descend into subdirectories. Unbounded — the tree is the whole extent. */
	recursive?: boolean;
	/** Report only nodes modified strictly after this time. */
	since?: number;
}

function now(): number {
	return Date.now();
}

export function newDir(): MemDir {
	const t = now();
	return {
		kind: "dir",
		children: new Map(),
		mtimeMs: t,
		ctimeMs: t,
		atimeMs: t,
	};
}

export function newFile(
	content: Uint8Array | LazyContent,
	size?: number
): MemFile {
	const t = now();
	return {
		kind: "file",
		size: size ?? (content instanceof Uint8Array ? content.length : 0),
		mtimeMs: t,
		ctimeMs: t,
		atimeMs: t,
		content,
	};
}

/** Materialize a file's bytes, collapsing a thunk on first use. */
async function bytesOf(file: MemFile): Promise<Uint8Array> {
	if (typeof file.content === "function") {
		const produced = await file.content();
		file.content = produced;
		file.size = produced.length;
	}
	return file.content as Uint8Array;
}

function segments(path: string): string[] {
	return path.split("/").filter((s) => s.length > 0 && s !== ".");
}

/**
 * The capacity `statfs` declares for a memory mount.
 *
 * Nothing can measure the real ceiling — it is the tab's heap, and no API reports how much of that
 * is available. 1 GiB is chosen to be larger than anything a scratch mount plausibly holds while
 * staying a believable figure for a filesystem, so a caller sizing a write against `free` gets a
 * sane answer instead of a zero.
 */
const MEMORY_CAPACITY = 1024 ** 3;

export interface MemoryProviderOptions {
	name?: string;
	/**
	 * The absolute path this provider is mounted at, used only to report full paths in
	 * watch events. The provider never consults the mount table — this is a constant it is
	 * handed, not a lookup.
	 */
	prefix?: string;
	/** Seed tree. Defaults to an empty root directory. */
	root?: MemDir;
	/** Where synthesized watch events go. */
	events?: FsEvents;
}

export interface MemoryProvider extends VfsProvider {
	/** @internal — the live tree, for seeding and for tests. */
	readonly root: MemDir;
	/** Create or replace a file, making parent directories as needed. */
	put(path: string, content: Uint8Array | LazyContent, size?: number): MemFile;
	/** Create a directory and any missing parents. Existing ones are left alone. */
	mkdirp(path: string): MemDir;
	/** Remove a path if present. Returns whether anything was removed. */
	drop(path: string): boolean;
	/**
	 * @internal — the file's **live** bytes, or undefined if the path is absent or a
	 * directory.
	 *
	 * The imperative counterpart to `put`: no ctx, no errno — this is for the host reading
	 * its own mount, not for the fs surface. Unlike `readFile` it does not copy, so
	 * nothing reading this may mutate what it gets.
	 *
	 * Synchronous, and therefore **throws on a lazily-backed entry** rather than silently
	 * reporting it absent. Nothing creates one today; an archive mount would, and its
	 * bytes have to be read through the async facade.
	 */
	get(path: string): Uint8Array | undefined;
	/**
	 * Walk a directory, or undefined if the path is absent or a file.
	 *
	 * Unlike `readdir` this can recurse without a depth cap and can filter by mtime,
	 * which is what makes "what changed since the run started?" answerable in one pass
	 * over a tree with a `node_modules` in it.
	 */
	list(path: string, opts?: MemListOptions): MemListEntry[] | undefined;
}

export function createMemoryProvider(
	opts: MemoryProviderOptions = {}
): MemoryProvider {
	const name = opts.name ?? "memory";
	const prefix = opts.prefix ?? "";
	const root: MemDir = opts.root ?? newDir();
	const events = opts.events ?? NO_EVENTS;

	/** The absolute path a local one corresponds to, for watch events. */
	function full(local: string): string {
		if (!prefix) return local;
		return local === "/" ? prefix : prefix + local;
	}

	function lookup(path: string): MemNode | undefined {
		let node: MemNode = root;
		for (const seg of segments(path)) {
			if (node.kind !== "dir") return undefined;
			const next = node.children.get(seg);
			if (!next) return undefined;
			node = next;
		}
		return node;
	}

	function mustFind(path: string, ctx: WireCtx): MemNode {
		const node = lookup(path);
		if (!node) throw fsError("ENOENT", ctx);
		return node;
	}

	function mustFile(path: string, ctx: WireCtx): MemFile {
		const node = mustFind(path, ctx);
		if (node.kind === "dir") throw fsError("EISDIR", ctx);
		return node;
	}

	/** The containing directory and final segment, for a mutation. */
	function parentOf(path: string, ctx: WireCtx): { dir: MemDir; name: string } {
		const parts = segments(path);
		// The mount root itself is not removable or replaceable.
		if (parts.length === 0) throw fsError("EPERM", ctx);
		const name = parts[parts.length - 1];
		let node: MemNode = root;
		for (const seg of parts.slice(0, -1)) {
			if (node.kind !== "dir") throw fsError("ENOTDIR", ctx);
			const next = node.children.get(seg);
			if (!next) throw fsError("ENOENT", ctx);
			node = next;
		}
		if (node.kind !== "dir") throw fsError("ENOTDIR", ctx);
		return { dir: node, name };
	}

	function entryFor(local: string, entryName: string, node: MemNode): FsEntry {
		return {
			// A *local* path. The facade re-roots it onto the mount before anything above
			// sees it, the same way it does for every provider.
			path: local,
			name: entryName,
			uid: "",
			isDir: node.kind === "dir",
			isSymlink: false,
			size: node.kind === "file" ? node.size : 0,
			modifiedMs: node.mtimeMs,
			createdMs: node.ctimeMs,
			accessedMs: node.atimeMs,
		};
	}

	function mkdirp(path: string): MemDir {
		let node: MemDir = root;
		for (const seg of segments(path)) {
			let next = node.children.get(seg);
			if (!next) {
				next = newDir();
				node.children.set(seg, next);
				node.mtimeMs = now();
			}
			if (next.kind !== "dir") {
				throw fsError("ENOTDIR", { syscall: "mkdir", path });
			}
			node = next;
		}
		return node;
	}

	/** "/", "" and "/a/" all normalize to the form `walk` concatenates against. */
	function normalizeLocal(path: string): string {
		const parts = segments(path);
		return parts.length === 0 ? "/" : "/" + parts.join("/");
	}

	// The `list` counterpart to `collect`. Kept separate rather than generalized:
	// `collect` answers readdir (FsEntry, capped depth) and this answers the host
	// (MemListEntry, uncapped, mtime-filtered). Folding them together would mean a
	// function whose every parameter exists for only one of its two callers.
	//
	// `since` filters the output, not the traversal: a directory's mtime does not
	// propagate up from its descendants, so an unmodified ancestor tells you nothing about
	// what changed underneath it and the walk has to be complete regardless.
	function walk(
		dir: MemDir,
		base: string,
		out: MemListEntry[],
		recursive: boolean,
		since?: number
	) {
		for (const [childName, child] of dir.children) {
			const childPath = base === "/" ? `/${childName}` : `${base}/${childName}`;
			if (since === undefined || child.mtimeMs > since) {
				out.push({
					path: childPath,
					kind: child.kind,
					size: child.kind === "file" ? child.size : 0,
					mtimeMs: child.mtimeMs,
				});
			}
			if (recursive && child.kind === "dir") {
				walk(child, childPath, out, recursive, since);
			}
		}
	}

	function collect(
		dir: MemDir,
		base: string,
		out: FsEntry[],
		recursive: boolean,
		depth: number
	) {
		for (const [childName, child] of dir.children) {
			const childPath = base === "/" ? `/${childName}` : `${base}/${childName}`;
			out.push(entryFor(childPath, childName, child));
			if (recursive && child.kind === "dir" && depth > 1) {
				collect(child, childPath, out, recursive, depth - 1);
			}
		}
	}

	const provider: MemoryProvider = {
		name,
		root,

		put(path, content, size) {
			const parts = segments(path);
			const dir = mkdirp("/" + parts.slice(0, -1).join("/"));
			const file = newFile(content, size);
			dir.children.set(parts[parts.length - 1], file);
			dir.mtimeMs = now();
			return file;
		},

		mkdirp,

		drop(path) {
			const parts = segments(path);
			if (parts.length === 0) return false;
			const parent = lookup("/" + parts.slice(0, -1).join("/"));
			if (!parent || parent.kind !== "dir") return false;
			const removed = parent.children.delete(parts[parts.length - 1]);
			if (removed) parent.mtimeMs = now();
			return removed;
		},

		get(path) {
			const node = lookup(path);
			if (!node || node.kind !== "file") return undefined;
			if (typeof node.content === "function") {
				throw new Error(
					`${path} is lazily backed and cannot be read synchronously; read it through the filesystem instead`
				);
			}
			// No atime bump: a host read is out-of-band inspection, not the program
			// touching its own file.
			return node.content;
		},

		list(path, listOpts) {
			const node = lookup(path);
			if (!node || node.kind !== "dir") return undefined;
			const out: MemListEntry[] = [];
			walk(
				node,
				normalizeLocal(path),
				out,
				!!listOpts?.recursive,
				listOpts?.since
			);
			return out;
		},

		async stat(ctx, path): Promise<FsEntry> {
			const node = mustFind(path, ctx);
			return entryFor(path, basename(path) || "/", node);
		},

		/**
		 * `statfs`, and answering with a real capacity is the whole point.
		 *
		 * Without this the facade falls back to `{ used: 0, capacity: 0 }`, which reads as a
		 * filesystem with **zero bytes free** — so a program that checks for room before writing
		 * concludes it has none. Not hypothetical: `/tmp` is a memory mount, Claude Code pre-flights
		 * free space on its temp directory before every Bash command, and the zero made *every*
		 * command fail with "the temp filesystem is full (0MB free)" while writes to that very
		 * directory were succeeding.
		 *
		 * `used` is summed from the tree — `MemFile.size` is known without materializing lazy
		 * content, so this is an in-memory walk with no I/O. `capacity` is a declared budget rather
		 * than a measurement: the real ceiling is the tab's heap, which nothing here can query, and
		 * this number's job is to be a plausible non-zero denominator with visible headroom. It
		 * grows if the contents ever approach it, so `free` never reaches zero and starves a caller
		 * that is only asking whether it may proceed.
		 */
		async statfs(): Promise<{ used: number; capacity: number }> {
			let used = 0;
			const walk = (dir: MemDir): void => {
				for (const child of dir.children.values()) {
					if (child.kind === "dir") walk(child);
					else used += child.size;
				}
			};
			walk(root);
			return { used, capacity: Math.max(MEMORY_CAPACITY, used * 2) };
		},

		async readdir(ctx, path, o?: ReaddirOpts): Promise<Listing> {
			const node = mustFind(path, ctx);
			if (node.kind !== "dir") throw fsError("ENOTDIR", ctx);
			const out: FsEntry[] = [];
			collect(node, path, out, !!o?.recursive, o?.depth ?? Infinity);
			// An in-memory listing is exhaustive by construction — there is no paging and
			// nothing to truncate, so negative inference above is always sound.
			return { entries: out, complete: true };
		},

		// Both reads copy.
		//
		// Handing back the stored bytes would be free, and wrong: `fs.readFileSync`
		// promises a fresh buffer, and callers do mutate what they get — decoders and
		// parsers work in place all the time. Aliasing means such a caller silently
		// rewrites the file it just read, with no write call anywhere. A network backend
		// never has this problem because every read materializes a new buffer from the
		// response, so the hazard is unique to serving bytes out of memory.
		//
		// `subarray` is a view, not a copy, so the ranged read needs the same treatment.
		async readFile(ctx, path): Promise<Uint8Array> {
			const node = mustFile(path, ctx);
			node.atimeMs = now();
			return new Uint8Array(await bytesOf(node));
		},

		async readRange(ctx, path, offset, length): Promise<Uint8Array> {
			const node = mustFile(path, ctx);
			return new Uint8Array(
				(await bytesOf(node)).subarray(offset, offset + length)
			);
		},

		async openRead(ctx, path, range): Promise<ProviderStream> {
			return streamOfBytes(await bytesOf(mustFile(path, ctx)), range);
		},

		async writeFile(ctx, path, data): Promise<void> {
			const { dir, name: base } = parentOf(path, ctx);
			const existing = dir.children.get(base);
			if (existing && existing.kind === "dir") throw fsError("EISDIR", ctx);
			// A copy: what arrives is a view into the request frame, which is not ours.
			const copy = new Uint8Array(data);
			if (existing) {
				existing.content = copy;
				existing.size = copy.length;
				existing.mtimeMs = now();
			} else {
				dir.children.set(base, newFile(copy));
				dir.mtimeMs = now();
			}
			// Millisecond resolution, unlike puterfs's one-second timestamps — so a
			// watcher can tell two writes in the same second apart.
			events.write(full(path));
		},

		async mkdir(ctx, path, o): Promise<string | undefined> {
			if (o.recursive) {
				mkdirp(path);
				events.add(full(path), true);
				return undefined;
			}
			const { dir, name: base } = parentOf(path, ctx);
			if (dir.children.has(base)) throw fsError("EEXIST", ctx);
			dir.children.set(base, newDir());
			dir.mtimeMs = now();
			events.add(full(path), true);
			return undefined;
		},

		async rm(ctx, path, o): Promise<void> {
			const parts = segments(path);
			if (parts.length === 0) throw fsError("EPERM", ctx);
			const parent = lookup("/" + parts.slice(0, -1).join("/"));
			if (!parent || parent.kind !== "dir") {
				if (o.force) return;
				throw fsError("ENOENT", ctx);
			}
			const base = parts[parts.length - 1];
			const node = parent.children.get(base);
			if (!node) {
				if (o.force) return;
				throw fsError("ENOENT", ctx);
			}
			if (node.kind === "dir" && node.children.size > 0 && !o.recursive) {
				throw fsError("ENOTEMPTY", ctx);
			}
			// Detach but leave the node intact: an open handle keeps reading it, and its
			// flush is suppressed rather than resurrecting the file.
			if (node.kind === "file") node.unlinked = true;
			parent.children.delete(base);
			parent.mtimeMs = now();
			events.remove(full(path), node.kind === "dir");
		},

		async rename(ctx, from, to): Promise<void> {
			const src = parentOf(from, ctx);
			const node = src.dir.children.get(src.name);
			if (!node) throw fsError("ENOENT", ctx);
			const dst = parentOf(to, ctx);

			// `rename(2)` replaces the destination, but only where that loses nothing. Without
			// these checks a directory renamed over a *non-empty* directory silently discarded the
			// whole subtree underneath it — which is data loss dressed as a successful call, and
			// what node reports instead is ENOTEMPTY.
			const existing = dst.dir.children.get(dst.name);
			if (existing && existing !== node) {
				const intoDir = existing.kind === "dir";
				if (node.kind === "dir" && !intoDir) throw fsError("ENOTDIR", ctx);
				if (node.kind !== "dir" && intoDir) throw fsError("EISDIR", ctx);
				if (intoDir && existing.children.size > 0) {
					throw fsError("ENOTEMPTY", ctx);
				}
			}

			src.dir.children.delete(src.name);
			dst.dir.children.set(dst.name, node);
			src.dir.mtimeMs = now();
			dst.dir.mtimeMs = now();
			events.move(full(from), full(to), node.kind === "dir");
		},

		async copyFile(ctx, from, to, o): Promise<void> {
			const node = mustFile(from, ctx);
			const dst = parentOf(to, ctx);
			if (dst.dir.children.has(dst.name) && !o.overwrite) {
				throw fsError("EEXIST", ctx);
			}
			dst.dir.children.set(
				dst.name,
				newFile(new Uint8Array(await bytesOf(node)))
			);
			dst.dir.mtimeMs = now();
			events.add(full(to));
		},

		// Real timestamps, exactly as asked — nothing here is limited to "now" the way
		// puterfs's `/touch` is.
		async utimes(ctx, path, atimeMs, mtimeMs): Promise<boolean> {
			const node = mustFind(path, ctx);
			node.atimeMs = atimeMs;
			node.mtimeMs = mtimeMs;
			events.write(full(path));
			return true;
		},
	};

	return provider;
}
