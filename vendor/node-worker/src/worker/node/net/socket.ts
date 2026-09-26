import nodeStream from "../stream";
import nodeBuffer from "../buffer";
import { getClient } from "../../epoxy";
import { connectToPeer } from "../../peer";
import { localServer } from "./server";
import * as keepalive from "../../keepalive";

/** The names for "this machine". Nothing here binds an interface; these are the addresses. */
const LOOPBACK = new Set(["localhost", "127.0.0.1", "::1", ""]);

function codedError(message: string, code: string): Error {
	let e = new Error(message);
	(e as any).code = code;
	return e;
}

/**
 * Reach a server listening on `port`, without leaving the machine.
 *
 * `localhost:port` is a Puter-wide address: a server registers with the signaller under
 * `(credential, port)`, so another app holding the same anonToken reaches it by dialing that
 * port. This resolves it in the order that costs least.
 *
 * 1. A server in *this* worker. Two `TransformStream`s wired crosswise and handed straight to
 *    its `_onAccept` — no signaller, no ICE, no relay, and backpressure for free from the
 *    streams. Talking to yourself should not be a round trip through the internet.
 * 2. Another app's, over WebRTC, by port rather than by invite code — a code only exists for
 *    an authenticated server, and the port is the address that always does.
 * 3. Nothing there: ECONNREFUSED.
 *
 * Never the Wisp relay, which is what used to happen: `connect` handed it the string
 * "localhost", and it resolved that on the *relay host*.
 */
async function connectLoopback(
	port: number
): Promise<{
	read: ReadableStream<Uint8Array>;
	write: WritableStream<Uint8Array>;
}> {
	let server = localServer(port);
	if (server) {
		// `listen` registers synchronously but only becomes `listening` once the signaller has
		// answered, and `_onAccept` drops a connection before then. Waiting is the honest
		// reading of "the server is coming up"; the alternative is a connection refused by a
		// server that is about to exist.
		if (server._starting) {
			await new Promise<void>((resolve, reject) => {
				server.once("listening", resolve);
				server.once("error", reject);
			}).catch(() => {});
		}
		if (server._listening) {
			let toServer = new TransformStream<Uint8Array, Uint8Array>();
			let toClient = new TransformStream<Uint8Array, Uint8Array>();
			server._onAccept([toServer.readable, toClient.writable]);
			return { read: toClient.readable, write: toServer.writable };
		}
	}

	try {
		let [readable, writable] = await connectToPeer({ port });
		return { read: readable, write: writable };
	} catch {
		throw codedError(`connect ECONNREFUSED 127.0.0.1:${port}`, "ECONNREFUSED");
	}
}
let Buffer = nodeBuffer.Buffer;

type NodeNet = typeof import("node:net");
type SocketOpts = import("node:net").SocketConstructorOpts;

type ConnectOptions = import("node:net").SocketConnectOpts;
type TcpConnectOptions = import("node:net").TcpSocketConnectOpts;
type IpcConnectOptions = import("node:net").IpcSocketConnectOpts;

