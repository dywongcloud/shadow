// @ts-nocheck
import { Socket } from "./socket";
import { BlockList } from "./blocklist";
import { Server } from "./server";

type NodeNet = typeof import("node:net");

let autoSelectFamily = true;
let autoSelectFamilyAttemptTimeout = 250;
const normalizedArgsSymbol = Symbol("normalizedArgs");

function normalizeArgs(args: any[]) {
	let arr: [any, any];

	if (args.length === 0) {
		arr = [{}, null];
		(arr as any)[normalizedArgsSymbol] = true;
		return arr;
	}

	const arg0 = args[0];
	let options: Record<string, any> = {};
	if (typeof arg0 === "object" && arg0 !== null) {
		options = arg0;
	} else if (typeof arg0 === "string") {
		options.path = arg0;
	} else {
		options.port = arg0;
		if (typeof args[1] === "string") {
			options.host = args[1];
		}
	}

	const cb = args[args.length - 1];
	arr = typeof cb === "function" ? [options, cb] : [options, null];
	(arr as any)[normalizedArgsSymbol] = true;
	return arr;
}

const nodeNet = {
	Socket,
	Server,
	_normalizeArgs: normalizeArgs,
	createServer(
		options?: import("node:net").ServerOpts | ((socket: InstanceType<typeof Socket>) => void),
		connectionListener?: (socket: InstanceType<typeof Socket>) => void
	) {
		return new (Server as any)(options as any, connectionListener);
	},
	connect(...args: any[]) {
		let socket = new Socket() as any;
		return socket.connect(...args);
	},
	createConnection(...args: any[]): any {
		return (nodeNet as any).connect(...args);
	},
	BlockList,
	isIP(input) {
		// Through the module object, not `this`: `const { isIP } = require("net")` detaches it. This
		// object cannot have every member blanket-bound the way fs's can, because it also exports
		// classes (Socket, Server, BlockList) and binding a class strips its prototype and statics.
		if (nodeNet.isIPv4(input)) return 4;
		if (nodeNet.isIPv6(input)) return 6;
		return 0;
	},
	isIPv4(input) {
		let parts = input.split(".");
		if (parts.length !== 4) return false;

		for (let part of parts) {
			if (!/^\d+$/.test(part)) return false;
			if (part.length > 1 && part.startsWith("0")) return false;

			let n = Number(part);
			if (!Number.isInteger(n) || n < 0 || n > 255) return false;
		}

		return true;
	},
	isIPv6(input) {
		if (!input.includes(":")) return false;
		if ((input.match(/::/g) || []).length > 1) return false;

		let chunks = input.split(":");
		if (input.includes("::")) {
			if (chunks.length > 8) return false;
		} else if (chunks.length !== 8) {
			return false;
		}

		for (let chunk of chunks) {
			if (!chunk.length) continue;
			if (chunk.length > 4) return false;
			if (!/^[\da-fA-F]+$/.test(chunk)) return false;
		}

		return true;
	},
	getDefaultAutoSelectFamily() {
		return autoSelectFamily;
	},
	setDefaultAutoSelectFamily(value: boolean) {
		autoSelectFamily = !!value;
	},
	getDefaultAutoSelectFamilyAttemptTimeout() {
		return autoSelectFamilyAttemptTimeout;
	},
	setDefaultAutoSelectFamilyAttemptTimeout(value: number) {
		autoSelectFamilyAttemptTimeout = Math.max(10, Number(value) || 10);
	},
} as unknown as NodeNet & { _normalizeArgs: typeof normalizeArgs };

export default nodeNet;
