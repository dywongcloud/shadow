// The one open-file handle, serving both fd families.
//
// A thin wrapper over a *host-side* fd now. Everything that made this file long — the
// whole-file buffer, the append cursor, the byte-range fragment cache, the offset reservation,
// the double-checked fills, the generation counter — lives next to the providers, in
// src/lib/vfs/handles.ts, and got simpler on the way: with both surfaces arriving as wire ops
// through one dispatcher, an ordinary FIFO queue per handle is correct, so none of the
// lock-free machinery is needed.
//
// What is left here is what belongs here: node's overload sets, argument coercion, and the fd
// table, so `fs.readSync(fd, …)` and `await filehandle.read()` are the same fd. That still
// matters — there used to be two handle classes and an `instanceof` check that made an fd from
// `openSync` fail with EBADF in `fs.read`.
//
// One consequence worth stating: `fs.readSync(fd, buffer, …)` writes into a caller-supplied
// buffer, and the bytes now arrive from the host, so there is one copy here that a worker-side
// buffer did not need. Against a saved round trip per call — `writeSync` was three — that is a
// good trade.

import nodeBuffer from "../buffer";
import { Stats } from "./classes";
import {
	createFsError,
	normalizePath,
	parseOpenFlags,
	toWriteBuffer,
	type OpenFlags,
} from "./util";
import { toEpochMs } from "./util";
import { fdTable } from "./fd-table";
import { ctx, host, hostAsync } from "./host";
// NOT a direct import of ./streams: that edge would pull the stream classes into the fs
// module-init cycle, where `class ReadStream extends Readable` runs before ../stream.ts exists.
// ./stream-registry.ts explains the arrangement.
import { streamCtors } from "./stream-registry";
import nodeStream from "../stream";

let Buffer = nodeBuffer.Buffer;

function toMutableBuffer(view: NodeJS.ArrayBufferView): Buffer {
	if (Buffer.isBuffer(view)) return view;
	return Buffer.from(view.buffer, view.byteOffset, view.byteLength);
}

function coercePosition(
	pos: number | bigint | null | undefined
): number | null {
	if (typeof pos === "bigint") pos = Number(pos);
	if (pos === undefined || pos === null || pos === -1) return null;
	if (!Number.isInteger(pos) || pos < 0) {
		throw createFsError("EINVAL", -22, "invalid position", "read");
	}
	return pos;
}

function isArrayBufferView(value: unknown): value is NodeJS.ArrayBufferView {
	return (
		typeof value === "object" && value !== null && ArrayBuffer.isView(value)
	);
}

// Splits a text stream into lines. Deliberately hand-rolled rather than
// delegating to node:readline — see FileHandle#readLines.
function makeLineReader(stream: any) {
	let lines: string[] = [];
	let pending = "";
	let waiters: Array<() => void> = [];
	let ended = false;
	let error: Error | undefined;
	let listeners: Array<(line: string) => void> = [];
	let closeListeners: Array<() => void> = [];

	let wake = () => {
		for (let w of waiters.splice(0)) w();
	};
	let push = (line: string) => {
		for (let fn of listeners) fn(line);
		lines.push(line);
	};

	stream.on("data", (chunk: any) => {
		pending += typeof chunk === "string" ? chunk : chunk.toString("utf8");
		let parts = pending.split("\n");
		pending = parts.pop() ?? "";
		for (let part of parts) {
			// Tolerate CRLF the way readline's crlfDelay:Infinity does.
			push(part.endsWith("\r") ? part.slice(0, -1) : part);
		}
		wake();
	});
	stream.on("error", (err: Error) => {
		error = err;
		ended = true;
		wake();
	});
	stream.on("end", () => {
		// A trailing fragment with no newline is still a line.
		if (pending.length > 0) {
			push(pending.endsWith("\r") ? pending.slice(0, -1) : pending);
			pending = "";
		}
		ended = true;
		for (let fn of closeListeners) fn();
		wake();
	});

	return {
		on(event: string, fn: any) {
			if (event === "line") listeners.push(fn);
			else if (event === "close") closeListeners.push(fn);
			return this;
		},
		close() {
			stream.destroy();
			ended = true;
			wake();
		},
		async *[Symbol.asyncIterator]() {
			while (true) {
				while (lines.length > 0) yield lines.shift()!;
				if (error) throw error;
				if (ended) return;
				await new Promise<void>((resolve) => waiters.push(resolve));
			}
		},
	};
}

