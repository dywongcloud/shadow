// JS reimplementation of `internalBinding('http2')` (node_http2.cc) backed by
// the nghttp2 wasm shim (see src/worker/node-wasm/nghttp2-shim.c). Client only.
//
// The C++ binding takes ownership of the socket's StreamBase and pumps bytes
// natively. This runtime's sockets are plain stream.Duplex, so instead the
// `consume(socket)` method (called from the patched core.js) installs a JS byte
// pump: socket 'data' -> receive() -> nghttp2 mem_recv (fires the js_h2_*
// callbacks) -> flush() drains nghttp2 output to socket.write().
//
// State arrays (sessionState/streamState/settingsBuffer/optionsBuffer) are the
// exact instances internal/http2/util.js reads/writes; refresh* copies values
// in/out of wasm scratch. Views over wasm `memory` are re-fetched on every call
// (ALLOW_MEMORY_GROWTH detaches them).

// @ts-nocheck — the binding surface is dynamically shaped to match node_http2.cc
// and fights strict typing (handles carry symbol-keyed core.js state).
import { getExports, registerEnv } from "../../../node-wasm/loader";
import streamWrap from "../stream_wrap";
import {
	constants,
	nameForErrorCode,
	DEFAULT_SETTINGS,
	SETTINGS_BUFFER_LEN,
	OPTIONS_BUFFER_LEN,
	IDX_SETTINGS_FLAGS,
	IDX_SESSION_STATE_COUNT,
	IDX_STREAM_STATE_COUNT,
	kSessionUint8FieldCount,
} from "./constants";

const {
	kReadBytesOrError,
	kArrayBufferOffset,
	kBytesWritten,
	kLastWriteWasAsync,
	streamBaseState,
} = streamWrap;

// nghttp2 frame types / flags / data flags (nghttp2.h).
const NGHTTP2_HEADERS = 1;
const NGHTTP2_SETTINGS = 4;
const NGHTTP2_PUSH_PROMISE = 5;
const NGHTTP2_PING = 6;
const NGHTTP2_GOAWAY = 7;
const NGHTTP2_FLAG_ACK = 0x1;
const NGHTTP2_DATA_FLAG_EOF = 0x1;
const NGHTTP2_DATA_FLAG_NO_END_STREAM = 0x2;

const textDecoder = new TextDecoder();
const textEncoder = new TextEncoder();

// ---------------------------------------------------------------------------
// Shared state arrays (the instances util.js destructures from the binding).
// ---------------------------------------------------------------------------
const sessionState = new Float64Array(IDX_SESSION_STATE_COUNT);
const streamState = new Float64Array(IDX_STREAM_STATE_COUNT);
const settingsBuffer = new Uint32Array(SETTINGS_BUFFER_LEN);
const optionsBuffer = new Uint32Array(OPTIONS_BUFFER_LEN);

// ---------------------------------------------------------------------------
// core.js callbacks (registered via setCallbackFunctions). Invoked with the
// handle as `this` — they read `this[owner_symbol]`.
// ---------------------------------------------------------------------------
let cb: {
	internalError?: Function;
	priority?: Function;
	settings?: Function;
	ping?: Function;
	sessionHeaders?: Function;
	frameError?: Function;
	goawayData?: Function;
	altSvc?: Function;
	origin?: Function;
	streamTrailers?: Function;
	streamClose?: Function;
} = {};

function setCallbackFunctions(
	onSessionInternalError: Function,
	onPriority: Function,
	onSettings: Function,
	onPing: Function,
	onSessionHeaders: Function,
	onFrameError: Function,
	onGoawayData: Function,
	onAltSvc: Function,
	onOrigin: Function,
	onStreamTrailers: Function,
	onStreamClose: Function
) {
	cb = {
		internalError: onSessionInternalError,
		priority: onPriority,
		settings: onSettings,
		ping: onPing,
		sessionHeaders: onSessionHeaders,
		frameError: onFrameError,
		goawayData: onGoawayData,
		altSvc: onAltSvc,
		origin: onOrigin,
		streamTrailers: onStreamTrailers,
		streamClose: onStreamClose,
	};
}

