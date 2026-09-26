import { getExports, registerEnv } from '../../../node-wasm/loader';

const textDecoder = new TextDecoder();
const defaultMaxHeaderSize = 16 * 1024;

const HPE_OK = 0;
const HPE_PAUSED_UPGRADE = 22;
const HPE_USER = 24;

const allMethods = Object.freeze([
	'DELETE',
	'GET',
	'HEAD',
	'POST',
	'PUT',
	'CONNECT',
	'OPTIONS',
	'TRACE',
	'COPY',
	'LOCK',
	'MKCOL',
	'MOVE',
	'PROPFIND',
	'PROPPATCH',
	'SEARCH',
	'UNLOCK',
	'BIND',
	'REBIND',
	'UNBIND',
	'ACL',
	'REPORT',
	'MKACTIVITY',
	'CHECKOUT',
	'MERGE',
	'M-SEARCH',
	'NOTIFY',
	'SUBSCRIBE',
	'UNSUBSCRIBE',
	'PATCH',
	'PURGE',
	'MKCALENDAR',
	'LINK',
	'UNLINK',
	'SOURCE',
	'PRI',
	'DESCRIBE',
	'ANNOUNCE',
	'SETUP',
	'PLAY',
	'PAUSE',
	'TEARDOWN',
	'GET_PARAMETER',
	'SET_PARAMETER',
	'REDIRECT',
	'RECORD',
	'FLUSH',
	'QUERY',
]);

const methods = Object.freeze(allMethods.filter((_, index) => index <= 33 || index === 46));

const parsers = new Map();

registerEnv({
	wasm_on_message_begin(pointer) {
		const parser = parsers.get(pointer);
		if (!parser) return 0;
		parser._onMessageBegin();
		return parser._callIntegerCallback(HTTPParser.kOnMessageBegin);
	},
	wasm_on_url(pointer, at, length) {
		const parser = parsers.get(pointer);
		if (!parser) return 0;
		parser._url += parser._decodeSpan(at, length);
		return 0;
	},
	wasm_on_status(pointer, at, length) {
		const parser = parsers.get(pointer);
		if (!parser) return 0;
		parser._statusMessage += parser._decodeSpan(at, length);
		return 0;
	},
	wasm_on_header_field(pointer, at, length) {
		const parser = parsers.get(pointer);
		if (!parser) return 0;
		parser._appendHeaderField(parser._decodeSpan(at, length));
		return 0;
	},
	wasm_on_header_value(pointer, at, length) {
		const parser = parsers.get(pointer);
		if (!parser) return 0;
		parser._appendHeaderValue(parser._decodeSpan(at, length));
		return 0;
	},
	wasm_on_headers_complete(pointer, statusCode, upgrade, shouldKeepAlive) {
		const parser = parsers.get(pointer);
		if (!parser) return 0;
		return parser._onHeadersComplete(statusCode, upgrade !== 0, shouldKeepAlive !== 0);
	},
	wasm_on_body(pointer, at, length) {
		const parser = parsers.get(pointer);
		if (!parser) return 0;
		const callback = parser[HTTPParser.kOnBody];
		if (typeof callback !== 'function') return 0;
		const body = parser._copySpan(at, length);
		const result = callback.call(parser, body);
		return Number.isInteger(result) ? result : 0;
	},
	wasm_on_message_complete(pointer) {
		const parser = parsers.get(pointer);
		if (!parser) return 0;
		parser._onMessageComplete();
		return parser._callIntegerCallback(HTTPParser.kOnMessageComplete);
	},
});

function getWasmExports() {
	return getExports();
}

function buildHeaders(fields, values) {
	const headers = [];
	for (let index = 0; index < fields.length; index++) {
		headers.push(fields[index], values[index] ?? '');
	}
	return headers;
}

function decodeCString(ptr) {
	if (!ptr) {
		return '';
	}

	const bytes = new Uint8Array(getWasmExports().memory.buffer);
	let end = ptr;
	while (bytes[end] !== 0) {
		end++;
	}
	return textDecoder.decode(bytes.subarray(ptr, end));
}

function toUint8Array(data) {
	if (data instanceof Uint8Array) {
		return data;
	}
	if (ArrayBuffer.isView(data)) {
		return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
	}
	if (data instanceof ArrayBuffer) {
		return new Uint8Array(data);
	}
	throw new TypeError('HTTPParser.execute() expects a Uint8Array or ArrayBufferView');
}

class ConnectionsList {
	#records = new Map();

