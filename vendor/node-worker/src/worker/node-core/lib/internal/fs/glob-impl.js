// The four fs functions upstream `internal/fs/glob` needs, reached the way every
// other override here reaches them: by relative import of the impl modules
// rather than through the `fs` barrel.
//
// `../../fs.js` is a forwarder whose own top level reads properties off
// `node/fs/index.ts`, and index.ts is what exposes glob — so letting upstream
// `require('fs')` here would put that forwarder in a cycle with index.ts, where
// it evaluates first and throws "Cannot access 'fs$N' before initialization".
// Importing ./sync.ts and ./promises.ts directly skips the barrel.
//
// Wrapped rather than re-exported for two reasons: the property read happens at
// call time (so this module is safe to evaluate at any point), and the receiver
// is preserved — `lstatSync` reaches `this.statSync` and `lstat` reaches
// `this.stat`.
//
// See node-patches/0002-glob-lazy-fs.patch for the upstream side, and
// node/fs/glob.ts for why the glob entry points live outside sync.ts.

import { fsSync } from '../../../../node/fs/sync';
import { promisesToDepromisify } from '../../../../node/fs/promises';

export const lstatSync = (path, options) => fsSync.lstatSync(path, options);
export const readdirSync = (path, options) => fsSync.readdirSync(path, options);
export const lstat = (path, options) => promisesToDepromisify.lstat(path, options);
export const readdir = (path, options) => promisesToDepromisify.readdir(path, options);

export default {
  lstatSync,
  readdirSync,
  lstat,
  readdir,
};
