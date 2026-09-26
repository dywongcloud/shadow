// Tiny `internal/assert` shim. Upstream uses this for invariants ("the
// runtime should never reach this point") rather than user-facing assertion
// messages, so a bare throw is sufficient.

class AssertionError extends Error {
  constructor(message) {
    super(message);
    this.name = 'AssertionError';
    this.code = 'ERR_INTERNAL_ASSERTION';
  }
}

function assert(value, message) {
  if (!value) {
    throw new AssertionError(message ?? 'Assertion failed');
  }
}

assert.fail = (message) => {
  throw new AssertionError(message ?? 'Assertion failed');
};
assert.ok = assert;
assert.AssertionError = AssertionError;

export default assert;
