// node:vm, minus the parts that need a real V8 context.
//
// Upstream `lib/vm.js` is a thin wrapper over `internalBinding('contextify')`,
// which is backed by V8's Context/Script C++ API. There is no wasm to compile
// and no Web Worker primitive that mints a fresh JS realm, so the "contextify"
// half of vm cannot be reproduced here.
//
// What IS faithfully reproducible is everything that runs in the *current*
// realm, because that's just `eval` / `new Function`:
//   - runInThisContext / Script#runInThisContext  -> indirect eval
//   - compileFunction (incl. params + contextExtensions) -> new Function
//   - createScript (deprecated alias for new Script)
//   - isContext / constants
//
// Everything that needs a separate global object throws:
//   - createContext, runInContext, runInNewContext
//   - Script#runInContext, Script#runInNewContext, Script#createCachedData
//   - measureMemory
//   - vm.Module / SourceTextModule / SyntheticModule are left UNDEFINED (as in
//     stock node without --experimental-vm-modules) so feature-detection falls
//     back cleanly instead of hitting a throw mid-flight.
//
// `Script` is `ContextifyScript` from internalBinding('contextify') rather than
// a second implementation of the same indirect eval. Upstream `lib/repl.js:106`
// is why: it lifts `vm.Script.prototype.runInThisContext` off the *public* vm
// and then applies it to a script the internal binding built, so the two have to
// be one function over one set of fields.

import { ContextifyScript } from "../node-core/internal-binding/contextify";

// Indirect eval: calling through any binding other than the `eval` identifier
// runs the code in global scope and returns the completion value of the final
// statement — exactly runInThisContext's contract (`var`/function declarations
// leak to the global object; lexical declarations stay script-scoped).
const indirectEval: (code: string) => any = eval;

function needsContext(name: string): never {
	throw new Error(
		`node:vm.${name} requires a separate V8 context, which is not available in this runtime`
	);
}

function normalizeOptions(options?: any): Record<string, any> {
	if (typeof options === "string") return { filename: options };
	return options || {};
}

// Appending a sourceURL comment is a no-op on the completion value (comments
// aren't statements) but gives the code a name in stack traces / devtools.
function withSourceURL(code: string, filename?: string): string {
	return filename ? `${code}\n//# sourceURL=${filename}` : code;
}

function runInThisContext(code: string, options?: any): any {
	const { filename } = normalizeOptions(options);
	return indirectEval(withSourceURL(String(code), filename));
}

class Script extends ContextifyScript {
	constructor(code: string, options?: any) {
		const { filename } = normalizeOptions(options);
		// cachedData / importModuleDynamically / lineOffset / timeout are
		// accepted and ignored — none change same-realm execution. The base
		// compiles eagerly, so a syntax error is raised here, as node does.
		super(String(code), filename);
	}

	// runInThisContext and the throwing runInContext are inherited.

	runInNewContext(_contextObject?: any, _options?: any): never {
		return needsContext("Script.prototype.runInNewContext");
	}

	createCachedData(): never {
		return needsContext("Script.prototype.createCachedData");
	}
}

// Deprecated alias for `new Script(...)`.
function createScript(code: string, options?: any): Script {
	return new Script(code, options);
}

function compileFunction(code: string, params?: string[], options?: any): Function {
	const opts = normalizeOptions(options);
	if (opts.parsingContext !== undefined) {
		needsContext("compileFunction with a parsingContext");
	}

	const paramList = Array.isArray(params) ? params : [];
	const body = withSourceURL(String(code), opts.filename);
	const ctxExts = opts.contextExtensions;

	if (ctxExts === undefined || (Array.isArray(ctxExts) && ctxExts.length === 0)) {
		return new Function(...paramList, body);
	}

	if (!Array.isArray(ctxExts)) {
		throw new TypeError(
			'The "options.contextExtensions" property must be an Array.'
		);
	}

	// contextExtensions put each object on the compiled function's scope chain.
	// We mirror that in-realm with nested `with` blocks: an outer factory closes
	// over the extension objects, and returns the real function wrapped so their
	// properties resolve as free variables — the same effect V8 produces.
	const extNames = ctxExts.map((_, i) => `__vmExt${i}`);
	let wrapped = `return function (${paramList.join(", ")}) {\n`;
	for (const name of extNames) wrapped += `with (${name}) {\n`;
	wrapped += body + "\n";
	for (let i = 0; i < extNames.length; i++) wrapped += "}\n";
	wrapped += "}";

	const factory = new Function(...extNames, wrapped);
	return factory(...ctxExts);
}

function isContext(object: any): boolean {
	if (typeof object !== "object" || object === null) {
		throw new TypeError('The "object" argument must be of type object.');
	}
	// We never contextify anything, so nothing is ever a vm context.
	return false;
}

const constants = {
	USE_MAIN_CONTEXT_DEFAULT_LOADER: 0,
	DONT_CONTEXTIFY: 1,
	measureMemory: {
		mode: { SUMMARY: 0, DETAILED: 1 },
		execution: { DEFAULT: 0, EAGER: 1 },
	},
};

const vm = {
	Script,
	createScript,
	runInThisContext,
	compileFunction,
	isContext,
	constants,
	createContext: needsContext.bind(null, "createContext"),
	runInContext: needsContext.bind(null, "runInContext"),
	runInNewContext: needsContext.bind(null, "runInNewContext"),
	measureMemory: needsContext.bind(null, "measureMemory"),
};

export default vm as unknown as typeof import("node:vm");
