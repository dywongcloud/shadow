import { parse as cjsLexerParse } from "cjs-module-lexer";
import { sync as resolveSync } from "resolve";
import {
	exports as exportsResolve,
	imports as importsResolve,
} from "resolve.exports";

import internalModules from "../node";
import { console_debug, console_warn } from "../console";
import { ctx, host } from "../node/fs/host";
import { MAX_DEPTH, relDepth } from "../node/fs/readdir-encode";
import type { Listing } from "../../vfs/entry";

export type ResolveCondition = "import" | "require";

export type ResolvedSourceType = "esm" | "cjs" | "internal";

export interface BaseResolvedSource {
	type: ResolvedSourceType;
	id: string;
}

export interface RuntimeResolvedSource extends BaseResolvedSource {
	type: "esm" | "cjs";
	path: string;
	dir: string;
	code: string;
}

export interface InternalResolvedSource extends BaseResolvedSource {
	type: "internal";
	module: string;
	exports: any;
}

export type ResolvedSource = RuntimeResolvedSource | InternalResolvedSource;

// The resolver's natural pattern is stat-walking node_modules, and most of those
// stats are *misses* — `resolve` probing `x`, `x.js`, `x/index.js`, and
// `findPackageJson` asking every ancestor for a node_modules that isn't there.
// A miss costs exactly as much as a hit, so answering them one at a time is what
// used to dominate load time.
//
// What remains here is the half that is *strategy* rather than storage: a probe
// into a node_modules pulls the whole tree down with one recursive
// `/fs/readdir`, which turns every subsequent probe under it — hit or miss —
// into something answerable without a request. Measured against a 200-package
// install where a program loads 30 of them: depth 2 needs 32 round trips and
// moves 2200 entries; depth 3 needs 2 and moves 4200.
//
// The storage half is *gone*. This file used to keep its own `statCache`,
// `readFileCache` and `completeDirs`, private to the resolver because — as the
// comment here said — serving `fs.statSync` generally from a cache "would go
// stale the moment another puter app writes to a path, and we have no way to
// hear about that". There is one now (lib/vfs/cache.ts, invalidated from
// puterfs's change feed), and it holds exactly these three things for the whole
// filesystem rather than for one module. So the prefetch below stays and simply
// warms *that*: same request, same effect on every later probe, and no second
// copy of the filesystem that nobody can invalidate.
//
// Two memos do survive, because they are computations over filesystem facts
// rather than mirrors of them — a parsed `package.json` "type" field, and the
// answer to a resolution. Re-deriving either is now a cache hit rather than a
// round trip, which is what makes clearing them wholesale on invalidation
// affordable.
let packageTypeCache: Map<string, "module" | "commonjs" | undefined> =
	new Map();

type StatKind = "file" | "dir" | "missing";

// `<dir>/node_modules` roots already attempted, successfully or not, and how many
// levels the attempt actually covered. Recorded before the request so a missing
// node_modules costs one 404, not one per package name probed at that level.
//
// The depth is what decides whether a probe needs its package pulled down in
// full: everything inside the seed is answerable from the filesystem cache the
// seed filled, and everything past it is not.
//
// These two are prefetch bookkeeping, not answers: a stale entry only means a
// prefetch is not re-issued, and the filesystem still answers correctly — one
// probe at a time instead of one listing for all of them.
let seededNodeModules: Map<string, number> = new Map();
// Package roots already walked at full depth. Once a package is hydrated a
// subsequent miss inside it is a real ENOENT and must not re-trigger.
let hydratedPackages: Set<string> = new Set();

// Deep enough to cover `<pkg>/<dir>/<file>` — so a package's `main`, its
// `exports` targets and `index.js` are all settled by the seed — and equally
// `@scope/<pkg>/package.json`, which is one level lower than the unscoped form.
//
// Measured against a 200-package install where a program loads 30 of them:
// depth 2 needs 32 blocking round trips (a seed plus one hydration per package
// reached into) and moves 2200 entries; depth 3 needs 2 and moves 4200. Twice
// the bytes for a sixteenth of the round trips is the right trade when every
// request is a synchronous XHR that freezes the worker.
const SEED_DEPTH = 3;
// Fallback for an install too big to seed at SEED_DEPTH. Just the package roots:
// enough to make `node_modules` itself complete, which is what kills
// findPackageJson's ancestor walk, at one page's worth of entries.
const SHALLOW_SEED_DEPTH = 1;
// Ceiling on one prefetch. Overrunning it means the listing comes back marked
// incomplete, which the filesystem cache stores as positives only — slower,
// never wrong.
const PREFETCH_MAX_ENTRIES = 20000;

