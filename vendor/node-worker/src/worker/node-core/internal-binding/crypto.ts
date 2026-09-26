// JS-side adapter for `internalBinding('crypto')`, backed by OpenSSL libcrypto
// compiled to wasm (src/worker/node-wasm/crypto-shim.c). Mirrors the buffer
// plumbing of internal-binding/zlib.ts: stage inputs into wasm memory, call the
// C shim, copy outputs back into fresh Buffers, re-fetching memory.buffer after
// every call since the heap can grow.
//
// The real upstream `node_core/lib/crypto.js` + `internal/crypto/*.js` run on
// top of this, so error/option/KeyObject semantics match node. Members left
// `undefined` are feature flags the JS layer guards on (Argon2Job, KmacJob,
// EVP_PKEY_ML_*, ...), which cleanly disables those algorithms.

import { getExports, registerEnv } from "../../node-wasm/loader";
// @ts-ignore — relative reach into the runtime buffer shim (Buffer isn't a
// global in the worker; crypto digests must return real node Buffers).
import nodeBuffer from "../../node/buffer";

const Buffer = nodeBuffer.Buffer;
const textDecoder = new TextDecoder();

// RNG entropy source for libcrypto's custom RAND_METHOD. Registered before the
// wasm is instantiated (this module loads at startup via internal-binding/index
// .ts), matching how http_parser registers its callbacks.
registerEnv({
	js_crypto_random(ptr: number, len: number): number {
		const wasm = getExports();
		const view = new Uint8Array(wasm.memory.buffer, ptr, len);
		// getRandomValues caps at 65536 bytes per call.
		for (let off = 0; off < len; off += 65536) {
			globalThis.crypto.getRandomValues(view.subarray(off, off + 65536));
		}
		return 0;
	},
});

let initialized = false;
function wasm() {
	const exports = getExports();
	if (!initialized) {
		exports.crypto_init();
		initialized = true;
	}
	return exports;
}

// --- memory plumbing ------------------------------------------------------

function withBytesIn<T>(bytes: Uint8Array, fn: (ptr: number, len: number) => T): T {
	const w = getExports();
	const len = bytes.length;
	const ptr = w.malloc(len || 1);
	if (!ptr) throw new Error("crypto: out of wasm memory");
	if (len) new Uint8Array(w.memory.buffer, ptr, len).set(bytes);
	try {
		return fn(ptr, len);
	} finally {
		w.free(ptr);
	}
}

function withCStr<T>(s: string, fn: (ptr: number) => T): T {
	return withBytesIn(new Uint8Array(Buffer.from(s + "\0", "utf8")), (ptr) => fn(ptr));
}

function readOut(ptr: number, len: number): Buffer {
	const w = getExports();
	return Buffer.from(new Uint8Array(w.memory.buffer, ptr, len));
}

function lastError(fallback: string): Error {
	const w = getExports();
	const cap = 256;
	const ptr = w.malloc(cap);
	let msg = fallback;
	try {
		const n = w.crypto_last_error(ptr, cap);
		if (n > 0) msg = textDecoder.decode(new Uint8Array(w.memory.buffer, ptr, n));
	} finally {
		w.free(ptr);
	}
	return new Error(msg);
}

// Coerce node's update(data, encoding) inputs to bytes.
function dataToU8(data: any, encoding?: string): Uint8Array {
	if (typeof data === "string") {
		const enc = !encoding || encoding === "buffer" ? "utf8" : encoding;
		return new Uint8Array(Buffer.from(data, enc as BufferEncoding));
	}
	if (ArrayBuffer.isView(data)) {
		return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
	}
	if (data instanceof ArrayBuffer) {
		return new Uint8Array(data);
	}
	throw new TypeError("crypto: expected string, ArrayBuffer, or ArrayBufferView");
}

function bufView(buf: any): Uint8Array {
	if (buf instanceof Uint8Array) return buf;
	if (ArrayBuffer.isView(buf)) {
		return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
	}
	if (buf instanceof ArrayBuffer) return new Uint8Array(buf);
	throw new TypeError("crypto: expected a buffer");
}

function notImplemented(name: string): never {
	throw new Error(`node:crypto: ${name} is not yet implemented in this runtime`);
}
function stubClass(name: string): any {
	return class {
		constructor() {
			notImplemented(name);
		}
	};
}

// --- digest (Hash) --------------------------------------------------------

class Hash {
	#handle = 0;
	#xof = 0;

