// The puterfs backend.
//
// One copy of each request shape. Nothing here touches node's `Stats`/`Dirent` — those are
// node's shapes and belong to the fs surface inside the worker, while this layer speaks
// `FsEntry`. That is a layering rule and a cycle guard at the same time.

import { fsError } from "../../vfs/errno";
import { basename, dirname } from "../../vfs/path";
import type { FsEntry, Listing, ReaddirOpts, WireCtx } from "../../vfs/entry";
import type { ProviderStream, VfsProvider } from "../../vfs/provider";
import { NO_EVENTS, type FsEvents } from "./events";
import {
	failPuter,
	getRandomId,
	normalizeFsEntry,
	readUrl,
	statRequest,
	type PuterApi,
} from "./puter-http";
import { readdirPages, readdirTree } from "./puter-readdir";

/**
 * The api can only set a timestamp to *now* (`POST /touch` takes `set_modified_to_now` and
 * friends — there is no field for an arbitrary value), so this decides whether a requested
 * time is close enough to now to be worth a round trip. Two seconds covers the gap between
 * a caller reading the clock and us issuing the request, which is what makes `touch(1)`-style
 * callers work.
 */
const TOUCH_NOW_TOLERANCE_MS = 2000;

function isEffectivelyNow(epochMs: number): boolean {
	return Math.abs(Date.now() - epochMs) <= TOUCH_NOW_TOLERANCE_MS;
}

/**
 * The multipart body for a whole-file write. puterfs has no partial-write primitive —
 * `POST /batch` with `op: "write"` and `overwrite: true` replacing the entire file is the
 * only way to change one — so this is the sole write path, and every caller that looks like
 * an incremental write is buffering to reach it.
 */
function writeBody(path: string, data: Uint8Array) {
	const name = basename(path);
	const parent = dirname(path);
	return (form: FormData) => {
		const opId = getRandomId();
		form.append("operation_id", opId);
		form.append(
			"fileinfo",
			JSON.stringify({
				name,
				type: "application/octet-stream",
				size: data.byteLength,
			})
		);
		form.append(
			"operation",
			JSON.stringify({
				op: "write",
				dedupe_name: false,
				overwrite: true,
				operation_id: opId,
				path: parent,
				name,
				item_upload_id: 0,
			})
		);
		// A fresh copy, because what arrives is a view into the request frame and a `Blob`
		// must not alias a buffer the transport may reuse.
		form.append("file", new File([new Uint8Array(data)], name));
	};
}

export interface PuterProviderOptions {
	api: PuterApi;
	events?: FsEvents;
}

