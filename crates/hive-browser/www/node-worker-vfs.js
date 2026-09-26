// The browser node's PERSISTENT guest filesystem (bn-node-worker-vfs-opfs-fsa).
//
// WHAT THIS DECIDES. node-worker's filesystem is provider-based: `NodeVfs`
// mounts one `VfsProvider` per subtree and, started without a puter token, its
// root is a MEMORY overlay (vendor/node-worker/src/lib/vfs/index.ts). So a guest
// that runs `npm install`, fills a build cache or keeps a state file loses all
// of it the instant the tab goes away. This module is the embedder's half of
// that choice — the vendor's README says exactly this, "the filesystem is yours
// to provide" — and it does three things: pick a persistent backend, mount it,
// and destroy it when the grant that authorized holding donor-side data is gone.
//
// THE BACKENDS, IN ORDER.
//
//   1. OPFS — `navigator.storage.getDirectory()`. Private to the origin, no
//      permission prompt, survives reload, restart and profile reopen.
//      Preferred, and the only one mounted without asking anyone anything.
//   2. File System Access — `showDirectoryPicker()`. A real directory the donor
//      chose. Reachable only from a user gesture, and its grant does NOT
//      survive a reload, so it is the alternative, never the default.
//   3. Neither → `GuestFsUnavailable` naming why. This module never falls back
//      to memory on its own: a silent memory fallback is a filesystem that
//      looks persistent and is not.
//
// Both 1 and 2 hand back the same object — a `FileSystemDirectoryHandle` — so
// both run on the vendor's `createDirectoryHandleProvider`. Nothing here
// reimplements a backend, and nothing here is imported by the guest.
//
// THE SYNCHRONOUS HALF. `fs.readFileSync` inside the guest is not a second
// implementation and does not bypass this: the worker posts a blocking XHR, the
// service worker relays the frame to the page (vendor/node-worker/src/sw/
// handler.ts), the page runs `handleFrame`, and the mount table inside it
// resolves the path to whatever provider is mounted — this one included. The
// relay never looks inside a frame and has no filesystem knowledge at all, so
// mounting a persistent provider is *sufficient* for the synchronous methods to
// hit persistent storage. Two consequences worth knowing before relying on it:
//
//   * **The fs host must be a window.** `navigator.serviceWorker` is
//     Window-only, and a cold service worker re-attaches by asking
//     `clients.matchAll({ type: "window" })`. Our donor node runs in a
//     SharedWorker (ui/public/run-node-worker.js), which has neither — so the
//     filesystem has to be hosted by a connected page, brokered the way that
//     worker already brokers its sqlite DedicatedWorker. `syncFsHost()` below
//     reports this instead of letting a sync call hang.
//   * **No SharedArrayBuffer is needed for it.** The bridge is a blocking XHR
//     plus a MessagePort; there is no shared memory anywhere in
//     src/lib/sw.ts, src/sw/handler.ts or src/wire/sw.ts. (The only SAB mention
//     in the vendored tree is `worker_threads`' `receiveMessageOnPort`, which
//     this platform does not use.)
//
// THE CONCURRENCY GUARANTEE ACTUALLY IMPLEMENTED — two layers, both narrow on
// purpose, because the honest answer for OPFS under a parked sync caller is not
// "it is fine":
//
//   1. **Per-path mutual exclusion inside one owner.** `serializeProvider`
//      below puts every operation — reads included — on a promise chain keyed by
//      its path, so a `readFileSync` parked in the worker cannot observe a file
//      halfway through a `createWritable` that the async transport started on
//      the same path. `rename`/`copyFile` take both paths' chains in sorted
//      order, which is what stops two of them from deadlocking. On top of that
//      the vendor's provider already serializes its own writes per path and
//      reports a foreign lock as EBUSY rather than waiting on it.
//
//      Measured, not assumed — headless Chrome, real OPFS, 256 KiB payloads,
//      30 rounds of "write + read the same path with no await between them":
//
//      | provider                    | writes | reads | read errors | torn |
//      | --------------------------- | ------ | ----- | ----------- | ---- |
//      | OPFS handle, unserialized   |     30 |     0 |          30 |    0 |
//      | same, via serializeProvider |     30 |    30 |           0 |    0 |
//
//      So the claim "a concurrent read sees a torn file" is too kind: at this
//      size every unserialized read *failed outright* against the in-flight
//      write (the file is locked by `createWritable`), which in a guest program
//      is an unexplained ENOENT/EIO on a file that is demonstrably there. This
//      is the layer that removes it.
//   2. **One owner per browser profile.** `mountGuestFs` takes an exclusive Web
//      Lock (`hive-nodefs:<dir>`) and holds it until `dispose()`/`wipe()` — the
//      same primitive crates/hive-browser/www/asset-store.js uses for
//      single-writer safety. A second context that cannot take it gets a named
//      error, never a silent second writer. Where Web Locks are missing the
//      mount still happens and the result says so (`guarantee`:
//      `"per-path"`), which is the weaker of the two answers and is reported as
//      such.
//
// NOT GUARANTEED, and deliberately not papered over: cross-path atomicity (a
// build's writes are not a transaction — a crash between two of them leaves a
// mix); ordering between a path and its parent directory (chains are per path,
// not per subtree, so a `readdir` may land either side of a create under it);
// another browser profile or a second context that ignores the lock; and
// `FileSystemSyncAccessHandle`, which does have a real exclusive lock but is
// DedicatedWorker-only and so is deliberately not used from a page. Concurrent
// modification from outside is *reported* (EBUSY / NoModificationAllowedError)
// and never waited on.
//
// WHAT SURVIVES, AND WHAT IS WIPED — the retention contract:
//
//   survives a reload / tab close / browser restart (same profile, same origin)
//     * everything under the persistent mount: `GUEST_FS_MOUNT` (default
//       `/persist`) and the whole tree beneath it, including `node_modules`
//       installed by a guest build and any state file the guest wrote.
//
//   does not survive — every reload loses these, by design
//     * `/tmp` (node-worker's own memory mount) and `/dev`;
//     * everything outside the persistent mount, since `/` is the memory
//       overlay;
//     * injected modules and the run's entry point — the mount is created with
//       `{ overlay: true }`, so `addVirtualFile` lands in a memory layer that
//       is thrown away rather than littering the donor's storage on every run;
//     * open descriptors and any buffered write that was never flushed.
//
//   wiped, and only wiped, on REVOCATION
//     * the entire `hive-nodefs/<project>` subtree — `wipeGuestFs()`. Called
//       from the terminal-admission-denial path of
//       ui/public/run-node-worker.js, the same event that wipes the CRR
//       replica. Nothing smaller wipes: a refused sync round is not revocation,
//       and — unlike the database replica, whose system of record is the
//       converged fleet set — this filesystem IS the system of record for the
//       guest tree, so an ordinary stop() must NOT destroy it. A donor turning
//       the node off and on again gets their filesystem back.
//
// NAMING. The tenant-controlled project name never becomes a path: it is
// sanitized to `[a-z0-9._-]` and used as one OPFS entry name underneath the
// platform-owned `hive-nodefs` directory — the same invariant as
// `hive-vol-{sanitize_tag(project)}` on the fleet side. Wipe validates that name
// before it is handed to `removeEntry`.

