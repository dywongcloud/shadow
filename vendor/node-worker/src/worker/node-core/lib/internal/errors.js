function createNodeError(code, BaseError = Error, messageBuilder = (...args) => `${code}: ${args.map(String).join(' ')}`) {
  class NodeError extends BaseError {
    constructor(...args) {
      super(messageBuilder(...args));
      this.code = code;
      this.name = `${this.constructor.name} [${code}]`;
    }
  }

  NodeError.HideStackFramesError = NodeError;
  return NodeError;
}

const errorSpecs = {
  __proto__: null,
  ERR_INVALID_ARG_TYPE: [TypeError, (name, expected, actual) => `The ${name} argument must be ${expected}; received ${actual}`],
  ERR_INVALID_ARG_VALUE: [TypeError, (name, value) => `The ${name} argument is invalid: ${value}`],
  ERR_INVALID_URL: [TypeError, (input) => `Invalid URL: ${input}`],
  ERR_INVALID_CURSOR_POS: [RangeError, () => 'Cursor position must be a finite number'],
  ERR_USE_AFTER_CLOSE: [Error, () => 'Readline was used after being closed'],
  ERR_ILLEGAL_CONSTRUCTOR: [TypeError, () => 'Illegal constructor'],
  ERR_METHOD_NOT_IMPLEMENTED: [Error, (name) => `${name} is not implemented`],
  ERR_OUT_OF_RANGE: [RangeError, (name, range, value) => `The value of ${name} is out of range. Expected ${range}; received ${value}`],
  ERR_UNKNOWN_ENCODING: [TypeError, (encoding) => `Unknown encoding: ${encoding}`],
  ERR_MULTIPLE_CALLBACK: [Error, () => 'Callback called multiple times'],
  ERR_STREAM_ALREADY_FINISHED: [Error, () => 'Stream already finished'],
  ERR_STREAM_CANNOT_PIPE: [Error, () => 'Cannot pipe, not readable'],
  ERR_STREAM_DESTROYED: [Error, (name = 'stream') => `${name} is destroyed`],
  ERR_STREAM_NULL_VALUES: [TypeError, () => 'May not write null values to stream'],
  ERR_STREAM_PREMATURE_CLOSE: [Error, () => 'Premature close'],
  ERR_STREAM_PUSH_AFTER_EOF: [Error, () => 'stream.push() after EOF'],
  ERR_STREAM_UNSHIFT_AFTER_END_EVENT: [Error, () => 'stream.unshift() after end event'],
  ERR_STREAM_WRITE_AFTER_END: [Error, () => 'write after end'],
  ERR_INVALID_RETURN_VALUE: [TypeError, (input, name, value) => `Expected ${input} to be returned from ${name}, got ${value}`],
  ERR_MISSING_ARGS: [TypeError, (...args) => `Missing required arguments: ${args.join(', ')}`],
  ERR_STREAM_UNABLE_TO_PIPE: [Error, () => 'Cannot pipe to this destination'],
  ERR_UNHANDLED_ERROR: [Error, (context) => `Unhandled error${context ? ` (${context})` : ''}`],
  ERR_INVALID_CHAR: [TypeError, (name, field = '') => `Invalid character in ${name}${field ? ` [${field}]` : ''}`],
  ERR_INVALID_HTTP_TOKEN: [TypeError, (name, value) => `${name} must be a valid HTTP token: ${value}`],
  ERR_INVALID_PROTOCOL: [TypeError, (protocol, expected) => `Protocol "${protocol}" not supported. Expected "${expected}"`],
  ERR_UNESCAPED_CHARACTERS: [TypeError, (name) => `${name} contains unescaped characters`],
  ERR_HTTP_HEADERS_SENT: [Error, (action) => `Cannot ${action} headers after they are sent to the client`],
  ERR_HTTP_INVALID_STATUS_CODE: [RangeError, (statusCode) => `Invalid status code: ${statusCode}`],
  ERR_HTTP_REQUEST_TIMEOUT: [Error, () => 'Request timeout'],
  ERR_HTTP_SOCKET_ASSIGNED: [Error, () => 'Socket is already assigned'],
  ERR_HTTP_SOCKET_ENCODING: [Error, () => 'Changing the socket encoding is not allowed'],
  ERR_HTTP_BODY_NOT_ALLOWED: [Error, () => 'Adding content for this request method or response status is not allowed'],
  ERR_HTTP_CONTENT_LENGTH_MISMATCH: [Error, (written, expected) => `Content-Length mismatch: wrote ${written}, expected ${expected}`],
  ERR_HTTP_INVALID_HEADER_VALUE: [TypeError, (value, name) => `Invalid value "${value}" for header "${name}"`],
  ERR_HTTP_TRAILER_INVALID: [Error, () => 'Trailers are invalid with this transfer encoding'],
  ERR_PROXY_INVALID_CONFIG: [TypeError, (value) => `Invalid proxy configuration: ${value}`],
  ERR_INVALID_STATE: [Error, (message = 'Invalid state') => message],
};

