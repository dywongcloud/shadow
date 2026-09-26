// Two providers stacked over one subtree.
//
// This is where layering lives, rather than in the mount table — the table stays
// one-provider-per-root and trivially correct, and the "which layer answers" question is a
// single independently testable module.
//
// The shape an archive mount will want: the zip as a read-only *lower* layer with real
// storage as the writable *upper* one, so `node_modules` reads come out of the archive
// while anything a build writes there (vite's dependency optimizer parking pre-bundled
// deps under `node_modules/.vite`) lands in real storage and survives a restart.
//
// ## No whiteouts
//
// There is nowhere to record "this lower-layer path was deleted": puterfs has no such
// concept and a sidecar marker file inside a user's node_modules is worse than the
// limitation. Two consequences, both intended for the archive case and both wrong for a
// general-purpose overlay:
//
//   - deleting an upper file that also exists below makes the lower version reappear;
//   - deleting a lower-only file reports EROFS.
//
// One thing that got *better* in moving host-side: `openRead` can now be forwarded. It
// used to be impossible — picking a layer needs a stat, and `openRead` sat outside the
// generator protocol with no way to perform one — so a union mount could not stream at
// all and `createReadStream` had to fall back. Here it is an ordinary `await`.

import { fsError } from "../../vfs/errno";
import { dirname } from "../../vfs/path";
import { streamOfBytes } from "./stream";
import type { FsEntry, Listing, ReaddirOpts, WireCtx } from "../../vfs/entry";
import type { ProviderStream, VfsProvider } from "../../vfs/provider";

export type WriteTarget =
	/** Everything is written to the upper layer. The archive mount. */
	| "upper"
	/**
	 * Written to whichever layer already holds the path, else the lower one. The root
	 * mount: a sparse memory layer shadows real files where it has them, but writing to an
	 * ordinary path still writes to ordinary storage.
	 */
	| "existing";

async function has(
	provider: VfsProvider,
	ctx: WireCtx,
	path: string
): Promise<FsEntry | undefined> {
	try {
		return await provider.stat(ctx, path);
	} catch (err) {
		const code = (err as NodeJS.ErrnoException).code;
		if (code === "ENOENT" || code === "ENOTDIR") return undefined;
		throw err;
	}
}

