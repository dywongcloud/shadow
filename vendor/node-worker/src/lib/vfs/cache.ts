// A read cache in front of a provider, invalidated from outside.
//
// The resolver has had a cache like this since the beginning
// (worker/module/resolve.ts), and its docblock names the exact reason it was
// never allowed out of that module: serving `fs.statSync` generally from a cache
// "would go stale the moment another puter app writes to a path, and we have no
// way to hear about that". We do now — ../fsevents.ts speaks the socket feed
// puterfs already publishes, and falls back to the change counter when the
// socket cannot authenticate. So this is that cache, generalized: one store
// serving every operation, with invalidation arriving from outside instead of
// being something each caller has to remember.
//
// ## A tree, not four maps
//
// The resolver kept `statCache`, `readFileCache`, `completeDirs` and a set of
// negatives, all keyed by path. Four flat maps mean a subtree invalidation —
// which is what "a directory was removed" and "a directory was renamed" both
// are — costs a linear sweep of everything cached. Here they are one tree, the
// way memory.ts is a tree, so dropping a subtree is dropping a node.
//
// The four collapse cleanly: a node's `entry` is the stat, a directory's `depth`
// is how far its `children` are known to be exhaustive, a file's `bytes` are the
// contents, and a `missing` node is a negative. `depth` is the load-bearing one
// — it is what turns a miss under a fully-listed directory into a local ENOENT,
// which is where most of the traffic in a module resolve or a `find` actually
// goes.
//
// ## Two kinds of invalidation, and why the coarse one is not a flush
//
// `applyEvent` is the precise kind: a path, a kind, and often a uid. It drops
// exactly what changed.
//
// `markStale` is the coarse kind — "something under this user changed and you
// were not told what", which is all the change-counter fallback can say. The
// obvious response is to throw everything away, and it is the wrong one: the
// counter is bumped by *our own* writes too, and nothing distinguishes ours from
// anyone else's, so a flush would mean every write emptied the cache. For an
// agent that edits a file and then greps the tree, that is the whole workload.
//
// So a stale mark bumps a generation instead. Nothing is discarded; everything
// merely stops being *believed*. The next read of an unbelieved path re-lists its
// parent directory — one request — and every child whose identity is unchanged is
// believed again with its bytes intact. A grep after a one-file edit costs one
// listing per directory it walks, not a re-download of the tree.
//
// Identity is `(uid, size, modifiedMs)`, and puterfs timestamps have one-second
// resolution — so two writes inside the same second, to the same length, are
// indistinguishable here. The socket path does not have this problem because it
// names the path that changed. It is the price of the fallback, and the same one
// `handles.ts` already documents for its own buffer revalidation.
//
// ## What this deliberately does not do
//
// It does not know about the mount table, per the provider invariants: the facade
// resolves a path to its mount before a provider is ever called, so a negative
// derived here can never contradict a mount grafted somewhere below — those paths
// are answered by that mount and never reach this file at all.

import { fsError } from "../../vfs/errno";
import { dirname, relDepth, toLocal, under } from "../../vfs/path";
import { streamOfBytes } from "./stream";
import type { FsEntry, Listing, ReaddirOpts, WireCtx } from "../../vfs/entry";
import type { ProviderStream, VfsProvider } from "../../vfs/provider";
import type { PuterFsEvent } from "../../wire/events";

/** Bytes of file content held, across all files. */
const DEFAULT_MAX_BYTES = 64 * 1024 * 1024;
/**
 * Largest single file whose contents are kept.
 *
 * A cap per file as well as in total, because without it one large read evicts
 * everything else to hold something nothing is likely to read twice — and the
 * files an agent reads repeatedly are source files, which are small.
 */
const DEFAULT_MAX_FILE_BYTES = 8 * 1024 * 1024;
/**
 * How far behind the feed this cache will let itself be before it stops
 * answering from itself.
 *
 * Only reachable when the socket is down, since a live socket reports
 * `freshAsOf()` as *now*. Comfortably above the poll interval in ../fsevents.ts
 * so the steady state costs no extra request: the periodic poll keeps this
 * satisfied on its own, and `ensureFresh` only fires when a poll has failed or
 * has not happened yet.
 */
const DEFAULT_MAX_STALE_MS = 3000;

/** `statfs` is a number about the whole account, which no `item.*` event reports. */
const STATFS_TTL_MS = 3000;

/** `depth` for a listing that ran to the bottom of the tree. */
const UNBOUNDED = Infinity;

/**
 * How far a seeded listing reaches.
 *
 * Counted from the *parent* of the directory that missed — see `seedSubtree` — so 4 settles
 * that directory and three more levels below it. A walk then re-seeds every `depth - 1` levels,
 * which is what makes the cost of a walk the number of directories sitting at those *stride*
 * levels, and why this is not monotonic: a deeper seed strides further but its next frontier
 * lands on a deeper, more numerous level. Measured over five random trees of each shape, walked
 * to the bottom, counting requests to the backend:
 *
 *  | depth | ≤4 levels | ≤6 levels | ≤9 levels |
 *  |-------|-----------|-----------|-----------|
 *  | off   |       232 |       618 |       445 |
 *  | 2     |        91 |       252 |       231 |
 *  | 3     |        31 |        86 |       148 |
 *  | **4** |    **59** |    **38** |    **52** |
 *  | 5     |         9 |        70 |       106 |
 *  | 6     |         9 |       138 |        30 |
 *
 * 4 is the only value that is close to the best on every shape rather than excellent on one and
 * mediocre on the next, and it moves the fewest extra entries of the three that come close
 * (+7% to +32% over the minimum, against a 4×–16× cut in requests). Nothing here is a claim
 * about a particular tree, unlike the resolver's `SEED_DEPTH`, which is derived from the layout
 * of node_modules — and getting it wrong costs only speed.
 */
