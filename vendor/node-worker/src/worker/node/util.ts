// `inspect`/`format`/`formatWithOptions`/`getStringWidth`/`stripVTControlCharacters`
// and `types` all come from upstream Node via the Rollup fallthrough (backed by
// the pure-JS `internalBinding('types')`/`('util')`/`('config')` shims). This
// gives real Node inspect fidelity — Map/Set/TypedArray/getter rendering,
// depth/breakLength/compact layout, circular refs, and colors that honor
// options — while promisify/inherits/deprecate/legacy `is*` helpers stay local.
// @ts-ignore resolved by the worker Rollup pipeline.
import types from "node-core:util/types";
// @ts-ignore resolved by the worker Rollup pipeline.
import inspectModule from "node-core:internal/util/inspect";
// Upstream's own comparison, for the same reason as `inspect`: deep equality is subtle in precisely
// the places a reimplementation gets wrong — boxed primitives, typed arrays versus their backing
// buffers, Map and Set membership, circular references, prototype identity — and node already has
// all of it.
// @ts-ignore resolved by the worker Rollup pipeline.
import comparisons from "node-core:internal/util/comparisons";

const { inspect, format, formatWithOptions, getStringWidth, stripVTControlCharacters } =
	inspectModule;

const kCustomPromisifiedSymbol = Symbol.for("nodejs.util.promisify.custom");
const kCustomPromisifyArgsSymbol = Symbol.for(
	"nodejs.util.promisify.customArgs"
);

function promisify(original: any): any {
	if (typeof original !== "function") {
		throw new TypeError("argument must be a function");
	}

	if (original[kCustomPromisifiedSymbol]) {
		const fn = original[kCustomPromisifiedSymbol];
		if (typeof fn !== "function") {
			throw new TypeError("custom promisified function must be a function");
		}
		Object.defineProperty(fn, kCustomPromisifiedSymbol, {
			value: fn,
			enumerable: false,
			writable: false,
			configurable: true,
		});
		return fn;
	}

	const argumentNames = original[kCustomPromisifyArgsSymbol];

	function fn(this: any, ...args: any[]) {
		return new Promise((resolve, reject) => {
			args.push((err: any, ...values: any[]) => {
				if (err) return reject(err);
				if (argumentNames !== undefined && values.length > 1) {
					const obj: any = {};
					for (let i = 0; i < argumentNames.length; i++) {
						obj[argumentNames[i]] = values[i];
					}
					resolve(obj);
				} else {
					resolve(values[0]);
				}
			});
			Reflect.apply(original, this, args);
		});
	}

	Object.setPrototypeOf(fn, Object.getPrototypeOf(original));
	Object.defineProperty(fn, kCustomPromisifiedSymbol, {
		value: fn,
		enumerable: false,
		writable: false,
		configurable: true,
	});
	return Object.defineProperties(
		fn,
		Object.getOwnPropertyDescriptors(original)
	);
}

(promisify as any).custom = kCustomPromisifiedSymbol;

function inherits(ctor: any, superCtor: any) {
	if (typeof superCtor !== "function" && superCtor !== null) {
		throw new TypeError("superCtor must be a function or null");
	}
	Object.defineProperty(ctor, "super_", {
		value: superCtor,
		enumerable: false,
		writable: true,
		configurable: true,
	});
	if (superCtor) {
		Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
	}
}

function deprecate<T extends (...args: any[]) => any>(fn: T, _msg: string): T {
	let warned = false;
	const wrapped = function (this: any, ...args: any[]) {
		if (!warned) {
			warned = true;
			if (typeof console !== "undefined" && console.warn) {
				console.warn(`(node:1) DeprecationWarning: ${_msg}`);
			}
		}
		return fn.apply(this, args);
	} as unknown as T;
	return wrapped;
}

/**
 * `util.callbackify`, the inverse of `promisify`.
 *
 * Two details are load-bearing and easy to drop. The callback runs on `nextTick` rather than in the
 * promise's own microtask, so a throw from it is an uncaught exception instead of a rejected
 * promise nobody is watching. And a promise that rejects with a *falsy* value still has to reach
 * the callback as an error, since `cb(null)` would read as success — node wraps it for that reason.
 */
function callbackify(original: (...args: any[]) => Promise<any>): (...args: any[]) => void {
	if (typeof original !== "function") {
		throw new TypeError("original must be a function");
	}

	function callbackified(this: any, ...args: any[]): void {
		const callback = args.pop();
		if (typeof callback !== "function") {
			throw new TypeError("last argument must be a function");
		}
		const bound = callback.bind(this);
		Reflect.apply(original, this, args).then(
			(value: any) => process.nextTick(bound, null, value),
			(reason: any) =>
				process.nextTick(
					bound,
					reason ||
						Object.assign(
							new Error(`Promise was rejected with a falsy value`),
							{ code: "ERR_FALSY_VALUE_REJECTION", reason }
						)
				)
		);
	}

	// Carry the original's own properties over, as node does: `length` gains the callback argument
	// and `name` gains the suffix, both of which are observable and documented.
	const descriptors: Record<string, PropertyDescriptor> =
		Object.getOwnPropertyDescriptors(original);
	if (typeof descriptors.length?.value === "number") descriptors.length.value++;
	if (typeof descriptors.name?.value === "string") descriptors.name.value += "Callbackified";
	Object.defineProperties(callbackified, descriptors);
	return callbackified;
}

const util = {
	promisify,
	callbackify,
	isDeepStrictEqual: comparisons.isDeepStrictEqual,
	format,
	formatWithOptions,
	inspect,
	stripVTControlCharacters,
	getStringWidth,
	inherits,
	deprecate,
	types,
	debuglog: (_section: string) => () => {},
	debug: (_section: string) => () => {},
	isArray: Array.isArray,
	isBoolean: (v: any) => typeof v === "boolean",
	isNull: (v: any) => v === null,
	isNullOrUndefined: (v: any) => v == null,
	isNumber: (v: any) => typeof v === "number",
	isString: (v: any) => typeof v === "string",
	isSymbol: (v: any) => typeof v === "symbol",
	isUndefined: (v: any) => v === undefined,
	isFunction: (v: any) => typeof v === "function",
	isObject: (v: any) => v !== null && typeof v === "object",
	isPrimitive: (v: any) => v === null || (typeof v !== "object" && typeof v !== "function"),
	isBuffer: (v: any) =>
		v != null && v.constructor != null && typeof v.constructor.isBuffer === "function" && v.constructor.isBuffer(v),
	TextEncoder,
	TextDecoder,
};

export default util as unknown as typeof import("node:util");