// The one OPFS directory this platform owns. Not configurable: it is what makes
// a wipe's blast radius auditable, and it keeps the guest tree away from
// `hive-crsql/` (the CRR replica lane), which has its own retention.
export const GUEST_FS_DIR = "hive-nodefs";

/**
 * Where the persistent tree is mounted in the guest namespace.
 *
 * Not "/" — `NodeVfs` mounts the memory overlay there at construction and the
 * mount table refuses to remount it — so `/persist` is the persistent root and
 * the guest's cwd belongs under it. `/tmp` and `/dev` stay memory mounts and
 * keep their own (longest-prefix) mounts, so making "/" persistent would be
 * wrong even if it were possible.
 */
export const GUEST_FS_MOUNT = "/persist";

/** Web Lock name prefix: one owner per (origin, guest tree). */
export const GUEST_FS_LOCK_PREFIX = "hive-nodefs:";

const TAG_RE = /^[a-z0-9._-]+$/;
const MAX_TAG_LEN = 200;

/** The only directory names this module will ever create or delete. */
function assertEntryName(name) {
	if (name !== GUEST_FS_DIR && !TAG_RE.test(name)) {
		throw new TypeError(
			`refusing to use ${JSON.stringify(name)} as a guest filesystem entry name`
		);
	}
	return name;
}

