import nodeBuffer from "../buffer";
import nodePath from "../path";
import { bigintDivideAway, type FsEntry } from "./util";

type NodeFs = typeof import("node:fs");

let Buffer = nodeBuffer.Buffer;

/**
 * A stable, distinct inode for an entry.
 *
 * Reporting 0 for everything was not the harmless placeholder it looked like. `(dev, ino)` is how
 * callers establish *identity*, and collapsing it makes every file look like the same file.
 * ripgrep's `--follow` uses the pair to detect symlink loops, so with every directory claiming inode
 * 0 the first subdirectory read as already-visited and a recursive walk stopped at the top level —
 * exit 0, no error, just none of the nested files. `tar`, `cp -al`, rsync and anything collapsing
 * hardlinks compare the same pair.
 *
 * puterfs already gives each entry a real uid, which *is* an identity, so prefer it; hashing the
 * path covers the backends that have none (OPFS, memory). Either way the same entry hashes to the
 * same value on every call, so a genuine revisit still compares equal — which is the half of loop
 * detection that has to keep working.
 *
 * FNV-1a, truncated to 32 bits so the result stays a safe integer. Collisions are possible in
 * principle; two paths would have to collide *and* be walked in the same traversal to matter, which
 * is a far smaller risk than the guaranteed collision this replaced.
 */
function inodeFor(entry: FsEntry): number {
	const key = entry.uid || entry.path;
	let hash = 0x811c9dc5;
	for (let i = 0; i < key.length; i++) {
		hash ^= key.charCodeAt(i);
		hash = Math.imul(hash, 0x01000193) >>> 0;
	}
	// Leave 0 free: it means "no inode" to callers, and it is what this replaced.
	return hash === 0 ? 1 : hash;
}

