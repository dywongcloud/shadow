// fs.Utf8Stream — node 25's buffered append-writer (the sonic-boom design pino
// is built on), adapted to puterfs.
//
// The buffer-until-flush shape maps well here: puterfs has no append api, so
// every flush is a whole-file upload. That makes flushing O(file size) rather
// than O(new bytes), which is why `minLength` matters much more on this
// filesystem than on a real one — a log that flushes per line uploads the whole
// log per line. The default (0) still flushes eagerly, matching node.
//
// `retryEAGAIN` is accepted and never called: EAGAIN/EBUSY are kernel pipe
// conditions with no analogue in an HTTP upload.

import nodeBuffer from "../buffer";
import nodePath from "../path";
import { FileHandle } from "./handle";
import { promisesToDepromisify } from "./promises";
import { fdTable } from "./fd-table";
import { createFsError, normalizePath } from "./util";
import { EmitterBase } from "./lazy-base";

type NodeFs = typeof import("node:fs");

let Buffer = nodeBuffer.Buffer;

const DEFAULT_MAX_WRITE = 16384;

class Utf8StreamImpl extends EmitterBase {
	readonly append: boolean;
	readonly contentMode: "utf8" | "buffer";
	readonly fsync: boolean;
	readonly maxLength: number;
	readonly minLength: number;
	readonly mkdir: boolean;
	readonly mode: number | string;
	readonly periodicFlush: number;
	readonly sync: boolean;

	#file: string;
	#fd: number;
	#handle: FileHandle | undefined;
	#opening: Promise<FileHandle> | undefined;
	#buffer: Buffer[] = [];
	#buffered = 0;
	#maxWrite: number;
	#writing = false;
	#destroyed = false;
	#ended = false;
	#periodic: ReturnType<typeof setInterval> | undefined;

	constructor(options: any) {
		super();
		let opts = options ?? {};

		this.append = opts.append !== false;
		this.contentMode = opts.contentMode === "buffer" ? "buffer" : "utf8";
		this.fsync = !!opts.fsync;
		this.maxLength = opts.maxLength ?? 0;
		this.minLength = opts.minLength ?? 0;
		this.mkdir = !!opts.mkdir;
		this.mode = opts.mode ?? 0o666;
		this.periodicFlush = opts.periodicFlush ?? 0;
		this.sync = !!opts.sync;
		this.#maxWrite = opts.maxWrite ?? DEFAULT_MAX_WRITE;

		let existing =
			typeof opts.fd === "number" ? fdTable.get(opts.fd) : undefined;
		if (existing instanceof FileHandle) {
			this.#handle = existing;
			this.#fd = opts.fd;
			this.#file = existing.filePath;
			queueMicrotask(() => this.emit("ready"));
		} else if (typeof opts.dest === "string") {
			this.#file = normalizePath(opts.dest);
			this.#fd = -1;
			void this.#open();
		} else {
			throw createFsError(
				"EINVAL",
				-22,
				"one of `dest` or `fd` is required",
				"open"
			);
		}

		if (this.periodicFlush > 0) {
			this.#periodic = setInterval(
				() => this.flush(() => {}),
				this.periodicFlush
			);
			// A log flusher shouldn't be the reason a run refuses to exit.
			(this.#periodic as any)?.unref?.();
		}
	}

	get fd(): number {
		return this.#fd;
	}

	get file(): string {
		return this.#file;
	}

	get writing(): boolean {
		return this.#writing;
	}

	#open(): Promise<FileHandle> {
		if (this.#handle) return Promise.resolve(this.#handle);
		if (this.#opening) return this.#opening;

		this.#opening = (async () => {
			if (this.mkdir) {
				await promisesToDepromisify
					.mkdir(nodePath.dirname(this.#file), { recursive: true })
					.catch(() => undefined);
			}
			let handle = await FileHandle.open(this.#file, this.append ? "a" : "w");
			this.#handle = handle;
			this.#fd = handle.fd;
			this.emit("ready");
			return handle;
		})();

		this.#opening.catch((err) => {
			this.#opening = undefined;
			this.emit("error", err);
		});
		return this.#opening;
	}

