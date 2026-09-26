// Re-export the browser's WHATWG `ReadableStream` family. Workers always have
// these globals; using them lets us drop the ~3000 line upstream impl entirely.
// External callers (`internal/blob`, `internal/fs/promises`, `stream/web`) use
// only the public classes plus a couple of brand-check / helper functions,
// which we shim below.

const ReadableStream = globalThis.ReadableStream;
const ReadableStreamDefaultReader = globalThis.ReadableStreamDefaultReader;
const ReadableStreamBYOBReader = globalThis.ReadableStreamBYOBReader;
const ReadableStreamBYOBRequest = globalThis.ReadableStreamBYOBRequest;
const ReadableByteStreamController = globalThis.ReadableByteStreamController;
const ReadableStreamDefaultController = globalThis.ReadableStreamDefaultController;

function isReadableStream(value) {
  return value instanceof ReadableStream;
}

function isReadableStreamLocked(stream) {
  return stream.locked;
}

function isReadableStreamDefaultReader(value) {
  return ReadableStreamDefaultReader && value instanceof ReadableStreamDefaultReader;
}

function isReadableStreamBYOBReader(value) {
  return ReadableStreamBYOBReader && value instanceof ReadableStreamBYOBReader;
}

function isReadableStreamBYOBRequest(value) {
  return ReadableStreamBYOBRequest && value instanceof ReadableStreamBYOBRequest;
}

function isReadableByteStreamController(value) {
  return ReadableByteStreamController && value instanceof ReadableByteStreamController;
}

function isWritableStreamDefaultWriter(value) {
  return globalThis.WritableStreamDefaultWriter && value instanceof globalThis.WritableStreamDefaultWriter;
}

function isWritableStreamDefaultController(value) {
  return globalThis.WritableStreamDefaultController && value instanceof globalThis.WritableStreamDefaultController;
}

// Used by `internal/fs/promises` to clean up file ReadStreams.
async function readableStreamCancel(stream, reason) {
  try {
    await stream.cancel(reason);
  } catch {
    // upstream swallows cancel errors at this site too
  }
}

async function readableStreamPipeTo(stream, dest, ...args) {
  return stream.pipeTo(dest, ...args);
}

function readableStreamTee(stream) {
  return stream.tee();
}

export {
  ReadableStream,
  ReadableStreamDefaultReader,
  ReadableStreamBYOBReader,
  ReadableStreamBYOBRequest,
  ReadableByteStreamController,
  ReadableStreamDefaultController,
  isReadableStream,
  isReadableStreamLocked,
  isReadableStreamDefaultReader,
  isReadableStreamBYOBReader,
  isReadableStreamBYOBRequest,
  isReadableByteStreamController,
  isWritableStreamDefaultWriter,
  isWritableStreamDefaultController,
  readableStreamCancel,
  readableStreamPipeTo,
  readableStreamTee,
};

export default {
  ReadableStream,
  ReadableStreamDefaultReader,
  ReadableStreamBYOBReader,
  ReadableStreamBYOBRequest,
  ReadableByteStreamController,
  ReadableStreamDefaultController,
  isReadableStream,
  isReadableStreamLocked,
  isReadableStreamDefaultReader,
  isReadableStreamBYOBReader,
  isReadableStreamBYOBRequest,
  isReadableByteStreamController,
  isWritableStreamDefaultWriter,
  isWritableStreamDefaultController,
  readableStreamCancel,
  readableStreamPipeTo,
  readableStreamTee,
};