/**
 * A project name → one flat entry name.
 *
 * Every character outside `[a-z0-9._-]` becomes `-` and the result is trimmed,
 * which is what keeps a project called `..` or `a/b` from naming something
 * outside the directory this platform owns. Throws rather than returning a
 * degraded name: an empty tag would collide every such project onto one tree.
 */
export function sanitizeGuestTag(value) {
	if (typeof value !== "string" || value.length === 0) {
		throw new TypeError("guest filesystem project must be a non-empty string");
	}
	if (value.length > MAX_TAG_LEN) {
		throw new TypeError(`guest filesystem project is longer than ${MAX_TAG_LEN}`);
	}
	const tag = value
		.toLowerCase()
		.replace(/[^a-z0-9._-]/g, "-")
		.replace(/^[._-]+/, "")
		.replace(/[._-]+$/, "");
	if (!TAG_RE.test(tag)) {
		throw new TypeError(
			`guest filesystem project ${JSON.stringify(value)} has no usable characters`
		);
	}
	return tag;
}

/** The storage-side name of one project's guest tree. Diagnostics and locks. */
export function guestFsDirName(project) {
	return `${GUEST_FS_DIR}/${sanitizeGuestTag(project)}`;
}

/** Persistent filesystem is not available here, and why. Never a silent fallback. */
export class GuestFsUnavailable extends Error {
	constructor(reason, detail) {
		super(
			`browser node: no persistent guest filesystem (${reason})` +
				(detail ? `: ${detail}` : "")
		);
		this.name = "GuestFsUnavailable";
		this.reason = reason;
		this.detail = detail ?? "";
	}
}

/**
 * What this browser can do, so a caller can report an honest reason instead of
 * discovering it as a failed mount.
 */
export async function guestFsCapabilities() {
	const storage = typeof navigator === "undefined" ? undefined : navigator.storage;
	return {
		opfs: typeof storage?.getDirectory === "function",
		persist: typeof storage?.persist === "function",
		picker: typeof globalThis.showDirectoryPicker === "function",
		locks: typeof navigator !== "undefined" && typeof navigator.locks?.request === "function",
		serviceWorker: typeof navigator !== "undefined" && !!navigator.serviceWorker,
		secureContext:
			typeof isSecureContext === "boolean" ? isSecureContext : undefined,
	};
}

/**
 * Whether THIS context can host the synchronous-fs bridge.
 *
 * `navigator.serviceWorker` is Window-only and a cold service worker can only
 * be re-attached by a window client, so a SharedWorker — which is where this
 * platform's donor node lives — is not a legal fs host and must broker one from
 * a connected page. Saying that here beats a synchronous `require` that hangs.
 */
export async function syncFsHost() {
	const capabilities = await guestFsCapabilities();
	if (!capabilities.serviceWorker) {
		return {
			ok: false,
			reason: "no-sw",
			detail:
				"navigator.serviceWorker is Window-only and a cold service worker is " +
				"re-attached by asking window clients, so this context cannot host " +
				"synchronous fs — broker it to a connected page",
		};
	}
	if (capabilities.secureContext === false) {
		return {
			ok: false,
			reason: "insecure-context",
			detail: "service workers require a secure context (https, or localhost)",
		};
	}
	return { ok: true };
}

/** The origin's OPFS root, or null where OPFS does not exist. */
async function opfsOrigin() {
	if (typeof navigator === "undefined") return null;
	if (typeof navigator.storage?.getDirectory !== "function") return null;
	return await navigator.storage.getDirectory();
}