// Through the facade, NOT `readdirPagesPlan` directly.
//
// That plan is the *puterfs* listing — it builds a `/fs/readdir` request — so
// calling it here sent every prefetch to the network regardless of which backend
// actually owns the path. A dependency tree in an in-memory mount was therefore
// answered with a 404 and cached as missing, and the package became unresolvable
// even though `readdirSync` listed it perfectly well one line earlier.
//
// Everything else in this file already goes through `fs` (`statKind` and
// `readFileText` both do); this was the one place that reached past it, and it
// predates there being anything to reach past.
//
// The result is deliberately dropped on the floor. The point is not the listing
// — it is that answering it fills the host's cache, so the hundreds of probes
// that follow are answered there instead of over the network. Both negatives and
// positives, because the reply says whether the walk ran to completion.
function prefetch(root: string, depth: number): Listing {
	return host.readdir(ctx("scandir", root), root, {
		recursive: true,
		depth,
		maxEntries: PREFETCH_MAX_ENTRIES,
	});
}

/** How deep `nmPath` has been laid out, seeding it first if nobody has. */
function seededDepth(nmPath: string): number {
	let known = seededNodeModules.get(nmPath);
	if (known !== undefined) return known;

	seededNodeModules.set(nmPath, SEED_DEPTH);
	try {
		if (!prefetch(nmPath, SEED_DEPTH).complete) {
			// Too big to enumerate at SEED_DEPTH. A shallow seed still settles which
			// packages exist; the ones actually loaded then come in whole, one
			// hydratePackage at a time.
			seededNodeModules.set(nmPath, SHALLOW_SEED_DEPTH);
			prefetch(nmPath, SHALLOW_SEED_DEPTH);
		}
	} catch (_e) {
		let e = _e as any;
		// No node_modules at this level, so no package inside it either — recorded
		// as covering everything, since there is nothing left to learn and a
		// hydration attempt would only repeat this 404. Nothing else to record: the
		// filesystem cache took the ENOENT from the same reply, so the ancestor
		// walk's probes are answered there for free.
		seededNodeModules.set(nmPath, Infinity);
		if (e?.code !== "ENOENT" && e?.code !== "ENOTDIR") {
			console_warn(
				"[node-worker] [resolve] node_modules prefetch failed",
				nmPath,
				e
			);
		}
	}
	return seededNodeModules.get(nmPath)!;
}

function hydratePackage(pkgRoot: string) {
	hydratedPackages.add(pkgRoot);
	try {
		prefetch(pkgRoot, MAX_DEPTH);
	} catch (e) {
		console_warn("[node-worker] [resolve] package prefetch failed", pkgRoot, e);
	}
}

// Locate the deepest `node_modules` on `path` and, within it, the package root
// (`<nm>/<pkg>` or `<nm>/@scope/<pkg>`). Returns null when `path` isn't inside a
// node_modules at all — the resolver only prefetches dependency trees, never the
// user's own source, which is the tree that actually changes underfoot.
function splitNodeModulesPath(
	path: string
): { nm: string; pkg: string | null } | null {
	if (path.endsWith("/node_modules")) return { nm: path, pkg: null };

	let idx = path.lastIndexOf("/node_modules/");
	if (idx === -1) return null;

	let nm = path.slice(0, idx + "/node_modules".length);
	let parts = path.slice(idx + "/node_modules/".length).split("/");
	// Scoped packages are two segments; a bare `@scope` directory is not a
	// package and has no root of its own.
	let take = parts[0].startsWith("@") ? 2 : 1;
	if (parts.length < take) return { nm, pkg: null };
	return { nm, pkg: `${nm}/${parts.slice(0, take).join("/")}` };
}

