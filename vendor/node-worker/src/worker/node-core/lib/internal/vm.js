// Minimal `internal/vm` override.
//
// Upstream this is the seam between public `vm` and `internalBinding('contextify')`,
// and most of it is the import-module-dynamically plumbing: `registerImportModuleDynamically`
// reaches `internal/vm/module` and `internal/modules/esm/utils`, which bring the ESM
// loader, `run_main` and the TypeScript stripper with them. Same reasoning as the
// `internal/modules/esm/resolve` stub — this runtime has its own module system, in
// `src/worker/module/`, and node's is several hundred modules of dead weight here.
//
// Only `lib/repl.js:170` reaches this, and it wants one function. (Upstream `lib/vm.js`
// is not in the bundle either: the public `node:vm` is hand-written, in
// `src/worker/node/vm.ts`.)

import { ContextifyScript } from "../../internal-binding/contextify";

/**
 * Compile a script, which here means checking that it parses — see `ContextifyScript`.
 *
 * `importModuleDynamically` is accepted and dropped. Upstream registers it against the
 * script's host-defined options so that an `import()` *inside* compiled source can be
 * routed back to a loader; there are no host-defined options on an indirect `eval`, and
 * the callback repl.js supplies goes to the ESM cascade this runtime does not have. A
 * dynamic import typed at the prompt therefore takes the ordinary path rather than the
 * REPL's.
 */
export function makeContextifyScript(
	code,
	filename,
	lineOffset,
	columnOffset,
	cachedData,
	produceCachedData,
	parsingContext,
	hostDefinedOptionId,
	_importModuleDynamically
) {
	return new ContextifyScript(
		code,
		filename,
		lineOffset,
		columnOffset,
		cachedData,
		produceCachedData,
		parsingContext,
		hostDefinedOptionId
	);
}

/** Nothing is ever contextified here, so nothing is a context. */
export function isContext(_object) {
	return false;
}

export function registerImportModuleDynamically(_referrer, _importModuleDynamically) {}

export default { makeContextifyScript, isContext, registerImportModuleDynamically };