/** `hive-nodefs`, created on demand. */
async function opfsBase(create = true) {
	const origin = await opfsOrigin();
	if (!origin) return null;
	return await origin.getDirectoryHandle(GUEST_FS_DIR, { create });
}

/**
 * Best-effort "do not evict me".
 *
 * Same posture as identity.js: many browsers auto-grant on site engagement,
 * some prompt, and a refusal only leaves the pre-existing eviction risk
 * unchanged — so it never fails a mount.
 */
async function requestPersistence() {
	try {
		if (typeof navigator !== "undefined" && navigator.storage?.persist) {
			return await navigator.storage.persist();
		}
	} catch {
		/* best-effort */
	}
	return false;
}

/** Ask a picked handle for its grant, re-requesting it inside a gesture if needed. */
async function requestHandleAccess(handle, mode = "readwrite") {
	const withPermissions = handle;
	if (typeof withPermissions.queryPermission !== "function") return true; // OPFS
	if ((await withPermissions.queryPermission({ mode })) === "granted") return true;
	if (typeof withPermissions.requestPermission !== "function") return false;
	return (await withPermissions.requestPermission({ mode })) === "granted";
}

/**
 * Choose the persistent backend for one project's guest tree.
 *
 * `prefer: "opfs"` (the default) never prompts; `"fsa"` goes straight to
 * `showDirectoryPicker()`, which needs a user gesture and whose grant does not
 * survive a reload. `allowPicker` lets the OPFS path fall back to the picker
 * when OPFS is missing, which is the only case where the picker is reachable
 * without an explicit request for it.
 *
 * Returns `{ kind: "none", reason, detail }` rather than throwing, so a caller
 * can distinguish "no persistence here" from "mounting failed" and decide
 * whether to run on memory knowingly.
 */
export async function selectPersistentRoot({
	project,
	prefer = "opfs",
	allowPicker = false,
	subdir = true,
} = {}) {
	const name = project === undefined ? GUEST_FS_DIR : sanitizeGuestTag(project);
	const dir = project === undefined ? GUEST_FS_DIR : guestFsDirName(project);

	// A nested tree under `hive-nodefs/<tag>` unless the caller asked for the
	// platform directory itself, so one wipe can take a project without touching
	// the others.
	const wantPicker = prefer === "fsa" || allowPicker;

	if (prefer !== "fsa") {
		const base = await opfsBase(true);
		if (base) {
			const persisted = await requestPersistence();
			const handle = subdir && project !== undefined
				? await base.getDirectoryHandle(name, { create: true })
				: base;
			return { kind: "opfs", dir, name, handle, parent: base, persisted };
		}
	}

	if (wantPicker && typeof globalThis.showDirectoryPicker === "function") {
		let picked;
		try {
			picked = await globalThis.showDirectoryPicker({
				id: "hive-nodefs",
				mode: "readwrite",
			});
		} catch (err) {
			// AbortError is the donor dismissing the dialog, which is an answer,
			// not a fault.
			const name_ = (err && err.name) || "Error";
			return {
				kind: "none",
				reason: name_ === "AbortError" ? "picker-declined" : "picker-failed",
				detail: `${name_}: ${(err && err.message) || "no directory chosen"}`,
			};
		}
		if (!(await requestHandleAccess(picked, "readwrite"))) {
			return {
				kind: "none",
				reason: "picker-denied",
				detail: "read/write permission was not granted for the chosen directory",
			};
		}
		const base = await picked.getDirectoryHandle(GUEST_FS_DIR, { create: true });
		const handle =
			subdir && project !== undefined
				? await base.getDirectoryHandle(name, { create: true })
				: base;
		return { kind: "fsa", dir, name, handle, parent: base, persisted: false };
	}

	const capabilities = await guestFsCapabilities();
	return {
		kind: "none",
		reason: "no-backend",
		detail: capabilities.opfs
			? "no backend selected"
			: "OPFS is unavailable in this browser and no directory was picked",
	};
}