function rawStatKind(path: string): StatKind {
	try {
		return internalModules.fs.statSync(path).isDirectory() ? "dir" : "file";
	} catch (_e) {
		let e = _e as any;
		if (!e || (e.code !== "ENOENT" && e.code !== "ENOTDIR")) throw e;
		return "missing";
	}
}

/**
 * What `path` is, with the prefetch that makes the *next* few hundred of these
 * free.
 *
 * The stat itself is one blocking round trip to the host, answered from its
 * cache when it can be. The prefetch is what fills that cache: a probe into a
 * dependency tree pulls the tree down in one recursive listing, so every
 * subsequent probe under it — including the misses, which are most of them — is
 * answered without leaving the browser. Tier 1 lays out the packages; tier 2
 * fills in the one package we are actually reaching into.
 *
 * Only inside a `node_modules`. The resolver never prefetches the user's own
 * source, which is the tree that actually changes underfoot.
 */
function statKind(path: string): StatKind {
	let nm = splitNodeModulesPath(path);
	if (nm) {
		let covered = seededDepth(nm.nm);
		// Past the horizon the seed reached. The directories down here were never
		// listed, so this probe and every sibling of it would each be a request;
		// one listing of the package settles all of them. Triggered by *depth*
		// rather than by the probe missing, because a hit at this depth means the
		// misses around it — `x`, `x.js`, `x/index.js` — are coming next.
		if (
			nm.pkg &&
			!hydratedPackages.has(nm.pkg) &&
			relDepth(nm.nm, path) > covered
		) {
			hydratePackage(nm.pkg);
		}
	}
	return rawStatKind(path);
}

function readFileText(path: string): string {
	return internalModules.fs.readFileSync(path, "utf-8") as string;
}

function readPackageType(filePath: string): "module" | "commonjs" | undefined {
	let dir = internalModules.path.dirname(filePath);
	let root = internalModules.path.parse(dir).root;

	// Walk once, remembering every directory we touch so siblings hit the cache
	// on their first call.
	let visited: string[] = [];
	let result: "module" | "commonjs" | undefined;

	while (true) {
		let cached = packageTypeCache.get(dir);
		if (cached !== undefined || packageTypeCache.has(dir)) {
			result = cached;
			break;
		}
		visited.push(dir);

		let packageJsonPath = internalModules.path.join(dir, "package.json");
		if (statKind(packageJsonPath) === "file") {
			let parsed = JSON.parse(readFileText(packageJsonPath));
			if (parsed && typeof parsed.type === "string") {
				if (parsed.type === "module") result = "module";
				else if (parsed.type === "commonjs") result = "commonjs";
			}
			break;
		}

		// stat-ing puter's `/` 500s, so stop one level above root.
		let parent = internalModules.path.dirname(dir);
		if (dir === root || parent === root || parent === dir) {
			result = undefined;
			break;
		}
		dir = parent;
	}

	for (let v of visited) packageTypeCache.set(v, result);
	return result;
}

function hasEsmOnlySyntax(code: string): boolean {
	// Detect esm via cjs-module-lexer, not a full acorn parse: real-world
	// dependency files (e.g. highlight.js's generated language grammars) nest
	// expressions deep enough — hundreds of `+` / call levels — that acorn's
	// recursive-descent parser blows the worker's call stack. The lexer is an
	// O(n) char scanner that can't overflow, and it throws with code
	// "ERR_LEXER_ESM_SYNTAX" the instant it hits a top-level import/export
	// statement. Anything it lexes cleanly is treated as commonjs (node's
	// default for an ambiguous `.js`). Files whose only esm marker is
	// `import.meta` or bare top-level await — with no import/export statement —
	// fall through to cjs, but those are vanishingly rare and never arise here.
	try {
		cjsLexerParse(code);
		return false;
	} catch (e) {
		return (e as any)?.code === "ERR_LEXER_ESM_SYNTAX";
	}
}

