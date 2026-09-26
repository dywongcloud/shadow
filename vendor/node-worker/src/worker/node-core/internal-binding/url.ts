// `internalBinding('url')` — only the bits upstream JS code actually pulls in.
// `canParse` ships as a static on the global URL since 2024. `format` is the
// helper backing `url.format(URL, options)`; we re-implement it on top of
// globalThis.URL since the native binding lives in C++. IDNA conversion is
// delegated to tr46 (UTS #46), matching what node does via ICU.

// @ts-ignore
import tr46 from "tr46";

function applyFormatOptions(href: string, options: {
	fragment: boolean;
	unicode: boolean;
	search: boolean;
	auth: boolean;
}): string {
	const u = new URL(href);
	if (!options.auth) {
		u.username = "";
		u.password = "";
	}
	if (!options.search) {
		u.search = "";
	}
	if (!options.fragment) {
		u.hash = "";
	}
	if (options.unicode && u.hostname) {
		const decoded = tr46.toUnicode(u.hostname).domain;
		if (decoded) u.hostname = decoded;
	}
	return u.toString();
}

export default {
	canParse(input: string, base?: string): boolean {
		return URL.canParse(input, base);
	},
	domainToASCII(input: string): string {
		return tr46.toASCII(input) ?? "";
	},
	domainToUnicode(input: string): string {
		return tr46.toUnicode(input).domain;
	},
	format(
		href: string,
		fragment: boolean,
		unicode: boolean,
		search: boolean,
		auth: boolean
	): string {
		return applyFormatOptions(href, { fragment, unicode, search, auth });
	},
};