	_attach(parser) {
		this.#records.set(parser.pointer, {
			parser,
			socket: parser.socket ?? null,
			active: false,
			startedAt: 0,
		});
	}

	_detach(parser) {
		this.#records.delete(parser.pointer);
	}

	_markActive(parser) {
		const record = this.#records.get(parser.pointer);
		if (!record) return;
		record.socket = parser.socket ?? record.socket;
		record.active = true;
		record.startedAt = Date.now();
	}

	_markIdle(parser) {
		const record = this.#records.get(parser.pointer);
		if (!record) return;
		record.socket = parser.socket ?? record.socket;
		record.active = false;
		record.startedAt = 0;
	}

	all() {
		return Array.from(this.#records.values());
	}

	idle() {
		return this.all().filter((record) => !record.active);
	}

	active() {
		return this.all().filter((record) => record.active);
	}

	expired(headersTimeout, requestTimeout) {
		const now = Date.now();
		const timeout = Math.max(headersTimeout || 0, requestTimeout || 0);
		if (timeout <= 0) return [];
		return this.active().filter((record) => record.startedAt > 0 && now - record.startedAt >= timeout);
	}
}

class HTTPParser {
	constructor() {
		const exports = getWasmExports();
		this.pointer = exports.llhttp_wasm_alloc(HTTPParser.BOTH);
		if (!this.pointer) {
			throw new Error('Failed to allocate llhttp parser');
		}

		parsers.set(this.pointer, this);
		this._resetState();
	}

	_resetState() {
		this._url = '';
		this._statusMessage = '';
		this._headerFields = [];
		this._headerValues = [];
		this._lastHeaderKind = null;
		this._lastBuffer = new Uint8Array(0);
		this._lastInputPointer = 0;
	}

	_onMessageBegin() {
		this._resetState();
		this.connectionsList?._markActive(this);
	}

	_onMessageComplete() {
		this.connectionsList?._markIdle(this);
	}

	_appendHeaderField(chunk) {
		if (this._lastHeaderKind === 'field' && this._headerFields.length > this._headerValues.length) {
			this._headerFields[this._headerFields.length - 1] += chunk;
		} else {
			this._headerFields.push(chunk);
		}
		this._lastHeaderKind = 'field';
	}

	_appendHeaderValue(chunk) {
		if (this._lastHeaderKind === 'value' && this._headerValues.length === this._headerFields.length) {
			this._headerValues[this._headerValues.length - 1] += chunk;
		} else {
			this._headerValues.push(chunk);
		}
		this._lastHeaderKind = 'value';
	}

	_decodeSpan(at, length) {
		return textDecoder.decode(new Uint8Array(getWasmExports().memory.buffer, at, length));
	}

	_copySpan(at, length) {
		return Uint8Array.from(new Uint8Array(getWasmExports().memory.buffer, at, length));
	}

	_callIntegerCallback(slot, ...args) {
		const callback = this[slot];
		if (typeof callback !== 'function') {
			return 0;
		}

		const result = callback.call(this, ...args);
		return Number.isInteger(result) ? result : 0;
	}

	_onHeadersComplete(statusCode, upgrade, shouldKeepAlive) {
		const exports = getWasmExports();
		const headers = buildHeaders(this._headerFields, this._headerValues);
		const versionMajor = exports.llhttp_get_http_major(this.pointer);
		const versionMinor = exports.llhttp_get_http_minor(this.pointer);
		const type = exports.llhttp_get_type(this.pointer);

		if (type === HTTPParser.REQUEST) {
			return this._callIntegerCallback(
				HTTPParser.kOnHeadersComplete,
				versionMajor,
				versionMinor,
				headers,
				exports.llhttp_get_method(this.pointer),
				this._url,
				undefined,
				undefined,
				upgrade,
				shouldKeepAlive,
			);
		}

		return this._callIntegerCallback(
			HTTPParser.kOnHeadersComplete,
			versionMajor,
			versionMinor,
			headers,
			undefined,
			undefined,
			statusCode,
			this._statusMessage,
			upgrade,
			shouldKeepAlive,
		);
	}

	_createError(err, bytesParsed) {
		const exports = getWasmExports();
		const errnoName = decodeCString(exports.llhttp_errno_name(err));
		const rawReason = decodeCString(exports.llhttp_get_error_reason(this.pointer));
		let code = errnoName;
		let reason = rawReason;

		if (err === HPE_USER) {
			const delimiter = rawReason.indexOf(':');
			if (delimiter !== -1) {
				code = rawReason.slice(0, delimiter);
				reason = rawReason.slice(delimiter + 1);
			}
		}

		const error = new Error(`Parse Error: ${reason || code}`);
		error.bytesParsed = bytesParsed;
		error.code = code;
		error.reason = reason || code;
		return error;
	}

