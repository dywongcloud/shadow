// `net.BlockList`, in pure JS.
//
// Upstream this is a thin wrapper over `internalBinding('block_list')`, which is C++, so there is
// no node_core implementation to reuse — it has to be written out. The API is small and fully
// specified, and real programs depend on it: a proxy layer typically builds one at module scope to
// recognise loopback addresses, and parses `NO_PROXY` CIDR entries into another. Both run
// unconditionally at import time, so its absence is a `TypeError: net.BlockList is not a
// constructor` before any of the surrounding logic gets a chance to be optional.
//
// Addresses are normalised to BigInt — 32-bit for v4, 128-bit for v6 — which makes every rule a
// range comparison and keeps subnet masking exact at 128 bits.

export type IPType = "ipv4" | "ipv6";

const V4_MAX = (1n << 32n) - 1n;
const V6_MAX = (1n << 128n) - 1n;
/** `::ffff:0:0/96` — where an IPv4 address sits when written as IPv4-mapped IPv6. */
const V4_MAPPED_PREFIX = 0xffffn << 32n;

function parseIPv4(address: string): bigint | null {
	const parts = address.split(".");
	if (parts.length !== 4) return null;
	let value = 0n;
	for (const part of parts) {
		if (!/^\d{1,3}$/.test(part)) return null;
		const octet = Number(part);
		if (octet > 255) return null;
		value = (value << 8n) | BigInt(octet);
	}
	return value;
}

function parseIPv6(address: string): bigint | null {
	let text = address;

	// A trailing IPv4 part (`::ffff:127.0.0.1`) is rewritten to two hex groups first.
	const lastColon = text.lastIndexOf(":");
	const tail = text.slice(lastColon + 1);
	if (tail.includes(".")) {
		const v4 = parseIPv4(tail);
		if (v4 === null) return null;
		const high = (v4 >> 16n) & 0xffffn;
		const low = v4 & 0xffffn;
		text = `${text.slice(0, lastColon + 1)}${high.toString(16)}:${low.toString(16)}`;
	}

	const doubleColon = text.indexOf("::");
	let head: string[];
	let rear: string[];
	if (doubleColon === -1) {
		head = text.split(":");
		rear = [];
		if (head.length !== 8) return null;
	} else {
		if (text.indexOf("::", doubleColon + 1) !== -1) return null;
		head = text.slice(0, doubleColon).split(":").filter((g) => g !== "");
		rear = text.slice(doubleColon + 2).split(":").filter((g) => g !== "");
		if (head.length + rear.length > 7) return null;
	}

	const groups = [...head, ...Array(8 - head.length - rear.length).fill("0"), ...rear];
	let value = 0n;
	for (const group of groups) {
		if (!/^[\da-fA-F]{1,4}$/.test(group)) return null;
		value = (value << 16n) | BigInt(Number.parseInt(group, 16));
	}
	return value;
}

function parse(address: string, type: IPType): bigint | null {
	if (typeof address !== "string") return null;
	return type === "ipv4" ? parseIPv4(address) : parseIPv6(address);
}

type Rule =
	| { kind: "address"; type: IPType; value: bigint }
	| { kind: "range"; type: IPType; start: bigint; end: bigint }
	| { kind: "subnet"; type: IPType; value: bigint; prefix: number; start: bigint; end: bigint };

function invalid(name: string, value: unknown): Error {
	const err = new TypeError(`The "${name}" argument is invalid. Received ${String(value)}`);
	(err as NodeJS.ErrnoException).code = "ERR_INVALID_ARG_VALUE";
	return err;
}

export class BlockList {
	#rules: Rule[] = [];

	static isBlockList(value: unknown): boolean {
		return value instanceof BlockList;
	}

	addAddress(address: string, type: IPType = "ipv4"): void {
		const value = parse(address, type);
		if (value === null) throw invalid("address", address);
		this.#rules.push({ kind: "address", type, value });
	}

	addRange(start: string, end: string, type: IPType = "ipv4"): void {
		const from = parse(start, type);
		const to = parse(end, type);
		if (from === null) throw invalid("start", start);
		if (to === null) throw invalid("end", end);
		// Node returns false rather than throwing when the range is inverted; matching that means a
		// bad rule is inert instead of fatal.
		if (to < from) return;
		this.#rules.push({ kind: "range", type, start: from, end: to });
	}

	addSubnet(network: string, prefix: number, type: IPType = "ipv4"): void {
		const value = parse(network, type);
		if (value === null) throw invalid("network", network);
		const width = type === "ipv4" ? 32 : 128;
		if (!Number.isInteger(prefix) || prefix < 0 || prefix > width) throw invalid("prefix", prefix);

		const full = type === "ipv4" ? V4_MAX : V6_MAX;
		// A /0 shift by the full width is undefined for fixed-width ints but well defined for BigInt;
		// the mask below is exact either way.
		const mask = full ^ ((1n << BigInt(width - prefix)) - 1n);
		const start = value & mask;
		const end = start | (full ^ mask);
		this.#rules.push({ kind: "subnet", type, value, prefix, start, end });
	}

	check(address: string, type: IPType = "ipv4"): boolean {
		let value = parse(address, type);
		if (value === null) return false;

		for (const rule of this.#rules) {
			if (this.#matches(rule, value, type)) return true;
		}

		// An IPv4-mapped IPv6 address is also the IPv4 address it embeds, and node checks both. This
		// is what makes `addSubnet("127.0.0.0", 8, "ipv4")` recognise `::ffff:127.0.0.1`.
		if (type === "ipv6" && (value >> 32n) === V4_MAPPED_PREFIX) {
			const embedded = value & V4_MAX;
			for (const rule of this.#rules) {
				if (this.#matches(rule, embedded, "ipv4")) return true;
			}
		}
		return false;
	}

	#matches(rule: Rule, value: bigint, type: IPType): boolean {
		if (rule.type !== type) return false;
		if (rule.kind === "address") return rule.value === value;
		return value >= rule.start && value <= rule.end;
	}

	/** Node exposes the rules as strings, in the same spellings it accepts. */
	get rules(): string[] {
		return this.#rules.map((rule) => {
			const format = (value: bigint) => (rule.type === "ipv4" ? formatIPv4(value) : formatIPv6(value));
			if (rule.kind === "address") return `Address: ${rule.type.toUpperCase()} ${format(rule.value)}`;
			if (rule.kind === "range") {
				return `Range: ${rule.type.toUpperCase()} ${format(rule.start)}-${format(rule.end)}`;
			}
			return `Subnet: ${rule.type.toUpperCase()} ${format(rule.value)}/${rule.prefix}`;
		});
	}
}

function formatIPv4(value: bigint): string {
	return [24n, 16n, 8n, 0n].map((shift) => Number((value >> shift) & 0xffn)).join(".");
}

function formatIPv6(value: bigint): string {
	const groups: string[] = [];
	for (let i = 7n; i >= 0n; i--) {
		groups.push((((value >> (i * 16n)) & 0xffffn) as bigint).toString(16));
	}
	return groups.join(":");
}
