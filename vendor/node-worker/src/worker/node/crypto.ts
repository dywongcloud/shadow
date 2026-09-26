// @ts-ignore
import crypto from "node-core:crypto";

// NOT YET IMPLEMENTED:
//   - Diffie-Hellman / ECDH key agreement: createDiffieHellman,
//     createDiffieHellmanGroup, getDiffieHellman, createECDH, diffieHellman(),
//     and generateKeyPair('dh')
//   - DSA key generation: generateKeyPair('dsa')
//   - RSA encryption helpers: publicEncrypt, publicDecrypt, privateEncrypt, privateDecrypt
//   - JWK key import/export: createPublicKey/createPrivateKey({ format: 'jwk' }),
//     KeyObject#export({ format: 'jwk' }), and raw Ed/X key import (initEDRaw)
//   - Prime helpers: generatePrime(Sync), checkPrime(Sync)
//   - SPKAC: crypto.Certificate (verifySpkac / exportPublicKey / exportChallenge)
//   - X509Certificate extras: toLegacyObject(), issuerCertificate (chain),
//     checkPrivateKey(), keyUsage (returns undefined)
//   - Post-quantum & misc algorithms (disabled at the binding level): ML-KEM,
//     ML-DSA, SLH-DSA, Argon2, KMAC, KEM encapsulate/decapsulate, TurboSHAKE,
//     KangarooTwelve
//   - KeyObject <-> WebCrypto CryptoKey interop (subtle is native and can't
//     import/export node KeyObjects)
//
// Implementation note: HMAC currently uses OpenSSL's deprecated-but-functional
// HMAC_CTX API; it could move to EVP_MAC later.
export default crypto as typeof import("node:crypto");
