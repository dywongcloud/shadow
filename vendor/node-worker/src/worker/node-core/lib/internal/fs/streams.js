// Forward to the impl in src/worker/node/fs/streams.ts, the same way ./dir.js
// forwards opendir. Upstream `internal/fs/streams` isn't usable here: it drives
// `require('fs').open/read/write/close` through a `kFs` indirection and assumes a
// real fd behind every read, while ours serves a whole file from one streamed
// GET.

import { ReadStream, WriteStream } from '../../../../node/fs/streams';

export { ReadStream, WriteStream };

export default {
  ReadStream,
  WriteStream,
};