const DEFAULT_PREFETCH_DEPTH = 4;
/**
 * Shallower than this is not seeding.
 *
 * A depth-1 listing of the parent re-asks the exact question that was already answered — the
 * parent's own listing is why we know this is a descent at all — so it settles nothing and the
 * walk still pays per directory. Measured, it is *worse* than not seeding: 161 requests against
 * 121, one wasted round trip per directory.
 */
const MIN_PREFETCH_DEPTH = 2;
/**
 * Ceiling on one seeded listing. Overrunning it is not an error — the listing is a valid prefix
 * and `ingest` refuses to record a depth for it — so this only bounds what one guess about a
 * walk can cost. Same number the resolver uses for the same reason.
 */
const DEFAULT_PREFETCH_MAX_ENTRIES = 20000;

export interface VfsCacheOptions {
	/** Off entirely; every call passes straight through. */
	enabled?: boolean;
	/** Bytes of file content to hold. **0 caches metadata only.** */
	maxBytes?: number;
	/** Files larger than this are never held. */
	maxFileBytes?: number;
	/** Tolerated window of unreported change. See {@link DEFAULT_MAX_STALE_MS}. */
	maxStaleMs?: number;
	/**
	 * Where the wrapped provider is mounted, so the absolute paths arriving on
	 * `applyEvent` can be re-rooted onto the mount-local ones a provider speaks.
	 * Empty or "/" for the root mount.
	 */
	prefix?: string;
	/** How this cache learns it may be behind. Absent ⇒ it trusts itself forever. */
	freshness?: VfsCacheFreshness;
	/**
	 * Turn a tree walk into subtree listings instead of one listing per directory. See
	 * `seedSubtree`.
	 *
	 * **Off unless asked for**, because whether it pays is a property of the provider rather
	 * than of the cache: it trades bytes for round trips, which is the right trade over a
	 * network and a pure loss over a mount that is already local.
	 */
	prefetch?: boolean | { depth?: number; maxEntries?: number };
}

/**
 * The half of the event feed that cannot be pushed.
 *
 * Whether a cached answer is still good is a question about *now*, so it has to
 * be asked rather than waited for. Satisfied by `FsEventsSubscription`.
 */
export interface VfsCacheFreshness {
	freshAsOf(): number;
	ensureFresh(maxAgeMs: number): Promise<void>;
}

export interface CachingProvider extends VfsProvider {
	/** One precise mutation, from the socket or anywhere else that knows. */
	applyEvent(event: PuterFsEvent): void;
	/** "Something changed and you were not told what." See the header. */
	markStale(): void;
	flush(): void;
	stats(): Record<string, number>;
}

// ------------------------------------------------------------------ the store

interface DirNode {
	kind: "dir";
	entry?: FsEntry;
	children: Map<string, CacheNode>;
	/**
	 * How many levels of `children` are known to be the complete set. 0 means
	 * nothing is known and a miss here proves nothing.
	 */
	depth: number;
	/**
	 * The generation in which this directory's own `readdir` was answered.
	 *
	 * Read by `seedSubtree`, which needs to know whether a miss is a *descent* —
	 * the only evidence available here that a walk is under way rather than
	 * somebody having run `ls` once.
	 */
	askedGen?: number;
	/** The generation in which this directory's subtree was seeded. */
	seededGen?: number;
	gen: number;
}

interface FileNode {
	kind: "file";
	entry?: FsEntry;
	bytes?: Uint8Array;
	gen: number;
}

/** A path known *not* to exist, and the errno that says so. */
interface MissingNode {
	kind: "missing";
	code: "ENOENT" | "ENOTDIR";
	gen: number;
}

type CacheNode = DirNode | FileNode | MissingNode;

function segments(path: string): string[] {
	return path.split("/").filter(Boolean);
}

function nameOf(path: string): string | undefined {
	const segs = segments(path);
	return segs.length === 0 ? undefined : segs[segs.length - 1];
}

function childPath(dir: string, name: string): string {
	return dir === "/" ? `/${name}` : `${dir}/${name}`;
}

/** Whether two entries describe the same file, unchanged. */
function sameFile(a: FsEntry | undefined, b: FsEntry): boolean {
	return (
		!!a &&
		a.uid === b.uid &&
		a.size === b.size &&
		a.modifiedMs === b.modifiedMs &&
		a.isDir === b.isDir
	);
}

function missingCode(err: unknown): "ENOENT" | "ENOTDIR" | undefined {
	const code = (err as NodeJS.ErrnoException)?.code;
	return code === "ENOENT" || code === "ENOTDIR" ? code : undefined;
}

/**
 * The wrapper, when it is turned off.
 *
 * `enabled: false` still has to produce something with the control methods on
 * it, so the wiring above does not have to branch — and it must not simply hand
 * back `inner`, since attaching no-ops to a provider someone else also holds
 * would give them methods they never asked for. A pass-through mirrors the
 * optional halves for the same reason the real one does: the mount snapshot
 * reads capability off which of them exist.
 */
