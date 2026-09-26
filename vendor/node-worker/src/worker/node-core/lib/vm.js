// `node:vm` for node-core's own consumers, so they get the same one user code does.
//
// Without this, a `require('vm')` from inside the node tree resolves to upstream
// `node_core/lib/vm.js` — a thin wrapper over `internalBinding('contextify')` and
// `internal/vm` — while `require("node:vm")` from a script resolves to
// `src/worker/node/vm.ts`. Two different `vm`s, and `lib/repl.js:104` is the first
// module in the tree to ask for one.
//
// It matters more than tidiness here. `repl.js:106` lifts
// `vm.Script.prototype.runInThisContext` off whichever `vm` it got and applies it to
// a script built by `internal/vm`'s `makeContextifyScript` — so both have to be the
// same function over the same fields, which they are only if there is one `vm`.
//
// Same shape as ./tls.js and ./child_process.js.
import vm from "../../node/vm";

export const Script = vm.Script;
export const createContext = vm.createContext;
export const createScript = vm.createScript;
export const runInContext = vm.runInContext;
export const runInNewContext = vm.runInNewContext;
export const runInThisContext = vm.runInThisContext;
export const compileFunction = vm.compileFunction;
export const isContext = vm.isContext;
export const measureMemory = vm.measureMemory;
export const constants = vm.constants;

export default vm;
