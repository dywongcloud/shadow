// Mirrors `internalBinding('constants')` from upstream node. Each top-level key
// (zlib/os/fs/crypto/...) is a flat object of numeric constants. Only keys that
// the bundled node code actually reads need to be filled in — extend as needed.

const zlib = {
	// flush values
	Z_NO_FLUSH: 0,
	Z_PARTIAL_FLUSH: 1,
	Z_SYNC_FLUSH: 2,
	Z_FULL_FLUSH: 3,
	Z_FINISH: 4,
	Z_BLOCK: 5,
	Z_TREES: 6,

	// return codes
	Z_OK: 0,
	Z_STREAM_END: 1,
	Z_NEED_DICT: 2,
	Z_ERRNO: -1,
	Z_STREAM_ERROR: -2,
	Z_DATA_ERROR: -3,
	Z_MEM_ERROR: -4,
	Z_BUF_ERROR: -5,
	Z_VERSION_ERROR: -6,

	// compression levels
	Z_NO_COMPRESSION: 0,
	Z_BEST_SPEED: 1,
	Z_BEST_COMPRESSION: 9,
	Z_DEFAULT_COMPRESSION: -1,

	// strategies
	Z_FILTERED: 1,
	Z_HUFFMAN_ONLY: 2,
	Z_RLE: 3,
	Z_FIXED: 4,
	Z_DEFAULT_STRATEGY: 0,

	// node_zlib_mode (matches src/node_zlib.cc enum)
	DEFLATE: 1,
	INFLATE: 2,
	GZIP: 3,
	GUNZIP: 4,
	DEFLATERAW: 5,
	INFLATERAW: 6,
	UNZIP: 7,
	BROTLI_DECODE: 8,
	BROTLI_ENCODE: 9,
	ZSTD_COMPRESS: 10,
	ZSTD_DECOMPRESS: 11,

	// zlib option ranges
	Z_MIN_WINDOWBITS: 8,
	Z_MAX_WINDOWBITS: 15,
	Z_DEFAULT_WINDOWBITS: 15,
	Z_MIN_CHUNK: 64,
	Z_MAX_CHUNK: Infinity,
	Z_DEFAULT_CHUNK: 16384,
	Z_MIN_MEMLEVEL: 1,
	Z_MAX_MEMLEVEL: 9,
	Z_DEFAULT_MEMLEVEL: 8,
	Z_MIN_LEVEL: -1,
	Z_MAX_LEVEL: 9,
	Z_DEFAULT_LEVEL: -1,

	// brotli operations
	BROTLI_OPERATION_PROCESS: 0,
	BROTLI_OPERATION_FLUSH: 1,
	BROTLI_OPERATION_FINISH: 2,
	BROTLI_OPERATION_EMIT_METADATA: 3,
	BROTLI_MODE_GENERIC: 0,
	BROTLI_MODE_TEXT: 1,
	BROTLI_MODE_FONT: 2,
	BROTLI_DEFAULT_MODE: 0,
	BROTLI_MIN_QUALITY: 0,
	BROTLI_MAX_QUALITY: 11,
	BROTLI_DEFAULT_QUALITY: 11,
	BROTLI_MIN_WINDOW_BITS: 10,
	BROTLI_MAX_WINDOW_BITS: 24,
	BROTLI_LARGE_MAX_WINDOW_BITS: 30,
	BROTLI_DEFAULT_WINDOW: 22,
	BROTLI_MIN_INPUT_BLOCK_BITS: 16,
	BROTLI_MAX_INPUT_BLOCK_BITS: 24,
	BROTLI_PARAM_MODE: 0,
	BROTLI_PARAM_QUALITY: 1,
	BROTLI_PARAM_LGWIN: 2,
	BROTLI_PARAM_LGBLOCK: 3,
	BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING: 4,
	BROTLI_PARAM_SIZE_HINT: 5,
	BROTLI_PARAM_LARGE_WINDOW: 6,
	BROTLI_PARAM_NPOSTFIX: 7,
	BROTLI_PARAM_NDIRECT: 8,

	// zstd end directives (matches ZSTD_EndDirective)
	ZSTD_e_continue: 0,
	ZSTD_e_flush: 1,
	ZSTD_e_end: 2,
};

// `internalBinding('constants').crypto`, read by crypto.js + internal/crypto/*
// (RSA padding modes, PSS salt-length sentinels, EC point conversion, DH check
// bits). Values match OpenSSL's headers / upstream node.
const crypto = {
	// RSA padding modes (openssl/rsa.h)
	RSA_PKCS1_PADDING: 1,
	RSA_SSLV23_PADDING: 2,
	RSA_NO_PADDING: 3,
	RSA_PKCS1_OAEP_PADDING: 4,
	RSA_X931_PADDING: 5,
	RSA_PKCS1_PSS_PADDING: 6,

	// PSS salt-length sentinels (openssl/rsa.h)
	RSA_PSS_SALTLEN_DIGEST: -1,
	RSA_PSS_SALTLEN_AUTO: -2,
	RSA_PSS_SALTLEN_MAX_SIGN: -2,
	RSA_PSS_SALTLEN_MAX: -3,

	// EC point conversion forms (openssl/ec.h)
	POINT_CONVERSION_COMPRESSED: 2,
	POINT_CONVERSION_UNCOMPRESSED: 4,
	POINT_CONVERSION_HYBRID: 6,

	// DH check result bits (openssl/dh.h)
	DH_CHECK_P_NOT_SAFE_PRIME: 2,
	DH_CHECK_P_NOT_PRIME: 1,
	DH_UNABLE_TO_CHECK_GENERATOR: 4,
	DH_NOT_SUITABLE_GENERATOR: 8,

	ENGINE_METHOD_ALL: 0xffff,
};

export default {
	zlib,
	crypto,
};