// Decide cjs vs esm the way `Module._extensions['.js']` does in upstream node:
// extension first, then the `type` field of the nearest enclosing
// package.json. Falls back to syntax sniffing for ambiguous `.js` files
// (and virtual sources with unknown/missing extensions) without a
// package.json.
function detectRuntimeSourceType(source: {
	path: string;
	code: string;
}): RuntimeResolvedSource["type"] {
	let ext = internalModules.path.extname(source.path);
	if (ext === ".mjs") return "esm";
	if (ext === ".cjs") return "cjs";
	// Decided by extension like the two above, and deliberately ahead of the package
	// type: node's `.json` handler lives in the CJS loader (see `createCjsModule`), so a
	// `"type": "module"` package.json overhead does not make a data file a module.
	if (ext === ".json") return "cjs";

	let packageType = readPackageType(source.path);
	if (packageType === "module") return "esm";
	if (packageType === "commonjs") return "cjs";

	return hasEsmOnlySyntax(source.code) ? "esm" : "cjs";
}

// `(condition, basedir, target)` → resolved path. Skips the entire node_modules
// walk on repeat lookups (very common: every file in a package re-requires its
// peers). Condition is part of the key because exports/imports can map the
// same specifier to different files under `import` vs `require`.
let resolvePathCache: Map<string, string> = new Map();

// Split a bare specifier into its package name and the requested subpath.
// "ws" → { pkgName: "ws", subpath: "." }
// "ws/lib/foo" → { pkgName: "ws", subpath: "./lib/foo" }
// "@scope/pkg/sub" → { pkgName: "@scope/pkg", subpath: "./sub" }
function splitBareSpecifier(target: string): {
	pkgName: string;
	subpath: string;
} {
	let parts = target.split("/");
	let pkgEnd = target.startsWith("@") ? 2 : 1;
	let pkgName = parts.slice(0, pkgEnd).join("/");
	let rest = parts.slice(pkgEnd).join("/");
	return { pkgName, subpath: rest ? `./${rest}` : "." };
}

// Walk up from basedir looking for `<dir>/node_modules/<pkgName>/package.json`
// (the standard node_modules resolution algorithm). Returns the package.json
// path or null.
function findPackageJson(pkgName: string, basedir: string): string | null {
	let dir = basedir;
	let root = internalModules.path.parse(dir).root;
	while (true) {
		let candidate = internalModules.path.join(
			dir,
			"node_modules",
			pkgName,
			"package.json"
		);
		if (statKind(candidate) === "file") return candidate;
		// stat-ing puter's `/` 500s, so stop one level above root. `parent === dir`
		// is the fixed-point guard: a non-absolute basedir (e.g. a stray `file://`
		// URL) has no POSIX root, so dirname converges to "." instead of `root` —
		// without this the walk would spin forever.
		let parent = internalModules.path.dirname(dir);
		if (dir === root || parent === root || parent === dir) return null;
		dir = parent;
	}
}

// Resolve a bare specifier via the package's `exports` (or `imports` for
// `#`-prefixed specifiers) field, honoring the caller's condition. Returns
// the resolved absolute file path, or null if the package has no exports
// field or the specifier doesn't match. Throws if exports is present but
// the subpath is explicitly not exported.
function resolveViaExportsField(
	target: string,
	basedir: string,
	condition: ResolveCondition
): string | null {
	// Imports field (`#foo`) is resolved relative to the importer's nearest
	// package.json, not via node_modules walking.
	if (target.startsWith("#")) {
		let dir = basedir;
		let root = internalModules.path.parse(dir).root;
		while (true) {
			let pjsonPath = internalModules.path.join(dir, "package.json");
			if (statKind(pjsonPath) === "file") {
				let pkg = JSON.parse(readFileText(pjsonPath));
				if (pkg && pkg.imports) {
					let matched = importsResolve(pkg, target, {
						conditions: ["node"],
						require: condition === "require",
					});
					if (matched && matched.length > 0) {
						let pkgDir = internalModules.path.dirname(pjsonPath);
						let first = matched[0];
						if (first.startsWith(".")) {
							return internalModules.path.join(pkgDir, first);
						}
						// Imports can map to an external package; recurse via
						// the normal resolver against that package.
						return null;
					}
				}
				return null;
			}
			let parent = internalModules.path.dirname(dir);
			if (dir === root || parent === root || parent === dir) return null;
			dir = parent;
		}
	}

	let { pkgName, subpath } = splitBareSpecifier(target);
	let pjsonPath = findPackageJson(pkgName, basedir);
	if (!pjsonPath) return null;

	let pkg = JSON.parse(readFileText(pjsonPath));
	if (!pkg || !pkg.exports) return null;

	let matched = exportsResolve(pkg, subpath, {
		conditions: ["node"],
		require: condition === "require",
	});
	if (!matched || matched.length === 0) {
		throw new Error(
			`Package "${pkgName}" has no "${subpath}" export under condition "${condition}"`
		);
	}
	let pkgDir = internalModules.path.dirname(pjsonPath);
	return internalModules.path.join(pkgDir, matched[0]);
}

