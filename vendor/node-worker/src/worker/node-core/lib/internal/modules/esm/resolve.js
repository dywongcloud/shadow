// Stub for `internal/modules/esm/resolve`. Upstream
// `internal/modules/package_json_reader` does a lazy require of this inside
// `findPackageJSON()` to handle URL-style specifiers — a code path the worker
// never calls. We override it here so rollup's static bundling doesn't drag in
// the entire ESM resolver (cjs/loader, esm/loader, get_format, ...). Calling
// any of these stubs will throw, which is the right outcome.

function unsupported(name) {
	return () => {
		throw new Error(
			`internal/modules/esm/resolve.${name} is not implemented in this runtime`
		);
	};
}

export const defaultResolve = unsupported("defaultResolve");
export const decorateErrorWithCommonJSHints = unsupported("decorateErrorWithCommonJSHints");
export const encodedSepRegEx = /%2[fF]|%5[cC]/;
export const legacyMainResolve = unsupported("legacyMainResolve");
export const packageExportsResolve = unsupported("packageExportsResolve");
export const packageImportsResolve = unsupported("packageImportsResolve");
export const packageResolve = unsupported("packageResolve");
export const throwIfInvalidParentURL = () => {};

export default {
	defaultResolve,
	decorateErrorWithCommonJSHints,
	encodedSepRegEx,
	legacyMainResolve,
	packageExportsResolve,
	packageImportsResolve,
	packageResolve,
	throwIfInvalidParentURL,
};
