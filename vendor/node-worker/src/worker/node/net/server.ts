// @ts-nocheck

import events from "../events";
import { Socket } from "./socket";
import { hostPeerServer } from "../../peer";
import * as keepalive from "../../keepalive";

const EventEmitter = events.EventEmitter;

/**
 * Servers listening in *this* worker, by port.
 *
 * The signaller is the address book for everyone else; this is the one case it should not be
 * asked about. Two processes in the same worker reaching each other through a signaller, ICE
 * and a TURN relay is a round trip through the internet to talk to yourself — and until the
 * client half learned to look here first, it was worse than that: `localhost` went to the Wisp
 * relay, which resolved it on the *relay host*, so the connection did not arrive at all.
 *
 * A fast path only. It must not change who is reachable: `listen` still registers with the
 * signaller, because that is what lets another Puter app reach this port.
 */
const LOCAL_SERVERS = new Map<number, any>();

/**
 * A port nobody in this worker is using, for `listen(0)`.
 *
 * Only the local map can be consulted — the signaller's registry is per-credential and not
 * enumerable from here — so this is "ephemeral" in the node sense of an unused high port, not
 * a guarantee against a collision with another app holding the same anonToken.
 */
function ephemeralPort(): number {
	for (let i = 0; i < 4096; i++) {
		let port = 49152 + Math.floor(Math.random() * 16383);
		if (!LOCAL_SERVERS.has(port)) return port;
	}
	throw codedError("no free port", "EADDRINUSE");
}

/** A server in this worker listening on `port`, if there is one. */
export function localServer(port: number): any | undefined {
	return LOCAL_SERVERS.get(port);
}

function codedError(message: string, code: string): Error {
	let e = new Error(message);
	(e as any).code = code;
	return e;
}

function Server(this: any, options?: any, connectionListener?: any) {
	if (!(this instanceof Server))
		return new (Server as any)(options, connectionListener);
	EventEmitter.call(this);

	if (typeof options === "function") {
		connectionListener = options;
		options = {};
	} else {
		options = options ?? {};
	}

	this._connections = 0;
	this.allowHalfOpen = options.allowHalfOpen || false;
	this.pauseOnConnect = !!options.pauseOnConnect;
	this.maxConnections = undefined; // unset => unlimited

	// Never used, but some consumers probe `_handle` to detect a bound handle.
	this._handle = null;

	this._listening = false;
	this._starting = false;
	this._closing = false;
	this._code = undefined;
	this._port = undefined;
	this._peerClose = undefined;

	// Keepalive discipline mirroring socket.ts: `_active` is the listening
	// handle's open state, `_refed` the user ref/unref flag (refed by default),
	// `_keepaliveRefed` the current contribution so ref/unref stay balanced.
	this._refed = true;
	this._active = false;
	this._keepaliveRefed = false;

	if (typeof connectionListener === "function")
		this.on("connection", connectionListener);
}

Object.setPrototypeOf(Server.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Server, EventEmitter);

Server.prototype._syncKeepalive = function () {
	let want = this._active && this._refed;
	if (want === this._keepaliveRefed) return;
	this._keepaliveRefed = want;
	if (want) keepalive.ref();
	else keepalive.unref();
};

// Accept `listen(port?, host?, backlog?, cb?)` and `listen(options, cb?)`.
// IPC/pipe (`path`) is unsupported, matching Socket. Returns { port, cb }.
function parseListenArgs(args: any[]): { port: number; cb?: () => void } {
	let cb: (() => void) | undefined;
	if (args.length && typeof args[args.length - 1] === "function") {
		cb = args[args.length - 1];
		args = args.slice(0, -1);
	}

	let first = args[0];
	let port: number | undefined;

	if (typeof first === "object" && first !== null) {
		if ("path" in first && first.path !== undefined)
			throw new Error("unsupported");
		if ("fd" in first && first.fd !== undefined) throw new Error("unsupported");
		port = first.port;
	} else if (typeof first === "string") {
		// A bare string is an IPC path.
		throw new Error("unsupported");
	} else {
		port = first;
	}

	if (port === undefined || port === null) port = 0;
	port = Number(port);
	if (!Number.isInteger(port) || port < 0 || port > 65535)
		throw new RangeError("port must be an integer between 0 and 65535");

	return { port, cb };
}

