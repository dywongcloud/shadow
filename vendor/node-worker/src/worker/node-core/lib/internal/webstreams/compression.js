// Browser-native CompressionStream / DecompressionStream support deflate,
// deflate-raw and gzip. Anything beyond that (brotli, etc.) is left to the
// platform — in 2026-era browsers brotli also lands here, but we don't
// fall back to anything if it's missing.
const CompressionStream = globalThis.CompressionStream;
const DecompressionStream = globalThis.DecompressionStream;

export {
  CompressionStream,
  DecompressionStream,
};

export default {
  CompressionStream,
  DecompressionStream,
};
