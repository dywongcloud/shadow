// `internalBinding('encoding_binding')` — IDNA conversion bits. Native node
// uses ICU; tr46 is the pure-JS UTS #46 implementation (also what
// `whatwg-url` ships, so it matches browser behavior).

// @ts-ignore — tr46 has no TS types in its package
import tr46 from "tr46";

export default {
	toASCII(input: string): string {
		const result = tr46.toASCII(input);
		// tr46 returns null on processing failure; node's binding returns the
		// empty string in that case.
		return result ?? "";
	},
	toUnicode(input: string): string {
		return tr46.toUnicode(input).domain;
	},
};
