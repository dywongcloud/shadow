// The mount table: which backend serves which subtree.
//
// Deliberately flat — one provider per root, longest matching prefix wins. Layering two
// backends over the same subtree is a *provider* concern (./union.ts), not a table
// concern, which keeps the lookup trivially correct and the layering independently
// testable.
//
// A leaf module: it knows nothing about any concrete provider, so providers can depend
// on the table's types without the table depending on them.
//
// One table per session, not one per page. Each `NodeWorker` has had its own `/tmp`, its
// own overlay and its own memory mounts for as long as those have existed — a single
// shared namespace would silently start sharing all three, which is wrong for injected
// modules and for a consumer that mounts a project per worker and treats each as a
// private replica.

import { checkRoot, dirname, toLocal, under } from "../../vfs/path";
import type { MountSnapshot } from "../../wire/fs";
import type { VfsProvider } from "../../vfs/provider";

export interface Mount {
	/** Absolute, normalized, no trailing slash — except "/" itself. */
	readonly root: string;
	readonly provider: VfsProvider;
	/** Rejects every mutation with EROFS. */
	readonly readOnly: boolean;
	/** Stands in as the mtime of a synthesized mount-point directory entry. */
	readonly createdMs: number;
}

export interface Resolved {
	readonly mount: Mount;
	/** Mount-relative. The mount point itself is "/". */
	readonly local: string;
	/** The absolute path the caller asked about, for error reporting. */
	readonly full: string;
}

export class MountTable {
	/**
	 * Sorted by root length descending, so the first match is the longest one. A linear
	 * scan over a handful of mounts beats any cleverer structure.
	 */
	#mounts: Mount[] = [];
	#onChange: (() => void) | undefined;

	/** Notified whenever the table changes, so the worker's snapshot can be re-pushed. */
	onChange(fn: () => void) {
		this.#onChange = fn;
	}

	mount(
		root: string,
		provider: VfsProvider,
		opts: { readOnly?: boolean } = {}
	): Mount {
		const checked = checkRoot(root);
		if (this.#mounts.some((m) => m.root === checked)) {
			throw new Error(`already mounted: ${checked}`);
		}
		const entry: Mount = {
			root: checked,
			provider,
			readOnly: !!opts.readOnly,
			createdMs: Date.now(),
		};
		this.#mounts.push(entry);
		this.#mounts.sort((a, b) => b.root.length - a.root.length);
		this.#onChange?.();
		return entry;
	}

	unmount(root: string): boolean {
		const checked = checkRoot(root);
		if (checked === "/") throw new Error("cannot unmount /");
		const before = this.#mounts.length;
		this.#mounts = this.#mounts.filter((m) => m.root !== checked);
		const changed = this.#mounts.length !== before;
		if (changed) this.#onChange?.();
		return changed;
	}

	resolve(path: string): Resolved {
		for (const mount of this.#mounts) {
			if (under(mount.root, path)) {
				return { mount, local: toLocal(mount.root, path), full: path };
			}
		}
		// Unreachable in practice: "/" is mounted at construction and matches everything.
		throw new Error(`no mount serves ${path}`);
	}

	/** Mounts rooted *directly* beneath `dir` — the ones a listing of `dir` must show. */
	childMounts(dir: string): Mount[] {
		return this.#mounts.filter(
			(m) => m.root !== "/" && dirname(m.root) === dir
		);
	}

	/** Mounts rooted strictly below `dir`, at any depth — for a recursive listing. */
	mountsUnder(dir: string): Mount[] {
		return this.#mounts.filter((m) => m.root !== dir && under(dir, m.root));
	}

	/**
	 * Whether `path` is a mount point, or an ancestor of one.
	 *
	 * A caching layer needs this before it may answer ENOENT from a listing: a
	 * directory's children as reported by *one* provider do not include the mounts
	 * grafted beneath it, so "absent from the listing" is not "absent from the
	 * filesystem" for these paths.
	 */
	isMountPathOrAncestor(path: string): boolean {
		return this.#mounts.some(
			(m) => m.root !== "/" && (m.root === path || under(path, m.root))
		);
	}

	list(): readonly Mount[] {
		return this.#mounts;
	}

	/**
	 * What the worker is told.
	 *
	 * The capability flags come from which optional methods a provider actually
	 * implements, so they cannot drift from the truth — and `hasNativeRange` has to be
	 * honest in both directions. Over-claiming is quadratic: a derived ranged read slices
	 * a whole-file read, so a positioned-read loop over a "native" range that isn't one
	 * re-reads the entire file per chunk. Under-claiming merely buffers it once.
	 */
	snapshot(): MountSnapshot[] {
		return this.#mounts.map((m) => ({
			root: m.root,
			name: m.provider.name,
			readOnly: m.readOnly,
			createdMs: m.createdMs,
			hasNativeRange: !!m.provider.readRange,
			canStream: !!m.provider.openRead,
			hasCopyFile: !!m.provider.copyFile,
			hasStatfs: !!m.provider.statfs,
		}));
	}
}
