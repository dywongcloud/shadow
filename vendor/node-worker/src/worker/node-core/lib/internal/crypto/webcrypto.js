'use strict';

// Upstream `internal/crypto/webcrypto` reimplements the whole
// WebCrypto SubtleCrypto API on top of `internalBinding('crypto')` async jobs.
// We don't need that: the worker already exposes a fast, audited native
// SubtleCrypto via `globalThis.crypto`. `crypto.webcrypto`/`crypto.subtle` (and
// `crypto.getRandomValues`) forward here; `internal/crypto/webidl` imports
// `CryptoKey` from this module.
//
// Loaded lazily (crypto.js only requires it on first webcrypto/subtle access),
// so keeping it native-backed also keeps the async job-class web crypto tree
// (aes/ec/rsa/cfrg/mac/webidl) out of the eager dependency graph.

const crypto = globalThis.crypto;
const CryptoKey = globalThis.CryptoKey;
const SubtleCrypto = globalThis.SubtleCrypto;
const Crypto = globalThis.Crypto;

export { crypto, CryptoKey, SubtleCrypto, Crypto };

export default { crypto, CryptoKey, SubtleCrypto, Crypto };
