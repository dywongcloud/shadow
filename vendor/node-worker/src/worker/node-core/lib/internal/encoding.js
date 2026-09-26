// Browser TextEncoder/TextDecoder are spec-compliant; just expose them under
// the names node's `internal/encoding` uses.

const TextEncoder = globalThis.TextEncoder;
const TextDecoder = globalThis.TextDecoder;

const ENCODING_ALIASES = {
  __proto__: null,
  'utf-8': 'utf8',
  'utf8': 'utf8',
  'utf-16le': 'utf16le',
  'utf16le': 'utf16le',
  'ucs2': 'utf16le',
  'ucs-2': 'utf16le',
  'iso-8859-1': 'latin1',
  'latin1': 'latin1',
  'binary': 'latin1',
  'us-ascii': 'ascii',
  'ascii': 'ascii',
};

function getEncodingFromLabel(label) {
  const key = String(label).toLowerCase().trim();
  return ENCODING_ALIASES[key];
}

export {
  TextEncoder,
  TextDecoder,
  getEncodingFromLabel,
};

export default {
  TextEncoder,
  TextDecoder,
  getEncodingFromLabel,
};
