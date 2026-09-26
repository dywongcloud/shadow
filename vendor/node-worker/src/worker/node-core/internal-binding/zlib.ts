// JS-side adapter for `internalBinding('zlib')`. Owns a wasm handle per
// stream and stages input/output through per-handle scratch buffers in wasm
// memory. The C shim (`src/worker/node-wasm/zlib-shim.c`) owns the
// `z_stream`, windowBits adjustments, dictionary application, and the
// trailing-gzip-member loop, so this class is mostly buffer plumbing.

import { getExports } from "../../node-wasm/loader";

// node_zlib_mode values (must match constants.ts and zlib-shim.c).
const NONE = 0;
const DEFLATE = 1;
const INFLATE = 2;
const GZIP = 3;
const GUNZIP = 4;
const DEFLATERAW = 5;
const INFLATERAW = 6;
const UNZIP = 7;
const BROTLI_DECODE = 8;
const BROTLI_ENCODE = 9;

// zlib return codes.
const Z_OK = 0;
const Z_STREAM_END = 1;
const Z_NEED_DICT = 2;
const Z_BUF_ERROR = -5;
const Z_DATA_ERROR = -3;

// flush values used here directly.
const Z_NO_FLUSH = 0;
const Z_FINISH = 4;

const errnoToCode: Record<number, string> = {
	[-1]: "Z_ERRNO",
	[-2]: "Z_STREAM_ERROR",
	[-3]: "Z_DATA_ERROR",
	[-4]: "Z_MEM_ERROR",
	[-5]: "Z_BUF_ERROR",
	[-6]: "Z_VERSION_ERROR",
};

const textDecoder = new TextDecoder();

function readCString(memory: WebAssembly.Memory, ptr: number): string {
	if (!ptr) return "";
	const bytes = new Uint8Array(memory.buffer);
	let end = ptr;
	while (bytes[end] !== 0) end++;
	return textDecoder.decode(bytes.subarray(ptr, end));
}

class Zlib {
	mode: number;
	handle: number = 0;
	err: number = Z_OK;
	flush: number = Z_NO_FLUSH;
	writeState: Uint32Array | null = null;
	processCallback: ((this: Zlib) => void) | null = null;
	onerror: ((msg: string, errno: number, code: string) => void) | null = null;

	// `node_core/lib/zlib.js` decorates the handle with `buffer`, `cb`,
	// `availInBefore`, `availOutBefore`, `inOff`, `flushFlag`, `[owner_symbol]`.
	// Keep the index signature so those writes don't fail strict checks.
	[key: string]: any;

	constructor(mode: number) {
		if (typeof mode !== "number" || mode < DEFLATE || mode > UNZIP) {
			throw new TypeError(`Invalid zlib mode: ${mode}`);
		}
		const wasm = getExports();
		this.mode = mode;
		this.handle = wasm.zlib_alloc(mode);
		if (!this.handle) {
			throw new Error(`Failed to allocate zlib handle for mode ${mode}`);
		}
	}

	init(
		windowBits: number,
		level: number,
		memLevel: number,
		strategy: number,
		writeState: Uint32Array,
		processCallback: (this: Zlib) => void,
		dictionary?: Uint8Array
	) {
		this.writeState = writeState;
		this.processCallback = processCallback;

		const wasm = getExports();
		let dictPtr = 0;
		let dictLen = 0;
		if (dictionary && dictionary.length > 0) {
			dictLen = dictionary.length;
			dictPtr = wasm.malloc(dictLen);
			if (!dictPtr) {
				this.err = -4;
				this._error("Failed to allocate dictionary");
				return;
			}
			new Uint8Array(wasm.memory.buffer, dictPtr, dictLen).set(dictionary);
		}

		this.err = wasm.zlib_init(
			this.handle,
			windowBits,
			level,
			memLevel,
			strategy,
			dictPtr,
			dictLen
		);

		if (dictPtr) wasm.free(dictPtr);

		if (this.err !== Z_OK) {
			this._error("Init error");
		}
	}

	write(
		flush: number,
		input: Uint8Array | null,
		inOff: number,
		inLen: number,
		out: Uint8Array,
		outOff: number,
		outLen: number
	): this {
		queueMicrotask(() => {
			this._process(flush, input, inOff, inLen, out, outOff, outLen);
			this.processCallback?.call(this);
		});
		return this;
	}

	writeSync(
		flush: number,
		input: Uint8Array | null,
		inOff: number,
		inLen: number,
		out: Uint8Array,
		outOff: number,
		outLen: number
	): void {
		this._process(flush, input, inOff, inLen, out, outOff, outLen);
	}

