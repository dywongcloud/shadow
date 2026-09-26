import { console_warn } from "../console";
import { ProcessExit } from "../exit";
import { CWD } from "../state";
// Installs the Node-only globals (Buffer, process, timers, …) onto globalThis —
// CJS modules read them from there rather than via wrapper parameters (see
// CJS_HARNESS below) — and provides ACF_GLOBAL, the async-context holder name the
// await transform emits references to.
import { ACF_GLOBAL } from "./globals";
import { compileModuleFunction } from "./compile";
import { resolveSource } from "./resolve";
import type { RuntimeResolvedSource } from "./resolve";
import { getRewriter } from "../node-rust/loader";
import path from "../node/path";
import url from "../node/url";

let decoder = new TextDecoder();

// Wrap `await` expressions in the CJS source so the async context propagates
// across them (see the rewriter's rewrite_awaits / node/async_hooks). This is the
// await-only transform — no ESM lowering. Skipped when the source has no `await`
// token at all (the common case), and falls back to the original source on any
// parse/transform error so a module never fails to load because of this.
function transformCjsAwaits(id: string, code: string): string {
	if (!code.includes("await")) return code;
	try {
		let rewritten = getRewriter().transform_awaits(code, ACF_GLOBAL);
		for (let error of rewritten.errors) {
			console_warn("[node-worker] cjs await-rewrite error for", id, error);
		}
		return decoder.decode(rewritten.js);
	} catch (err) {
		console_warn("[node-worker] cjs await-rewrite failed for", id, err);
		return code;
	}
}

export interface CJSModule {
	children: CJSModule[];
	exports: any;
	filename: string;
	id: string;
	isPreloading: false;
	loaded: boolean;
	path: string;
	paths: string[];
	require: (id: string) => any;
}

// Match Node's real CJS module wrapper: only `require`, `module`, `exports`,
// `__dirname`, `__filename` are injected as parameters. Node globals (Buffer,
// process, timers, …) live on globalThis (installed by ./globals), NOT as
// wrapper params. Injecting them as params breaks any module that declares a
// top-level lexical binding of the same name — e.g. undici's
// `const Buffer = require('node:buffer').Buffer` throws
// "Identifier 'Buffer' has already been declared". As globals, such a
// declaration simply shadows the global within the module scope, as in Node.
//
// `this` is the receiver, not a parameter: upstream invokes the wrapper as
// `compiledWrapper.call(module.exports, …)`, so a module's top-level `this` is
// its own exports object. Binding `null` instead handed sloppy-mode code
// `globalThis`, which quietly breaks the (common in transpiler output and
// hand-written CJS shims) `this.foo = …` / `Object.assign(this, …)` form of
// export: the assignment landed on the global object and `module.exports`
// stayed empty. Note the binding captures the *initial* exports object — a
// later `module.exports = x` doesn't retarget `this`, which is also how Node
// behaves.
let CJS_HARNESS = (code: string, module: CJSModule) =>
	compileModuleFunction(
		["require", "module", "exports", "__dirname", "__filename"],
		code,
		module.filename
	).bind(
		module.exports,
		module.require,
		module,
		module.exports,
		module.path,
		module.filename
	);

/**
 * node's `Module._extensions[".json"]`: the file is data, and the module's exports are
 * the value it parses to.
 *
 * Without this the text was compiled as JavaScript, and the two JSON shapes failed in
 * different ways — an object is a syntax error (`{ "name": … }` is a block, then a string
 * followed by a colon), while an array is a *valid* expression statement that evaluates
 * and exports nothing. The second is the worse one, because it looks like a successful
 * load: `@babel/traverse` does
 * `require("@babel/helper-globals/data/builtin-lower.json")` and got `{}` back, which
 * surfaced much later and much further away as "globalsBuiltinLower is not iterable".
 */
function parseJsonModule(filename: string, code: string): unknown {
	// A BOM is legal in a JSON file and `JSON.parse` rejects it, so node strips it here.
	let text = code.charCodeAt(0) === 0xfeff ? code.slice(1) : code;
	try {
		return JSON.parse(text);
	} catch (e) {
		// node names the file in the message; a bare "Unexpected token }" from
		// somewhere inside a dependency tree is close to unactionable.
		let err = e as Error;
		err.message = `${filename}: ${err.message}`;
		throw err;
	}
}

export function createCjsModule(
	resolvedSource: RuntimeResolvedSource
): [CJSModule, () => void] {
	let module: CJSModule = {
		children: [], // TODO handle children
		exports: Object.create({}),
		filename: resolvedSource.path,
		id: resolvedSource.path,
		isPreloading: false,
		loaded: false,
		path: resolvedSource.dir,
		paths: [], // TODO handle paths
		require: createRequireFromDir(resolvedSource.dir),
	};

	if (path.extname(resolvedSource.path) === ".json") {
		module.exports = parseJsonModule(resolvedSource.path, resolvedSource.code);
		return [
			module,
			() => {
				module.loaded = true;
			},
		];
	}

	let harness = CJS_HARNESS(
		transformCjsAwaits(resolvedSource.path, resolvedSource.code),
		module
	);
	return [
		module,
		() => {
			harness();
			module.loaded = true;
		},
	];
}

let REQUIRE_CACHE: Record<string, any> = {};

function requireWithBasedir(target: string, basedir: string): any {
	let resolvedSource = resolveSource(target, basedir, "require");

	if (resolvedSource.type === "internal") {
		return resolvedSource.exports;
	}

	if (Object.hasOwn(REQUIRE_CACHE, resolvedSource.path)) {
		return REQUIRE_CACHE[resolvedSource.path];
	}

	try {
		if (resolvedSource.type === "esm") throw new Error("unsupported");
		let [module, fn] = createCjsModule(resolvedSource);

		REQUIRE_CACHE[resolvedSource.path] = module.exports;
		fn();
		REQUIRE_CACHE[resolvedSource.path] = module.exports;

		return module.exports;
	} catch (e) {
		// `process.exit` unwinds by throwing, so it passes through here on its way out
		// of every module on the stack. It is control flow, not a failed load: wrapping
		// it would turn a CLI's ordinary successful exit into
		// `Failed to load module from "…/tsc.js"`, and hide the exit code with it.
		if (e instanceof ProcessExit) throw e;
		console_warn("[node-worker] [resolve] [cjs] load failed", e);
		throw new Error(`Failed to load module from "${resolvedSource.path}"`, {
			cause: e,
		});
	}
}

interface RequireFn {
	(target: string): any;
	cache: Record<string, any>;
}

function createRequireFromDir(basedir: string): RequireFn {
	let fn: RequireFn = ((target: string) =>
		requireWithBasedir(target, basedir)) as any;
	fn.cache = REQUIRE_CACHE;
	return fn;
}

export function createRequire(filename: string | URL): RequireFn {
	let pathname =
		filename instanceof URL || String(filename).startsWith("file:")
			? url.fileURLToPath(filename as any)
			: String(filename);
	return createRequireFromDir(path.dirname(pathname));
}

export function require(target: string): any {
	return requireWithBasedir(target, CWD);
}
