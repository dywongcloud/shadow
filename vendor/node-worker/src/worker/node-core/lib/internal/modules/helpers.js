// Minimal `internal/modules/helpers` override.
//
// This one is not about weight, it is about a single line. Upstream's
// `getBuiltinModule` ends in `require(normalizedId)` — a `require` with a
// *variable* argument — and the worker build runs `@rollup/plugin-commonjs` with
// `dynamicRequireTargets: ["node_core/**/*.js"]`. One dynamic require anywhere in
// the graph therefore pulls in every JavaScript file in the node checkout:
// `benchmark/`, `deps/v8/test/`, the lot. The build stops finishing rather than
// failing, which is how this one presents.
//
// `lib/repl.js:88-91` takes two functions from here.

import { createRequire, require as requireFromCwd } from "../../../../module/cjs";

/**
 * The `require` the prompt gets.
 *
 * Bound to the REPL module's filename, which `internal/repl/utils.js:798` keeps
 * pointed at the working directory, so a relative `require("./lib.js")` typed at
 * the prompt resolves from where the command was run.
 */
export function makeRequireFunction(mod) {
	const req = (request) => mod.require(request);
	const bound = mod.filename ? createRequire(mod.filename) : requireFromCwd;

	req.resolve = (request) => {
		throw new Error("require.resolve is not implemented in this runtime");
	};
	req.main = undefined;
	req.extensions = { __proto__: null, ".js": null, ".json": null, ".node": null };
	req.cache = bound.cache ?? { __proto__: null };
	return req;
}

/**
 * `fs.readFileSync(...)` at the prompt without requiring `fs` first.
 *
 * Each builtin becomes a lazy, non-enumerable getter that replaces itself with the
 * module on first touch, and an assignment to the name drops the getter entirely —
 * so `fs = 1` at the prompt behaves like the plain global it looks like. Upstream's
 * shape, minus the deprecation plumbing.
 *
 * The list is read here rather than at module load, and that is the point: it lives
 * in `src/worker/node/index.ts`, which imports repl, which imports this — at load
 * time the registry does not exist yet. By the time `repl.start()` calls this it
 * does.
 */
export function addBuiltinLibsToObject(object, dummyModuleName) {
	let names;
	try {
		names = requireFromCwd("module").builtinModules ?? [];
	} catch {
		// No registry to read: the prompt simply has no bare-name globals.
		return;
	}

	for (const name of names) {
		if (name.startsWith("_") || name.startsWith("node:")) continue;
		if (Object.prototype.hasOwnProperty.call(object, name)) continue;

		const setReal = (value) => {
			delete object[name];
			object[name] = value;
		};

		Object.defineProperty(object, name, {
			__proto__: null,
			get: () => {
				const lib = requireFromCwd(name);
				Object.defineProperty(object, name, {
					__proto__: null,
					get: () => lib,
					set: setReal,
					configurable: true,
					enumerable: false,
				});
				return lib;
			},
			set: setReal,
			configurable: true,
			enumerable: false,
		});
	}
}

/** Upstream exports these beside the two above; nothing in this graph calls them. */
export function stringify(body) {
	return typeof body === "string" ? body : String(body);
}

export function toRealPath(requestPath) {
	return requestPath;
}

export default { makeRequireFunction, addBuiltinLibsToObject, stringify, toRealPath };