	_process(
		flush: number,
		input: Uint8Array | null,
		inOff: number,
		inLen: number,
		out: Uint8Array,
		outOff: number,
		outLen: number
	) {
		const wasm = getExports();
		this.flush = flush;

		if (input == null) {
			inLen = 0;
			inOff = 0;
		}

		const inBufPtr = wasm.zlib_ensure_in_buf(this.handle, inLen || 1);
		const outBufPtr = wasm.zlib_ensure_out_buf(this.handle, outLen || 1);
		if (!inBufPtr || !outBufPtr) {
			this.err = -4;
			this._error("Failed to allocate scratch buffer");
			return;
		}

		// Re-fetch memory after potential growth.
		if (input && inLen > 0) {
			new Uint8Array(wasm.memory.buffer, inBufPtr, inLen).set(
				input.subarray(inOff, inOff + inLen)
			);
		}

		this.err = wasm.zlib_write(this.handle, flush, inLen, outLen);

		const availOut = wasm.zlib_avail_out(this.handle);
		const availIn = wasm.zlib_avail_in(this.handle);
		const produced = outLen - availOut;

		if (produced > 0) {
			// Re-fetch view in case zlib_write grew the heap (unlikely once
			// init is past, but cheap to be safe).
			const outView = new Uint8Array(wasm.memory.buffer, outBufPtr, produced);
			new Uint8Array(out.buffer, out.byteOffset + outOff, produced).set(outView);
		}

		if (this.writeState) {
			this.writeState[0] = availOut;
			this.writeState[1] = availIn;
		}

		this._checkError();
	}

	_checkError(): boolean {
		switch (this.err) {
			case Z_OK:
			case Z_BUF_ERROR:
				if (this._availOutAfter() !== 0 && this.flush === Z_FINISH) {
					this._error("unexpected end of file");
					return false;
				}
				break;
			case Z_STREAM_END:
				break;
			case Z_NEED_DICT:
				this._error("Missing dictionary");
				return false;
			default:
				this._error("Zlib error");
				return false;
		}
		return true;
	}

	_availOutAfter(): number {
		return this.writeState ? this.writeState[0] : 0;
	}

	_error(fallbackMessage: string) {
		const wasm = getExports();
		const msgPtr = wasm.zlib_get_msg(this.handle);
		const msg = msgPtr ? readCString(wasm.memory, msgPtr) : fallbackMessage;
		const code = errnoToCode[this.err] ?? "Z_UNKNOWN";
		if (this.onerror) {
			this.onerror(msg, this.err, code);
		} else {
			const err = new Error(msg) as Error & { errno: number; code: string };
			err.errno = this.err;
			err.code = code;
			throw err;
		}
	}

	params(level: number, strategy: number) {
		const wasm = getExports();
		wasm.zlib_params(this.handle, level, strategy);
	}

	reset() {
		const wasm = getExports();
		this.err = wasm.zlib_reset(this.handle);
		if (this.err !== Z_OK) {
			this._error("Failed to reset stream");
		}
	}

	close() {
		if (this.handle) {
			const wasm = getExports();
			wasm.zlib_end(this.handle);
			this.handle = 0;
		}
		this.mode = NONE;
	}
}

// Shared base for BrotliEncoder / BrotliDecoder. `node_core/lib/zlib.js` only
// distinguishes the two by which constructor it calls; the binding logic is
// identical once you have a handle.
class BrotliBase {
	mode: number;
	handle: number = 0;
	err: number = 0;
	flush: number = 0;
	writeState: Uint32Array | null = null;
	processCallback: ((this: BrotliBase) => void) | null = null;
	onerror: ((msg: string, errno: number, code: string) => void) | null = null;

	[key: string]: any;

	constructor(mode: number) {
		if (mode !== BROTLI_DECODE && mode !== BROTLI_ENCODE) {
			throw new TypeError(`Invalid brotli mode: ${mode}`);
		}
		const wasm = getExports();
		this.mode = mode;
		this.handle = wasm.brotli_alloc(mode);
		if (!this.handle) {
			throw new Error(`Failed to allocate brotli handle for mode ${mode}`);
		}
	}

	init(
		params: Uint32Array,
		writeState: Uint32Array,
		processCallback: (this: BrotliBase) => void,
		dictionary?: Uint8Array
	) {
		this.writeState = writeState;
		this.processCallback = processCallback;

		const wasm = getExports();
		const paramsLen = params.length;
		const paramsBytes = paramsLen * 4;
		const paramsPtr = wasm.malloc(paramsBytes);
		if (!paramsPtr) {
			this.err = -1;
			this._error("Failed to allocate brotli params buffer");
			return;
		}
		new Uint32Array(wasm.memory.buffer, paramsPtr, paramsLen).set(params);

		let dictPtr = 0;
		let dictLen = 0;
		if (dictionary && dictionary.length > 0) {
			dictLen = dictionary.length;
			dictPtr = wasm.malloc(dictLen);
			if (!dictPtr) {
				wasm.free(paramsPtr);
				this.err = -1;
				this._error("Failed to allocate brotli dictionary buffer");
				return;
			}
			new Uint8Array(wasm.memory.buffer, dictPtr, dictLen).set(dictionary);
		}

		this.err = wasm.brotli_init(
			this.handle,
			paramsPtr,
			paramsLen,
			dictPtr,
			dictLen
		);

		wasm.free(paramsPtr);
		if (dictPtr) wasm.free(dictPtr);

		if (this.err < 0) {
			this._error("Brotli init failed");
		}
	}

