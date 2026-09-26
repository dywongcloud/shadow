// Minimal `internal/modules/cjs/loader` override.
//
// Upstream this is node's whole CommonJS loader, and pulling it in pulls the ESM
// loader, the package.json reader and the TypeScript stripper behind it — several
// hundred modules that exist to do a job this runtime already does its own way, in
// `src/worker/module/`. The same reasoning as the `internal/modules/esm/resolve`
// stub beside it, which says so in as many words.
//
// What reaches this is `lib/repl.js` and its two helpers, and between them they
// want six things. `Module.prototype.require` is the one that has to genuinely
// work: `repl.js:1173` hands `makeRequireFunction(replModule)` to the prompt as its
// `require`, so this is what a user typing `require("fs")` ends up in.

import { createRequire, require as requireFromCwd } from "../../../../../module/cjs";

const SEP = "/";

/**
 * The classic node_modules walk: every directory from `from` up to the root, each
 * with "node_modules" appended. Completion offers these as places a bare specifier
 * might live, and `repl.js:1161` puts them on the REPL module.
 */
function nodeModulePaths(from) {
	const parts = String(from).split(SEP).filter(Boolean);
	const paths = [];
	for (let i = parts.length; i >= 0; i--) {
		if (parts[i - 1] === "node_modules") continue;
		paths.push(SEP + [...parts.slice(0, i), "node_modules"].join(SEP));
	}
	return paths;
}

function Module(id = "", parent = undefined) {
	this.id = id;
	this.path = ".";
	this.exports = {};
	this.parent = parent;
	this.filename = null;
	this.loaded = false;
	this.children = [];
	this.paths = [];
}

/**
 * Resolution, through this runtime's own resolver rather than a second one.
 *
 * `filename` is whatever `fixReplRequire` last set — `internal/repl/utils.js:798`
 * points it at the working directory — so a relative `require("./lib.js")` at the
 * prompt resolves against the directory the command was run from, which is what
 * someone typing it means.
 */
Module.prototype.require = function require(request) {
	const from = this.filename;
	return from ? createRequire(from)(request) : requireFromCwd(request);
};

Module._nodeModulePaths = nodeModulePaths;

Module._resolveLookupPaths = function _resolveLookupPaths(request, parent) {
	if (request === "<repl>" || !parent?.filename) return nodeModulePaths(SEP);
	return nodeModulePaths(parent.filename);
};

// Completion reads the keys to know which suffixes to try when offering the
// contents of a directory; nothing here dispatches on the values.
Module._extensions = { __proto__: null, ".js": null, ".json": null, ".node": null };

// NODE_PATH, which this runtime has no notion of.
Module.globalPaths = [];

/*
 * Left empty, and it costs one convenience.
 *
 * `internal/repl/utils.js:822` reads this at *its* module top level to build the
 * list `addBuiltinLibsToObject` turns into lazy globals — the reason `fs.readFile`
 * works at a real node prompt without requiring it first. The list lives in
 * `src/worker/node/index.ts`, which imports repl, which imports this: by the time
 * that read happens the registry does not exist yet, so there is nothing truthful
 * to return. A getter would answer correctly later and still be empty at the only
 * moment anything asks.
 *
 * `require("fs")` at the prompt is unaffected — that goes through the resolver
 * above. Only the bare-name globals are missing.
 */
Module.builtinModules = [];

export { Module };

export default { Module };
