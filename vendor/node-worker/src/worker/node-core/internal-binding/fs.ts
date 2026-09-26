// `internalBinding('fs')` — destructured at module top by upstream code (e.g.
// `internal/modules/package_json_reader`). The methods are only invoked from
// code paths we don't exercise; they exist so the destructure doesn't crash
// at module load. Any actual call routes through the user-facing
// `node-core:fs` (our custom puter-fs impl) instead.

function unsupported(name: string): never {
	throw new Error(
		`internalBinding('fs').${name} is not implemented in this runtime`
	);
}

export default {
	internalModuleStat(_filename: string): number {
		// Spec: 0 = file, 1 = directory, negative = errno. We return -ENOENT so
		// any caller that doesn't go through `node-core:fs` sees "not found"
		// rather than crashing.
		return -2;
	},
	open() {
		return unsupported("open");
	},
	close() {
		return unsupported("close");
	},
	read() {
		return unsupported("read");
	},
	readFileUtf8() {
		return unsupported("readFileUtf8");
	},
	stat() {
		return unsupported("stat");
	},
};
