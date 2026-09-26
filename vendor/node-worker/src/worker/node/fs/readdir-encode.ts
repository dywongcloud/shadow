// Turning a directory entry into what `readdir` actually yields.
//
// All that is left of what used to also hold the puterfs paging and depth-horizon walk — that
// moved to the host with the provider it belongs to (src/lib/vfs/puter-readdir.ts). What stays
// is the part that builds node's shapes, which is this side's job.

import nodeBuffer from "../buffer";
import nodePath from "../path";
import { Dirent } from "./classes";
import type { FsEntry } from "./util";

let Buffer = nodeBuffer.Buffer;

export interface EncodeOptions {
	encoding?: BufferEncoding | "buffer" | null;
	withFileTypes?: boolean;
	recursive?: boolean;
}

/**
 * Turn one entry into what `readdir` actually yields, per
 * `node_core/lib/fs.js` (`handleDirents` / `handleFilePaths`):
 *
 * - `withFileTypes`: a `Dirent` whose `name` is the *base* name and whose
 *   `parentPath` is the full containing directory.
 * - `recursive` without `withFileTypes`: the path **relative to the directory
 *   that was read** (`"a/b.txt"`), not the base name.
 * - otherwise: the base name.
 */
export function encodeEntry(
	entry: FsEntry,
	root: string,
	options: EncodeOptions
): any {
	let raw =
		options.recursive && !options.withFileTypes
			? nodePath.relative(root, entry.path)
			: entry.name;

	let nameBuf = Buffer.from(raw, "utf8");
	let name: string | Buffer =
		options.encoding === "buffer"
			? nameBuf
			: nameBuf.toString(options.encoding || undefined);

	return options.withFileTypes ? new Dirent(name, entry) : name;
}

/**
 * How many path segments `p` sits below `root`; direct children are 1, and -1 means `p` isn't
 * under `root` at all. Re-exported from the shared module, where the host walker needs it too.
 */
export { relDepth, MAX_DEPTH } from "../../../vfs/path";
