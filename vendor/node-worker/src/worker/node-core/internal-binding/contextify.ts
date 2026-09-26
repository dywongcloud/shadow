// `internalBinding('contextify')` — the slice of V8's Context/Script bridge that
// upstream `internal/vm.js` and `lib/repl.js` actually reach for.
//
// There is no V8 API here and no Web Worker primitive that mints a fresh JS
// realm, so the "contextify" half — a separate global object — cannot be
// reproduced; see `src/worker/node/vm.ts`, which has said so for longer. What IS
// reproducible is everything that runs in the *current* realm, because that is
// just indirect `eval`. That covers node's own REPL, which passes
// `useGlobal: true` (`node_core/lib/internal/repl.js:23`) and therefore never
// asks for a second context at all.
//
// This module is the primitive; `node/vm.ts` builds the public `vm.Script` on
// top of it. The direction matters: `repl.js:106` destructures
// `vm.Script.prototype.runInThisContext` from the *public* vm and then applies
// it to a `ContextifyScript` built here, so the two must be the same function
// over the same fields.

// Calling eval through any binding other than the `eval` identifier runs the
// code in global scope and returns the completion value of the final statement
// — exactly what runInThisContext has to do.
const indirectEval: (code: string) => any = eval;

function withSourceURL(code: string, filename?: string): string {
	return filename ? `${code}\n//# sourceURL=${filename}` : code;
}

/**
 * Compile `code` without running it, so a syntax error is raised *now*.
 *
 * This is load-bearing rather than a nicety: `repl.js:539` builds the script
 * purely to check the syntax, and an input that throws here is what makes the
 * REPL offer its `...` continuation prompt instead of reporting an error. A
 * constructor that only stored the string would turn every unfinished line
 * (`if (x) {`) into a hard error at the moment it was typed.
 *
 * `if (0) { … }` rather than `new Function(code)` because the body of a
 * function is not a script: `new Function` accepts a top-level `return`, which
 * a script rejects, so the REPL would go on to fail at run time with a
 * different error than node gives. The block is compiled and never entered.
 *
 * Sloppy-mode `var` declarations inside it still hoist to the global object as
 * `undefined`. That is visible only for a line the REPL then declines to run —
 * one still being typed — and `useGlobal` would have created the same global a
 * keystroke later anyway.
 */
function syntaxCheck(code: string, filename?: string): void {
	indirectEval(withSourceURL(`if (0) {\n${code}\n}`, filename));
}

function needsContext(name: string): never {
	throw new Error(
		`node:vm.${name} requires a separate V8 context, which is not available in this runtime`
	);
}

export class ContextifyScript {
	code: string;
	filename?: string;

	// The positional signature upstream `internal/vm.js`'s `makeContextifyScript`
	// calls with. Everything after the filename is accepted and ignored: cached
	// data is a V8 serialisation format with nothing to deserialise here, and
	// there is no parsing context to be had.
	constructor(
		code: string,
		filename?: string,
		_lineOffset?: number,
		_columnOffset?: number,
		_cachedData?: unknown,
		_produceCachedData?: boolean,
		_parsingContext?: unknown,
		_hostDefinedOptionId?: unknown
	) {
		this.code = String(code);
		this.filename = filename;
		syntaxCheck(this.code, filename);
	}

	runInThisContext(_options?: any): any {
		return indirectEval(withSourceURL(this.code, this.filename));
	}

	runInContext(_context?: any, _options?: any): never {
		return needsContext("Script.prototype.runInContext");
	}
}

/**
 * The SIGINT watchdog, which exists here only so `repl.js` can ask for it.
 *
 * `repl.js:596-598` throws `ERR_CANNOT_WATCH_SIGINT` when starting it fails, so
 * this must answer true — reporting "cannot watch" would break *every*
 * evaluation rather than only the interrupt. Stopping answers false, meaning no
 * SIGINT arrived while the script ran, which is the truth: a Worker has no
 * signals, and an indirect `eval` could not be interrupted by one anyway.
 *
 * phoenix's `node` command passes `breakEvalOnSigint: false` for that reason, so
 * on that path neither of these is reached.
 */
function startSigintWatchdog(): boolean {
	return true;
}

function stopSigintWatchdog(): boolean {
	return false;
}

/** The positional form `internal/vm.js`'s `internalCompileFunction` calls. */
function compileFunction(
	code: string,
	filename?: string,
	_lineOffset?: number,
	_columnOffset?: number,
	_cachedData?: unknown,
	_produceCachedData?: boolean,
	_parsingContext?: unknown,
	contextExtensions?: object[],
	params?: string[]
): { function: Function } {
	if (_parsingContext !== undefined) needsContext("compileFunction with a parsingContext");

	const paramList = Array.isArray(params) ? params : [];
	const body = withSourceURL(String(code), filename);

	if (contextExtensions === undefined || contextExtensions.length === 0) {
		return { function: new Function(...paramList, body) };
	}

	// Each extension object goes on the compiled function's scope chain, which
	// nested `with` blocks reproduce in-realm: an outer factory closes over them
	// and returns the real function, so their properties resolve as free
	// variables. Same shape as `node/vm.ts`'s public `compileFunction`.
	const extNames = contextExtensions.map((_, i) => `__vmExt${i}`);
	let wrapped = `return function (${paramList.join(", ")}) {\n`;
	for (const name of extNames) wrapped += `with (${name}) {\n`;
	wrapped += body + "\n";
	for (let i = 0; i < extNames.length; i++) wrapped += "}\n";
	wrapped += "}";

	const factory = new Function(...extNames, wrapped);
	return { function: factory(...contextExtensions) };
}

export default {
	ContextifyScript,
	compileFunction,
	startSigintWatchdog,
	stopSigintWatchdog,
	makeContext: needsContext.bind(null, "createContext"),
	isContext: () => false,
	measureMemory: needsContext.bind(null, "measureMemory"),
	constants: {
		measureMemory: {
			mode: { SUMMARY: 0, DETAILED: 1 },
			execution: { DEFAULT: 0, EAGER: 1 },
		},
	},
};
