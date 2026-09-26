import { depromisify } from "../utils";
import { fsConstants } from "./util";
import { Stats, StatsFs, Dirent, Dir } from "./classes";
import { promisesToDepromisify as promises1 } from "./promises";
import { promisesRemaining as promises2 } from "./promises-sync";
import { fsSync } from "./sync";
import { fdOps } from "./fd";
import { watch, watchFile, unwatchFile } from "./watch";
import {
	ReadStream,
	WriteStream,
	createReadStream,
	createWriteStream,
} from "./streams";
import { Utf8Stream } from "./utf8-stream";
import { glob, globSync } from "./glob";

type NodeFs = typeof import("node:fs");

let promises: typeof promises1 & typeof promises2 = Object.assign(
	{},
	promises1,
	promises2
);

let callbackFs = depromisify(promises1);

// `realpath` carries a `.native` variant. puterfs resolves nothing, so it's the
// same function — but it has to be attached before the `satisfies` check below,
// not patched on afterwards, or the type won't carry it.
let realpath = Object.assign(callbackFs.realpath, {
	native: callbackFs.realpath,
}) as unknown as NodeFs["realpath"];

// Deprecated callback `fs.exists`: its callback takes a lone boolean (not the
// error-first shape depromisify produces), so it's written out by hand.
function exists(path: any, callback: (exists: boolean) => void) {
	try {
		callback(fsSync.existsSync(path));
	} catch {
		callback(false);
	}
}
// node exposes util.promisify(fs.exists) via this hook (resolves a boolean).
(exists as any).__promisify__ = (path: any) =>
	new Promise<boolean>((resolve) => exists(path, resolve));

// Each class is typed in `./classes.ts` as `Pick<NodeFs[X], keyof NodeFs[X]>
// & { new(puterShapedArgs): any }` so static members and instance shape are
// pinned to node, but our internal puter-shaped construction is allowed.
// `Pick<X, keyof X>` doesn't carry over the private construct signature node
// uses on these classes, so the `as any` here is the irreducible bit — TS
// treats private constructors nominally and we can't reproduce the brand.
let fs = {
	Dir: Dir as any,
	Dirent: Dirent as any,
	Stats: Stats as any,
	StatsFs: StatsFs as any,
	constants: fsConstants,
	promises,
	glob,
	globSync,
	// Backed by puter's `item.*` socket.io feed rather than an inotify-alike;
	// see ./watch.ts. `fs.FSWatcher`/`fs.StatWatcher` are deliberately not
	// re-exported here: node's typings declare them as interfaces, not values, so
	// adding them trips `satisfies`'s excess-property check.
	watch,
	watchFile,
	unwatchFile,
	createReadStream,
	createWriteStream,
	ReadStream: ReadStream as any,
	WriteStream: WriteStream as any,
	Utf8Stream,
	...fsSync,
	...callbackFs,
	// fd family overrides depromisify's `open` (which resolves a FileHandle) with
	// the callback contract that yields a numeric fd, and adds read/write/etc.
	...fdOps,
	// `fs.exists`'s callback takes a lone boolean, not depromisify's error-first
	// shape, so define it after the spreads.
	exists: exists as any,
	realpath,
} satisfies typeof import("node:fs");

// `promises.realpath` carries the same `.native` alias; the sync and callback
// forms attach theirs where they're built (./sync.ts and `realpath` above).
(promises.realpath as any).native = promises.realpath;

export default fs;