function passthrough(inner: VfsProvider): CachingProvider {
	const provider: VfsProvider & Partial<CachingProvider> = {
		name: inner.name,
		stat: (ctx, path) => inner.stat(ctx, path),
		readdir: (ctx, path, o) => inner.readdir(ctx, path, o),
		readFile: (ctx, path) => inner.readFile(ctx, path),
		writeFile: (ctx, path, data) => inner.writeFile(ctx, path, data),
		mkdir: (ctx, path, o) => inner.mkdir(ctx, path, o),
		rm: (ctx, path, o) => inner.rm(ctx, path, o),
		rename: (ctx, from, to) => inner.rename(ctx, from, to),
		utimes: (ctx, path, a, m) => inner.utimes(ctx, path, a, m),
		applyEvent: () => {},
		markStale: () => {},
		flush: () => {},
		stats: () => ({}),
	};
	if (inner.readRange) {
		provider.readRange = (ctx, p, off, len) =>
			inner.readRange!(ctx, p, off, len);
	}
	if (inner.openRead) {
		provider.openRead = (ctx, p, range) => inner.openRead!(ctx, p, range);
	}
	if (inner.copyFile) {
		provider.copyFile = (ctx, from, to, o) => inner.copyFile!(ctx, from, to, o);
	}
	if (inner.statfs) provider.statfs = (ctx) => inner.statfs!(ctx);
	return provider as CachingProvider;
}