class AbortError extends Error {
  constructor(message = 'The operation was aborted', options = undefined) {
    super(message, options);
    this.code = 'ABORT_ERR';
    this.name = 'AbortError';
  }
}

class ConnResetException extends Error {
  constructor(message = 'socket hang up') {
    super(message);
    this.code = 'ECONNRESET';
    this.name = 'ConnResetException';
  }
}

function aggregateTwoErrors(first, second) {
  if (first == null) {
    return second;
  }
  if (second == null || first === second) {
    return first;
  }

  const error = second instanceof Error ? second : new Error(String(second));
  error.errors = [first, second];
  return error;
}

function genericNodeError(message, options = undefined) {
  const error = new Error(message, options);
  if (options?.code) {
    error.code = options.code;
  }
  return error;
}

// Upstream inspects the V8 stack-overflow error's message/frames; a message
// match is enough here. `internal/console/constructor.js` uses this to decide
// whether a synchronous write error is fatal (re-thrown) or swallowed.
function isStackOverflowError(err) {
  return err instanceof RangeError &&
    typeof err.message === 'string' &&
    err.message.includes('call stack');
}

function hideStackFrames(fn) {
  // Upstream attaches a `.withoutStackTrace` alias (the same fn, minus the
  // stack-trace bookkeeping) that callers invoke directly. We don't trim
  // stack frames, so it's just the function itself.
  fn.withoutStackTrace = fn;
  return fn;
}

// Build (and memoize) the NodeError class for `code`, using its spec when one
// exists and a generic Error otherwise. Single construction path for all codes.
const errorCache = { __proto__: null };
function lookupError(code) {
  let ctor = errorCache[code];
  if (ctor !== undefined) return ctor;
  const spec = errorSpecs[code];
  ctor = spec ? createNodeError(code, spec[0], spec[1]) : createNodeError(code);
  // ERR_INVALID_STATE additionally exposes a TypeError variant in upstream node.
  if (code === 'ERR_INVALID_STATE') {
    ctor.TypeError = createNodeError('ERR_INVALID_STATE', TypeError, (message = 'Invalid state') => message);
  }
  errorCache[code] = ctor;
  return ctor;
}

// `codes` resolves every `ERR_*` lookup through `lookupError` lazily, so the
// open-ended crypto/OpenSSL codes work without being enumerated, while the
// codes in `errorSpecs` keep their precise base class and message.
const codes = new Proxy(errorCache, {
  get(target, prop) {
    if (typeof prop === 'string' && prop.startsWith('ERR_')) {
      return lookupError(prop);
    }
    return target[prop];
  },
  has(_target, prop) {
    return typeof prop === 'string' && prop.startsWith('ERR_');
  },
});


/*
 * Where a module parks a custom stack formatter for one specific error.
 *
 * Upstream it is read by node's `prepareStackTrace`, which this runtime does not
 * install — V8 formats stacks itself here. So entries are written and never read,
 * and the cost is cosmetic: `lib/repl.js:1658` uses it to trim the REPL's own frames
 * off a stack trace, so an error thrown at the prompt shows the frames underneath it
 * as well as the user's. Nothing depends on the trimming having happened.
 */
const overrideStackTrace = new WeakMap();

/**
 * Whether `Error.stackTraceLimit` can be assigned to.
 *
 * Callers set it to 0 to collect an error without paying for a stack, then put it
 * back. V8 leaves it a plain writable property unless an embedder freezes it, and
 * nothing here does — but this checks rather than asserts, because a caller that
 * believes a frozen limit is writable throws in strict mode on the way past.
 */
function isErrorStackTraceLimitWritable() {
  const descriptor = Object.getOwnPropertyDescriptor(Error, 'stackTraceLimit');
  if (descriptor === undefined) return Object.isExtensible(Error);
  return Object.prototype.hasOwnProperty.call(descriptor, 'writable')
    ? descriptor.writable === true
    : descriptor.set !== undefined;
}

export {
  AbortError,
  ConnResetException,
  aggregateTwoErrors,
  codes,
  genericNodeError,
  hideStackFrames,
  isErrorStackTraceLimitWritable,
  isStackOverflowError,
  overrideStackTrace,
};

export default {
  AbortError,
  ConnResetException,
  aggregateTwoErrors,
  codes,
  genericNodeError,
  hideStackFrames,
  isErrorStackTraceLimitWritable,
  isStackOverflowError,
  overrideStackTrace,
};
