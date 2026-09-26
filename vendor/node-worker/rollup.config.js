import { defineConfig } from "rollup";
import path from "node:path";
import fs from "node:fs/promises";
import { fileURLToPath } from "node:url";

import nodeResolve from "@rollup/plugin-node-resolve";
import commonjs from "@rollup/plugin-commonjs";
import typescript from "@rollup/plugin-typescript";
import inject from "@rollup/plugin-inject";
import json from "@rollup/plugin-json";
import terser from "@rollup/plugin-terser";
import dts from "rollup-plugin-dts";

const rootDir = path.dirname(fileURLToPath(import.meta.url));
const packageJson = JSON.parse(await fs.readFile(path.join(rootDir, 'package.json'), 'utf8'));
const nodeRoot = path.resolve(rootDir, packageJson.nodeCore.checkoutDir);
const nodeLibRoot = path.join(nodeRoot, 'lib');
const nodeDepsRoot = path.join(nodeRoot, 'deps');
const runtimeRoot = path.join(rootDir, 'src/worker/node-core/lib');
const nodeCoreMarkerQuery = '?node-core-from-plugin=1';

function isolatedSystemJs() {
	const file = import.meta.resolve("systemjs/s.js");
	return {
		name: "isolated-systemjs",
		resolveId: id => id === "isolated-systemjs" ? "\0isolated-systemjs" : null,
		async load(id) {
			if (id !== "\0isolated-systemjs") return null;
			const src = await fs.readFile(fileURLToPath(file));
			return `let sbx = {};(function(self, window, global, globalThis, document){${src}})(sbx, undefined, sbx, sbx, undefined);export default sbx.System;`
		}
	}
}

function nodeCorePlugin() {
	// Resolve `request` (e.g. "buffer", "fs/promises") against, in order:
	//   1. a runtime override under runtimeRoot
	//   2. the real node source under nodeLibRoot (with a marker so we don't
	//      recurse on it)
	// Returns null if neither resolves. The `selfId` parameter lets a runtime
	// override do a bare import of its own name (`lib/buffer.js` does
	// `import 'buffer'`) and have us skip the override and fall through to the
	// npm polyfill via nodeResolve.
	async function resolveNodeRequest(ctx, request, importer, selfId) {
		const runtimeRequest = path.join(runtimeRoot, request);
		const resolvedRuntimeModule = await ctx.resolve(runtimeRequest, importer, { skipSelf: true });
		if (resolvedRuntimeModule) {
			const id = resolvedRuntimeModule.id.split('?')[0];
			if (id !== selfId) return id;
			// The runtime override matched the importer itself — fall through
			// to nodeResolve so the npm polyfill (e.g. `buffer`) is used,
			// rather than looping back into upstream node's `lib/<request>.js`
			// (which itself depends on `internal/<request>.js` and would form
			// a cycle through this same override).
			return null;
		}

		const nodeLibRequest = `${path.join(nodeLibRoot, request)}${nodeCoreMarkerQuery}`;
		const resolved = await ctx.resolve(nodeLibRequest, importer, { skipSelf: true });
		if (resolved) {
			return resolved.id.split('?')[0];
		}

		return null;
	}

	return {
		name: 'node-core-plugin',
		async resolveId(source, importer) {
			if (source.endsWith(nodeCoreMarkerQuery)) return null;
			if (path.isAbsolute(source)) return null;
			if (source.startsWith('.')) return null;

			// Explicit `internal/deps/foo` -> node_core/deps/foo. Used by node
			// runtime files to reach bundled deps like undici and minimatch.
			if (source.startsWith('internal/deps/')) {
				const depsRequest = source.slice('internal/deps/'.length);
				const depsModule = path.join(nodeDepsRoot, depsRequest);
				const resolvedDepsModule = await this.resolve(depsModule, importer, { skipSelf: true });
				if (resolvedDepsModule) {
					return resolvedDepsModule.id.replace(/^\0/, "").split("?")[0];
				}
				return null;
			}

			// Explicit `internal/fs/foo` -> runtime override under internal/fs/,
			// falling through to upstream `node_core/lib/internal/fs/foo.js` if
			// no override exists. Overrides take precedence so unsupported fs
			// internals (`watchers`, `rimraf`, `cp`, ...) can be replaced with
			// loud throw-stubs while supported ones (`glob`, `utils`, ...) flow
			// through to the upstream implementation.
			if (source.startsWith('internal/fs/')) {
				const fsRequest = source.slice('internal/fs/'.length);
				const fsModule = path.join(runtimeRoot, 'internal', 'fs', fsRequest);
				const resolvedFsModule = await this.resolve(fsModule, importer, { skipSelf: true });
				if (resolvedFsModule) return resolvedFsModule.id.split('?')[0];

				const nodeLibRequest = `${path.join(nodeLibRoot, 'internal', 'fs', fsRequest)}${nodeCoreMarkerQuery}`;
				const resolved = await this.resolve(nodeLibRequest, importer, { skipSelf: true });
				if (resolved) return resolved.id.split('?')[0];

				throw new Error(`Cannot resolve internal/fs/${fsRequest}`);
			}

			// Explicit `node-core:foo` is the only way our own (audited) source
			// is allowed to reach the node runtime. Any other usage from outside
			// the node tree falls through to the rest of rollup's resolver chain
			// and will fail to resolve.
			if (source.startsWith('node-core:')) {
				const request = source.slice('node-core:'.length);
				const resolved = await resolveNodeRequest(this, request, importer, null);
				if (resolved) return resolved;
				throw new Error(`Unsupported node-core module: ${request}`);
			}

			// Bare and `node:` imports are only relaxed when they originate from
			// inside the node runtime tree itself (upstream node_core sources, the
			// bundled `internal/deps/*` packages, our hand-written runtime
			// overrides) or from inside a third-party npm package — both are
			// expected to talk to node APIs without ceremony. Audited code in
			// src/worker/ must use `node-core:` instead, so an accidental bare
			// `import 'fs'` there fails the build.
			const importerInRelaxedZone = Boolean(
				importer && (
					importer.startsWith(nodeLibRoot) ||
					importer.startsWith(nodeDepsRoot) ||
					importer.startsWith(runtimeRoot) ||
					importer.includes(`${path.sep}node_modules${path.sep}`)
				)
			);
			if (!importerInRelaxedZone) return null;

			const usesNodeProtocol = source.startsWith('node:');
			const request = usesNodeProtocol ? source.slice('node:'.length) : source;

			// If a runtime override does a bare import of its own name
			// (`lib/buffer.js` does `import 'buffer'`), skip the override so
			// nodeResolve finds the npm polyfill instead of looping back here.
			const selfId = importer && importer.startsWith(runtimeRoot) &&
				path.relative(runtimeRoot, importer).replace(/\.[mc]?js$/, '') === request
				? importer
				: null;

			return resolveNodeRequest(this, request, importer, selfId);
		},
	};
}

