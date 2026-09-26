// The `fs.glob` family, isolated in its own module for module-init ordering.
//
// Upstream `internal/fs/glob` needs readdir/lstat, and the modules that provide
// them (./sync.ts, ./promises.ts) are the ones that expose glob — so importing
// `Glob` from either of them puts glob.js inside a cycle, where its top-level
// `require` destructures run before their targets exist. Keeping the entry points
// here, imported only by ./index.ts, leaves glob.js strictly downstream of
// everything it needs.
//
// The other half of the fix is node-patches/0002-glob-lazy-fs.patch, which stops
// upstream from reaching readdir/lstat through the `fs` barrel (whose own
// forwarder reads properties off ./index.ts at load).

// @ts-ignore — upstream node JS, glob spec impl backed by minimatch
import { Glob } from "node-core:internal/fs/glob";

type NodeFs = typeof import("node:fs");

export let globSync = ((pattern: any, options?: any) =>
	new Glob(pattern, options).globSync()) as unknown as NodeFs["globSync"];

export let globPromise = ((pattern: any, options?: any) =>
	new Glob(pattern, options).glob()) as unknown as NodeFs["promises"]["glob"];

// Callback-style fs.glob: drains the async iterator and hands the array to the
// callback. Mirrors `node_core/lib/fs.js`'s `glob`.
function globImpl(pattern: any, options: any, callback?: any) {
	if (typeof options === "function") {
		callback = options;
		options = undefined;
	}
	(async () => {
		const out: any[] = [];
		for await (const entry of new Glob(pattern, options).glob())
			out.push(entry);
		return out;
	})().then(
		(res) => callback(null, res),
		(err) => callback(err)
	);
}

export let glob = globImpl as unknown as NodeFs["glob"];