	write(
		flush: number,
		input: Uint8Array | null,
		inOff: number,
		inLen: number,
		out: Uint8Array,
		outOff: number,
		outLen: number
	): this {
		queueMicrotask(() => {
			this._process(flush, input, inOff, inLen, out, outOff, outLen);
			this.processCallback?.call(this);
		});
		return this;
	}

	writeSync(
		flush: number,
		input: Uint8Array | null,
		inOff: number,
		inLen: number,
		out: Uint8Array,
		outOff: number,
		outLen: number
	): void {
		this._process(flush, input, inOff, inLen, out, outOff, outLen);
	}

	_process(
		flush: number,
		input: Uint8Array | null,
		inOff: number,
		inLen: number,
		out: Uint8Array,
		outOff: number,
		outLen: number
	) {
		const wasm = getExports();
		this.flush = flush;

		if (input == null) {
			inLen = 0;
			inOff = 0;
		}

		const inBufPtr = wasm.brotli_ensure_in_buf(this.handle, inLen || 1);
		const outBufPtr = wasm.brotli_ensure_out_buf(this.handle, outLen || 1);
		if (!inBufPtr || !outBufPtr) {
			this.err = -1;
			this._error("Failed to allocate brotli scratch buffer");
			return;
		}

		if (input && inLen > 0) {
			new Uint8Array(wasm.memory.buffer, inBufPtr, inLen).set(
				input.subarray(inOff, inOff + inLen)
			);
		}

		this.err = wasm.brotli_write(this.handle, flush, inLen, outLen);

		const availOut = wasm.brotli_avail_out(this.handle);
		const availIn = wasm.brotli_avail_in(this.handle);
		const produced = outLen - availOut;

		if (produced > 0) {
			const outView = new Uint8Array(wasm.memory.buffer, outBufPtr, produced);
			new Uint8Array(out.buffer, out.byteOffset + outOff, produced).set(outView);
		}

		if (this.writeState) {
			this.writeState[0] = availOut;
			this.writeState[1] = availIn;
		}

		if (this.err < 0) {
			this._error("Brotli stream error");
		}
	}

	_error(fallbackMessage: string) {
		const wasm = getExports();
		const msgPtr = wasm.brotli_get_msg(this.handle);
		const msg = msgPtr ? readCString(wasm.memory, msgPtr) : fallbackMessage;
		const code = "ERR_OPERATION_FAILED";
		if (this.onerror) {
			this.onerror(msg, this.err, code);
		} else {
			const err = new Error(msg) as Error & { errno: number; code: string };
			err.errno = this.err;
			err.code = code;
			throw err;
		}
	}

	params(_level: number, _strategy: number) {
		// Brotli doesn't expose mid-stream parameter changes; node's binding
		// accepts the call as a no-op for symmetry with the zlib code path.
	}

	reset() {
		// Brotli has no stream reset. node_core/lib/zlib.js doesn't call
		// reset() on brotli streams in its normal flow; throw if it ever does
		// so the regression surfaces loudly.
		throw new Error("brotli streams cannot be reset");
	}

	close() {
		if (this.handle) {
			const wasm = getExports();
			wasm.brotli_end(this.handle);
			this.handle = 0;
		}
		this.mode = NONE;
	}
}

class BrotliEncoder extends BrotliBase {
	constructor() {
		super(BROTLI_ENCODE);
	}
}

class BrotliDecoder extends BrotliBase {
	constructor() {
		super(BROTLI_DECODE);
	}
}

class ZstdCompress {
	constructor() {
		throw new Error("Zstd compression is not supported in this runtime");
	}
}

class ZstdDecompress {
	constructor() {
		throw new Error("Zstd decompression is not supported in this runtime");
	}
}

function crc32(data: Uint8Array, initial: number = 0): number {
	const wasm = getExports();
	const len = data.length;
	if (len === 0) {
		return initial >>> 0;
	}
	const ptr = wasm.malloc(len);
	if (!ptr) {
		throw new Error("Failed to allocate crc32 buffer");
	}
	try {
		new Uint8Array(wasm.memory.buffer, ptr, len).set(data);
		return wasm.zlib_crc32_buf(ptr, len, initial >>> 0) >>> 0;
	} finally {
		wasm.free(ptr);
	}
}

export default {
	Zlib,
	BrotliEncoder,
	BrotliDecoder,
	ZstdCompress,
	ZstdDecompress,
	crc32,
};
