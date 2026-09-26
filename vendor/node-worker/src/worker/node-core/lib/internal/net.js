const IPv4Pattern = /^(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(?:\.(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;
const IPv6Pattern = /:/;

export function isIPv4(input) {
	return IPv4Pattern.test(input);
}

export function isIPv6(input) {
	if (!IPv6Pattern.test(input)) {
		return false;
	}
	if ((input.match(/::/g) || []).length > 1) {
		return false;
	}
	const pieces = input.split(':');
	if (!input.includes('::') && pieces.length !== 8) {
		return false;
	}
	if (input.includes('::') && pieces.length > 8) {
		return false;
	}
	return pieces.every((piece) => piece === '' || /^[\da-fA-F]{1,4}$/.test(piece));
}

export function isIP(input) {
	if (isIPv4(input)) return 4;
	if (isIPv6(input)) return 6;
	return 0;
}

export default {
	kReinitializeHandle: Symbol('kReinitializeHandle'),
	kSetNoDelay: Symbol('kSetNoDelay'),
	kSetKeepAlive: Symbol('kSetKeepAlive'),
	kSetKeepAliveInitialDelay: Symbol('kSetKeepAliveInitialDelay'),
	normalizedArgsSymbol: Symbol('normalizedArgs'),
	isIP,
	isIPv4,
	isIPv6,
	isLoopback(host) {
		const value = String(host).toLowerCase();
		return value === 'localhost' || value.startsWith('127.') || value === '[::1]' || value === '[0:0:0:0:0:0:0:1]';
	},
};
