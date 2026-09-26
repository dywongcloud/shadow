// node:constants (deprecated, DEP0008). Upstream `lib/constants.js` does nothing
// but flatten the os/fs/crypto constant tables into one frozen object; we mirror
// that exactly, sourcing from the same public modules instead of
// `internalBinding('constants')`. Completeness tracks those modules — extend
// os.constants / fs.constants / crypto.constants to grow this.
import os from "./os";
import fs from "./fs";
import crypto from "./crypto";

const constants = Object.freeze({
	...os.constants.dlopen,
	...os.constants.errno,
	...os.constants.priority,
	...os.constants.signals,
	...fs.constants,
	...crypto.constants,
});

export default constants as unknown as typeof import("node:constants");
