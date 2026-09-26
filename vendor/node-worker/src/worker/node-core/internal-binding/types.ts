// `internalBinding('types')` — upstream provides these predicates from C++.
// Almost all are recoverable in pure JS via `Object.prototype.toString` tags
// (realm-agnostic) plus a couple of instanceof checks. Upstream
// `internal/util/types.js` spreads this object and layers the TypedArray
// predicates on top, so this binding makes that file (and `node:util/types`)
// work verbatim via Rollup fallthrough. Only `isProxy` and `isExternal` have no
// JS equivalent (proxies are transparent; there are no native externals here),
// so they always report false — matching how those features are unused in this
// runtime.

const toString = Object.prototype.toString;

function tagged(tag: string): (v: unknown) => boolean {
	const expected = `[object ${tag}]`;
	return (v: unknown) => toString.call(v) === expected;
}

// Boxed primitives: same toString tag as the primitive, distinguished by being
// an object (typeof 'object') rather than the primitive itself.
function boxed(tag: string): (v: unknown) => boolean {
	const expected = `[object ${tag}]`;
	return (v: unknown) =>
		typeof v === "object" && v !== null && toString.call(v) === expected;
}

const isDate = tagged("Date");
const isRegExp = tagged("RegExp");
const isArgumentsObject = tagged("Arguments");
const isGeneratorFunction = tagged("GeneratorFunction");
const isAsyncFunction = tagged("AsyncFunction");
const isGeneratorObject = tagged("Generator");
const isMap = tagged("Map");
const isSet = tagged("Set");
const isWeakMap = tagged("WeakMap");
const isWeakSet = tagged("WeakSet");
const isMapIterator = tagged("Map Iterator");
const isSetIterator = tagged("Set Iterator");
const isArrayBuffer = tagged("ArrayBuffer");
const isSharedArrayBuffer = tagged("SharedArrayBuffer");
const isModuleNamespaceObject = tagged("Module");

const isNumberObject = boxed("Number");
const isStringObject = boxed("String");
const isBooleanObject = boxed("Boolean");
const isBigIntObject = boxed("BigInt");
const isSymbolObject = boxed("Symbol");

function isNativeError(v: unknown): boolean {
	return v instanceof Error || toString.call(v) === "[object Error]";
}

function isPromise(v: unknown): boolean {
	return v instanceof Promise || toString.call(v) === "[object Promise]";
}

function isDataView(v: unknown): boolean {
	return v instanceof DataView || toString.call(v) === "[object DataView]";
}

function isArrayBufferView(v: unknown): boolean {
	return ArrayBuffer.isView(v);
}

function isTypedArray(v: unknown): boolean {
	return ArrayBuffer.isView(v) && !(v instanceof DataView);
}

function isAnyArrayBuffer(v: unknown): boolean {
	return isArrayBuffer(v) || isSharedArrayBuffer(v);
}

function isBoxedPrimitive(v: unknown): boolean {
	return (
		isNumberObject(v) ||
		isStringObject(v) ||
		isBooleanObject(v) ||
		isBigIntObject(v) ||
		isSymbolObject(v)
	);
}

export default {
	// No JS equivalent — see file header.
	isExternal: (_v: unknown) => false,
	isProxy: (_v: unknown) => false,

	isDate,
	isArgumentsObject,
	isBigIntObject,
	isBooleanObject,
	isNumberObject,
	isStringObject,
	isSymbolObject,
	isNativeError,
	isRegExp,
	isAsyncFunction,
	isGeneratorFunction,
	isGeneratorObject,
	isPromise,
	isMap,
	isSet,
	isMapIterator,
	isSetIterator,
	isWeakMap,
	isWeakSet,
	isArrayBuffer,
	isDataView,
	isSharedArrayBuffer,
	isModuleNamespaceObject,
	isAnyArrayBuffer,
	isBoxedPrimitive,
	isArrayBufferView,
	isTypedArray,

	// TypedArray predicates via toString tag. `internal/util/types.js` overrides
	// these with its own copies, but direct binding consumers get them too.
	isUint8Array: tagged("Uint8Array"),
	isUint8ClampedArray: tagged("Uint8ClampedArray"),
	isUint16Array: tagged("Uint16Array"),
	isUint32Array: tagged("Uint32Array"),
	isInt8Array: tagged("Int8Array"),
	isInt16Array: tagged("Int16Array"),
	isInt32Array: tagged("Int32Array"),
	isFloat16Array: tagged("Float16Array"),
	isFloat32Array: tagged("Float32Array"),
	isFloat64Array: tagged("Float64Array"),
	isBigInt64Array: tagged("BigInt64Array"),
	isBigUint64Array: tagged("BigUint64Array"),
};