export default defineConfig([
	{
		input: "src/worker/index.ts",
		output: [{ file: "dist/worker.js", format: "es" }],
		onwarn(warning, warn) {
			if (warning.code === "CIRCULAR_DEPENDENCY") {
				console.warn(warning.message);
				return;
			}
			warn(warning);
		},
		plugins: [
			isolatedSystemJs(),
			nodeCorePlugin(),
			{
				// cjs-module-lexer's `exports` field routes ESM imports to the
				// wasm-backed `dist/lexer.mjs`, which throws "Not initialized"
				// unless `init()`/`initSync()` runs first. detectCjsExports is
				// called sync during ESM rewriting, so route to the pure-JS
				// `lexer.js` (the package's `default` condition) instead.
				name: 'cjs-module-lexer-sync',
				resolveId(source) {
					if (source !== 'cjs-module-lexer') return null;
					return path.join(rootDir, 'node_modules/cjs-module-lexer/lexer.js');
				},
			},
			nodeResolve({
				preferBuiltins: false,
				mainFields: ["browser", "module", "main"],
			}),
			commonjs({
				dynamicRequireTargets: ["node_core/**/*.js"],
			}),
			json(),
			inject({
				process: [path.resolve(rootDir, "src", "worker", "node", "process.ts"), "default"],
				primordials: [path.join(rootDir, "src", "worker", "node-core", "primordials.ts"), "default"],
				internalBinding: [path.join(rootDir, "src", "worker", "node-core", "internal-binding", "index.ts"), "default"],
			}),
			typescript({
				tsconfig: "./tsconfig.worker.json",
			}),
			//			terser()
		],
	},
	{
		input: "src/lib/index.ts",
		output: [{ file: "dist/index.js", format: "es" }],
		onwarn(warning, warn) {
			// Suppress circular dependency warnings
			if (warning.code === "CIRCULAR_DEPENDENCY") return;
			warn(warning);
		},
		plugins: [
			typescript({
				tsconfig: "./tsconfig.main.json",
			}),
			//			terser()
		],
	},
	{
		input: "src/lib/index.ts",
		output: [{ file: "dist/index.d.ts", format: "es" }],
		plugins: [dts()],
	},
	{
		// The service worker that relays the node worker's blocking filesystem requests to
		// the page. A CLASSIC script, not a module: module service workers are still not
		// universally shipped (Firefox), and there is nothing to gain from ESM here — this
		// bundle imports only constants, because the worker never looks inside a frame.
		input: "src/sw/index.ts",
		output: [{ file: "dist/sw.js", format: "iife" }],
		plugins: [typescript({ tsconfig: "./tsconfig.sw.json" })],
	},
	{
		// The same fetch handler, importable into a host application's *own* service worker.
		// Not optional polish: only one service worker can own a scope, so an app that
		// already has one could otherwise never use synchronous filesystem access.
		input: "src/sw/handler.ts",
		output: [{ file: "dist/sw-handler.js", format: "es" }],
		plugins: [typescript({ tsconfig: "./tsconfig.sw.json" })],
	},
]);
