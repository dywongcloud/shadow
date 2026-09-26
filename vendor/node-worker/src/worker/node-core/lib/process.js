// Forward to the impl in src/worker/node/process.ts. Named exports cover the
// process surface so bare `require('process').env` from inside node_modules/
// hits the real values rather than the synthetic namespace wrapper.
//
// stdin/stdout/stderr are deliberately omitted: they're populated by
// initConsole *after* this module's named-export bindings are snapshotted, so
// a named export would be perpetually undefined. Consumers that need live
// stdio should `import process from 'process'` and read the default — that
// reference stays in sync because it's the same live object.
import process from '../../node/process';

export const env = process.env;
export const platform = process.platform;
export const arch = process.arch;
export const pid = process.pid;
export const ppid = process.ppid;
export const argv = process.argv;
export const argv0 = process.argv0;
export const execPath = process.execPath;
export const execArgv = process.execArgv;
export const version = process.version;
export const versions = process.versions;
export const features = process.features;

export const cwd = process.cwd;
export const chdir = process.chdir;
export const nextTick = process.nextTick;
export const emitWarning = process.emitWarning;
// `process` is now an EventEmitter, so these are `this`-dependent prototype
// methods. Bind them to the live process so `const { on } = require('process')`
// keeps working when destructured off the named exports.
export const on = process.on.bind(process);
export const once = process.once.bind(process);
export const off = process.off.bind(process);
export const addListener = process.addListener.bind(process);
export const removeListener = process.removeListener.bind(process);
export const removeAllListeners = process.removeAllListeners.bind(process);
export const listeners = process.listeners.bind(process);
export const listenerCount = process.listenerCount.bind(process);
export const emit = process.emit.bind(process);
export const kill = process.kill;
export const exit = process.exit;
export const hrtime = process.hrtime;
export const uptime = process.uptime;
export const binding = process.binding;

export default process;
