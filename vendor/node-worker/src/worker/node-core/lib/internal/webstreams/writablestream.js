const WritableStream = globalThis.WritableStream;
const WritableStreamDefaultWriter = globalThis.WritableStreamDefaultWriter;
const WritableStreamDefaultController = globalThis.WritableStreamDefaultController;

function isWritableStream(value) {
  return value instanceof WritableStream;
}

function isWritableStreamLocked(stream) {
  return stream.locked;
}

async function writableStreamClose(stream) {
  await stream.close();
}

async function writableStreamAbort(stream, reason) {
  await stream.abort(reason);
}

export {
  WritableStream,
  WritableStreamDefaultWriter,
  WritableStreamDefaultController,
  isWritableStream,
  isWritableStreamLocked,
  writableStreamClose,
  writableStreamAbort,
};

export default {
  WritableStream,
  WritableStreamDefaultWriter,
  WritableStreamDefaultController,
  isWritableStream,
  isWritableStreamLocked,
  writableStreamClose,
  writableStreamAbort,
};