	write(data: string | Buffer): boolean {
		if (this.#destroyed) {
			throw createFsError("EBADF", -9, "stream destroyed", "write", this.#file);
		}

		let buf = typeof data === "string" ? Buffer.from(data, "utf8") : data;

		// node drops the write (and announces it) rather than growing past
		// maxLength, so a busy disk degrades by losing lines instead of memory.
		if (
			this.maxLength > 0 &&
			this.#buffered + buf.byteLength > this.maxLength
		) {
			this.emit("drop", data);
			return true;
		}

		this.#buffer.push(buf as Buffer);
		this.#buffered += buf.byteLength;

		if (this.#buffered >= this.minLength) this.flush(() => {});

		return this.#buffered < this.#maxWrite;
	}

	flush(callback?: (err: Error | null) => void): void {
		let cb = callback ?? (() => {});
		if (this.#destroyed) return cb(null);
		// "Do nothing if it is already writing" — the in-flight flush will pick up
		// whatever accumulated, and #drain re-checks after it lands.
		if (this.#writing || this.#buffered === 0) return cb(null);

		this.#writing = true;
		let payload = Buffer.concat(this.#buffer);
		this.#buffer = [];
		this.#buffered = 0;

		(async () => {
			let handle = await this.#open();
			// The handle holds the whole file; appendFile + sync is the upload.
			await handle.appendFile(payload as unknown as NodeJS.ArrayBufferView);
			await handle.sync();
			return payload.byteLength;
		})().then(
			(written) => {
				this.#writing = false;
				this.emit("write", written);
				this.#drain();
				cb(null);
			},
			(err) => {
				this.#writing = false;
				this.emit("error", err);
				cb(err as Error);
			}
		);
	}

	#drain() {
		if (this.#buffered > 0 && !this.#destroyed) this.flush(() => {});
		else if (this.#ended) this.#finish();
	}

	// A blocking whole-file rewrite. Genuinely costly, as node's docs warn — here it also parks
	// the worker on a synchronous request.
	//
	// Two requests rather than five: the open is one, and write+flush+close is a single
	// `fdFlushWrite`. A logger calling this per line felt every one of the round trips this used
	// to make.
	flushSync(): void {
		if (this.#destroyed || this.#buffered === 0) return;

		let payload = Buffer.concat(this.#buffer);
		this.#buffer = [];
		this.#buffered = 0;

		let handle = FileHandle.openSync(this.#file, this.append ? "a" : "w");
		handle.flushWriteSync(payload);
		this.emit("write", payload.byteLength);
	}

	reopen(file?: any): void {
		let next = file === undefined ? this.#file : normalizePath(file);
		this.flush(() => {
			let previous = this.#handle;
			this.#handle = undefined;
			this.#opening = undefined;
			this.#fd = -1;
			this.#file = next;
			previous?.close().catch(() => undefined);
			void this.#open();
		});
	}

	end(): void {
		if (this.#ended || this.#destroyed) return;
		this.#ended = true;
		if (this.#buffered > 0 || this.#writing) this.flush(() => this.#drain());
		else this.#finish();
	}

	#finish() {
		if (this.#destroyed) return;
		this.emit("finish");
		this.#teardown();
	}

	destroy(): void {
		if (this.#destroyed) return;
		// Deliberately drops the buffer — that's what "without flushing" means.
		this.#buffer = [];
		this.#buffered = 0;
		this.#teardown();
	}

	#teardown() {
		this.#destroyed = true;
		if (this.#periodic !== undefined) clearInterval(this.#periodic);
		this.#periodic = undefined;
		let handle = this.#handle;
		this.#handle = undefined;
		this.#opening = undefined;
		Promise.resolve(handle?.close())
			.catch(() => undefined)
			.then(() => this.emit("close"));
	}

	[Symbol.dispose](): void {
		this.destroy();
	}
}

export let Utf8Stream = Utf8StreamImpl as unknown as NodeFs["Utf8Stream"];