export function createCachingProvider(
	inner: VfsProvider,
	opts: VfsCacheOptions = {}
): CachingProvider {
	if (opts.enabled === false) return passthrough(inner);

	const maxBytes = opts.maxBytes ?? DEFAULT_MAX_BYTES;
	const maxFileBytes = opts.maxFileBytes ?? DEFAULT_MAX_FILE_BYTES;
	const maxStaleMs = opts.maxStaleMs ?? DEFAULT_MAX_STALE_MS;
	const prefix = opts.prefix ? opts.prefix : "/";
	const freshness = opts.freshness;
	const prefetch = !opts.prefetch
		? undefined
		: {
				depth: Math.max(
					MIN_PREFETCH_DEPTH,
					(opts.prefetch === true ? undefined : opts.prefetch.depth) ??
						DEFAULT_PREFETCH_DEPTH
				),
				maxEntries:
					(opts.prefetch === true ? undefined : opts.prefetch.maxEntries) ??
					DEFAULT_PREFETCH_MAX_ENTRIES,
			};

	const counts = new Map<string, number>();
	const bump = (key: string) => counts.set(key, (counts.get(key) ?? 0) + 1);

	let gen = 1;
	let root: DirNode = { kind: "dir", children: new Map(), depth: 0, gen };
	/**
	 * Files holding bytes, least-recently-used first — a `Set` because insertion
	 * order *is* the ordering, and re-inserting is how a hit moves to the back.
	 */
	let lru = new Set<FileNode>();
	let held = 0;
	/**
	 * uid → the path we have it cached at.
	 *
	 * The api answers `POST /rename` with `item.updated` naming only the *new*
	 * path — no old one — so a rename performed by another client would otherwise
	 * leave a live entry at a name that no longer exists. The uid is the only
	 * thing tying the two together.
	 */
	let byUid = new Map<string, string>();
	let statfsAt = 0;
	let statfsValue: { used: number; capacity: number } | undefined;

	/** In-flight reads, so concurrent identical ones cost one request. */
	const inflight = new Map<string, Promise<unknown>>();

	function once<T>(key: string, run: () => Promise<T>): Promise<T> {
		const existing = inflight.get(key) as Promise<T> | undefined;
		if (existing) {
			bump("coalesced");
			return existing;
		}
		const started = run().finally(() => inflight.delete(key));
		inflight.set(key, started);
		return started;
	}

	// ------------------------------------------------------------- walking

	interface Walk {
		/** The node at the path, if the tree has one. */
		node?: CacheNode;
		/** The directory the walk fell out of, when it did. */
		missingUnder?: DirNode;
		/** A file found where a directory was needed. */
		blockedBy?: FileNode;
	}

	function walk(path: string): Walk {
		let current: CacheNode = root;
		for (const seg of segments(path)) {
			if (current.kind === "file") return { blockedBy: current };
			// A missing ancestor settles everything below it.
			if (current.kind === "missing") return { node: current };
			const next = current.children.get(seg);
			if (!next) return { missingUnder: current };
			current = next;
		}
		return { node: current };
	}

	function dirNodeAt(path: string): DirNode | undefined {
		const node = walk(path).node;
		return node?.kind === "dir" ? node : undefined;
	}

	function live(node: CacheNode): boolean {
		return node.gen === gen;
	}

	/**
	 * The directory node at `path`, creating the chain if it isn't there.
	 *
	 * Creating a child inside a directory clears that directory's completeness:
	 * it is a child the listing did not report, so whatever the listing claimed
	 * about the child set is no longer what the tree holds.
	 */
	function dirAt(path: string): DirNode {
		let current = root;
		for (const seg of segments(path)) {
			const next = current.children.get(seg);
			if (next?.kind === "dir") {
				current = next;
				continue;
			}
			if (next) forget(next);
			const created: DirNode = {
				kind: "dir",
				children: new Map(),
				depth: 0,
				gen,
			};
			current.children.set(seg, created);
			current.depth = 0;
			current = created;
		}
		return current;
	}

	// ------------------------------------------------------ accounting and drops

	function release(node: FileNode) {
		if (node.bytes) {
			held -= node.bytes.length;
			node.bytes = undefined;
		}
		lru.delete(node);
	}

	/** Un-account a node and everything below it, before it leaves the tree. */
	function forget(node: CacheNode) {
		if (node.kind === "missing") return;
		if (node.entry?.uid) byUid.delete(node.entry.uid);
		if (node.kind === "file") {
			release(node);
			return;
		}
		for (const child of node.children.values()) forget(child);
	}

	function evict() {
		// Deleting the current element of a `Set` mid-iteration is safe; the
		// entries not yet reached are unaffected.
		for (const node of lru) {
			if (held <= maxBytes) return;
			bump("evicted");
			release(node);
		}
	}

	function admit(node: FileNode, bytes: Uint8Array) {
		if (maxBytes <= 0 || bytes.length > maxFileBytes) return;
		release(node);
		node.bytes = bytes;
		held += bytes.length;
		lru.add(node);
		evict();
	}

	function touch(node: FileNode) {
		// Re-inserting moves it to the back, which is the whole ordering trick.
		if (lru.delete(node)) lru.add(node);
	}

	// ------------------------------------------------------------ tree mutation

	/**
	 * Stop believing every ancestor of `path` is completely listed.
	 *
	 * A path appearing or disappearing changes their child sets, and only a fresh
	 * listing can say how.
	 *
	 * A seed is a completeness claim like any other, so it goes too: a subtree
	 * whose listing has been voided is one a later walk should be free to
	 * re-establish in a single request rather than a directory at a time.
	 */
	function unseal(path: string) {
		let current = root;
		current.depth = 0;
		current.seededGen = undefined;
		for (const seg of segments(dirname(path))) {
			const next = current.children.get(seg);
			if (next?.kind !== "dir") return;
			next.depth = 0;
			next.seededGen = undefined;
			current = next;
		}
	}

	/** Put something else at `path`, or nothing. Ancestor completeness untouched. */
	function replace(path: string, replacement?: CacheNode) {
		const name = nameOf(path);
		if (name === undefined) {
			flushTree();
			return;
		}
		const parentPath = dirname(path);
		const parent = replacement ? dirAt(parentPath) : dirNodeAt(parentPath);
		if (!parent) return;
		const existing = parent.children.get(name);
		if (existing) forget(existing);
		if (!replacement) {
			parent.children.delete(name);
			return;
		}
		parent.children.set(name, replacement);
		// `forget` above dropped the uid index entry, including for an identity the
		// replacement is carrying over from what it displaced.
		if (replacement.kind !== "missing" && replacement.entry?.uid) {
			byUid.set(replacement.entry.uid, path);
		}
	}

	/** Drop what we know about `path`, and what its ancestors claimed to know. */
	function invalidate(path: string, replacement?: CacheNode) {
		unseal(path);
		replace(path, replacement);
	}

	/** Everything below `path` goes; `path` itself stays if it is a directory. */
	function invalidateChildren(path: string) {
		const dir = dirNodeAt(path);
		if (!dir) {
			invalidate(path);
			return;
		}
		for (const child of dir.children.values()) forget(child);
		dir.children.clear();
		dir.depth = 0;
		dir.seededGen = undefined;
	}

	function flushTree() {
		root = { kind: "dir", children: new Map(), depth: 0, gen };
		lru = new Set();
		held = 0;
		byUid = new Map();
		statfsValue = undefined;
	}

	/**
	 * Record how far a directory's children are known, at the current generation.
	 *
	 * A depth recorded before a stale mark does not survive it: the whole point of
	 * a generation bump is that no completeness claim is believed until something
	 * re-establishes it, and `Math.max` against the old value would quietly
	 * resurrect a claim nothing verified.
	 */
	function seal(dir: DirNode, depth: number) {
		if (dir.gen !== gen) {
			dir.depth = 0;
			dir.gen = gen;
		}
		dir.depth = Math.max(dir.depth, depth);
	}

	/**
	 * Record one entry, reusing the node already there when it describes the same
	 * file — which is what lets a revalidating listing keep the bytes it already
	 * holds instead of re-reading every file to prove nothing changed.
	 */
	function put(path: string, entry: FsEntry): CacheNode {
		const name = nameOf(path);
		if (name === undefined) {
			// The provider's own root. It has no parent to be a child of.
			root.entry = entry;
			root.gen = gen;
			return root;
		}
		const parent = dirAt(dirname(path));
		const existing = parent.children.get(name);

		if (entry.isDir) {
			if (existing?.kind === "dir") {
				// Crossing a generation expires what it claimed about its children.
				if (existing.gen !== gen) existing.depth = 0;
				existing.entry = entry;
				existing.gen = gen;
				if (entry.uid) byUid.set(entry.uid, path);
				return existing;
			}
			if (existing) forget(existing);
			const node: DirNode = {
				kind: "dir",
				entry,
				children: new Map(),
				depth: 0,
				gen,
			};
			parent.children.set(name, node);
			if (entry.uid) byUid.set(entry.uid, path);
			return node;
		}

		if (existing?.kind === "file") {
			if (sameFile(existing.entry, entry)) touch(existing);
			else release(existing);
			existing.entry = entry;
			existing.gen = gen;
			if (entry.uid) byUid.set(entry.uid, path);
			return existing;
		}
		if (existing) forget(existing);
		const node: FileNode = { kind: "file", entry, gen };
		parent.children.set(name, node);
		if (entry.uid) byUid.set(entry.uid, path);
		return node;
	}

	/**
	 * Record that a path is not there.
	 *
	 * ENOENT only. ENOTDIR says an *ancestor* is a file without saying which, and
	 * writing a negative at the leaf would make `replace` build a directory chain
	 * through that file to hold it — replacing a real cached file with an empty
	 * directory, which is worse than knowing nothing. The derivation in `believe`
	 * still answers ENOTDIR from a file node it can actually see.
	 */
	function putMissing(path: string, code: "ENOENT" | "ENOTDIR") {
		if (code !== "ENOENT") return;
		if (nameOf(path) === undefined) return;
		replace(path, { kind: "missing", code, gen });
	}

	/**
	 * Fold a listing into the tree.
	 *
	 * `depth` may only be recorded when the listing ran to completion: a walk cut
	 * short by `maxEntries` is a valid prefix but not an exhaustive picture, and
	 * every negative answer this cache gives rests on the difference.
	 *
	 * The horizon rule is the subtle half, and it is the same one the resolver's
	 * `ingestListing` documents: a directory sitting *at* the requested depth came
	 * back, so it exists, but its own children were never asked for. Recording it
	 * as complete would invent ENOENTs for files that are really there.
	 */
	function ingest(path: string, depth: number, listing: Listing) {
		const seen = new Set<string>();
		for (const entry of listing.entries) {
			put(entry.path, entry);
			seen.add(entry.path);
		}
		if (!listing.complete) return;

		const dir = dirAt(path);
		seal(dir, depth);
		reconcile(dir, path, seen);

		for (const entry of listing.entries) {
			if (!entry.isDir) continue;
			const below = depth - relDepth(path, entry.path);
			if (below <= 0) continue;
			const node = dirNodeAt(entry.path);
			if (!node) continue;
			seal(node, below);
			reconcile(node, entry.path, seen);
		}
	}

	/**
	 * Drop children the listing did not mention.
	 *
	 * This is what heals a rename performed elsewhere even when no event named the
	 * old path: the first fresh listing of the directory simply does not contain it
	 * any more.
	 */
	function reconcile(dir: DirNode, dirPath: string, seen: Set<string>) {
		for (const [name, child] of [...dir.children]) {
			if (seen.has(childPath(dirPath, name))) continue;
			forget(child);
			dir.children.set(name, { kind: "missing", code: "ENOENT", gen });
		}
	}

	// -------------------------------------------------------------- revalidation

	/**
	 * Bring the node at `path` back into the current generation, cheaply.
	 *
	 * One non-recursive listing of the parent re-verifies every sibling at once,
	 * which is the point: after a coarse stale mark, a walk over a directory pays a
	 * single request rather than one per file. Only worth doing when the parent was
	 * listed before — otherwise there is no sibling set to amortize over and the
	 * caller's own point read is cheaper.
	 *
	 * Best-effort throughout: every failure here has a correct fallback one line
	 * later, which is the caller going to the backend itself.
	 */
	async function revalidate(ctx: WireCtx, path: string): Promise<void> {
		const parentPath = dirname(path);
		if (parentPath === path) return;
		const parent = dirNodeAt(parentPath);
		if (!parent || parent.depth === 0) return;
		bump("revalidated");
		try {
			await listThrough(ctx, parentPath, 1);
		} catch (err) {
			if (missingCode(err)) invalidate(parentPath);
		}
	}

	/** Whether the cache may answer from itself at all right now. */
	async function believable(): Promise<boolean> {
		if (!freshness) return true;
		await freshness.ensureFresh(maxStaleMs);
		return Date.now() - freshness.freshAsOf() <= maxStaleMs;
	}

	/**
	 * The node at `path`, in the current generation, without going to the backend.
	 *
	 * Undefined means the tree has nothing believable to say, which is the signal
	 * to fetch.
	 */
	async function believe(
		ctx: WireCtx,
		path: string,
		/**
		 * Whether an unbelieved node is worth a listing of its parent.
		 *
		 * False for `readdir`, where it is strictly a loss: a listing of the parent
		 * re-verifies this directory's *entry* but says nothing about its children,
		 * so the caller lists it anyway and has paid twice.
		 */
		mayRevalidate = true
	): Promise<CacheNode | undefined> {
		if (!(await believable())) return undefined;

		let found = walk(path);
		if (found.node && !live(found.node) && mayRevalidate) {
			await revalidate(ctx, path);
			found = walk(path);
		}
		if (found.node) return live(found.node) ? found.node : undefined;

		// Not in the tree at all. An ancestor may still prove it cannot be.
		if (found.blockedBy && live(found.blockedBy)) {
			return { kind: "missing", code: "ENOTDIR", gen };
		}
		const parent = found.missingUnder;
		if (parent && live(parent) && parent.depth >= 1) {
			return { kind: "missing", code: "ENOENT", gen };
		}
		return undefined;
	}

	// ------------------------------------------------------------- backend reads
	//
	// Each of these coalesces concurrent identical requests, and each re-raises a
	// "not there" answer against *this* caller's context rather than passing the
	// shared error along: a coalesced request would otherwise report the path
	// whoever arrived first had asked about, and `WireCtx.reportPath` exists
	// precisely so an error names the path the caller spelled.

	async function statThrough(ctx: WireCtx, path: string): Promise<FsEntry> {
		try {
			return await once(`stat\0${path}`, async () => {
				try {
					const entry = await inner.stat(ctx, path);
					put(path, entry);
					return entry;
				} catch (err) {
					const code = missingCode(err);
					if (code) putMissing(path, code);
					throw err;
				}
			});
		} catch (err) {
			const code = missingCode(err);
			throw code ? fsError(code, ctx) : err;
		}
	}

	async function listThrough(
		ctx: WireCtx,
		path: string,
		depth: number,
		readOpts?: ReaddirOpts
	): Promise<Listing> {
		// The budget is part of the key: two callers asking the same directory with
		// different `maxEntries` are asking different questions, and handing the
		// larger one a listing truncated for the smaller would be a short answer
		// reported as a whole one.
		const budget = readOpts?.maxEntries ?? "";
		try {
			return await once(`readdir\0${depth}\0${budget}\0${path}`, async () => {
				try {
					const result = await inner.readdir(ctx, path, readOpts);
					ingest(path, depth, result);
					return result;
				} catch (err) {
					const code = missingCode(err);
					if (code) putMissing(path, code);
					throw err;
				}
			});
		} catch (err) {
			const code = missingCode(err);
			throw code ? fsError(code, ctx) : err;
		}
	}

	/**
	 * Answer a walk's next directory by listing the whole subtree its parent sits on.
	 *
	 * A tree walk asks for one directory, descends into each of its subdirectories, and asks
	 * again — so over a network mount it pays a round trip per directory, and a `node_modules`
	 * with two thousand of them costs two thousand requests. That is what took a ripgrep over
	 * one workspace to ~1000 `GET /fs/readdir`, the last 44 of them answered with 429.
	 *
	 * Nothing about the *first* listing says a walk is happening, and a single `ls` must not
	 * drag a subtree over the wire. The signal is the second one: a miss inside a directory
	 * whose own listing this cache has already answered is a descent, and a descent is a walk.
	 * So the seed is rooted at the **parent** rather than at the path that missed, which is
	 * what makes it cover the siblings the walk is about to ask for too — one request for a
	 * directory with fifty subdirectories in it instead of fifty-one.
	 *
	 * This is the general form of what `../../worker/module/resolve.ts` does for node_modules.
	 * That one can seed on sight because it knows the shape of what it is looking at; here the
	 * only thing to go on is the descent, and every walk gets it rather than only the
	 * resolver's.
	 *
	 * Returns whether the seed landed, which is a statement about the request and not about
	 * `path`: the reply may show it complete, incomplete, or gone.
	 */
	async function seedSubtree(ctx: WireCtx, path: string): Promise<boolean> {
		if (!prefetch) return false;
		const parentPath = dirname(path);
		if (parentPath === path) return false;
		// Never the provider's own root. puterfs refuses a recursive listing there — it
		// would be a prefix scan over every user, see ./puter-readdir.ts — so the one
		// request this could make is a request that cannot succeed. The walk seeds one level
		// down instead, which costs it a single extra listing.
		if (parentPath === "/") return false;

		const parent = dirNodeAt(parentPath);
		// Not a descent. Nobody has listed the parent, so there is no reason to believe
		// anything else in this subtree is about to be asked for.
		if (!parent || parent.askedGen !== gen) return false;
		// Once per root, recorded *before* the request rather than after: an overrun or a
		// failure retried once per sibling is the one shape that makes this cost more than
		// no seeding at all.
		if (parent.seededGen === gen) return false;
		// Nothing to gain from filling a tree that is not currently allowed to answer.
		if (!(await believable())) return false;

		parent.seededGen = gen;
		bump("readdir.seed");
		try {
			await listThrough(ctx, parentPath, prefetch.depth, {
				recursive: true,
				depth: prefetch.depth,
				maxEntries: prefetch.maxEntries,
			});
			return true;
		} catch {
			// Best effort by construction: the caller's own listing is the next line and will
			// report whatever this ran into, against their context rather than this one.
			bump("readdir.seed.failed");
			return false;
		}
	}

	async function readThrough(ctx: WireCtx, path: string): Promise<Uint8Array> {
		try {
			return await once(`readFile\0${path}`, async () => {
				try {
					const bytes = await inner.readFile(ctx, path);
					if (maxBytes > 0 && bytes.length <= maxFileBytes) {
						// The backend allocated these, so they are ours to hold. A
						// *write's* buffer is not — see `writeFile`.
						const node: FileNode = { kind: "file", gen };
						const previous = walk(path).node;
						// Keep a stat only if it is still believed; a stale one would
						// pair fresh bytes with an identity nothing has verified.
						if (previous?.kind === "file" && live(previous)) {
							node.entry = previous.entry;
						}
						replace(path, node);
						admit(node, bytes);
					}
					return bytes;
				} catch (err) {
					const code = missingCode(err);
					if (code) putMissing(path, code);
					throw err;
				}
			});
		} catch (err) {
			const code = missingCode(err);
			throw code ? fsError(code, ctx) : err;
		}
	}

	/**
	 * Every entry at or below `dir`, down to `depth` levels.
	 *
	 * Undefined when the tree holds a child it cannot describe — a directory node
	 * created to hold something below it, with no stat of its own. Answering
	 * without it would be a `complete: true` listing that silently omits a real
	 * entry, which is the one way a cached listing can be actively wrong.
	 */
	function collect(dir: DirNode, depth: number): FsEntry[] | undefined {
		const out: FsEntry[] = [];
		const visit = (node: DirNode, left: number): boolean => {
			if (left <= 0) return true;
			for (const child of node.children.values()) {
				if (child.kind === "missing") continue;
				if (!child.entry) return false;
				out.push(child.entry);
				if (child.kind === "dir" && !visit(child, left - 1)) return false;
			}
			return true;
		};
		return visit(dir, depth) ? out : undefined;
	}

	/**
	 * A `readdir` answered from the tree, or undefined when the tree cannot answer it.
	 *
	 * Throws for a path that is not a directory — which is an answer, and one this can give
	 * without a request.
	 */
	function listFromTree(
		ctx: WireCtx,
		node: CacheNode | undefined,
		want: number,
		budget: number
	): Listing | undefined {
		if (node?.kind === "missing") throw fsError(node.code, ctx);
		if (node?.kind === "file") throw fsError("ENOTDIR", ctx);
		if (node?.kind !== "dir" || node.depth < want) return undefined;
		const entries = collect(node, want);
		if (!entries) return undefined;
		// Answering past the caller's budget would answer a different question: `complete` is
		// what tells them whether a negative may be derived from this, and a truncated listing
		// carries no such licence.
		return entries.length > budget
			? { entries: entries.slice(0, budget), complete: false }
			: { entries, complete: true };
	}

	/**
	 * Record that a directory's own listing was answered, and how deep a question it answered.
	 *
	 * `seedSubtree` reads the first of these to recognise a descent. A *recursive* answer also
	 * counts as having seeded that root, because it is the same request seeding would have
	 * made — without which the walk's first miss below it would immediately ask for it again.
	 */
	function markAnswered(path: string, want: number) {
		const dir = dirNodeAt(path);
		if (!dir) return;
		dir.askedGen = gen;
		if (want > 1) dir.seededGen = gen;
	}

	// ------------------------------------------------------------------- the ops

	const provider: VfsProvider & Partial<CachingProvider> = {
		name: `cache(${inner.name})`,

		async stat(ctx, path): Promise<FsEntry> {
			const node = await believe(ctx, path);
			if (node?.kind === "missing") {
				bump("stat.hit");
				throw fsError(node.code, ctx);
			}
			if (node?.entry) {
				bump("stat.hit");
				return node.entry;
			}
			bump("stat.miss");
			return statThrough(ctx, path);
		},

		async readdir(ctx, path, readOpts?: ReaddirOpts): Promise<Listing> {
			// `recursive` with no depth means "everything", which the backend serves
			// with a horizon walk — so what it establishes is unbounded, not MAX_DEPTH.
			const want = readOpts?.recursive ? (readOpts.depth ?? UNBOUNDED) : 1;
			const budget = readOpts?.maxEntries ?? Infinity;

			const cached = listFromTree(
				ctx,
				await believe(ctx, path, false),
				want,
				budget
			);
			if (cached) {
				bump("readdir.hit");
				markAnswered(path, want);
				return cached;
			}

			bump("readdir.miss");
			// A walk pays a request per directory it descends into; seeding the parent's
			// subtree on the first descent makes it pay one per subtree. Only for a plain
			// listing — a caller who asked recursively is already asking for a subtree.
			if (want === 1 && (await seedSubtree(ctx, path))) {
				const seeded = listFromTree(
					ctx,
					await believe(ctx, path, false),
					want,
					budget
				);
				if (seeded) {
					bump("readdir.seeded");
					markAnswered(path, want);
					return seeded;
				}
			}

			const listing = await listThrough(ctx, path, want, readOpts);
			markAnswered(path, want);
			return listing;
		},

		async readFile(ctx, path): Promise<Uint8Array> {
			const node = await believe(ctx, path);
			if (node?.kind === "missing") {
				bump("readFile.hit");
				throw fsError(node.code, ctx);
			}
			if (node?.kind === "dir") {
				bump("readFile.hit");
				throw fsError("EISDIR", ctx);
			}
			if (node?.kind === "file" && node.bytes) {
				bump("readFile.hit");
				touch(node);
				return node.bytes;
			}
			bump("readFile.miss");
			return readThrough(ctx, path);
		},

		async writeFile(ctx, path, data): Promise<void> {
			await inner.writeFile(ctx, path, data);

			// The bytes are now known exactly; the stat is not. Only the backend can
			// say what size and mtime it recorded — puterfs stamps its own clock at
			// one-second resolution — and inventing them to keep the enclosing
			// listing intact would put a wrong `Stats` in front of every caller that
			// lists this directory. So the entry goes, and with it the ancestors'
			// claim to know their own contents.
			//
			// Keeping the bytes is what this is really for: an agent that writes a
			// file and reads it straight back pays nothing, which is the common half
			// of an edit.
			const node: FileNode = { kind: "file", gen };
			invalidate(path, node);

			// A copy: what arrives is a view into the request frame, whose buffer the
			// transport may reuse the moment this returns.
			if (maxBytes > 0 && data.length <= maxFileBytes) {
				admit(node, new Uint8Array(data));
			}
			statfsValue = undefined;
		},

		async mkdir(ctx, path, mkdirOpts): Promise<string | undefined> {
			const created = await inner.mkdir(ctx, path, mkdirOpts);
			// A recursive mkdir may have created ancestors too, and reports at most
			// one of them, so nothing narrower than the whole chain is safe.
			invalidate(path);
			statfsValue = undefined;
			return created;
		},

		async rm(ctx, path, rmOpts): Promise<void> {
			await inner.rm(ctx, path, rmOpts);
			invalidate(path, { kind: "missing", code: "ENOENT", gen });
			statfsValue = undefined;
		},

		async rename(ctx, from, to): Promise<void> {
			await inner.rename(ctx, from, to);
			invalidate(from, { kind: "missing", code: "ENOENT", gen });
			invalidate(to);
		},

		async utimes(ctx, path, atimeMs, mtimeMs): Promise<boolean> {
			const applied = await inner.utimes(ctx, path, atimeMs, mtimeMs);
			if (!applied) return applied;
			// The timestamps moved; the contents did not. Dropping the stat while
			// keeping the bytes is the honest description of what changed — though a
			// later revalidation cannot confirm bytes it has no identity for, so they
			// go on the next stale mark.
			const node = walk(path).node;
			if (node && node.kind !== "missing") {
				if (node.entry?.uid) byUid.delete(node.entry.uid);
				node.entry = undefined;
			}
			return applied;
		},
	};

	// ------------------------------------------- optional halves of the interface
	//
	// Mirrored from the wrapped provider rather than declared unconditionally,
	// because the mount snapshot derives its capability flags from which of these
	// exist and the worker picks a read strategy from those. Declaring one the
	// backend does not have would make the advertised capability a lie — and
	// over-claiming `hasNativeRange` in particular is quadratic.

	if (inner.readRange) {
		provider.readRange = async (ctx, path, offset, length) => {
			const node = await believe(ctx, path);
			if (node?.kind === "file" && node.bytes) {
				bump("readRange.hit");
				touch(node);
				return node.bytes.subarray(offset, offset + length);
			}
			bump("readRange.miss");
			// Deliberately not stored: a window of a file is not the file, and
			// admitting it as one is how a positioned read silently becomes a short
			// one.
			return inner.readRange!(ctx, path, offset, length);
		};
	}

	if (inner.openRead) {
		provider.openRead = async (ctx, path, range): Promise<ProviderStream> => {
			const node = await believe(ctx, path);
			if (node?.kind === "file" && node.bytes) {
				bump("openRead.hit");
				touch(node);
				return streamOfBytes(node.bytes, range);
			}
			bump("openRead.miss");
			// Passed through without being held: a stream is what a caller reaches for
			// when a file is too big to want in memory, which is exactly the file this
			// should not be holding.
			return inner.openRead!(ctx, path, range);
		};
	}

	if (inner.copyFile) {
		provider.copyFile = async (ctx, from, to, copyOpts) => {
			await inner.copyFile!(ctx, from, to, copyOpts);
			invalidate(to);
			statfsValue = undefined;
		};
	}

	if (inner.statfs) {
		provider.statfs = async (ctx) => {
			if (statfsValue && Date.now() - statfsAt < STATFS_TTL_MS) {
				bump("statfs.hit");
				return statfsValue;
			}
			bump("statfs.miss");
			statfsValue = await inner.statfs!(ctx);
			statfsAt = Date.now();
			return statfsValue;
		};
	}

	// ------------------------------------------------------------------- control

	provider.applyEvent = (event: PuterFsEvent) => {
		if (!under(prefix, event.path)) return;
		const path = toLocal(prefix, event.path);
		bump("event");

		// A uid we hold at some *other* path means that entry moved and nothing said
		// so — the api answers a rename with `item.updated` naming only the new path.
		// Without this the old name stays cached, and believed, forever.
		if (event.uid) {
			const previous = byUid.get(event.uid);
			if (previous !== undefined && previous !== path) {
				invalidate(previous, { kind: "missing", code: "ENOENT", gen });
			}
		}

		if (event.kind === "removed") {
			if (event.descendantsOnly) invalidateChildren(path);
			else invalidate(path, { kind: "missing", code: "ENOENT", gen });
			return;
		}
		if (
			event.kind === "moved" &&
			event.oldPath &&
			under(prefix, event.oldPath)
		) {
			invalidate(toLocal(prefix, event.oldPath), {
				kind: "missing",
				code: "ENOENT",
				gen,
			});
		}
		invalidate(path);
	};

	provider.markStale = () => {
		bump("stale");
		gen++;
		statfsValue = undefined;
	};

	provider.flush = () => {
		bump("flush");
		gen++;
		flushTree();
	};

	provider.stats = () => ({
		...Object.fromEntries([...counts].sort((a, b) => b[1] - a[1])),
		bytesHeld: held,
		filesHeld: lru.size,
		generation: gen,
	});

	return provider as CachingProvider;
}