// ---------------------------------------------------------------------------
// wasm memory helpers + scratch region
// ---------------------------------------------------------------------------
function e() {
	return getExports();
}
function memU8(ptr: number, len: number) {
	return new Uint8Array(e().memory.buffer, ptr, len);
}
function memU32(ptr: number, len: number) {
	return new Uint32Array(e().memory.buffer, ptr, len);
}
function memF64(ptr: number, len: number) {
	return new Float64Array(e().memory.buffer, ptr, len);
}

// Persistent scratch buffers (malloc pointers survive memory growth).
let scratchF64Ptr = 0; // 9 doubles
let scratchU32Ptr = 0; // SETTINGS_BUFFER_LEN u32
let scratchPingPtr = 0; // 8 bytes
function ensureScratch() {
	if (scratchF64Ptr) return;
	const x = e();
	scratchF64Ptr = x.malloc(IDX_SESSION_STATE_COUNT * 8);
	scratchU32Ptr = x.malloc(SETTINGS_BUFFER_LEN * 4);
	scratchPingPtr = x.malloc(8);
}

function decodeCString(ptr: number): string {
	if (!ptr) return "";
	const bytes = new Uint8Array(e().memory.buffer);
	let end = ptr;
	while (bytes[end] !== 0) end++;
	return textDecoder.decode(bytes.subarray(ptr, end));
}

function toU8(data: any): Uint8Array {
	if (data instanceof Uint8Array) return data;
	if (ArrayBuffer.isView(data))
		return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
	if (data instanceof ArrayBuffer) return new Uint8Array(data);
	if (typeof data === "string") return textEncoder.encode(data);
	throw new TypeError("http2: unsupported data type");
}

function encodeString(str: string, encoding: string): Uint8Array {
	switch (encoding) {
		case "utf8":
		case "utf-8":
			return textEncoder.encode(str);
		case "latin1":
		case "binary": {
			const out = new Uint8Array(str.length);
			for (let i = 0; i < str.length; i++) out[i] = str.charCodeAt(i) & 0xff;
			return out;
		}
		case "ascii": {
			const out = new Uint8Array(str.length);
			for (let i = 0; i < str.length; i++) out[i] = str.charCodeAt(i) & 0x7f;
			return out;
		}
		case "ucs2":
		case "utf16le": {
			const out = new Uint8Array(str.length * 2);
			const dv = new DataView(out.buffer);
			for (let i = 0; i < str.length; i++)
				dv.setUint16(i * 2, str.charCodeAt(i), true);
			return out;
		}
		default:
			return textEncoder.encode(str);
	}
}

// Writes a buildNgHeaderString `[string, count]` pair (Latin1) into wasm and
// returns [ptr, byteLen, count]; caller frees ptr.
function writeHeaderBlob(headersList: [string, number]): [number, number, number] {
	const str = headersList[0];
	const count = headersList[1] | 0;
	const byteLen = str.length;
	const ptr = e().malloc(byteLen || 1);
	if (byteLen) {
		const view = memU8(ptr, byteLen);
		for (let i = 0; i < byteLen; i++) view[i] = str.charCodeAt(i) & 0xff;
	}
	return [ptr, byteLen, count];
}

// ---------------------------------------------------------------------------
// Registries
// ---------------------------------------------------------------------------
const sessions = new Map<number, Http2Session>();
let nextSessionId = 1;
let asyncIdCounter = 1;

// ===========================================================================
// Http2Stream handle
// ===========================================================================
class Http2Stream {
	#session: Http2Session;
	#id: number;
	#asyncId = asyncIdCounter++;
	#outQueue: { data: Uint8Array; off: number; req: any }[] = [];
	#writableEnded = false;
	#shutdownReq: any = null;
	#hasBody: boolean;
	#trailersPending: boolean;
	#destroyed = false;
	onread: Function | null = null;
	reading = false;

