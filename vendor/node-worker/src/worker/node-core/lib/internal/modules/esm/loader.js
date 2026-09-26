// Stub for `internal/modules/esm/loader`, for the same reason as `./resolve`:
// upstream this is node's ESM cascade, and bundling it drags in the whole module
// system behind it. This runtime resolves ESM its own way, in
// `src/worker/module/esm.ts`.
//
// `lib/repl.js:475` is what reaches it — the `importModuleDynamically` hook it
// gives every script it compiles, so that `await import("./x.mjs")` typed at the
// prompt goes somewhere. Nothing calls it here: the hook is only consulted for a
// dynamic import inside REPL-compiled source, and `internal/vm.js` never registers
// it, because this runtime's `vm` has no host-defined options to hang it on.

function unsupported(name) {
	return () => {
		throw new Error(
			`internal/modules/esm/loader.${name} is not implemented in this runtime`
		);
	};
}

export const getOrInitializeCascadedLoader = unsupported("getOrInitializeCascadedLoader");
export const registerModule = () => {};

export default { getOrInitializeCascadedLoader, registerModule };