export let Socket: NodeNet["Socket"] = class Socket extends nodeStream.Duplex {
	#read: (buf: Uint8Array) => void;
	#reader?: ReadableStreamDefaultReader<Uint8Array>;
	#writer?: WritableStreamDefaultWriter<Uint8Array>;
	#connectPromise?: Promise<void>;
	#bytesRead = 0;
	#bytesWritten = 0;
	#host?: string;
	#port?: number;
	#connecting = false;
	#pending = true;
	#bufferSize = 0;

	// Keepalive: a connecting/connected socket is a refed active handle in node,
	// keeping the loop alive until it closes. `#active` is the handle's open
	// state, `#refed` the user ref/unref flag (refed by default), and
	// `#keepaliveRefed` the current contribution so ref/unref stay balanced.
	#refed = true;
	#active = false;
	#keepaliveRefed = false;

	// `close` is documented to carry whether the socket died of a transmission error, and
	// callers branch on it (an http agent retries differently on a clean close). The
	// generic Duplex emits `close` with no arguments, so the flag is recorded in
	// `_destroy` and filled in below rather than taking emission over from the stream
	// layer, which is what orders `error` before `close` in the first place.
	#hadError = false;

	constructor(options?: SocketOpts) {
		super({ allowHalfOpen: options?.allowHalfOpen ?? false });
		if (!options) options = {};

		// options.readable and options.writable ignored
		if (options.fd) throw new Error("unsupported");
		if (options.blockList) throw new Error("unsupported");

		if (options.signal) {
			if (options.signal.aborted) {
				queueMicrotask(() => {
					this.destroy(new Error("Socket operation was aborted"));
				});
			} else {
				options.signal.addEventListener(
					"abort",
					() => {
						this.destroy(new Error("Socket operation was aborted"));
					},
					{ once: true }
				);
			}
		}

		if (options.onread) {
			let _target: Buffer | Uint8Array;
			if (typeof options.onread.buffer == "function") {
				_target = options.onread.buffer();
			} else {
				_target = options.onread.buffer;
			}
			let target = new Uint8Array(
				_target.buffer,
				_target.byteOffset,
				_target.byteLength
			);
			let cb = options.onread.callback;
			if (!target.byteLength) throw new Error("onread buffer cannot be empty");

			this.#read = (buf) => {
				while (buf.byteLength) {
					let split = buf.subarray(0, target.byteLength);
					buf = buf.subarray(target.byteLength);
					target.set(split);
					let shouldContinue = cb(split.byteLength, target);
					if (shouldContinue === false) this.pause();
				}
			};
		} else {
			this.#read = (buf) => {
				this.push(Buffer.from(buf));
			};
		}
	}

	emit(event: string | symbol, ...args: unknown[]): boolean {
		if (event === "close" && args.length === 0) args = [this.#hadError];
		return super.emit(event, ...args);
	}

	#syncKeepalive() {
		let want = this.#active && this.#refed;
		if (want === this.#keepaliveRefed) return;
		this.#keepaliveRefed = want;
		if (want) keepalive.ref();
		else keepalive.unref();
	}

	#attach(
		host: string,
		port: number,
		readable: ReadableStream<Uint8Array>,
		writable: WritableStream<Uint8Array>
	) {
		this.#host = host;
		this.#port = port;
		this.#reader = readable.getReader();
		this.#writer = writable.getWriter();
		this.#connecting = false;
		this.#pending = false;
	}

	_acceptStreams(
		host: string,
		port: number,
		readable: ReadableStream<Uint8Array>,
		writable: WritableStream<Uint8Array>
	) {
		this.#attach(host, port, readable, writable);
		this.#active = true;
		this.#syncKeepalive();

		queueMicrotask(() => {
			this.emit("connect");
			this.emit("ready");
		});

		void this.#pumpRead();
	}

	// Shared connect path for plain TCP (net.Socket) and TLS (tls.TLSSocket).
	// `open` resolves to the underlying byte streams once the (optionally
	// encrypted) connection is established; `onSecure` runs after `connect`/
	// `ready` so a subclass can emit `secureConnect`. Assigning `#connectPromise`
	// is what lets writes issued before the connection completes get buffered by
	// `_write` instead of throwing "Socket is not connected".
	_beginConnect(
		host: string,
		port: number,
		open: () => Promise<{
			read: ReadableStream<Uint8Array>;
			write: WritableStream<Uint8Array>;
		}>,
		onSecure?: () => void
	) {
		this.#host = host;
		this.#port = port;
		this.#connecting = true;
		this.#pending = true;
		// A connecting socket keeps the loop alive, same as in node.
		this.#active = true;
		this.#syncKeepalive();

		this.#connectPromise = (async () => {
			let stream = await open();
			this.#attach(host, port, stream.read, stream.write);

			this.emit("connect");
			this.emit("ready");
			if (onSecure) onSecure();

			void this.#pumpRead();
		})();

		this.#connectPromise.catch((_e) => {
			let e = _e instanceof Error ? _e : new Error(String(_e));
			this.destroy(e);
		});
	}

	async #pumpRead() {
		if (!this.#reader) return;

		try {
			while (true) {
				let { done, value } = await this.#reader.read();
				if (done) break;
				if (!value) continue;

				this.#bytesRead += value.byteLength;
				this.#read(value);
			}

			// A cancelled reader also resolves `{done: true}`, so this is reached on
			// `destroy()` as well as on a real EOF. Node emits `end` only for the latter —
			// a destroyed socket goes straight to `close`.
			if (!this.destroyed) this.push(null);
		} catch (_e) {
			let e = _e instanceof Error ? _e : new Error(String(_e));
			this.destroy(e);
		}
	}

	_read() {}

	_write(
		chunk: Uint8Array | string,
		encoding: BufferEncoding,
		callback: (error?: Error | null) => void
	) {
		(async () => {
			if (this.#connectPromise) await this.#connectPromise;
			if (!this.#writer) throw new Error("Socket is not connected");

			let buffer =
				typeof chunk === "string"
					? Buffer.from(chunk, encoding)
					: new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
			let writeSize = buffer.byteLength;
			this.#bufferSize += writeSize;

			let writer = this.#writer;
			let writePromise = writer.write(buffer);
			writePromise
				.then(() => writer.ready)
				.then(
					() => {
						this.#bufferSize = Math.max(0, this.#bufferSize - writeSize);
					},
					() => {
						this.#bufferSize = Math.max(0, this.#bufferSize - writeSize);
					}
				);

			await writePromise;

			this.#bytesWritten += buffer.byteLength;
		})()
			.then(() => callback())
			.catch((_e) => {
				let e = _e instanceof Error ? _e : new Error(String(_e));
				callback(e);
			});
	}

	_final(callback: (error?: Error | null) => void) {
		if (!this.#writer) {
			callback();
			return;
		}

		this.#writer
			.close()
			.then(() => {
				this.#writer = undefined;
				callback();
			})
			.catch((_e) => {
				let e = _e instanceof Error ? _e : new Error(String(_e));
				callback(e);
			});
	}

	_destroy(
		error: Error | null,
		callback: (error?: Error | null | undefined) => void
	) {
		this.#hadError = error !== null && error !== undefined;
		this.#connecting = false;
		this.#pending = false;
		this.#bufferSize = 0;
		// The handle is gone; stop keeping the loop alive.
		this.#active = false;
		this.#syncKeepalive();

		let reader = this.#reader;
		let writer = this.#writer;
		this.#reader = undefined;
		this.#writer = undefined;

		Promise.allSettled([
			reader ? reader.cancel(error ?? undefined) : Promise.resolve(),
			writer ? writer.abort(error ?? undefined) : Promise.resolve(),
		])
			.then(() => callback(error))
			.catch(() => callback(error));
	}

	get autoSelectFamilyAttemptedAddresses(): string[] {
		if (this.#host === undefined || this.#port === undefined) return [];
		return [`${this.#host}:${this.#port}`];
	}

	get bytesRead() {
		return this.#bytesRead;
	}

	get bytesWritten() {
		return this.#bytesWritten;
	}

	get bufferSize() {
		return this.#bufferSize;
	}

	get connecting() {
		return this.#connecting;
	}

	get pending() {
		return this.#pending;
	}

	get remoteAddress() {
		return this.#host;
	}

	get remotePort() {
		return this.#port;
	}

	get remoteFamily() {
		if (!this.#host) return undefined;
		return this.#host.includes(":") ? "IPv6" : "IPv4";
	}

	get readyState(): import("node:net").SocketReadyState {
		if (this.destroyed) return "closed";
		if (this.#connecting) return "opening";
		if (this.readableEnded && !this.writableEnded) return "writeOnly";
		if (!this.readableEnded && this.writableEnded) return "readOnly";
		return "open";
	}

	address() {
		return {};
	}

	destroySoon() {
		if (this.writableFinished) {
			this.destroy();
			return;
		}

		this.end(() => this.destroy());
	}

	resetAndDestroy() {
		this.destroy();
		return this;
	}

	setTimeout(timeout: number, callback?: () => void) {
		void timeout;
		void callback;
		return this;
	}

	setNoDelay() {
		return this;
	}

	setKeepAlive() {
		return this;
	}

	ref() {
		this.#refed = true;
		this.#syncKeepalive();
		return this;
	}

	unref() {
		this.#refed = false;
		this.#syncKeepalive();
		return this;
	}

	connect(
		options:
			| ConnectOptions
			| TcpConnectOptions["port"]
			| IpcConnectOptions["path"],
		arg1?: NonNullable<TcpConnectOptions["host"]> | (() => void),
		connectionListener?: () => void
	): this {
		let host: string;
		let port: number;
		let onConnect: (() => void) | undefined;
		let bufferSize: number | undefined;

		if (typeof options == "object" && options !== null) {
			onConnect = typeof arg1 == "function" ? arg1 : connectionListener;
			if (!("port" in options)) throw new Error("unsupported");

			host = options.host ?? "localhost";
			port = options.port;
		} else if (typeof options == "string") {
			onConnect = typeof arg1 == "function" ? arg1 : connectionListener;
			throw new Error("unsupported");
		} else {
			host = typeof arg1 == "string" ? arg1 : "localhost";
			port = options;
			onConnect = typeof arg1 == "function" ? arg1 : connectionListener;
		}

		port = Number(port);
		if (!Number.isInteger(port) || port < 0 || port > 65535) {
			throw new RangeError("port must be an integer between 0 and 65535");
		}
		if (onConnect) this.once("connect", onConnect);

		this._beginConnect(host, port, async () => {
			if (LOOPBACK.has(host)) return await connectLoopback(port);
			let client = await getClient();
			return await client.connect(host, port, bufferSize);
		});

		return this;
	}
};