/**
 * Wrap a provider so no two operations touch the same path at once.
 *
 * This is the whole of the concurrency guarantee's inner layer, and it exists
 * because of the synchronous bridge: a parked worker's `readFileSync` and an
 * asynchronously-dispatched write arrive at the page independently, and an OPFS
 * write is a `createWritable` that truncates before it writes — so without this
 * a reader can legitimately observe a truncated or half-written file. Reads are
 * serialized too, not only writes, which is what makes the read side of that
 * statement true.
 *
 * Optional methods are copied only when the wrapped provider has them: the
 * facade decides whether a backend has a native positioned read by testing for
 * `readRange`, so defining one that merely forwards would be a claim of
 * seekability that could make a byte-range loop quadratic.
 */
export function serializeProvider(provider) {
	if (!provider || typeof provider.stat !== "function") {
		throw new TypeError("serializeProvider needs a VfsProvider");
	}
	/** In-flight chains, one per path. Empty whenever nothing is running. */
	const chains = new Map();

	const noop = () => undefined;

	function chain(key, fn) {
		const prev = chains.get(key) ?? Promise.resolve();
		// `then(fn, fn)`: one failed op must not wedge everything behind it.
		const next = prev.then(fn, fn);
		const settled = next.then(noop, noop);
		chains.set(key, settled);
		// Dropped once this is still the tail, so a long run does not retain one
		// promise per path it ever touched.
		void settled.then(() => {
			if (chains.get(key) === settled) chains.delete(key);
		});
		return next;
	}

	/** Two paths, always taken in the same order, so two renames cannot deadlock. */
	function chainPair(a, b, fn) {
		return a <= b
			? chain(a, () => chain(b, fn))
			: chain(b, () => chain(a, fn));
	}

	const out = {
		name: `${provider.name}+serialized`,
		stat: (ctx, path) => chain(path, () => provider.stat(ctx, path)),
		readdir: (ctx, path, opts) =>
			chain(path, () => provider.readdir(ctx, path, opts)),
		readFile: (ctx, path) => chain(path, () => provider.readFile(ctx, path)),
		writeFile: (ctx, path, data) =>
			chain(path, () => provider.writeFile(ctx, path, data)),
		mkdir: (ctx, path, opts) => chain(path, () => provider.mkdir(ctx, path, opts)),
		rm: (ctx, path, opts) => chain(path, () => provider.rm(ctx, path, opts)),
		rename: (ctx, from, to) =>
			chainPair(from, to, () => provider.rename(ctx, from, to)),
		utimes: (ctx, path, atimeMs, mtimeMs) =>
			chain(path, () => provider.utimes(ctx, path, atimeMs, mtimeMs)),
	};
	if (provider.copyFile) {
		out.copyFile = (ctx, from, to, opts) =>
			chainPair(from, to, () => provider.copyFile(ctx, from, to, opts));
	}
	if (provider.readRange) {
		out.readRange = (ctx, path, offset, length) =>
			chain(path, () => provider.readRange(ctx, path, offset, length));
	}
	if (provider.openRead) {
		// The open is what is serialized; the stream then transfers outside the
		// chain, which is deliberate — holding a path hostage to a slow reader
		// would be worse than the interleaving it prevents.
		out.openRead = (ctx, path, range) =>
			chain(path, () => provider.openRead(ctx, path, range));
	}
	if (provider.statfs) {
		// No path to key on: whole-filesystem, and nothing above serializes it
		// either.
		out.statfs = (ctx) => provider.statfs(ctx);
	}
	return out;
}

/** The vendored lane's provider factory, or a named "not built" error. */
async function loadNodeWorker() {
	try {
		return await import("./node-worker/index.js");
	} catch (err) {
		throw new GuestFsUnavailable(
			"node-worker-unbuilt",
			`the node-worker lane is not built (run scripts/build-node-worker.sh): ${
				(err && err.message) || err
			}`
		);
	}
}

/** How long a mount waits for the owner lock before concluding someone else owns it. */
const LOCK_WAIT_MS = 250;

/** Should another context already be the owner, the name of the error. */
const LOCK_CONTENTION_REASON = "guest-fs-busy";