	constructor(algorithm: any, xofLen?: number, _algorithmId?: number, _cache?: any) {
		const w = wasm();
		if (algorithm instanceof Hash) {
			this.#xof = algorithm.#xof;
			this.#handle = w.crypto_md_copy(algorithm.#handle);
			if (!this.#handle) throw lastError("hash copy failed");
			return;
		}
		this.#xof = (xofLen ?? 0) >>> 0;
		const xof = this.#xof;
		this.#handle = withCStr(algorithm, (p) => w.crypto_md_new(p, xof));
		if (!this.#handle) throw new Error(`Digest method not supported: ${algorithm}`);
	}

	update(data: any, encoding?: string): boolean {
		const w = wasm();
		const rc = withBytesIn(dataToU8(data, encoding), (p, n) =>
			w.crypto_md_update(this.#handle, p, n)
		);
		return rc === 0;
	}

	digest(encoding?: string): Buffer | string {
		const w = wasm();
		const cap = Math.max(this.#xof || 0, 64);
		const outPtr = w.malloc(cap);
		try {
			const n = w.crypto_md_final(this.#handle, outPtr, cap);
			if (n < 0) throw lastError("digest failed");
			const buf = readOut(outPtr, n);
			return encoding && encoding !== "buffer"
				? buf.toString(encoding as BufferEncoding)
				: buf;
		} finally {
			w.free(outPtr);
			w.crypto_md_free(this.#handle);
			this.#handle = 0;
		}
	}
}

// --- HMAC -----------------------------------------------------------------

class Hmac {
	#handle = 0;

	init(algorithm: string, key: any): void {
		const w = wasm();
		const keyBytes =
			key instanceof KeyObjectHandle ? key.secretBytes() : dataToU8(key);
		this.#handle = withCStr(algorithm, (mp) =>
			withBytesIn(keyBytes, (kp, kl) => w.crypto_hmac_new(mp, kp, kl))
		);
		if (!this.#handle) throw new Error(`Unknown message digest: ${algorithm}`);
	}

	update(data: any, encoding?: string): boolean {
		const w = wasm();
		const rc = withBytesIn(dataToU8(data, encoding), (p, n) =>
			w.crypto_hmac_update(this.#handle, p, n)
		);
		return rc === 0;
	}

	digest(encoding?: string): Buffer | string {
		const w = wasm();
		const cap = 64; // EVP_MAX_MD_SIZE
		const outPtr = w.malloc(cap);
		try {
			const n = w.crypto_hmac_final(this.#handle, outPtr, cap);
			if (n < 0) throw lastError("hmac digest failed");
			const buf = readOut(outPtr, n);
			return encoding && encoding !== "buffer"
				? buf.toString(encoding as BufferEncoding)
				: buf;
		} finally {
			w.free(outPtr);
			w.crypto_hmac_free(this.#handle);
			this.#handle = 0;
		}
	}
}

function oneShotDigest(
	algorithm: string,
	_hashId: number,
	_cache: any,
	input: any,
	normalizedEncoding: string,
	_encId: number,
	outputLength?: number
): Buffer | string {
	const h = new Hash(algorithm, outputLength);
	h.update(input);
	return h.digest(
		normalizedEncoding === "buffer" ? undefined : normalizedEncoding
	);
}

// --- key objects ----------------------------------------------------------

const kKeyTypeSecret = 0;
const kKeyTypePublic = 1;
const kKeyTypePrivate = 2;
const kKeyFormatDER = 0;
const kKeyFormatPEM = 1;
const kKeyFormatJWK = 2;
const kKeyEncodingPKCS1 = 0;
const kKeyEncodingPKCS8 = 1;
const kKeyEncodingSPKI = 2;
const kKeyEncodingSEC1 = 3;
const kSignJobModeSign = 0;
const kSignJobModeVerify = 1;
// Sentinel for "PSS salt length not provided" (real values include -1/-2/-3);
// must match SALTLEN_UNSET in crypto-shim.c.
const SALTLEN_UNSET = 0x7fffffff;

// Asymmetric EVP_PKEY handles are GC-managed (like node's native KeyObject).
const pkeyRegistry = new FinalizationRegistry((ptr: number) => {
	getExports().crypto_pkey_free(ptr);
});

class KeyObjectHandle {
	#secret: Uint8Array | null = null;
	#pkey = 0; // EVP_PKEY* for asymmetric keys
	#keyType: string = ""; // 'secret' | 'public' | 'private'

	init(type: number, ...args: any[]): any {
		if (type === kKeyTypeSecret) {
			this.#secret = dataToU8(args[0]);
			this.#keyType = "secret";
			return undefined;
		}
		// asymmetric: args = [data, format, encType, passphrase]
		const [data, format, encType, passphrase] = args;
		const w = wasm();
		const fmt = format === kKeyFormatDER ? 0 : 1; // JWK goes through initJwk
		const enc =
			typeof encType === "number"
				? encType
				: type === kKeyTypePublic
					? kKeyEncodingSPKI
					: kKeyEncodingPKCS8;
		const keyTypeC = type === kKeyTypePublic ? 1 : 2;
		const bytes = dataToU8(data);
		const passBytes = passphrase ? dataToU8(passphrase) : null;
		this.#pkey = withBytesIn(bytes, (dp, dl) =>
			passBytes
				? withBytesIn(passBytes, (pp, pl) =>
						w.crypto_pkey_parse(keyTypeC, dp, dl, fmt, enc, pp, pl)
					)
				: w.crypto_pkey_parse(keyTypeC, dp, dl, fmt, enc, 0, 0)
		);
		if (!this.#pkey) {
			throw lastError(
				`Failed to read ${type === kKeyTypePublic ? "public" : "private"} key`
			);
		}
		this.#keyType = type === kKeyTypePublic ? "public" : "private";
		pkeyRegistry.register(this, this.#pkey);
		return undefined;
	}

	// internal: adopt an already-generated EVP_PKEY (from a keygen job).
	adoptPkey(ptr: number, keyType: string): void {
		this.#pkey = ptr;
		this.#keyType = keyType;
		pkeyRegistry.register(this, ptr);
	}
	pkeyPtr(): number {
		return this.#pkey;
	}

	secretBytes(): Uint8Array {
		if (!this.#secret) notImplemented("KeyObjectHandle: non-secret key access");
		return this.#secret;
	}

	getSymmetricKeySize(): number {
		return this.#secret ? this.#secret.length : 0;
	}

	export(...args: any[]): any {
		if (this.#secret) return Buffer.from(this.#secret);
		const [format, type, cipher, passphrase] = args;
		const w = wasm();
		const keyTypeC = this.#keyType === "public" ? 1 : 2;
		const fmt = format === kKeyFormatDER ? 0 : 1;
		const enc = typeof type === "number" ? type : keyTypeC === 1 ? kKeyEncodingSPKI : kKeyEncodingPKCS8;
		const cipherName = cipher || "";
		const passBytes = passphrase ? dataToU8(passphrase) : null;
		const cap = 16384;
		const outPtr = w.malloc(cap);
		try {
			const n = withCStr(cipherName, (cp) =>
				passBytes
					? withBytesIn(passBytes, (pp, pl) =>
							w.crypto_pkey_export(this.#pkey, keyTypeC, fmt, enc, cp, pp, pl, outPtr, cap)
						)
					: w.crypto_pkey_export(this.#pkey, keyTypeC, fmt, enc, cp, 0, 0, outPtr, cap)
			);
			if (n < 0) throw lastError("key export failed");
			const buf = readOut(outPtr, n);
			return fmt === 1 ? buf.toString("utf8") : buf;
		} finally {
			w.free(outPtr);
		}
	}

	getAsymmetricKeyType(): string {
		const w = wasm();
		const cap = 32;
		const outPtr = w.malloc(cap);
		try {
			const n = w.crypto_pkey_type(this.#pkey, outPtr, cap);
			return textDecoder.decode(new Uint8Array(w.memory.buffer, outPtr, n));
		} finally {
			w.free(outPtr);
		}
	}

	keyDetail(): any {
		const w = wasm();
		const cap = 256;
		const outPtr = w.malloc(cap);
		try {
			const n = w.crypto_pkey_detail(this.#pkey, outPtr, cap);
			if (n < 0) return {};
			const obj = JSON.parse(
				textDecoder.decode(new Uint8Array(w.memory.buffer, outPtr, n))
			);
			// node's normalizeKeyDetails expects publicExponent as the raw
			// big-endian bytes (it does `new Uint8Array(details.publicExponent)`).
			if (obj.publicExponentHex !== undefined) {
				let hex = obj.publicExponentHex;
				if (hex.length % 2) hex = "0" + hex;
				const bytes = new Uint8Array(hex.length / 2);
				for (let i = 0; i < bytes.length; i++) {
					bytes[i] = parseInt(hex.substr(i * 2, 2), 16);
				}
				obj.publicExponent = bytes.buffer;
				delete obj.publicExponentHex;
			}
			return obj;
		} finally {
			w.free(outPtr);
		}
	}

	equals(other: KeyObjectHandle): boolean {
		if (this.#secret && other.#secret) {
			return (
				this.#secret.length === other.#secret.length &&
				timingSafeEqual(this.#secret, other.#secret)
			);
		}
		if (this.#pkey && other.#pkey) {
			try {
				const a = this.export(kKeyFormatDER, kKeyEncodingSPKI);
				const b = other.export(kKeyFormatDER, kKeyEncodingSPKI);
				return a.length === b.length && timingSafeEqual(a, b);
			} catch {
				return false;
			}
		}
		return false;
	}

	initJwk(): any {
		notImplemented("KeyObjectHandle.initJwk (JWK key import)");
	}
	initEDRaw(): any {
		notImplemented("KeyObjectHandle.initEDRaw");
	}
	exportJwk(): any {
		notImplemented("KeyObjectHandle.exportJwk (JWK key export)");
	}
}

const kNativeHandle = Symbol("nativeKeyHandle");
function createNativeKeyObjectClass(cb: (base: any) => any): any {
	class NativeKeyObject {
		constructor(handle: any) {
			(this as any)[kNativeHandle] = handle;
		}
	}
	return cb(NativeKeyObject);
}

// --- async/sync job base --------------------------------------------------

const kCryptoJobAsync = 0;
const kCryptoJobSync = 1;

abstract class CryptoJob {
	ondone?: (...args: any[]) => void;
	#mode: number;
	constructor(mode: number) {
		this.#mode = mode;
	}
	protected abstract exec(): any[];
	run(): any[] | undefined {
		if (this.#mode === kCryptoJobSync) {
			return this.exec();
		}
		queueMicrotask(() => {
			const result = this.exec();
			this.ondone?.(result[0], result[1]);
		});
		return undefined;
	}
}

class RandomBytesJob extends CryptoJob {
	#buf: any;
	#offset: number;
	#size: number;
	constructor(mode: number, buf: any, offset: number, size: number) {
		super(mode);
		this.#buf = buf;
		this.#offset = offset;
		this.#size = size;
	}
	protected exec(): any[] {
		const w = wasm();
		const ptr = w.malloc(this.#size || 1);
		try {
			const rc = w.crypto_rand_bytes(ptr, this.#size);
			if (rc !== 0) return [lastError("randomBytes failed")];
			const src = new Uint8Array(w.memory.buffer, ptr, this.#size);
			bufView(this.#buf).set(src, this.#offset);
			return [null];
		} finally {
			w.free(ptr);
		}
	}
}

// Copy `len` bytes from wasm memory at `ptr` into a fresh, exactly-sized
// ArrayBuffer (the node KDF/job APIs return ArrayBuffers). Re-fetches
// memory.buffer since scrypt can grow the heap.
function copyToArrayBuffer(ptr: number, len: number): ArrayBuffer {
	const w = getExports();
	return new Uint8Array(new Uint8Array(w.memory.buffer, ptr, len)).buffer;
}

class PBKDF2Job extends CryptoJob {
	#password: any;
	#salt: any;
	#iterations: number;
	#keylen: number;
	#digest: string;
	constructor(mode: number, password: any, salt: any, iterations: number, keylen: number, digest: string) {
		super(mode);
		this.#password = password;
		this.#salt = salt;
		this.#iterations = iterations;
		this.#keylen = keylen;
		this.#digest = digest;
	}
	protected exec(): any[] {
		if (this.#keylen === 0) return [undefined, new ArrayBuffer(0)];
		const w = wasm();
		return withBytesIn(dataToU8(this.#password), (pp, pl) =>
			withBytesIn(dataToU8(this.#salt), (sp, sl) =>
				withCStr(this.#digest, (np) => {
					const outPtr = w.malloc(this.#keylen);
					try {
						const rc = w.crypto_pbkdf2(pp, pl, sp, sl, this.#iterations, np, outPtr, this.#keylen);
						if (rc !== 0) return [lastError("pbkdf2 failed")];
						return [undefined, copyToArrayBuffer(outPtr, this.#keylen)];
					} finally {
						w.free(outPtr);
					}
				})
			)
		);
	}
}

class ScryptJob extends CryptoJob {
	#password: any;
	#salt: any;
	#N: number;
	#r: number;
	#p: number;
	#maxmem: number;
	#keylen: number;
	constructor(mode: number, password: any, salt: any, N: number, r: number, p: number, maxmem: number, keylen: number) {
		super(mode);
		this.#password = password;
		this.#salt = salt;
		this.#N = N;
		this.#r = r;
		this.#p = p;
		this.#maxmem = maxmem;
		this.#keylen = keylen;
	}
	protected exec(): any[] {
		if (this.#keylen === 0) return [undefined, new ArrayBuffer(0)];
		const w = wasm();
		return withBytesIn(dataToU8(this.#password), (pp, pl) =>
			withBytesIn(dataToU8(this.#salt), (sp, sl) => {
				const outPtr = w.malloc(this.#keylen);
				try {
					const rc = w.crypto_scrypt(pp, pl, sp, sl, this.#N, this.#r, this.#p, this.#maxmem, outPtr, this.#keylen);
					if (rc !== 0) return [lastError("scrypt failed")];
					return [undefined, copyToArrayBuffer(outPtr, this.#keylen)];
				} finally {
					w.free(outPtr);
				}
			})
		);
	}
}

class HKDFJob extends CryptoJob {
	#hash: string;
	#key: any;
	#salt: any;
	#info: any;
	#length: number;
	constructor(mode: number, hash: string, key: any, salt: any, info: any, length: number) {
		super(mode);
		this.#hash = hash;
		this.#key = key;
		this.#salt = salt;
		this.#info = info;
		this.#length = length;
	}
	protected exec(): any[] {
		if (this.#length === 0) return [undefined, new ArrayBuffer(0)];
		const w = wasm();
		// key is a KeyObject (createSecretKey); reach its handle via the
		// NativeKeyObject base symbol our createNativeKeyObjectClass sets.
		const handle =
			this.#key instanceof KeyObjectHandle ? this.#key : this.#key[kNativeHandle];
		const ikm = handle.secretBytes();
		return withCStr(this.#hash, (np) =>
			withBytesIn(ikm, (kp, kl) =>
				withBytesIn(dataToU8(this.#salt), (sp, sl) =>
					withBytesIn(dataToU8(this.#info), (ip, il) => {
						const outPtr = w.malloc(this.#length);
						try {
							const rc = w.crypto_hkdf(np, kp, kl, sp, sl, ip, il, outPtr, this.#length);
							if (rc !== 0) return [lastError("hkdf failed")];
							return [undefined, copyToArrayBuffer(outPtr, this.#length)];
						} finally {
							w.free(outPtr);
						}
					})
				)
			)
		);
	}
}

// --- symmetric ciphers ----------------------------------------------------

// node's CipherBase has no explicit close; the native handle is GC-managed.
// Mirror that with a FinalizationRegistry that frees the wasm cipher handle.
const cipherRegistry = new FinalizationRegistry((handle: number) => {
	getExports().crypto_cipher_free(handle);
});

const EVP_MODE_NAMES: Record<number, string> = {
	0x1: "ecb",
	0x2: "cbc",
	0x3: "cfb",
	0x4: "ofb",
	0x5: "ctr",
	0x6: "gcm",
	0x7: "ccm",
	0x10003: "ocb",
	0x10001: "xts",
	0x10002: "wrap",
};

class CipherBase {
	#handle = 0;

	constructor(isEncrypt: boolean, cipher: string, credential: any, iv: any, authTagLength: number) {
		const w = wasm();
		const key =
			credential instanceof KeyObjectHandle ? credential.secretBytes() : dataToU8(credential);
		const ivU8 = iv == null ? null : dataToU8(iv);
		const enc = isEncrypt ? 1 : 0;
		const atl = typeof authTagLength === "number" ? authTagLength : -1;
		this.#handle = withCStr(cipher, (cp) =>
			withBytesIn(key, (kp, kl) => {
				if (ivU8) {
					return withBytesIn(ivU8, (ip, il) =>
						w.crypto_cipher_new(enc, cp, kp, kl, ip, il, atl)
					);
				}
				return w.crypto_cipher_new(enc, cp, kp, kl, 0, 0, atl);
			})
		);
		if (!this.#handle) throw new Error(`Unknown cipher or invalid key/iv length: ${cipher}`);
		cipherRegistry.register(this, this.#handle);
	}

	update(data: any, inputEncoding?: string): Buffer {
		const w = wasm();
		const bytes = dataToU8(data, inputEncoding);
		const cap = bytes.length + 16; // + max block size
		return withBytesIn(bytes, (ip, il) => {
			const outPtr = w.malloc(cap);
			try {
				const n = w.crypto_cipher_update(this.#handle, ip, il, outPtr, cap);
				if (n < 0) throw lastError("Cipher update failed");
				return readOut(outPtr, n);
			} finally {
				w.free(outPtr);
			}
		});
	}

	final(): Buffer {
		const w = wasm();
		const cap = 32;
		const outPtr = w.malloc(cap);
		try {
			const n = w.crypto_cipher_final(this.#handle, outPtr, cap);
			if (n < 0) throw lastError("Unsupported state or unable to authenticate data");
			return readOut(outPtr, n);
		} finally {
			w.free(outPtr);
		}
	}

	setAutoPadding(ap: boolean): boolean {
		return wasm().crypto_cipher_set_auto_padding(this.#handle, ap ? 1 : 0) === 0;
	}

	getAuthTag(): Buffer | undefined {
		const w = wasm();
		const cap = 16;
		const outPtr = w.malloc(cap);
		try {
			const n = w.crypto_cipher_get_auth_tag(this.#handle, outPtr, cap);
			return n < 0 ? undefined : readOut(outPtr, n);
		} finally {
			w.free(outPtr);
		}
	}

	setAuthTag(tagbuf: any): boolean {
		const w = wasm();
		return (
			withBytesIn(dataToU8(tagbuf), (tp, tl) =>
				w.crypto_cipher_set_auth_tag(this.#handle, tp, tl)
			) === 0
		);
	}

	setAAD(aadbuf: any, plaintextLength: number): boolean {
		const w = wasm();
		const pl = typeof plaintextLength === "number" ? plaintextLength : -1;
		return (
			withBytesIn(dataToU8(aadbuf), (ap, al) =>
				w.crypto_cipher_set_aad(this.#handle, ap, al, pl)
			) === 0
		);
	}
}

function getCipherInfo(nameOrNid: any, _keyLength?: number, _ivLength?: number): any {
	if (typeof nameOrNid !== "string") return undefined; // nid lookup unsupported
	const w = wasm();
	const outPtr = w.malloc(5 * 4);
	const nameCap = 64;
	const namePtr = w.malloc(nameCap);
	try {
		const rc = withCStr(nameOrNid, (np) =>
			w.crypto_cipher_info(np, outPtr, namePtr, nameCap)
		);
		if (rc !== 0) return undefined;
		const ints = new Int32Array(w.memory.buffer, outPtr, 5);
		const nameBytes = new Uint8Array(w.memory.buffer, namePtr, nameCap);
		let end = 0;
		while (end < nameCap && nameBytes[end] !== 0) end++;
		return {
			name: textDecoder.decode(nameBytes.subarray(0, end)),
			nid: ints[0],
			blockSize: ints[1],
			ivLength: ints[2],
			keyLength: ints[3],
			mode: EVP_MODE_NAMES[ints[4]] ?? "stream",
		};
	} finally {
		w.free(outPtr);
		w.free(namePtr);
	}
}

// --- asymmetric sign / verify / keygen ------------------------------------

function concatU8(chunks: Uint8Array[]): Uint8Array {
	let total = 0;
	for (const c of chunks) total += c.length;
	const all = new Uint8Array(total);
	let off = 0;
	for (const c of chunks) {
		all.set(c, off);
		off += c.length;
	}
	return all;
}

// Resolve a key argument (KeyObjectHandle or raw data+encoding) to an EVP_PKEY
// pointer. `owned` means the caller must free it after use.
function resolveKey(
	data: any,
	format: number,
	type: number,
	passphrase: any,
	keyTypeC: number
): { ptr: number; owned: boolean } {
	if (data instanceof KeyObjectHandle) return { ptr: data.pkeyPtr(), owned: false };
	const w = wasm();
	const fmt = format === kKeyFormatDER ? 0 : 1;
	const enc =
		typeof type === "number" ? type : keyTypeC === 1 ? kKeyEncodingSPKI : kKeyEncodingPKCS8;
	const passBytes = passphrase ? dataToU8(passphrase) : null;
	const ptr = withBytesIn(dataToU8(data), (dp, dl) =>
		passBytes
			? withBytesIn(passBytes, (pp, pl) => w.crypto_pkey_parse(keyTypeC, dp, dl, fmt, enc, pp, pl))
			: w.crypto_pkey_parse(keyTypeC, dp, dl, fmt, enc, 0, 0)
	);
	if (!ptr) throw lastError("Failed to parse key");
	return { ptr, owned: true };
}

function signWith(
	ptr: number,
	md: string,
	data: Uint8Array,
	rsaPadding: any,
	pssSaltLen: any,
	dsaSigEnc: any
): Buffer {
	const w = wasm();
	const rp = typeof rsaPadding === "number" ? rsaPadding : -1;
	const sl = typeof pssSaltLen === "number" ? pssSaltLen : SALTLEN_UNSET;
	const de = typeof dsaSigEnc === "number" ? dsaSigEnc : 0;
	const cap = 4096; // covers RSA up to 16384-bit
	const outPtr = w.malloc(cap);
	try {
		const n = withCStr(md, (mp) =>
			withBytesIn(data, (dp, dl) => w.crypto_pkey_sign(ptr, mp, dp, dl, rp, sl, de, outPtr, cap))
		);
		if (n < 0) throw lastError("sign failed");
		return readOut(outPtr, n);
	} finally {
		w.free(outPtr);
	}
}

function verifyWith(
	ptr: number,
	md: string,
	data: Uint8Array,
	sig: Uint8Array,
	rsaPadding: any,
	pssSaltLen: any,
	dsaSigEnc: any
): boolean {
	const w = wasm();
	const rp = typeof rsaPadding === "number" ? rsaPadding : -1;
	const sl = typeof pssSaltLen === "number" ? pssSaltLen : SALTLEN_UNSET;
	const de = typeof dsaSigEnc === "number" ? dsaSigEnc : 0;
	const r = withCStr(md, (mp) =>
		withBytesIn(data, (dp, dl) =>
			withBytesIn(sig, (sp, sl2) =>
				w.crypto_pkey_verify(ptr, mp, dp, dl, sp, sl2, rp, sl, de)
			)
		)
	);
	if (r < 0) throw lastError("verify failed");
	return r === 1;
}

class Sign {
	#md = "";
	#chunks: Uint8Array[] = [];
	init(algorithm: string): void {
		this.#md = algorithm;
	}
	update(data: any, encoding?: string): void {
		this.#chunks.push(dataToU8(data, encoding));
	}
	sign(
		data: any,
		format: number,
		type: number,
		passphrase: any,
		rsaPadding: any,
		pssSaltLen: any,
		dsaSigEnc: any
	): Buffer {
		const w = wasm();
		const { ptr, owned } = resolveKey(data, format, type, passphrase, 2);
		try {
			return signWith(ptr, this.#md, concatU8(this.#chunks), rsaPadding, pssSaltLen, dsaSigEnc);
		} finally {
			if (owned) w.crypto_pkey_free(ptr);
		}
	}
}

class Verify {
	#md = "";
	#chunks: Uint8Array[] = [];
	init(algorithm: string): void {
		this.#md = algorithm;
	}
	update(data: any, encoding?: string): void {
		this.#chunks.push(dataToU8(data, encoding));
	}
	verify(
		data: any,
		format: number,
		type: number,
		passphrase: any,
		signature: any,
		rsaPadding: any,
		pssSaltLen: any,
		dsaSigEnc: any
	): boolean {
		const w = wasm();
		const { ptr, owned } = resolveKey(data, format, type, passphrase, 1);
		try {
			return verifyWith(
				ptr,
				this.#md,
				concatU8(this.#chunks),
				dataToU8(signature),
				rsaPadding,
				pssSaltLen,
				dsaSigEnc
			);
		} finally {
			if (owned) w.crypto_pkey_free(ptr);
		}
	}
}

class SignJob extends CryptoJob {
	#jobMode: number;
	#keyData: any;
	#keyFormat: number;
	#keyType: number;
	#keyPass: any;
	#data: any;
	#algorithm: any;
	#pssSaltLen: any;
	#rsaPadding: any;
	#dsaSigEnc: any;
	#signature: any;
	constructor(
		mode: number,
		jobMode: number,
		keyData: any,
		keyFormat: number,
		keyType: number,
		keyPass: any,
		data: any,
		algorithm: any,
		pssSaltLen: any,
		rsaPadding: any,
		dsaSigEnc: any,
		_context: any,
		signature: any
	) {
		super(mode);
		this.#jobMode = jobMode;
		this.#keyData = keyData;
		this.#keyFormat = keyFormat;
		this.#keyType = keyType;
		this.#keyPass = keyPass;
		this.#data = data;
		this.#algorithm = algorithm;
		this.#pssSaltLen = pssSaltLen;
		this.#rsaPadding = rsaPadding;
		this.#dsaSigEnc = dsaSigEnc;
		this.#signature = signature;
	}
	protected exec(): any[] {
		const w = wasm();
		const isSign = this.#jobMode === kSignJobModeSign;
		const { ptr, owned } = resolveKey(
			this.#keyData,
			this.#keyFormat,
			this.#keyType,
			this.#keyPass,
			isSign ? 2 : 1
		);
		try {
			const md = this.#algorithm || ""; // null => raw (Ed25519)
			const data = dataToU8(this.#data);
			if (isSign) {
				const sig = signWith(ptr, md, data, this.#rsaPadding, this.#pssSaltLen, this.#dsaSigEnc);
				return [undefined, new Uint8Array(sig).buffer];
			}
			const ok = verifyWith(
				ptr,
				md,
				data,
				dataToU8(this.#signature),
				this.#rsaPadding,
				this.#pssSaltLen,
				this.#dsaSigEnc
			);
			return [undefined, ok];
		} catch (e) {
			return [e];
		} finally {
			if (owned) w.crypto_pkey_free(ptr);
		}
	}
}

// Produce one keygen output: a KeyObjectHandle (when no encoding chosen) or an
// encoded PEM string / DER Buffer. `ownsRef` is true for exactly one caller
// (the private side), which consumes/frees the original EVP_PKEY reference.
function makeKeyOutput(
	pkeyPtr: number,
	keyTypeStr: string,
	format: number | undefined,
	encType: number | undefined,
	cipher: any,
	passphrase: any,
	ownsRef: boolean
): any {
	const w = wasm();
	if (format === undefined || format === null) {
		const h = new KeyObjectHandle();
		h.adoptPkey(ownsRef ? pkeyPtr : w.crypto_pkey_up_ref(pkeyPtr), keyTypeStr);
		return h;
	}
	const keyTypeC = keyTypeStr === "public" ? 1 : 2;
	const fmt = format === kKeyFormatDER ? 0 : 1;
	const enc = encType as number;
	const cipherName = cipher || "";
	const passBytes = passphrase ? dataToU8(passphrase) : null;
	const cap = 16384;
	const outPtr = w.malloc(cap);
	try {
		const n = withCStr(cipherName, (cp) =>
			passBytes
				? withBytesIn(passBytes, (pp, pl) =>
						w.crypto_pkey_export(pkeyPtr, keyTypeC, fmt, enc, cp, pp, pl, outPtr, cap)
					)
				: w.crypto_pkey_export(pkeyPtr, keyTypeC, fmt, enc, cp, 0, 0, outPtr, cap)
		);
		if (n < 0) throw lastError("key export failed");
		const buf = readOut(outPtr, n);
		return fmt === 1 ? buf.toString("utf8") : buf;
	} finally {
		w.free(outPtr);
		if (ownsRef) w.crypto_pkey_free(pkeyPtr);
	}
}

class KeyPairGenJob extends CryptoJob {
	#generate: () => number;
	#encoding: any[];
	constructor(mode: number, generate: () => number, encoding: any[]) {
		super(mode);
		this.#generate = generate;
		this.#encoding = encoding;
	}
	protected exec(): any[] {
		const pkey = this.#generate();
		if (!pkey) return [lastError("key generation failed")];
		const [pubFmt, pubType, privFmt, privType, cipher, passphrase] = this.#encoding;
		try {
			const pub = makeKeyOutput(pkey, "public", pubFmt, pubType, null, null, false);
			const priv = makeKeyOutput(pkey, "private", privFmt, privType, cipher, passphrase, true);
			return [undefined, [pub, priv]];
		} catch (e) {
			return [e];
		}
	}
}

class RsaKeyPairGenJob extends KeyPairGenJob {
	constructor(mode: number, _variant: number, modulusLength: number, publicExponent: number, ...rest: any[]) {
		// rest tail is the 6-element encoding tuple (rsa-pss prepends hash opts).
		const encoding = rest.slice(rest.length - 6);
		super(mode, () => wasm().crypto_generate_rsa(modulusLength, publicExponent >>> 0), encoding);
	}
}

class EcKeyPairGenJob extends KeyPairGenJob {
	constructor(mode: number, namedCurve: string, _paramEncoding: number, ...encoding: any[]) {
		super(mode, () => withCStr(namedCurve, (cp) => wasm().crypto_generate_ec(cp)), encoding);
	}
}

class NidKeyPairGenJob extends KeyPairGenJob {
	constructor(mode: number, nid: number, ...encoding: any[]) {
		super(mode, () => wasm().crypto_generate_ed(nid), encoding);
	}
}

class SecretKeyGenJob extends CryptoJob {
	#length: number;
	constructor(mode: number, length: number) {
		super(mode);
		this.#length = length;
	}
	protected exec(): any[] {
		const w = wasm();
		const nbytes = Math.ceil(this.#length / 8);
		const ptr = w.malloc(nbytes || 1);
		try {
			const rc = w.crypto_generate_secret(ptr, nbytes);
			if (rc !== 0) return [lastError("secret key generation failed")];
			const bytes = new Uint8Array(new Uint8Array(w.memory.buffer, ptr, nbytes));
			const h = new KeyObjectHandle();
			h.init(kKeyTypeSecret, bytes);
			return [undefined, h];
		} finally {
			w.free(ptr);
		}
	}
}

// --- X.509 certificates ---------------------------------------------------

const x509Registry = new FinalizationRegistry((ptr: number) => {
	getExports().crypto_x509_free(ptr);
});

// Read a string result, growing the buffer on -needed. n===0 -> absent
// (undefined); n===-1 -> hard error (also undefined / treated as absent).
function x509Str(call: (outPtr: number, cap: number) => number): string | undefined {
	const w = wasm();
	let cap = 1024;
	for (;;) {
		const outPtr = w.malloc(cap);
		let n: number;
		try {
			n = call(outPtr, cap);
			if (n > 0) return textDecoder.decode(new Uint8Array(w.memory.buffer, outPtr, n));
			if (n === 0 || n === -1) return undefined;
		} finally {
			w.free(outPtr);
		}
		cap = -n + 64;
	}
}

class X509Handle {
	#ptr: number;
	constructor(ptr: number) {
		this.#ptr = ptr;
		x509Registry.register(this, ptr);
	}
	subject() {
		return x509Str((o, c) => wasm().crypto_x509_name(this.#ptr, 0, o, c));
	}
	issuer() {
		return x509Str((o, c) => wasm().crypto_x509_name(this.#ptr, 1, o, c));
	}
	subjectAltName() {
		return x509Str((o, c) => wasm().crypto_x509_subject_alt_name(this.#ptr, o, c));
	}
	infoAccess() {
		return x509Str((o, c) => wasm().crypto_x509_info_access(this.#ptr, o, c));
	}
	validFrom() {
		return x509Str((o, c) => wasm().crypto_x509_valid(this.#ptr, 0, o, c));
	}
	validTo() {
		return x509Str((o, c) => wasm().crypto_x509_valid(this.#ptr, 1, o, c));
	}
	validFromDate() {
		const s = this.validFrom();
		return s ? new Date(s) : undefined;
	}
	validToDate() {
		const s = this.validTo();
		return s ? new Date(s) : undefined;
	}
	#fp(md: string) {
		return x509Str((o, c) => withCStr(md, (m) => wasm().crypto_x509_fingerprint(this.#ptr, m, o, c)));
	}
	fingerprint() {
		return this.#fp("sha1");
	}
	fingerprint256() {
		return this.#fp("sha256");
	}
	fingerprint512() {
		return this.#fp("sha512");
	}
	serialNumber() {
		return x509Str((o, c) => wasm().crypto_x509_serial(this.#ptr, o, c));
	}
	signatureAlgorithm() {
		return x509Str((o, c) => wasm().crypto_x509_sig_alg(this.#ptr, 0, o, c));
	}
	signatureAlgorithmOid() {
		return x509Str((o, c) => wasm().crypto_x509_sig_alg(this.#ptr, 1, o, c));
	}
	keyUsage() {
		return undefined; // not parsed; node returns undefined when the ext is absent
	}
	raw(): Buffer {
		const w = wasm();
		let cap = 4096;
		for (;;) {
			const outPtr = w.malloc(cap);
			let n: number;
			try {
				n = w.crypto_x509_raw(this.#ptr, outPtr, cap);
				if (n >= 0) return readOut(outPtr, n);
			} finally {
				w.free(outPtr);
			}
			if (n === -1) throw lastError("x509 raw failed");
			cap = -n + 64;
		}
	}
	pem() {
		return x509Str((o, c) => wasm().crypto_x509_pem(this.#ptr, o, c));
	}
	publicKey() {
		const p = wasm().crypto_x509_public_key(this.#ptr);
		if (!p) throw lastError("x509 public key extraction failed");
		const h = new KeyObjectHandle();
		h.adoptPkey(p, "public");
		return h;
	}
	checkHost(name: string, flags: number) {
		const r = withCStr(name, (np) => wasm().crypto_x509_check_host(this.#ptr, np, flags | 0));
		return r === 1 ? name : undefined;
	}
	checkEmail(email: string, flags: number) {
		const r = withCStr(email, (np) => wasm().crypto_x509_check_email(this.#ptr, np, flags | 0));
		return r === 1 ? email : undefined;
	}
	checkIP(ip: string, flags: number) {
		const r = withCStr(ip, (np) => wasm().crypto_x509_check_ip(this.#ptr, np, flags | 0));
		return r === 1 ? ip : undefined;
	}
	checkCA() {
		return wasm().crypto_x509_check_ca(this.#ptr) === 1;
	}
	verify(pkeyHandle: KeyObjectHandle) {
		return wasm().crypto_x509_verify(this.#ptr, pkeyHandle.pkeyPtr()) === 1;
	}
	checkIssued(other: X509Handle) {
		// true if `this` was issued by `other` (issuer = other, subject = this).
		return wasm().crypto_x509_check_issued(other.#ptr, this.#ptr) === 1;
	}
	getIssuerCert() {
		return undefined; // chain resolution unsupported
	}
	checkPrivateKey(_pkeyHandle: KeyObjectHandle) {
		return false; // unsupported
	}
	toLegacy() {
		notImplemented("X509Certificate.toLegacyObject");
	}
}

function parseX509(buffer: any): X509Handle {
	const w = wasm();
	const ptr = withBytesIn(dataToU8(buffer), (dp, dl) => w.crypto_x509_parse(dp, dl));
	if (!ptr) throw lastError("Failed to parse X509 certificate");
	return new X509Handle(ptr);
}

// --- capabilities ---------------------------------------------------------

function enumNames(call: (ptr: number, cap: number) => number): string[] {
	const w = wasm();
	const cap = 16384;
	const ptr = w.malloc(cap);
	try {
		const n = call(ptr, cap);
		if (n <= 0) return [];
		const s = textDecoder.decode(new Uint8Array(w.memory.buffer, ptr, n));
		// OpenSSL returns canonical names (SHA2-256, AES-128-CBC); lowercase to
		// better match node's getHashes/getCiphers output.
		return s.split("\n").filter(Boolean).map((x) => x.toLowerCase());
	} finally {
		w.free(ptr);
	}
}

function getHashes(): string[] {
	const w = wasm();
	return enumNames((p, c) => w.crypto_get_hashes(p, c));
}
function getCiphers(): string[] {
	const w = wasm();
	return enumNames((p, c) => w.crypto_get_ciphers(p, c));
}
function getCurves(): string[] {
	const w = wasm();
	return enumNames((p, c) => w.crypto_get_curves(p, c));
}

function timingSafeEqual(a: any, b: any): boolean {
	const av = bufView(a);
	const bv = bufView(b);
	if (av.length !== bv.length) {
		throw new RangeError("Input buffers must have the same byte length");
	}
	const w = wasm();
	return (
		withBytesIn(av, (ap) =>
			withBytesIn(bv, (bp) => w.crypto_timing_safe_equal(ap, bp, av.length))
		) === 1
	);
}

function secureBuffer(size: number): Buffer {
	return Buffer.alloc(size);
}

// --- binding export -------------------------------------------------------

export default {
	// CryptoJobMode
	kCryptoJobAsync,
	kCryptoJobSync,

	// hashing
	Hash,
	Hmac,
	oneShotDigest,

	// random
	RandomBytesJob,
	secureBuffer,
	RandomPrimeJob: stubClass("RandomPrimeJob"),
	CheckPrimeJob: stubClass("CheckPrimeJob"),

	// kdf
	PBKDF2Job,
	HKDFJob,
	ScryptJob,

	// keys
	KeyObjectHandle,
	createNativeKeyObjectClass,

	// keygen
	RsaKeyPairGenJob,
	EcKeyPairGenJob,
	NidKeyPairGenJob,
	SecretKeyGenJob,
	DsaKeyPairGenJob: stubClass("DsaKeyPairGenJob"),
	DhKeyPairGenJob: stubClass("DhKeyPairGenJob"),

	// cipher
	CipherBase,
	getCipherInfo,
	publicEncrypt: () => notImplemented("publicEncrypt"),
	publicDecrypt: () => notImplemented("publicDecrypt"),
	privateEncrypt: () => notImplemented("privateEncrypt"),
	privateDecrypt: () => notImplemented("privateDecrypt"),

	// sign/verify
	Sign,
	Verify,
	SignJob,
	kSigEncDER: 0,
	kSigEncP1363: 1,
	kSignJobModeSign: 0,
	kSignJobModeVerify: 1,

	// dh/ecdh (Stage 4)
	DiffieHellman: stubClass("DiffieHellman"),
	DiffieHellmanGroup: stubClass("DiffieHellmanGroup"),
	ECDH: stubClass("ECDH"),
	ECDHConvertKey: () => notImplemented("ECDHConvertKey"),
	DHBitsJob: stubClass("DHBitsJob"),

	// x509
	parseX509,
	certVerifySpkac: () => notImplemented("certVerifySpkac"),
	certExportPublicKey: () => notImplemented("certExportPublicKey"),
	certExportChallenge: () => notImplemented("certExportChallenge"),

	// capabilities
	getCiphers,
	getHashes,
	getCurves,
	getCachedAliases: () => ({}),
	getOpenSSLSecLevelCrypto: () => 0,
	secureHeapUsed: () => undefined,
	timingSafeEqual,
	getFipsCrypto: () => 0,
	setFipsCrypto: (_value: boolean) => {},

	// key type / format / encoding constants
	kKeyTypeSecret,
	kKeyTypePublic,
	kKeyTypePrivate,
	kKeyFormatDER: 0,
	kKeyFormatPEM: 1,
	kKeyFormatJWK: 2,
	kKeyEncodingPKCS1: 0,
	kKeyEncodingPKCS8: 1,
	kKeyEncodingSPKI: 2,
	kKeyEncodingSEC1: 3,

	// WebCrypto enums (kept for parity; subtle itself stays native)
	kWebCryptoKeyFormatRaw: 0,
	kWebCryptoKeyFormatPKCS8: 1,
	kWebCryptoKeyFormatSPKI: 2,
	kWebCryptoKeyFormatJWK: 3,
	kWebCryptoCipherEncrypt: 0,
	kWebCryptoCipherDecrypt: 1,

	// RSA key variants
	kKeyVariantRSA_SSA_PKCS1_v1_5: 0,
	kKeyVariantRSA_PSS: 1,
	kKeyVariantRSA_OAEP: 2,

	// EVP_PKEY ids (OpenSSL NIDs) for the curve25519/448 family
	EVP_PKEY_ED25519: 1087,
	EVP_PKEY_ED448: 1088,
	EVP_PKEY_X25519: 1034,
	EVP_PKEY_X448: 1035,

	// EC parameter encoding
	OPENSSL_EC_EXPLICIT_CURVE: 0,
	OPENSSL_EC_NAMED_CURVE: 1,

	// X509 host/email/ip check flags
	X509_CHECK_FLAG_ALWAYS_CHECK_SUBJECT: 0x1,
	X509_CHECK_FLAG_NO_WILDCARDS: 0x2,
	X509_CHECK_FLAG_NO_PARTIAL_WILDCARDS: 0x4,
	X509_CHECK_FLAG_MULTI_LABEL_WILDCARDS: 0x8,
	X509_CHECK_FLAG_SINGLE_LABEL_SUBDOMAINS: 0x10,
	X509_CHECK_FLAG_NEVER_CHECK_SUBJECT: 0x20,

	// Note: Argon2Job, KmacJob, EVP_PKEY_ML_*, kKeyVariantAES_OCB_128,
	// TurboShakeJob, KangarooTwelveJob, HashJob, ECDHBitsJob, KEM*Job are left
	// undefined on purpose — the JS layer treats their absence as "unsupported".
};