// When the resolver lands on `<fromPkg>/<fromSubpath>`, serve
// `<toPkg>/<toSubpath>` instead. Used to swap native/prebuilt-binary modules
// for pure-WASM/JS equivalents, the way StackBlitz WebContainer does. We
// redirect to the target's real *path* (not a rewritten body) so its own
// relative requires resolve against the target install dir and find sibling
// assets (e.g. the `.wasm`).
interface ModuleRedirect {
	fromPkg: string;
	fromSubpath: string; // package-relative, no leading "./"
	toPkg: string;
	toSubpath: string;
	missingHint?: string; // thrown if toPkg isn't installed
}

let moduleRedirects: ModuleRedirect[] = [
	{
		// Rollup's dist/native.js only loads a prebuilt `.node` addon
		// (`@rollup/rollup-<platform>-<arch>`); for platform "browser"/arch
		// "wasm" there is none — its lookup table misses and it throws
		// `... not yet supported by the native Rollup build` before anything
		// loads. (And our npm-install ignores optionalDependencies, so the addon
		// packages aren't even on disk.) @rollup/wasm-node exposes the identical
		// `parse`/`parseAsync`/`xxhash*` API backed by a wasm SWC parser
		// (instantiated synchronously, which is allowed off the main thread), so
		// the AST buffer rollup's `convert-ast` decodes is byte-compatible.
		fromPkg: "rollup",
		fromSubpath: "dist/native.js",
		toPkg: "@rollup/wasm-node",
		toSubpath: "dist/native.js",
		missingHint:
			`rollup needs a native binding that doesn't exist for platform "browser"/arch "wasm". ` +
			`Add "@rollup/wasm-node" (matching your rollup major version) to your project's ` +
			`dependencies and reinstall so the runtime can use the WASM build.`,
	},
	{
		// esbuild's JS API is a *client*: `lib/main.js` looks up
		// `@esbuild/<platform>-<arch>` for a prebuilt executable and talks to it
		// over a pipe via child_process. Platform "browser"/arch "wasm" isn't in
		// its table ("Unsupported platform: browser wasm LE"), no such package
		// exists, and child_process can't spawn anything here regardless.
		//
		// Unlike @rollup/wasm-node, esbuild-wasm is not a drop-in: its own
		// `lib/main.js` is that same subprocess client, and the usable half
		// (`lib/browser.js`) has a different contract — an explicit
		// `initialize()` with the wasm bytes, async-only APIs, and a Go runtime
		// that needs `globalThis.fs` wired up before it can see any files. So the
		// target here is an adapter that the harness installs into esbuild-wasm's
		// own lib/ (node-worker-test/src/shims/esbuild-wasm.cjs, written by its
		// npm-install). Keeping it there rather than in this bundle means the
		// runtime's whole share of the swap is this rule, and the shim's
		// `require("./browser.js")` and `__dirname`-relative wasm read resolve on
		// their own.
		fromPkg: "esbuild",
		fromSubpath: "lib/main.js",
		toPkg: "esbuild-wasm",
		toSubpath: "lib/node-worker-shim.cjs",
		missingHint:
			`esbuild drives a native binary subprocess, which doesn't exist for platform ` +
			`"browser"/arch "wasm". Add "esbuild-wasm" (same version as your esbuild) to ` +
			`your project's dependencies and reinstall with node-worker frontend so the runtime can use the WASM build.`,
	},
];

