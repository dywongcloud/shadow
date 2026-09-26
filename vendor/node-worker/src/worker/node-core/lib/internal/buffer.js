import { Buffer } from 'buffer';

// Mostly a pass-through to the npm `buffer` polyfill, with one difference:
// upstream's `FastBuffer` is `class FastBuffer extends Uint8Array {}`, so
// `new FastBuffer()` (no args) yields an empty buffer. The polyfill's
// `new Buffer()` rejects undefined input, which breaks callers like
// `ZlibBase.prototype._flush` that use a bare `new FastBuffer()` placeholder.
function FastBuffer(arg, encodingOrOffset, length) {
  if (arg === undefined) return Buffer.alloc(0);
  if (typeof arg === 'number') return Buffer.alloc(arg);
  return Buffer.from(arg, encodingOrOffset, length);
}
FastBuffer.prototype = Buffer.prototype;

// Standalone big-endian reads (upstream internal/buffer exports these as
// `(buf, offset)` functions; used by internal/http2 getUnpackedSettings).
function readUInt16BE(buf, offset = 0) {
  return buf[offset] * 2 ** 8 + buf[offset + 1];
}

function readUInt32BE(buf, offset = 0) {
  return (
    buf[offset] * 2 ** 24 +
    buf[offset + 1] * 2 ** 16 +
    buf[offset + 2] * 2 ** 8 +
    buf[offset + 3]
  );
}

export { FastBuffer, readUInt16BE, readUInt32BE };

export default {
  FastBuffer,
  readUInt16BE,
  readUInt32BE,
};