export function unionProvider(
	upper: VfsProvider,
	lower: VfsProvider,
	write: WriteTarget
): VfsProvider {
	function roFail(ctx: WireCtx): never {
		throw fsError("EROFS", ctx);
	}

	/** Which layer a mutation should go to. */
	async function writeLayer(ctx: WireCtx, path: string): Promise<VfsProvider> {
		if (write === "upper") return upper;
		if (await has(upper, ctx, path)) return upper;
		return lower;
	}

	/** Which layer a read should come from. */
	async function readLayer(ctx: WireCtx, path: string): Promise<VfsProvider> {
		return (await has(upper, ctx, path)) ? upper : lower;
	}

	const provider: VfsProvider = {
		name: `union(${upper.name},${lower.name})`,

		async stat(ctx, path): Promise<FsEntry> {
			const up = await has(upper, ctx, path);
			if (!up) return lower.stat(ctx, path);

			// A directory present in both layers is *one* directory — the union shows the
			// merged contents either way, so the only question is whose timestamps to
			// report, and the answer is the layer that primarily owns the data. With
			// `write: "existing"` the upper layer is a sparse overlay whose directories
			// exist only as scaffolding to hold a shadowing file, and reporting their
			// creation time in place of the real directory's mtime would be misleading.
			if (up.isDir && write === "existing") {
				const low = await has(lower, ctx, path);
				if (low && low.isDir) return low;
			}
			return up;
		},

		async readdir(ctx, path, opts?: ReaddirOpts): Promise<Listing> {
			let upList: Listing | undefined;
			let lowList: Listing | undefined;
			let lowErr: unknown;
			try {
				upList = await upper.readdir(ctx, path, opts);
			} catch (err) {
				const code = (err as NodeJS.ErrnoException).code;
				if (code !== "ENOENT" && code !== "ENOTDIR") throw err;
			}
			try {
				lowList = await lower.readdir(ctx, path, opts);
			} catch (err) {
				const code = (err as NodeJS.ErrnoException).code;
				if (code !== "ENOENT" && code !== "ENOTDIR") throw err;
				lowErr = err;
			}
			if (!upList && !lowList) {
				// Neither layer has it. Rethrow the error already in hand rather than
				// asking again — the lower layer is usually a network filesystem, and
				// re-running the listing to reproduce its ENOENT would double the cost of
				// every miss.
				throw lowErr;
			}

			// Merge by path, upper winning. Keyed on the full entry path rather than the
			// basename so a recursive listing dedupes correctly at every depth.
			const merged = new Map<string, FsEntry>();
			for (const e of lowList?.entries ?? []) merged.set(e.path, e);
			for (const e of upList?.entries ?? []) merged.set(e.path, e);
			return {
				entries: [...merged.values()],
				// Only exhaustive if both halves were.
				complete: (upList?.complete ?? true) && (lowList?.complete ?? true),
			};
		},

		async readFile(ctx, path): Promise<Uint8Array> {
			return (await readLayer(ctx, path)).readFile(ctx, path);
		},

		async readRange(ctx, path, offset, length): Promise<Uint8Array> {
			const target = await readLayer(ctx, path);
			if (target.readRange) return target.readRange(ctx, path, offset, length);
			const whole = await target.readFile(ctx, path);
			return whole.subarray(offset, offset + length);
		},

		async writeFile(ctx, path, data): Promise<void> {
			const target = await writeLayer(ctx, path);
			if (target === upper && write === "upper") {
				// The upper layer may not have the containing directory yet — the whole
				// point of the archive case is that `node_modules/...` exists only in the
				// lower layer until something writes there.
				await upper.mkdir(ctx, dirname(path), { recursive: true });
			}
			await target.writeFile(ctx, path, data);
		},

		async mkdir(ctx, path, opts): Promise<string | undefined> {
			const target = write === "upper" ? upper : await writeLayer(ctx, path);
			return target.mkdir(ctx, path, opts);
		},

		async rm(ctx, path, opts): Promise<void> {
			if (await has(upper, ctx, path)) return upper.rm(ctx, path, opts);
			// Present only below, where nothing can be removed and nothing can record that
			// it was.
			if (await has(lower, ctx, path)) {
				if (write === "upper") {
					if (opts.force) return;
					roFail(ctx);
				}
				return lower.rm(ctx, path, opts);
			}
			if (opts.force) return;
			return upper.rm(ctx, path, opts); // for its ENOENT
		},

		async rename(ctx, from, to): Promise<void> {
			if (await has(upper, ctx, from)) return upper.rename(ctx, from, to);
			if (write === "upper" && (await has(lower, ctx, from))) roFail(ctx);
			return lower.rename(ctx, from, to);
		},

		async utimes(ctx, path, atimeMs, mtimeMs): Promise<boolean> {
			if (await has(upper, ctx, path)) {
				return upper.utimes(ctx, path, atimeMs, mtimeMs);
			}
			if (write === "upper" && (await has(lower, ctx, path))) roFail(ctx);
			return lower.utimes(ctx, path, atimeMs, mtimeMs);
		},

		// statfs is intentionally absent: capacity belongs to whichever backend actually
		// stores bytes, and a union has no single answer.
	};

	// Attached conditionally, because the mount snapshot derives `canStream` from whether
	// this method exists and the worker uses that to pick a read strategy. Declaring it
	// unconditionally and throwing for a layer that cannot stream would make the
	// advertised capability a lie; declaring it never would stop a union mount from
	// streaming at all, which is the limitation that made `createReadStream` fall back to
	// per-chunk reads before this moved host-side.
	//
	// So: advertise iff *some* layer can stream, and synthesize for the case where the
	// layer actually chosen is the one that cannot.
	if (upper.openRead || lower.openRead) {
		provider.openRead = async (ctx, path, range): Promise<ProviderStream> => {
			const target = await readLayer(ctx, path);
			if (target.openRead) return target.openRead(ctx, path, range);
			return streamOfBytes(await target.readFile(ctx, path), range);
		};
	}

	return provider;
}