export class FileHandle {
	#fd: number;
	#path: string;
	#flags: OpenFlags;
	/**
	 * Whether *this* wrapper still considers the fd usable.
	 *
	 * The host owns the real answer and will say EBADF regardless; this exists so a local
	 * lookup can fail without a round trip, and so `close()` is idempotent.
	 */
	#closed = false;

	private constructor(fd: number, path: string, flags: OpenFlags) {
		this.#fd = fd;
		this.#path = path;
		this.#flags = flags;
		fdTable.set(fd, this);
	}

	// ---------------------------------------------------------------------- open

	static async open(
		pathLike: string | Buffer | URL,
		flagsLike?: string | number
	): Promise<FileHandle> {
		const path = normalizePath(pathLike);
		const flags = parseOpenFlags(flagsLike);
		const fd = await hostAsync.open(ctx("open", path), path, flags.flag);
		return new FileHandle(fd, path, flags);
	}

	/** @internal — the `openSync` family. */
	static openSync(
		pathLike: string | Buffer | URL,
		flagsLike?: string | number
	): FileHandle {
		const path = normalizePath(pathLike);
		const flags = parseOpenFlags(flagsLike);
		const fd = host.open(ctx("open", path), path, flags.flag);
		return new FileHandle(fd, path, flags);
	}

	get fd(): number {
		return this.#fd;
	}

	/** @internal — `futimes` and the stream constructors need the opened path. */
	get filePath(): string {
		return this.#path;
	}

	/** @internal */
	get flags(): OpenFlags {
		return this.#flags;
	}

