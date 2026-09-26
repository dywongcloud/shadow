import {
	resolveSource,
	ResolvedSource,
	RuntimeResolvedSource,
} from "./resolve";
import { CWD } from "../state";
import { compileModuleFunction } from "./compile";
import { createCjsModule } from "./cjs";
import internalModules from "../node";
import { ACF_GLOBAL } from "./globals";
import System, { Registration } from "isolated-systemjs";
import { getRewriter } from "../node-rust/loader";
import { console_warn } from "../console";
import * as keepalive from "../keepalive";

let PUTER_NODE_SYSTEMJS = "__puter_node_systemjs";
let decoder = new TextDecoder();
let pending = new Map<string, ResolvedSource>();

function exportNamespace(_export: (prop: string, val: any) => void, ns: any) {
	_export("default", ns);
	if (ns != null && (typeof ns === "object" || typeof ns === "function"))
		for (const k of Object.keys(ns)) if (k !== "default") _export(k, ns[k]);
}

function esmHelper(src: RuntimeResolvedSource) {
	if (src.type !== "esm") throw "";

	let rewritten = getRewriter().rewrite_js(
		src.code,
		PUTER_NODE_SYSTEMJS,
		ACF_GLOBAL
	);
	for (let error of rewritten.errors) {
		console_warn("[node-worker] rewrite error for", src.id, error);
	}

	let js = decoder.decode(rewritten.js);
	try {
		return compileModuleFunction([PUTER_NODE_SYSTEMJS], js, src.path);
	} catch (err) {
		console_warn("[node-worker] failed to create function for", src.id, js);
		throw err;
	}
}

System.resolve = function (id, parent) {
	let src = resolveSource(
		id,
		parent ? internalModules.path.dirname(parent) : CWD,
		"import"
	);
	pending.set(src.id, src);
	return src.id;
};

let baseCreateContext = System.createContext.bind(System);
System.createContext = function (id) {
	let ctx = baseCreateContext(id) as any;
	ctx.url = internalModules.url.pathToFileURL(id).href;
	ctx.filename = id;
	ctx.dirname = internalModules.path.dirname(id);
	return ctx;
};

System.instantiate = async function (url) {
	let src = pending.get(url);
	pending.delete(url);
	if (!src) throw new Error("unknown module instantiated" + url);

	if (src.type === "internal") {
		return [
			[],
			(_export) => ({
				execute() {
					exportNamespace(_export, src.exports);
				},
			}),
		] satisfies Registration;
	} else if (src.type === "cjs") {
		let [module, run] = createCjsModule(src);
		return [
			[],
			(_export) => ({
				execute() {
					run();
					exportNamespace(_export, module.exports);
				},
			}),
		] satisfies Registration;
	} else if (src.type === "esm") {
		esmHelper(src)(System);
		return System.getRegister() as Registration;
	} else {
		throw new Error("unreachable");
	}
};

// A pending module load keeps the loop alive, as it does in node — where resolving and
// reading a module is fs work on the loop, so a graph still being pulled in is never
// mistaken for a program that has finished.
//
// This is load-bearing for any CLI that hands control to a floating dynamic import.
// vite's bin ends with an unawaited `start()`, whose whole body is
// `import('../dist/node/cli.js')`: the entry module finishes evaluating almost
// immediately, and without a ref here the run looks over while vite is still reading
// its own chunks off disk.
let baseSystemImport = System.import.bind(System);
System.import = function (id: string, parent?: string) {
	let release = keepalive.refOperation();
	return baseSystemImport(id, parent).finally(release);
} as typeof System.import;

export function esmImport(src: string) {
	return System.import(src);
}
