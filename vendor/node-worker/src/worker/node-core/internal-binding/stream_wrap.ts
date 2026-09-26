// `internalBinding('stream_wrap')` exposes the libuv handle wrappers used by
// `internal/webstreams/adapters.js` and `internal/stream_base_commons.js`.
// The http2 client path (internal-binding/http2) drives real StreamBase writes
// through the Http2Stream handle, so `createWriteWrap` / `shutdownWritable`
// construct these — they must be plain, instantiable carrier objects (fields
// set by stream_base_commons / core.js), not throwing stubs.

class WriteWrap {
	handle: any = null;
	oncomplete: any = null;
	callback: any = null;
	async = false;
	bytes = 0;
	buffer: any = null;
}

class ShutdownWrap {
	handle: any = null;
	oncomplete: any = null;
	callback: any = null;
}

// Indices into `streamBaseState`. The fields here just have to exist; the
// values follow upstream's `node_stream_base.h` ordering.
const kReadBytesOrError = 0;
const kArrayBufferOffset = 1;
const kBytesWritten = 2;
const kLastWriteWasAsync = 3;
const kStreamBaseStateFields = 4;

const streamBaseState = new Uint32Array(kStreamBaseStateFields);

export default {
	WriteWrap,
	ShutdownWrap,
	kReadBytesOrError,
	kArrayBufferOffset,
	kBytesWritten,
	kLastWriteWasAsync,
	streamBaseState,
};
