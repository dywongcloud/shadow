// `internalBinding('modules')` shim. Backs upstream
// `internal/modules/package_json_reader.js` so we can use its `read()` and
// `getNearestParentPackageJSON()` walk-ups verbatim instead of maintaining a
// hand-rolled equivalent in `module/resolve.ts`.
//
// The C++ binding returns a "serialized" tuple
//   [name, main, type, imports, exports, optionalFilePath]
// where `imports`/`exports` are either plain strings or a stringified JSON
// blob. `undefined` means the file doesn't exist. We mirror that shape here.

import fs from "../../node/fs";
import path from "../../node/path";

type SerializedPackageConfig =
	| undefined
	| [
			string | null,
			string | null,
			string | null,
			string | null,
			string | null,
			string | null,
	  ];

// Deliberately does NOT stat first.
//
// The read answers every question the stat did: a missing file is ENOENT, a
// non-directory component on the way down is ENOTDIR, and a *directory* named
// `package.json` — the only other thing `isFile()` was screening out, since nothing
// here reports a symlink or a device — is EISDIR. So the stat was a second round trip
// that could only ever confirm what the read was about to say, and this walk is the
// hottest caller of both: `getNearestParentPackageJSON` probes every ancestor of every
// specifier, and unlike the C++ binding it replaces, this shim has no cache in front of
// it. Halving that is worth more than the redundant check.
//
// (Upstream's own `readPackageJSON` does the same thing — it reads and treats any
// failure as "no package.json here" — so this is a convergence, not a divergence.)
function readPjson(jsonPath: string): SerializedPackageConfig {
	let raw: string;
	try {
		raw = fs.readFileSync(jsonPath, "utf-8") as string;
	} catch (e: any) {
		if (
			e &&
			(e.code === "ENOENT" || e.code === "ENOTDIR" || e.code === "EISDIR")
		) {
			return undefined;
		}
		throw e;
	}

	let parsed: any;
	try {
		parsed = JSON.parse(raw);
	} catch {
		return undefined;
	}
	if (!parsed || typeof parsed !== "object") return undefined;

	const name = typeof parsed.name === "string" ? parsed.name : null;
	const main = typeof parsed.main === "string" ? parsed.main : null;
	const type = typeof parsed.type === "string" ? parsed.type : null;
	const imports =
		parsed.imports == null
			? null
			: typeof parsed.imports === "string"
				? parsed.imports
				: JSON.stringify(parsed.imports);
	const exports =
		parsed.exports == null
			? null
			: typeof parsed.exports === "string"
				? parsed.exports
				: JSON.stringify(parsed.exports);

	return [name, main, type, imports, exports, jsonPath];
}

function readPackageJSON(
	jsonPath: string,
	_isESM?: boolean,
	_base?: string,
	_specifier?: string
): SerializedPackageConfig {
	return readPjson(jsonPath);
}

// Walk up from `checkPath` looking for the nearest enclosing `package.json`.
// Mirrors the C++ binding's contract — returns the same serialized tuple as
// `readPackageJSON`, or undefined if nothing was found.
function getNearestParentPackageJSON(
	checkPath: string
): SerializedPackageConfig {
	let dir = path.dirname(checkPath);
	const root = path.parse(dir).root;

	while (true) {
		const pjson = path.join(dir, "package.json");
		const result = readPjson(pjson);
		if (result !== undefined) return result;

		// Hack mirrored from the old resolver: stat-ing `/` blows up on the
		// puter backend, so stop one level above root.
		if (dir === root || path.dirname(dir) === root) return undefined;
		dir = path.dirname(dir);
	}
}

// Used by ESM/CJS `exports`/`imports` resolution paths in upstream. Returns
// either an array (the same serialized tuple) or a string path. We return the
// tuple for consistency.
function getPackageScopeConfig(
	resolved: string
): SerializedPackageConfig | string {
	const result = getNearestParentPackageJSON(resolved);
	if (result === undefined) {
		// Match upstream: when nothing is found, return the would-be path.
		return path.join(path.dirname(resolved), "package.json");
	}
	return result;
}

function getPackageType(url: string): string {
	const result = getNearestParentPackageJSON(url);
	if (!result) return "none";
	return result[2] ?? "none";
}

export default {
	readPackageJSON,
	getNearestParentPackageJSON,
	getPackageScopeConfig,
	getPackageType,
};