function maybeRedirectModule(path: string): string {
	for (let rule of moduleRedirects) {
		// Leading "/" anchors the match at a path segment boundary, so
		// "foo-rollup/dist/native.js" won't match the "rollup" rule.
		if (!path.endsWith(`/${rule.fromPkg}/${rule.fromSubpath}`)) continue;

		let toPkgJson = findPackageJson(
			rule.toPkg,
			internalModules.path.dirname(path)
		);
		if (!toPkgJson) {
			throw new Error(
				rule.missingHint ??
					`"${rule.fromPkg}/${rule.fromSubpath}" redirects to "${rule.toPkg}", which isn't installed.`
			);
		}
		return internalModules.path.join(
			internalModules.path.dirname(toPkgJson),
			rule.toSubpath
		);
	}
	return path;
}

// Overrides handed to `resolve` so its internal isFile/isDirectory/realpath/
// readFile calls share our cache. realpath is identity because puterfs has no
// symlinks (see fs/sync.ts:367), so the default realpath would just burn a
// stat per resolution.
let resolveSyncOpts = {
	isFile(file: string) {
		return statKind(file) === "file";
	},
	isDirectory(dir: string) {
		return statKind(dir) === "dir";
	},
	realpathSync(x: string) {
		return x;
	},
	readFileSync(file: string) {
		return readFileText(file);
	},
	// paths: [] disables resolve's home-directory defaults
	// (~/.node_modules, ~/.node_libraries), which would call
	// path.join with a null homedir.
	paths: [] as string[],
};

// node 11 code or something
function stripShebang(content: string): string {
	if (content.charAt(0) === "#" && content.charAt(1) === "!") {
		let index = content.indexOf("\n", 2);
		if (index === -1) return "";
		if (content.charAt(index - 1) === "\r") index--;
		content = content.slice(index);
	}
	return content;
}

// A failed resolve is not automatically a problem: probing for an optional
// dependency and falling back is a normal pattern in real packages — `debug`
// does `try { humanize = require("ms") } catch { humanize = ownImpl }`, `ws`
// does it for `bufferutil`/`utf-8-validate`, `chokidar` for `fsevents` — so
// warning here shouted about four working fallbacks on every vite run. The
// throw is the whole report; whoever ends up handling it decides whether it
// mattered. `console_debug` keeps a trace at devtools' Verbose level for when
// a resolve fails and you want to know why.
//
// The error must also carry node's `code`, because the other half of that
// pattern is `catch (e) { if (e.code !== "MODULE_NOT_FOUND") throw e }` — the
// old `new Error("Unknown target x")` wrapper dropped the `code` that
// `resolve` sets, turning an expected miss into a rethrown crash.
function moduleNotFound(
	target: string,
	basedir: string,
	condition: ResolveCondition,
	cause: unknown
): Error {
	console_debug("[node-worker] [resolve] resolve failed", cause);
	let err = new Error(`Cannot find module '${target}' from '${basedir}'`, {
		cause,
	}) as Error & { code: string };
	// `require` and `import` fail under different codes upstream.
	err.code =
		condition === "require" ? "MODULE_NOT_FOUND" : "ERR_MODULE_NOT_FOUND";
	return err;
}

