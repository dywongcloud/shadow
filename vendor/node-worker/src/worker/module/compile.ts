// Compile a module body into a callable function, named so devtools can find it.
//
// `new Function(params, code)` gives V8 an anonymous script, and everything that
// runs inside it is stamped with where it was *compiled* rather than what it is:
//
//   at inner (eval at esmHelper (worker-cqtaabdx.js:116661:16), <anonymous>:5:3)
//
// which names the bundle, not the module, keeps the file out of the Sources tree,
// and gives breakpoints nothing to attach to. A `//# sourceURL=` pragma is all
// V8 needs to treat the script as its own file:
//
//   at inner (/proj/node_modules/foo/index.js:5:3)
//
// and devtools lists it at that path, with breakpoints that survive reloads.
//
// The URL is a bare vfs path, not a `file://` URL, because that is the form node
// itself puts in stack traces — anything that parses a frame back into a path to
// read it (code-frame printers, source-map-support, test runners) expects a path
// there.
//
// Indirect eval rather than `new Function` because of line numbers: V8 counts the
//
//   function anonymous(require,module
//   ) {
//
// preamble it synthesizes for `new Function` as part of the script, so every
// frame inside it reports two lines lower than the file it came from. The wrapper
// below is one line with no trailing newline, so body line N stays line N.
// That's exact for CJS — the await transform only ever makes same-line edits, and
// files with no `await` aren't touched at all — so a CJS frame's line/column pair
// now matches the real file. ESM is close but not exact: the SystemJS lowering
// hoists function declarations up into the `register` header, so lines below the
// first hoisted `function` shift up. Fixing that needs real mappings out of the
// rewriter; the sourceURL is orthogonal and lands either way.

// Calling through anything but the bare `eval` identifier compiles in global
// scope, exactly as `new Function` does: the module body must not see this
// module's locals, and must not inherit the bundle's strict mode — sloppy is what
// CJS gets in node, and plenty of dependencies need it (`with`, implicit globals,
// octal literals). The `(0, eval)` spelling keeps that true even if a minifier
// ever inlines this binding; a bare `eval(code)` would be a *direct* eval.
const indirectEval = (code: string): any => (0, eval)(code);

// `//# sourceURL=` runs to the end of the line, so a newline in a path would
// close the pragma and leak the remainder back into the script.
function sanitizeSourceURL(path: string): string {
	return path.replace(/[\r\n]/g, "");
}

export function compileModuleFunction(
	params: string[],
	code: string,
	path: string
): Function {
	// The newline before `}` is load-bearing: a source whose last line is a `//`
	// comment with no trailing newline (a `//# sourceMappingURL=` pragma, which
	// most bundled files end with) would otherwise swallow the closing brace.
	let src =
		`(function (${params.join(", ")}) {` +
		code +
		`\n})\n//# sourceURL=${sanitizeSourceURL(path)}`;
	return indirectEval(src) as Function;
}