	constructor(session: Http2Session, id: number, options: number) {
		this.#session = session;
		this.#id = id;
		this.#hasBody = !(options & constants.STREAM_OPTION_EMPTY_PAYLOAD);
		this.#trailersPending = !!(options & constants.STREAM_OPTION_GET_TRAILERS);
		session._registerStream(id, this);
	}

	get sessionPtr() {
		return this.#session.ptr;
	}

	id() {
		return this.#id;
	}
	getAsyncId() {
		return this.#asyncId;
	}

	// StreamBase write surface (internal/stream_base_commons.js).
	writeBuffer(req: any, data: any) {
		return this.#enqueue(toU8(data), req, data.length ?? data.byteLength);
	}
	writeLatin1String(req: any, data: string) {
		const b = encodeString(data, "latin1");
		return this.#enqueue(b, req, b.length);
	}
	writeUtf8String(req: any, data: string) {
		const b = encodeString(data, "utf8");
		return this.#enqueue(b, req, b.length);
	}
	writeAsciiString(req: any, data: string) {
		const b = encodeString(data, "ascii");
		return this.#enqueue(b, req, b.length);
	}
	writeUcs2String(req: any, data: string) {
		const b = encodeString(data, "ucs2");
		return this.#enqueue(b, req, b.length);
	}
	writev(req: any, chunks: any[], allBuffers: boolean) {
		let total = 0;
		const items: Uint8Array[] = [];
		if (allBuffers) {
			for (let i = 0; i < chunks.length; i++) {
				const b = toU8(chunks[i]);
				items.push(b);
				total += b.length;
			}
		} else {
			for (let i = 0; i < chunks.length; i += 2) {
				const chunk = chunks[i];
				const enc = chunks[i + 1];
				const b =
					typeof chunk === "string" ? encodeString(chunk, enc) : toU8(chunk);
				items.push(b);
				total += b.length;
			}
		}
		for (let i = 0; i < items.length; i++) {
			this.#outQueue.push({
				data: items[i],
				off: 0,
				req: i === items.length - 1 ? req : null,
			});
		}
		streamBaseState[kBytesWritten] = total;
		streamBaseState[kLastWriteWasAsync] = 1;
		e().h2_resume_data(this.#session.ptr, this.#id);
		this.#session._scheduleFlush();
		return 0;
	}

	#enqueue(bytes: Uint8Array, req: any, reportBytes: number) {
		this.#outQueue.push({ data: Uint8Array.from(bytes), off: 0, req });
		streamBaseState[kBytesWritten] = reportBytes;
		streamBaseState[kLastWriteWasAsync] = 1;
		e().h2_resume_data(this.#session.ptr, this.#id);
		this.#session._scheduleFlush();
		return 0;
	}

	// Called by cb_data_read via the session. Fills up to `length` body bytes at
	// `bufPtr`; returns bytes written, sets *flagsPtr, or -1 to defer.
	_provideData(bufPtr: number, length: number, flagsPtr: number): number {
		let written = 0;
		const buf = memU8(bufPtr, length);
		while (written < length && this.#outQueue.length) {
			const item = this.#outQueue[0];
			const avail = item.data.length - item.off;
			const n = Math.min(avail, length - written);
			buf.set(item.data.subarray(item.off, item.off + n), written);
			item.off += n;
			written += n;
			if (item.off >= item.data.length) {
				this.#outQueue.shift();
				if (item.req) this.#session._completeWrite(item.req);
			}
		}
		let flags = 0;
		if (this.#outQueue.length === 0 && this.#writableEnded) {
			flags |= NGHTTP2_DATA_FLAG_EOF;
			if (this.#trailersPending) flags |= NGHTTP2_DATA_FLAG_NO_END_STREAM;
			if (this.#shutdownReq) {
				this.#session._completeWrite(this.#shutdownReq);
				this.#shutdownReq = null;
			}
		}
		if (written === 0 && !(flags & NGHTTP2_DATA_FLAG_EOF)) {
			return -1; // NGHTTP2_ERR_DEFERRED
		}
		memU32(flagsPtr, 1)[0] = flags;
		return written;
	}

	shutdown(req: any) {
		if (!this.#hasBody) return 1; // END_STREAM already flagged on HEADERS
		this.#writableEnded = true;
		this.#shutdownReq = req;
		e().h2_resume_data(this.#session.ptr, this.#id);
		this.#session._scheduleFlush();
		return 0;
	}

	// Deliver a received DATA chunk into the readable side via onread.
	_onData(bytes: Uint8Array) {
		if (!this.onread) return;
		const copy = Uint8Array.from(bytes);
		streamBaseState[kReadBytesOrError] = copy.length;
		streamBaseState[kArrayBufferOffset] = copy.byteOffset;
		this.onread(copy.buffer);
	}

	trailers(headersList: [string, number]) {
		const [ptr, byteLen, count] = writeHeaderBlob(headersList);
		const rv = e().h2_submit_trailers(this.#session.ptr, this.#id, ptr, byteLen, count);
		e().free(ptr);
		if (rv === 0) this.#session._scheduleFlush();
		return rv;
	}

	rstStream(code: number) {
		const rv = e().h2_submit_rst_stream(this.#session.ptr, this.#id, code >>> 0);
		this.#session._scheduleFlush();
		return rv;
	}

	// RFC 9113 deprecated stream priorities; nghttp2 ignores them.
	priority() {
		return 0;
	}
	// Server-only surface — never hit on the client path.
	respond() {
		return constants.NGHTTP2_ERR_INVALID_ARGUMENT;
	}
	pushPromise() {
		return constants.NGHTTP2_ERR_INVALID_ARGUMENT;
	}
	info() {
		return 0;
	}

	readStart() {
		this.reading = true;
		return 0;
	}
	readStop() {
		this.reading = false;
		return 0;
	}

	refreshState() {
		ensureScratch();
		e().h2_refresh_stream_state(this.#session.ptr, this.#id, scratchF64Ptr);
		streamState.set(memF64(scratchF64Ptr, IDX_STREAM_STATE_COUNT));
	}

	destroy() {
		if (this.#destroyed) return;
		this.#destroyed = true;
		this.#outQueue = [];
		this.#session._unregisterStream(this.#id);
	}
}

// ===========================================================================
// Http2Session handle
// ===========================================================================
class Http2Session {
	#sid: number;
	#ptr: number;
	#socket: any = null;
	#streams = new Map<number, Http2Stream>();
	#completedWrites: any[] = [];
	#pingCbs: { cb: Function; ts: number }[] = [];
	#settingsCbs: { cb: Function; ts: number }[] = [];
	#flushing = false;
	#needFlush = false;
	#gracefulClose = false;
	#gracefulDone = false;
	#destroyed = false;
	// Header accumulation for the in-flight HEADERS block.
	#pending: {
		id: number;
		cat: number;
		names: string[];
		values: string[];
		sensitive: string[];
	} | null = null;

	// core.js-assigned fields.
	fields: Uint8Array;
	ongracefulclosecomplete: Function | null = null;
	ondone: Function | null = null;
	chunksSentSinceLastWrite = 0;

	constructor(type: number) {
		this.#sid = nextSessionId++;
		this.fields = new Uint8Array(kSessionUint8FieldCount);
		ensureScratch();
		// Copy the (already-populated) shared optionsBuffer into wasm scratch.
		const optU = memU32(scratchU32Ptr, OPTIONS_BUFFER_LEN);
		optU.set(optionsBuffer.subarray(0, OPTIONS_BUFFER_LEN));
		this.#ptr = e().h2_session_new(this.#sid, scratchU32Ptr);
		if (!this.#ptr) throw new Error("http2: failed to create nghttp2 session");
		sessions.set(this.#sid, this);
		void type; // client-only
	}

	get ptr() {
		return this.#ptr;
	}
	getAsyncId() {
		return this.#sid;
	}

	_registerStream(id: number, stream: Http2Stream) {
		this.#streams.set(id, stream);
	}
	_unregisterStream(id: number) {
		this.#streams.delete(id);
	}
	_completeWrite(req: any) {
		this.#completedWrites.push(req);
	}

	// --- JS byte pump (replaces C++ StreamBase consume) ---
	consume(socket: any) {
		this.#socket = socket;
		socket.on("data", (chunk: any) => this.receive(chunk));
		socket.on("end", () => this.receive(null));
		socket.on("drain", () => this._scheduleFlush());
	}

	receive(data: any) {
		if (this.#destroyed || !this.#ptr) return;
		if (data === null) return; // peer half-close; stream close handles EOF
		const x = e();
		const u = toU8(data);
		const len = u.length;
		if (len > 0) {
			const ptr = x.h2_recv_buf(this.#ptr, len);
			memU8(ptr, len).set(u);
			const rv = x.h2_session_mem_recv(this.#ptr, len);
			if (rv < 0) {
				this.#onError(rv);
				return;
			}
		}
		this._scheduleFlush();
	}

	_scheduleFlush() {
		if (this.#destroyed || !this.#ptr) return;
		if (this.#flushing) {
			this.#needFlush = true;
			return;
		}
		this.#flushing = true;
		try {
			do {
				this.#needFlush = false;
				this.#doSend();
			} while (this.#needFlush && !this.#destroyed && this.#ptr);
		} finally {
			this.#flushing = false;
		}
		this.#maybeGracefulComplete();
	}

	#doSend() {
		const x = e();
		const len = x.h2_session_send(this.#ptr);
		if (len < 0) {
			this.#onError(len);
			return;
		}
		if (len > 0 && this.#socket) {
			const ptr = x.h2_session_send_ptr(this.#ptr);
			const out = Uint8Array.from(memU8(ptr, len));
			this.chunksSentSinceLastWrite++;
			try {
				this.#socket.write(out);
			} catch {
				/* socket gone */
			}
		}
		if (this.#completedWrites.length) {
			const done = this.#completedWrites;
			this.#completedWrites = [];
			for (const req of done) {
				try {
					req.oncomplete(0);
				} catch {
					/* ignore */
				}
			}
		}
	}

	#maybeGracefulComplete() {
		if (
			this.#gracefulClose &&
			!this.#gracefulDone &&
			this.#ptr &&
			e().h2_session_want_write(this.#ptr) === 0
		) {
			this.#gracefulDone = true;
			if (this.ongracefulclosecomplete) {
				try {
					this.ongracefulclosecomplete();
				} catch {
					/* ignore */
				}
			}
		}
	}

	#onError(code: number) {
		if (cb.internalError) {
			try {
				cb.internalError.call(this, code);
			} catch {
				/* ignore */
			}
		}
	}

	// --- request / frame submission ---
	request(
		headersList: [string, number],
		streamOptions: number,
		parent: number,
		weight: number,
		exclusive: boolean
	) {
		const [ptr, byteLen, count] = writeHeaderBlob(headersList);
		const id = e().h2_submit_request(
			this.#ptr,
			ptr,
			byteLen,
			count,
			streamOptions,
			parent | 0,
			weight | 0,
			exclusive ? 1 : 0
		);
		e().free(ptr);
		if (id < 0) return id; // numeric error
		const stream = new Http2Stream(this, id, streamOptions);
		this._scheduleFlush();
		return stream;
	}

	settings(callback: Function) {
		ensureScratch();
		memU32(scratchU32Ptr, SETTINGS_BUFFER_LEN).set(settingsBuffer);
		const rv = e().h2_submit_settings(this.#ptr, scratchU32Ptr);
		if (rv !== 0) return false;
		this.#settingsCbs.push({ cb: callback, ts: Date.now() });
		this._scheduleFlush();
		return true;
	}

	ping(payload: any, callback: Function) {
		let ptr = 0;
		if (payload) {
			ptr = e().malloc(8);
			memU8(ptr, 8).set(toU8(payload).subarray(0, 8));
		}
		const rv = e().h2_submit_ping(this.#ptr, ptr);
		if (ptr) e().free(ptr);
		if (rv !== 0) return false;
		this.#pingCbs.push({ cb: callback, ts: Date.now() });
		this._scheduleFlush();
		return true;
	}

	goaway(code: number, lastStreamID: number, opaqueData: any) {
		let ptr = 0;
		let len = 0;
		if (opaqueData) {
			const u = toU8(opaqueData);
			len = u.length;
			ptr = e().malloc(len || 1);
			if (len) memU8(ptr, len).set(u);
		}
		const rv = e().h2_submit_goaway(
			this.#ptr,
			code >>> 0,
			lastStreamID | 0,
			ptr,
			len
		);
		if (ptr) e().free(ptr);
		this._scheduleFlush();
		return rv;
	}

	rstStream(code: number) {
		// Session-level refuse (used in onSessionHeaders when closed) — no stream.
		return e().h2_submit_rst_stream(this.#ptr, 0, code >>> 0);
	}

	setNextStreamID(id: number) {
		return e().h2_set_next_stream_id(this.#ptr, id | 0);
	}
	setLocalWindowSize(windowSize: number) {
		const rv = e().h2_set_local_window_size(this.#ptr, 0, windowSize | 0);
		this._scheduleFlush();
		return rv;
	}
	updateChunksSent() {
		return this.chunksSentSinceLastWrite;
	}
	hasPendingData() {
		return !!this.#ptr && e().h2_session_want_write(this.#ptr) !== 0;
	}
	setGracefulClose() {
		this.#gracefulClose = true;
		this.#maybeGracefulComplete();
	}

	// Server-only — present so shared core.js code doesn't throw.
	altsvc() {}
	origin() {}

	localSettings() {
		ensureScratch();
		e().h2_get_settings(this.#ptr, 1, scratchU32Ptr);
		settingsBuffer.set(memU32(scratchU32Ptr, 7).subarray(0, 7), 0);
	}
	remoteSettings() {
		ensureScratch();
		e().h2_get_settings(this.#ptr, 0, scratchU32Ptr);
		settingsBuffer.set(memU32(scratchU32Ptr, 7).subarray(0, 7), 0);
	}

	refreshState() {
		ensureScratch();
		e().h2_refresh_session_state(this.#ptr, scratchF64Ptr);
		sessionState.set(memF64(scratchF64Ptr, IDX_SESSION_STATE_COUNT));
	}

	destroy(code: number, _socketClosed: boolean) {
		if (this.#destroyed) return;
		this.#destroyed = true;
		if (this.#ptr) {
			try {
				e().h2_terminate(this.#ptr, (code | 0) >>> 0);
				this.#doSendFinal();
			} catch {
				/* ignore */
			}
			e().h2_session_del(this.#ptr);
			this.#ptr = 0;
		}
		sessions.delete(this.#sid);
		this.#streams.clear();
		if (this.ondone) {
			const done = this.ondone;
			this.ondone = null;
			try {
				done();
			} catch {
				/* ignore */
			}
		}
	}

	#doSendFinal() {
		const x = e();
		const len = x.h2_session_send(this.#ptr);
		if (len > 0 && this.#socket) {
			const ptr = x.h2_session_send_ptr(this.#ptr);
			const out = Uint8Array.from(memU8(ptr, len));
			try {
				this.#socket.write(out);
			} catch {
				/* ignore */
			}
		}
	}

	// --- callback dispatch helpers (called by env trampolines) ---
	_stream(id: number) {
		return this.#streams.get(id);
	}

	_onBeginHeaders(id: number, cat: number) {
		this.#pending = { id, cat, names: [], values: [], sensitive: [] };
	}
	_onHeader(id: number, name: string, value: string, flags: number) {
		if (!this.#pending || this.#pending.id !== id) {
			this.#pending = { id, cat: 1, names: [], values: [], sensitive: [] };
		}
		this.#pending.names.push(name);
		this.#pending.values.push(value);
		if (flags & constants.NGHTTP2_NV_FLAG_NO_INDEX)
			this.#pending.sensitive.push(name);
	}

	_dispatchFrame(type: number, flags: number, streamId: number) {
		if (type === NGHTTP2_HEADERS || type === NGHTTP2_PUSH_PROMISE) {
			const p = this.#pending;
			this.#pending = null;
			const cat = p && p.id === streamId ? p.cat : 1;
			const headers: any[] = [];
			if (p) {
				for (let i = 0; i < p.names.length; i++) {
					headers.push(p.names[i], p.values[i]);
				}
			}
			const sensitive = p ? p.sensitive : [];
			let stream = this.#streams.get(streamId);
			if (!stream) {
				// Peer-initiated (push) — create a handle so core.js can wrap it.
				stream = new Http2Stream(this, streamId, 0);
			}
			if (cb.sessionHeaders)
				cb.sessionHeaders.call(this, stream, streamId, cat, flags, headers, sensitive);
			return;
		}
		if (type === NGHTTP2_SETTINGS) {
			if (flags & NGHTTP2_FLAG_ACK) {
				const entry = this.#settingsCbs.shift();
				if (entry) entry.cb.call(this, true, Date.now() - entry.ts);
			} else if (cb.settings) {
				cb.settings.call(this);
			}
			return;
		}
		if (type === NGHTTP2_PING) {
			if (flags & NGHTTP2_FLAG_ACK) {
				const entry = this.#pingCbs.shift();
				if (entry) {
					ensureScratch();
					e().h2_get_ping_data(this.#ptr, scratchPingPtr);
					const payload = Uint8Array.from(memU8(scratchPingPtr, 8));
					entry.cb.call(this, true, Date.now() - entry.ts, payload);
				}
			} else if (cb.ping) {
				ensureScratch();
				e().h2_get_ping_data(this.#ptr, scratchPingPtr);
				const payload = Uint8Array.from(memU8(scratchPingPtr, 8));
				cb.ping.call(this, payload);
			}
			return;
		}
		if (type === NGHTTP2_GOAWAY) {
			if (cb.goawayData) {
				const x = e();
				const code = x.h2_get_goaway_code(this.#ptr);
				const last = x.h2_get_goaway_last_stream(this.#ptr);
				const oplen = x.h2_get_goaway_opaque_len(this.#ptr);
				let buf: Uint8Array;
				if (oplen > 0) {
					const opptr = x.h2_get_goaway_opaque_ptr(this.#ptr);
					buf = Uint8Array.from(memU8(opptr, oplen));
				} else {
					buf = new Uint8Array(0);
				}
				cb.goawayData.call(this, code, last, buf);
			}
			return;
		}
	}

	_onStreamClose(id: number, code: number) {
		const stream = this.#streams.get(id);
		if (stream && cb.streamClose) cb.streamClose.call(stream, code);
	}

	_onFrameNotSent(id: number, type: number, libError: number) {
		if (cb.frameError) cb.frameError.call(this, id, type, libError);
	}
}

// ---------------------------------------------------------------------------
// env callbacks — dispatch by session id
// ---------------------------------------------------------------------------
registerEnv({
	js_h2_on_begin_headers(sid: number, streamId: number, cat: number) {
		const s = sessions.get(sid);
		if (s) s._onBeginHeaders(streamId, cat);
		return 0;
	},
	js_h2_on_header(
		sid: number,
		streamId: number,
		namePtr: number,
		nameLen: number,
		valuePtr: number,
		valueLen: number,
		flags: number
	) {
		const s = sessions.get(sid);
		if (!s) return 0;
		const buf = new Uint8Array(getExports().memory.buffer);
		const name = textDecoder.decode(buf.subarray(namePtr, namePtr + nameLen));
		const value = textDecoder.decode(
			buf.subarray(valuePtr, valuePtr + valueLen)
		);
		s._onHeader(streamId, name, value, flags);
		return 0;
	},
	js_h2_on_frame_recv(sid: number, type: number, flags: number, streamId: number) {
		const s = sessions.get(sid);
		if (s) s._dispatchFrame(type, flags, streamId);
		return 0;
	},
	js_h2_on_data_chunk(
		sid: number,
		streamId: number,
		dataPtr: number,
		len: number,
		_flags: number
	) {
		const s = sessions.get(sid);
		if (!s) return 0;
		const stream = s._stream(streamId);
		if (stream)
			stream._onData(
				new Uint8Array(getExports().memory.buffer, dataPtr, len)
			);
		return 0;
	},
	js_h2_on_stream_close(sid: number, streamId: number, code: number) {
		const s = sessions.get(sid);
		if (s) s._onStreamClose(streamId, code >>> 0);
		return 0;
	},
	js_h2_on_frame_not_sent(
		sid: number,
		streamId: number,
		type: number,
		libError: number
	) {
		const s = sessions.get(sid);
		if (s) s._onFrameNotSent(streamId, type, libError);
		return 0;
	},
	js_h2_on_frame_send() {
		return 0;
	},
	js_h2_on_error(sid: number, libError: number, _msg: number, _len: number) {
		const s = sessions.get(sid);
		if (s && cb.internalError) {
			try {
				cb.internalError.call(s, libError);
			} catch {
				/* ignore */
			}
		}
	},
	js_h2_data_read(
		sid: number,
		streamId: number,
		bufPtr: number,
		length: number,
		flagsPtr: number
	) {
		const s = sessions.get(sid);
		if (!s) return -1;
		const stream = s._stream(streamId);
		if (!stream) return -1;
		return stream._provideData(bufPtr, length, flagsPtr);
	},
});

// ---------------------------------------------------------------------------
// module-level binding functions
// ---------------------------------------------------------------------------
function packSettings(): Uint8Array {
	ensureScratch();
	memU32(scratchU32Ptr, SETTINGS_BUFFER_LEN).set(settingsBuffer);
	const cap = 6 * 17;
	const outPtr = e().malloc(cap);
	const len = e().h2_pack_settings(scratchU32Ptr, outPtr, cap);
	let result: Uint8Array;
	if (len < 0) {
		result = new Uint8Array(0);
	} else {
		result = Uint8Array.from(memU8(outPtr, len));
	}
	e().free(outPtr);
	return result;
}

function refreshDefaultSettings() {
	for (let i = 0; i < 7; i++) settingsBuffer[i] = DEFAULT_SETTINGS[i];
	settingsBuffer[IDX_SETTINGS_FLAGS] = 0x7f; // all 7 present
	settingsBuffer[IDX_SETTINGS_FLAGS + 1] = 0; // no custom
}

function nghttp2ErrorString(code: number): string {
	return decodeCString(e().h2_strerror(code | 0));
}

export default {
	Http2Session,
	Http2Stream,
	constants,
	nameForErrorCode,
	setCallbackFunctions,
	packSettings,
	refreshDefaultSettings,
	nghttp2ErrorString,
	sessionState,
	streamState,
	settingsBuffer,
	optionsBuffer,
};
