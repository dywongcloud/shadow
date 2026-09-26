// Stub for `internal/modules/esm/get_format`. Upstream it decides a module's
// format from its extension and the nearest package.json, which means the
// package.json reader and the TypeScript stripper come with it.
//
// `internal/repl/completion.js:51` wants one thing: the extension-to-format map,
// read only to know which file suffixes are worth offering when completing a
// `require("./…")` path. The mapping is a constant upstream too.

export const extensionFormatMap = {
	__proto__: null,
	".js": "commonjs",
	".cjs": "commonjs",
	".mjs": "module",
	".json": "json",
	".node": "commonjs",
	".wasm": "wasm",
};

export const defaultGetFormat = () => undefined;
export const defaultGetFormatWithoutErrors = () => undefined;

export default { extensionFormatMap, defaultGetFormat, defaultGetFormatWithoutErrors };
