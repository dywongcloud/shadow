// Open files: one handle per fd, and the buffering that makes a partial write possible on a
// backend that has none.
//
// ## Why a handle needs a buffer at all
//
// puterfs has no partial-write primitive — every write is a whole-file upload — and a
// `FileSystemFileHandle` is not much better. So `fs.writeSync(fd, bytes, …, position)` has to
// become read-modify-write, and a handle holds the file's contents to make a sequence of small
// writes cost one upload at flush rather than one each. That buffer is also what lets `fstat`
// on a dirty fd report the size the program just wrote rather than the stale one on the server.
//
// ## What changed in moving here, and why it got simpler
//
// The worker's version could not take a lock. A "write" is a multi-round-trip
// read-modify-write over mutable state, so two overlapping async writes would interleave at
// every await and lose data — but a *synchronous* write cannot wait on a promise queue, and it
// could genuinely arrive while one was held (`let p = handle.readFile(); fs.readSync(fd, …)`).
// Rejecting that with EBUSY would have invented a failure mode node does not have. So the
// invariant moved into the code instead: offsets reserved in straight-line code before the
// first yield, buffer fills double-checked after every yield, and a generation counter to fail
// anything that resumed into a closed fd.
//
// None of that is needed here. Both surfaces arrive as wire ops through one dispatcher, and a
// host operation never needs the worker to make progress, so an ordinary FIFO queue per handle
// cannot deadlock a blocked caller. With operations serialized:
//
//   - offsets advance by the *actual* bytes moved, instead of being reserved optimistically and
//     rolled back on a short read;
//   - a fill cannot be raced by a write, so the double-checks go;
//   - nothing can resume into a changed generation, so the counter goes.
//
// Two concurrent positionless reads on one fd now serialize rather than interleaving. That is a
// stronger guarantee than node makes, not a weaker one.

import { fsError } from "../../vfs/errno";
import { basename } from "../../vfs/path";
import type { FsEntry, WireCtx } from "../../vfs/entry";
import type { ProviderStream } from "../../vfs/provider";
import type { OpenFlags } from "../../vfs/flags";
import type { Facade } from "./facade";
import { streamOfBytes } from "./stream";

/** node starts its own fds at 0; 10 leaves room for the stdio numbers the worker owns. */
const FIRST_FD = 10;

/**
 * A cap, so a worker that leaks handles is bounded.
 *
 * This matters more than it did worker-side: a handle now outlives the worker that opened it
 * unless the session is torn down, so an unbounded registry is a real leak of whatever those
 * handles hold — including, for a memory mount, the contents of unlinked files that are kept
 * alive on purpose.
 */
const MAX_OPEN = 4096;

interface Fragment {
	start: number;
	end: number;
	data: Uint8Array;
}

function concat(a: Uint8Array, b: Uint8Array, at: number): Uint8Array {
	const size = Math.max(a.length, at + b.length);
	const out = new Uint8Array(size);
	out.set(a, 0);
	out.set(b, at);
	return out;
}

export class Handle {
	readonly fd: number;
	readonly path: string;
	readonly flags: OpenFlags;
	/**
	 * The sync session that opened this fd, so `closeAll` can drop one worker's handles
	 * without touching another's. Undefined only for a handle opened outside a session,
	 * which nothing does today — `dispatch.ts`'s `open` is the sole caller — and which
	 * `closeAll(owner)` deliberately leaves alone rather than guessing about.
	 */
	readonly owner: string | undefined;

	#fs: Facade;
	#closed = false;
	#closing = false;

	#position = 0;
	/** Where the next append lands: the later of the file's length and any append already made. */
	#appendCursor = 0;

	/** Byte-range cache, used only when the backend has a real positioned read. */
	#fragments: Fragment[] = [];
	/** Whole-file buffer: the truth once anything has been written. */
	#buffer: Uint8Array | undefined;
	#bufferMtimeMs: number | undefined;
	/** Last known size on the backend, used to keep ranged reads inside the file. */
	#serverSize: number | undefined;
	#dirty = false;
	#hasNativeRange: boolean;

	/** FIFO. See the note at the top of this file for why this is allowed to exist now. */
	#tail: Promise<unknown> = Promise.resolve();

	constructor(
		fd: number,
		path: string,
		flags: OpenFlags,
		fs: Facade,
		hasNativeRange: boolean,
		owner?: string
	) {
		this.fd = fd;
		this.path = path;
		this.flags = flags;
		this.#fs = fs;
		this.#hasNativeRange = hasNativeRange;
		this.owner = owner;
	}

