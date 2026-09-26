class AssertionError extends Error {
	constructor(opts = {}) {
		super(opts.message || 'Assertion failed');
		this.name = 'AssertionError';
		this.actual = opts.actual;
		this.expected = opts.expected;
		this.operator = opts.operator;
		this.generatedMessage = !opts.message;
		this.code = 'ERR_ASSERTION';
	}
}

function assert(value, message) {
	if (!value) {
		throw new AssertionError({
			message: message || `'${value}' == true`,
			actual: value,
			expected: true,
			operator: '==',
		});
	}
}

function ok(value, message) {
	assert(value, message);
}

function equal(actual, expected, message) {
	if (actual != expected) {
		throw new AssertionError({
			message: message || `${actual} == ${expected}`,
			actual,
			expected,
			operator: '==',
		});
	}
}

function notEqual(actual, expected, message) {
	if (actual == expected) {
		throw new AssertionError({
			message: message || `${actual} != ${expected}`,
			actual,
			expected,
			operator: '!=',
		});
	}
}

function strictEqual(actual, expected, message) {
	if (!Object.is(actual, expected)) {
		throw new AssertionError({
			message: message || `${actual} === ${expected}`,
			actual,
			expected,
			operator: '===',
		});
	}
}

function notStrictEqual(actual, expected, message) {
	if (Object.is(actual, expected)) {
		throw new AssertionError({
			message: message || `${actual} !== ${expected}`,
			actual,
			expected,
			operator: '!==',
		});
	}
}

function deepEqual(actual, expected, message) {
	if (JSON.stringify(actual) !== JSON.stringify(expected)) {
		throw new AssertionError({
			message: message || 'deepEqual failed',
			actual,
			expected,
			operator: 'deepEqual',
		});
	}
}

const deepStrictEqual = deepEqual;
const notDeepEqual = (a, e, m) => {
	try {
		deepEqual(a, e, m);
	} catch {
		return;
	}
	throw new AssertionError({ message: m || 'notDeepEqual failed' });
};
const notDeepStrictEqual = notDeepEqual;

function fail(message) {
	throw new AssertionError({ message: message || 'Failed' });
}

function throws(fn, error, message) {
	let thrown = false;
	try {
		fn();
	} catch (e) {
		thrown = true;
	}
	if (!thrown) {
		throw new AssertionError({
			message: message || 'Expected function to throw',
			operator: 'throws',
		});
	}
}

function doesNotThrow(fn, _error, message) {
	try {
		fn();
	} catch (e) {
		throw new AssertionError({
			message: message || `Expected not to throw: ${e}`,
			operator: 'doesNotThrow',
		});
	}
}

async function rejects(asyncFn, _error, message) {
	let rejected = false;
	try {
		await (typeof asyncFn === 'function' ? asyncFn() : asyncFn);
	} catch {
		rejected = true;
	}
	if (!rejected) {
		throw new AssertionError({
			message: message || 'Expected promise to reject',
			operator: 'rejects',
		});
	}
}

async function doesNotReject(asyncFn, _error, message) {
	try {
		await (typeof asyncFn === 'function' ? asyncFn() : asyncFn);
	} catch (e) {
		throw new AssertionError({
			message: message || `Expected promise to not reject: ${e}`,
			operator: 'doesNotReject',
		});
	}
}

function ifError(value) {
	if (value !== null && value !== undefined) {
		throw value;
	}
}

function match(actual, regex, message) {
	if (!regex.test(actual)) {
		throw new AssertionError({
			message: message || `${actual} did not match ${regex}`,
			actual,
			expected: regex,
			operator: 'match',
		});
	}
}

function doesNotMatch(actual, regex, message) {
	if (regex.test(actual)) {
		throw new AssertionError({
			message: message || `${actual} matched ${regex}`,
			actual,
			expected: regex,
			operator: 'doesNotMatch',
		});
	}
}

const strict = Object.assign(assert, {
	AssertionError,
	ok,
	equal: strictEqual,
	notEqual: notStrictEqual,
	deepEqual: deepStrictEqual,
	notDeepEqual: notDeepStrictEqual,
	strictEqual,
	notStrictEqual,
	deepStrictEqual,
	notDeepStrictEqual,
	fail,
	throws,
	doesNotThrow,
	rejects,
	doesNotReject,
	ifError,
	match,
	doesNotMatch,
});

Object.assign(assert, {
	AssertionError,
	ok,
	equal,
	notEqual,
	deepEqual,
	notDeepEqual,
	strictEqual,
	notStrictEqual,
	deepStrictEqual,
	notDeepStrictEqual,
	fail,
	throws,
	doesNotThrow,
	rejects,
	doesNotReject,
	ifError,
	match,
	doesNotMatch,
	strict,
});

export {
	AssertionError,
	ok,
	equal,
	notEqual,
	deepEqual,
	notDeepEqual,
	strictEqual,
	notStrictEqual,
	deepStrictEqual,
	notDeepStrictEqual,
	fail,
	throws,
	doesNotThrow,
	rejects,
	doesNotReject,
	ifError,
	match,
	doesNotMatch,
	strict,
};

export default assert;