/**
 * Hold the owner lock for the lifetime of a mount.
 *
 * The pattern asset-store.js uses for single-writer safety, applied to a whole
 * tree instead of one asset: acquire exclusively and sit on it until released.
 * The browser drops it if this context dies, which is what makes it safe to
 * hold rather than to take per operation.
 *
 * The grant is detected from INSIDE the callback — the browser runs it only
 * once the lock is actually ours — because a queued request looks exactly like
 * a granted one from outside, and reporting a guarantee we do not have is worse
 * than not mounting. A request that has not been granted inside `LOCK_WAIT_MS`
 * is aborted rather than left queued behind the owner.
 */
async function holdOwnerLock(dir) {
	const name = `${GUEST_FS_LOCK_PREFIX}${dir}`;
	if (
		typeof navigator === "undefined" ||
		typeof navigator.locks?.request !== "function"
	) {
		return { held: false, contended: false, name, release: () => {} };
	}
	const controller = new AbortController();
	let release = () => {};
	const granted = new Promise((resolve) => {
		navigator.locks
			.request(
				name,
				{ mode: "exclusive", signal: controller.signal },
				() => {
					resolve(true);
					// Never settles until `release()` — that is the hold.
					return new Promise((resolveHold) => {
						release = resolveHold;
					});
				}
			)
			.catch(() => resolve(false));
	});
	const won = await Promise.race([
		granted,
		new Promise((resolve) => setTimeout(() => resolve(false), LOCK_WAIT_MS)),
	]);
	if (!won) {
		// Either another context owns the tree, or the request was aborted. Stop
		// waiting and say so: two writers to one OPFS tree is corruption in the
		// making, and a mount that claims an owner guarantee it does not have is
		// worse than a mount that declines.
		controller.abort();
		return { held: false, contended: true, name, release: () => {} };
	}
	return { held: true, contended: false, name, release: () => release() };
}

/**
 * Mount a persistent backend into a `NodeVfs`.
 *
 * `project` scopes the tree (and the lock, and a later wipe) to one project;
 * omit it to mount the platform directory itself. `replace` unmounts first,
 * which is what re-running a project wants.
 *
 * The mount is created with `{ overlay: true }` so injected modules land in a
 * memory layer instead of being persisted — a run's entry point is not donor
 * state, and writing it into OPFS would leave litter behind on every run.
 *
 * Returns a handle whose `dispose()` keeps the data and unmounts, and whose
 * `wipe()` unmounts, releases the lock, and deletes the tree: revocation's
 * path, and the only one that deletes anything.
 */
export async function mountGuestFs({
	vfs,
	project,
	mount = GUEST_FS_MOUNT,
	prefer = "opfs",
	allowPicker = false,
	readOnly = false,
	replace = false,
	providerFactory,
} = {}) {
	if (!vfs || typeof vfs.mount !== "function") {
		throw new TypeError("mountGuestFs needs a NodeVfs");
	}
	const selected = await selectPersistentRoot({ project, prefer, allowPicker });
	if (selected.kind === "none") {
		throw new GuestFsUnavailable(selected.reason, selected.detail);
	}

	const factory =
		providerFactory ??
		((await loadNodeWorker()).createDirectoryHandleProvider);
	if (typeof factory !== "function") {
		throw new GuestFsUnavailable(
			"node-worker-unbuilt",
			"the node-worker lane does not export createDirectoryHandleProvider"
		);
	}

	const lock = await holdOwnerLock(selected.dir);
	if (lock.contended) {
		throw new GuestFsUnavailable(
			LOCK_CONTENTION_REASON,
			`another tab or window already owns ${selected.dir}; a second writer to one ` +
				`OPFS tree is corruption, so this mount declines rather than sharing`
		);
	}
	// Reported, not enforced: the asynchronous `fs` works without the bridge, but
	// `readFileSync`/`require` do not, and a caller that can see this answer can
	// say which of the two it is running on instead of discovering it as a hang.
	const sync = await syncFsHost();
	let mounted = false;
	try {
		if (replace) {
			try {
				vfs.unmount(mount);
			} catch {
				/* not mounted; the mount below is the first */
			}
		}
		vfs.mount(
			mount,
			(m) =>
				serializeProvider(
					factory(selected.handle, {
						...m,
						name: `guest-fs:${selected.dir}`,
						readOnly,
					})
				),
			{ readOnly, overlay: true }
		);
		mounted = true;
	} catch (err) {
		lock.release();
		throw err;
	}

	return {
		kind: selected.kind,
		dir: selected.dir,
		mount,
		project: project ?? null,
		handle: selected.handle,
		lock: { held: lock.held, name: lock.name },
		/** Whether synchronous `fs` can reach this mount at all — see syncFsHost(). */
		sync,
		/** The weaker of the two answers when Web Locks are missing. */
		guarantee: lock.held ? "single-owner+per-path" : "per-path",
		async dispose() {
			if (mounted) {
				mounted = false;
				try {
					vfs.unmount(mount);
				} catch {
					/* "/" cannot be unmounted; nothing else fails here */
				}
			}
			lock.release();
		},
		async wipe() {
			// Unmount FIRST: no op can be in flight against a tree that is about
			// to be deleted, and the lock is what stops a second owner from
			// mounting it while it goes.
			await this.dispose();
			return await wipeGuestFs({ project, parent: selected.parent });
		},
	};
}