	/** @internal — set by the registry right after a truncating open. */
	seedEmptied() {
		// No re-stat. The size is zero by construction, and the mtime was only ever used to
		// decide whether to re-read before a write — pointless for a handle about to replace
		// the whole file. Leaving the mtime undefined disables that revalidation, which is what
		// makes skipping the round trip safe.
		this.#serverSize = 0;
		this.#appendCursor = 0;
		this.#buffer = new Uint8Array(0);
		this.#bufferMtimeMs = undefined;
	}

	/** @internal */
	seedExisting(size: number) {
		this.#serverSize = size;
		this.#appendCursor = size;
	}

	get closed() {
		return this.#closed || this.#closing;
	}

	// --------------------------------------------------------------- the queue

	#run<T>(fn: () => Promise<T>): Promise<T> {
		// `then(fn, fn)` rather than `then(fn)`: a failed operation must not wedge the queue
		// behind it.
		const result = this.#tail.then(fn, fn);
		this.#tail = result.catch(() => undefined);
		return result;
	}

	/**
	 * Admission control, then queue.
	 *
	 * The open check happens **here, at submission**, not inside the queued body. An operation
	 * that was accepted before `close()` was called has already been issued as far as the caller
	 * is concerned, and node lets it complete; only operations submitted *after* the close see
	 * EBADF. Checking inside the body instead would fail everything still queued behind a close,
	 * silently dropping writes the program believed it had made.
	 */
	#submit<T>(syscall: string, fn: () => Promise<T>): Promise<T> {
		if (this.#closed || this.#closing) {
			return Promise.reject(fsError("EBADF", { syscall, path: this.path }));
		}
		return this.#run(fn);
	}

	#ctx(syscall: string): WireCtx {
		return { syscall, reportPath: this.path };
	}

	#assertCanRead() {
		if (!this.flags.read)
			throw fsError("EBADF", { syscall: "read", path: this.path });
	}

	#assertCanWrite() {
		if (!this.flags.write) {
			throw fsError("EBADF", { syscall: "write", path: this.path });
		}
	}

	// ------------------------------------------------------------ buffer filling

	/** Ensure the buffer holds the file's current contents. Never discards unflushed writes. */
	async #fill(syscall: string): Promise<Uint8Array> {
		if (this.#dirty) return this.#buffer!;
		const c = this.#ctx(syscall);

		if (this.#buffer !== undefined) {
			// No recorded mtime means revalidation is deliberately off — see `seedEmptied`.
			if (this.#bufferMtimeMs === undefined) return this.#buffer;
			// Someone else may have changed the file since. Note the known limit: puterfs
			// timestamps have one-second resolution, so a read-modify-write inside one second is
			// not detectable this way.
			const current = await this.#fs.stat(c, this.path);
			if (current.modifiedMs === this.#bufferMtimeMs) return this.#buffer;
			this.#bufferMtimeMs = current.modifiedMs;
			this.#serverSize = current.size;
		}

		const fetched = await this.#fs.readFile(c, this.path);
		this.#buffer = fetched;
		this.#fragments = [];
		this.#serverSize = fetched.length;
		this.#appendCursor = Math.max(this.#appendCursor, fetched.length);
		return fetched;
	}

	/**
	 * Fill, and make sure an mtime is recorded, so a later clean fill can tell whether someone
	 * else has changed the file.
	 */
	async #fillForWrite(syscall: string): Promise<Uint8Array> {
		const first = this.#buffer === undefined;
		const buf = await this.#fill(syscall);
		if (first && !this.#dirty && this.#bufferMtimeMs === undefined) {
			const entry = await this.#fs.stat(this.#ctx(syscall), this.path);
			this.#bufferMtimeMs = entry.modifiedMs;
		}
		return buf;
	}

	// ----------------------------------------------------------- fragment cache

	#missingRanges(start: number, end: number): Array<[number, number]> {
		if (end <= start) return [];
		const ranges: Array<[number, number]> = [];
		let cursor = start;
		for (const fragment of this.#fragments) {
			if (fragment.end <= cursor) continue;
			if (fragment.start >= end) break;
			if (fragment.start > cursor)
				ranges.push([cursor, Math.min(fragment.start, end)]);
			cursor = Math.max(cursor, Math.min(fragment.end, end));
			if (cursor >= end) break;
		}
		if (cursor < end) ranges.push([cursor, end]);
		return ranges;
	}

	#mergeFragments() {
		if (this.#fragments.length < 2) return;
		this.#fragments.sort((a, b) => a.start - b.start);
		const merged: Fragment[] = [this.#fragments[0]];
		for (let i = 1; i < this.#fragments.length; i++) {
			const prev = merged[merged.length - 1];
			const curr = this.#fragments[i];
			if (curr.start > prev.end) {
				merged.push(curr);
				continue;
			}
			const start = prev.start;
			const end = Math.max(prev.end, curr.end);
			const data = new Uint8Array(end - start);
			data.set(prev.data, prev.start - start);
			data.set(curr.data, curr.start - start);
			merged[merged.length - 1] = { start, end, data };
		}
		this.#fragments = merged;
	}

	#addFragment(start: number, data: Uint8Array) {
		if (data.length === 0) return;
		this.#fragments.push({
			start,
			end: start + data.length,
			data: new Uint8Array(data),
		});
		this.#mergeFragments();
	}

	async #ensureRanges(start: number, end: number) {
		const c = this.#ctx("read");
		// Never request a range starting at or past EOF: puterfs answers those with a 500, not
		// a 416. Re-stat before concluding EOF, so a file another client has grown since we
		// opened it is still readable.
		if (this.#serverSize === undefined || start >= this.#serverSize) {
			const entry = await this.#fs.stat(c, this.path);
			this.#serverSize = entry.size;
		}
		if (start >= this.#serverSize) return;

		const limit = Math.min(end, this.#serverSize);
		for (const [from, to] of this.#missingRanges(start, limit)) {
			const size = to - from;
			const data = await this.#fs.readRange(c, this.path, from, size);
			if (data.length === 0) break;
			this.#addFragment(from, data);
			if (data.length < size) break;
		}
	}

	#readFromFragments(
		target: Uint8Array,
		targetOffset: number,
		start: number,
		end: number
	): number {
		let cursor = start;
		let written = 0;
		for (const fragment of this.#fragments) {
			if (fragment.end <= cursor) continue;
			if (fragment.start > cursor) break;
			if (fragment.end > cursor) {
				const takeUntil = Math.min(fragment.end, end);
				const take = takeUntil - cursor;
				if (take <= 0) continue;
				target.set(
					fragment.data.subarray(
						cursor - fragment.start,
						cursor - fragment.start + take
					),
					targetOffset + written
				);
				cursor += take;
				written += take;
				if (cursor >= end) break;
			}
		}
		return written;
	}

	// --------------------------------------------------------------------- read

	/** Up to `length` bytes. `position === null` reads from — and advances — the handle's offset. */
	read(length: number, position: number | null): Promise<Uint8Array> {
		return this.#submit("read", async () => {
			this.#assertCanRead();
			if (!Number.isInteger(length) || length < 0) {
				throw fsError("EINVAL", {
					syscall: "read",
					path: this.path,
					message: "invalid length",
				});
			}
			if (length === 0) return new Uint8Array(0);

			// Serialized, so the offset can advance by what was actually read rather than being
			// reserved up front and rolled back on a short read.
			const start = position === null ? this.#position : position;
			const end = start + length;
			const out = new Uint8Array(length);
			let bytesRead = 0;

			if (this.#buffer !== undefined) {
				if (start < this.#buffer.length) {
					bytesRead = Math.min(length, this.#buffer.length - start);
					out.set(this.#buffer.subarray(start, start + bytesRead), 0);
				}
			} else if (this.#hasNativeRange) {
				await this.#ensureRanges(start, end);
				bytesRead = this.#readFromFragments(out, 0, start, end);
			} else {
				// No real positioned read on this backend, so holding the whole file once beats
				// slicing it out of a fresh full download per positioned read.
				const buf = await this.#fill("read");
				if (start < buf.length) {
					bytesRead = Math.min(length, buf.length - start);
					out.set(buf.subarray(start, start + bytesRead), 0);
				}
			}

			if (position === null) this.#position = start + bytesRead;
			return out.subarray(0, bytesRead);
		});
	}

	/** `readv`: several lengths in one round trip, stopping at the first short read. */
	readv(lengths: number[], position: number | null): Promise<Uint8Array[]> {
		return this.#submit("readv", async () => {
			this.#assertCanRead();
			const out: Uint8Array[] = [];
			let at = position;
			for (const length of lengths) {
				// Reuses `read`'s body through the queue would deadlock, so the offset walk is
				// done here and the primitive is called directly.
				const chunk = await this.#readUnqueued(length, at);
				out.push(chunk);
				if (at !== null) at += chunk.length;
				if (chunk.length < length) break;
			}
			return out;
		});
	}

	/** The body of `read`, without the queue — for callers already holding it. */
	async #readUnqueued(
		length: number,
		position: number | null
	): Promise<Uint8Array> {
		if (length === 0) return new Uint8Array(0);
		const start = position === null ? this.#position : position;
		const end = start + length;
		const out = new Uint8Array(length);
		let bytesRead = 0;
		if (this.#buffer !== undefined) {
			if (start < this.#buffer.length) {
				bytesRead = Math.min(length, this.#buffer.length - start);
				out.set(this.#buffer.subarray(start, start + bytesRead), 0);
			}
		} else if (this.#hasNativeRange) {
			await this.#ensureRanges(start, end);
			bytesRead = this.#readFromFragments(out, 0, start, end);
		} else {
			const buf = await this.#fill("read");
			if (start < buf.length) {
				bytesRead = Math.min(length, buf.length - start);
				out.set(buf.subarray(start, start + bytesRead), 0);
			}
		}
		if (position === null) this.#position = start + bytesRead;
		return out.subarray(0, bytesRead);
	}

	/** From the handle's offset to EOF, as node's `filehandle.readFile` does. */
	readFile(): Promise<Uint8Array> {
		return this.#submit("read", async () => {
			this.#assertCanRead();
			const buf = await this.#fill("read");
			const from = this.#position;
			this.#position = buf.length;
			return new Uint8Array(buf.subarray(from));
		});
	}

	// -------------------------------------------------------------------- write

	#splice(position: number, data: Uint8Array) {
		const buf = this.#buffer ?? new Uint8Array(0);
		this.#buffer =
			position + data.length <= buf.length
				? (() => {
						// In place, but on a copy: the buffer may still be the array a provider
						// handed back, and mutating that would rewrite the file underneath us.
						const owned = new Uint8Array(buf);
						owned.set(data, position);
						return owned;
					})()
				: concat(buf, data, position);
		this.#dirty = true;
	}

	write(data: Uint8Array, position: number | null): Promise<number> {
		return this.#submit("write", async () => {
			this.#assertCanWrite();
			await this.#fillForWrite("write");
			// O_APPEND means "past everything this handle knows about", which is the later of the
			// file's length and any append already made through it.
			//
			// For an append-flagged handle the two are provably equal — `#fill` raises the cursor
			// to any longer file it refetches, and every write here sets the cursor to the new
			// buffer length — so the `max` is belt-and-braces on this path and a mutation to it is
			// not observable. It is load-bearing in `appendFile`, where a *positioned* write can
			// extend the buffer past EOF without moving the cursor; there it is tested.
			const at = this.flags.append
				? Math.max(this.#buffer?.length ?? 0, this.#appendCursor)
				: position === null
					? this.#position
					: position;
			this.#splice(at, data);
			if (this.flags.append) this.#appendCursor = at + data.length;
			if (position === null || this.flags.append)
				this.#position = at + data.length;
			return data.length;
		});
	}

	writev(chunks: Uint8Array[], position: number | null): Promise<number> {
		return this.#submit("writev", async () => {
			this.#assertCanWrite();
			await this.#fillForWrite("writev");
			let total = 0;
			let at = position;
			for (const chunk of chunks) {
				const target = this.flags.append
					? Math.max(this.#buffer?.length ?? 0, this.#appendCursor)
					: at === null
						? this.#position
						: at;
				this.#splice(target, chunk);
				if (this.flags.append) this.#appendCursor = target + chunk.length;
				if (at === null || this.flags.append)
					this.#position = target + chunk.length;
				else at = target + chunk.length;
				total += chunk.length;
			}
			return total;
		});
	}

	writeFile(data: Uint8Array): Promise<void> {
		return this.#submit("write", async () => {
			this.#assertCanWrite();
			// The whole file is being replaced, so nothing needs reading first — but the fill
			// still runs, so mtime bookkeeping is settled for a later clean read.
			await this.#fillForWrite("write");
			this.#buffer = new Uint8Array(data);
			this.#dirty = true;
			this.#position = this.#buffer.length;
			this.#appendCursor = this.#buffer.length;
			this.#fragments = [];
		});
	}

	appendFile(data: Uint8Array): Promise<void> {
		return this.#submit("write", async () => {
			this.#assertCanWrite();
			const buf = await this.#fillForWrite("write");
			const at = Math.max(buf.length, this.#appendCursor);
			this.#appendCursor = at + data.length;
			this.#splice(at, data);
			this.#position = at + data.length;
		});
	}

	truncate(len = 0): Promise<void> {
		return this.#submit("ftruncate", async () => {
			this.#assertCanWrite();
			if (!Number.isInteger(len)) len = Math.trunc(len);
			if (len < 0) len = 0;

			const buf = await this.#fillForWrite("ftruncate");
			let out: Uint8Array;
			if (len <= buf.length) out = new Uint8Array(buf.subarray(0, len));
			else {
				// Growing a file zero-fills, as ftruncate(2) does.
				out = new Uint8Array(len);
				out.set(buf, 0);
			}
			this.#buffer = out;
			this.#dirty = true;
			this.#appendCursor = len;
			this.#fragments = [];
		});
	}

	// --------------------------------------------------------------- flush/close

	/** Whether this fd is holding writes the backing store has not seen yet. */
	get dirty() {
		return this.#dirty;
	}

	sync(): Promise<void> {
		return this.#run(() => this.#syncUnqueued());
	}

	async #syncUnqueued(): Promise<void> {
		if (!this.#dirty || !this.#buffer) return;
		const buf = this.#buffer;
		// Cleared before the upload so a failure can restore it, rather than a later write
		// having its flag wiped when this completes.
		this.#dirty = false;
		try {
			await this.#fs.writeFile(this.#ctx("write"), this.path, buf);
		} catch (err) {
			this.#dirty = true;
			throw err;
		}
		this.#serverSize = buf.length;
		this.#fragments = [];
		// The file has changed and the provider has already announced it. Drop the recorded
		// mtime rather than spend a stat learning the new one; a later read serves this buffer.
		this.#bufferMtimeMs = undefined;
	}

	/**
	 * Flush and retire the fd.
	 *
	 * `#closing` is set synchronously so nothing new is accepted, while whatever is already
	 * queued still runs — with a FIFO there is no half-applied operation to strand, which is the
	 * problem the generation counter used to solve.
	 */
	close(): Promise<void> {
		if (this.#closed) return Promise.resolve();
		this.#closing = true;
		return this.#run(async () => {
			try {
				await this.#syncUnqueued();
			} finally {
				this.#closed = true;
				this.#closing = false;
			}
		});
	}

	// ---------------------------------------------------------------------- stat

	stat(): Promise<FsEntry> {
		return this.#submit("fstat", async () => {
			// A dirty handle's buffer IS the file as far as this fd is concerned, and node's
			// fstat on a dirty fd reports the real size. Synthesizing it is both more correct
			// than reporting the stale server size and one round trip cheaper.
			if (this.#dirty && this.#buffer) {
				const now = Date.now();
				return {
					path: this.path,
					name: basename(this.path),
					uid: "",
					isDir: false,
					isSymlink: false,
					size: this.#buffer.length,
					modifiedMs: now,
					createdMs: now,
					accessedMs: now,
				};
			}
			return this.#fs.stat(this.#ctx("fstat"), this.path);
		});
	}

	utimes(atimeMs: number, mtimeMs: number): Promise<boolean> {
		return this.#submit("futimes", async () => {
			return this.#fs.utimes(this.#ctx("futimes"), this.path, atimeMs, mtimeMs);
		});
	}

	/**
	 * A stream over this fd.
	 *
	 * Serves the handle's own buffer when it is dirty, because those bytes are the file as far
	 * as this fd is concerned and the backend has not seen them yet — which is exactly the case
	 * `createReadStream({ fd })` exists for.
	 */
	openRead(range?: { start: number; end?: number }): Promise<ProviderStream> {
		return this.#submit("read", async () => {
			this.#assertCanRead();
			if (this.#dirty && this.#buffer)
				return streamOfBytes(this.#buffer, range);
			return this.#fs.openRead(this.#ctx("read"), this.path, range);
		});
	}
}

