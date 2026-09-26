// `/dev`, enough of it to be useful: `null`, `zero`, `full`.
//
// `>/dev/null` is not an optional nicety — it is the single most common redirect in shell code, and
// a shell running against a filesystem has to *write to the file* to honour it. Without the file
// every such command fails, and it fails at the redirect rather than in the command, so the error
// names a path the author never typed. That is exactly what happened here: Claude Code 2.1.220
// prefixes every command it builds with `{ … } >/dev/null 2>&1 || true`, so `true` and `echo hello`
// failed identically with "no such file or directory: /dev/null".
//
// `os.devNull` already reported "/dev/null" (src/worker/node/os.ts), so the runtime was naming a
// path it did not provide.
//
// Writes are **discarded**, not stored. A memory-backed file would have worked for the redirect and
// then grown without bound the first time something piped real volume into it — which is the kind of
// slow, invisible wrongness a device node exists to avoid.

import { fsError } from "../../vfs/errno";
import type { FsEntry, Listing, WireCtx } from "../../vfs/entry";
import type { VfsProvider } from "../../vfs/provider";
import { NO_EVENTS, type FsEvents } from "./events";

/** How much `/dev/zero` will hand over in one read. Unbounded is not an option in a browser. */
const ZERO_READ_LIMIT = 1024 * 1024;

type DeviceName = "null" | "zero" | "full";

const DEVICES: ReadonlySet<string> = new Set(["null", "zero", "full"]);

function localName(path: string): string {
	// The facade hands the provider a mount-local path: "/" for /dev itself, "/null" below it.
	return path.replace(/^\/+/, "").replace(/\/+$/, "");
}

function entry(path: string, name: string, isDir: boolean): FsEntry {
	return {
		path,
		name,
		uid: "",
		isDir,
		isSymlink: false,
		size: 0,
		modifiedMs: 0,
		createdMs: 0,
		accessedMs: 0,
	};
}

export function createDevProvider(
	opts: { events?: FsEvents } = {}
): VfsProvider {
	const events = opts.events ?? NO_EVENTS;
	void events;

	const device = (path: string, ctx: WireCtx): DeviceName => {
		const name = localName(path);
		if (!DEVICES.has(name)) throw fsError("ENOENT", ctx);
		return name as DeviceName;
	};

	return {
		name: "dev",

		async stat(ctx, path): Promise<FsEntry> {
			const name = localName(path);
			if (name === "") return entry(path, "dev", true);
			if (!DEVICES.has(name)) throw fsError("ENOENT", ctx);
			return entry(path, name, false);
		},

		async readdir(ctx, path): Promise<Listing> {
			if (localName(path) !== "") throw fsError("ENOTDIR", ctx);
			return {
				entries: [...DEVICES].map((name) => entry(`/${name}`, name, false)),
				complete: true,
			};
		},

		async readFile(ctx, path): Promise<Uint8Array> {
			const name = device(path, ctx);
			// `null` is at EOF immediately; `zero` and `full` read as zeroes. A real `/dev/zero` never
			// ends, which a whole-file read cannot express, so it is capped — a caller wanting a
			// stream of zeroes is better served by asking for a length.
			return name === "null"
				? new Uint8Array(0)
				: new Uint8Array(ZERO_READ_LIMIT);
		},

		async writeFile(ctx, path, data): Promise<void> {
			const name = device(path, ctx);
			// `/dev/full` exists to fail, and is the only honest way to test an ENOSPC path.
			if (name === "full" && data.byteLength > 0) throw fsError("ENOSPC", ctx);
			// null and zero swallow everything, and store nothing.
		},

		async mkdir(ctx, path, opts): Promise<string | undefined> {
			// `mkdir -p /dev` has to succeed, because /dev already exists — that is what `recursive`
			// means, and refusing it is not a theoretical nicety: any writer that ensures the parent
			// directory before writing (which is most of them) would otherwise fail on
			// `>/dev/null` with the mkdir's error, naming a permission problem for a write that is
			// perfectly allowed.
			if (localName(path) === "") {
				if (opts.recursive) return undefined;
				throw fsError("EEXIST", ctx);
			}
			// A new device node, on the other hand, is not something to invent.
			throw fsError("EPERM", ctx);
		},

		async rm(ctx): Promise<void> {
			throw fsError("EPERM", ctx);
		},

		async rename(ctx): Promise<void> {
			throw fsError("EPERM", ctx);
		},

		async utimes(ctx, path): Promise<boolean> {
			// The path has to exist for the ENOENT to be right, but a device has no timestamps to set.
			device(path, ctx);
			return false;
		},
	};
}