Server.prototype.listen = function (...args: any[]) {
	if (this._listening || this._starting)
		throw codedError("Server is already listening", "ERR_SERVER_ALREADY_LISTEN");

	let { port, cb } = parseListenArgs(args);
	if (cb) this.once("listening", cb);

	// node picks one when asked for 0, and `address()` has to report what it picked.
	if (port === 0) port = ephemeralPort();
	if (LOCAL_SERVERS.has(port)) {
		throw codedError(
			`listen EADDRINUSE: address already in use :::${port}`,
			"EADDRINUSE"
		);
	}

	this._starting = true;
	this._port = port;
	/*
	 * Registered now, not when the signaller answers. `listen` is synchronous in node, and a
	 * client in this same worker can call `connect` on the next line — which is exactly what a
	 * test fixture does. Waiting for two Puter API round trips first would make that a race.
	 */
	LOCAL_SERVERS.set(port, this);

	// Ref *now*, not when the peer server finishes coming up. node's listen()
	// creates a refed handle synchronously, so the loop is alive from the call;
	// here bringing the server up is asynchronous (two puter API round trips for
	// signaller/ICE plus a host round trip to open the peer server), and leaving
	// the count at zero for that window lets drain() settle the run while a server
	// is nominally listening. Same discipline as Socket#_beginConnect, which refs
	// before awaiting its connect. `close()` during startup clears `_active`.
	this._active = true;
	this._syncKeepalive();

	hostPeerServer(port, (pair) => this._onAccept(pair))
		.then(({ code, close }) => {
			this._starting = false;
			// A `close()` may have raced in while we were starting.
			if (this._closing) {
				close();
				return;
			}
			this._listening = true;
			this._code = code;
			this._peerClose = close;
			this._active = true;
			this._syncKeepalive();
			this.emit("listening");
		})
		.catch((_e) => {
			let e = _e instanceof Error ? _e : new Error(String(_e));
			this._starting = false;
			/*
			 * Failing to *publish* is not failing to listen.
			 *
			 * hostPeerServer needs a puter token or a peer token, and without either the
			 * signaller is simply unavailable — which is an ordinary configuration, not an
			 * error in this call. The port is still bound as far as this machine is concerned:
			 * it is in the local registry, and a client in this worker reaches it without the
			 * signaller ever being involved. Emitting `error` here would take down a server
			 * that works, and `listen` has no other way to say "bound, but only here".
			 *
			 * So it listens, and `_peerError` records why nobody else can reach it.
			 */
			this._peerError = e;
			this._listening = true;
			this._active = true;
			this._syncKeepalive();
			this.emit("listening");
		});

	return this;
};

Server.prototype._onAccept = function ([readable, writable]: [
	ReadableStream<Uint8Array>,
	WritableStream<Uint8Array>,
]) {
	if (!this._listening) {
		readable.cancel().catch(() => {});
		writable.abort().catch(() => {});
		return;
	}

	if (this.maxConnections != null && this._connections >= this.maxConnections) {
		readable.cancel().catch(() => {});
		writable.abort().catch(() => {});
		this.emit("drop", { localPort: this._port });
		return;
	}

	let socket = new (Socket as any)({ allowHalfOpen: this.allowHalfOpen });
	// The peer transport gives no per-connection remote address; use a neutral
	// placeholder. (The addressing scheme is handled on the epoxy client side.)
	socket._acceptStreams("0.0.0.0", this._port, readable, writable);
	socket.server = this;
	socket._server = this;

	this._connections++;
	socket.once("close", () => {
		this._connections--;
		this._emitCloseIfDrained();
	});

	if (this.pauseOnConnect) socket.pause();

	this.emit("connection", socket);
};

Server.prototype.close = function (cb?: (err?: Error) => void) {
	if (typeof cb === "function") {
		if (!this._listening && !this._starting) {
			let e = codedError("Server is not running.", "ERR_SERVER_NOT_RUNNING");
			queueMicrotask(() => cb(e));
		} else {
			this.once("close", cb);
		}
	}

	if (this._port != null && LOCAL_SERVERS.get(this._port) === this) {
		LOCAL_SERVERS.delete(this._port);
	}

	if (this._listening || this._starting) {
		this._listening = false;
		this._closing = true;
		if (this._peerClose) this._peerClose();
		this._peerClose = undefined;
		this._active = false;
		this._syncKeepalive();
	}

	this._emitCloseIfDrained();
	return this;
};

// Emit `close` once the server is closed AND all connections have drained
// (node semantics: close() does not tear down existing connections).
Server.prototype._emitCloseIfDrained = function () {
	if (!this._closing || this._listening || this._connections > 0) return;
	this._closing = false;
	queueMicrotask(() => this.emit("close"));
};

Server.prototype.address = function () {
	if (!this._listening) return null;
	/*
	 * Loopback, not 0.0.0.0. Nothing here binds an interface — there are none — and the address
	 * that actually works is the one a client can hand to `connect`, which is this one.
	 */
	return {
		port: this._port,
		family: "IPv4",
		address: "127.0.0.1",
	};
};

Server.prototype.getConnections = function (
	cb: (err: Error | null, count: number) => void
) {
	queueMicrotask(() => cb(null, this._connections));
	return this;
};

Server.prototype.ref = function () {
	this._refed = true;
	this._syncKeepalive();
	return this;
};

Server.prototype.unref = function () {
	this._refed = false;
	this._syncKeepalive();
	return this;
};

Object.defineProperty(Server.prototype, "listening", {
	get() {
		return this._listening;
	},
	configurable: true,
});

export { Server };
