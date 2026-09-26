// Base classes whose real superclass is resolved on first construction.
//
// The fs barrel sits inside a module-init cycle it can't escape:
//   node/events.ts -> node_core/lib/events.js -> internal/util/inspect.js
//     -> internal-binding/index.ts -> internal-binding/modules.ts
//     -> node/fs/index.ts -> ... -> node/events.ts
// (`internal-binding/modules.ts` needs fs to back upstream's module loader.)
//
// Anything in the fs subgraph therefore risks being evaluated *before* the
// `node/*` barrel it imports, which makes the usual module-scope idiom
//   const EventEmitter = events.EventEmitter
// read `undefined.EventEmitter` and throw. `class X extends EventEmitter` needs
// its superclass at class-definition time, so the read can't simply be moved
// into a method.
//
// So: subclass a placeholder, and the first time one is constructed, graft the
// real superclass into the placeholder's prototype chain. Because the chain is
// shared by reference, every instance — including ones already being constructed
// — picks it up, and `instanceof EventEmitter` holds afterwards.
//
// This works because node's EventEmitter, Readable and Writable are all plain
// functions rather than ES classes, so they can be `.call()`ed on an existing
// `this`. An ES class would throw "cannot be invoked without 'new'".

import events from "../events";
import nodeStream from "../stream";

function lazyBase(resolve: () => any): any {
	let linked = false;
	class Base {
		constructor(...args: any[]) {
			const Real = resolve();
			if (!linked) {
				linked = true;
				Object.setPrototypeOf(Base.prototype, Real.prototype);
				Object.setPrototypeOf(Base, Real);
			}
			Real.call(this, ...args);
		}
	}
	return Base;
}

export const EmitterBase = lazyBase(() => events.EventEmitter);
export const ReadableBase = lazyBase(() => nodeStream.Readable);
export const WritableBase = lazyBase(() => nodeStream.Writable);
