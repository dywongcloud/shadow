// `internalBinding('buffer')`.
//
// Only `compare` so far, which is what `internal/util/comparisons.js` needs to decide typed-array
// and Buffer equality. Members get added as the upstream modules we pull in actually ask for them —
// a half-guessed binding is worse than a missing one, because a wrong answer here is silent.
//
// A leaf module on purpose: ./index.ts is inside a require cycle and only the bindings that import
// nothing are safe to resolve during bootstrap.

/**
 * node's `compare(a, b)`: memcmp over the shared prefix, then shorter-is-less.
 *
 * Returns -1, 0 or 1 — not the raw byte difference. Callers switch on the three values, and
 * `comparisons.js` tests `=== 0`.
 */
function compare(a: Uint8Array, b: Uint8Array): -1 | 0 | 1 {
	if (a === b) return 0;
	const shared = Math.min(a.length, b.length);
	for (let i = 0; i < shared; i++) {
		if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
	}
	if (a.length === b.length) return 0;
	return a.length < b.length ? -1 : 1;
}

export default { compare };