export function createPuterProvider(opts: PuterProviderOptions): VfsProvider {
	const { api } = opts;
	const events = opts.events ?? NO_EVENTS;

	/**
	 * mkdir, including the implicitly-created parents a recursive mkdir reports in
	 * `parent_dirs_created`. node's watchers see each new directory, so emitting only the
	 * leaf would hide the rest of the chain.
	 */
	function emitMkdir(path: string, res: any) {
		const parents = res?.parent_dirs_created;
		if (Array.isArray(parents)) {
			for (const parent of parents) {
				if (typeof parent === "string" && parent !== path)
					events.add(parent, true);
			}
		}
		events.add(path, true);
	}

	return {
		name: "puter",

		async stat(ctx, path): Promise<FsEntry> {
			const res = await api.fetch("stat", statRequest(path));
			const body = res.json();
			if (!res.ok) failPuter(body, ctx);
			return normalizeFsEntry(body);
		},

		async readdir(ctx, path, opts?: ReaddirOpts): Promise<Listing> {
			if (opts?.recursive && opts.depth === undefined) {
				// No depth given means "everything", which needs the re-rooting horizon walk
				// rather than a single capped request.
				return { entries: await readdirTree(api, ctx, path), complete: true };
			}
			return readdirPages(api, ctx, path, {
				recursive: opts?.recursive,
				depth: opts?.depth,
				maxEntries: opts?.maxEntries,
			});
		},

		async readFile(ctx, path): Promise<Uint8Array> {
			const res = await api.fetch(readUrl(path));
			if (!res.ok) failPuter(res.json(), ctx);
			return res.bytes;
		},

		/**
		 * Ranged read via the HTTP `Range` header.
		 *
		 * NOT `?offset=&byte_count=`: the api's `/read` handler ignores those query
		 * parameters and answers with the *whole file*, which is worse than an error — a
		 * caller reading at a non-zero position would silently get bytes from offset 0, and
		 * a stream would never see EOF because every read returns data.
		 *
		 * The cost is a CORS preflight, since a custom header makes this a non-simple GET.
		 * Only positioned reads pay it; whole-file reads go through `readFile`.
		 */
		async readRange(ctx, path, offset, length): Promise<Uint8Array> {
			const end = offset + length - 1;
			const res = await api.fetch(readUrl(path), undefined, undefined, {
				Range: `bytes=${offset}-${end}`,
			});
			if (!res.ok) {
				// 416 means the range starts at or past EOF, which for a positioned read is
				// simply "no bytes there" — what libuv reports as 0.
				if (res.status === 416) return new Uint8Array(0);
				failPuter(res.json(), ctx);
			}
			// A 200 means the server ignored the range and sent everything; slicing keeps
			// this correct if that ever regresses.
			return res.status === 206
				? res.bytes
				: res.bytes.subarray(offset, offset + length);
		},

		/**
		 * A whole file (or a window of one) over a single streamed GET, rather than a ranged
		 * read per chunk — the difference between 1 and `ceil(size / highWaterMark)` api
		 * calls for a large file.
		 *
		 * The 416 and 200-vs-206 handling that used to sit in the worker's `ReadStream`
		 * belongs here: it is knowledge about *this* backend, and having it upstairs meant
		 * every mount paid attention to puterfs's quirks.
		 */
		async openRead(ctx, path, range): Promise<ProviderStream> {
			const ranged =
				range !== undefined && (range.start !== 0 || range.end !== undefined);
			const headers = ranged
				? {
						Range: `bytes=${range!.start}-${range!.end === undefined ? "" : range!.end}`,
					}
				: undefined;
			const res = await api.fetchStream(readUrl(path), undefined, headers);

			if (!res.ok) {
				// 416: `start` is at or past EOF. node's createReadStream yields no data for
				// that rather than erroring.
				if (res.status === 416) return { size: 0, stream: emptyStream() };
				let body: any;
				try {
					body = JSON.parse(await res.text());
				} catch {
					body = undefined;
				}
				failPuter(body, ctx);
			}
			if (!res.body) return { size: 0, stream: emptyStream() };

			// A 200 for a ranged request means the server ignored the Range; trim so the
			// window is still honored.
			if (ranged && res.status !== 206) {
				const all = new Uint8Array(await res.arrayBuffer());
				const start = range!.start;
				const end = range!.end === undefined ? all.length : range!.end + 1;
				const slice = new Uint8Array(all.subarray(start, end));
				return {
					size: slice.length,
					stream: new ReadableStream<Uint8Array>({
						start(c) {
							if (slice.length) c.enqueue(slice);
							c.close();
						},
					}),
				};
			}
			const len = Number(res.headers.get("content-length"));
			return {
				size: Number.isFinite(len) && len >= 0 ? len : undefined,
				stream: res.body,
			};
		},

		async writeFile(ctx, path, data): Promise<void> {
			const res = await api.fetch("batch", writeBody(path, data));
			// `/batch` reports per-operation success in its body and answers 218 when any
			// operation failed, so the transport-level `ok` is not the whole story.
			const result = res.json()?.results?.[0];
			if (result?.success === false) failPuter(result, ctx);
			if (!res.ok && !result) failPuter(res.json(), ctx);
			// This is the moment the file changes as far as puterfs is concerned.
			events.write(path);
		},

		async mkdir(ctx, path, opts): Promise<string | undefined> {
			const res = await api.fetch("mkdir", {
				parent: dirname(path),
				path: basename(path),
				overwrite: opts.recursive,
				dedupe_name: false,
				create_missing_parents: opts.recursive,
			});
			const body = res.json();
			if (!res.ok) failPuter(body, ctx);
			emitMkdir(path, body);
			// node returns the first directory created, or undefined. puterfs doesn't
			// reliably report it, so guard rather than throw.
			return opts.recursive ? body?.parent_dirs_created?.[0] : undefined;
		},

		async rm(ctx, path, opts): Promise<void> {
			const res = await api.fetch("delete", {
				paths: [path],
				recursive: opts.recursive,
				descendants_only: false,
			});
			if (!res.ok) {
				// `force` swallows the failure — but nothing was removed, so no event.
				if (opts.force) return;
				failPuter(res.json(), ctx);
			}
			events.remove(path, opts.recursive);
		},

		/**
		 * `rename`, including POSIX's replace-the-destination behaviour.
		 *
		 * This has to replace an existing destination, because write-to-temp-then-rename is how
		 * every atomic save in the ecosystem works — `write-file-atomic`, fs-extra, npm, git, and
		 * Claude Code's own `.claude.json`, which failed with EEXIST on *every* save while this
		 * refused the collision.
		 *
		 * Puter's `move` only replaces when `overwrite` is set, and its `overwrite` is stronger than
		 * rename is allowed to be: it `remove(collision, { recursive: true })`s whatever is in the
		 * way. rename must never do that. Replacing a directory with a file is an error, and so is
		 * replacing a non-empty directory — not a licence to delete a tree.
		 *
		 * So the collision is resolved rather than pre-empted: try without `overwrite`, and only if
		 * something is actually in the way look at what it is. The common case stays one round trip,
		 * and a stat is paid for only when there is a decision to make.
		 */
		async rename(ctx, from, to): Promise<void> {
			const move = (overwrite: boolean) =>
				api.fetch("move", {
					source: from,
					destination: dirname(to),
					new_name: basename(to),
					overwrite,
					create_missing_parents: false,
				});

			let res = await move(false);
			if (!res.ok && res.json()?.code === "item_with_same_name_exists") {
				const [source, dest] = await Promise.all([
					api.fetch("stat", statRequest(from)),
					api.fetch("stat", statRequest(to)),
				]);
				const sourceIsDir =
					source.ok && !!normalizeFsEntry(source.json()).isDir;
				const destIsDir = dest.ok && !!normalizeFsEntry(dest.json()).isDir;

				if (destIsDir && !sourceIsDir) throw fsError("EISDIR", ctx);
				if (!destIsDir && sourceIsDir) throw fsError("ENOTDIR", ctx);
				if (destIsDir && sourceIsDir) {
					// A directory may only take the place of an empty one.
					const listing = await readdirPages(api, ctx, to, { maxEntries: 1 });
					if (listing.entries.length > 0) throw fsError("ENOTEMPTY", ctx);
				}
				res = await move(true);
			}
			if (!res.ok) failPuter(res.json(), ctx);
			events.move(from, to);
		},

		async copyFile(ctx, from, to, opts): Promise<void> {
			const res = await api.fetch("copy", {
				source: from,
				destination: dirname(to),
				new_name: basename(to),
				overwrite: opts.overwrite,
				dedupe_name: false,
			});
			if (!res.ok) failPuter(res.json(), ctx);
			events.add(to);
		},

		/**
		 * The only timestamp api is `POST /touch`, whose fields are
		 * `set_{modified,accessed,created}_to_now` — there is no field for a value. So
		 * "approximately now" is the only representable request, and anything else is
		 * reported as not applied rather than faked.
		 */
		async utimes(ctx, path, atimeMs, mtimeMs): Promise<boolean> {
			const setAccessed = isEffectivelyNow(atimeMs);
			const setModified = isEffectivelyNow(mtimeMs);
			if (!setAccessed && !setModified) return false;

			const res = await api.fetch("touch", {
				path,
				set_accessed_to_now: setAccessed,
				set_modified_to_now: setModified,
				create_missing_parents: false,
			});
			if (!res.ok) failPuter(res.json(), ctx);
			events.write(path);
			return true;
		},

		async statfs(ctx): Promise<{ used: number; capacity: number }> {
			// `body: {}` rather than omitting it — an empty JSON POST, which is what `/df`
			// expects. Presence of a body is what selects POST over GET.
			const res = await api.fetch("df", {});
			const body = res.json();
			if (!res.ok) failPuter(body, ctx);
			return body;
		},
	};
}

function emptyStream(): ReadableStream<Uint8Array> {
	return new ReadableStream<Uint8Array>({
		start(c) {
			c.close();
		},
	});
}
