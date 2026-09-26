// Forward to the impl in src/worker/node/fs/watch.ts. Upstream
// `internal/fs/watchers` is built on the libuv FSEvent/StatWatcher handles from
// internalBinding('fs_event_wrap'), which have no analogue here — ours are driven
// by puter's `item.*` socket.io feed instead.

import { unsupported } from './_unsupported.js';
import { FSWatcher, StatWatcher, watch } from '../../../../node/fs/watch';

// Upstream uses this symbol to start a watcher it constructed itself; ours start
// in their constructor, so it's an inert marker. Kept because
// `internal/fs/recursive_watch` imports it.
export const kFSWatchStart = Symbol('kFSWatchStart');

// Only reachable through upstream's `fs.watch` ignore-pattern handling, which we
// don't route through. Loud, rather than silently dropping a filter.
export const createIgnoreMatcher = unsupported(
  'internal/fs/watchers.createIgnoreMatcher',
);

export { FSWatcher, StatWatcher, watch };

export default {
  kFSWatchStart,
  createIgnoreMatcher,
  watch,
  FSWatcher,
  StatWatcher,
};