// node's typings declare these four classes with `private constructor()`. We
// can't satisfy that nominally, so each export is typed as
// `NodeFs[X] & { new(...args): any }`: instance shape and statics flow
// through from node's typing (`Pick<T, keyof T>` is `T`), and the extra
// constructor signature carries our internal puter-shaped construction.
export let StatsFs: Pick<NodeFs["StatsFs"], keyof NodeFs["StatsFs"]> & {
	new (puterStats: any, bigint: boolean): any;
} = class StatsFs<T extends number | bigint = number> {
	#bigint: boolean;
	#used: T;
	#total: T;

	constructor(puterStats: any, bigint: boolean) {
		this.#bigint = bigint;
		if (bigint) {
			this.#used = BigInt(puterStats.used) as any;
			this.#total = BigInt(puterStats.capacity) as any;
		} else {
			this.#used = puterStats.used as any;
			this.#total = puterStats.capacity as any;
		}
	}

	// @internal
	get _avail(): T {
		return (this.#total - this.#used) as any;
	}

	get type(): T {
		// fusefs_super_magic
		return this.#bigint ? (0x65735546n as any) : (0x65735546 as any);
	}

	get bsize(): T {
		return this.#bigint ? (4096n as any) : (4096 as any);
	}
	get blocks(): T {
		if (this.#bigint) {
			return bigintDivideAway(this.#total as any, this.bsize as any) as any;
		} else {
			return Math.ceil(this.#total / this.bsize) as any;
		}
	}
	get bfree(): T {
		if (this.#bigint) {
			return bigintDivideAway(this._avail as any, this.bsize as any) as any;
		} else {
			return Math.ceil(this._avail / this.bsize) as any;
		}
	}
	get bavail(): T {
		return this.bfree;
	}

	get files(): T {
		return this.#bigint ? (1024n as any) : (1024 as any);
	}
	get ffree(): T {
		return this.#bigint ? (1024n as any) : (1024 as any);
	}
};

export let Stats: Pick<NodeFs["Stats"], keyof NodeFs["Stats"]> & {
	new (entry: FsEntry, bigint: boolean, exists?: boolean): any;
} = class Stats<T extends number | bigint = number> {
	#bigint: boolean;
	#size: T;
	#ctime: T;
	#mtime: T;
	#atime: T;
	#isSymlink: boolean;
	#isDir: boolean;
	// `false` produces the all-zero Stats libuv yields when the stat itself
	// failed. `watchFile` hands one to its listeners for a path that doesn't
	// exist, and consumers rely on every field — not just the timestamps —
	// reading zero.
	#exists: boolean;

	#ino: T;

	constructor(entry: FsEntry, bigint: boolean, exists = true) {
		this.#isSymlink = entry.isSymlink;
		this.#isDir = entry.isDir;
		this.#exists = exists;

		const ino = exists ? inodeFor(entry) : 0;
		this.#ino = (bigint ? BigInt(ino) : ino) as any;

		this.#bigint = bigint;
		if (bigint) {
			this.#ctime = BigInt(entry.createdMs) as any;
			this.#mtime = BigInt(entry.modifiedMs) as any;
			this.#atime = BigInt(entry.accessedMs) as any;
			this.#size = BigInt(entry.size) as any;
		} else {
			this.#ctime = entry.createdMs as any;
			this.#mtime = entry.modifiedMs as any;
			this.#atime = entry.accessedMs as any;
			this.#size = entry.size as any;
		}
	}

	isFile() {
		return this.#exists && !this.#isDir;
	}
	isDirectory() {
		return this.#exists && this.#isDir;
	}
	isBlockDevice() {
		return false;
	}
	isCharacterDevice() {
		return false;
	}
	isFIFO() {
		return false;
	}
	isSocket() {
		return false;
	}
	isSymbolicLink() {
		return this.#exists && this.#isSymlink;
	}

	get dev(): T {
		// One device, so identity lives entirely in `ino` — but not zero, because a (0, 0) pair is
		// what libuv reports for a failed stat and some callers test the pair rather than the errno.
		if (!this.#exists) return this.#bigint ? (0n as any) : (0 as any);
		return this.#bigint ? (1n as any) : (1 as any);
	}
	get ino(): T {
		return this.#ino;
	}
	// The file-type bits matter: `stats.mode & S_IFMT` is how tar, fs-extra and
	// friends classify an entry, and a bare 0o777 makes every one of them read as
	// a character device. puterfs has no permission bits, so the low nine stay
	// wide open.
	get mode(): T {
		if (!this.#exists) return this.#bigint ? (0n as any) : (0 as any);
		let type = this.#isDir ? 0o040000 : 0o100000;
		return this.#bigint
			? ((BigInt(type) | 0o777n) as any)
			: ((type | 0o777) as any);
	}
	get nlink(): T {
		// A directory's link count is at least 2 (itself plus "."); puterfs has
		// no hardlinks, so a file is always 1.
		let links = this.#exists ? (this.#isDir ? 2 : 1) : 0;
		return this.#bigint ? (BigInt(links) as any) : (links as any);
	}
	get uid(): T {
		return this.#bigint ? (0n as any) : (0 as any);
	}
	get gid(): T {
		return this.#bigint ? (0n as any) : (0 as any);
	}
	get rdev(): T {
		return this.#bigint ? (0n as any) : (0 as any);
	}
	get size(): T {
		return this.#size;
	}
	get blksize(): T {
		if (!this.#exists) return this.#bigint ? (0n as any) : (0 as any);
		return this.#bigint ? (4096n as any) : (4096 as any);
	}
	get blocks(): T {
		if (!this.#exists) return this.#bigint ? (0n as any) : (0 as any);
		if (this.#bigint) {
			return bigintDivideAway(this.size as any, this.blksize as any) as any;
		} else {
			return Math.ceil(this.size / this.blksize) as any;
		}
	}

	get atimeMs(): T {
		return this.#atime;
	}
	get atimeNs(): T {
		return (this.#atime * ((this.#bigint ? 1000000n : 1000000) as any)) as any;
	}
	get ctimeMs(): T {
		return this.#ctime;
	}
	get ctimeNs(): T {
		return (this.#ctime * ((this.#bigint ? 1000000n : 1000000) as any)) as any;
	}
	get birthtimeMs(): T {
		return this.#ctime;
	}
	get birthtimeNs(): T {
		return (this.#ctime * ((this.#bigint ? 1000000n : 1000000) as any)) as any;
	}
	get mtimeMs(): T {
		return this.#mtime;
	}
	get mtimeNs(): T {
		return (this.#mtime * ((this.#bigint ? 1000000n : 1000000) as any)) as any;
	}

	get atime(): Date {
		return new Date(+("" + this.atimeMs));
	}
	get ctime(): Date {
		return new Date(+("" + this.ctimeMs));
	}
	get mtime(): Date {
		return new Date(+("" + this.mtimeMs));
	}
	get birthtime(): Date {
		return new Date(+("" + this.birthtimeMs));
	}
};

export let Dirent: Pick<NodeFs["Dirent"], keyof NodeFs["Dirent"]> & {
	new (name: string | Buffer, entry: FsEntry): any;
} = class Dirent {
	#isDir: boolean;
	#isSymlink: boolean;
	#name: string | Buffer;
	#parentPath: string;

	constructor(name: string | Buffer, entry: FsEntry) {
		this.#isSymlink = entry.isSymlink;
		this.#isDir = entry.isDir;
		this.#name = name;
		// node's `parentPath` is the *full* path of the containing directory
		// (node_core/lib/fs.js `handleDirents`), not just its base name.
		this.#parentPath = nodePath.dirname(entry.path);
	}

	isFile() {
		return !this.#isDir;
	}
	isDirectory() {
		return this.#isDir;
	}
	isBlockDevice() {
		return false;
	}
	isCharacterDevice() {
		return false;
	}
	isFIFO() {
		return false;
	}
	isSocket() {
		return false;
	}
	isSymbolicLink() {
		return this.#isSymlink;
	}

	get name() {
		return this.#name;
	}
	get parentPath() {
		return this.#parentPath;
	}
};

export let Dir: Pick<NodeFs["Dir"], keyof NodeFs["Dir"]> & {
	new (path: string, entries: InstanceType<typeof Dirent>[]): any;
} = class Dir {
	#path: string;
	#entries: InstanceType<typeof Dirent>[];
	#index: number;
	#closed: boolean;

	constructor(path: string, entries: InstanceType<typeof Dirent>[]) {
		this.#path = path;
		this.#entries = entries;
		this.#index = 0;
		this.#closed = false;
	}

	get path(): string {
		return this.#path;
	}

	readSync(): InstanceType<typeof Dirent> | null {
		if (this.#closed) throw new Error("Directory handle was closed");
		if (this.#index >= this.#entries.length) return null;
		return this.#entries[this.#index++];
	}

	async read(): Promise<InstanceType<typeof Dirent> | null> {
		return this.readSync();
	}

	closeSync(): void {
		if (this.#closed) throw new Error("Directory handle was closed");
		this.#closed = true;
	}

	async close(): Promise<void> {
		this.closeSync();
	}

	async *[Symbol.asyncIterator](): AsyncGenerator<
		InstanceType<typeof Dirent>,
		undefined
	> {
		let entry;
		while ((entry = this.readSync()) !== null) {
			yield entry;
		}
		if (!this.#closed) this.closeSync();
		return undefined;
	}

	async [Symbol.asyncDispose](): Promise<void> {
		if (!this.#closed) await this.close();
	}

	[Symbol.dispose](): void {
		if (!this.#closed) this.closeSync();
	}
};
