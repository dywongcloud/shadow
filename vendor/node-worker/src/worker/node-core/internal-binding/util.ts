// `internalBinding('util')` — the surface upstream node-core JS actually calls.
// `internal/util/inspect.js` destructures the property-filter constants and a
// handful of reflection helpers from here; most are recoverable in pure JS.
// Two are genuinely native-only and degrade gracefully:
//   - getPromiseDetails: a promise's state/value can't be read synchronously in
//     JS, so we always report "pending" (promises inspect as `Promise { <pending> }`).
//   - getProxyDetails / getExternalValue: proxies are transparent and there are
//     no native externals here, so these are only reached via unused paths.

// V8 PropertyFilter bits and promise-state enum. SKIP_SYMBOLS is
// `internal/repl/completion.js`'s: it lists an object's own properties to offer
// them after a dot, and a symbol is not something anyone can type.
const ALL_PROPERTIES = 0;
const ONLY_ENUMERABLE = 2;
const SKIP_SYMBOLS = 16;
const kPending = 0;
const kRejected = 2;

function isInsideNodeModules(_depth: number): boolean {
	return false;
}

// See header. `internal/console/constructor.js`'s `table` uses this on Map/Set
// iterators; console hands us a throwaway iterator, so draining it is fine.
function previewEntries(iterator: Iterable<unknown>, isKeyValue?: boolean): any {
	const entries = Array.from(iterator);
	if (isKeyValue) {
		const flat: unknown[] = [];
		let keyValue = false;
		for (const entry of entries) {
			if (Array.isArray(entry) && entry.length === 2) {
				keyValue = true;
				flat.push(entry[0], entry[1]);
			} else {
				flat.push(entry);
			}
		}
		return [flat, keyValue];
	}
	return entries;
}

// A canonical array index is an integer in [0, 2^32 - 1) whose string form
// round-trips. Those are the keys upstream's C++ helper skips.
function isArrayIndex(key: string): boolean {
	const n = Number(key);
	return Number.isInteger(n) && n >= 0 && n < 4294967295 && String(n) === key;
}

// own, non-array-index properties (strings + symbols), optionally enumerable
// only — matches the subset of the C++ helper inspect relies on (filter is
// ALL_PROPERTIES or ONLY_ENUMERABLE).
function getOwnNonIndexProperties(obj: object, filter: number): (string | symbol)[] {
	const onlyEnumerable = (filter & ONLY_ENUMERABLE) !== 0;
	const skipSymbols = (filter & SKIP_SYMBOLS) !== 0;
	const result: (string | symbol)[] = [];
	for (const key of Object.getOwnPropertyNames(obj)) {
		if (isArrayIndex(key)) continue;
		if (onlyEnumerable && !Object.getOwnPropertyDescriptor(obj, key)?.enumerable) continue;
		result.push(key);
	}
	if (skipSymbols) return result;
	for (const sym of Object.getOwnPropertySymbols(obj)) {
		if (onlyEnumerable && !Object.getOwnPropertyDescriptor(obj, sym)?.enumerable) continue;
		result.push(sym);
	}
	return result;
}

// Fallback constructor name (inspect walks the prototype chain itself and only
// calls this when that fails, e.g. null-proto objects). The toString tag gives
// "Object"/"Array"/"Uint8Array"/… which is the right shape.
function getConstructorName(obj: object): string {
	return Object.prototype.toString.call(obj).slice(8, -1);
}

function getPromiseDetails(_promise: unknown): [number, unknown] {
	return [kPending, undefined];
}

function getProxyDetails(_value: unknown, _fullProxy?: boolean): undefined {
	return undefined;
}

function getExternalValue(_value: unknown): bigint {
	return 0n;
}

export default {
	constants: { ALL_PROPERTIES, ONLY_ENUMERABLE, SKIP_SYMBOLS, kPending, kRejected },
	isInsideNodeModules,
	previewEntries,
	getOwnNonIndexProperties,
	getConstructorName,
	getPromiseDetails,
	getProxyDetails,
	getExternalValue,
};
