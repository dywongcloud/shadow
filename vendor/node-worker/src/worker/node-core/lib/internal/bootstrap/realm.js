// Minimal `internal/bootstrap/realm` override. Upstream this is the builtin
// module loader/registry; the modules we load reach two things on it.
//
// `BuiltinModule.exists(id)` comes from `internal/util/inspect.js`, used purely to
// annotate `node:` frames when formatting error stacks. Reporting "not a builtin"
// just skips that cosmetic annotation — everything else is unaffected.
//
// `getSchemeOnlyModuleNames()` comes from `internal/repl/completion.js:82`, which
// calls it at module load to build the list of names the prompt offers after
// `require("node:`. Upstream it is the handful of builtins that exist *only* under
// the scheme — `node:test`, `node:sea` — and this runtime has none: every name in
// its registry works with or without the prefix. The empty list is the true answer,
// not a stub.

const BuiltinModule = {
  exists(_id) {
    return false;
  },

  getSchemeOnlyModuleNames() {
    return [];
  },
};

export { BuiltinModule };

export default { BuiltinModule };
