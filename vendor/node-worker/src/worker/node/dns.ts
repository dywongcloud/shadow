function unsupported(name: string) {
	return () => {
		throw new Error(`node:dns.${name} is not supported in this runtime`);
	};
}

let defaultResultOrder: "verbatim" | "ipv4first" | "ipv6first" = "verbatim";

function getDefaultResultOrder() {
	return defaultResultOrder;
}

function setDefaultResultOrder(order: typeof defaultResultOrder) {
	if (
		order !== "ipv4first" &&
		order !== "verbatim" &&
		order !== "ipv6first"
	) {
		throw new TypeError(`Invalid argument "order" ${order}`);
	}
	defaultResultOrder = order;
}

function getServers(): string[] {
	return [];
}

function setServers(_servers: string[]) {}

interface LookupResult {
	address: string;
	family: 4 | 6;
}

function lookupHostname(hostname: string): LookupResult | null {
	if (hostname === "localhost" || hostname === "127.0.0.1") {
		return { address: "127.0.0.1", family: 4 };
	}
	if (hostname === "::1") {
		return { address: "::1", family: 6 };
	}
	return null;
}

function makeNotFoundError(hostname: string) {
	const err = new Error(`getaddrinfo ENOTFOUND ${hostname}`) as Error & {
		code: string;
		errno: number;
		syscall: string;
		hostname: string;
	};
	err.code = "ENOTFOUND";
	err.errno = -3008;
	err.syscall = "getaddrinfo";
	err.hostname = hostname;
	return err;
}

function lookup(hostname: string, options: any, callback?: any) {
	if (typeof options === "function") {
		callback = options;
		options = undefined;
	}
	if (typeof callback !== "function") {
		throw new TypeError("callback must be a function");
	}

	const resolved = lookupHostname(hostname);
	if (resolved) {
		queueMicrotask(() => {
			if (options && options.all) callback(null, [resolved]);
			else callback(null, resolved.address, resolved.family);
		});
		return;
	}

	queueMicrotask(() => callback(makeNotFoundError(hostname)));
}

function lookupPromise(hostname: string, options?: any) {
	const resolved = lookupHostname(hostname);
	if (!resolved) return Promise.reject(makeNotFoundError(hostname));
	if (options && options.all) return Promise.resolve([resolved]);
	return Promise.resolve(resolved);
}

class Resolver {
	constructor() {
		throw new Error("node:dns.Resolver is not supported in this runtime");
	}
}

const errorCodes = {
	NODATA: "ENODATA",
	FORMERR: "EFORMERR",
	SERVFAIL: "ESERVFAIL",
	NOTFOUND: "ENOTFOUND",
	NOTIMP: "ENOTIMP",
	REFUSED: "EREFUSED",
	BADQUERY: "EBADQUERY",
	BADNAME: "EBADNAME",
	BADFAMILY: "EBADFAMILY",
	BADRESP: "EBADRESP",
	CONNREFUSED: "ECONNREFUSED",
	TIMEOUT: "ETIMEOUT",
	EOF: "EOF",
	FILE: "EFILE",
	NOMEM: "ENOMEM",
	DESTRUCTION: "EDESTRUCTION",
	BADSTR: "EBADSTR",
	BADFLAGS: "EBADFLAGS",
	NONAME: "ENONAME",
	BADHINTS: "EBADHINTS",
	NOTINITIALIZED: "ENOTINITIALIZED",
	LOADIPHLPAPI: "ELOADIPHLPAPI",
	ADDRGETNETWORKPARAMS: "EADDRGETNETWORKPARAMS",
	CANCELLED: "ECANCELLED",
};

const flags = {
	ADDRCONFIG: 32,
	V4MAPPED: 8,
	ALL: 16,
};

const promises = {
	...flags,
	...errorCodes,
	Resolver,
	getDefaultResultOrder,
	setDefaultResultOrder,
	getServers,
	setServers,
	lookup: lookupPromise,
	lookupService: unsupported("lookupService"),
	resolve: unsupported("resolve"),
	resolve4: unsupported("resolve4"),
	resolve6: unsupported("resolve6"),
	resolveAny: unsupported("resolveAny"),
	resolveCname: unsupported("resolveCname"),
	resolveCaa: unsupported("resolveCaa"),
	resolveMx: unsupported("resolveMx"),
	resolveNaptr: unsupported("resolveNaptr"),
	resolveNs: unsupported("resolveNs"),
	resolvePtr: unsupported("resolvePtr"),
	resolveSoa: unsupported("resolveSoa"),
	resolveSrv: unsupported("resolveSrv"),
	resolveTxt: unsupported("resolveTxt"),
	reverse: unsupported("reverse"),
};

const dns = {
	...flags,
	...errorCodes,
	Resolver,
	getDefaultResultOrder,
	setDefaultResultOrder,
	getServers,
	setServers,
	lookup,
	lookupService: unsupported("lookupService"),
	resolve: unsupported("resolve"),
	resolve4: unsupported("resolve4"),
	resolve6: unsupported("resolve6"),
	resolveAny: unsupported("resolveAny"),
	resolveCname: unsupported("resolveCname"),
	resolveCaa: unsupported("resolveCaa"),
	resolveMx: unsupported("resolveMx"),
	resolveNaptr: unsupported("resolveNaptr"),
	resolveNs: unsupported("resolveNs"),
	resolvePtr: unsupported("resolvePtr"),
	resolveSoa: unsupported("resolveSoa"),
	resolveSrv: unsupported("resolveSrv"),
	resolveTxt: unsupported("resolveTxt"),
	reverse: unsupported("reverse"),
	promises,
};

export default dns as unknown as typeof import("node:dns");