	initialize(type, resource, maxHeaderSize = 0, lenientFlags = 0, connectionsList = null) {
		const exports = getWasmExports();
		exports.llhttp_wasm_init(this.pointer, type);
		this.resource = resource;
		this.maxHeaderSize = maxHeaderSize || defaultMaxHeaderSize;
		this.connectionsList?._detach(this);
		this.connectionsList = connectionsList;
		if (this.connectionsList) {
			this.connectionsList._attach(this);
		}
		this._resetState();

		if (lenientFlags & HTTPParser.kLenientHeaders) {
			exports.llhttp_set_lenient_headers(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientChunkedLength) {
			exports.llhttp_set_lenient_chunked_length(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientKeepAlive) {
			exports.llhttp_set_lenient_keep_alive(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientTransferEncoding) {
			exports.llhttp_set_lenient_transfer_encoding(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientVersion) {
			exports.llhttp_set_lenient_version(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientDataAfterClose) {
			exports.llhttp_set_lenient_data_after_close(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientOptionalLFAfterCR) {
			exports.llhttp_set_lenient_optional_lf_after_cr(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientOptionalCRLFAfterChunk) {
			exports.llhttp_set_lenient_optional_crlf_after_chunk(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientOptionalCRBeforeLF) {
			exports.llhttp_set_lenient_optional_cr_before_lf(this.pointer, 1);
		}
		if (lenientFlags & HTTPParser.kLenientSpacesAfterChunkSize) {
			exports.llhttp_set_lenient_spaces_after_chunk_size(this.pointer, 1);
		}
	}

	execute(data) {
		const exports = getWasmExports();
		const input = toUint8Array(data);
		const pointer = exports.malloc(Math.max(input.byteLength, 1));
		if (!pointer) {
			throw new Error('Failed to allocate wasm input buffer');
		}

		this._lastBuffer = Uint8Array.from(input);
		this._lastInputPointer = pointer;

		if (input.byteLength > 0) {
			new Uint8Array(exports.memory.buffer, pointer, input.byteLength).set(input);
		}

		let err;
		let bytesParsed = input.byteLength;
		try {
			err = exports.llhttp_execute(this.pointer, pointer, input.byteLength);
			if (err !== HPE_OK) {
				bytesParsed = Math.max(0, exports.llhttp_get_error_pos(this.pointer) - pointer);
			}
		} finally {
			exports.free(pointer);
			this._lastInputPointer = 0;
		}

		if (err === HPE_PAUSED_UPGRADE) {
			exports.llhttp_resume_after_upgrade(this.pointer);
			return bytesParsed;
		}

		if (err !== HPE_OK) {
			return this._createError(err, bytesParsed);
		}

		return bytesParsed;
	}

	finish() {
		const err = getWasmExports().llhttp_finish(this.pointer);
		if (err === HPE_OK) {
			return undefined;
		}
		return this._createError(err, 0);
	}

	pause() {
		getWasmExports().llhttp_pause(this.pointer);
	}

	resume() {
		getWasmExports().llhttp_resume(this.pointer);
	}

	close() {
		this.destroy();
	}

	free() {}

	remove() {
		this.connectionsList?._detach(this);
	}

	consume() {
		this._consumed = true;
	}

	unconsume() {
		this._consumed = false;
	}

	getCurrentBuffer() {
		return Uint8Array.from(this._lastBuffer);
	}

	destroy() {
		if (!this.pointer) {
			return;
		}
		this.connectionsList?._detach(this);
		parsers.delete(this.pointer);
		getWasmExports().llhttp_wasm_free(this.pointer);
		this.pointer = 0;
	}
}

HTTPParser.BOTH = 0;
HTTPParser.REQUEST = 1;
HTTPParser.RESPONSE = 2;

HTTPParser.kOnMessageBegin = 0;
HTTPParser.kOnHeaders = 1;
HTTPParser.kOnHeadersComplete = 2;
HTTPParser.kOnBody = 3;
HTTPParser.kOnMessageComplete = 4;
HTTPParser.kOnExecute = 5;
HTTPParser.kOnTimeout = 6;

HTTPParser.kLenientNone = 0;
HTTPParser.kLenientHeaders = 1 << 0;
HTTPParser.kLenientChunkedLength = 1 << 1;
HTTPParser.kLenientKeepAlive = 1 << 2;
HTTPParser.kLenientTransferEncoding = 1 << 3;
HTTPParser.kLenientVersion = 1 << 4;
HTTPParser.kLenientDataAfterClose = 1 << 5;
HTTPParser.kLenientOptionalLFAfterCR = 1 << 6;
HTTPParser.kLenientOptionalCRLFAfterChunk = 1 << 7;
HTTPParser.kLenientOptionalCRBeforeLF = 1 << 8;
HTTPParser.kLenientSpacesAfterChunkSize = 1 << 9;
HTTPParser.kLenientAll =
	HTTPParser.kLenientHeaders |
	HTTPParser.kLenientChunkedLength |
	HTTPParser.kLenientKeepAlive |
	HTTPParser.kLenientTransferEncoding |
	HTTPParser.kLenientVersion |
	HTTPParser.kLenientDataAfterClose |
	HTTPParser.kLenientOptionalLFAfterCR |
	HTTPParser.kLenientOptionalCRLFAfterChunk |
	HTTPParser.kLenientOptionalCRBeforeLF |
	HTTPParser.kLenientSpacesAfterChunkSize;

export { ConnectionsList, HTTPParser, allMethods, methods };

export default {
	ConnectionsList,
	HTTPParser,
	allMethods,
	methods,
};