export function resolveSource(
	target: string,
	basedir: string,
	condition: ResolveCondition = "require"
): ResolvedSource {
	if (target.startsWith("node:")) {
		target = target.slice("node:".length);
		if (Object.hasOwn(internalModules, target)) {
			return {
				type: "internal",
				id: target,
				module: target,
				exports: (internalModules as any)[target],
			};
		}
		throw new Error(`Unknown internal module "node:${target}"`);
	}

	if (Object.hasOwn(internalModules, target)) {
		return {
			type: "internal",
			id: target,
			module: target,
			exports: (internalModules as any)[target],
		};
	}

	// `import()` takes a file: URL as readily as a path, and for a path computed at runtime
	// the URL is the *idiomatic* form — `await import(pathToFileURL(p).href)` is how you load
	// one without a bare specifier being assumed. It is how vite loads every `vite.config.ts`:
	// the config is bundled to a temp `.mjs` and imported by URL, so without this no project
	// with a config file can start.
	//
	// Converted here, at the edge, because module ids on this side are paths — only
	// `import.meta.url` is a URL (see `System.createContext`). `require` is deliberately left
	// out: node's CJS loader takes no URLs either.
	if (condition === "import" && target.startsWith("file:")) {
		target = internalModules.url.fileURLToPath(target);
	}

	// No special case for injected sources any more. They are real files in the
	// in-memory overlay mounted over "/" (see node/fs/vfs/virtual.ts), so they
	// resolve, stat and read through exactly this path — which is also what makes a
	// relative `require("./x")` inside one resolve against its own directory rather
	// than against the root, and lets `detectRuntimeSourceType` find the enclosing
	// package.json the ordinary way.
	let path: string;
	let cacheKey = condition + "\0" + basedir + "\0" + target;
	let cachedPath = resolvePathCache.get(cacheKey);
	if (cachedPath !== undefined) {
		path = cachedPath;
	} else {
		// Bare specifiers (and `#`-imports) may need exports/imports field
		// resolution, which `resolve` v1.x doesn't do. Try that first; on
		// miss (no exports field, or relative/absolute specifier) fall
		// through to the legacy main-field walk.
		let viaExports: string | null = null;
		let isBare =
			!target.startsWith(".") &&
			!target.startsWith("/") &&
			!internalModules.path.isAbsolute(target);
		if (isBare) {
			try {
				viaExports = resolveViaExportsField(target, basedir, condition);
			} catch (e) {
				console_warn("[node-worker] [resolve] exports resolution failed", e);
				throw e;
			}
		}
		if (viaExports !== null) {
			path = viaExports;
		} else {
			try {
				path = resolveSync(target, { ...resolveSyncOpts, basedir });
			} catch (e) {
				// No retry here any more. This used to stat the candidate one more
				// time, because a directory listed in full *before* the program wrote
				// a file into it answered "nothing there" forever — vite hits that on
				// every start, bundling its config into `node_modules/.vite-temp/`
				// inside a tree listed while resolving vite itself. The negative comes
				// from the host cache now, and a write through this filesystem drops it
				// as it happens, so the first attempt already sees the file.
				throw moduleNotFound(target, basedir, condition, e);
			}
		}
		path = maybeRedirectModule(path);
		resolvePathCache.set(cacheKey, path);
	}
	let code = stripShebang(readFileText(path));

	return {
		type: detectRuntimeSourceType({ path, code }),
		id: path,
		dir: internalModules.path.dirname(path),
		path,
		code,
	};
}

/**
 * Forget what this module concluded about `path`.
 *
 * Only the memos live here now — the filesystem's own cache is invalidated by the
 * host, on the same reply that carries this. What remains is the resolution of a
 * *specifier*, which is a fact about the tree rather than about one file: a file
 * appearing or being replaced can change where `require("./x")` lands, and the
 * overlay (node/fs/vfs/virtual.ts) is exactly such a case — the testbed
 * re-registers its eval module at one stable path on every run, and without this
 * the second run would compile the first run's source.
 *
 * Cleared wholesale rather than by key. `resolvePathCache` is keyed by specifier
 * and `packageTypeCache` by directory, so neither can be indexed by the path that
 * changed — and rebuilding them is now a handful of cache hits rather than a
 * handful of round trips, which is what makes the blunt instrument affordable.
 */
export function invalidateResolved(_path: string) {
	packageTypeCache.clear();
	resolvePathCache.clear();
}

/**
 * Forget everything concluded at or below `prefix`.
 *
 * Mounting a filesystem somewhere invalidates far more than one path: every probe
 * that concluded "nothing here" while the mount was absent is now wrong. The
 * negatives themselves belong to the host's cache and are dropped there; what has
 * to go here is the prefetch bookkeeping, or a dependency tree that was scanned
 * before the mount existed would never be scanned again.
 */
export function invalidateResolvedSubtree(prefix: string) {
	let under = (p: string) =>
		p === prefix || p.startsWith(prefix === "/" ? "/" : prefix + "/");

	for (let key of [...seededNodeModules.keys()])
		if (under(key)) seededNodeModules.delete(key);
	for (let key of [...hydratedPackages])
		if (under(key)) hydratedPackages.delete(key);

	packageTypeCache.clear();
	resolvePathCache.clear();
}