	#assertOpen(syscall: string) {
		if (this.#closed) {
			throw createFsError(
				"EBADF",
				-9,
				"bad file descriptor",
				syscall,
				this.#path
			);
		}
	}

	#ctx(syscall: string) {
		return ctx(syscall, this.#path);
	}

	// ---------------------------------------------------------------------- read

	/**
	 * @internal The read both surfaces drive. Returns bytes read.
	 *
	 * Argument validation stays here because it is node's contract about the *destination
	 * buffer*, which the host never sees.
	 */
	#checkRead(dest: Buffer, offset: number, length: number) {
		if (!Number.isInteger(offset) || offset < 0) {
			throw createFsError("EINVAL", -22, "invalid offset", "read", this.#path);
		}
		if (!Number.isInteger(length) || length < 0) {
			throw createFsError("EINVAL", -22, "invalid length", "read", this.#path);
		}
		if (offset + length > dest.length) {
			throw createFsError("EINVAL", -22, "invalid length", "read", this.#path);
		}
	}

	/** @internal */
	readSync(
		target: NodeJS.ArrayBufferView,
		offset: number,
		length: number,
		position: number | null
	): number {
		this.#assertOpen("read");
		const dest = toMutableBuffer(target);
		this.#checkRead(dest, offset, length);
		if (length === 0) return 0;
		const bytes = host.read(this.#ctx("read"), this.#fd, length, position);
		bytes.copy(dest, offset);
		return bytes.length;
	}

	/** @internal */
	async readAsync(
		target: NodeJS.ArrayBufferView,
		offset: number,
		length: number,
		position: number | null
	): Promise<number> {
		this.#assertOpen("read");
		const dest = toMutableBuffer(target);
		this.#checkRead(dest, offset, length);
		if (length === 0) return 0;
		const bytes = await hostAsync.read(
			this.#ctx("read"),
			this.#fd,
			length,
			position
		);
		bytes.copy(dest, offset);
		return bytes.length;
	}

	async read(
		bufferOrOptions?:
			| NodeJS.ArrayBufferView
			| {
					buffer?: NodeJS.ArrayBufferView;
					offset?: number;
					length?: number;
					position?: number | null;
			  },
		offsetArg?: number,
		lengthArg?: number,
		positionArg?: number | null
	): Promise<{ bytesRead: number; buffer: NodeJS.ArrayBufferView }> {
		let inputBuffer: NodeJS.ArrayBufferView;
		let offset: number;
		let length: number;
		let position: number | null;

		if (isArrayBufferView(bufferOrOptions) || bufferOrOptions === undefined) {
			inputBuffer =
				bufferOrOptions ??
				(Buffer.alloc(16384) as unknown as NodeJS.ArrayBufferView);
			offset = offsetArg ?? 0;
			length = lengthArg ?? toMutableBuffer(inputBuffer).byteLength - offset;
			position = coercePosition(positionArg);
		} else {
			const o = bufferOrOptions ?? {};
			inputBuffer =
				o.buffer ?? (Buffer.alloc(16384) as unknown as NodeJS.ArrayBufferView);
			offset = o.offset ?? 0;
			length = o.length ?? toMutableBuffer(inputBuffer).byteLength - offset;
			position = coercePosition(o.position);
		}

		const bytesRead = await this.readAsync(
			inputBuffer,
			offset,
			length,
			position
		);
		return { bytesRead, buffer: inputBuffer };
	}

	async readFile(
		options?: BufferEncoding | { encoding?: BufferEncoding | null }
	): Promise<Buffer | string> {
		this.#assertOpen("read");
		const encoding = typeof options === "string" ? options : options?.encoding;
		const buf = await hostAsync.readFileFd(this.#ctx("read"), this.#fd);
		return encoding ? buf.toString(encoding) : buf;
	}

	/** @internal */
	readFileSync(): Buffer {
		this.#assertOpen("read");
		return host.readFileFd(this.#ctx("read"), this.#fd);
	}

	// --------------------------------------------------------------------- write

	/** @internal */
	writeSync(src: Buffer, position: number | null): number {
		this.#assertOpen("write");
		return host.write(this.#ctx("write"), this.#fd, src, position);
	}

	async write(
		bufferOrString: string | NodeJS.ArrayBufferView,
		offsetOrPosition?:
			| number
			| { offset?: number; length?: number; position?: number | null }
			| null,
		lengthOrEncoding?: number | BufferEncoding,
		positionArg?: number | null
	): Promise<{
		bytesWritten: number;
		buffer: string | NodeJS.ArrayBufferView;
	}> {
		this.#assertOpen("write");
		let src: Buffer;
		let position: number | null;

		if (typeof bufferOrString === "string") {
			// write(string, position?, encoding?)
			position =
				typeof offsetOrPosition === "number" || offsetOrPosition === null
					? coercePosition(offsetOrPosition)
					: null;
			const encoding =
				typeof lengthOrEncoding === "string" ? lengthOrEncoding : "utf8";
			src = toWriteBuffer(bufferOrString, encoding);
		} else {
			const all = toWriteBuffer(bufferOrString);
			let offset: number;
			let length: number;
			if (typeof offsetOrPosition === "object" && offsetOrPosition !== null) {
				offset = offsetOrPosition.offset ?? 0;
				length = offsetOrPosition.length ?? all.byteLength - offset;
				position = coercePosition(offsetOrPosition.position);
			} else {
				offset = (offsetOrPosition as number | undefined) ?? 0;
				length =
					typeof lengthOrEncoding === "number"
						? lengthOrEncoding
						: all.byteLength - offset;
				position = coercePosition(positionArg);
			}
			if (!Number.isInteger(offset) || offset < 0) {
				throw createFsError(
					"EINVAL",
					-22,
					"invalid offset",
					"write",
					this.#path
				);
			}
			if (!Number.isInteger(length) || length < 0) {
				throw createFsError(
					"EINVAL",
					-22,
					"invalid length",
					"write",
					this.#path
				);
			}
			if (offset + length > all.length) {
				throw createFsError(
					"EINVAL",
					-22,
					"invalid length",
					"write",
					this.#path
				);
			}
			src = all.subarray(offset, offset + length);
		}

		const bytesWritten = await hostAsync.write(
			this.#ctx("write"),
			this.#fd,
			src,
			position
		);
		return { bytesWritten, buffer: bufferOrString };
	}

	async writeFile(
		data: string | NodeJS.ArrayBufferView | ArrayBuffer,
		options?: BufferEncoding | { encoding?: BufferEncoding | null }
	): Promise<void> {
		this.#assertOpen("write");
		const encoding =
			typeof options === "string" ? options : (options?.encoding ?? undefined);
		await hostAsync.writeFileFd(
			this.#ctx("write"),
			this.#fd,
			toWriteBuffer(data, encoding)
		);
	}

	async appendFile(
		data: string | NodeJS.ArrayBufferView | ArrayBuffer,
		options?: BufferEncoding | { encoding?: BufferEncoding | null }
	): Promise<void> {
		this.#assertOpen("write");
		const encoding =
			typeof options === "string" ? options : (options?.encoding ?? undefined);
		await hostAsync.appendFd(
			this.#ctx("write"),
			this.#fd,
			toWriteBuffer(data, encoding)
		);
	}

	/** @internal */
	appendFileSync(data: Buffer): void {
		this.#assertOpen("write");
		host.appendFd(this.#ctx("write"), this.#fd, data);
	}

	async truncate(len = 0): Promise<void> {
		this.#assertOpen("ftruncate");
		await hostAsync.ftruncate(this.#ctx("ftruncate"), this.#fd, len);
	}

	/** @internal */
	truncateSync(len = 0): void {
		this.#assertOpen("ftruncate");
		host.ftruncate(this.#ctx("ftruncate"), this.#fd, len);
	}

	// -------------------------------------------------------------- flush/close

	async sync(): Promise<void> {
		this.#assertOpen("fsync");
		await hostAsync.fsync(this.#ctx("fsync"), this.#fd);
	}

	/** @internal */
	syncSync(): void {
		this.#assertOpen("fsync");
		host.fsync(this.#ctx("fsync"), this.#fd);
	}

	async datasync(): Promise<void> {
		return this.sync();
	}

	async close(): Promise<void> {
		if (this.#closed) return;
		this.#closed = true;
		fdTable.delete(this.#fd);
		await hostAsync.close(this.#ctx("close"), this.#fd);
	}

	/** @internal */
	closeSync(): void {
		if (this.#closed) return;
		this.#closed = true;
		fdTable.delete(this.#fd);
		host.close(this.#ctx("close"), this.#fd);
	}

	/** @internal — write + flush + close in one round trip. See `Utf8Stream#flushSync`. */
	flushWriteSync(data?: Buffer): void {
		if (this.#closed) return;
		this.#closed = true;
		fdTable.delete(this.#fd);
		host.flushWrite(this.#ctx("write"), this.#fd, data);
	}

	// ---------------------------------------------------------------------- stat

	async stat(options?: {
		bigint?: boolean;
	}): Promise<InstanceType<typeof Stats>> {
		this.#assertOpen("fstat");
		const entry = await hostAsync.fstat(this.#ctx("fstat"), this.#fd);
		return new Stats(entry, options?.bigint || false);
	}

	/** @internal */
	statSync(bigint: boolean): InstanceType<typeof Stats> {
		this.#assertOpen("fstat");
		return new Stats(host.fstat(this.#ctx("fstat"), this.#fd), bigint);
	}

	// puterfs has no mode/owner bits (Stats reports a constant 0o777), so these validate the
	// handle is open and otherwise no-op — matching how a lot of tooling expects chmod/chown to
	// "succeed".
	async chmod(_mode: number): Promise<void> {
		this.#assertOpen("fchmod");
	}

	async chown(_uid: number, _gid: number): Promise<void> {
		this.#assertOpen("fchown");
	}

	// The handle is already open, so `false` only means the requested times were not
	// representable — there is no missing path to report.
	async utimes(
		atime: number | string | Date,
		mtime: number | string | Date
	): Promise<void> {
		this.#assertOpen("futime");
		await hostAsync.futimes(
			this.#ctx("futime"),
			this.#fd,
			toEpochMs(atime, "utime"),
			toEpochMs(mtime, "utime")
		);
	}

	// ------------------------------------------------------------ scatter/gather

	async readv(
		buffers: readonly NodeJS.ArrayBufferView[],
		position?: number | null
	): Promise<{ bytesRead: number; buffers: NodeJS.ArrayBufferView[] }> {
		this.#assertOpen("readv");
		// One round trip for the whole vector, where this used to be one per buffer.
		const parts = await hostAsync.readv(
			this.#ctx("readv"),
			this.#fd,
			buffers.map((b) => b.byteLength),
			position ?? null
		);
		let total = 0;
		for (let i = 0; i < parts.length; i++) {
			toMutableBuffer(buffers[i]).set(parts[i], 0);
			total += parts[i].length;
		}
		return { bytesRead: total, buffers: buffers as NodeJS.ArrayBufferView[] };
	}

	/** @internal */
	readvSync(
		buffers: readonly NodeJS.ArrayBufferView[],
		position: number | null
	): number {
		this.#assertOpen("readv");
		const parts = host.readv(
			this.#ctx("readv"),
			this.#fd,
			buffers.map((b) => b.byteLength),
			position
		);
		let total = 0;
		for (let i = 0; i < parts.length; i++) {
			toMutableBuffer(buffers[i]).set(parts[i], 0);
			total += parts[i].length;
		}
		return total;
	}

	async writev(
		buffers: readonly NodeJS.ArrayBufferView[],
		position?: number | null
	): Promise<{ bytesWritten: number; buffers: NodeJS.ArrayBufferView[] }> {
		this.#assertOpen("writev");
		const bytesWritten = await hostAsync.writev(
			this.#ctx("writev"),
			this.#fd,
			buffers.map((b) => toWriteBuffer(b)),
			position ?? null
		);
		return { bytesWritten, buffers: buffers as NodeJS.ArrayBufferView[] };
	}

	/** @internal */
	writevSync(
		buffers: readonly NodeJS.ArrayBufferView[],
		position: number | null
	): number {
		this.#assertOpen("writev");
		return host.writev(
			this.#ctx("writev"),
			this.#fd,
			buffers.map((b) => toWriteBuffer(b)),
			position
		);
	}

	// -------------------------------------------------------------------- streams

	createReadStream(options?: any): any {
		return new streamCtors.ReadStream!(this.#path, {
			...(options ?? {}),
			fd: this,
		});
	}

	createWriteStream(options?: any): any {
		return new streamCtors.WriteStream!(this.#path, {
			...(options ?? {}),
			fd: this,
		});
	}

	readableWebStream(options?: { type?: "bytes" | "default" }): ReadableStream {
		void options;
		// autoClose:false because the caller still owns this handle — node's readableWebStream
		// doesn't close it either.
		return (nodeStream.Readable as any).toWeb(
			this.createReadStream({ autoClose: false })
		) as unknown as ReadableStream;
	}

	// node returns a `readline.Interface` here. Importing node:readline would put its subgraph —
	// which reaches `internal/util/inspect`, whose top level calls `internalBinding('util')` —
	// into the fs bootstrap path, ahead of the binding table. So this is a minimal stand-in:
	// async-iterable, emits 'line'/'close', and closeable, which covers what `readLines` is
	// used for.
	readLines(options?: any): any {
		let stream = this.createReadStream({
			encoding: "utf8",
			...(options ?? {}),
		});
		return makeLineReader(stream);
	}

	async [Symbol.asyncDispose](): Promise<void> {
		await this.close();
	}
}