export class HandleRegistry {
	#fs: Facade;
	#handles = new Map<number, Handle>();
	#nextFd = FIRST_FD;

	constructor(fs: Facade) {
		this.#fs = fs;
	}

	get size() {
		return this.#handles.size;
	}

	/**
	 * `open`, including the create/truncate/exclusive semantics of the flags.
	 *
	 * `hasNativeRange` decides the read strategy for the fd's lifetime: with a real positioned
	 * read the handle keeps a byte-range cache, and without one it buffers the whole file once.
	 * Getting that wrong in the over-claiming direction is quadratic — a derived range slices a
	 * fresh whole-file read, so a positioned-read loop would re-read the file per chunk.
	 */
	async open(
		path: string,
		flags: OpenFlags,
		hasNativeRange: boolean,
		owner?: string
	): Promise<{ fd: number; entry: FsEntry | undefined }> {
		if (this.#handles.size >= MAX_OPEN) {
			throw fsError("EMFILE", { syscall: "open", path });
		}
		const c: WireCtx = { syscall: "open", reportPath: path };

		let existing: FsEntry | undefined;
		try {
			existing = await this.#fs.stat(c, path);
		} catch (err) {
			const code = (err as NodeJS.ErrnoException).code;
			if (code !== "ENOENT" && code !== "ENOTDIR") throw err;
		}

		if (flags.exclusive && existing)
			throw fsError("EEXIST", { syscall: "open", path });
		if (!existing && !flags.create)
			throw fsError("ENOENT", { syscall: "open", path });

		let emptied = false;
		if ((!existing && flags.create) || (existing && flags.truncateOnOpen)) {
			await this.#fs.writeFile(c, path, new Uint8Array(0));
			emptied = true;
		}

		const fd = this.#nextFd++;
		const handle = new Handle(fd, path, flags, this.#fs, hasNativeRange, owner);
		if (emptied) handle.seedEmptied();
		else handle.seedExisting(existing?.size ?? 0);
		this.#handles.set(fd, handle);
		return { fd, entry: existing };
	}

	get(fd: number, syscall: string): Handle {
		const handle = this.#handles.get(fd);
		if (!handle || handle.closed) throw fsError("EBADF", { syscall });
		return handle;
	}

	/**
	 * Flush whatever is buffered for `path`, so an operation addressing the file *by path* sees
	 * bytes that were written through an open fd.
	 *
	 * Without this the buffering above is observable as data loss. A program writes to an fd and
	 * then reads the same file by path — a log written on one descriptor and tailed by name is the
	 * common shape — and gets the pre-write contents, with the write having reported success and
	 * `fstat(fd)` even confirming the new size. `copyFile` and `rename` are worse: they would move
	 * the stale bytes and the later flush would then write over the result.
	 *
	 * Writes stay batched for the case the buffer exists to serve — a run of small writes with
	 * nothing else looking at the file — because this only does work when there is a dirty handle
	 * for the exact path being asked about.
	 */
	async flushPath(path: string): Promise<void> {
		let pending: Promise<void>[] | undefined;
		for (const handle of this.#handles.values()) {
			if (handle.closed || !handle.dirty || handle.path !== path) continue;
			(pending ??= []).push(handle.sync());
		}
		if (pending) await Promise.all(pending);
	}

	async close(fd: number): Promise<void> {
		const handle = this.#handles.get(fd);
		if (!handle) throw fsError("EBADF", { syscall: "close" });
		// Removed up front, so the fd stops resolving the moment close is requested — node
		// throws EBADF for a closed fd rather than queueing behind the flush.
		this.#handles.delete(fd);
		await handle.close();
	}

	/**
	 * Drop the handles one session holds, or every handle when no session is named.
	 *
	 * The reason this exists at all: a handle now lives on the host and outlives the worker that
	 * opened it, so without an explicit teardown at `exit`/`terminate`/`pagehide` every run
	 * leaks its open files — and for a memory mount that means leaking the contents of unlinked
	 * files, which are kept alive on purpose for exactly as long as a handle refers to them.
	 *
	 * The `owner` argument is what makes a shared `NodeVfs` survive a short-lived worker. Several
	 * workers may run against one filesystem — that is explicitly allowed, and it is the whole
	 * shape of a page that spawns a worker per child process — and one of them terminating must
	 * not close descriptors the others are still reading. Clearing the map unconditionally, which
	 * is what this did, presents as an unrelated later command failing EBADF partway through.
	 *
	 * A handle with no owner is left alone by a scoped call: nothing opens one today, and leaking
	 * it is a great deal cheaper than closing a descriptor that belongs to somebody else.
	 *
	 * Buffers are **not** flushed. A worker that died did not ask for its dirty writes to land,
	 * and inventing a flush would publish half-written files no program asked to publish.
	 */
	closeAll(owner?: string): void {
		if (owner === undefined) {
			this.#handles.clear();
			return;
		}
		for (const [fd, handle] of this.#handles) {
			if (handle.owner === owner) this.#handles.delete(fd);
		}
	}
}