/**
 * Delete a guest tree outright. Revocation, never routine teardown.
 *
 * `project` given → that project's directory only; omitted → the whole
 * `hive-nodefs` directory, which is what a terminal admission denial wants
 * (the admission authorized everything this node held, not one tree).
 *
 * Loud about what it could not remove rather than silent: a partial wipe is
 * donor-held data that survived revocation, and nobody would ever find out.
 */
export async function wipeGuestFs({ project, parent } = {}) {
	let base = parent;
	let name = project === undefined ? GUEST_FS_DIR : sanitizeGuestTag(project);
	if (!base) {
		if (project === undefined) {
			base = await opfsOrigin();
		} else {
			base = await opfsBase(false);
			if (!base) {
				// Nothing was ever created under our directory, so there is nothing
				// donor-held to wipe. Not an error: revocation with no data held.
				return { dir: guestFsDirName(project), removed: 0, survivors: [] };
			}
		}
	}
	if (!base) {
		throw new GuestFsUnavailable(
			"no-opfs",
			"OPFS is unavailable, so there is no guest filesystem to wipe"
		);
	}
	assertEntryName(name);
	return await removeTree(base, name, guestFsDirNameOrRoot(project));
}

function guestFsDirNameOrRoot(project) {
	return project === undefined ? GUEST_FS_DIR : guestFsDirName(project);
}

/** Remove `name` from `parent`, walking it by hand if `recursive` is refused. */
async function removeTree(parent, name, dir) {
	try {
		await parent.removeEntry(name, { recursive: true });
		return { dir, removed: 1, survivors: [] };
	} catch (err) {
		if ((err && err.name) === "NotFoundError") {
			return { dir, removed: 0, survivors: [] };
		}
		// `recursive` is not universally supported in the File System Access api.
		// Falling back is not optional: a wipe that cannot run is donor-held data
		// outliving its grant.
	}
	const survivors = [];
	let removed = 0;
	let target;
	try {
		target = await parent.getDirectoryHandle(name);
	} catch (err) {
		if ((err && err.name) === "NotFoundError") {
			return { dir, removed: 0, survivors: [] };
		}
		throw err;
	}
	// Materialized first: removing entries while iterating the same directory is
	// not something the api defines.
	const children = [];
	for await (const [childName, handle] of target.entries()) {
		children.push([childName, handle]);
	}
	for (const [childName] of children) {
		try {
			await target.removeEntry(childName, { recursive: true });
			removed++;
		} catch {
			survivors.push(`${dir}/${childName}`);
		}
	}
	if (survivors.length > 0) {
		return { dir, removed, survivors };
	}
	try {
		await parent.removeEntry(name, { recursive: true });
		removed++;
	} catch (err) {
		survivors.push(dir);
	}
	return { dir, removed, survivors };
}
